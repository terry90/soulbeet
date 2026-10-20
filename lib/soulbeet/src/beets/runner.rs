//! Runs a beets subprocess without ever blocking on a full pipe.
//!
//! Both pipes are read to EOF by their own task while the main loop watches the
//! exit, an idle timer (reset by any bytes read) and a total cap. Every run ends by
//! killing beets' whole process group, so hook scripts and plugin helpers go too.

use itertools::Itertools;
use regex::Regex;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, Instant};

const LINE_CAP: usize = 8 * 1024;
const TRUNCATED_MARKER: &str = " [truncated]";
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 2048;

static RE_ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])").unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// Ends the run when neither pipe produced a byte for this long.
    pub idle: Duration,
    /// Ends the run after this long, however chatty it is.
    pub total: Duration,
    /// How long to wait for the pipes to close once the run has ended.
    pub drain: Duration,
}

impl RunLimits {
    pub const PROBE: RunLimits = RunLimits {
        idle: Duration::from_secs(30),
        total: Duration::from_secs(30),
        drain: Duration::from_secs(3),
    };

    /// `beet ls` of a library, which has no import to wait for.
    pub const LIBRARY_LS: RunLimits = RunLimits {
        idle: Duration::from_secs(60),
        total: Duration::from_secs(5 * 60),
        drain: Duration::from_secs(3),
    };
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            idle: Duration::from_secs(180),
            total: Duration::from_secs(30 * 60),
            drain: Duration::from_secs(3),
        }
    }
}

pub struct RunSpec<'a> {
    pub program: &'a Path,
    pub args: Vec<OsString>,
    pub limits: RunLimits,
}

#[derive(Debug)]
pub enum RunEnd {
    Exited(ExitStatus),
    Idle,
    Total,
    /// The process could not be started, or waiting on it failed.
    SpawnFailed(io::Error),
}

#[derive(Debug)]
pub struct RunOutcome {
    pub end: RunEnd,
    /// The last output lines, ANSI stripped.
    pub tail: String,
    /// A process outside beets' group kept a pipe open past the drain bound.
    pub drain_timed_out: bool,
}

pub(crate) fn strip_ansi(s: &str) -> Cow<'_, str> {
    RE_ANSI.replace_all(s, "")
}

