//! Waiting inside a call for running work to change (design 10.4).

use std::future::Future;
use std::time::Duration;

use shared::failure::Failure;

use crate::ledger::Ledger;

/// The longest wait a call takes. With the 20 seconds a call may then read for, an answer comes
/// within 50 seconds: under the 60 second read timeout common reverse proxies have by default,
/// and under the 90 seconds after which claude.ai was seen to drop a call (2026-11-17).
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(30);

/// Reads with `read` until `ready` holds for what it read, reading again on every ledger
/// change, or until `wait` (at most MAX_WAIT) has passed: the last read either way.
pub(crate) async fn until<T, F, Fut>(
    ledger: &Ledger,
    wait: Duration,
    read: F,
    ready: impl Fn(&T) -> bool,
) -> Result<T, Failure>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, Failure>>,
{
    let deadline = tokio::time::Instant::now() + wait.min(MAX_WAIT);
    let mut changes = ledger.changes();
    loop {
        // Seen before the read, so a change made after it wakes the wait.
        changes.borrow_and_update();
        let value = read().await?;
        if ready(&value) {
            return Ok(value);
        }
        match tokio::time::timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => continue,
            // The time is up, or the writer stopped and nothing will change.
            Ok(Err(_)) | Err(_) => return Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use shared::download::{DownloadQuery, SearchStats};

    use super::*;
    use crate::auth::UserId;
    use crate::ledger::NewSearch;

    // On the real clock: the ledger's writer answers on SQLite's own thread.
    #[tokio::test]
    async fn a_wait_reads_again_on_every_change_and_ends_with_the_last_read() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db);
        let reads = AtomicU32::new(0);
        let read = || async { Ok(reads.fetch_add(1, Ordering::SeqCst)) };

        // Nothing changes: the first read, once the time is up.
        let started = tokio::time::Instant::now();
        let wait = Duration::from_millis(300);
        assert_eq!(until(&ledger, wait, read, |_| false).await, Ok(0));
        assert!(started.elapsed() >= wait);
        assert_eq!(until(&ledger, Duration::ZERO, read, |_| false).await, Ok(1));

        // A search that ends is a change: the wait reads again and ends once it is ready.
        ledger
            .record_search(NewSearch {
                id: "s1".into(),
                user: UserId::new("alice"),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![]),
                spec: None,
            })
            .await
            .unwrap();
        let writer = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            writer
                .search_ended("s1", Ok((vec![], Some(SearchStats::default()))))
                .await
                .unwrap();
        });
        let started = tokio::time::Instant::now();
        assert_eq!(until(&ledger, MAX_WAIT, read, |n| *n >= 3).await, Ok(3));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
