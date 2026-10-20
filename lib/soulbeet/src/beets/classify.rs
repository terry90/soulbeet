//! Turns one beets import run into an [`ImportResult`].
//!
//! The two pipes do not keep their relative order, so only per-stream markers
//! and the `--log` run log are used, never adjacency across streams.

use crate::beets::runner::{strip_ansi, RunEnd, RunLimits, Stream};
use crate::{ImportMode, ImportOutcome, ImportResult, SkipReason, TaskCount, TimeoutKind};
use regex::Regex;
use std::os::unix::process::ExitStatusExt;
use std::sync::LazyLock;

static RE_ITEMS_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*) \((\d+) items\)$").unwrap());
static RE_CANDIDATES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:Evaluating|Found) (\d+) candidates\.$").unwrap());
static RE_ALREADY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^This (album|item) is already in the library!$").unwrap());
static RE_NOTHING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^No files imported from (.+)$").unwrap());
static RE_SEARCH_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\w+: )?Error searching (\S+): (.*)$").unwrap());
static RE_SOURCE_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Error in '([^.']+)\.\w+': (.*)$").unwrap());
static RE_TAG_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^error (reading|writing) ").unwrap());
static RE_FATAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:error|Error|configuration error|database access error|invalid query): ")
        .unwrap()
});
static RE_PLUGIN_LOAD_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*\* error loading plugin (\S+)$").unwrap());

const TASK_CREATED: &str = "Sending event: import_task_created";
const ALBUM_IMPORTED: &str = "Sending event: album_imported";
const ITEM_IMPORTED: &str = "Sending event: item_imported";
const TRACEBACK: &str = "Traceback (most recent call last):";
const CHAINED: [&str; 2] = [
    "The above exception was the direct cause of the following exception:",
    "During handling of the above exception, another exception occurred:",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunLogAction {
    Skip,
    Asis,
    DuplicateSkip,
    DuplicateKeep,
    DuplicateReplace,
}

/// One line of `beet import --log`: a task beets did not simply apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunLogEntry {
    pub action: RunLogAction,
    pub paths: Vec<String>,
}

pub(crate) fn parse_run_log(text: &str) -> Vec<RunLogEntry> {
    text.lines()
        .filter_map(|line| {
            let (action, paths) = line.split_once(' ')?;
            let action = match action {
                "skip" => RunLogAction::Skip,
                "asis" => RunLogAction::Asis,
                "duplicate-skip" => RunLogAction::DuplicateSkip,
                "duplicate-keep" => RunLogAction::DuplicateKeep,
                "duplicate-replace" => RunLogAction::DuplicateReplace,
                _ => return None,
            };
            Some(RunLogEntry {
                action,
                paths: paths.split("; ").map(str::to_string).collect(),
            })
        })
        .collect()
}

/// Where the stderr reader is relative to Python tracebacks.
#[derive(Debug)]
enum Traceback {
    Outside,
    /// Inside a traceback; `plugin` when it belongs to a plugin load failure.
    Inside {
        plugin: bool,
    },
    /// After a plugin load traceback, where a chained one may still follow.
    AfterPlugin,
}

pub(crate) struct Classifier {
    mode: ImportMode,
    submitted: usize,
    autotag_off: bool,
    tasks: TaskCount,
    /// Candidate count when every lookup reported the same one.
    candidates: Option<u32>,
    candidates_differ: bool,
    /// Sum of the album headers' `(N items)`, when any was printed.
    header_items: Option<u32>,
    stdout_skipping: bool,
    stdout_asis: bool,
    already_in_library: bool,
    nothing_imported_from: bool,
    lookup_failure: Option<(String, String)>,
    tag_errors: u32,
    fatal: Option<String>,
    pending_plugin_traceback: bool,
    traceback: Traceback,
    exception: Option<String>,
    last_line: Option<String>,
}

