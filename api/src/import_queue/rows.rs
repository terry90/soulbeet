//! What a job's rows show for each state it goes through.

use super::retry::keeps_retry;
use super::{ImportJob, JobOutcome};
use shared::download::{DownloadProgress, DownloadState, SkipKind};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage, UserAction};
use soulbeet::{FailKind, ImportMode, ImportOutcome, ImportResult, SkipReason, TimeoutKind};

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
/// is the result's text, so a row shows what it always showed.
fn import_failure(result: &ImportResult) -> Option<Failure> {
    let (code, retry) = match &result.outcome {
        ImportOutcome::Imported { .. } => return None,
        ImportOutcome::Skipped(SkipReason::AlreadyInLibrary) => (
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
            FailKind::AsIsMissingTags => (
                FailureCode::ImportAsIsMissingTags,
                RetryAdvice::NeedsDecision,
            ),
            FailKind::ChoiceInvalid => {
                (FailureCode::ImportChoiceInvalid, RetryAdvice::NeedsDecision)
            }
        },
    };
    let message = result.to_string();
    let mut failure = Failure::new(code, Stage::Import, retry, message.clone());
    if code == FailureCode::ImporterBroken {
        failure = failure.with_user_action(UserAction::fix_importer(&message));
    }
    if !result.output_tail.is_empty() {
        failure = failure.with_cause(Service::Beets, None, result.output_tail.clone());
    }
    Some(failure)
}

pub fn rows_with_state(rows: &[DownloadProgress], state: DownloadState) -> Vec<DownloadProgress> {
    rows.iter()
        .map(|row| DownloadProgress {
            state: state.clone(),
            failure: None,
            skip: None,
            ..row.clone()
        })
        .collect()
}

/// The rows of a finished job: the outcome's failure and text, the folder the
/// files were kept in when a retry is kept for them, whether that retry can
/// pick a match, and the output tail on the first row only.
pub fn rows_for_done(
    rows: &[DownloadProgress],
    job: &ImportJob,
    done: &JobOutcome,
) -> Vec<DownloadProgress> {
    let result = &done.result;
    // The result's text, unlike the outcome's, carries the task count.
    let text = result.to_string();
    let (state, error) = match row_state(&result.outcome) {
        DownloadState::Imported => {
            let notable = matches!(
                &result.outcome,
                ImportOutcome::Imported { untagged, warnings } if *untagged || !warnings.is_empty()
            );
            (DownloadState::Imported, notable.then_some(text))
        }
        DownloadState::Failed(_) => (DownloadState::Failed(text.clone()), Some(text)),
        state => (state, Some(text)),
    };
    // Same rule as the retry store, so a row offers Retry only when one is kept.
    let retryable = keeps_retry(&result.outcome);
    let can_choose_import = retryable && job.choices.is_some();
    let local_path = retryable.then(|| kept_in(job)).flatten();
    let output = (!result.output_tail.is_empty()).then(|| result.output_tail.clone());
    let skip = match &result.outcome {
        ImportOutcome::Skipped(reason) => Some(SkipKind::from(reason)),
        _ => None,
    };
    let failure = import_failure(result);
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let built = DownloadProgress {
                state: state.clone(),
                error: error.clone(),
                failure: None,
                skip,
                retryable,
                can_choose_import,
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
    use crate::import_queue::test_support::{candidate, imported, no_match, row, track_choices};
    use crate::import_queue::{ImportJob, JobOutcome};
    use shared::failure::UserActionCode;
    use soulbeet::{DriverChoice, TaskCount};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn row_state_maps_every_outcome() {
        let failed = |o: ImportOutcome| DownloadState::Failed(o.to_string());
        let cases = [
            (
                ImportOutcome::Imported {
                    untagged: true,
                    warnings: vec![],
                },
                DownloadState::Imported,
            ),
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary),
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
            target: PathBuf::from("/music"),
            mode,
            owner: None,
            snapshot_target: false,
            cleanup_root: None,
            choice: DriverChoice::Auto,
            choices: None,
        }
    }

    fn done(result: ImportResult) -> JobOutcome {
        JobOutcome {
            result,
            new_files: vec![],
            ran: true,
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
        };
        let cases = [
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary),
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
        let refused = import_failure(&run(ImportOutcome::Refused {
            issue: "No metadata source plugin loaded".into(),
        }))
        .unwrap();
        assert_eq!(
            refused.user_action.unwrap().code,
            UserActionCode::FixImporter
        );
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

    #[test]
    fn rows_offer_choices_only_while_a_kept_job_has_them() {
        let with_choices = ImportJob {
            choices: Some(Arc::new(track_choices(vec![candidate("FakeSrc", "w1")]))),
            ..job("/dl/a.flac", ImportMode::Singleton)
        };
        let choosable = |job: &ImportJob, result: ImportResult| {
            rows_for_done(&[row("a")], job, &done(result))[0].can_choose_import
        };
        assert!(choosable(&with_choices, no_match(1)));
        assert!(choosable(
            &with_choices,
            ImportResult::failed(
                FailKind::ChoiceInvalid,
                "FakeSrc has no recording w1".into()
            )
        ));
        assert!(!choosable(&with_choices, imported()));
        assert!(!choosable(
            &job("/dl/a.flac", ImportMode::Singleton),
            no_match(1)
        ));
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
        assert_eq!((&rows[0].failure, rows[0].skip), (&None, None));
    }
}
