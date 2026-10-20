//! What a job's rows show for each state it goes through.

use super::retry::keeps_retry;
use super::{ImportJob, JobOutcome};
use shared::download::{DownloadProgress, DownloadState};
use soulbeet::{ImportMode, ImportOutcome};

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

pub fn rows_with_state(rows: &[DownloadProgress], state: DownloadState) -> Vec<DownloadProgress> {
    rows.iter()
        .map(|row| DownloadProgress {
            state: state.clone(),
            ..row.clone()
        })
        .collect()
}

/// The rows of a finished job: the outcome text as the error, the folder the
/// files were kept in when a retry is kept for them, and the output tail on
/// the first row only.
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
    let local_path = retryable.then(|| kept_in(job)).flatten();
    let output = (!result.output_tail.is_empty()).then(|| result.output_tail.clone());
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            DownloadProgress {
                state: state.clone(),
                error: error.clone(),
                retryable,
                ..row.clone()
            }
            .with_import_details(local_path.clone(), output.clone().filter(|_| i == 0))
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
    use crate::import_queue::test_support::{imported, row};
    use crate::import_queue::{ImportJob, JobOutcome};
    use soulbeet::{ImportMode, ImportResult, SkipReason, TaskCount, TimeoutKind};
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
        }
    }

    fn done(result: ImportResult) -> JobOutcome {
        JobOutcome {
            result,
            new_files: vec![],
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
        };

        let album = rows_for_done(
            &rows,
            &job("/dl/X [12345678]", ImportMode::Album),
            &done(skipped.clone()),
        );
        for r in &album {
            assert_eq!(r.state, DownloadState::ImportSkipped);
            assert_eq!(
                r.error.as_deref(),
                Some("No close match (3 candidates) (0 of 2 imported)")
            );
            assert_eq!(r.local_path.as_deref(), Some("/dl/X [12345678]"));
            assert!(r.retryable);
        }
        assert_eq!(album[0].import_output.as_deref(), Some("tail"));
        assert_eq!(album[1].import_output, None);

        let failed = ImportResult {
            outcome: ImportOutcome::Failed {
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

        let ok = rows_for_done(
            &rows,
            &job("/dl/X [12345678]", ImportMode::Album),
            &done(imported()),
        );
        for r in &ok {
            assert_eq!(r.state, DownloadState::Imported);
            assert_eq!(r.error, None);
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
    }
}
