//! How the backend talks to its helper, a Python script run once per call: one request as JSON
//! on stdin, then one JSON object per line on stdout. The helper runs with a fixed argument
//! list, no shell, an environment of its own folders only, and in a process group of its own
//! that is killed when the call ends, times out or is dropped.

use super::error::{clean, Fault, Kind};
use crate::process::ProcessGroup;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{sleep_until, Instant};

/// The helper, embedded so the binary and the script always match.
pub(crate) const HELPER: &str = include_str!("helper.py");

/// The longest stdout line read; a longer one is not the helper's.
const LINE_CAP: usize = 1024 * 1024;
/// The longest stderr line kept.
const ERR_LINE_CAP: usize = 2048;
/// How long a killed run gets to be reaped.
const REAP_WAIT: Duration = Duration::from_secs(3);

/// The programs the helper needs, as the image installs them.
#[derive(Debug, Clone)]
pub struct Tools {
    /// The interpreter the helper runs in.
    pub python: PathBuf,
    /// yt-dlp, yt-dlp-ejs and ytmusicapi, a tree of its own apart from beets'.
    pub site_packages: PathBuf,
    /// The JS runtime yt-dlp solves YouTube's challenges with, sandboxed by yt-dlp.
    pub deno: PathBuf,
    /// ffmpeg, which copies the Opus stream out of its WebM; None when it is not on PATH.
    pub ffmpeg: Option<PathBuf>,
}

impl Tools {
    /// These tools with every relative path read from the server's working folder.
    pub(crate) fn absolute(self) -> std::io::Result<Tools> {
        Ok(Tools {
            python: std::path::absolute(&self.python)?,
            site_packages: std::path::absolute(&self.site_packages)?,
            deno: std::path::absolute(&self.deno)?,
            ffmpeg: self.ffmpeg.map(std::path::absolute).transpose()?,
        })
    }
}

/// What one call asks the helper.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum Request {
    /// The versions of what the helper needs. No request to YouTube.
    Version,
    SearchAlbums {
        query: String,
        limit: u32,
    },
    Album {
        browse_id: String,
    },
    SearchSongs {
        query: String,
        limit: u32,
    },
    /// Downloads the video's Opus audio into `staging_dir`, of `min_abr` kbps or more.
    Fetch {
        video_id: String,
        staging_dir: PathBuf,
        min_abr: u32,
    },
    /// Fetches the first bytes of a Creative Commons track, to see that YouTube serves media.
    Probe {
        video_id: String,
        staging_dir: PathBuf,
    },
}

/// What every request carries besides its own fields.
#[derive(Serialize)]
struct Envelope<'a> {
    site_packages: &'a Path,
    deno: &'a Path,
    ffmpeg: Option<&'a Path>,
    /// yt-dlp's cache, which keeps YouTube's player script between calls.
    cache_dir: &'a Path,
    #[serde(flatten)]
    request: &'a Request,
}

/// A line the helper prints.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Answer {
    Version(Versions),
    Albums(Vec<AlbumHit>),
    Album(AlbumPage),
    Songs(Vec<Song>),
    Progress(Progress),
    Done(Done),
    Probe(String),
    Error(Fault),
}

/// What the helper has: a version for each piece, None for a piece it lacks.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct Versions {
    pub python: Option<String>,
    pub yt_dlp: Option<String>,
    pub ejs: Option<String>,
    pub ytmusicapi: Option<String>,
    pub deno: Option<String>,
    pub ffmpeg: Option<String>,
}

impl Versions {
    /// The pieces missing, as their programs are called.
    pub(crate) fn missing(&self) -> Vec<&'static str> {
        [
            ("yt-dlp", &self.yt_dlp),
            ("yt-dlp-ejs", &self.ejs),
            ("ytmusicapi", &self.ytmusicapi),
            ("deno", &self.deno),
            ("ffmpeg", &self.ffmpeg),
        ]
        .into_iter()
        .filter(|(_, version)| version.is_none())
        .map(|(name, _)| name)
        .collect()
    }
}

/// An album a search found.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AlbumHit {
    pub browse_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub year: Option<String>,
    pub explicit: bool,
}

/// An album and its tracklist.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AlbumPage {
    pub title: String,
    pub artists: Vec<String>,
    pub year: Option<String>,
    pub tracks: Vec<AlbumTrack>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AlbumTrack {
    pub video_id: Option<String>,
    pub title: String,
    pub track_number: Option<u32>,
    pub duration_s: Option<u32>,
    pub available: bool,
    pub video_type: Option<String>,
    pub artists: Vec<String>,
    pub explicit: bool,
}