/// Runs `spec`, calling `on_line` with every output line (ANSI stripped) as it
/// arrives. stdin is closed, so a beets prompt fails instead of waiting.
pub async fn run_beet(
    spec: RunSpec<'_>,
    on_line: &mut (dyn FnMut(Stream, &str) + Send),
) -> RunOutcome {
    let mut child = match Command::new(spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("PYTHONUNBUFFERED", "1")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return RunOutcome {
                end: RunEnd::SpawnFailed(e),
                tail: String::new(),
                drain_timed_out: false,
            }
        }
    };
    let mut group = GroupGuard {
        pgid: child.id().map_or(0, |id| id as libc::pid_t),
        armed: true,
    };

    let (tx, mut rx) = mpsc::channel::<(Stream, String)>(256);
    let activity = Arc::new(Notify::new());
    let mut readers = JoinSet::new();
    if let Some(out) = child.stdout.take() {
        readers.spawn(read_pipe(out, Stream::Stdout, tx.clone(), activity.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.spawn(read_pipe(err, Stream::Stderr, tx.clone(), activity.clone()));
    }
    drop(tx);

    let limits = spec.limits;
    let mut tail = TailBuffer::default();
    let mut handle = |stream: Stream, line: String| {
        let line = strip_ansi(&line);
        tracing::debug!(target: "soulbeet::beets::output", "{stream:?}: {line}");
        tail.push(&line);
        on_line(stream, &line);
    };

    let total = sleep(limits.total);
    let idle = sleep(limits.idle);
    tokio::pin!(total, idle);
    let end = loop {
        tokio::select! {
            biased;
            status = child.wait() => break match status {
                Ok(status) => RunEnd::Exited(status),
                Err(e) => RunEnd::SpawnFailed(e),
            },
            _ = &mut total => break RunEnd::Total,
            _ = activity.notified() => idle.as_mut().reset(Instant::now() + limits.idle),
            Some((stream, line)) = rx.recv() => handle(stream, line),
            _ = &mut idle => break RunEnd::Idle,
        }
    };

    // Kill the group before waiting on anything: a leftover group member holding
    // a pipe would otherwise keep the readers from reaching EOF.
    group.kill();
    let drain_timed_out = timeout(limits.drain, async {
        while let Some((stream, line)) = rx.recv().await {
            handle(stream, line);
        }
    })
    .await
    .is_err();
    readers.abort_all();
    group.armed = false;

    if drain_timed_out && matches!(end, RunEnd::Exited(_)) {
        tracing::warn!("beets exited but a child process kept its output open");
    }

    RunOutcome {
        end,
        tail: tail.render(),
        drain_timed_out,
    }
}

/// Reads one pipe to EOF. It must not stop early: beets gets EPIPE on a closed
/// pipe and exits 0 halfway through an import.
async fn read_pipe(
    pipe: impl AsyncRead + Unpin,
    stream: Stream,
    tx: mpsc::Sender<(Stream, String)>,
    activity: Arc<Notify>,
) {
    let mut reader = BufReader::with_capacity(64 * 1024, pipe);
    let mut splitter = LineSplitter::default();
    let mut lines = Vec::new();
    loop {
        let read = match reader.fill_buf().await {
            Ok([]) => break,
            Ok(chunk) => {
                splitter.feed(chunk, &mut lines);
                chunk.len()
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!("reading beets {stream:?} failed: {e}");
                break;
            }
        };
        reader.consume(read);
        activity.notify_one();
        for line in lines.drain(..) {
            // A closed receiver means the run is over; keep reading regardless.
            let _ = tx.send((stream, line)).await;
        }
    }
    splitter.finish(&mut lines);
    for line in lines {
        let _ = tx.send((stream, line)).await;
    }
}

/// Splits bytes into lines on `\n`, `\r\n` or a bare `\r` (progress output),
/// capping each line at `LINE_CAP` bytes.
#[derive(Default)]
struct LineSplitter {
    buf: Vec<u8>,
    truncated: bool,
    after_cr: bool,
}

impl LineSplitter {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            match b {
                b'\n' if self.after_cr => self.after_cr = false,
                b'\n' => self.emit(out),
                b'\r' => {
                    self.emit(out);
                    self.after_cr = true;
                }
                _ => {
                    self.after_cr = false;
                    if self.buf.len() < LINE_CAP {
                        self.buf.push(b);
                    } else {
                        self.truncated = true;
                    }
                }
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        if !self.buf.is_empty() || self.truncated {
            self.emit(out);
        }
    }

    fn emit(&mut self, out: &mut Vec<String>) {
        let mut line = String::from_utf8_lossy(&self.buf).into_owned();
        if self.truncated {
            line.push_str(TRUNCATED_MARKER);
        }
        out.push(line);
        self.buf.clear();
        self.truncated = false;
    }
}

/// The last `TAIL_LINES` non-blank lines, at most `TAIL_BYTES` once joined.
#[derive(Default)]
struct TailBuffer {
    lines: VecDeque<String>,
    bytes: usize,
}

impl TailBuffer {
    fn push(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let mut cut = line.len().min(TAIL_BYTES);
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        self.bytes += cut;
        self.lines.push_back(line[..cut].to_string());
        while self.lines.len() > TAIL_LINES || self.joined_len() > TAIL_BYTES {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }

    fn joined_len(&self) -> usize {
        self.bytes + self.lines.len().saturating_sub(1)
    }

    fn render(&self) -> String {
        self.lines.iter().join("\n")
    }
}

/// Kills the process group on drop unless the run finished and drained.
/// `kill_on_drop` alone only signals beets' own pid.
struct GroupGuard {
    pgid: libc::pid_t,
    armed: bool,
}

impl GroupGuard {
    fn kill(&self) {
        if self.pgid > 1 {
            // SAFETY: killpg has no memory-safety preconditions.
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Loose enough that a loaded machine never hits them by accident. Tests
    // about a limit set that one tight.
    const SHORT: RunLimits = RunLimits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(30),
        drain: Duration::from_millis(300),
    };

    // Scripts run through `sh -c`, not as freshly written executables: exec'ing
    // a file this process just wrote fails with ETXTBSY whenever another test
    // thread forks while the file is still open for writing.
    fn sh(body: &str) -> Vec<OsString> {
        vec!["-c".into(), body.into()]
    }

    async fn run_in(
        program: &Path,
        args: Vec<OsString>,
        limits: RunLimits,
    ) -> (RunOutcome, Vec<(Stream, String)>) {
        let mut lines = Vec::new();
        let mut on_line = |stream: Stream, line: &str| lines.push((stream, line.to_string()));
        let spec = RunSpec {
            program,
            args,
            limits,
        };
        let out = run_beet(spec, &mut on_line).await;
        (out, lines)
    }

    async fn run(body: &str, limits: RunLimits) -> (RunOutcome, Vec<(Stream, String)>) {
        run_in(Path::new("/bin/sh"), sh(body), limits).await
    }

    fn exited_ok(end: &RunEnd) -> bool {
        matches!(end, RunEnd::Exited(s) if s.success())
    }

    fn texts(lines: &[(Stream, String)]) -> Vec<&str> {
        lines.iter().map(|(_, l)| l.as_str()).collect()
    }

    #[tokio::test]
    async fn output_larger_than_any_pipe_does_not_deadlock() {
        // A deadlock would end as Idle.
        let (out, lines) = run(
            "head -c 2000000 /dev/zero | tr '\\0' 'x' | fold -w 100 >&2; exit 0",
            SHORT,
        )
        .await;
        assert!(exited_ok(&out.end), "{:?}", out.end);
        assert_eq!(lines.len(), 20000);
        assert!(lines.iter().all(|(s, _)| *s == Stream::Stderr));
    }

    #[tokio::test]
    async fn idle_resets_on_bytes_without_newline() {
        // Each gap is under the idle limit, both together are over it.
        let limits = RunLimits {
            idle: Duration::from_secs(4),
            ..SHORT
        };
        let (out, lines) = run(
            "printf 'a'; sleep 2.5; printf 'b'; sleep 2.5; printf 'c\\n'; exit 0",
            limits,
        )
        .await;
        assert!(exited_ok(&out.end), "{:?}", out.end);
        assert_eq!(texts(&lines), ["abc"]);
    }

    #[tokio::test]
    async fn silence_ends_as_idle() {
        let limits = RunLimits {
            idle: Duration::from_secs(1),
            ..SHORT
        };
        let (out, _) = run("echo start; sleep 30", limits).await;
        assert!(matches!(out.end, RunEnd::Idle), "{:?}", out.end);
        assert!(out.tail.ends_with("start"), "{:?}", out.tail);
    }

    #[tokio::test]
    async fn total_cap_ends_a_chatty_run() {
        let limits = RunLimits {
            total: Duration::from_secs(1),
            ..SHORT
        };
        let (out, _) = run("while :; do echo x; sleep 0.05; done", limits).await;
        assert!(matches!(out.end, RunEnd::Total), "{:?}", out.end);
    }

    #[tokio::test]
    async fn a_backgrounded_grandchild_holding_stdout_is_not_a_timeout() {
        let start = Instant::now();
        let (out, lines) = run("(sleep 30 &) ; echo done; exit 0", SHORT).await;
        assert!(exited_ok(&out.end), "{:?}", out.end);
        assert!(!out.drain_timed_out);
        assert_eq!(texts(&lines), ["done"]);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_setsid_escapee_hits_the_drain_bound() {
        let dir = tempfile::tempdir().unwrap();
        // Wait for the escapee to leave the group, or the end-of-run kill races it.
        let marker = dir.path().join("escaped");
        let start = Instant::now();
        let (out, _) = run(
            &format!(
                "setsid sh -c 'touch \"$0\"; sleep 20' '{m}' & \
                 while [ ! -e '{m}' ]; do sleep 0.01; done; echo done; exit 0",
                m = marker.display()
            ),
            SHORT,
        )
        .await;
        assert!(exited_ok(&out.end), "{:?}", out.end);
        assert!(out.drain_timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn bad_bytes_carriage_returns_and_long_lines_are_split_safely() {
        let (out, lines) = run(
            "printf '\\377\\376ok\\n'; printf 'p1\\rp2\\rp3\\n'; \
             head -c 300000 /dev/zero | tr '\\0' 'y'; echo",
            SHORT,
        )
        .await;
        assert!(exited_ok(&out.end), "{:?}", out.end);
        let lines = texts(&lines);
        assert_eq!(lines.len(), 5, "{:?}", lines);
        assert!(lines[0].contains('\u{fffd}') && lines[0].contains("ok"));
        assert_eq!(lines[1..4], ["p1", "p2", "p3"]);
        assert_eq!(lines[4], format!("{} [truncated]", "y".repeat(8192)));
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (out, _) = run_in(&dir.path().join("does-not-exist"), Vec::new(), SHORT).await;
        assert!(matches!(out.end, RunEnd::SpawnFailed(_)), "{:?}", out.end);
    }

    /// Puts an open pipe on this process's stdin until dropped, so a child
    /// that inherited stdin would see a pipe, not whatever the harness gave.
    struct PipeOnStdin {
        saved: i32,
        write_end: i32,
    }

    impl PipeOnStdin {
        fn new() -> Self {
            let mut fds = [0; 2];
            // SAFETY: plain fd calls on fds this test owns; fd 0 is restored on drop.
            unsafe {
                assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
                let saved = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3);
                assert!(saved >= 0);
                assert_eq!(libc::dup2(fds[0], 0), 0);
                libc::close(fds[0]);
                PipeOnStdin {
                    saved,
                    write_end: fds[1],
                }
            }
        }
    }

    impl Drop for PipeOnStdin {
        fn drop(&mut self) {
            // SAFETY: as in new.
            unsafe {
                libc::dup2(self.saved, 0);
                libc::close(self.saved);
                libc::close(self.write_end);
            }
        }
    }

    #[tokio::test]
    async fn a_prompt_on_closed_stdin_ends_fast() {
        let _stdin = PipeOnStdin::new();
        let start = Instant::now();
        // Exit 7 if stdin is not /dev/null, as it would be if inherited.
        let (out, _) = run(
            "in=$(readlink /proc/self/fd/0); \
             [ \"$in\" = /dev/null ] || { echo \"stdin is $in\" >&2; exit 7; }; \
             read x || { echo 'error: stdin stream ended while input required' >&2; exit 1; }",
            SHORT,
        )
        .await;
        assert!(
            matches!(out.end, RunEnd::Exited(s) if s.code() == Some(1)),
            "{:?}",
            out.end
        );
        // A read waiting on stdin would run into the 10 s idle limit.
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(out.tail.contains("stdin stream ended"), "{:?}", out.tail);
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pids");
        let body = format!(
            "echo $$ > '{p}'; sleep 30 & echo $! >> '{p}'; wait",
            p = pidfile.display()
        );
        let task = tokio::spawn(async move {
            let mut on_line = |_: Stream, _: &str| {};
            let spec = RunSpec {
                program: Path::new("/bin/sh"),
                args: sh(&body),
                limits: RunLimits::default(),
            };
            run_beet(spec, &mut on_line).await
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let pids: Vec<i32> = loop {
            let pids: Vec<i32> = std::fs::read_to_string(&pidfile)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
            if pids.len() == 2 {
                break pids;
            }
            assert!(Instant::now() < deadline, "script never wrote its pids");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(pids.iter().all(|&p| alive(p)));

        task.abort();
        let _ = task.await;
        // Reaping the killed processes can lag on a loaded machine.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|&p| alive(p)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for pid in pids {
            assert!(!alive(pid), "pid {pid} survived the dropped run");
        }
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn tail_keeps_last_20_lines_within_2_kib() {
        let mut tail = TailBuffer::default();
        for i in 1..=50 {
            tail.push(&format!("{i:03}{}", "z".repeat(197)));
        }
        let text = tail.render();
        assert!(text.lines().count() <= 20);
        assert!(text.len() <= 2048);
        assert!(text.ends_with(&format!("050{}", "z".repeat(197))));
    }
}
