//! The last job of each row that was not imported, so the row can be
//! retried without the client naming any path. Kept in memory per user and
//! lost on restart.

use super::choice::{resolve, Refusal};
use super::{ImportJob, ImportQueue, IMPORT_QUEUE};
use shared::download::{ImportChoice, MatchChoices};
use soulbeet::audio::count_audio_files;
use soulbeet::{ImportOutcome, ImportResult, SkipReason};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Rows a client loses on reload are never forgotten by it, so each user
/// keeps at most this many, the oldest dropped first.
const MAX_PER_USER: usize = 200;

/// Whether a job that ended with `outcome` is kept for a retry. Not when it
/// imported, or when nothing was left to import, which no retry can fix.
pub(super) fn keeps_retry(outcome: &ImportOutcome) -> bool {
    !matches!(
        outcome,
        ImportOutcome::Imported { .. } | ImportOutcome::Skipped(SkipReason::NothingToImport)
    )
}

struct Kept {
    /// Order of recording, for dropping the oldest.
    seq: u64,
    /// Shared by all rows of the job.
    job: Arc<ImportJob>,
}

#[derive(Default)]
struct Store {
    next_seq: u64,
    /// username -> row item -> job
    users: HashMap<String, HashMap<String, Kept>>,
}

#[derive(Default)]
pub(super) struct RetryStore {
    store: Mutex<Store>,
}

impl RetryStore {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps the job for each of its rows when `keeps_retry` says so, and
    /// drops what its rows kept before otherwise.
    pub(super) fn record(&self, job: &Arc<ImportJob>, result: &ImportResult) {
        let Some(owner) = &job.owner else {
            return;
        };
        let keep = keeps_retry(&result.outcome);
        let mut store = self.store();
        let seq = store.next_seq;
        store.next_seq += 1;
        let user = store.users.entry(owner.username.clone()).or_default();
        for row in &owner.rows {
            if keep {
                let kept = Kept {
                    seq,
                    job: job.clone(),
                };
                user.insert(row.item.clone(), kept);
            } else {
                user.remove(&row.item);
            }
        }
        while user.len() > MAX_PER_USER {
            let oldest = user
                .iter()
                .min_by_key(|(_, kept)| kept.seq)
                .map(|(item, _)| item.clone());
            match oldest {
                Some(item) => user.remove(&item),
                None => break,
            };
        }
        if user.is_empty() {
            store.users.remove(&owner.username);
        }
    }

    /// Removes and returns the job of `item` once `accept` takes it, along
    /// with its other rows' entries, so no row of it can resubmit it a second
    /// time. A job `accept` refuses stays kept.
    fn take<T>(
        &self,
        username: &str,
        item: &str,
        accept: impl FnOnce(&ImportJob) -> Result<T, Refusal>,
    ) -> Result<(Arc<ImportJob>, T), Refusal> {
        let mut store = self.store();
        let user = store
            .users
            .get_mut(username)
            .ok_or_else(Refusal::nothing_to_retry)?;
        let kept = user.get(item).ok_or_else(Refusal::nothing_to_retry)?;
        let accepted = accept(&kept.job)?;
        let job = kept.job.clone();
        user.retain(|_, kept| kept.job.sources != job.sources);
        if user.is_empty() {
            store.users.remove(username);
        }
        Ok((job, accepted))
    }

    fn peek(&self, username: &str, item: &str) -> Option<Arc<ImportJob>> {
        let store = self.store();
        Some(store.users.get(username)?.get(item)?.job.clone())
    }

    fn forget(&self, username: &str, items: &[String]) {
        let mut store = self.store();
        let Some(user) = store.users.get_mut(username) else {
            return;
        };
        for item in items {
            user.remove(item);
        }
        if user.is_empty() {
            store.users.remove(username);
        }
    }
}

