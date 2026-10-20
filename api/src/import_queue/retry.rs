//! The last job of each row that was not imported, so the row can be
//! retried without the client naming any path. Kept in memory per user and
//! lost on restart.

use super::{ImportJob, ImportQueue, IMPORT_QUEUE};
use soulbeet::audio::count_audio_files;
use soulbeet::{ImportOutcome, ImportResult, SkipReason};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

const NOTHING_TO_RETRY: &str = "Nothing to retry";
const FILES_GONE: &str = "The kept files are gone";

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
    pub(super) fn record(&self, job: &ImportJob, result: &ImportResult) {
        let Some(owner) = &job.owner else {
            return;
        };
        let keep = keeps_retry(&result.outcome);
        let mut store = self.store();
        let seq = store.next_seq;
        store.next_seq += 1;
        let shared = Arc::new(job.clone());
        let user = store.users.entry(owner.username.clone()).or_default();
        for row in &owner.rows {
            if keep {
                let kept = Kept {
                    seq,
                    job: shared.clone(),
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

    /// Removes and returns the job of `item`, along with its other rows'
    /// entries, so no row of it can resubmit it a second time.
    fn take(&self, username: &str, item: &str) -> Option<Arc<ImportJob>> {
        let mut store = self.store();
        let user = store.users.get_mut(username)?;
        let job = user.remove(item)?.job;
        user.retain(|_, kept| kept.job.sources != job.sources);
        if user.is_empty() {
            store.users.remove(username);
        }
        Some(job)
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
    pub async fn retry(&self, username: &str, item: &str) -> Result<(), String> {
        let job = self
            .shared
            .retries
            .take(username, item)
            .ok_or_else(|| NOTHING_TO_RETRY.to_string())?;
        let sources: Vec<&Path> = job.sources.iter().map(PathBuf::as_path).collect();
        if count_audio_files(&sources).await == 0 {
            return Err(FILES_GONE.to_string());
        }
        self.submit(Arc::unwrap_or_clone(job)).await;
        Ok(())
    }

    pub fn forget_retries(&self, username: &str, items: &[String]) {
        self.shared.retries.forget(username, items);
    }
}

pub async fn retry_import_for(username: &str, item: &str) -> Result<(), String> {
    IMPORT_QUEUE.retry(username, item).await
}

pub fn forget_retries(username: &str, items: &[String]) {
    IMPORT_QUEUE.forget_retries(username, items);
}

#[cfg(test)]
mod tests {
    use crate::import_queue::test_support::{broken, ready, RecordingDeps, StubImporter};
    use crate::import_queue::tests::{file, finish, job, owned, queue};
    use crate::import_queue::ImportMode;
    use shared::download::DownloadState;
    use soulbeet::ImportResult;
    use std::time::Duration;

    const NOTHING: &str = "Nothing to retry";
    const FILES_GONE: &str = "The kept files are gone";

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

        assert_eq!(queue.retry("u", "a").await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 2);
        assert_eq!(stub.seen()[1].0, vec![album]);
        // The whole album went back in, so its other row has nothing left.
        assert_eq!(queue.retry("u", "b").await, Err(NOTHING.into()));

        // An import that succeeds later drops the stored job too.
        stub.script(ImportResult::failed("beets exited with code 1".into()));
        finish(queue.submit(failing.clone()).await).await;
        finish(queue.submit(failing).await).await;
        assert_eq!(queue.retry("u", "a").await, Err(NOTHING.into()));
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

        assert_eq!(queue.retry("u", "a").await, Ok(()));
        assert_eq!(queue.retry("u", "a").await, Err(NOTHING.into()));
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

        assert_eq!(queue.retry("u", "a").await, Err(FILES_GONE.into()));
        assert_eq!(queue.retry("u", "a").await, Err(NOTHING.into()));
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

        assert_eq!(queue.retry("u", "a").await, Err(NOTHING.into()));
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn each_user_keeps_only_the_newest_retries() {
        use super::{RetryStore, MAX_PER_USER};
        use crate::import_queue::test_support::row;
        use crate::import_queue::{ImportJob, JobOwner};
        use std::path::PathBuf;

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
            };
            store.record(&job, &failed);
        };
        for i in 0..=MAX_PER_USER {
            record("u", &format!("{i}"));
        }
        record("v", "0");

        assert!(store.take("u", "0").is_none());
        for i in 1..=MAX_PER_USER {
            assert!(store.take("u", &format!("{i}")).is_some(), "row {i}");
        }
        assert!(store.take("v", "0").is_some());
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

        assert_eq!(queue.retry("u", "a").await, Err(NOTHING.into()));
        assert_eq!(queue.retry("v", "b").await, Err(NOTHING.into()));
        assert_eq!(queue.retry("u", "b").await, Ok(()));
    }
}
