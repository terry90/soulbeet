//! Claiming files in the transfer registry, then enqueueing them.
//!
//! Every enqueue site claims its keys before it asks the backend for
//! anything, and releases each key whose file was not enqueued.

use shared::download::{DownloadProgress, DownloadableItem, EnqueueStatus, QueuedDownload};
use soulbeet::error::{Result as SoulResult, SoulseekError};
use soulbeet::DownloadBackend;
use std::sync::Arc;

use crate::transfer_registry::{
    AddedSlot, BatchRef, InboxReservation, RequestClaim, TransferKey, TransferRegistry,
};
use tracing::warn;

/// Row text for a file another user's batch is downloading.
pub const OTHER_USER: &str = "already being downloaded by another user";
/// Row text for a file slskd is already fetching outside any batch.
pub const ALREADY_IN_SLSKD: &str = "already downloading in slskd";
/// Refusal for a request whose files the same user is already downloading
/// into another library folder.
pub const ANOTHER_FOLDER: &str = "already downloading into another folder";
/// Row text for a file that joined a batch whose monitor had stopped.
pub const MONITOR_STOPPED: &str = "Download monitoring stopped";

pub fn transfer_key(backend: &'static str, peer: &str, filename: &str) -> TransferKey {
    TransferKey {
        backend,
        peer: peer.to_string(),
        filename: filename.to_string(),
    }
}

fn item_key(backend: &'static str, item: &DownloadableItem) -> TransferKey {
    transfer_key(backend, &item.source, &item.id)
}

/// Why a file gets a Failed row. `None` when it was enqueued, or when one of
/// the user's batches is already downloading it and shows its row.
pub fn failed_reason(status: &EnqueueStatus) -> Option<&str> {
    match status {
        EnqueueStatus::Failed(reason) => Some(reason),
        EnqueueStatus::Enqueued | EnqueueStatus::AlreadyInFlight => None,
    }
}

/// Enqueues files `batch` has claimed into its folder. A file slskd is
/// already fetching comes back Failed, since no batch of ours owns that
/// transfer. Every key whose file was not enqueued is released.
pub async fn enqueue_claimed(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    batch: &BatchRef,
) -> SoulResult<Vec<QueuedDownload>> {
    let registry = TransferRegistry::global();
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let mut queued = match backend.download(items, &batch.destination).await {
        Ok(queued) => queued,
        Err(e) => {
            keys.iter()
                .for_each(|key| registry.release(key, &batch.owner));
            return Err(e);
        }
    };
    for q in &mut queued {
        if q.status == EnqueueStatus::AlreadyInFlight {
            q.status = EnqueueStatus::Failed(ALREADY_IN_SLSKD.to_string());
        }
    }
    // Includes files the backend dropped without answering for them.
    for key in &keys {
        let enqueued = queued.iter().any(|q| {
            q.status == EnqueueStatus::Enqueued && q.source == key.peer && q.item == key.filename
        });
        if !enqueued {
            registry.release(key, &batch.owner);
        }
    }
    Ok(queued)
}

/// Who watches the transfers a request enqueued.
pub enum Watcher {
    /// The request's own batch: it starts a monitor.
    Own,
    /// A running batch of the same user: deliver them through `deliver`.
    Joined(InboxReservation),
}

pub struct RequestEnqueue {
    /// The batch the new transfers went into.
    pub batch: BatchRef,
    /// One entry per file, except those the user is already downloading into
    /// another folder: that batch shows their rows. AlreadyInFlight marks a
    /// file one of the user's batches is already downloading into this
    /// folder, which was not enqueued again.
    pub results: Vec<QueuedDownload>,
    pub watcher: Watcher,
    /// The backend call itself failed: `results` lacks the files sent in it,
    /// and their keys are released.
    pub error: Option<SoulseekError>,
    /// Some files are already downloading into another folder.
    another_folder: bool,
}

impl RequestEnqueue {
    /// Why the request did nothing, when that is because its files are
    /// already downloading into another folder.
    pub fn refusal(&self) -> Option<&'static str> {
        let enqueued = self
            .results
            .iter()
            .any(|q| q.status == EnqueueStatus::Enqueued);
        (self.another_folder && !enqueued).then_some(ANOTHER_FOLDER)
    }
}

