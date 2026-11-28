//! What a job's rows show for each state it goes through.

use super::retry::keeps_retry;
use super::{ImportJob, JobOutcome};
use shared::download::{DownloadProgress, DownloadState, SkipKind};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use soulbeet::{FailKind, ImportMode, ImportOutcome, ImportResult, SkipReason, TimeoutKind};
use std::time::Duration;

pub fn row_state(outcome: &ImportOutcome) -> DownloadState {
    match outcome {
        ImportOutcome::Imported { .. } => DownloadState::Imported,
        ImportOutcome::Skipped(_) => DownloadState::ImportSkipped,
        ImportOutcome::LookupFailed { .. }
        | ImportOutcome::Failed { .. }
        | ImportOutcome::TimedOut { .. }
        | ImportOutcome::Refused { .. } => DownloadState::Failed(outcome.to_string()),
    }
}

/// The typed failure for an import outcome that is not a success. Its message
/// is the result's text, so a row shows what it always showed; its user
/// action is the importer's.
pub(crate) fn import_failure(result: &ImportResult) -> Option<Failure> {
    let (code, retry) = match &result.outcome {
        ImportOutcome::Imported { .. } => return None,
        ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { .. }) => (
            FailureCode::ImportAlreadyInLibrary,
            RetryAdvice::NotRetryable,
        ),
        ImportOutcome::Skipped(SkipReason::NoMatch { .. }) => {
            (FailureCode::ImportNoMatch, RetryAdvice::NeedsDecision)
        }
        ImportOutcome::Skipped(SkipReason::NothingToImport) => (
            FailureCode::ImportNothingToImport,
            RetryAdvice::NotRetryable,
        ),
        ImportOutcome::LookupFailed { .. } => (
            FailureCode::ImportLookupFailed,
            RetryAdvice::SameSourceLater,
        ),
        ImportOutcome::TimedOut {
            limit: TimeoutKind::Idle(_),
            ..
        } => (FailureCode::ImportIdleTimeout, RetryAdvice::SameSourceLater),
        ImportOutcome::TimedOut {
            limit: TimeoutKind::Total(_),
            ..
        } => (
            FailureCode::ImportTotalTimeout,
            RetryAdvice::SameSourceLater,
        ),
        ImportOutcome::Refused { .. } => (FailureCode::ImporterBroken, RetryAdvice::NeedsUser),
        ImportOutcome::Failed { kind, .. } => match kind {
            FailKind::Beets => (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater),
            FailKind::Setup => (FailureCode::ImporterBroken, RetryAdvice::NeedsUser),
            FailKind::TargetUnwritable => (
                FailureCode::ImportTargetUnwritable,
                RetryAdvice::NotRetryable,
            ),
            FailKind::DiskFull => (FailureCode::ImportDiskFull, RetryAdvice::NeedsUser),
            // A lock is let go: the queue runs the import again by itself.
            FailKind::LibraryLocked => (
                FailureCode::ImportLibraryLocked,
                RetryAdvice::SameSourceLater,
            ),
            FailKind::AsIsMissingTags => (
                FailureCode::ImportAsIsMissingTags,
                RetryAdvice::NeedsDecision,
            ),
            FailKind::ChoiceInvalid => {
                (FailureCode::ImportChoiceInvalid, RetryAdvice::NeedsDecision)
            }
        },
    };
    let mut failure = Failure::new(code, Stage::Import, retry, result.to_string());
    if let Some(action) = &result.user_action {
        failure = failure.with_user_action(action.clone());
    }
    if !result.output_tail.is_empty() {
        failure = failure.with_cause(Service::Beets, None, result.output_tail.clone());
    }
    Some(failure)
}

/// `row` in a state on the way to an outcome, with no earlier failure, skip
/// or failure note left on it.
pub fn row_with_state(row: DownloadProgress, state: DownloadState) -> DownloadProgress {
    row.without_failure().with_state(state)
}

/// `rows`, each as `row_with_state` makes it.
pub fn rows_with_state(rows: &[DownloadProgress], state: DownloadState) -> Vec<DownloadProgress> {
    rows.iter()
        .map(|row| row_with_state(row.clone(), state.clone()))
        .collect()
}

