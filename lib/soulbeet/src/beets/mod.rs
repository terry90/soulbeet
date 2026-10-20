mod classify;
mod library;
mod probe;
mod runner;

pub use probe::{python_from_shebang, resolve_on_path};
pub use runner::{run_beet, RunEnd, RunLimits, RunOutcome, RunSpec, Stream};
pub use shared::library::{DuplicateGroup, DuplicateReport, LibraryTrack};

use crate::audio::count_audio_files;
use crate::{ImportMode, ImportOutcome, ImportResult, MusicImporter};
use classify::{parse_run_log, Classifier};
use probe::{assess, fingerprint, run_probe, Fingerprint, ProbeFailure, ProbeReport};
use shared::system::ImporterHealth;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Where beets is and how to run it. Resolved once at startup.
#[derive(Debug, Clone)]
pub struct BeetsSettings {
    /// The `-c` config file.
    pub config: PathBuf,
    /// The `beet` executable.
    pub beet: PathBuf,
    /// The interpreter that runs `beet`, for the config check.
    pub python: Option<PathBuf>,
    pub limits: RunLimits,
}

#[derive(Default)]
struct HealthState {
    health: ImporterHealth,
    /// The last report the probe produced, kept through later failed checks.
    report: Option<ProbeReport>,
    /// None until the first check finished.
    fingerprint: Option<Fingerprint>,
    /// Bumped by every finished check.
    generation: u64,
    /// When the last check timed out. None if it finished.
    timed_out_at: Option<Instant>,
}

/// A timed-out check says nothing about the config, so it is repeated after
/// this long even when no file changed.
const TIMEOUT_RECHECK: Duration = Duration::from_secs(5 * 60);

pub struct BeetsImporter {
    settings: BeetsSettings,
    state: std::sync::Mutex<HealthState>,
    /// Held for the whole of a config check, so only one runs at a time.
    refresh_gate: tokio::sync::Mutex<()>,
}

