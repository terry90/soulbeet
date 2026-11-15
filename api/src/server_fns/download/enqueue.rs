//! Claiming files in the transfer registry, then enqueueing them.
//!
//! Every enqueue site claims its keys before it asks the backend for
//! anything, and releases each key whose file was not enqueued.

use shared::download::{
    DownloadProgress, DownloadableGroup, DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Cause, Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::{Result as SoulResult, SoulseekError};
use soulbeet::DownloadBackend;
use std::sync::Arc;

use super::failover::{align_pool_tracks, alternates_for};
use crate::transfer_registry::{
    AddedSlot, BatchRef, Claim, InboxReservation, RequestClaim, TransferKey, TransferRegistry,
};
use tracing::warn;

/// Row text for a file another user's batch is downloading.
pub const OTHER_USER: &str = "already being downloaded by another user";
/// Row text for a file slskd is already fetching outside any batch.
pub const ALREADY_IN_SLSKD: &str = "already downloading in slskd";
/// Refusal for a request whose files the same user is already downloading
/// into another library folder.
pub const ANOTHER_FOLDER: &str = "already downloading into another folder";
/// Reason for a file the same user's batch is already downloading into the
/// same folder.
pub const ALREADY_MINE: &str = "already downloading for this user";
/// Row text for a file that joined a batch whose monitor had stopped.
pub const MONITOR_STOPPED: &str = "Download monitoring stopped";
/// Reason for a file the backend answered nothing for.
pub const NOTHING_ENQUEUED: &str = "nothing enqueued";

/// A file that joined a batch whose monitor had stopped.
pub fn monitor_stopped() -> Failure {
    Failure::new(
        FailureCode::TransferCancelled,
        Stage::Transfer,
        RetryAdvice::SameSourceLater,
        MONITOR_STOPPED,
    )
}

/// Another user's batch is downloading the file.
pub fn other_user() -> Failure {
    Failure::new(
        FailureCode::AlreadyInFlightOtherUser,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        OTHER_USER,
    )
}

/// The backend is fetching the file outside every batch of ours, so there
/// is no run to follow. `cause` is the backend's answer.
pub fn already_in_slskd(cause: &Cause) -> Failure {
    Failure {
        cause: Some(cause.clone()),
        ..Failure::new(
            FailureCode::EnqueueFailed,
            Stage::Enqueue,
            RetryAdvice::OtherSource,
            ALREADY_IN_SLSKD,
        )
    }
}

/// The same user's batch is downloading the file into another folder.
pub fn another_folder(retry: RetryAdvice) -> Failure {
    Failure::new(
        FailureCode::AlreadyInFlightOtherFolder,
        Stage::Enqueue,
        retry,
        ANOTHER_FOLDER,
    )
}

/// The same user's batch is downloading the file into this folder.
pub fn already_running() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        ALREADY_MINE,
    )
}

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
/// the user's batches is already downloading it and shows its row. A file
/// the backend is already fetching fails, since no batch of ours owns that
/// transfer.
pub fn failed_reason(status: &EnqueueStatus) -> Option<Failure> {
    match status {
        EnqueueStatus::Failed(reason) => Some(reason.clone()),
        EnqueueStatus::AlreadyInFlight(cause) => Some(already_in_slskd(cause)),
        EnqueueStatus::Enqueued | EnqueueStatus::AlreadyRunning => None,
    }
}

/// Why none of `results` was enqueued: the first failed file's failure, else
/// `nothing enqueued` for a backend that answered nothing for its files.
pub fn not_enqueued_failure(results: &[QueuedDownload]) -> Failure {
    results
        .iter()
        .find_map(|q| failed_reason(&q.status))
        .unwrap_or_else(|| {
            Failure::new(
                FailureCode::EnqueueFailed,
                Stage::Enqueue,
                RetryAdvice::OtherSource,
                NOTHING_ENQUEUED,
            )
        })
}