/// A song a search found.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Song {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<SongAlbum>,
    pub duration_s: Option<u32>,
    pub available: bool,
    pub video_type: Option<String>,
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct SongAlbum {
    pub name: String,
    pub browse_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes per second.
    pub speed: Option<f64>,
}

/// A fetched file, in the staging folder.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Done {
    pub path: PathBuf,
    pub format_id: String,
    pub acodec: String,
    /// kbps, as YouTube lists the format.
    pub abr: Option<f64>,
    pub asr: Option<u32>,
    /// Seconds, as YouTube lists the video.
    pub duration: Option<f64>,
}

/// How long a call may run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Ends the call when the helper printed no line for this long.
    pub idle: Duration,
    /// Ends the call after this long, however much it prints.
    pub total: Duration,
}

impl Limits {
    pub(crate) const VERSION: Limits = Limits {
        idle: Duration::from_secs(60),
        total: Duration::from_secs(60),
    };
    pub(crate) const SEARCH: Limits = Limits {
        idle: Duration::from_secs(60),
        total: Duration::from_secs(90),
    };
    pub(crate) const PROBE: Limits = Limits {
        idle: Duration::from_secs(25),
        total: Duration::from_secs(25),
    };
}

/// Runs the helper.
#[derive(Clone)]
pub(crate) struct Helper {
    tools: Tools,
    script: Arc<str>,
    /// HOME, TMPDIR and working folder of every call: the backend's own.
    home: PathBuf,
    cache: PathBuf,
}

impl Helper {
    pub(crate) fn new(tools: Tools, script: Arc<str>, home: PathBuf, cache: PathBuf) -> Helper {
        Helper {
            tools,
            script,
            home,
            cache,
        }
    }

    /// Asks `request` of a new helper run: its answer, or why there is none. Each progress line
    /// goes to `on_progress` as it comes. The run's process group is killed when the call ends,
    /// and when its future is dropped.
    pub(crate) async fn call(
        &self,
        request: &Request,
        limits: Limits,
        on_progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Answer, Fault> {
        let python = &self.tools.python;
        let mut command = Command::new(python);
        command
            .args(["-I", "-X", "utf8", "-c"])
            .arg(&*self.script)
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", &self.home)
            .env("LANG", "C.UTF-8")
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, mut group) = ProcessGroup::spawn(&mut command).map_err(|e| {
            Fault::new(
                Kind::Tooling,
                format!("Cannot run {}: {e}", python.display()),
            )
        })?;
        let envelope = Envelope {
            site_packages: &self.tools.site_packages,
            deno: &self.tools.deno,
            ffmpeg: self.tools.ffmpeg.as_deref(),
            cache_dir: &self.cache,
            request,
        };
        let mut line = serde_json::to_vec(&envelope)
            .map_err(|e| Fault::new(Kind::Internal, format!("Cannot write the request: {e}")))?;
        line.push(b'\n');
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Fault::new(
                Kind::Internal,
                "The helper's pipes were not opened",
            ));
        };
        let errors = tokio::spawn(last_line(stderr));

        let deadline = Instant::now() + limits.total;
        let answered = async {
            if let Err(e) = stdin.write_all(&line).await {
                return Err(Ended::Fault(Fault::new(
                    Kind::Internal,
                    format!("The helper took no request: {e}"),
                )));
            }
            drop(stdin);
            let mut lines = Lines::new(stdout);
            loop {
                let idle = Instant::now() + limits.idle;
                let next = tokio::select! {
                    next = lines.next(LINE_CAP) => next,
                    () = sleep_until(idle.min(deadline)) => {
                        let which = if Instant::now() >= deadline { limits.total } else { limits.idle };
                        return Err(Ended::Fault(Fault::new(Kind::TimedOut, format!(
                            "The helper gave no answer within {} s", which.as_secs()
                        ))));
                    }
                };
                let text = match next {
                    Ok(Read::Line(text)) => text,
                    Ok(Read::TooLong) => {
                        return Err(Ended::Fault(Fault::new(
                            Kind::Unreadable,
                            format!("The helper printed a line longer than {LINE_CAP} bytes"),
                        )))
                    }
                    Ok(Read::End) => return Err(Ended::Silent),
                    Err(e) => {
                        return Err(Ended::Fault(Fault::new(
                            Kind::Internal,
                            format!("Cannot read the helper: {e}"),
                        )))
                    }
                };
                match read_line(&text).map_err(Ended::Fault)? {
                    Answer::Progress(progress) => on_progress(progress),
                    Answer::Error(fault) => {
                        return Err(Ended::Fault(Fault::new(fault.kind, fault.message)))
                    }
                    answer => return Ok(answer),
                }
            }
        }
        .await;

        group.kill();
        let status = tokio::time::timeout(REAP_WAIT, child.wait()).await;
        group.disarm();
        let said = errors.await.unwrap_or_default();
        if let Some(line) = &said {
            tracing::debug!("YouTube Music helper said: {line}");
        }
        answered.map_err(|ended| match ended {
            Ended::Fault(fault) => fault,
            // What it last said is all there is.
            Ended::Silent => {
                let status = match status {
                    Ok(Ok(status)) => status.to_string(),
                    _ => "no exit status".to_string(),
                };
                let said = said.unwrap_or_else(|| "nothing on stderr".to_string());
                Fault::new(
                    Kind::Internal,
                    format!("The helper ended without an answer ({status}): {said}"),
                )
            }
        })
    }
}