impl BeetsImporter {
    pub fn new(settings: BeetsSettings) -> Self {
        BeetsImporter {
            settings,
            state: Default::default(),
            refresh_gate: Default::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HealthState> {
        // The state stays consistent even if a holder panicked: every write is
        // a plain field assignment.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The `--log` file of one import run, removed when dropped.
struct RunLog(PathBuf);

impl RunLog {
    fn create() -> std::io::Result<RunLog> {
        let dir = std::env::temp_dir();
        loop {
            let path = dir.join(format!("soulbeet-import-{}.log", rand::random::<u64>()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(RunLog(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for RunLog {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            warn!("Could not remove beets run log {}: {e}", self.0.display());
        }
    }
}

fn import_args(
    s: &BeetsSettings,
    sources: &[&Path],
    target: &Path,
    mode: ImportMode,
    run_log: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-v".into(),
        "-c".into(),
        s.config.clone().into(),
        "-l".into(),
        library::library_db(target).into(),
        "-d".into(),
        target.into(),
        "import".into(),
        "-q".into(),
        // Every source is a batch folder only a retry imports again, so the
        // user's import history and resume state could only make beets skip it.
        "--noincremental".into(),
        "--noresume".into(),
        "--log".into(),
        run_log.into(),
    ];
    if mode == ImportMode::Singleton {
        args.push("-s".into());
    }
    args.extend(sources.iter().map(|p| p.as_os_str().to_owned()));
    args
}

fn outcome_kind(outcome: &ImportOutcome) -> &'static str {
    match outcome {
        ImportOutcome::Imported { .. } => "imported",
        ImportOutcome::Skipped(_) => "skipped",
        ImportOutcome::LookupFailed { .. } => "lookup failed",
        ImportOutcome::Failed { .. } => "failed",
        ImportOutcome::TimedOut { .. } => "timed out",
        ImportOutcome::Refused { .. } => "refused",
    }
}

#[async_trait::async_trait]
impl MusicImporter for BeetsImporter {
    fn id(&self) -> &'static str {
        "beets"
    }

    fn name(&self) -> &'static str {
        "Beets"
    }

    async fn import(&self, sources: &[&Path], target: &Path, mode: ImportMode) -> ImportResult {
        // beets prompts instead of creating a missing library db directory.
        if let Err(e) = tokio::fs::create_dir_all(target).await {
            return ImportResult::failed(format!(
                "Could not create library folder {}: {e}",
                target.display()
            ));
        }
        let submitted = count_audio_files(sources).await;
        let run_log = match RunLog::create() {
            Ok(log) => log,
            Err(e) => {
                return ImportResult::failed(format!("Could not create the beets run log: {e}"))
            }
        };
        let s = &self.settings;
        let autotag_off = self.state().report.as_ref().is_some_and(|r| !r.autotag);

        info!(
            "beets import into {} ({mode:?}): {:?}",
            target.display(),
            sources
        );
        let started = Instant::now();
        let mut classifier = Classifier::new(mode, submitted, autotag_off);
        let spec = RunSpec {
            program: &s.beet,
            args: import_args(s, sources, target, mode, &run_log.0),
            limits: s.limits,
        };
        let run = run_beet(spec, &mut |stream, line| classifier.feed(stream, line)).await;
        let entries = match tokio::fs::read_to_string(&run_log.0).await {
            Ok(text) => parse_run_log(&text),
            Err(e) => {
                warn!("Could not read beets run log {}: {e}", run_log.0.display());
                Vec::new()
            }
        };
        drop(run_log);

        let result = classifier.finish(&run.end, &s.limits, &entries, run.tail);
        info!(
            "beets import into {} {} after {:.1}s: {result}",
            target.display(),
            outcome_kind(&result.outcome),
            started.elapsed().as_secs_f64()
        );
        if matches!(
            result.outcome,
            ImportOutcome::Failed { .. }
                | ImportOutcome::TimedOut { .. }
                | ImportOutcome::LookupFailed { .. }
        ) {
            warn!("beets output tail:\n{}", result.output_tail);
        }
        result
    }

    async fn find_duplicates(&self, libraries: &[&Path]) -> Result<DuplicateReport, String> {
        library::find_duplicates_across_libraries(&self.settings, libraries).await
    }

    fn health(&self) -> ImporterHealth {
        self.state().health.clone()
    }

    async fn refresh_health(&self) -> ImporterHealth {
        let seen = self.state().generation;
        let _gate = self.refresh_gate.lock().await;
        let (previous, last_report) = {
            let state = self.state();
            if state.generation != seen {
                // A check finished while this caller waited for the gate.
                return state.health.clone();
            }
            (state.health.clone(), state.report.clone())
        };

        let s = &self.settings;
        // Taken before the probe, so an edit made while it runs still reads
        // as a change on the next poll.
        let before = fingerprint(&s.config, last_report.as_ref()).await;
        let probe = run_probe(s.python.as_deref(), &s.config).await;
        let health = assess(&s.config, &probe);
        let timed_out = matches!(probe, Err(ProbeFailure::Timeout));
        // A failed check says nothing about which files matter, so the last
        // report keeps naming them.
        let report = probe.ok().or(last_report);
        let after = fingerprint(&s.config, report.as_ref())
            .await
            .backdated(&before);

        if health.level != previous.level {
            info!(
                "beets health: {} -> {}",
                previous.level.chip(),
                health.level.chip()
            );
        }
        if health.issues != previous.issues {
            for issue in &health.issues {
                warn!("beets: {} ({})", issue.title, issue.detail);
            }
        }

        let mut state = self.state();
        state.health = health.clone();
        state.report = report;
        state.fingerprint = Some(after);
        state.timed_out_at = timed_out.then(Instant::now);
        state.generation += 1;
        health
    }

    async fn health_stale(&self) -> bool {
        if self.refresh_gate.try_lock().is_err() {
            return false;
        }
        let (stored, report, timed_out_at) = {
            let state = self.state();
            (
                state.fingerprint.clone(),
                state.report.clone(),
                state.timed_out_at,
            )
        };
        if timed_out_at.is_some_and(|at| at.elapsed() >= TIMEOUT_RECHECK) {
            return true;
        }
        match stored {
            None => true,
            Some(stored) => fingerprint(&self.settings.config, report.as_ref()).await != stored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SkipReason;
    use shared::system::HealthLevel;
    use tokio::io::AsyncWriteExt;

    const SHORT: RunLimits = RunLimits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(30),
        drain: Duration::from_secs(1),
    };

    /// Writes an executable script. A child process writes it, so no fd in
    /// this process ever has it open for writing: a fork from another test
    /// thread would inherit such an fd and exec would fail with ETXTBSY.
    async fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("cat > \"$0\" && chmod 755 \"$0\"")
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .await
            .unwrap();
        drop(stdin);
        assert!(child.wait().await.unwrap().success());
        path
    }

    fn settings(dir: &Path, beet: PathBuf, python: Option<PathBuf>) -> BeetsSettings {
        let config = dir.join("config.yaml");
        std::fs::write(&config, "plugins: musicbrainz\n").unwrap();
        BeetsSettings {
            config,
            beet,
            python,
            limits: SHORT,
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn ready_report(pluginpath: &str) -> String {
        format!(
            r#"{{"fatal": null, "loaded": ["musicbrainz"], "plugin_errors": [], "metadata_sources": ["MusicBrainz"], "user_config_path": null, "pluginpath": ["{pluginpath}"], "autotag": true}}"#
        )
    }

    /// A python that records each call in calls.txt, fails while `fail`
    /// exists, and otherwise prints a Ready report naming `plugins` as the
    /// pluginpath.
    async fn fake_python(dir: &Path, delay: &str) -> PathBuf {
        let d = dir.display();
        let report = ready_report(&dir.join("plugins").to_string_lossy());
        script(
            dir,
            "python",
            &format!(
                "echo call >> '{d}/calls.txt'; sleep {delay}; \
                 [ -e '{d}/fail' ] && {{ echo broken >&2; exit 1; }}; echo '{report}'"
            ),
        )
        .await
    }

    async fn importer_with_python(dir: &Path, delay: &str) -> BeetsImporter {
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        let python = fake_python(dir, delay).await;
        BeetsImporter::new(settings(
            dir,
            PathBuf::from("/nonexistent/beet"),
            Some(python),
        ))
    }

    #[tokio::test]
    async fn import_passes_run_log_and_mode_flags() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let beet = script(
            d,
            "beet",
            &format!(
                "printf '%s\\n' \"$@\" > '{}/args.txt'; \
                 while [ $# -gt 0 ]; do \
                   [ \"$1\" = --log ] && echo 'skip /src/a' > \"$2\"; shift; \
                 done; exit 0",
                d.display()
            ),
        )
        .await;
        let s = settings(d, beet, None);
        let importer = BeetsImporter::new(s.clone());
        let target = d.join("lib");
        let src = d.join("src");

        for mode in [ImportMode::Singleton, ImportMode::Album] {
            let result = importer.import(&[&src], &target, mode).await;
            assert_eq!(
                result.outcome,
                ImportOutcome::Skipped(SkipReason::NoMatch { candidates: None })
            );

            let args = lines(&d.join("args.txt"));
            let db = target.join(".beets_library.db");
            let expected_head = [
                "-v",
                "-c",
                &s.config.to_string_lossy(),
                "-l",
                &db.to_string_lossy(),
                "-d",
                &target.to_string_lossy(),
                "import",
                "-q",
                "--noincremental",
                "--noresume",
                "--log",
            ]
            .map(str::to_string);
            assert_eq!(args[..12], expected_head);

            let log = PathBuf::from(&args[12]);
            assert_eq!(log.parent(), Some(std::env::temp_dir().as_path()));
            let name = log.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with("soulbeet-import-") && name.ends_with(".log"),
                "{name}"
            );
            assert!(!log.exists(), "run log left behind");

            let rest = &args[13..];
            let src = src.to_string_lossy().into_owned();
            match mode {
                ImportMode::Singleton => assert_eq!(rest, ["-s".to_string(), src]),
                ImportMode::Album => assert_eq!(rest, [src]),
            }
        }
    }

    #[tokio::test]
    async fn import_creates_a_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let beet = script(d, "beet", "exit 0").await;
        let importer = BeetsImporter::new(settings(d, beet, None));

        let target = d.join("new/library");
        importer
            .import(&[&d.join("src")], &target, ImportMode::Album)
            .await;
        assert!(target.is_dir());

        let blocked = d.join("config.yaml").join("library");
        let result = importer
            .import(&[&d.join("src")], &blocked, ImportMode::Album)
            .await;
        let text = result.to_string();
        assert!(
            text.starts_with(&format!(
                "Could not create library folder {}: ",
                blocked.display()
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn health_is_checking_until_the_first_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0").await;

        assert_eq!(importer.health().level, HealthLevel::Checking);
        assert!(importer.health_stale().await);

        let health = importer.refresh_health().await;
        assert_eq!(health.level, HealthLevel::Ready, "{health:?}");
        assert_eq!(importer.health(), health);
        assert!(!importer.health_stale().await);
    }

    #[tokio::test]
    async fn concurrent_refreshes_run_one_probe() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0.3").await;

        let results = futures::future::join_all((0..8).map(|_| importer.refresh_health())).await;
        assert!(results.iter().all(|h| h.level == HealthLevel::Ready));
        assert_eq!(lines(&dir.path().join("calls.txt")).len(), 1);
    }

    #[tokio::test]
    async fn health_is_not_stale_while_a_refresh_runs() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0.5").await;

        let refresh = importer.refresh_health();
        let check = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            importer.health_stale().await
        };
        let (_, stale) = tokio::join!(refresh, check);
        assert!(!stale);
    }

    #[tokio::test]
    async fn health_is_stale_after_the_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0").await;
        importer.refresh_health().await;
        assert!(!importer.health_stale().await);

        let config = &importer.settings.config;
        let tmp = dir.path().join("config.yaml.new");
        std::fs::write(&tmp, "plugins: musicbrainz chroma\n").unwrap();
        std::fs::rename(&tmp, config).unwrap();
        assert!(importer.health_stale().await);

        importer.refresh_health().await;
        assert!(!importer.health_stale().await);
        std::fs::write(dir.path().join("plugins/new.py"), "").unwrap();
        assert!(importer.health_stale().await);
    }

    #[tokio::test]
    async fn a_timed_out_check_goes_stale_after_the_recheck_time() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0").await;
        importer.refresh_health().await;
        assert!(!importer.health_stale().await);

        importer.state().timed_out_at = Some(Instant::now());
        assert!(!importer.health_stale().await);
        let long_ago = Instant::now()
            .checked_sub(TIMEOUT_RECHECK + Duration::from_secs(1))
            .expect("uptime shorter than the recheck time");
        importer.state().timed_out_at = Some(long_ago);
        assert!(importer.health_stale().await);

        importer.refresh_health().await;
        assert!(!importer.health_stale().await);
    }

    #[tokio::test]
    async fn a_failed_check_still_watches_the_last_known_plugin_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let importer = importer_with_python(dir.path(), "0").await;
        importer.refresh_health().await;

        std::fs::write(dir.path().join("fail"), "").unwrap();
        let health = importer.refresh_health().await;
        assert_eq!(health.level, HealthLevel::Broken, "{health:?}");
        assert!(!importer.health_stale().await);

        std::fs::write(dir.path().join("plugins/fixed.py"), "").unwrap();
        assert!(importer.health_stale().await);
    }

    #[tokio::test]
    async fn query_library_passes_the_library_dir_and_skips_prompt_noise() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let beet = script(
            d,
            "beet",
            &format!(
                "printf '%s\\n' \"$@\" > '{}/args.txt'; \
                 echo 'Enter the code: @@sb@@/m/a.flac|||A|||T|||Al|||AA'; \
                 echo noise; \
                 echo '@@sb@@/m/b.flac|||B|||T2|||Al|||AA'",
                d.display()
            ),
        )
        .await;
        let s = settings(d, beet, None);
        let root = d.join("music");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".beets_library.db"), "").unwrap();

        let tracks = library::query_library(&s, &root).await.unwrap();

        let args = lines(&d.join("args.txt"));
        let at = args.iter().position(|a| a == "-d").expect("no -d");
        assert_eq!(args[at + 1], root.to_string_lossy());
        assert_eq!(tracks.len(), 2);
        let paths: Vec<&str> = tracks.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, ["/m/a.flac", "/m/b.flac"]);
        assert_eq!(tracks[1].artist, "B");
        assert_eq!(tracks[1].album_artist, "AA");
    }

    #[tokio::test]
    async fn query_library_retries_once_when_the_db_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let beet = script(
            d,
            "beet",
            &format!(
                "if [ ! -e '{d}/locked-once' ]; then : > '{d}/locked-once'; \
                   echo 'database access error: database is locked' >&2; exit 1; fi; \
                 echo '@@sb@@/m/a.flac|||A|||T|||Al|||AA'",
                d = d.display()
            ),
        )
        .await;
        let s = settings(d, beet, None);
        std::fs::write(d.join(".beets_library.db"), "").unwrap();

        let tracks = library::query_library(&s, d).await.unwrap();
        assert_eq!(tracks.len(), 1);
    }

    #[tokio::test]
    async fn query_library_without_a_db_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path(), PathBuf::from("/nonexistent/beet"), None);
        assert!(library::query_library(&s, dir.path())
            .await
            .unwrap()
            .is_empty());
    }
}
