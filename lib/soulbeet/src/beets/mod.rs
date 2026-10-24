mod classify;
mod library;
mod probe;
mod report;
mod runner;

pub use probe::{python_from_shebang, resolve_on_path};
pub use runner::{run_beet, RunEnd, RunLimits, RunOutcome, RunSpec, Stream};
pub use shared::library::{DuplicateGroup, DuplicateReport, LibraryTrack};

use crate::audio::count_audio_files;
use crate::{DriverChoice, ImportMode, ImportOutcome, ImportResult, MusicImporter};
use classify::{parse_run_log, Classifier};
use probe::{assess, fingerprint, run_probe, Fingerprint, ProbeFailure, ProbeReport};
use report::Report;
use shared::system::ImporterHealth;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const IMPORT_PY: &str = include_str!("import.py");

/// Where beets is and how to run it. Resolved once at startup.
#[derive(Debug, Clone)]
pub struct BeetsSettings {
    /// The `-c` config file.
    pub config: PathBuf,
    /// The `beet` executable.
    pub beet: PathBuf,
    /// The interpreter that runs `beet`, for the config check and imports.
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

/// A file of one import run in the temp dir, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn create(extension: &str) -> std::io::Result<TempFile> {
        let dir = std::env::temp_dir();
        loop {
            let name = format!("soulbeet-import-{}.{extension}", rand::random::<u64>());
            let path = dir.join(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(TempFile(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            warn!("Could not remove {}: {e}", self.0.display());
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

/// Runs import.py, which runs beets' own CLI with `beet_args`.
fn driver_args(report: &Path, choice: &DriverChoice, beet_args: Vec<OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-P".into(), "-c".into(), IMPORT_PY.into(), report.into()];
    match choice {
        DriverChoice::Auto => args.push("auto".into()),
        DriverChoice::AsIs => args.push("asis".into()),
        DriverChoice::Pick { data_source, id } => {
            args.extend(["pick".into(), data_source.into(), id.into()])
        }
    }
    args.push("--".into());
    args.extend(beet_args);
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

    async fn import(
        &self,
        sources: &[&Path],
        target: &Path,
        mode: ImportMode,
        choice: &DriverChoice,
    ) -> ImportResult {
        let s = &self.settings;
        let Some(python) = s.python.as_deref() else {
            return ImportResult::failed(
                "Cannot determine the Python that runs beet. Set BEETS_PYTHON.".into(),
            );
        };
        // beets prompts instead of creating a missing library db directory.
        if let Err(e) = tokio::fs::create_dir_all(target).await {
            return ImportResult::failed(format!(
                "Could not create library folder {}: {e}",
                target.display()
            ));
        }
        let submitted = count_audio_files(sources).await;
        let (run_log, report_file) = match (TempFile::create("log"), TempFile::create("json")) {
            (Ok(log), Ok(report)) => (log, report),
            (Err(e), _) | (_, Err(e)) => {
                return ImportResult::failed(format!("Could not create the beets run files: {e}"))
            }
        };
        // -A leaves no marker in beets' output.
        let autotag_off = *choice == DriverChoice::AsIs
            || self.state().report.as_ref().is_some_and(|r| !r.autotag);

        info!(
            "beets import into {} ({mode:?}, {choice:?}): {:?}",
            target.display(),
            sources
        );
        let started = Instant::now();
        let mut classifier = Classifier::new(mode, submitted, autotag_off);
        let beet_args = import_args(s, sources, target, mode, &run_log.0);
        let spec = RunSpec {
            program: python,
            args: driver_args(&report_file.0, choice, beet_args),
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
        let report = read_report(&run.end, &report_file.0).await;
        drop(report_file);

        let result = classifier.finish(&run.end, &s.limits, &entries, run.tail);
        let result = match report {
            Some(report) => report.apply(result, mode, choice),
            None => result,
        };
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

/// The driver's report of a run that exited on its own. A driver that was
/// killed or never started writes none, so only a missing or invalid report
/// after an exit is worth a warning.
async fn read_report(end: &RunEnd, path: &Path) -> Option<Report> {
    if !matches!(end, RunEnd::Exited(status) if status.code().is_some()) {
        return None;
    }
    let report = match tokio::fs::read_to_string(path).await {
        Ok(text) => Report::parse(&text).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    report
        .inspect_err(|e| {
            warn!("No usable report from the beets import, judging it by its output: {e}")
        })
        .ok()
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
            r#"{{"fatal": null, "loaded": ["musicbrainz"], "plugin_errors": [], "metadata_sources": ["MusicBrainz"], "user_config_path": null, "pluginpath": ["{pluginpath}"], "autotag": true, "import_hook": true}}"#
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

    /// What the fake driver does on its next run.
    #[derive(Default)]
    struct DriverRun<'a> {
        stdout: &'a str,
        stderr: &'a str,
        runlog: &'a str,
        /// Written to the report path; None leaves the report as created.
        report: Option<&'a str>,
    }

    /// A python standing in for import.py: records its args NUL-separated in
    /// `args`, then plays back what `stage` left in the dir.
    async fn fake_driver(dir: &Path) -> PathBuf {
        script(
            dir,
            "python",
            &format!(
                "d='{}'; printf '%s\\0' \"$@\" > \"$d/args\"; report=$4; \
                 while [ $# -gt 0 ]; do [ \"$1\" = --log ] && cp \"$d/runlog\" \"$2\"; shift; done; \
                 [ -e \"$d/report\" ] && cp \"$d/report\" \"$report\"; \
                 cat \"$d/stdout\"; cat \"$d/stderr\" >&2; exit 0",
                dir.display()
            ),
        )
        .await
    }

    fn stage(dir: &Path, run: DriverRun) {
        std::fs::write(dir.join("stdout"), run.stdout).unwrap();
        std::fs::write(dir.join("stderr"), run.stderr).unwrap();
        std::fs::write(dir.join("runlog"), run.runlog).unwrap();
        match run.report {
            Some(report) => std::fs::write(dir.join("report"), report).unwrap(),
            None => {
                let _ = std::fs::remove_file(dir.join("report"));
            }
        }
    }

    fn driver_args_seen(dir: &Path) -> Vec<String> {
        let raw = std::fs::read(dir.join("args")).unwrap();
        let mut args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8(a.to_vec()).unwrap())
            .collect();
        assert_eq!(args.pop().as_deref(), Some(""));
        args
    }

    async fn driver_importer(dir: &Path) -> BeetsImporter {
        let python = fake_driver(dir).await;
        BeetsImporter::new(settings(
            dir,
            PathBuf::from("/nonexistent/beet"),
            Some(python),
        ))
    }

    /// A weak singleton run as beets prints it.
    const SKIPPED: DriverRun = DriverRun {
        stdout: "Skipping.\n",
        stderr: "Sending event: import_task_created\nFound 2 candidates.\n",
        runlog: "skip /src/a.mp3\n",
        report: None,
    };

    const TRACK_REPORT: &str = r#"{"tasks": [{"paths": ["/src/a.mp3"], "album": false, "candidates": [
        {"data_source": "MusicBrainz", "id": "dfeb96a7-f78a-4b04-93ce-c39e28c9a173",
         "artist": "Radiohead", "title": "Karma Police", "album": null, "year": null,
         "country": null, "disambig": null, "per_mille": 714, "differs": ["length"],
         "length": 263}]}], "pick_missing": false, "refused": null}"#;

    fn file_name(path: &str) -> String {
        Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn import_runs_the_driver_with_the_choice_and_the_beet_args() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        let s = &importer.settings;
        let target = d.join("lib");
        let src = d.join("src");
        let pick = DriverChoice::Pick {
            data_source: "MusicBrainz".into(),
            id: "dfeb96a7-f78a-4b04-93ce-c39e28c9a173".into(),
        };

        for (choice, words) in [
            (DriverChoice::Auto, vec!["auto"]),
            (DriverChoice::AsIs, vec!["asis"]),
            (
                pick,
                vec![
                    "pick",
                    "MusicBrainz",
                    "dfeb96a7-f78a-4b04-93ce-c39e28c9a173",
                ],
            ),
        ] {
            for mode in [ImportMode::Singleton, ImportMode::Album] {
                stage(d, SKIPPED);
                let result = importer.import(&[&src], &target, mode, &choice).await;
                assert_eq!(
                    result.outcome,
                    ImportOutcome::Skipped(SkipReason::NoMatch {
                        candidates: Some(2)
                    })
                );

                let args = driver_args_seen(d);
                assert_eq!(args[..3], ["-P", "-c", IMPORT_PY].map(str::to_string));
                let report = PathBuf::from(&args[3]);
                assert_eq!(report.parent(), Some(std::env::temp_dir().as_path()));
                let name = file_name(&args[3]);
                assert!(
                    name.starts_with("soulbeet-import-") && name.ends_with(".json"),
                    "{name}"
                );
                assert!(!report.exists(), "report left behind");

                let after = 4 + words.len();
                assert_eq!(args[4..after], words[..]);
                assert_eq!(args[after], "--");

                let beet_args = &args[after + 1..];
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
                assert_eq!(beet_args[..12], expected_head);

                let log = PathBuf::from(&beet_args[12]);
                assert_eq!(log.parent(), Some(std::env::temp_dir().as_path()));
                let name = file_name(&beet_args[12]);
                assert!(
                    name.starts_with("soulbeet-import-") && name.ends_with(".log"),
                    "{name}"
                );
                assert!(!log.exists(), "run log left behind");

                let rest = &beet_args[13..];
                let src = src.to_string_lossy().into_owned();
                match mode {
                    ImportMode::Singleton => assert_eq!(rest, ["-s".to_string(), src]),
                    ImportMode::Album => assert_eq!(rest, [src]),
                }
            }
        }
    }

    #[tokio::test]
    async fn a_skipped_automatic_import_carries_the_reported_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        let src = d.join("src");

        stage(
            d,
            DriverRun {
                report: Some(TRACK_REPORT),
                ..SKIPPED
            },
        );
        let result = importer
            .import(
                &[&src],
                &d.join("lib"),
                ImportMode::Singleton,
                &DriverChoice::Auto,
            )
            .await;
        assert_eq!(result.to_string(), "No close match (2 candidates)");
        let choices = result.choices.expect("no choices");
        assert_eq!(choices.kind, shared::download::MatchKind::Track);
        let [candidate] = &choices.candidates[..] else {
            panic!("{choices:?}")
        };
        assert_eq!(candidate.title, "Karma Police");
        assert_eq!(candidate.per_mille, 714);

        // The same report from an as-is run offers nothing.
        let result = importer
            .import(
                &[&src],
                &d.join("lib"),
                ImportMode::Singleton,
                &DriverChoice::AsIs,
            )
            .await;
        assert_eq!(result.choices, None);
    }

    #[tokio::test]
    async fn a_pick_the_source_does_not_have_fails_with_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        let id = "dfeb96a7-f78a-4b04-93ce-c39e28c9a173";
        let pick = DriverChoice::Pick {
            data_source: "MusicBrainz".into(),
            id: id.into(),
        };

        stage(
            d,
            DriverRun {
                stdout: "",
                stderr: "Sending event: import_task_created\n\
                         soulbeet: MusicBrainz has no single match for dfeb96a7 (0 found)\n",
                report: Some(r#"{"tasks": [], "pick_missing": true, "refused": null}"#),
                ..SKIPPED
            },
        );
        let result = importer
            .import(
                &[&d.join("src")],
                &d.join("lib"),
                ImportMode::Singleton,
                &pick,
            )
            .await;
        assert_eq!(
            result.outcome,
            ImportOutcome::Failed {
                message: format!("MusicBrainz has no recording {id}")
            }
        );
        assert_eq!(result.choices, None);
    }

    #[tokio::test]
    async fn as_is_imports_are_untagged_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        let src = d.join("src");
        let lib = d.join("lib");

        // -A prints no marker; only the choice says the import was as-is.
        stage(
            d,
            DriverRun {
                stderr: "Sending event: import_task_created\nSending event: item_imported\n",
                report: Some(r#"{"tasks": [], "pick_missing": false, "refused": null}"#),
                ..Default::default()
            },
        );
        let result = importer
            .import(&[&src], &lib, ImportMode::Singleton, &DriverChoice::AsIs)
            .await;
        assert_eq!(result.to_string(), "Imported as-is (untagged)");
        let result = importer
            .import(&[&src], &lib, ImportMode::Singleton, &DriverChoice::Auto)
            .await;
        assert_eq!(result.to_string(), "Imported");

        let why = "Import as-is needs artist and title tags, and 1 file lacks them: a.mp3";
        let refused = format!(r#"{{"tasks": [], "pick_missing": false, "refused": "{why}"}}"#);
        stage(
            d,
            DriverRun {
                stderr: &format!("soulbeet: {why}\n"),
                report: Some(&refused),
                ..Default::default()
            },
        );
        let result = importer
            .import(&[&src], &lib, ImportMode::Singleton, &DriverChoice::AsIs)
            .await;
        assert_eq!(
            result.outcome,
            ImportOutcome::Failed {
                message: why.into()
            }
        );
    }

    #[tokio::test]
    async fn without_a_usable_report_the_output_decides() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        let no_match = ImportOutcome::Skipped(SkipReason::NoMatch {
            candidates: Some(2),
        });

        for report in [None, Some("not json"), Some(r#"{"tasks": [{}]}"#)] {
            stage(d, DriverRun { report, ..SKIPPED });
            let result = importer
                .import(
                    &[&d.join("src")],
                    &d.join("lib"),
                    ImportMode::Singleton,
                    &DriverChoice::Auto,
                )
                .await;
            assert_eq!(result.outcome, no_match, "{report:?}");
            assert_eq!(result.choices, None);
        }
    }

    #[tokio::test]
    async fn only_a_run_that_exited_has_its_report_read() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        tokio::fs::write(
            &path,
            r#"{"tasks": [], "pick_missing": false, "refused": null}"#,
        )
        .await
        .unwrap();

        let exited = |raw| RunEnd::Exited(std::process::ExitStatus::from_raw(raw));
        // Exit code 1, as after a beets crash, still comes with a report.
        for code in [0, 1] {
            assert!(read_report(&exited(code << 8), &path).await.is_some());
        }
        for end in [
            // Killed by SIGKILL.
            exited(9),
            RunEnd::Idle,
            RunEnd::Total,
            RunEnd::SpawnFailed(std::io::Error::other("no python")),
        ] {
            assert!(read_report(&end, &path).await.is_none(), "{end:?}");
        }
    }

    #[tokio::test]
    async fn import_without_python_fails() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = BeetsImporter::new(settings(d, PathBuf::from("/nonexistent/beet"), None));
        let result = importer
            .import(
                &[&d.join("src")],
                &d.join("lib"),
                ImportMode::Album,
                &DriverChoice::Auto,
            )
            .await;
        assert_eq!(
            result.outcome,
            ImportOutcome::Failed {
                message: "Cannot determine the Python that runs beet. Set BEETS_PYTHON.".into()
            }
        );
        assert!(!d.join("lib").exists());
    }

    #[tokio::test]
    async fn import_creates_a_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let importer = driver_importer(d).await;
        stage(d, DriverRun::default());

        let target = d.join("new/library");
        importer
            .import(
                &[&d.join("src")],
                &target,
                ImportMode::Album,
                &DriverChoice::Auto,
            )
            .await;
        assert!(target.is_dir());

        let blocked = d.join("config.yaml").join("library");
        let result = importer
            .import(
                &[&d.join("src")],
                &blocked,
                ImportMode::Album,
                &DriverChoice::Auto,
            )
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