/// How a call ended without an answer.
enum Ended {
    Fault(Fault),
    /// The helper closed stdout without an answer.
    Silent,
}

/// One line of stdout as an answer; a line the helper never prints is unreadable.
fn read_line(text: &str) -> Result<Answer, Fault> {
    serde_json::from_str(text).map_err(|e| {
        let shown: String = text.chars().take(120).collect();
        Fault::new(
            Kind::Unreadable,
            format!("The helper printed a line Soulbeet cannot read ({e}): {shown}"),
        )
    })
}

/// What the next read of a pipe gave.
enum Read {
    Line(String),
    /// A line longer than the cap, read to its end and thrown away.
    TooLong,
    End,
}

/// The lines of a pipe.
struct Lines<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    fn new(pipe: R) -> Self {
        Lines {
            reader: BufReader::new(pipe),
        }
    }

    /// The next line without its end, kept when it has at most `cap` bytes.
    async fn next(&mut self, cap: usize) -> std::io::Result<Read> {
        let mut line = Vec::new();
        let mut over = false;
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return Ok(match (over, line.is_empty()) {
                    (true, _) => Read::TooLong,
                    (false, true) => Read::End,
                    (false, false) => Read::Line(String::from_utf8_lossy(&line).into()),
                });
            }
            let (taken, ended) = match chunk.iter().position(|&b| b == b'\n') {
                Some(at) => (at, true),
                None => (chunk.len(), false),
            };
            if !over {
                line.extend_from_slice(&chunk[..taken]);
                if line.len() > cap {
                    over = true;
                    line = Vec::new();
                }
            }
            self.reader.consume(taken + usize::from(ended));
            if ended {
                return Ok(match over {
                    true => Read::TooLong,
                    false => Read::Line(String::from_utf8_lossy(&line).into()),
                });
            }
        }
    }
}

