//! Claiming files in the transfer registry, then enqueueing them.
//!
//! Every enqueue site claims its keys before it asks the backend for
//! anything, and releases each key whose file was not enqueued.

use shared::download::{
    DownloadProgress, DownloadableItem, EnqueueStatus, JobSlot, QueuedDownload,
};
use shared::failure::{Cause, Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::{Result as SoulResult, SoulseekError};
use soulbeet::DownloadBackend;
use std::sync::Arc;

use super::failover::{alternates_for, item_index};
use crate::core::acquire::Acquisition;
use crate::ledger::DuplicateRule;
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
/// Reason for a file of a batch that skips duplicates, which another batch of
/// the same user is already downloading into the same folder.
const TAKEN_BY_ANOTHER_DOWNLOAD: &str =
    "Another download of this user takes the same file into this folder";
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

/// Another batch of the same user is downloading the file into this folder
/// for a batch that skips duplicates, which must not hand its slot over to
/// that batch: the batch's own import would never run on it.
pub(crate) fn file_taken_by_another_download() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        TAKEN_BY_ANOTHER_DOWNLOAD,
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

/// A file the backend answered nothing for.
pub fn nothing_enqueued() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        NOTHING_ENQUEUED,
    )
}

/// Why none of `results` was enqueued: the first failed file's failure, else
/// `nothing_enqueued` for a backend that answered nothing for its files.
pub fn not_enqueued_failure(results: &[QueuedDownload]) -> Failure {
    results
        .iter()
        .find_map(|q| failed_reason(&q.status))
        .unwrap_or_else(nothing_enqueued)
}

/// Why `batch` could not claim its one key, when `claim` found it held:
/// by another user, by the same user into another folder, or by the same
/// user into this one.
pub fn held_failure(claim: &Claim, batch: &BatchRef) -> Failure {
    if !claim.other_user.is_empty() {
        other_user()
    } else if claim.mine[0].1.target != batch.target {
        another_folder(RetryAdvice::OtherSource)
    } else if matches!(
        batch.duplicates,
        DuplicateRule::Skip | DuplicateRule::Replace(_)
    ) {
        // An MCP job's batch does not join another one's download.
        file_taken_by_another_download()
    } else {
        already_running()
    }
}

