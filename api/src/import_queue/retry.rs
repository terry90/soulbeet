//! The last job of each row that was not imported, so the row can be
//! retried without the client naming any path. Kept in memory per user;
//! after a restart the ledger's kept imports are restored.

use super::choice::{files_gone, nothing_to_retry, resolve};
use super::rows::rows_with_state;
use super::{ImportJob, ImportQueue, IMPORT_QUEUE};
use crate::auth::UserId;
use crate::ledger::{self, JobEvent};
use shared::download::{
    DownloadState, ImportChoice, ImportRefusal, JobSlot, MatchChoices, MatchKind,
};
use soulbeet::{ImportMode, ImportOutcome, SkipReason};
use std::collections::HashMap;
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

impl Kept {
    /// The ledger slot of the job's row for `item`.
    fn slot(&self, item: &str) -> Option<JobSlot> {
        let owner = self.job.owner.as_ref()?;
        owner.rows.iter().find(|row| row.item == item)?.job.clone()
    }
}

impl ImportJob {
    /// Whether one of the job's rows is `slot`.
    fn has_slot(&self, slot: &JobSlot) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.rows.iter().any(|row| row.job.as_ref() == Some(slot)))
    }
}

#[derive(Default)]
struct Store {
    next_seq: u64,
    /// user id -> row item -> job
    users: HashMap<UserId, HashMap<String, Kept>>,
}

#[derive(Default)]
pub(super) struct RetryStore {
    store: Mutex<Store>,
}

impl RetryStore {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps the job for each of its rows when `keep`, and drops what its
    /// rows kept before otherwise. Returns the slots of the rows this dropped:
    /// other jobs' rows its rows replaced or removed, and rows dropped to stay
    /// within MAX_PER_USER.
    pub(super) fn record(&self, job: &Arc<ImportJob>, keep: bool) -> Vec<JobSlot> {
        let Some(owner) = &job.owner else {
            return vec![];
        };
        let mut dropped = vec![];
        let mut store = self.store();
        let seq = store.next_seq;
        store.next_seq += 1;
        let user = store.users.entry(owner.user_id.clone()).or_default();
        for row in &owner.rows {
            let before = if keep {
                let kept = Kept {
                    seq,
                    job: job.clone(),
                };
                user.insert(row.item.clone(), kept)
            } else {
                user.remove(&row.item)
            };
            dropped.extend(
                before
                    .and_then(|kept| kept.slot(&row.item))
                    .filter(|slot| !job.has_slot(slot)),
            );
        }
        while user.len() > MAX_PER_USER {
            let oldest = user
                .iter()
                .min_by_key(|(_, kept)| kept.seq)
                .map(|(item, _)| item.clone());
            let Some(item) = oldest else {
                break;
            };
            dropped.extend(user.remove(&item).and_then(|kept| kept.slot(&item)));
        }
        if user.is_empty() {
            store.users.remove(&owner.user_id);
        }
        dropped
    }

    /// Removes and returns the job of `item` once `accept` takes it, along
    /// with every entry of its sources, so no row of it can resubmit it a
    /// second time. A job `accept` refuses stays kept. Also returns the slots
    /// of other jobs' rows whose entries went with it.
    fn take<T>(
        &self,
        owner: &UserId,
        item: &str,
        accept: impl FnOnce(&ImportJob) -> Result<T, ImportRefusal>,
    ) -> Result<(Arc<ImportJob>, T, Vec<JobSlot>), ImportRefusal> {
        let mut store = self.store();
        let user = store.users.get_mut(owner).ok_or_else(nothing_to_retry)?;
        let kept = user.get(item).ok_or_else(nothing_to_retry)?;
        let accepted = accept(&kept.job)?;
        let job = kept.job.clone();
        let mut dropped = vec![];
        user.retain(|item, kept| {
            if kept.job.sources != job.sources {
                return true;
            }
            dropped.extend(kept.slot(item).filter(|slot| !job.has_slot(slot)));
            false
        });
        if user.is_empty() {
            store.users.remove(owner);
        }
        Ok((job, accepted, dropped))
    }