/// The last line of stderr that is not blank, cleaned of URLs: only that is ever shown.
async fn last_line(pipe: impl AsyncRead + Unpin) -> Option<String> {
    let mut lines = Lines::new(pipe);
    let mut last = None;
    loop {
        match lines.next(ERR_LINE_CAP).await {
            Ok(Read::Line(text)) => {
                let text = clean(&text);
                if !text.is_empty() {
                    last = Some(text);
                }
            }
            Ok(Read::TooLong) => {}
            Ok(Read::End) | Err(_) => return last,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::beets::resolve_on_path;

    /// A helper of `script`, run by the python3 on PATH, in a fresh folder.
    pub(crate) fn helper_of(script: &str, dir: &Path) -> Helper {
        let python = resolve_on_path("python3").expect("python3 on PATH");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Helper::new(
            Tools {
                python,
                site_packages: dir.join("site-packages"),
                deno: dir.join("deno"),
                ffmpeg: None,
            },
            script.into(),
            home,
            dir.join("cache"),
        )
    }

    const SHORT: Limits = Limits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(20),
    };

    async fn ask(script: &str, limits: Limits) -> (Result<Answer, Fault>, Vec<Progress>) {
        let dir = tempfile::tempdir().unwrap();
        let helper = helper_of(script, dir.path());
        let mut seen = Vec::new();
        let answer = helper
            .call(&Request::Version, limits, &mut |p| seen.push(p))
            .await;
        (answer, seen)
    }

    #[tokio::test]
    async fn the_request_comes_on_stdin_and_the_answer_on_stdout() {
        let (answer, seen) = ask(
            r#"
import json, sys
request = json.loads(sys.stdin.readline())
assert request["command"] == "version", request
print(json.dumps({"progress": {"downloaded": 5, "total": 10, "speed": None}}), flush=True)
print(json.dumps({"version": {"python": "3.11", "yt_dlp": request["site_packages"]}}))
"#,
            SHORT,
        )
        .await;
        let Answer::Version(v) = answer.unwrap() else {
            panic!("not a version");
        };
        assert!(v.yt_dlp.unwrap().ends_with("site-packages"));
        assert_eq!(seen.len(), 1);
        assert_eq!((seen[0].downloaded, seen[0].total), (5, Some(10)));
    }

    #[tokio::test]
    async fn the_helper_sees_none_of_the_servers_environment() {
        std::env::set_var("SOULBEET_TEST_SECRET", "hunter2");
        let (answer, _) = ask(
            r#"
import json, os, sys
sys.stdin.readline()
print(json.dumps({"version": {"python": " ".join(sorted(os.environ))}}))
"#,
            SHORT,
        )
        .await;
        let Answer::Version(v) = answer.unwrap() else {
            panic!("not a version");
        };
        // LC_CTYPE is what Python itself sets for -X utf8.
        let names = v.python.unwrap();
        for name in names.split(' ') {
            assert!(
                ["HOME", "LANG", "LC_CTYPE", "TMPDIR"].contains(&name),
                "{names}"
            );
        }
    }

    #[tokio::test]
    async fn an_error_line_is_the_answer_cleaned_of_urls() {
        let (answer, _) = ask(
            r#"
import json, sys
sys.stdin.readline()
print(json.dumps({"error": {"kind": "forbidden", "message": "HTTP Error 403 at https://rr1.googlevideo.com/videoplayback?ip=198.51.100.4"}}))
"#,
            SHORT,
        )
        .await;
        let fault = answer.unwrap_err();
        assert_eq!(fault.kind, Kind::Forbidden);
        assert_eq!(fault.message, "HTTP Error 403 at <url>");
    }

    #[tokio::test]
    async fn garbage_on_stdout_is_unreadable() {
        let (answer, _) = ask(
            "import sys\nsys.stdin.readline()\nprint('[download] 12.5% of 2.0MiB')\n",
            SHORT,
        )
        .await;
        assert_eq!(answer.unwrap_err().kind, Kind::Unreadable);
    }

    #[tokio::test]
    async fn an_unknown_answer_is_unreadable() {
        let (answer, _) = ask(
            "import sys\nsys.stdin.readline()\nprint('{\"shrug\": 1}')\n",
            SHORT,
        )
        .await;
        assert_eq!(answer.unwrap_err().kind, Kind::Unreadable);
    }

    #[tokio::test]
    async fn a_helper_that_dies_says_what_it_last_said_without_its_urls() {
        let (answer, _) = ask(
            r#"
import os, sys
sys.stdin.readline()
sys.stderr.write("Traceback (most recent call last):\n")
sys.stderr.write("KeyError: 'streamingData' from https://www.youtube.com/youtubei/v1/player?key=k\n")
sys.stderr.flush()
os._exit(3)
"#,
            SHORT,
        )
        .await;
        let fault = answer.unwrap_err();
        assert_eq!(fault.kind, Kind::Internal);
        assert!(
            fault.message.contains("exit status: 3"),
            "{}",
            fault.message
        );
        assert!(
            fault
                .message
                .ends_with("KeyError: 'streamingData' from <url>"),
            "{}",
            fault.message
        );
    }

    #[tokio::test]
    async fn a_helper_that_hangs_times_out_and_its_group_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let script = format!(
            r#"
import os, subprocess, sys, time
sys.stdin.readline()
child = subprocess.Popen(["sleep", "60"])
open({pids:?}, "w").write(f"{{os.getpid()}} {{child.pid}}")
time.sleep(60)
"#,
            pids = pids.display().to_string()
        );
        let helper = helper_of(&script, dir.path());
        let started = std::time::Instant::now();
        let limits = Limits {
            idle: Duration::from_secs(2),
            total: Duration::from_secs(20),
        };
        let fault = helper
            .call(&Request::Version, limits, &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(fault.kind, Kind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(15));
        let pids: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap()
            .split(' ')
            .map(|p| p.parse().unwrap())
            .collect();
        for pid in pids {
            assert!(gone(pid).await, "pid {pid} survived the call");
        }
    }

    /// Whether `pid` is gone within 5 s: reaping a killed process can lag on a loaded machine.
    pub(crate) async fn gone(pid: i32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 only asks whether the process exists.
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            // A zombie still answers; its stat line says so.
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.split(' ').nth(2) == Some("Z"));
            if !alive || zombie {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_dropped_call_kills_the_helpers_group() {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let script = format!(
            r#"
import os, subprocess, sys, time
sys.stdin.readline()
child = subprocess.Popen(["sleep", "60"])
open({pids:?}, "w").write(f"{{os.getpid()}} {{child.pid}}")
time.sleep(60)
"#,
            pids = pids.display().to_string()
        );
        let helper = helper_of(&script, dir.path());
        let task = tokio::spawn(async move {
            helper
                .call(&Request::Version, Limits::SEARCH, &mut |_| {})
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pids: Vec<i32> = loop {
            let text = std::fs::read_to_string(&pids).unwrap_or_default();
            let read: Vec<i32> = text.split(' ').filter_map(|p| p.parse().ok()).collect();
            if read.len() == 2 {
                break read;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        task.abort();
        let _ = task.await;
        for pid in pids {
            assert!(gone(pid).await, "pid {pid} survived the dropped call");
        }
    }

    #[tokio::test]
    async fn a_python_that_is_not_there_is_missing_tooling() {
        let dir = tempfile::tempdir().unwrap();
        let mut helper = helper_of("", dir.path());
        helper.tools.python = dir.path().join("no-python");
        let fault = helper
            .call(&Request::Version, SHORT, &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(fault.kind, Kind::Tooling);
        assert!(
            fault.message.starts_with("Cannot run "),
            "{}",
            fault.message
        );
    }

    #[test]
    fn requests_name_their_command_and_carry_the_tools() {
        let request = Request::Fetch {
            video_id: "MdzF0rFOC9Y".into(),
            staging_dir: "/d/.soulful-partial/i/ytdlp/t1".into(),
            min_abr: 96,
        };
        let envelope = Envelope {
            site_packages: Path::new("/opt/ytdlp/lib/python3.11/site-packages"),
            deno: Path::new("/opt/ytdlp/bin/deno"),
            ffmpeg: Some(Path::new("/usr/local/bin/ffmpeg")),
            cache_dir: Path::new("/d/.soulful-partial/i/ytdlp/cache"),
            request: &request,
        };
        assert_eq!(
            serde_json::to_value(&envelope).unwrap(),
            serde_json::json!({
                "command": "fetch",
                "video_id": "MdzF0rFOC9Y",
                "staging_dir": "/d/.soulful-partial/i/ytdlp/t1",
                "min_abr": 96,
                "site_packages": "/opt/ytdlp/lib/python3.11/site-packages",
                "deno": "/opt/ytdlp/bin/deno",
                "ffmpeg": "/usr/local/bin/ffmpeg",
                "cache_dir": "/d/.soulful-partial/i/ytdlp/cache",
            })
        );
    }

    #[test]
    fn relative_tool_paths_are_read_from_the_working_folder() {
        let cwd = std::env::current_dir().unwrap();
        let tools = Tools {
            python: "/usr/bin/python3".into(),
            site_packages: "venv/site-packages".into(),
            deno: "bin/deno".into(),
            ffmpeg: Some("ffmpeg".into()),
        }
        .absolute()
        .unwrap();
        assert_eq!(tools.python, PathBuf::from("/usr/bin/python3"));
        assert_eq!(tools.site_packages, cwd.join("venv/site-packages"));
        assert_eq!(tools.deno, cwd.join("bin/deno"));
        assert_eq!(tools.ffmpeg, Some(cwd.join("ffmpeg")));
    }

    #[test]
    fn versions_name_each_missing_piece() {
        let v = Versions {
            python: Some("3.11.2".into()),
            yt_dlp: Some("2026.11.23".into()),
            ejs: Some("0.8.0".into()),
            ytmusicapi: Some("1.12.4".into()),
            deno: None,
            ffmpeg: None,
        };
        assert_eq!(v.missing(), ["deno", "ffmpeg"]);
        assert_eq!(Versions::default().missing().len(), 5);
    }
}