/// The rows of a finished job: the outcome's failure and text (an imported
/// row's note, and the failure of a step after the import that it reports,
/// such as a new copy not moved into place), the folder the files were kept in
/// when a retry is kept for them, whether that retry can pick a match, and the
/// output tail on the first row only.
pub fn rows_for_done(
    rows: &[DownloadProgress],
    job: &ImportJob,
    done: &JobOutcome,
) -> Vec<DownloadProgress> {
    let result = &done.result;
    // The result's text, unlike the outcome's, carries the task count.
    let text = result.to_string();
    let state = row_state(&result.outcome);
    // The note on an imported row is not the failure it may carry; with_failure
    // writes the rest.
    let (error, after) = match &result.outcome {
        ImportOutcome::Imported {
            untagged,
            warnings,
            failure,
            ..
        } => (
            (*untagged || !warnings.is_empty()).then_some(text),
            failure.clone(),
        ),
        _ => (None, None),
    };
    // Same rule as the retry store, so a row offers Retry only when one is kept.
    let retryable = keeps_retry(&result.outcome, &job.duplicates);
    let local_path = retryable.then(|| kept_in(job)).flatten();
    let output = (!result.output_tail.is_empty()).then(|| result.output_tail.clone());
    let skip = match &result.outcome {
        ImportOutcome::Skipped(reason) => Some(SkipKind::from(reason)),
        _ => None,
    };
    let failure = import_failure(result).map(|failure| match done.retry_in {
        Some(wait) => retried_in(failure, wait),
        None => failure,
    });
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let built = DownloadProgress {
                state: state.clone(),
                error: error.clone(),
                failure: after.clone(),
                skip,
                retryable,
                ..row.clone()
            }
            .with_import_details(local_path.clone(), output.clone().filter(|_| i == 0));
            match &failure {
                Some(failure) => built.with_failure(failure.clone()),
                None => built,
            }
        })
        .collect()
}

/// `failure`, of a run the queue makes again by itself after `wait`, saying so.
fn retried_in(failure: Failure, wait: Duration) -> Failure {
    let secs = wait.as_secs();
    let when = match secs >= 60 && secs.is_multiple_of(60) {
        true => format!("{} min", secs / 60),
        false => format!("{secs} s"),
    };
    let message = format!("{}; Soulbeet tries again in {when}", failure.message);
    failure
        .with_message(message)
        .with_retry_after(u32::try_from(secs).unwrap_or(u32::MAX))
}