impl Classifier {
    pub(crate) fn new(mode: ImportMode, submitted: usize, autotag_off: bool) -> Self {
        Classifier {
            mode,
            submitted,
            autotag_off,
            tasks: TaskCount::default(),
            candidates: None,
            candidates_differ: false,
            header_items: None,
            stdout_skipping: false,
            stdout_asis: false,
            already_in_library: false,
            nothing_imported_from: false,
            lookup_failure: None,
            tag_errors: 0,
            fatal: None,
            pending_plugin_traceback: false,
            traceback: Traceback::Outside,
            exception: None,
            last_line: None,
        }
    }

    pub(crate) fn feed(&mut self, stream: Stream, line: &str) {
        let line = strip_ansi(line);
        let line = line.trim_end();
        if !line.trim_start().is_empty() {
            self.last_line = Some(line.to_string());
        }
        match stream {
            Stream::Stdout => self.feed_stdout(line),
            Stream::Stderr => self.feed_stderr(line),
        }
    }

    fn feed_stdout(&mut self, line: &str) {
        if line == "Skipping." {
            self.stdout_skipping = true;
        } else if line == "Importing as-is." {
            self.stdout_asis = true;
        } else if let Some(c) = RE_ITEMS_HEADER.captures(line) {
            let items: u32 = c[2].parse().unwrap_or(0);
            self.header_items = Some(self.header_items.unwrap_or(0).saturating_add(items));
        }
    }

    fn feed_stderr(&mut self, line: &str) {
        self.track_traceback(line);

        if line == TASK_CREATED {
            self.tasks.total += 1;
        } else if line == ALBUM_IMPORTED || line == ITEM_IMPORTED {
            self.tasks.imported += 1;
        } else if let Some(c) = RE_CANDIDATES.captures(line) {
            let n: u32 = c[1].parse().unwrap_or(0);
            match self.candidates {
                Some(seen) if seen != n => self.candidates_differ = true,
                _ => self.candidates = Some(n),
            }
        } else if RE_ALREADY.is_match(line) {
            self.already_in_library = true;
        } else if RE_NOTHING.is_match(line) {
            self.nothing_imported_from = true;
        } else if RE_TAG_ERROR.is_match(line) {
            self.tag_errors += 1;
        } else if RE_FATAL.is_match(line) {
            self.fatal = Some(line.to_string());
        } else if self.lookup_failure.is_none() {
            if let Some(c) = RE_SEARCH_ERROR
                .captures(line)
                .or_else(|| RE_SOURCE_ERROR.captures(line))
            {
                self.lookup_failure = Some((c[1].to_string(), c[2].to_string()));
            }
        }
    }

    /// Remembers the last exception line of a traceback that is not part of a
    /// `** error loading plugin` block: beets keeps going after those.
    fn track_traceback(&mut self, line: &str) {
        if RE_PLUGIN_LOAD_ERROR.is_match(line) {
            self.pending_plugin_traceback = true;
            self.traceback = Traceback::Outside;
            return;
        }
        if line == TRACEBACK {
            let plugin = std::mem::take(&mut self.pending_plugin_traceback)
                || matches!(self.traceback, Traceback::AfterPlugin);
            self.traceback = Traceback::Inside { plugin };
            return;
        }
        match self.traceback {
            Traceback::Inside { plugin } => {
                if line.is_empty() || line.starts_with(' ') {
                    return;
                }
                if plugin {
                    self.traceback = Traceback::AfterPlugin;
                } else {
                    self.exception = Some(line.to_string());
                    self.traceback = Traceback::Outside;
                }
            }
            Traceback::AfterPlugin => {
                if !line.is_empty() && !CHAINED.contains(&line) {
                    self.traceback = Traceback::Outside;
                }
            }
            Traceback::Outside => {}
        }
    }