/// Enqueues files `batch` has claimed into its folder, in the backend request
/// `request` (`DownloadBackend::new_request`). Every key whose file was not
/// enqueued is released. An answer for a file not asked for is left
/// out, and a transfer it enqueued is cancelled: no batch owns it, so it
/// would download with nobody to import it.
pub async fn enqueue_claimed(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    batch: &BatchRef,
    request: &str,
) -> SoulResult<Vec<QueuedDownload>> {
    let registry = TransferRegistry::global();
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let answered = match backend.download(items, &batch.destination, request).await {
        Ok(queued) => queued,
        Err(e) => {
            for key in &keys {
                registry.release(key, &batch.owner);
            }
            return Err(e);
        }
    };
    let (queued, stray): (Vec<_>, Vec<_>) = answered.into_iter().partition(|q| {
        keys.iter()
            .any(|k| k.peer == q.source && k.filename == q.item)
    });
    for q in stray {
        warn!(
            "{} answered for {} from {}, which was not asked for",
            backend.name(),
            q.item,
            q.source
        );
        if q.status == EnqueueStatus::Enqueued {
            if let Err(e) = backend.cancel_download(&q.source, &q.id, false).await {
                warn!("Failed to cancel unasked transfer {} in slskd: {}", q.id, e);
            }
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
    /// another folder: that batch shows their rows. AlreadyRunning marks a
    /// file one of the user's batches is already downloading into this
    /// folder, which was not enqueued again.
    pub results: Vec<QueuedDownload>,
    pub watcher: Watcher,
    /// The backend call itself failed: `results` lacks the files sent in it,
    /// and their keys are released.
    pub error: Option<SoulseekError>,
    /// Keys the same user is already downloading into another folder: that
    /// batch shows their rows, and they get no entry in `results`.
    pub elsewhere: Vec<TransferKey>,
}

impl RequestEnqueue {
    /// The request's failure when it did nothing because its files are
    /// already downloading into another folder.
    pub fn refusal(&self) -> Option<Failure> {
        let enqueued = self
            .results
            .iter()
            .any(|q| q.status == EnqueueStatus::Enqueued);
        (!self.elsewhere.is_empty() && !enqueued).then(|| another_folder(RetryAdvice::NotRetryable))
    }
}

/// A request's files as `claim_request` left them, nothing sent yet.
pub struct ClaimedRequest {
    batch: BatchRef,
    to_enqueue: Vec<DownloadableItem>,
    results: Vec<QueuedDownload>,
    watcher: Watcher,
    elsewhere: Vec<TransferKey>,
}

impl ClaimedRequest {
    /// The batch the request's free files go into.
    pub fn batch(&self) -> &BatchRef {
        &self.batch
    }

    /// Whether the request sends the backend any file.
    pub fn sends(&self) -> bool {
        !self.to_enqueue.is_empty()
    }

    /// Sends nothing: releases the keys of the files it would have sent, which
    /// get no entry in `results`.
    pub fn withdraw(self, backend: &Arc<dyn DownloadBackend>) -> RequestEnqueue {
        let registry = TransferRegistry::global();
        for item in &self.to_enqueue {
            registry.release(&item_key(backend.id(), item), &self.batch.owner);
        }
        RequestEnqueue {
            batch: self.batch,
            results: self.results,
            watcher: self.watcher,
            error: None,
            elsewhere: self.elsewhere,
        }
    }

    /// Enqueues the claimed files in the backend request `request`.
    pub async fn enqueue(
        self,
        backend: &Arc<dyn DownloadBackend>,
        request: &str,
    ) -> RequestEnqueue {
        let ClaimedRequest {
            batch,
            to_enqueue,
            mut results,
            watcher,
            elsewhere,
        } = self;
        let mut error = None;
        if !to_enqueue.is_empty() {
            match enqueue_claimed(backend, to_enqueue, &batch, request).await {
                Ok(queued) => results.extend(queued),
                Err(e) => error = Some(e),
            }
        }
        RequestEnqueue {
            batch,
            results,
            watcher,
            error,
            elsewhere,
        }
    }
}

/// Claims a request's files; `ClaimedRequest::enqueue` then enqueues the
/// free ones. When the user is already downloading some of them into the
/// same folder, the rest join that batch, so its monitor imports them
/// together, unless it has finished. Otherwise they go to `fresh`.
///
/// A request left with its own batch and nothing enqueued still owes that
/// batch a monitor when `batch.inbox.close_if_idle()` fails: another request
/// saw its claims and joined it meanwhile.
pub fn claim_request(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
) -> ClaimedRequest {
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let RequestClaim {
        claim,
        batch,
        reservation,
    } = TransferRegistry::global().claim_for_request(&keys, &fresh);
    sort_claim(keys, items, claim, batch, reservation)
}

/// Claims a request's files for `batch`, a running batch whose inbox
/// `reservation` holds open, as `claim_request` does but joining that batch
/// whatever the user's other batches hold. `ClaimedRequest::enqueue` then
/// enqueues the free ones into its folder, and they are delivered to its
/// monitor.
pub fn claim_into(
    backend: &Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    batch: BatchRef,
    reservation: InboxReservation,
) -> ClaimedRequest {
    let keys: Vec<TransferKey> = items.iter().map(|i| item_key(backend.id(), i)).collect();
    let claim = TransferRegistry::global().claim(&keys, &batch);
    sort_claim(keys, items, claim, batch, Some(reservation))
}

/// What a request does with each of `items` (by `keys`) after `claim`: the
/// free files go into `batch`, watched by its own monitor or, through
/// `reservation`, by the running batch's.
fn sort_claim(
    keys: Vec<TransferKey>,
    items: Vec<DownloadableItem>,
    claim: Claim,
    batch: BatchRef,
    reservation: Option<InboxReservation>,
) -> ClaimedRequest {
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
    // lets it import the shared ones here. `batch` imports into the folder
    // the request asked for, whichever batch it is. A file held here is
    // pursued by the batch holding it, unless `batch` skips duplicates.
    let (here, elsewhere): (Vec<_>, Vec<_>) = claim
        .mine
        .iter()
        .partition(|(_, running)| running.target == batch.target);
    let held = match batch.duplicates {
        DuplicateRule::Guarded => EnqueueStatus::AlreadyRunning,
        DuplicateRule::Skip | DuplicateRule::Replace(_) => {
            EnqueueStatus::Failed(file_taken_by_another_download())
        }
    };
    let results: Vec<QueuedDownload> = here
        .iter()
        .map(|(key, _)| not_enqueued(key, held.clone()))
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
    ClaimedRequest {
        batch,
        to_enqueue,
        results,
        watcher,
        elsewhere: elsewhere.into_iter().map(|(key, _)| key.clone()).collect(),
    }
}

/// Hands a joining request's enqueued transfers to `batch`'s monitor, each
/// as the slot of the job its file has among `acquisition`'s items, with the
/// alternates and source cap `acquisition` gives it. A transfer no picked file lines up with is no
/// slot of the job and is not handed over.
/// Returns the ones nobody will watch because that monitor stopped first,
/// with their keys released and their transfers cancelled, since they would
/// download into the batch's folder with nobody to import them.
pub async fn deliver(
    reservation: InboxReservation,
    enqueued: &[QueuedDownload],
    acquisition: &Acquisition,
    backend: &Arc<dyn DownloadBackend>,
    batch: &BatchRef,
) -> Vec<AddedSlot> {
    let Acquisition {
        job,
        items: picked,
        alternates: rest,
        max_sources,
    } = acquisition;
    let slots = enqueued
        .iter()
        .filter_map(|q| {
            let (slot, item) = &picked[item_index(q, picked.iter().map(|(_, item)| item))?];
            Some(AddedSlot {
                source: q.source.clone(),
                filename: q.item.clone(),
                transfer_id: q.id.clone(),
                alternates: alternates_for(item, rest),
                max_sources: *max_sources,
                job: JobSlot {
                    job: job.clone(),
                    slot: *slot,
                },
            })
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
            .with_job(slot.job.clone())
        })
        .collect()
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
    use crate::server_fns::download::test_support::{
        batch, group, item, key, same_user_into, Cancel, ScriptedOutcome, StubBackend,
        STUB_IN_FLIGHT,
    };
    use crate::transfer_registry::TransferOwner;
    use shared::failure::{FailureCode, RetryAdvice, Service, Stage};
    use soulbeet::ImportMode;
    use std::path::PathBuf;

    /// Another batch of `running`'s user, importing into the same folder.
    fn same_user(running: &BatchRef) -> BatchRef {
        same_user_into(running, running.target.clone())
    }

    /// Claims a request's files and enqueues the free ones.
    async fn enqueue_request(
        backend: &Arc<dyn DownloadBackend>,
        items: Vec<DownloadableItem>,
        fresh: BatchRef,
    ) -> RequestEnqueue {
        claim_request(backend, items, fresh)
            .enqueue(backend, &backend.new_request())
            .await
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
        assert_eq!(nothing, nothing_enqueued());
    }

    #[test]
    fn a_file_another_download_holds_fails_a_skip_batch_and_is_dismissed_for_a_guarded_one() {
        let running = batch("Come Over");
        let held = Claim {
            mine: vec![(transfer_key("slskd", "peer_a", "01.flac"), running.clone())],
            ..Claim::default()
        };
        let guarded = same_user(&running);
        assert_eq!(held_failure(&held, &guarded), already_running());
        let taken = held_failure(&held, &guarded.with_duplicates(DuplicateRule::Skip));
        assert_eq!(taken, file_taken_by_another_download());
        assert_eq!(
            (taken.code, taken.stage, taken.retry, taken.message.as_str()),
            (
                FailureCode::AlreadyRunning,
                Stage::Enqueue,
                RetryAdvice::OtherSource,
                "Another download of this user takes the same file into this folder"
            )
        );
    }

    #[tokio::test]
    async fn a_skip_request_fails_the_files_the_users_guarded_batch_holds() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = same_user(&running).with_duplicates(DuplicateRule::Skip);

        let enq = enqueue_request(
            &backend,
            vec![
                item("peer_a", "One", "01.flac"),
                item("peer_a", "Two", "02.flac"),
            ],
            fresh.clone(),
        )
        .await;

        let taken = enq.results.iter().find(|q| q.item == "01.flac").unwrap();
        assert_eq!(
            taken.status,
            EnqueueStatus::Failed(file_taken_by_another_download())
        );
        assert_eq!(
            statuses(&enq.results),
            vec![
                (
                    "01.flac".to_string(),
                    Answer::Failed(FailureCode::AlreadyRunning)
                ),
                ("02.flac".to_string(), Answer::Enqueued),
            ]
        );
        assert!(matches!(enq.watcher, Watcher::Own));
        assert_eq!(enq.batch.owner, fresh.owner);
        assert_eq!(stub.destinations(), vec![fresh.destination.clone()]);
    }

    #[tokio::test]
    async fn a_repeat_request_in_another_import_mode_starts_its_own_batch() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut running = batch("Come Over");
        running.import_mode = ImportMode::Album;
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = BatchRef::new(
            TransferOwner {
                user_id: running.owner.user_id.clone(),
                batch_id: uuid::Uuid::new_v4().to_string(),
            },
            "Come Over".into(),
            running.target.clone(),
            ImportMode::Singleton,
        );

        let enq = enqueue_request(
            &backend,
            vec![
                item("peer_a", "One", "01.flac"),
                item("peer_a", "Two", "02.flac"),
            ],
            fresh.clone(),
        )
        .await;

        assert_eq!(
            statuses(&enq.results),
            vec![
                ("01.flac".to_string(), Answer::AlreadyRunning),
                ("02.flac".to_string(), Answer::Enqueued),
            ]
        );
        assert!(matches!(enq.watcher, Watcher::Own));
        assert_eq!(enq.batch.owner, fresh.owner);
        assert_eq!(stub.destinations(), vec![fresh.destination.clone()]);
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

        let picked: Vec<(u32, DownloadableItem)> = (0..).zip(items.clone()).collect();
        let enq = enqueue_request(&backend, items, fresh.clone()).await;

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
        assert!(deliver(
            reservation,
            &enqueued,
            &Acquisition {
                job: "j1".into(),
                items: picked.clone(),
                alternates: rest.clone(),
                max_sources: MAX_SOURCES_PER_TRACK,
            },
            &backend,
            &running,
        )
        .await
        .is_empty());
        assert_eq!(
            running.inbox.take(),
            vec![AddedSlot {
                source: "peer_a".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02.flac".to_string(),
                alternates: vec![item("peer_b", "Two", "b/02.flac")],
                max_sources: MAX_SOURCES_PER_TRACK,
                job: JobSlot {
                    job: "j1".to_string(),
                    slot: 1,
                },
            }]
        );
    }

    #[tokio::test]
    async fn a_transfer_no_picked_file_lines_up_with_is_not_delivered() {
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let running = batch("Come Over");
        let reservation = running.inbox.reserve().expect("open inbox");
        // The backend answered for a file the request never named.
        let enqueued = vec![QueuedDownload {
            id: "t-02.flac".to_string(),
            source: "peer_a".to_string(),
            item: "02.flac".to_string(),
            size: 0,
            status: EnqueueStatus::Enqueued,
        }];
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Two", "b/02.flac")],
            0.8,
        )];

        let lost = deliver(
            reservation,
            &enqueued,
            &Acquisition {
                job: "j1".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: rest.clone(),
                max_sources: MAX_SOURCES_PER_TRACK,
            },
            &backend,
            &running,
        )
        .await;

        assert!(lost.is_empty());
        assert!(running.inbox.take().is_empty(), "it is no slot of the job");
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
        assert_eq!(
            enq.refusal(),
            Some(another_folder(RetryAdvice::NotRetryable))
        );
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
    async fn an_answer_for_a_file_not_asked_for_is_left_out_and_cancelled() {
        let stub = StubBackend::new(vec![ScriptedOutcome::EnqueuedAs("other.flac")]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();

        let enq =
            enqueue_request(&backend, vec![item("peer_a", "One", "01.flac")], batch("X")).await;

        assert!(enq.results.is_empty(), "{:?}", statuses(&enq.results));
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-other.flac")]
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
            &Acquisition {
                job: "j1".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
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
        enqueue_claimed(
            backend,
            vec![item("peer_a", "One", "01.flac")],
            running,
            &backend.new_request(),
        )
        .await
        .expect("enqueued")
    }
}