/// The folder holding the files a job did not import.
fn kept_in(job: &ImportJob) -> Option<String> {
    let source = job.sources.first()?;
    let dir = match job.mode {
        ImportMode::Album => source.as_path(),
        ImportMode::Singleton => source.parent()?,
    };
    Some(dir.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::{imported, row};
    use crate::import_queue::{ImportJob, JobOutcome};
    use shared::failure::{UserAction, UserActionCode};
    use soulbeet::{DriverChoice, TaskCount};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn row_state_maps_every_outcome() {
        let failed = |o: ImportOutcome| DownloadState::Failed(o.to_string());
        let cases = [
            (
                ImportOutcome::Imported {
                    untagged: true,
                    warnings: vec![],
                    items: vec![],
                    failure: None,
                },
                DownloadState::Imported,
            ),
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
                DownloadState::ImportSkipped,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NoMatch { candidates: None }),
                DownloadState::ImportSkipped,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NothingToImport),
                DownloadState::ImportSkipped,
            ),
        ];
        for (outcome, state) in cases {
            assert_eq!(row_state(&outcome), state, "{outcome:?}");
        }
        for outcome in [
            ImportOutcome::LookupFailed {
                source: "MusicBrainz".into(),
                message: "timed out".into(),
            },
            ImportOutcome::Failed {
                kind: FailKind::Beets,
                message: "beets exited with code 1".into(),
            },
            ImportOutcome::TimedOut {
                limit: TimeoutKind::Total(Duration::from_secs(1800)),
                last_line: None,
            },
            ImportOutcome::Refused {
                issue: "No metadata source plugin loaded".into(),
            },
        ] {
            assert_eq!(row_state(&outcome), failed(outcome.clone()));
        }
    }

    fn job(source: &str, mode: ImportMode) -> ImportJob {
        ImportJob {
            sources: vec![PathBuf::from(source)],
            files: vec![PathBuf::from(source)],
            target: PathBuf::from("/music"),
            mode,
            owner: None,
            snapshot_target: false,
            cleanup_root: None,
            choice: DriverChoice::Auto,
            choices: None,
            duplicates: crate::import_queue::Duplicates::AsConfigured,
            last_run: None,
            lock_retries: 0,
            placement: soulbeet::ImportPlacement::Default,
        }
    }

    fn done(result: ImportResult) -> JobOutcome {
        JobOutcome {
            result,
            new_files: vec![],
            ran: true,
            replaced: vec![],
            not_replaced: vec![],
            retry_in: None,
        }
    }

    #[test]
    fn done_rows_carry_reason_folder_and_tail_on_the_first_row_only() {
        let rows = [row("a"), row("b")];
        let skipped = ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::NoMatch {
                candidates: Some(3),
            }),
            output_tail: "tail".into(),
            tasks: TaskCount {
                imported: 0,
                total: 2,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        };

        let album = rows_for_done(
            &rows,
            &job("/dl/X [12345678]", ImportMode::Album),
            &done(skipped.clone()),
        );
        for r in &album {
            assert_eq!(r.state, DownloadState::ImportSkipped);
            let text = "No close match (3 candidates) (0 of 2 imported)";
            assert_eq!(r.error.as_deref(), Some(text));
            assert_eq!(r.failure.as_ref().unwrap().message, text);
            assert_eq!(
                r.skip,
                Some(SkipKind::NoMatch {
                    candidates: Some(3)
                })
            );
            assert_eq!(r.local_path.as_deref(), Some("/dl/X [12345678]"));
            assert!(r.retryable);
        }
        assert_eq!(album[0].import_output.as_deref(), Some("tail"));
        assert_eq!(album[1].import_output, None);

        let failed = ImportResult {
            outcome: ImportOutcome::Failed {
                kind: FailKind::Beets,
                message: "beets exited with code 1".into(),
            },
            ..skipped
        };
        let single = rows_for_done(
            &rows[..1],
            &job("/dl/X [12345678]/a.flac", ImportMode::Singleton),
            &done(failed),
        );
        assert_eq!(
            single[0].state,
            DownloadState::Failed("beets exited with code 1 (0 of 2 imported)".into())
        );
        assert_eq!(single[0].local_path.as_deref(), Some("/dl/X [12345678]"));
        assert!(single[0].retryable);
        assert_eq!(
            single[0].failure.as_ref().unwrap().code,
            FailureCode::ImportBeetsFailed
        );
        assert_eq!(single[0].skip, None);

        let ok = rows_for_done(
            &rows,
            &job("/dl/X [12345678]", ImportMode::Album),
            &done(imported()),
        );
        for r in &ok {
            assert_eq!(r.state, DownloadState::Imported);
            assert_eq!(r.error, None);
            assert_eq!((&r.failure, r.skip), (&None, None));
            assert_eq!(r.local_path, None);
            assert!(!r.retryable);
        }
        assert_eq!(ok[0].import_output.as_deref(), Some("imported tail"));
        assert_eq!(ok[1].import_output, None);

        let untagged = ImportResult {
            outcome: ImportOutcome::Imported {
                untagged: true,
                warnings: vec!["1 file(s) were not imported".into()],
                items: vec![],
                failure: None,
            },
            ..imported()
        };
        let ok = rows_for_done(
            &rows[..1],
            &job("/dl/a.flac", ImportMode::Singleton),
            &done(untagged),
        );
        assert_eq!(
            ok[0].error.as_deref(),
            Some("Imported as-is (untagged); 1 file(s) were not imported")
        );
        assert_eq!(ok[0].failure, None);
    }

    #[test]
    fn import_failure_codes_every_outcome() {
        let run = |outcome| ImportResult {
            outcome,
            output_tail: "tail".into(),
            tasks: TaskCount {
                imported: 0,
                total: 1,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        };
        let cases = [
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
                FailureCode::ImportAlreadyInLibrary,
                RetryAdvice::NotRetryable,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NoMatch {
                    candidates: Some(2),
                }),
                FailureCode::ImportNoMatch,
                RetryAdvice::NeedsDecision,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NothingToImport),
                FailureCode::ImportNothingToImport,
                RetryAdvice::NotRetryable,
            ),
            (
                ImportOutcome::LookupFailed {
                    source: "MusicBrainz".into(),
                    message: "timed out".into(),
                },
                FailureCode::ImportLookupFailed,
                RetryAdvice::SameSourceLater,
            ),
            (
                ImportOutcome::TimedOut {
                    limit: TimeoutKind::Idle(Duration::from_secs(180)),
                    last_line: None,
                },
                FailureCode::ImportIdleTimeout,
                RetryAdvice::SameSourceLater,
            ),
            (
                ImportOutcome::TimedOut {
                    limit: TimeoutKind::Total(Duration::from_secs(1800)),
                    last_line: None,
                },
                FailureCode::ImportTotalTimeout,
                RetryAdvice::SameSourceLater,
            ),
            (
                ImportOutcome::Refused {
                    issue: "No metadata source plugin loaded".into(),
                },
                FailureCode::ImporterBroken,
                RetryAdvice::NeedsUser,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::Beets,
                    message: "beets exited with code 1".into(),
                },
                FailureCode::ImportBeetsFailed,
                RetryAdvice::SameSourceLater,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::Setup,
                    message: "Could not start beets: x".into(),
                },
                FailureCode::ImporterBroken,
                RetryAdvice::NeedsUser,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::TargetUnwritable,
                    message: "Could not create /m: x".into(),
                },
                FailureCode::ImportTargetUnwritable,
                RetryAdvice::NotRetryable,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::DiskFull,
                    message:
                        "Error: No space left on device while copying /dl/a.flac to /music/A/a.flac"
                            .into(),
                },
                FailureCode::ImportDiskFull,
                RetryAdvice::NeedsUser,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::LibraryLocked,
                    message: "The beets library database is locked: sqlite3.OperationalError: \
                              database is locked"
                        .into(),
                },
                FailureCode::ImportLibraryLocked,
                RetryAdvice::SameSourceLater,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::AsIsMissingTags,
                    message: "Import as-is needs tags".into(),
                },
                FailureCode::ImportAsIsMissingTags,
                RetryAdvice::NeedsDecision,
            ),
            (
                ImportOutcome::Failed {
                    kind: FailKind::ChoiceInvalid,
                    message: "FakeSrc has no recording w1".into(),
                },
                FailureCode::ImportChoiceInvalid,
                RetryAdvice::NeedsDecision,
            ),
        ];
        for (outcome, code, retry) in cases {
            let result = run(outcome);
            let f = import_failure(&result).expect("a failure");
            assert_eq!(
                (f.code, f.retry, f.stage),
                (code, retry, Stage::Import),
                "{:?}",
                result.outcome
            );
            assert_eq!(f.message, result.to_string());
            assert_eq!(f.cause.unwrap().raw, "tail");
        }
        assert_eq!(import_failure(&imported()), None);
    }

    /// The importer words what a person does; the row mapping adds none of
    /// its own.
    #[test]
    fn an_import_failure_carries_the_importers_user_action() {
        let refused = ImportResult::refused("No metadata source plugin loaded".into());
        assert_eq!(import_failure(&refused).unwrap().user_action, None);

        let action = UserAction::new(
            UserActionCode::FixImporter,
            "Fix the beets setup: No metadata source plugin loaded",
            "Settings > Config",
        );
        let f = import_failure(&refused.with_user_action(action.clone())).unwrap();
        assert_eq!(f.code, FailureCode::ImporterBroken);
        assert_eq!(f.user_action, Some(action));
    }

    #[test]
    fn an_imported_row_carries_the_failure_of_a_copy_not_moved_into_place() {
        // As move_items words a move beets made out of the library.
        let unmoved = Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::NotRetryable,
            "beets moved item 7 to /elsewhere/03.flac, which is not inside /music/alice",
        );
        let result = ImportResult {
            outcome: ImportOutcome::Imported {
                untagged: false,
                warnings: vec![
                    "replaced the library's earlier copy A/x.flac".into(),
                    unmoved.message.clone(),
                ],
                items: vec![],
                failure: Some(unmoved.clone()),
            },
            ..imported()
        };
        let rows = rows_for_done(
            &[row("a")],
            &job("/dl/a.flac", ImportMode::Singleton),
            &done(result),
        );
        assert_eq!(rows[0].state, DownloadState::Imported);
        assert_eq!(
            rows[0].error.as_deref(),
            Some("Imported; replaced the library's earlier copy A/x.flac; beets moved item 7 to /elsewhere/03.flac, which is not inside /music/alice")
        );
        assert_eq!(rows[0].failure, Some(unmoved));
    }

    #[test]
    fn an_imported_result_clears_an_earlier_failure_and_skip() {
        let earlier = import_failure(&ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ))
        .unwrap();
        let stale = DownloadProgress {
            skip: Some(SkipKind::AlreadyInLibrary),
            ..row("a").with_failure(earlier)
        };
        let rows = rows_for_done(
            &[stale],
            &job("/dl/a.flac", ImportMode::Singleton),
            &done(imported()),
        );
        assert_eq!(rows[0].state, DownloadState::Imported);
        assert_eq!(
            (&rows[0].failure, rows[0].skip, &rows[0].error),
            (&None, None, &None)
        );
    }

    /// The row of a run the queue makes again by itself says when, and the
    /// failure carries the wait for an app.
    #[test]
    fn a_locked_library_row_says_when_the_queue_tries_again() {
        let message = "The beets library database is locked: sqlite3.OperationalError: database \
                       is locked";
        let locked = ImportResult::failed(FailKind::LibraryLocked, message.into());
        let rows = rows_for_done(
            &[row("a")],
            &job("/dl/a.flac", ImportMode::Singleton),
            &JobOutcome {
                retry_in: Some(Duration::from_secs(300)),
                ..done(locked.clone())
            },
        );
        let text = format!("{message}; Soulbeet tries again in 5 min");
        assert_eq!(rows[0].state, DownloadState::Failed(text.clone()));
        let failure = rows[0].failure.as_ref().unwrap();
        assert_eq!(
            (failure.code, failure.retry, failure.retry_after_seconds),
            (
                FailureCode::ImportLibraryLocked,
                RetryAdvice::SameSourceLater,
                Some(300)
            )
        );
        assert_eq!(failure.message, text);
        assert!(rows[0].retryable);

        // The last run says only what failed.
        let last = rows_for_done(
            &[row("a")],
            &job("/dl/a.flac", ImportMode::Singleton),
            &done(locked),
        );
        assert_eq!(last[0].state, DownloadState::Failed(message.into()));
        assert_eq!(last[0].failure.as_ref().unwrap().retry_after_seconds, None);
    }

    #[test]
    fn nothing_to_import_rows_offer_no_retry_and_no_folder() {
        let rows = rows_for_done(
            &[row("a")],
            &job("/dl/X [12345678]", ImportMode::Album),
            &done(ImportResult::nothing_to_import()),
        );
        assert_eq!(rows[0].state, DownloadState::ImportSkipped);
        assert_eq!(rows[0].error.as_deref(), Some("Nothing left to import"));
        assert_eq!(rows[0].local_path, None);
        assert!(!rows[0].retryable);
    }

    #[test]
    fn rows_with_state_only_changes_the_state() {
        let mut waiting = row("a");
        waiting.batch_id = Some("b1".into());
        let rows = rows_with_state(&[waiting.clone()], DownloadState::ImportQueued(2));
        assert_eq!(
            rows,
            vec![DownloadProgress {
                state: DownloadState::ImportQueued(2),
                ..waiting
            }]
        );

        let earlier = import_failure(&ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ))
        .unwrap();
        let stale = DownloadProgress {
            skip: Some(SkipKind::AlreadyInLibrary),
            ..row("a").with_failure(earlier)
        };
        let rows = rows_with_state(&[stale], DownloadState::Importing);
        assert_eq!(
            (&rows[0].failure, rows[0].skip, &rows[0].error),
            (&None, None, &None)
        );
    }
}