    pub(crate) fn finish(
        self,
        end: &RunEnd,
        limits: &RunLimits,
        run_log: &[RunLogEntry],
        tail: String,
    ) -> ImportResult {
        let tasks = self.tasks;
        let outcome = match end {
            RunEnd::SpawnFailed(e) => ImportOutcome::Failed {
                message: format!("Could not start beets: {e}"),
            },
            RunEnd::Idle => ImportOutcome::TimedOut {
                limit: TimeoutKind::Idle(limits.idle),
                last_line: self.last_line,
            },
            RunEnd::Total => ImportOutcome::TimedOut {
                limit: TimeoutKind::Total(limits.total),
                last_line: self.last_line,
            },
            RunEnd::Exited(status) if !status.success() => {
                let message = self.fatal.or(self.exception).unwrap_or_else(|| {
                    match (status.code(), status.signal()) {
                        (Some(code), _) => format!("beets exited with code {code}"),
                        (None, Some(signal)) => format!("beets was killed by signal {signal}"),
                        (None, None) => format!("beets exited with {status}"),
                    }
                });
                ImportOutcome::Failed { message }
            }
            RunEnd::Exited(_) => self.exited_cleanly(run_log),
        };
        ImportResult {
            outcome,
            output_tail: tail,
            tasks,
        }
    }