    fn peek(&self, owner: &UserId, item: &str) -> Option<Arc<ImportJob>> {
        let store = self.store();
        Some(store.users.get(owner)?.get(item)?.job.clone())
    }

    /// Drops the kept jobs of `items` and returns the slots of their rows.
    fn forget(&self, owner: &UserId, items: &[String]) -> Vec<JobSlot> {
        let mut store = self.store();
        let Some(user) = store.users.get_mut(owner) else {
            return vec![];
        };
        let dropped = items
            .iter()
            .filter_map(|item| user.remove(item)?.slot(item))
            .collect();
        if user.is_empty() {
            store.users.remove(owner);
        }
        dropped
    }
}

impl ImportQueue {
    pub async fn retry(
        &self,
        user: &UserId,
        item: &str,
        choice: ImportChoice,
    ) -> Result<(), ImportRefusal> {
        let job = {
            // A finish recording what it kept cannot be split by the take.
            let _emit = self.shared.emit_lock.lock().await;
            let (job, choice, dropped) = self
                .shared
                .retries
                .take(user, item, |job| resolve(&choice, job))?;
            self.shared.dismiss(dropped);
            ImportJob {
                choice,
                ..Arc::unwrap_or_clone(job)
            }
        };
        if !job.holds_audio().await {
            let refusal = files_gone();
            // The job is no longer kept, so its slots end as a restart would end them.
            if let Some(owner) = &job.owner {
                let rows = rows_with_state(&owner.rows, DownloadState::Failed(String::new()))
                    .into_iter()
                    .map(|row| row.with_failure(refusal.failure.clone()))
                    .collect();
                self.shared.deps.record(JobEvent::Slots {
                    state: ledger::JobState::Failed,
                    rows,
                });
            }
            return Err(refusal);
        }
        self.submit(job).await;
        Ok(())
    }

    /// Keeps `job` for its rows' Retry as a finished automatic run would have
    /// (a kept import read back after a restart), when its sources still hold
    /// audio; files_gone() otherwise, keeping nothing.
    pub async fn restore(&self, job: ImportJob) -> Result<(), ImportRefusal> {
        if !job.holds_audio().await {
            return Err(files_gone());
        }
        let _emit = self.shared.emit_lock.lock().await;
        let dropped = self.shared.retries.record(&Arc::new(job), true);
        self.shared.dismiss(dropped);
        Ok(())
    }

    /// What the kept job of `item` can pick from, leaving it kept.
    pub fn import_choices(&self, user: &UserId, item: &str) -> Result<MatchChoices, ImportRefusal> {
        let job = self
            .shared
            .retries
            .peek(user, item)
            .ok_or_else(nothing_to_retry)?;
        Ok(job.choices.as_deref().cloned().unwrap_or(MatchChoices {
            kind: match job.mode {
                ImportMode::Album => MatchKind::Album,
                ImportMode::Singleton => MatchKind::Track,
            },
            candidates: vec![],
        }))
    }

    /// Drops the kept jobs of `items` and dismisses their slots (D5).
    pub fn forget_retries(&self, user: &UserId, items: &[String]) {
        let dropped = self.shared.retries.forget(user, items);
        self.shared.dismiss(dropped);
    }
}

pub async fn retry_import_for(
    user: &UserId,
    item: &str,
    choice: ImportChoice,
) -> Result<(), ImportRefusal> {
    IMPORT_QUEUE.retry(user, item, choice).await
}

pub fn import_choices_for(user: &UserId, item: &str) -> Result<MatchChoices, ImportRefusal> {
    IMPORT_QUEUE.import_choices(user, item)
}

pub fn forget_retries(user: &UserId, items: &[String]) {
    IMPORT_QUEUE.forget_retries(user, items);
}