impl ImportQueue {
    pub async fn retry(
        &self,
        username: &str,
        item: &str,
        choice: ImportChoice,
    ) -> Result<(), Refusal> {
        let (job, choice) = self
            .shared
            .retries
            .take(username, item, |job| resolve(&choice, job))?;
        let sources: Vec<&Path> = job.sources.iter().map(PathBuf::as_path).collect();
        if count_audio_files(&sources).await == 0 {
            return Err(Refusal::files_gone());
        }
        self.submit(ImportJob {
            choice,
            ..Arc::unwrap_or_clone(job)
        })
        .await;
        Ok(())
    }

    /// What the kept job of `item` can pick from, leaving it kept.
    pub fn import_choices(&self, username: &str, item: &str) -> Result<MatchChoices, Refusal> {
        let job = self
            .shared
            .retries
            .peek(username, item)
            .ok_or_else(Refusal::nothing_to_retry)?;
        job.choices
            .as_deref()
            .cloned()
            .ok_or_else(Refusal::no_choices)
    }

    pub fn forget_retries(&self, username: &str, items: &[String]) {
        self.shared.retries.forget(username, items);
    }
}

pub async fn retry_import_for(
    username: &str,
    item: &str,
    choice: ImportChoice,
) -> Result<(), Refusal> {
    IMPORT_QUEUE.retry(username, item, choice).await
}

pub fn import_choices_for(username: &str, item: &str) -> Result<MatchChoices, Refusal> {
    IMPORT_QUEUE.import_choices(username, item)
}

pub fn forget_retries(username: &str, items: &[String]) {
    IMPORT_QUEUE.forget_retries(username, items);
}

#[cfg(test)]
mod tests {
    use crate::import_queue::test_support::{
        broken, candidate, no_match, ready, track_choices, RecordingDeps, StubImporter,
    };
    use crate::import_queue::tests::{file, finish, job, owned, queue};
    use crate::import_queue::{ImportJob, ImportMode, ImportQueue, Refusal};
    use shared::download::{
        DownloadProgress, DownloadState, ImportChoice, KeptImport, MatchChoices,
    };
    use soulbeet::{DriverChoice, ImportResult};
    use std::sync::Arc;
    use std::time::Duration;

    /// A refusal of the choice sent, which keeps the job and its matches.
    fn bad_choice(message: &str) -> Refusal {
        Refusal {
            message: message.into(),
            kept: KeptImport::WithMatches,
        }
    }