/// Claims a request's files and enqueues the free ones. When the user is
/// already downloading some of them into the same folder, the rest join that
/// batch, so its monitor imports them together, unless it has finished.
/// Otherwise they go to `fresh`.
///
/// A request left with its own batch and nothing enqueued still owes that
/// batch a monitor when `batch.inbox.close_if_idle()` fails: another request
/// saw its claims and joined it meanwhile.
pub async fn enqueue_request(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
) -> RequestEnqueue {
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let RequestClaim {
        claim,
        batch,
        reservation,
    } = TransferRegistry::global().claim_for_request(&keys, &fresh);
    let item_of = |key: &TransferKey| {
        let at = keys
            .iter()
            .position(|k| k == key)
            .expect("claimed from these keys");
        items[at].clone()
    };
    let not_enqueued = |key: &TransferKey, status: EnqueueStatus| {
        let item = item_of(key);
        QueuedDownload {
            id: item.id.clone(),
            source: item.source,
            item: item.id,
            size: item.size.unwrap_or(0),
            status,
        }
    };

    // A batch importing elsewhere neither takes this request's files nor
    // lets it import the shared ones here.
    let (here, elsewhere): (Vec<_>, Vec<_>) = claim
        .mine
        .iter()
        .partition(|(_, running)| running.target == fresh.target);
    let mut results: Vec<QueuedDownload> = here
        .iter()
        .map(|(key, _)| not_enqueued(key, EnqueueStatus::AlreadyInFlight))
        .chain(
            claim
                .other_user
                .iter()
                .map(|key| not_enqueued(key, EnqueueStatus::Failed(OTHER_USER.to_string()))),
        )
        .collect();
    let watcher = match reservation {
        Some(reservation) => Watcher::Joined(reservation),
        None => Watcher::Own,
    };

    let to_enqueue: Vec<DownloadableItem> = claim.claimed.iter().map(item_of).collect();
    let mut error = None;
    if !to_enqueue.is_empty() {
        match enqueue_claimed(backend, to_enqueue, &batch).await {
            Ok(queued) => results.extend(queued),
            Err(e) => error = Some(e),
        }
    }
    RequestEnqueue {
        batch,
        results,
        watcher,
        error,
        another_folder: !elsewhere.is_empty(),
    }
}

/// Hands a joining request's enqueued transfers to `batch`'s monitor.
/// Returns the ones nobody will watch because that monitor stopped first,
/// with their keys released and their transfers cancelled, since they would
/// download into the batch's folder with nobody to import them.
pub async fn deliver(
    reservation: InboxReservation,
    enqueued: &[QueuedDownload],
    backend: &Arc<dyn DownloadBackend>,
    batch: &BatchRef,
) -> Vec<AddedSlot> {
    let slots = enqueued
        .iter()
        .map(|q| AddedSlot {
            source: q.source.clone(),
            filename: q.item.clone(),
            transfer_id: q.id.clone(),
        })
        .collect();
    let lost = match reservation.deliver(slots) {
        Ok(()) => return Vec::new(),
        Err(lost) => lost,
    };
    let registry = TransferRegistry::global();
    for slot in &lost {
        registry.release(
            &transfer_key(backend.id(), &slot.source, &slot.filename),
            &batch.owner,
        );
        if let Err(e) = backend
            .cancel_download(&slot.source, &slot.transfer_id, false)
            .await
        {
            warn!(
                "Failed to cancel unwatched transfer {} in slskd: {}",
                slot.transfer_id, e
            );
        }
    }
    lost
}