    /// Precedence after exit 0: lookup failure, already in library, nothing to
    /// import, no match, imported. Lookup failure and nothing to import are not
    /// tied to a task, so they only count when something was left unimported.
    fn exited_cleanly(self, run_log: &[RunLogEntry]) -> ImportOutcome {
        let has = |action| run_log.iter().any(|e| e.action == action);
        let all_imported = self.tasks.imported > 0 && self.tasks.imported >= self.tasks.total;

        if let Some((source, message)) = self.lookup_failure.filter(|_| !all_imported) {
            return ImportOutcome::LookupFailed { source, message };
        }
        if self.already_in_library || has(RunLogAction::DuplicateSkip) {
            return ImportOutcome::Skipped(SkipReason::AlreadyInLibrary);
        }
        if self.nothing_imported_from && self.tasks.imported == 0 {
            return ImportOutcome::Skipped(SkipReason::NothingToImport);
        }
        if has(RunLogAction::Skip) || self.stdout_skipping {
            let candidates = self.candidates.filter(|_| !self.candidates_differ);
            return ImportOutcome::Skipped(SkipReason::NoMatch { candidates });
        }
        if self.tasks.imported == 0 {
            if self.tasks.total == 0 {
                return ImportOutcome::Skipped(SkipReason::NothingToImport);
            }
            // Tasks ran, none was applied, and beets gave no reason.
            return ImportOutcome::Failed {
                message: "beets finished without importing anything".to_string(),
            };
        }

        let mut warnings = Vec::new();
        if self.tag_errors > 0 {
            warnings.push(format!(
                "beets could not write tags to {} file(s)",
                self.tag_errors
            ));
        }
        let taken = match self.mode {
            ImportMode::Album => self.header_items,
            ImportMode::Singleton => Some(self.tasks.imported),
        };
        if let Some(taken) = taken {
            let dropped = self.submitted.saturating_sub(taken as usize);
            if dropped > 0 {
                warnings.push(format!("{dropped} file(s) were not imported"));
            }
        }
        ImportOutcome::Imported {
            untagged: self.autotag_off || self.stdout_asis || has(RunLogAction::Asis),
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;
    use std::time::Duration;

    struct Fixture {
        stdout: &'static str,
        stderr: &'static str,
        runlog: &'static str,
        meta: &'static str,
    }

    macro_rules! fixture {
        ($case:literal) => {
            Fixture {
                stdout: include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fixtures/beets/",
                    $case,
                    "/stdout.txt"
                )),
                stderr: include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fixtures/beets/",
                    $case,
                    "/stderr.txt"
                )),
                runlog: include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fixtures/beets/",
                    $case,
                    "/runlog.txt"
                )),
                meta: include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fixtures/beets/",
                    $case,
                    "/meta.json"
                )),
            }
        };
    }

    fn exited(code: i32) -> RunEnd {
        RunEnd::Exited(ExitStatus::from_raw(code << 8))
    }

    fn classify_with(f: &Fixture, autotag_off: bool, stderr_first: bool) -> ImportResult {
        let meta: serde_json::Value = serde_json::from_str(f.meta).unwrap();
        let mode = match meta["mode"].as_str().unwrap() {
            "album" => ImportMode::Album,
            "singleton" => ImportMode::Singleton,
            other => panic!("unknown mode {other}"),
        };
        let submitted = meta["submitted"].as_u64().unwrap() as usize;
        let exit = meta["exit"].as_i64().unwrap() as i32;

        let mut classifier = Classifier::new(mode, submitted, autotag_off);
        let mut streams = [(Stream::Stdout, f.stdout), (Stream::Stderr, f.stderr)];
        if stderr_first {
            streams.reverse();
        }
        for (stream, text) in streams {
            for line in text.lines() {
                classifier.feed(stream, line);
            }
        }
        classifier.finish(
            &exited(exit),
            &RunLimits::default(),
            &parse_run_log(f.runlog),
            "tail".to_string(),
        )
    }

    /// Classifies a fixture, checking that the order between the two streams
    /// does not change the result (pipes do not preserve it).
    fn classify(f: Fixture, autotag_off: bool) -> ImportResult {
        let result = classify_with(&f, autotag_off, false);
        assert_eq!(result, classify_with(&f, autotag_off, true));
        assert_eq!(result.output_tail, "tail");
        result
    }

    fn imported(untagged: bool, warnings: &[&str]) -> ImportOutcome {
        ImportOutcome::Imported {
            untagged,
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
        }
    }

    fn no_match(candidates: Option<u32>) -> ImportOutcome {
        ImportOutcome::Skipped(SkipReason::NoMatch { candidates })
    }

    const ALREADY: ImportOutcome = ImportOutcome::Skipped(SkipReason::AlreadyInLibrary);
    const NOTHING: ImportOutcome = ImportOutcome::Skipped(SkipReason::NothingToImport);

    /// The fixture's fatal line, which the Failed message must repeat.
    fn fatal_line(f: &Fixture) -> &'static str {
        f.stderr
            .lines()
            .find(|l| l.starts_with("error: ") || l.starts_with("configuration error: "))
            .expect("fixture has no fatal line")
    }

    #[test]
    fn strong_matches_import() {
        let album = classify(fixture!("album_strong"), false);
        assert_eq!(album.outcome, imported(false, &[]));
        assert_eq!(
            album.tasks,
            TaskCount {
                imported: 1,
                total: 1
            }
        );
        assert_eq!(album.to_string(), "Imported");

        let single = classify(fixture!("single_strong"), false);
        assert_eq!(single.outcome, imported(false, &[]));
        assert_eq!(
            single.tasks,
            TaskCount {
                imported: 2,
                total: 2
            }
        );
    }

    #[test]
    fn weak_and_empty_lookups_are_no_match_with_the_candidate_count() {
        let f = fixture!("album_weak");
        assert!(f.stderr.lines().any(|l| l == "Evaluating 3 candidates."));
        let album = classify(f, false);
        assert_eq!(album.outcome, no_match(Some(3)));
        assert_eq!(album.to_string(), "No close match (3 candidates)");

        assert_eq!(
            classify(fixture!("single_weak"), false).outcome,
            no_match(Some(3))
        );
        assert_eq!(
            classify(fixture!("album_none"), false).outcome,
            no_match(Some(0))
        );
        assert_eq!(
            classify(fixture!("single_none"), false).outcome,
            no_match(Some(0))
        );
    }

    #[test]
    fn duplicates_that_beets_skips_are_already_in_library() {
        for f in [
            fixture!("dup_ask_album"),
            fixture!("dup_ask_single"),
            fixture!("dup_skip"),
            fixture!("asis_dup"),
        ] {
            let result = classify(f, false);
            assert_eq!(result.outcome, ALREADY);
            assert_eq!(result.outcome.to_string(), "Already in library");
        }
    }

    /// merge folds the library copy into the new task and looks it up again.
    /// In 2.11 that ends in a plain `skip` (after two `duplicate-keep` lines),
    /// not `duplicate-skip`, and nothing is imported.
    #[test]
    fn duplicate_merge_that_ends_in_a_skip_is_not_imported() {
        let f = fixture!("dup_merge");
        let log = parse_run_log(f.runlog);
        assert!(!log.iter().any(|e| e.action == RunLogAction::DuplicateSkip));
        assert_eq!(log.last().unwrap().action, RunLogAction::Skip);

        let result = classify(f, false);
        assert!(!matches!(result.outcome, ImportOutcome::Imported { .. }));
        assert_eq!(result.outcome, no_match(Some(1)));
        assert_eq!(
            result.tasks,
            TaskCount {
                imported: 0,
                total: 1
            }
        );
    }

    #[test]
    fn duplicates_that_beets_replaces_or_keeps_are_imported() {
        assert_eq!(
            classify(fixture!("dup_remove"), false).outcome,
            imported(false, &[])
        );
        assert_eq!(
            classify(fixture!("dup_keep"), false).outcome,
            imported(false, &[])
        );
    }

    #[test]
    fn as_is_imports_are_untagged() {
        let asis = classify(fixture!("asis_weak"), false);
        assert_eq!(asis.outcome, imported(true, &[]));
        assert_eq!(asis.to_string(), "Imported as-is (untagged)");

        // autotag: no leaves no marker in the output, only the probe knows.
        let noauto = classify(fixture!("noautotag"), true);
        assert_eq!(noauto.outcome, imported(true, &[]));
        assert_eq!(noauto.to_string(), "Imported as-is (untagged)");
        assert_eq!(
            classify(fixture!("noautotag"), false).outcome,
            imported(false, &[])
        );
    }

    #[test]
    fn sources_without_audio_are_nothing_to_import() {
        for f in [fixture!("empty"), fixture!("nonmusic")] {
            let result = classify(f, false);
            assert_eq!(result.outcome, NOTHING);
            assert_eq!(result.to_string(), "Nothing left to import");
        }
    }

    #[test]
    fn a_file_beets_drops_silently_is_a_warning() {
        let result = classify(fixture!("corrupt_one"), false);
        assert_eq!(
            result.outcome,
            imported(false, &["1 file(s) were not imported"])
        );
        assert_eq!(result.to_string(), "Imported; 1 file(s) were not imported");
    }

    #[test]
    fn lookup_errors_are_not_reported_as_no_match() {
        let mb = classify(fixture!("mbdown"), false);
        assert!(
            matches!(&mb.outcome, ImportOutcome::LookupFailed { source, .. } if source == "MusicBrainz"),
            "{:?}",
            mb.outcome
        );
        assert!(mb.to_string().starts_with("Lookup failed in MusicBrainz:"));

        assert_eq!(
            classify(fixture!("srcerr"), false).outcome,
            ImportOutcome::LookupFailed {
                source: "FakeSrc".to_string(),
                message: "simulated network failure".to_string(),
            }
        );
    }

    #[test]
    fn fatal_lines_become_the_failed_message() {
        for f in [
            fixture!("missing_path"),
            fixture!("timid"),
            fixture!("missing_config"),
        ] {
            let line = fatal_line(&f);
            let result = classify(f, false);
            assert_eq!(
                result.outcome,
                ImportOutcome::Failed {
                    message: line.to_string()
                }
            );
            assert_eq!(result.to_string(), line);
        }

        let readonly = classify(fixture!("readonly_target"), false);
        assert!(
            matches!(&readonly.outcome, ImportOutcome::Failed { message } if message.starts_with("Error:")),
            "{:?}",
            readonly.outcome
        );
    }

    #[test]
    fn a_crash_reports_its_own_exception_not_the_plugin_load_one() {
        let f = fixture!("pipeline_crash");
        assert!(f.stderr.contains("PluginImportError"));
        assert_eq!(
            classify(f, false).outcome,
            ImportOutcome::Failed {
                message: "TypeError: simulated crash in import stage".to_string()
            }
        );
    }

    #[test]
    fn a_plugin_that_fails_to_load_does_not_fail_a_good_import() {
        assert_eq!(
            classify(fixture!("bad_plugin"), false).outcome,
            imported(false, &[])
        );
    }

    #[test]
    fn several_tasks_report_the_worst_outcome_with_counts() {
        let result = classify(fixture!("multi_task"), false);
        assert_eq!(
            result.tasks,
            TaskCount {
                imported: 1,
                total: 2
            }
        );
        // The two tasks weighed 1 and 3 candidates, so no single count is right.
        assert_eq!(result.outcome, no_match(None));
        assert!(
            result.to_string().ends_with(" (1 of 2 imported)"),
            "{result}"
        );
        assert_eq!(result.to_string(), "No close match (1 of 2 imported)");
    }

    #[test]
    fn idle_and_total_map_to_timed_out() {
        let limits = RunLimits::default();
        let mut c = Classifier::new(ImportMode::Album, 2, false);
        c.feed(Stream::Stderr, "x");
        let idle = c.finish(&RunEnd::Idle, &limits, &[], String::new());
        assert_eq!(
            idle.outcome,
            ImportOutcome::TimedOut {
                limit: TimeoutKind::Idle(Duration::from_secs(180)),
                last_line: Some("x".to_string()),
            }
        );
        assert_eq!(idle.to_string(), "No output from beets for 3 min (last: x)");

        let silent = Classifier::new(ImportMode::Album, 2, false).finish(
            &RunEnd::Idle,
            &limits,
            &[],
            String::new(),
        );
        assert_eq!(silent.to_string(), "No output from beets for 3 min");

        let total = Classifier::new(ImportMode::Album, 2, false).finish(
            &RunEnd::Total,
            &limits,
            &[],
            String::new(),
        );
        assert!(matches!(
            total.outcome,
            ImportOutcome::TimedOut { limit: TimeoutKind::Total(d), .. } if d == Duration::from_secs(1800)
        ));
        assert_eq!(total.to_string(), "Import exceeded 30 min");
    }

    #[test]
    fn spawn_failure_text() {
        let end = RunEnd::SpawnFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ));
        let result = Classifier::new(ImportMode::Album, 1, false).finish(
            &end,
            &RunLimits::default(),
            &[],
            String::new(),
        );
        assert_eq!(
            result.to_string(),
            "Could not start beets: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn unknown_nonzero_exit_without_lines() {
        let result = Classifier::new(ImportMode::Album, 1, false).finish(
            &exited(2),
            &RunLimits::default(),
            &[],
            String::new(),
        );
        assert_eq!(
            result.outcome,
            ImportOutcome::Failed {
                message: "beets exited with code 2".to_string()
            }
        );
    }

    #[test]
    fn a_plugin_load_traceback_alone_is_not_the_failure_message() {
        let mut c = Classifier::new(ImportMode::Album, 1, false);
        for line in fixture!("bad_plugin").stderr.lines() {
            c.feed(Stream::Stderr, line);
        }
        let result = c.finish(&exited(1), &RunLimits::default(), &[], String::new());
        assert_eq!(result.to_string(), "beets exited with code 1");
    }

    #[test]
    fn a_fatal_line_wins_over_a_later_traceback() {
        let mut c = Classifier::new(ImportMode::Album, 1, false);
        for line in [
            "error: no such file or directory: /src/a",
            "Traceback (most recent call last):",
            "  File \"x.py\", line 1, in <module>",
            "ValueError: later",
        ] {
            c.feed(Stream::Stderr, line);
        }
        let result = c.finish(&exited(1), &RunLimits::default(), &[], String::new());
        assert_eq!(
            result.to_string(),
            "error: no such file or directory: /src/a"
        );
    }

    #[test]
    fn markers_are_matched_after_ansi_stripping() {
        let mut c = Classifier::new(ImportMode::Album, 2, false);
        c.feed(
            Stream::Stdout,
            "\x1b[1;34m/src\x1b[39;49;00m \x1b[1;34m(2 items)\x1b[39;49;00m",
        );
        c.feed(Stream::Stdout, "\x1b[33mSkipping.\x1b[0m");
        c.feed(Stream::Stderr, "\x1b[2mEvaluating 4 candidates.\x1b[0m");
        let result = c.finish(&exited(0), &RunLimits::default(), &[], String::new());
        assert_eq!(result.to_string(), "No close match (4 candidates)");
    }

    #[test]
    fn skipping_on_stderr_is_not_a_no_match() {
        // ask + quiet logs "Skipping." on stderr for a duplicate; only the
        // stdout line means a weak match.
        let mut c = Classifier::new(ImportMode::Album, 2, false);
        c.feed(Stream::Stderr, "Sending event: import_task_created");
        c.feed(Stream::Stderr, "Skipping.");
        c.feed(Stream::Stderr, "Sending event: album_imported");
        let result = c.finish(&exited(0), &RunLimits::default(), &[], String::new());
        assert_eq!(result.outcome, imported(false, &[]));
    }

    #[test]
    fn tag_write_errors_are_warnings() {
        let mut c = Classifier::new(ImportMode::Album, 2, false);
        c.feed(Stream::Stdout, "/src (2 items)");
        c.feed(Stream::Stderr, "Sending event: import_task_created");
        c.feed(
            Stream::Stderr,
            "error writing <super: <class 'WriteError'>, <WriteError object>>",
        );
        c.feed(
            Stream::Stderr,
            "error writing <super: <class 'WriteError'>, <WriteError object>>",
        );
        c.feed(Stream::Stderr, "Sending event: album_imported");
        let result = c.finish(&exited(0), &RunLimits::default(), &[], String::new());
        assert_eq!(
            result.to_string(),
            "Imported; beets could not write tags to 2 file(s)"
        );
    }

    #[test]
    fn tasks_that_end_without_an_import_or_a_reason_are_a_failure() {
        let mut c = Classifier::new(ImportMode::Album, 2, false);
        c.feed(Stream::Stderr, "Sending event: import_task_created");
        let result = c.finish(&exited(0), &RunLimits::default(), &[], String::new());
        assert_eq!(
            result.outcome,
            ImportOutcome::Failed {
                message: "beets finished without importing anything".to_string()
            }
        );

        let none = Classifier::new(ImportMode::Album, 0, false).finish(
            &exited(0),
            &RunLimits::default(),
            &[],
            String::new(),
        );
        assert_eq!(none.outcome, NOTHING);
    }

    #[test]
    fn one_candidate_is_singular() {
        assert_eq!(
            no_match(Some(1)).to_string(),
            "No close match (1 candidate)"
        );
        assert_eq!(
            no_match(Some(0)).to_string(),
            "No close match (0 candidates)"
        );
    }

    #[test]
    fn a_long_last_line_is_clipped_in_the_timeout_text() {
        let line = "é".repeat(250);
        let outcome = ImportOutcome::TimedOut {
            limit: TimeoutKind::Idle(Duration::from_secs(180)),
            last_line: Some(line),
        };
        assert_eq!(
            outcome.to_string(),
            format!("No output from beets for 3 min (last: {})", "é".repeat(200))
        );
    }

    #[test]
    fn a_signal_is_named_in_the_failure() {
        let result = Classifier::new(ImportMode::Album, 1, false).finish(
            &RunEnd::Exited(ExitStatus::from_raw(9)),
            &RunLimits::default(),
            &[],
            String::new(),
        );
        assert_eq!(result.to_string(), "beets was killed by signal 9");
    }

    #[test]
    fn run_log_parses_paths() {
        let log = parse_run_log("import started Tue Oct 20 02:30:29 2026\nskip /src/a; /src/b\n");
        assert_eq!(
            log,
            vec![RunLogEntry {
                action: RunLogAction::Skip,
                paths: vec!["/src/a".to_string(), "/src/b".to_string()],
            }]
        );
    }

    #[test]
    fn result_constructors() {
        assert_eq!(
            ImportResult::refused("beets cannot start".into()).to_string(),
            "beets cannot start"
        );
        assert_eq!(ImportResult::failed("boom".into()).to_string(), "boom");
        assert_eq!(ImportResult::nothing_to_import().outcome, NOTHING);
    }
}