/// Why `batch` could not claim its one key, when `claim` found it held:
/// by another user, by the same user into another folder, or by the same
/// user into this one.
pub fn held_failure(claim: &Claim, batch: &BatchRef) -> Failure {
    if !claim.other_user.is_empty() {
        other_user()
    } else if claim.mine[0].1.target != batch.target {
        another_folder(RetryAdvice::OtherSource)
    } else {
        already_running()
    }
}

/// Enqueues files `batch` has claimed into its folder. Every key whose file
/// was not enqueued is released.
pub async fn enqueue_claimed(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    batch: &BatchRef,
) -> SoulResult<Vec<QueuedDownload>> {
    let registry = TransferRegistry::global();
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let queued = match backend.download(items, &batch.destination).await {
        Ok(queued) => queued,
        Err(e) => {
            keys.iter()
                .for_each(|key| registry.release(key, &batch.owner));
            return Err(e);
        }
    };
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
    /// another folder: that batch shows their rows. AlreadyRunning marks a
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
        .map(|(key, _)| not_enqueued(key, EnqueueStatus::AlreadyRunning))
        .chain(
            claim
                .other_user
                .iter()
                .map(|key| not_enqueued(key, EnqueueStatus::Failed(other_user()))),
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

/// Hands a joining request's enqueued transfers to `batch`'s monitor, each
/// with the alternates `rest` holds for the item it came from among `items`,
/// none for one it cannot be lined up with.
/// Returns the ones nobody will watch because that monitor stopped first,
/// with their keys released and their transfers cancelled, since they would
/// download into the batch's folder with nobody to import them.
pub async fn deliver(
    reservation: InboxReservation,
    enqueued: &[QueuedDownload],
    items: &[DownloadableItem],
    rest: &[DownloadableGroup],
    backend: &Arc<dyn DownloadBackend>,
    batch: &BatchRef,
) -> Vec<AddedSlot> {
    let slots = enqueued
        .iter()
        .zip(align_pool_tracks(enqueued, items))
        .map(|(q, track)| AddedSlot {
            source: q.source.clone(),
            filename: q.item.clone(),
            transfer_id: q.id.clone(),
            alternates: track.map_or_else(Vec::new, |track| alternates_for(&track, rest)),
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
                monitor_stopped(),
            )
            .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
        })
        .collect()
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::server_fns::download::test_support::{
        batch, group, item, key, same_user_into, Cancel, ScriptedOutcome, StubBackend,
        STUB_IN_FLIGHT,
    };
    use shared::failure::{FailureCode, RetryAdvice, Service, Stage};
    use std::path::PathBuf;

    /// Another batch of `running`'s user, importing into the same folder.
    fn same_user(running: &BatchRef) -> BatchRef {
        same_user_into(running, running.target.clone())
    }

    /// An `EnqueueStatus` with a failure reduced to its code.
    #[derive(Debug, PartialEq)]
    enum Answer {
        Enqueued,
        AlreadyInFlight,
        AlreadyRunning,
        Failed(FailureCode),
    }

    fn statuses(results: &[QueuedDownload]) -> Vec<(String, Answer)> {
        let mut statuses: Vec<_> = results
            .iter()
            .map(|q| {
                let answer = match &q.status {
                    EnqueueStatus::Enqueued => Answer::Enqueued,
                    EnqueueStatus::AlreadyInFlight(_) => Answer::AlreadyInFlight,
                    EnqueueStatus::AlreadyRunning => Answer::AlreadyRunning,
                    EnqueueStatus::Failed(f) => Answer::Failed(f.code),
                };
                (q.item.clone(), answer)
            })
            .collect();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));
        statuses
    }

    #[test]
    fn enqueue_refusals_keep_their_text() {
        use RetryAdvice::*;
        let in_backend = Cause::new(Service::Slskd, None, "Skipped: Already in progress");
        for (f, code, retry, message) in [
            (
                other_user(),
                FailureCode::AlreadyInFlightOtherUser,
                OtherSource,
                OTHER_USER,
            ),
            (
                already_in_slskd(&in_backend),
                FailureCode::EnqueueFailed,
                OtherSource,
                ALREADY_IN_SLSKD,
            ),
            (
                another_folder(OtherSource),
                FailureCode::AlreadyInFlightOtherFolder,
                OtherSource,
                ANOTHER_FOLDER,
            ),
            (
                another_folder(NotRetryable),
                FailureCode::AlreadyInFlightOtherFolder,
                NotRetryable,
                ANOTHER_FOLDER,
            ),
            (
                already_running(),
                FailureCode::AlreadyRunning,
                OtherSource,
                ALREADY_MINE,
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, Stage::Enqueue, retry, message)
            );
        }
        assert_eq!(already_in_slskd(&in_backend).cause, Some(in_backend));
    }

    #[test]
    fn a_request_left_unenqueued_names_the_first_files_reason() {
        let answer = |status| QueuedDownload {
            id: "01.flac".into(),
            source: "peer_a".into(),
            item: "01.flac".into(),
            size: 0,
            status,
        };
        let offline = Failure::new(
            FailureCode::PeerOffline,
            Stage::Enqueue,
            RetryAdvice::OtherSource,
            "User 'peer_a' is offline",
        );
        assert_eq!(
            not_enqueued_failure(&[
                answer(EnqueueStatus::AlreadyRunning),
                answer(EnqueueStatus::Failed(offline.clone())),
            ]),
            offline
        );
        let in_flight = Cause::new(Service::Slskd, None, STUB_IN_FLIGHT);
        assert_eq!(
            not_enqueued_failure(&[answer(EnqueueStatus::AlreadyInFlight(in_flight.clone()))]),
            already_in_slskd(&in_flight)
        );

        let nothing = not_enqueued_failure(&[]);
        assert_eq!(
            (
                nothing.code,
                nothing.stage,
                nothing.retry,
                nothing.message.as_str()
            ),
            (
                FailureCode::EnqueueFailed,
                Stage::Enqueue,
                RetryAdvice::OtherSource,
                NOTHING_ENQUEUED
            )
        );
        assert_eq!(nothing.cause, None);
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

        let rest = vec![group(
            "peer_b",
            vec![
                item("peer_b", "Two", "b/02.flac"),
                item("peer_b", "Three", "b/03.flac"),
            ],
            0.8,
        )];

        let enq = enqueue_request(&backend, items.clone(), fresh.clone()).await;

        assert_eq!(
            statuses(&enq.results),
            vec![
                ("01.flac".to_string(), Answer::AlreadyRunning),
                ("02.flac".to_string(), Answer::Enqueued),
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
        assert!(
            deliver(reservation, &enqueued, &items, &rest, &backend, &running)
                .await
                .is_empty()
        );
        assert_eq!(
            running.inbox.take(),
            vec![AddedSlot {
                source: "peer_a".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02.flac".to_string(),
                alternates: vec![item("peer_b", "Two", "b/02.flac")],
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
            vec![("02.flac".to_string(), Answer::Enqueued)]
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
                ("02.flac".to_string(), Answer::AlreadyRunning),
                ("03.flac".to_string(), Answer::Enqueued),
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
            vec![("01.flac".to_string(), Answer::AlreadyRunning)]
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
                Answer::Failed(FailureCode::AlreadyInFlightOtherUser)
            )]
        );
        assert_eq!(
            failed_reason(&enq.results[0].status).unwrap().message,
            OTHER_USER
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
            vec![("01.flac".to_string(), Answer::AlreadyInFlight)]
        );
        let failure = failed_reason(&enq.results[0].status).unwrap();
        assert_eq!(
            (failure.code, failure.message.as_str()),
            (FailureCode::EnqueueFailed, ALREADY_IN_SLSKD)
        );
        assert_eq!(
            failure.cause,
            Some(Cause::new(Service::Slskd, None, STUB_IN_FLIGHT)),
            "the cause is the backend's own answer"
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

        let lost = deliver(
            reservation,
            &enqueued,
            &[item("peer_a", "One", "01.flac")],
            &[],
            &backend,
            &running,
        )
        .await;

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