/// Failed rows for transfers no monitor will watch because the batch they
/// joined stopped first.
pub fn stopped_rows(lost: &[AddedSlot], batch: &BatchRef) -> Vec<DownloadProgress> {
    lost.iter()
        .map(|slot| {
            DownloadProgress::failed(
                slot.transfer_id.clone(),
                slot.source.clone(),
                slot.filename.clone(),
                MONITOR_STOPPED.to_string(),
            )
            .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
        })
        .collect()
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::server_fns::download::test_support::{
        batch, item, key, Cancel, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferOwner;
    use std::path::PathBuf;

    /// Another batch of `running`'s user, importing into the same folder.
    fn same_user(running: &BatchRef) -> BatchRef {
        same_user_into(running, running.target.clone())
    }

    fn same_user_into(running: &BatchRef, target: PathBuf) -> BatchRef {
        BatchRef::new(
            TransferOwner {
                user_id: running.owner.user_id.clone(),
                batch_id: uuid::Uuid::new_v4().to_string(),
            },
            "Come Over".to_string(),
            target,
        )
    }

    fn statuses(results: &[QueuedDownload]) -> Vec<(String, EnqueueStatus)> {
        let mut statuses: Vec<_> = results
            .iter()
            .map(|q| (q.item.clone(), q.status.clone()))
            .collect();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));
        statuses
    }

    #[tokio::test]
    async fn a_repeat_request_joins_the_running_batch() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = same_user(&running);
        let items = vec![
            item("peer_a", "One", "01.flac"),
            item("peer_a", "Two", "02.flac"),
        ];

        let enq = enqueue_request(&backend, items, fresh.clone()).await;

        assert_eq!(
            statuses(&enq.results),
            vec![
                ("01.flac".to_string(), EnqueueStatus::AlreadyInFlight),
                ("02.flac".to_string(), EnqueueStatus::Enqueued),
            ]
        );
        assert_eq!(enq.batch.owner, running.owner);
        assert_eq!(stub.destinations(), vec![running.destination.clone()]);
        let registry = TransferRegistry::global();
        let new_key = key(&backend, "peer_a", "02.flac");
        assert!(registry.may_adopt(&new_key, &running.owner));
        assert!(!registry.may_adopt(&new_key, &fresh.owner));

        let Watcher::Joined(reservation) = enq.watcher else {
            panic!("expected to join the running batch");
        };
        let enqueued: Vec<QueuedDownload> = enq
            .results
            .into_iter()
            .filter(|q| q.status == EnqueueStatus::Enqueued)
            .collect();
        assert!(deliver(reservation, &enqueued, &backend, &running)
            .await
            .is_empty());
        assert_eq!(
            running.inbox.take(),
            vec![AddedSlot {
                source: "peer_a".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02.flac".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn a_repeat_request_for_another_folder_does_not_join() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = same_user_into(&running, PathBuf::from("/other-library"));
        let items = vec![
            item("peer_a", "One", "01.flac"),
            item("peer_a", "Two", "02.flac"),
        ];

        let enq = enqueue_request(&backend, items, fresh.clone()).await;

        // The running batch shows the shared file's row.
        assert_eq!(
            statuses(&enq.results),
            vec![("02.flac".to_string(), EnqueueStatus::Enqueued)]
        );
        assert_eq!(enq.refusal(), None);
        assert!(matches!(enq.watcher, Watcher::Own));
        assert_eq!(enq.batch.owner, fresh.owner);
        assert_eq!(stub.destinations(), vec![fresh.destination.clone()]);
        assert!(
            running.inbox.close_if_idle(),
            "the running batch was not joined"
        );
    }

    #[tokio::test]
    async fn a_repeat_request_joins_the_batch_importing_into_its_folder() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let elsewhere = batch("Come Over");
        let here = same_user_into(&elsewhere, PathBuf::from("/other-library"));
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &elsewhere);
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "02.flac")], &here);
        let items = vec![
            item("peer_a", "One", "01.flac"),
            item("peer_a", "Two", "02.flac"),
            item("peer_a", "Three", "03.flac"),
        ];

        let enq = enqueue_request(&backend, items, same_user(&here)).await;

        assert!(matches!(enq.watcher, Watcher::Joined(_)));
        assert_eq!(enq.batch.owner, here.owner);
        assert_eq!(stub.destinations(), vec![here.destination.clone()]);
        assert_eq!(
            statuses(&enq.results),
            vec![
                ("02.flac".to_string(), EnqueueStatus::AlreadyInFlight),
                ("03.flac".to_string(), EnqueueStatus::Enqueued),
            ]
        );
    }

    #[tokio::test]
    async fn a_request_only_for_files_downloading_elsewhere_is_refused() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);

        let enq = enqueue_request(
            &backend,
            vec![item("peer_a", "One", "01.flac")],
            same_user_into(&running, PathBuf::from("/other-library")),
        )
        .await;

        assert!(
            enq.results.is_empty(),
            "no row would replace the running one"
        );
        assert_eq!(enq.refusal(), Some(ANOTHER_FOLDER));
        assert!(stub.destinations().is_empty());
        assert!(
            running.inbox.close_if_idle(),
            "the running batch was not joined"
        );
    }

    #[tokio::test]
    async fn a_request_already_fully_in_flight_enqueues_nothing() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);

        let enq = enqueue_request(
            &backend,
            vec![item("peer_a", "One", "01.flac")],
            same_user(&running),
        )
        .await;

        assert_eq!(
            statuses(&enq.results),
            vec![("01.flac".to_string(), EnqueueStatus::AlreadyInFlight)]
        );
        assert!(stub.destinations().is_empty());
        assert!(matches!(enq.watcher, Watcher::Own));
        assert!(
            running.inbox.close_if_idle(),
            "no reservation was left behind"
        );
    }

    #[tokio::test]
    async fn a_file_another_user_is_downloading_is_refused() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let theirs = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &theirs);

        let enq = enqueue_request(
            &backend,
            vec![item("peer_a", "One", "01.flac")],
            batch("Mine"),
        )
        .await;

        assert_eq!(
            statuses(&enq.results),
            vec![(
                "01.flac".to_string(),
                EnqueueStatus::Failed(OTHER_USER.to_string())
            )]
        );
        assert!(stub.destinations().is_empty());
        // Still theirs: a third batch may not take it over.
        let held = key(&backend, "peer_a", "01.flac");
        assert!(TransferRegistry::global().may_adopt(&held, &theirs.owner));
        assert!(!TransferRegistry::global().may_adopt(&held, &batch("Other").owner));
    }

    #[tokio::test]
    async fn a_file_slskd_already_fetches_fails_and_frees_its_key() {
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::AlreadyInFlight]);
        let mine = batch("Come Over");

        let enq = enqueue_request(&backend, vec![item("peer_a", "One", "01.flac")], mine).await;

        assert_eq!(
            statuses(&enq.results),
            vec![(
                "01.flac".to_string(),
                EnqueueStatus::Failed(ALREADY_IN_SLSKD.to_string())
            )]
        );
        assert!(TransferRegistry::global()
            .may_adopt(&key(&backend, "peer_a", "01.flac"), &batch("Other").owner));
    }

    #[tokio::test]
    async fn a_finished_running_batch_sends_the_rest_to_a_new_one() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        assert!(running.inbox.close_if_idle());
        let fresh = same_user(&running);
        let items = vec![
            item("peer_a", "One", "01.flac"),
            item("peer_a", "Two", "02.flac"),
        ];

        let enq = enqueue_request(&backend, items, fresh.clone()).await;

        assert!(matches!(enq.watcher, Watcher::Own));
        assert_eq!(enq.batch.owner, fresh.owner);
        assert_eq!(stub.destinations(), vec![fresh.destination.clone()]);
    }

    #[tokio::test]
    async fn a_transport_error_releases_every_claimed_key() {
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);

        let enq =
            enqueue_request(&backend, vec![item("peer_a", "One", "01.flac")], batch("X")).await;

        assert!(enq.error.is_some());
        assert!(enq.results.is_empty());
        assert!(
            enq.batch.inbox.close_if_idle(),
            "nobody joined, so no monitor is owed"
        );
        assert!(TransferRegistry::global()
            .may_adopt(&key(&backend, "peer_a", "01.flac"), &batch("Other").owner));
    }

    #[tokio::test]
    async fn a_delivery_to_a_stopped_batch_comes_back_released_and_cancelled() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        let reservation = running.inbox.reserve().expect("open inbox");
        let enqueued = enqueue_claimed_for(&backend, &running).await;
        running.inbox.close();

        let lost = deliver(reservation, &enqueued, &backend, &running).await;

        assert_eq!(lost.len(), 1);
        assert!(TransferRegistry::global()
            .may_adopt(&key(&backend, "peer_a", "01.flac"), &batch("Other").owner));
        assert_eq!(stub.cancelled(), vec![Cancel::only("peer_a", "t-01.flac")]);
    }

    /// Claims and enqueues `01.flac` into `running`.
    async fn enqueue_claimed_for(
        backend: &Arc<dyn DownloadBackend>,
        running: &BatchRef,
    ) -> Vec<QueuedDownload> {
        let claim = TransferRegistry::global().claim(&[key(backend, "peer_a", "01.flac")], running);
        assert_eq!(claim.claimed.len(), 1);
        enqueue_claimed(backend, vec![item("peer_a", "One", "01.flac")], running)
            .await
            .expect("enqueued")
    }
}
