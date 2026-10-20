//! The last job of each row that was not imported, so the row can be
//! retried without the client naming any path. Kept in memory per user and
//! lost on restart.

use super::{ImportJob, ImportQueue, IMPORT_QUEUE};
use soulbeet::{ImportOutcome, ImportResult};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

const NOTHING_TO_RETRY: &str = "Nothing to retry";

/// username -> row item -> job
#[derive(Default)]
pub(super) struct RetryStore {
    jobs: Mutex<HashMap<String, HashMap<String, ImportJob>>>,
}

impl RetryStore {
    fn jobs(&self) -> MutexGuard<'_, HashMap<String, HashMap<String, ImportJob>>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps the job for each of its rows unless it imported.
    pub(super) fn record(&self, job: &ImportJob, result: &ImportResult) {
        let Some(owner) = &job.owner else {
            return;
        };
        let imported = matches!(result.outcome, ImportOutcome::Imported { .. });
        let mut jobs = self.jobs();
        let user = jobs.entry(owner.username.clone()).or_default();
        for row in &owner.rows {
            if imported {
                user.remove(&row.item);
            } else {
                user.insert(row.item.clone(), job.clone());
            }
        }
        if user.is_empty() {
            jobs.remove(&owner.username);
        }
    }

    /// Removes and returns the job of `item`, along with its other rows'
    /// entries, so no row of it can resubmit it a second time.
    fn take(&self, username: &str, item: &str) -> Option<ImportJob> {
        let mut jobs = self.jobs();
        let user = jobs.get_mut(username)?;
        let job = user.remove(item)?;
        user.retain(|_, stored| stored.sources != job.sources);
        if user.is_empty() {
            jobs.remove(username);
        }
        Some(job)
    }

    fn forget(&self, username: &str, items: &[String]) {
        let mut jobs = self.jobs();
        let Some(user) = jobs.get_mut(username) else {
            return;
        };
        for item in items {
            user.remove(item);
        }
        if user.is_empty() {
            jobs.remove(username);
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
        self.submit(job).await;
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