#[cfg(test)]
mod tests {
    use crate::auth::UserId;
    use crate::import_queue::choice::{
        files_gone, no_choices, not_an_id, nothing_to_retry, unknown_candidate,
    };
    use crate::import_queue::test_support::{
        alice, bob, broken, candidate, no_match, ready, track_choices, RecordingDeps, StubImporter,
    };
    use crate::import_queue::tests::{file, finish, job, owned, queue};
    use crate::import_queue::{ImportJob, ImportMode, ImportQueue};
    use crate::ledger::{JobEvent, JobState as LedgerState};
    use shared::download::{
        DownloadProgress, DownloadState, ImportChoice, JobSlot, MatchChoices, MatchKind,
    };
    use soulbeet::{DriverChoice, FailKind, ImportResult};
    use std::sync::Arc;
    use std::time::Duration;

    async fn until_imported(deps: &RecordingDeps, item: &str) {
        let done = || {
            deps.progress_for(&alice())
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
            deps.rows_for(&alice())
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
        let skipping = owned(
            job(file(tmp, "dl/a.flac"), &tmp.join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(skipping.clone()).await).await;
        (queue, stub, deps, skipping)
    }

    #[tokio::test]
    async fn a_skipped_automatic_import_offers_its_matches_until_one_is_picked() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        assert!(done_row(&deps, "a", 1).await.retryable);
        // Looking does not use up the row.
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w2".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Ok(()));
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
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
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
            queue.retry(&alice(), "a", unknown).await,
            Err(unknown_candidate())
        );
        assert_eq!(
            queue
                .retry(&alice(), "a", ImportChoice::MusicBrainz("nope".into()))
                .await,
            Err(not_an_id())
        );
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.choices(), vec![DriverChoice::Auto, DriverChoice::AsIs]);
    }

    #[tokio::test]
    async fn a_chosen_run_keeps_the_matches_and_an_automatic_run_replaces_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.script(ImportResult::failed(
            FailKind::ChoiceInvalid,
            "FakeSrc has no recording w1".into(),
        ));
        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Ok(()));
        let failed_pick = done_row(&deps, "a", 2).await;
        assert!(failed_pick.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        stub.script(ImportResult::failed(
            FailKind::AsIsMissingTags,
            "Import as-is needs tags".into(),
        ));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
        assert!(done_row(&deps, "a", 3).await.retryable);

        // An automatic run that ends some other way leaves nothing to pick.
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let failed_auto = done_row(&deps, "a", 4).await;
        assert!(failed_auto.retryable);
        assert_eq!(
            queue.import_choices(&alice(), "a"),
            Ok(MatchChoices {
                kind: MatchKind::Track,
                candidates: vec![]
            })
        );
        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Err(no_choices()));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
    }

    #[tokio::test]
    async fn an_automatic_retry_the_importer_never_ran_keeps_the_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.set_health(broken());
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let refused = done_row(&deps, "a", 2).await;
        assert!(refused.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        stub.set_health(ready());
        stub.panic_next();
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let stopped = done_row(&deps, "a", 3).await;
        assert!(stopped.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));
    }

    #[tokio::test]
    async fn a_musicbrainz_link_picks_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;
        let mbid = "6a09041b-cc7f-45d3-b302-39752c525af6";

        let link = format!("https://musicbrainz.org/recording/{mbid}");
        assert_eq!(
            queue
                .retry(&alice(), "a", ImportChoice::MusicBrainz(link))
                .await,
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
        assert_eq!(queue.import_choices(&bob(), "a"), Err(nothing_to_retry()));
        assert_eq!(queue.import_choices(&alice(), "b"), Err(nothing_to_retry()));

        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(
            job(file(tmp.path(), "dl/b.flac"), &tmp.path().join("lib")),
            &alice(),
            &["b"],
        );
        finish(queue.submit(failing).await).await;
        assert_eq!(
            queue.import_choices(&alice(), "b"),
            Ok(MatchChoices {
                kind: MatchKind::Track,
                candidates: vec![]
            })
        );
    }

    #[tokio::test]
    async fn retry_resubmits_the_stored_job_and_forgets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let album = tmp.path().join("dl/X");
        file(&album, "1.flac");
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));

        let mut failing = owned(job(album.clone(), &lib), &alice(), &["a", "b"]);
        failing.mode = ImportMode::Album;
        finish(queue.submit(failing.clone()).await).await;

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 2);
        assert_eq!(stub.seen()[1].0, vec![album]);
        // The whole album went back in, so its other row has nothing left.
        assert_eq!(
            queue.retry(&alice(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );

        // An import that succeeds later drops the stored job too.
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        finish(queue.submit(failing.clone()).await).await;
        finish(queue.submit(failing).await).await;
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn a_second_retry_click_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.set_health(broken());
        let refused = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(refused).await).await;
        stub.set_health(ready());

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn a_retry_whose_files_are_gone_is_refused_without_importing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(
            job(source.clone(), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(failing).await).await;
        std::fs::remove_file(&source).unwrap();

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(files_gone())
        );
        // The job is no longer kept, so its slot fails with the refusal.
        let events = deps.ledger_events();
        let Some(JobEvent::Slots {
            state: LedgerState::Failed,
            rows,
        }) = events.last()
        else {
            panic!("{events:?}")
        };
        let ended: Vec<_> = rows
            .iter()
            .map(|row| (row.job.clone(), row.failure.clone()))
            .collect();
        assert_eq!(ended, vec![(Some(first("j1")), Some(files_gone().failure))]);
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn nothing_to_import_is_not_kept_for_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(job(source.clone(), &lib), &alice(), &["a"]);
        finish(queue.submit(failing.clone()).await).await;
        std::fs::remove_file(&source).unwrap();

        // The same job again, now with nothing on disk, drops the stored one.
        finish(queue.submit(failing).await).await;

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    /// A singleton job of `user` for `item`, its row the ledger's `slot`.
    fn single(user: &UserId, item: &str, slot: JobSlot) -> Arc<ImportJob> {
        use crate::import_queue::test_support::row;
        use crate::import_queue::JobOwner;
        Arc::new(ImportJob {
            sources: vec![std::path::PathBuf::from(format!("/dl/{item}.flac"))],
            target: std::path::PathBuf::from("/music"),
            mode: ImportMode::Singleton,
            owner: Some(JobOwner {
                user_id: user.clone(),
                rows: vec![row(item).with_job(slot)],
            }),
            snapshot_target: false,
            cleanup_root: None,
            choice: DriverChoice::Auto,
            choices: None,
        })
    }

    /// `job` with its rows moved to the ledger job `id`.
    fn under(mut job: ImportJob, id: &str) -> ImportJob {
        for row in &mut job.owner.as_mut().unwrap().rows {
            row.job.as_mut().unwrap().job = id.into();
        }
        job
    }

    /// Every slot the queue recorded as dismissed, in order.
    fn dismissed(deps: &RecordingDeps) -> Vec<JobSlot> {
        deps.ledger_events()
            .into_iter()
            .filter_map(|event| match event {
                JobEvent::Dismiss(slots) => Some(slots),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn first(id: &str) -> JobSlot {
        JobSlot {
            job: id.into(),
            slot: 0,
        }
    }

    #[test]
    fn a_row_another_job_kept_hands_back_that_jobs_slot() {
        let store = super::RetryStore::default();
        let a = |id: &str| single(&alice(), "a", first(id));
        assert!(store.record(&a("j1"), true).is_empty());
        // The same job kept again replaces only itself.
        assert!(store.record(&a("j1"), true).is_empty());
        assert_eq!(store.record(&a("j2"), true), vec![first("j1")]);
        assert_eq!(store.record(&a("j3"), false), vec![first("j2")]);
        assert!(store.record(&a("j3"), false).is_empty());
    }

    #[tokio::test]
    async fn a_later_job_for_the_same_row_dismisses_the_earlier_kept_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, deps) = queue(StubImporter::new());
        let submit = |id: &str, dir: &str| {
            let source = file(tmp.path(), &format!("{dir}/a.flac"));
            under(owned(job(source, &lib), &alice(), &["a"]), id)
        };
        stub.script(no_match(1));
        finish(queue.submit(submit("j1", "dl1")).await).await;
        stub.script(no_match(1));
        finish(queue.submit(submit("j2", "dl2")).await).await;
        assert_eq!(dismissed(&deps), vec![first("j1")]);

        // A later job that imports drops the kept one too.
        finish(queue.submit(submit("j3", "dl3")).await).await;
        assert_eq!(dismissed(&deps), vec![first("j1"), first("j2")]);
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
    }

    #[tokio::test]
    async fn a_restored_import_dismisses_the_kept_row_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, _, deps) = queue(StubImporter::new());
        for (id, dir) in [("j1", "dl1"), ("j2", "dl2")] {
            let source = file(tmp.path(), &format!("{dir}/a.flac"));
            let kept = under(owned(job(source, &lib), &alice(), &["a"]), id);
            let restored = ImportJob::from_stored(kept.stored().unwrap(), alice());
            assert_eq!(queue.restore(restored).await, Ok(()));
        }
        assert_eq!(dismissed(&deps), vec![first("j1")]);
    }

    #[tokio::test]
    async fn a_retry_dismisses_other_jobs_rows_of_the_same_files() {
        let tmp = tempfile::tempdir().unwrap();
        let source = file(tmp.path(), "dl/a.flac");
        let lib = tmp.path().join("lib");
        let (queue, stub, deps) = queue(StubImporter::new());
        for (id, item) in [("j1", "a"), ("j2", "b")] {
            stub.script(no_match(1));
            let kept = under(owned(job(source.clone(), &lib), &alice(), &[item]), id);
            finish(queue.submit(kept).await).await;
        }
        assert!(dismissed(&deps).is_empty());

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        assert_eq!(dismissed(&deps), vec![first("j2")]);
        assert_eq!(
            queue.retry(&alice(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[test]
    fn each_user_keeps_only_the_newest_retries() {
        use super::{RetryStore, MAX_PER_USER};

        let store = RetryStore::default();
        let slot = |item: &str| JobSlot {
            job: format!("j-{item}"),
            slot: 0,
        };
        let record =
            |user: &UserId, item: &str| store.record(&single(user, item, slot(item)), true);
        for i in 0..MAX_PER_USER {
            assert!(record(&alice(), &format!("{i}")).is_empty());
        }
        // The oldest row is dropped, and its slot returned for dismissing.
        assert_eq!(
            record(&alice(), &format!("{MAX_PER_USER}")),
            vec![slot("0")]
        );
        assert!(record(&bob(), "0").is_empty());

        let take = |user: &UserId, item: &str| store.take(user, item, |_| Ok(()));
        assert_eq!(take(&alice(), "0").unwrap_err(), nothing_to_retry());
        for i in 1..=MAX_PER_USER {
            assert!(take(&alice(), &format!("{i}")).is_ok(), "row {i}");
        }
        assert!(take(&bob(), "0").is_ok());
    }

    #[tokio::test]
    async fn a_kept_import_is_found_by_the_owners_id_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let alice = alice();
        let failing = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice,
            &["a"],
        );
        finish(queue.submit(failing).await).await;

        assert_eq!(
            queue
                .retry(&UserId::new("alice"), "a", ImportChoice::Auto)
                .await,
            Err(nothing_to_retry())
        );
        assert_ne!(
            queue.retry(&alice, "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn forget_drops_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, _) = queue(StubImporter::new());
        for item in ["a", "b"] {
            stub.script(ImportResult::failed(
                FailKind::Beets,
                "beets exited with code 1".into(),
            ));
            let failing = owned(
                job(file(tmp.path(), &format!("dl/{item}.flac")), &lib),
                &alice(),
                &[item],
            );
            finish(queue.submit(failing).await).await;
        }

        queue.forget_retries(&bob(), &["b".into()]);
        queue.forget_retries(&alice(), &["a".into()]);

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(
            queue.retry(&bob(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(queue.retry(&alice(), "b", ImportChoice::Auto).await, Ok(()));
    }
}