    async fn until_imported(deps: &RecordingDeps, item: &str) {
        let done = || {
            deps.progress_for("u")
                .iter()
                .flatten()
                .any(|(i, s)| i == item && *s == DownloadState::Imported)
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retried job never imported");
    }

    /// Waits for the `runs`th finished row of `item` and returns it.
    async fn done_row(deps: &RecordingDeps, item: &str, runs: usize) -> DownloadProgress {
        let done = || {
            deps.rows_for("u")
                .into_iter()
                .flatten()
                .filter(|r| {
                    r.item == item
                        && !matches!(
                            r.state,
                            DownloadState::ImportQueued(_) | DownloadState::Importing
                        )
                })
                .nth(runs - 1)
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(row) = done() {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("job never finished")
    }

    fn found() -> MatchChoices {
        track_choices(vec![candidate("FakeSrc", "w1"), candidate("FakeSrc", "w2")])
    }

    /// Submits `a` as a singleton job that skips with `found()`, and waits
    /// for its row.
    async fn skipped(
        tmp: &std::path::Path,
    ) -> (
        ImportQueue,
        Arc<StubImporter>,
        Arc<RecordingDeps>,
        ImportJob,
    ) {
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult {
            choices: Some(found()),
            ..no_match(2)
        });
        let skipping = owned(job(file(tmp, "dl/a.flac"), &tmp.join("lib")), "u", &["a"]);
        finish(queue.submit(skipping.clone()).await).await;
        (queue, stub, deps, skipping)
    }

    #[tokio::test]
    async fn a_skipped_automatic_import_offers_its_matches_until_one_is_picked() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        assert!(done_row(&deps, "a", 1).await.can_choose_import);
        // Looking does not use up the row.
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));

        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w2".into(),
        };
        assert_eq!(queue.retry("u", "a", pick).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(
            stub.choices(),
            vec![
                DriverChoice::Auto,
                DriverChoice::Pick {
                    data_source: "FakeSrc".into(),
                    id: "w2".into()
                }
            ]
        );
        assert_eq!(
            queue.import_choices("u", "a"),
            Err(Refusal::nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn a_refused_choice_leaves_the_row_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        let unknown = ImportChoice::Candidate {
            data_source: "OtherSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(
            queue.retry("u", "a", unknown).await,
            Err(bad_choice(
                "That match is not one beets found for this import"
            ))
        );
        assert_eq!(
            queue
                .retry("u", "a", ImportChoice::MusicBrainz("nope".into()))
                .await,
            Err(bad_choice("That is not a MusicBrainz link or ID"))
        );
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));

        assert_eq!(queue.retry("u", "a", ImportChoice::AsIs).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.choices(), vec![DriverChoice::Auto, DriverChoice::AsIs]);
    }

    #[tokio::test]
    async fn a_chosen_run_keeps_the_matches_and_an_automatic_run_replaces_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.script(ImportResult::failed("FakeSrc has no recording w1".into()));
        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(queue.retry("u", "a", pick).await, Ok(()));
        let failed_pick = done_row(&deps, "a", 2).await;
        assert!(failed_pick.retryable && failed_pick.can_choose_import);
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));

        stub.script(ImportResult::failed("Import as-is needs tags".into()));
        assert_eq!(queue.retry("u", "a", ImportChoice::AsIs).await, Ok(()));
        assert!(done_row(&deps, "a", 3).await.can_choose_import);

        // An automatic run that ends some other way leaves nothing to pick.
        stub.script(ImportResult::failed("beets exited with code 1".into()));
        assert_eq!(queue.retry("u", "a", ImportChoice::Auto).await, Ok(()));
        let failed_auto = done_row(&deps, "a", 4).await;
        assert!(failed_auto.retryable && !failed_auto.can_choose_import);
        assert_eq!(queue.import_choices("u", "a"), Err(Refusal::no_choices()));
        assert_eq!(
            queue.retry("u", "a", ImportChoice::AsIs).await,
            Err(Refusal::no_choices())
        );
    }

    #[tokio::test]
    async fn an_automatic_retry_the_importer_never_ran_keeps_the_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.set_health(broken());
        assert_eq!(queue.retry("u", "a", ImportChoice::Auto).await, Ok(()));
        let refused = done_row(&deps, "a", 2).await;
        assert!(refused.retryable && refused.can_choose_import);
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));

        stub.set_health(ready());
        stub.panic_next();
        assert_eq!(queue.retry("u", "a", ImportChoice::Auto).await, Ok(()));
        let stopped = done_row(&deps, "a", 3).await;
        assert!(stopped.retryable && stopped.can_choose_import);
        assert_eq!(queue.import_choices("u", "a"), Ok(found()));
    }

    #[tokio::test]
    async fn a_musicbrainz_link_picks_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;
        let mbid = "6a09041b-cc7f-45d3-b302-39752c525af6";

        let link = format!("https://musicbrainz.org/recording/{mbid}");
        assert_eq!(
            queue.retry("u", "a", ImportChoice::MusicBrainz(link)).await,
            Ok(())
        );
        until_imported(&deps, "a").await;
        assert_eq!(
            stub.choices()[1],
            DriverChoice::Pick {
                data_source: "MusicBrainz".into(),
                id: mbid.into()
            }
        );
    }

    #[tokio::test]
    async fn choices_need_a_kept_job_of_this_user() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _, _) = skipped(tmp.path()).await;
        assert_eq!(
            queue.import_choices("v", "a"),
            Err(Refusal::nothing_to_retry())
        );
        assert_eq!(
            queue.import_choices("u", "b"),
            Err(Refusal::nothing_to_retry())
        );

        stub.script(ImportResult::failed("beets exited with code 1".into()));
        let failing = owned(
            job(file(tmp.path(), "dl/b.flac"), &tmp.path().join("lib")),
            "u",
            &["b"],
        );
        finish(queue.submit(failing).await).await;
        assert_eq!(queue.import_choices("u", "b"), Err(Refusal::no_choices()));
    }

    #[tokio::test]
    async fn retry_resubmits_the_stored_job_and_forgets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let album = tmp.path().join("dl/X");
        file(&album, "1.flac");
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed("beets exited with code 1".into()));

        let mut failing = owned(job(album.clone(), &lib), "u", &["a", "b"]);
        failing.mode = ImportMode::Album;
        finish(queue.submit(failing.clone()).await).await;

        assert_eq!(queue.retry("u", "a", ImportChoice::Auto).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 2);
        assert_eq!(stub.seen()[1].0, vec![album]);
        // The whole album went back in, so its other row has nothing left.
        assert_eq!(
            queue.retry("u", "b", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );

        // An import that succeeds later drops the stored job too.
        stub.script(ImportResult::failed("beets exited with code 1".into()));
        finish(queue.submit(failing.clone()).await).await;
        finish(queue.submit(failing).await).await;
        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn a_second_retry_click_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.set_health(broken());
        let refused = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            "u",
            &["a"],
        );
        finish(queue.submit(refused).await).await;
        stub.set_health(ready());

        assert_eq!(queue.retry("u", "a", ImportChoice::Auto).await, Ok(()));
        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn a_retry_whose_files_are_gone_is_refused_without_importing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed("beets exited with code 1".into()));
        let failing = owned(job(source.clone(), &tmp.path().join("lib")), "u", &["a"]);
        finish(queue.submit(failing).await).await;
        std::fs::remove_file(&source).unwrap();

        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::files_gone())
        );
        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn nothing_to_import_is_not_kept_for_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed("beets exited with code 1".into()));
        let failing = owned(job(source.clone(), &lib), "u", &["a"]);
        finish(queue.submit(failing.clone()).await).await;
        std::fs::remove_file(&source).unwrap();

        // The same job again, now with nothing on disk, drops the stored one.
        finish(queue.submit(failing).await).await;

        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn each_user_keeps_only_the_newest_retries() {
        use super::{RetryStore, MAX_PER_USER};
        use crate::import_queue::test_support::row;
        use crate::import_queue::{ImportJob, JobOwner};
        use std::path::PathBuf;
        use std::sync::Arc;

        let store = RetryStore::default();
        let failed = ImportResult::failed("beets exited with code 1".into());
        let record = |username: &str, item: &str| {
            let job = ImportJob {
                sources: vec![PathBuf::from(format!("/dl/{item}.flac"))],
                target: PathBuf::from("/music"),
                mode: ImportMode::Singleton,
                owner: Some(JobOwner {
                    username: username.into(),
                    rows: vec![row(item)],
                }),
                snapshot_target: false,
                cleanup_root: None,
                choice: DriverChoice::Auto,
                choices: None,
            };
            store.record(&Arc::new(job), &failed);
        };
        for i in 0..=MAX_PER_USER {
            record("u", &format!("{i}"));
        }
        record("v", "0");

        let take = |username: &str, item: &str| store.take(username, item, |_| Ok(()));
        assert_eq!(take("u", "0").unwrap_err(), Refusal::nothing_to_retry());
        for i in 1..=MAX_PER_USER {
            assert!(take("u", &format!("{i}")).is_ok(), "row {i}");
        }
        assert!(take("v", "0").is_ok());
    }

    #[tokio::test]
    async fn forget_drops_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, _) = queue(StubImporter::new());
        for item in ["a", "b"] {
            stub.script(ImportResult::failed("beets exited with code 1".into()));
            let failing = owned(
                job(file(tmp.path(), &format!("dl/{item}.flac")), &lib),
                "u",
                &[item],
            );
            finish(queue.submit(failing).await).await;
        }

        queue.forget_retries("v", &["b".into()]);
        queue.forget_retries("u", &["a".into()]);

        assert_eq!(
            queue.retry("u", "a", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
        assert_eq!(
            queue.retry("v", "b", ImportChoice::Auto).await,
            Err(Refusal::nothing_to_retry())
        );
        assert_eq!(queue.retry("u", "b", ImportChoice::Auto).await, Ok(()));
    }
}
