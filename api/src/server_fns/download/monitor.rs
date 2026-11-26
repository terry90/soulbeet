//! Download monitoring logic for tracking slskd download progress.
//!
//! This module encapsulates the polling loop that monitors downloads from slskd,
//! handles per-track timeouts, and hands finished files to the import queue.
//! Every slot is found in slskd's list by its transfer id, and a transfer's
//! record is only removed once its file was found on disk: removing it
//! earlier can cancel slskd between persisting the transfer as finished and
//! moving the file, which strands the file in slskd's incomplete folder.

use dioxus::logger::tracing::{debug, info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState, JobSlot, QueuedDownload};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::{BackendError, DownloadBackend, ImportMode};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::enqueue::{stopped_rows, transfer_key};
use super::failover::{Sent, SourcePool, MAX_SOURCES_PER_TRACK};
use super::process::{
    album_jobs, import_job, resolve_on_disk, unresolved_row, Downloaded, ResolvedSlot,
};
use super::utils::{file_id, FileId};
use crate::config::CONFIG;
use crate::globals::send_user_event;
use crate::import_queue::IMPORT_QUEUE;
use crate::ledger::{DuplicateRule, JobEvent, JobState, Ledger};
use crate::services::{BackendLookupError, Backends};
use crate::transfer_registry::{AddedSlot, BatchRef, TransferKey, TransferOwner, TransferRegistry};

/// Poll interval for checking download status (2 seconds).
const POLL_INTERVAL_SECS: u64 = 2;

/// Grace period for downloads to appear in slskd (30 seconds = 15 * 2s intervals).
const MAX_CONSECUTIVE_EMPTY: usize = 15;

/// How long a watched transfer may go without progress before the slot
/// moves to another source (1 hour). Progress is bytes moved since the last
/// poll, and waiting in its source's queue while that source sends this
/// slskd another file: a peer sends one file at a time, so the last tracks
/// of an album wait there for as long as the ones before them take, and a
/// slow peer that keeps sending is never cut off.
const PER_TRACK_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How long a transfer may wait in its source's own queue before the slot
/// moves to another source. A transfer waiting in slskd's queue is not held
/// to it: slskd's download slots free up on their own.
const REMOTE_QUEUE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a track may stay absent from slskd's transfer list before it is
/// marked failed: never appearing at all, or vanishing after being seen.
/// Without this, one absent track keeps the whole batch unfinished forever.
const ABSENT_TRACK_TIMEOUT: Duration = Duration::from_secs(120);

/// Consecutive backend resolution failures tolerated before giving up.
const MAX_BACKEND_FAILURES: u32 = 5;

/// Consecutive unparseable download listings tolerated before failing the
/// batch. Scoped to schema drift (`BackendError::answer_unreadable`): slskd
/// answered but its response no longer parses, which will not fix itself,
/// so the batch must fail with that error instead of "Download never
/// appeared in slskd". Transport errors are excluded: slskd being briefly
/// unreachable (restart, upgrade) resolves on its own, and the shared
/// circuit breaker can keep rejecting requests for up to a minute after
/// recovery, so a bounded fuse on those would fail healthy batches.
const MAX_INVALID_RESPONSES: u32 = 15;

/// How long the monitor may go without a readable transfer list before it
/// gives up. Transport errors are deliberately not fused by count (see
/// above), but they must still be fused by time: every other deadline in
/// this loop is evaluated from a successful poll, so a monitor whose
/// requests keep timing out never re-checks any of them and polls forever,
/// holding its task and channel alive for the life of the process (#78).
///
/// Ten minutes clears the circuit breaker's recovery window by an order of
/// magnitude, so a slskd restart or upgrade still resolves on its own.
const BACKEND_STALL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a failed transfer state must persist before it is treated as
/// final. A failed attempt slskd will retry reads as queued for
/// `RETRY_PENDING_FOR` (`read_pending_retry`); this window covers a failed
/// sighting the backend replaces anyway, such as a slskd retry beyond the
/// limit the client read before slskd's limit was raised. Acting on a single
/// sighting would abandon that transfer.
///
/// Shared with discovery's poll loop, which waits on the same transfers and
/// so must judge them by the same rule.
pub(crate) const FAILED_STATE_CONFIRM: Duration = Duration::from_secs(4);

/// How long a failed attempt slskd says it will retry may keep reading as
/// queued. slskd writes the retry's `Queued, Locally` in `onRetry`, before
/// the backoff delay (`Common/Retry.cs:118-119`,
/// `DownloadService.cs:1426-1428`), so a retry shows the failure only for
/// the database writes in between. A download cancelled during that delay
/// keeps the failure for good: `Task.Delay` throws, `Retry.Do` does not
/// retry a cancel and throws, and `TryFail` finds the record `Queued,
/// Locally` and writes `Completed, Errored` (`DownloadService.cs:1045-1068`,
/// called again from the download task's continuation at `:596`), keeping
/// the earlier attempt's exception, end time and the raised attempt count.
/// Nothing tells that record from a retry about to start but how long it
/// stays.
///
/// Counted from this watcher's first sighting of the record pending, on
/// Soulful's monotonic clock: `endedAt` is slskd's host clock, and a
/// difference between the two hosts would move the bound.
pub(crate) const RETRY_PENDING_FOR: Duration = Duration::from_secs(30);

/// `dl` as a watcher that has held it pending since `pending_since` reads
/// it: a pending retry stays queued for `RETRY_PENDING_FOR`, then reads as
/// the failure its attempt stated. Starts the clock on the first sighting
/// and clears it for a row that is not pending.
pub(crate) fn read_pending_retry(
    dl: &DownloadProgress,
    pending_since: &mut Option<Instant>,
) -> DownloadProgress {
    let Some(failure) = &dl.pending_retry else {
        *pending_since = None;
        return dl.clone();
    };
    if pending_since.get_or_insert_with(Instant::now).elapsed() < RETRY_PENDING_FOR {
        return dl.clone();
    }
    DownloadProgress {
        pending_retry: None,
        ..dl.clone()
    }
    .with_state(DownloadState::Failed(String::new()))
    .with_failure(failure.clone())
}

fn transfer_failure(code: FailureCode, retry: RetryAdvice, message: &str) -> Failure {
    Failure::new(code, Stage::Transfer, retry, message)
}

/// A transfer the backend whose texts name `service` never listed.
fn never_appeared(service: &str) -> Failure {
    transfer_failure(
        FailureCode::TransferNeverAppeared,
        RetryAdvice::OtherSource,
        &format!("Download never appeared in {service}"),
    )
}

/// A transfer the backend whose texts name `service` listed and then dropped.
fn disappeared(service: &str) -> Failure {
    transfer_failure(
        FailureCode::TransferDisappeared,
        RetryAdvice::SameSourceLater,
        &format!("Download disappeared from {service}"),
    )
}

/// A slot that went `PER_TRACK_TIMEOUT` without progress with no peer left.
fn per_track_timeout() -> Failure {
    transfer_failure(
        FailureCode::TransferTimedOut,
        RetryAdvice::OtherSource,
        "Per-track timeout",
    )
}

/// A slot that waited past `REMOTE_QUEUE_TIMEOUT` in its source's queue
/// with no other source left.
fn stuck_in_remote_queue() -> Failure {
    transfer_failure(
        FailureCode::TransferStuckInRemoteQueue,
        RetryAdvice::OtherSource,
        &format!(
            "Waited {} minutes in the source's queue",
            REMOTE_QUEUE_TIMEOUT.as_secs() / 60
        ),
    )
}

/// A transfer its user cancelled.
pub(crate) fn cancelled_on_request() -> Failure {
    transfer_failure(
        FailureCode::TransferCancelled,
        RetryAdvice::NotRetryable,
        "Cancelled on request",
    )
}

/// The transfer list of the backend whose texts name `service` stayed
/// unreadable for `BACKEND_STALL_TIMEOUT`, the last read failing with `e`. Its
/// code, advice and cause are `e`'s: a refused API key needs a person, an
/// unreachable slskd needs time.
fn stalled(e: &BackendError, service: &str) -> Failure {
    e.failure(
        Stage::Transfer,
        format!(
            "{service}'s transfer list was unreadable for {} minutes",
            BACKEND_STALL_TIMEOUT.as_secs() / 60
        ),
    )
}

/// No download backend was available for `MAX_BACKEND_FAILURES` polls, the
/// last lookup failing with `e`, whose failure this is. A backend an admin
/// turned off says so.
fn backend_unavailable(e: &BackendLookupError) -> Failure {
    match e {
        BackendLookupError::Disabled(_) => e.failure(Stage::Transfer, e.to_string()),
        _ => e.failure(Stage::Transfer, "Download backend unavailable"),
    }
}

/// The downloads list of the backend whose texts name `service` kept failing to parse.
fn list_unreadable(e: &BackendError, service: &str) -> Failure {
    e.failure(
        Stage::Transfer,
        format!("Could not read {service} downloads list: {e}"),
    )
}

/// Where a slot is in its life. Only a `Watching` slot is looked at, shown
/// or rebound: the others belong to the import queue or are finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    /// Its transfer is being watched.
    Watching,
    /// Its file went to the import queue, which shows its row from then on.
    HandedOff,
    /// It ran past the per-track timeout or the remote queue limit with no
    /// source left, and its transfer was cancelled.
    TimedOut,
    /// It ended without a file: failed, absent, or not found on disk.
    Settled,
}

/// State tracking for individual track downloads.
struct TrackState {
    /// When this slot was bound to its current transfer. Reset on every
    /// rebind, so the never-appeared window measures the age of THIS
    /// binding. A batch-wide clock would declare a slot that just failed over
    /// absent on the same poll, because the alternate's transfer cannot
    /// appear in a transfer list that was fetched before it was enqueued.
    bound_at: Instant,
    /// Whether slskd's download list showed the track's transfer since it
    /// was bound.
    seen: bool,
    /// The bytes the transfer had moved on the last poll that listed it.
    transferred: u64,
    /// Since when the transfer has made no progress (`PER_TRACK_TIMEOUT`).
    /// Cleared on any poll that shows it moved bytes or waiting behind a
    /// file its source sends this slskd (`peers_sending`).
    stalled_since: Option<Instant>,
    /// When the track went missing from slskd's list after being seen.
    missing_since: Option<Instant>,
    /// When the track's transfer was first seen in a failed terminal state.
    failing_since: Option<Instant>,
    /// When the track's transfer was first seen waiting in its source's
    /// queue while that source sent this slskd nothing. Cleared on any poll
    /// that does not show it waiting there, and on any poll that shows its
    /// source sending this slskd any file (`peers_sending`).
    queued_at_source_since: Option<Instant>,
    /// When the track's transfer was first seen as a failed attempt slskd
    /// will retry (`read_pending_retry`).
    retry_pending_since: Option<Instant>,
    phase: SlotPhase,
}

impl TrackState {
    /// Fresh state for a slot bound to a transfer right now. Every
    /// construction site is a binding or a rebinding, so there is no
    /// meaningful `Default`.
    fn bound_now() -> Self {
        Self {
            bound_at: Instant::now(),
            seen: false,
            transferred: 0,
            stalled_since: None,
            missing_since: None,
            failing_since: None,
            queued_at_source_since: None,
            retry_pending_since: None,
            phase: SlotPhase::Watching,
        }
    }
}

/// A transfer a monitor starts out watching.
pub struct WatchedTransfer {
    pub source: String,
    pub filename: String,
    pub transfer_id: String,
    /// The job slot it downloads for.
    pub job: JobSlot,
    /// An earlier version adopted it from a transfer the batch did not
    /// enqueue, storing its attempt without a candidate: slskd saved it
    /// where its own enqueue said.
    pub adopted: bool,
    /// Every alternate the slot's failover sent before the monitor took it.
    pub sent: Vec<Sent>,
}

impl WatchedTransfer {
    /// The transfer `queued` names, enqueued into the batch's folder for `job`.
    pub fn enqueued(queued: &QueuedDownload, job: JobSlot) -> Self {
        Self {
            source: queued.source.clone(),
            filename: queued.item.clone(),
            transfer_id: queued.id.clone(),
            job,
            adopted: false,
            sent: Vec::new(),
        }
    }
}

/// The transfer a slot currently watches.
#[derive(Clone, Debug)]
struct TrackedFile {
    source: String,
    filename: String,
    transfer_id: String,
    /// The batch folder the transfer was enqueued into, or `None` for one an
    /// earlier version adopted (`WatchedTransfer::adopted`), which slskd
    /// saves wherever its own enqueue said.
    destination: Option<String>,
    /// The job slot the slot downloads for, kept through every rebind.
    job: JobSlot,
    /// Every alternate the slot's failover sent, kept through every rebind:
    /// the only transfers the slot may adopt (`adoptable`).
    sent: Vec<Sent>,
}

/// Monitors download progress from slskd and hands finished files to the
/// import queue.
pub struct DownloadMonitor {
    /// The transfer each slot watches, by slot index.
    tracked_files: Vec<TrackedFile>,
    /// Target directory for imports.
    target_path: PathBuf,
    /// Broadcast sender for UI updates.
    tx: broadcast::Sender<DownloadEvent>,
    /// Per-track state, parallel to `tracked_files` by index.
    track_states: Vec<TrackState>,
    /// Alternate sources per slot, parallel to `tracked_files` by index.
    pool: SourcePool,
    /// The batch this monitor watches: its rows, its folder under the
    /// download directory, and the inbox joining requests deliver to.
    batch: BatchRef,
    /// slskd's download directory as this server sees it.
    download_base: PathBuf,
    /// Files already handed to the import queue.
    claimed: Arc<Mutex<HashSet<FileId>>>,
    /// `(peer, transfer id)` of transfers this batch stopped watching: a
    /// cancel only soft-removes a running transfer, so their records are
    /// removed at the end.
    abandoned: Vec<(String, String)>,
    /// Singleton handoffs still resolving or importing.
    handoffs: Vec<JoinHandle<()>>,
    /// When slskd's transfer list was last read successfully. Every other
    /// deadline is evaluated from a readable list, so this one bounds the
    /// case where no list is readable at all.
    last_readable_poll: Instant,
    /// `(peer, transfer id)` of every transfer the last listing showed
    /// Completed, so only its first sighting counts as its peer sending this
    /// slskd a file.
    completed_seen: HashSet<(String, String)>,
    /// Where every row the monitor sends is recorded with its slot's state.
    ledger: Ledger,
    /// Finished files of an album batch found on disk before it was watched
    /// (a resumed batch's): imported with the batch's album once every slot
    /// settled, or when it stops early (`stop_early`).
    found: Vec<ResolvedSlot>,
    /// Each slot's transfer as the last readable listing showed it, paired
    /// with the slot: what a batch that stops early knows finished.
    last_listed: Vec<(usize, DownloadProgress)>,
}

impl DownloadMonitor {
    /// The service the batch's failure texts name.
    fn service(&self) -> &str {
        shared::backends::service(&self.batch.backend)
    }

    /// Create a new download monitor. The transfers must have been enqueued
    /// into `batch.destination`. Dropping the monitor closes the batch's
    /// inbox and releases every registry key the batch holds. Every row it
    /// sends is recorded in `ledger`.
    pub fn new(
        transfers: Vec<WatchedTransfer>,
        target_path: PathBuf,
        tx: broadcast::Sender<DownloadEvent>,
        batch: BatchRef,
        ledger: Ledger,
    ) -> Self {
        let tracked_files: Vec<TrackedFile> = transfers
            .into_iter()
            .map(|t| TrackedFile {
                source: t.source,
                filename: t.filename,
                transfer_id: t.transfer_id,
                destination: (!t.adopted).then(|| batch.destination.clone()),
                job: t.job,
                sent: t.sent,
            })
            .collect();

        let track_states = (0..tracked_files.len())
            .map(|_| TrackState::bound_now())
            .collect();
        let pool =
            SourcePool::from_tracks(&vec![None; tracked_files.len()], &[], MAX_SOURCES_PER_TRACK);

        Self {
            tracked_files,
            target_path,
            tx,
            track_states,
            pool,
            batch,
            download_base: CONFIG.download_path().clone(),
            claimed: Arc::new(Mutex::new(HashSet::new())),
            abandoned: Vec::new(),
            handoffs: Vec::new(),
            last_readable_poll: Instant::now(),
            completed_seen: HashSet::new(),
            ledger,
            found: Vec::new(),
            last_listed: Vec::new(),
        }
    }

    /// Hands an album batch `found`, finished files of it already on disk:
    /// they are imported with the files it watches once every one of those
    /// settled, or with the ones that finished when the monitor stops before
    /// that, and are never resolved again for another slot.
    pub fn with_found(mut self, found: Vec<ResolvedSlot>) -> Self {
        let mut claimed = self.claimed.lock().unwrap_or_else(|e| e.into_inner());
        claimed.extend(found.iter().filter_map(|f| file_id(&f.path)));
        drop(claimed);
        self.found = found;
        self
    }

    fn watching(&self, slot: usize) -> bool {
        self.track_states[slot].phase == SlotPhase::Watching
    }

    /// Whether no slot is left watching a transfer.
    fn watches_nothing(&self) -> bool {
        (0..self.tracked_files.len()).all(|slot| !self.watching(slot))
    }

    /// Whether `dl` is the transfer `slot` watches now. A slot rebound during
    /// a poll no longer owns the entry that poll matched it with.
    fn is_current(&self, slot: usize, dl: &DownloadProgress) -> bool {
        let tracked = &self.tracked_files[slot];
        is_transfer(dl, &tracked.source, &tracked.transfer_id)
    }

    /// The registry key of the transfer `slot` watches.
    fn slot_key(&self, slot: usize, backend: &'static str) -> TransferKey {
        let tracked = &self.tracked_files[slot];
        transfer_key(backend, &tracked.source, &tracked.filename)
    }

    /// Ends `slot` without a file and frees its transfer for anyone.
    /// Returns whether its user had asked to cancel it.
    fn settle(&mut self, slot: usize, phase: SlotPhase, backend: &'static str) -> bool {
        self.track_states[slot].phase = phase;
        TransferRegistry::global().release(&self.slot_key(slot, backend), &self.batch.owner)
    }

    /// Whether `slot`'s job is marked cancelled (D9): its user cancelled the
    /// job, wherever failover moved the slot.
    fn job_cancelled(&self, slot: usize) -> bool {
        TransferRegistry::global().job_cancelled(&self.tracked_files[slot].job.job)
    }

    /// Ends `slot` without a file as `settle` does, and gives the row it ends
    /// with: `row`, or Cancelled when its user cancelled it, or its job, after
    /// this poll's `apply_cancels` ran.
    fn write_off(
        &mut self,
        slot: usize,
        phase: SlotPhase,
        backend: &'static str,
        row: DownloadProgress,
    ) -> DownloadProgress {
        if self.settle(slot, phase, backend) || self.job_cancelled(slot) {
            self.cancelled_row(slot)
        } else {
            row
        }
    }

    /// The row of `slot` its user cancelled.
    fn cancelled_row(&self, slot: usize) -> DownloadProgress {
        let tracked = &self.tracked_files[slot];
        DownloadProgress::queued(
            tracked.transfer_id.clone(),
            tracked.source.clone(),
            tracked.filename.clone(),
            0,
        )
        .with_state(DownloadState::Cancelled)
        .with_failure(cancelled_on_request())
    }

    /// The peers this batch is currently waiting on, deduplicated. Follows
    /// failover: a rebound slot names its new peer.
    fn sources(&self) -> Vec<String> {
        let mut sources: Vec<String> = Vec::new();
        for file in &self.tracked_files {
            if !sources.contains(&file.source) {
                sources.push(file.source.clone());
            }
        }
        sources
    }

    /// The peers this batch is not waiting on that a watched slot with no
    /// usable transfer in `downloads` sent an alternate to. The transfer
    /// such a send made is the only one the slot may adopt (`adoptable`),
    /// and only a listing of its peer shows it.
    fn adoption_peers(&self, downloads: &[DownloadProgress]) -> Vec<String> {
        let waiting_on = self.sources();
        let mut peers: Vec<String> = Vec::new();
        for slot in (0..self.tracked_files.len()).filter(|slot| self.watching(*slot)) {
            let tracked = &self.tracked_files[slot];
            if transfer_by_id(downloads, &tracked.source, &tracked.transfer_id)
                .is_some_and(|dl| !is_failed(&dl.state))
            {
                continue;
            }
            for sent in &tracked.sent {
                let peer = &sent.candidate.source;
                if !waiting_on.contains(peer) && !peers.contains(peer) {
                    peers.push(peer.clone());
                }
            }
        }
        peers
    }

    /// Read each watched slot's own row in `downloads` as `read_pending_retry`
    /// does, on that slot's clock.
    fn read_pending_retries(&mut self, downloads: &mut [DownloadProgress]) {
        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot) {
                continue;
            }
            let tracked = &self.tracked_files[slot];
            if let Some(dl) = downloads
                .iter_mut()
                .find(|dl| is_transfer(dl, &tracked.source, &tracked.transfer_id))
            {
                *dl = read_pending_retry(dl, &mut self.track_states[slot].retry_pending_since);
            }
        }
    }

    /// Watch the transfers joining requests delivered since the last poll.
    /// Returns whether there were any.
    fn drain_inbox(&mut self) -> bool {
        let added = self.batch.inbox.take();
        let any = !added.is_empty();
        for slot in added {
            info!(
                "Batch {} now also watches {} from {}",
                self.batch.owner.batch_id, slot.filename, slot.source
            );
            self.tracked_files.push(TrackedFile {
                source: slot.source,
                filename: slot.filename,
                transfer_id: slot.transfer_id,
                destination: Some(self.batch.destination.clone()),
                job: slot.job,
                sent: slot.sent,
            });
            self.track_states.push(TrackState::bound_now());
            self.pool
                .push_slot(slot.alternates, slot.used, slot.max_sources);
        }
        any
    }

    /// Stop taking slots, and write off the ones delivered but never taken.
    /// Returns those. Idempotent.
    fn close_inbox(&self) -> Vec<AddedSlot> {
        let unwatched = self.batch.inbox.close();
        if !unwatched.is_empty() {
            warn!(
                "Batch {} stopped before watching {} added downloads",
                self.batch.owner.batch_id,
                unwatched.len()
            );
            self.report(JobState::Failed, stopped_rows(&unwatched, &self.batch));
        }
        unwatched
    }

    /// `close_inbox`, then cancel the transfers it wrote off: left running
    /// they would download into this folder with nobody to import them.
    /// End-of-run cleanup removes their records like the abandoned ones.
    async fn close_inbox_cancelling(&mut self, backend: &Arc<dyn DownloadBackend>) {
        for slot in self.close_inbox() {
            if let Err(e) = backend
                .cancel_download(&slot.source, &slot.transfer_id, false)
                .await
            {
                warn!(
                    "Failed to cancel unwatched transfer {} in {}: {}",
                    slot.transfer_id,
                    backend.id(),
                    e
                );
            }
            self.abandoned.push((slot.source, slot.transfer_id));
        }
    }

    /// Count a failed read of slskd's transfer list. Returns the failure the
    /// batch must stop with (`stop_early`), or None to keep polling.
    ///
    /// An unparseable list is fused by count (`MAX_INVALID_RESPONSES`).
    /// Every error is also fused by time: transport errors are deliberately
    /// not fused by count, because a slskd restart resolves on its own, but
    /// while no list can be read no per-track deadline is ever evaluated and
    /// the monitor would poll for the life of the process (#78). The batch
    /// fails with the error that ended it.
    fn poll_failed(&self, e: BackendError, invalid_responses: &mut u32) -> Option<Failure> {
        if e.answer_unreadable() {
            // Schema drift: slskd answered but its downloads list no
            // longer parses. That will not fix itself, so fail the
            // batch with the real error instead of polling forever
            // (pre-fix this surfaced as the misleading "Download
            // never appeared in slskd", issue #73).
            *invalid_responses += 1;
            warn!(
                "Unparseable {} downloads list ({}/{}): {}",
                self.batch.backend, invalid_responses, MAX_INVALID_RESPONSES, e
            );
            if *invalid_responses >= MAX_INVALID_RESPONSES {
                return Some(list_unreadable(&e, self.service()));
            }
        } else {
            warn!(
                "Error fetching download status from {}: {}",
                self.batch.backend, e
            );
        }
        if self.last_readable_poll.elapsed() <= BACKEND_STALL_TIMEOUT {
            return None;
        }
        let failure = stalled(&e, self.service());
        warn!(
            "{} - giving up on batch {:?}",
            failure.message,
            self.filenames()
        );
        Some(failure)
    }

    /// Attach alternate sources so failed transfers retry from another peer.
    /// `pool` has a slot per transfer the monitor was created with.
    pub fn with_failover(mut self, pool: SourcePool) -> Self {
        debug_assert_eq!(pool.slot_count(), self.tracked_files.len());
        self.pool = pool;
        self
    }

    /// Re-enqueue this slot from its next-best peer. Returns true when the
    /// slot was rebound, in which case the caller must not mark it failed.
    /// `left` is the row the slot's transfer ended with, its failure set:
    /// before an alternate is tried it ends that attempt in the ledger, the
    /// slot still downloading. With no alternate to try, the caller's settled
    /// row ends it.
    ///
    /// A cancel marked before this poll's `apply_cancels` has settled the
    /// slot already. One marked later in the poll, the next peer's enqueue
    /// included, moves to the new transfer with the slot, and the next
    /// poll's `apply_cancels` stops that transfer. A slot whose job is
    /// marked cancelled tries no other source.
    async fn try_failover(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        left: DownloadProgress,
    ) -> bool {
        if !self.pool.has_next(slot) || self.job_cancelled(slot) {
            return false;
        }
        let reason = left
            .failure
            .as_ref()
            .map_or("Transfer failed", |f| f.message.as_str())
            .to_string();
        // When every source is refused, the slot ends with `left`, whose
        // attempt is older than the refused ones: a reader finds a row's
        // attempt by its source and file, never by its place.
        let job = self.tracked_files[slot].job.clone();
        let left = self.stamp(slot, left);
        let (next, mut sends) = self
            .pool
            .fail_over(slot, backend, &self.batch, &self.ledger, &job, left)
            .await;
        self.tracked_files[slot].sent.append(&mut sends);
        let Some((_, queued)) = next.queued else {
            return false;
        };

        let attempt = self.pool.attempts_used(slot);
        info!(
            "{} for {} from {}, retrying from {} ({}/{})",
            reason,
            self.tracked_files[slot].filename,
            self.tracked_files[slot].source,
            queued.source,
            attempt,
            self.pool.max_sources(slot)
        );

        self.abandon(slot, backend).await;
        let old_key = self.slot_key(slot, backend.id());
        let new_key = transfer_key(backend.id(), &queued.source, &queued.item);
        TransferRegistry::global().hand_over(&old_key, &new_key, &self.batch.owner);

        // The new peer serves this track under a completely different remote
        // path and transfer id, so the slot must track both outright or the
        // monitor loses sight of it.
        let job = self.tracked_files[slot].job.clone();
        let sent = std::mem::take(&mut self.tracked_files[slot].sent);
        let previous_item = std::mem::replace(
            &mut self.tracked_files[slot],
            TrackedFile {
                source: queued.source.clone(),
                filename: queued.item.clone(),
                transfer_id: queued.id.clone(),
                destination: Some(self.batch.destination.clone()),
                job,
                sent,
            },
        )
        .filename;
        self.track_states[slot] = TrackState::bound_now();

        let entry = DownloadProgress::queued(queued.id, queued.source, queued.item, queued.size);
        self.replace_row(previous_item, self.stamp(slot, entry));
        true
    }

    /// Records that `slot` now downloads the transfer `sent` made, as an
    /// attempt of its own: the answer that refused the send ended the one
    /// stored before it, and a restart finds the slot's transfer by its
    /// newest attempt.
    fn record_adoption(&self, slot: usize, sent: Sent) {
        self.ledger.record(JobEvent::Attempt {
            slot: self.tracked_files[slot].job.clone(),
            candidate: Some(Box::new(sent.candidate)),
            request: Some(sent.request),
        });
    }

    /// Shows `entry`, the stamped row of a slot rebound to another transfer,
    /// in place of `previous_item`'s row, and records it as downloading. One
    /// slot is still one download, so the panel replaces the abandoned
    /// peer's row instead of gaining a second one for the same track.
    fn replace_row(&self, previous_item: String, entry: DownloadProgress) {
        let _ = self.tx.send(DownloadEvent::Replaced {
            previous_item,
            entry: Box::new(entry.clone()),
        });
        self.ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![entry],
        });
    }

    /// Stop the transfer a slot is about to stop watching (`cancel_abandoned`),
    /// and remember it so its record is removed at the end.
    ///
    /// At the per-track timeout that transfer is still running, so leaving it
    /// alone would have two peers sending the same track, the abandoned one
    /// into this batch's folder. Even a dead transfer has to go: once no slot
    /// points at it nothing else prunes it. A cancel that fails is not worth
    /// losing the retry over.
    async fn abandon(&mut self, slot: usize, backend: &Arc<dyn DownloadBackend>) {
        let tracked = &self.tracked_files[slot];
        cancel_abandoned(backend, &tracked.source, &tracked.transfer_id).await;
        self.abandoned
            .push((tracked.source.clone(), tracked.transfer_id.clone()));
    }

    /// Run the monitoring loop until all downloads complete or timeout. Every
    /// backend call goes to the batch's own backend, looked up in `backends`.
    pub async fn run(&mut self, backends: &dyn Backends) {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        let mut consecutive_empty = 0;
        let mut poll_count = 0;
        let mut backend_failures: u32 = 0;
        let mut invalid_responses: u32 = 0;
        // The last backend looked up, which holds the records of the files a
        // batch that stops early still imports.
        let mut last_backend: Option<Arc<dyn DownloadBackend>> = None;
        self.last_readable_poll = Instant::now();

        // Poll immediately on first iteration
        interval.tick().await;

        loop {
            if self.drain_inbox() {
                // The new transfers get the whole window to show up.
                consecutive_empty = 0;
            }

            poll_count += 1;

            let backend = match backends.get(&self.batch.backend).await {
                Ok(b) => {
                    backend_failures = 0;
                    last_backend = Some(Arc::clone(&b));
                    b
                }
                Err(e) => {
                    backend_failures += 1;
                    warn!(
                        "No download backend available for monitoring ({}/{}): {}",
                        backend_failures, MAX_BACKEND_FAILURES, e
                    );
                    if backend_failures >= MAX_BACKEND_FAILURES {
                        self.stop_early(backend_unavailable(&e), last_backend.as_ref())
                            .await;
                        break;
                    }
                    interval.tick().await;
                    continue;
                }
            };
            // With no slot to watch, no listing can tell more: a resumed
            // batch whose files were all on disk, or whose files sent again
            // were all refused, finishes once nothing more is on its way.
            if self.watches_nothing() && self.check_completion(&[], &backend).await {
                break;
            }
            match backend.get_downloads_for(&self.sources()).await {
                Ok(mut downloads) => {
                    invalid_responses = 0;
                    self.last_readable_poll = Instant::now();
                    self.read_pending_retries(&mut downloads);

                    // A slot with nothing usable may adopt what its failover
                    // sent to a peer the batch no longer waits on. No slot's
                    // own transfer is from those peers, so their rows need
                    // no pending-retry reading.
                    let peers = self.adoption_peers(&downloads);
                    if !peers.is_empty() {
                        match backend.get_downloads_for(&peers).await {
                            Ok(more) => downloads.extend(more),
                            Err(e) => {
                                debug!("Could not list the peers a slot may adopt from: {}", e)
                            }
                        }
                    }

                    let should_break = self
                        .process_poll_result(
                            downloads,
                            &mut consecutive_empty,
                            poll_count,
                            &backend,
                        )
                        .await;
                    if should_break {
                        break;
                    }
                }
                Err(e) => {
                    if let Some(failure) = self.poll_failed(e, &mut invalid_responses) {
                        self.stop_early(failure, Some(&backend)).await;
                        break;
                    }
                }
            }

            interval.tick().await;
        }

        // Joiners must not enqueue into a batch that is only cleaning up.
        match backends.get(&self.batch.backend).await {
            Ok(backend) => {
                self.close_inbox_cancelling(&backend).await;
                self.remove_batch_transfers(&backend).await;
            }
            Err(_) => {
                self.close_inbox();
            }
        }

        // Their keys stay claimed until their imports are done, and the
        // batch's keys are released when the monitor drops.
        self.join_handoffs().await;

        info!(
            "Download monitoring task completed for user: {}",
            self.batch.owner.user_id
        );
    }

    /// Wait for every singleton handoff to finish its import.
    async fn join_handoffs(&mut self) {
        for handoff in std::mem::take(&mut self.handoffs) {
            if let Err(e) = handoff.await {
                warn!(
                    "An import handoff of batch {} stopped: {}",
                    self.batch.owner.batch_id, e
                );
            }
        }
    }

    /// Remove the records of this batch's transfers, watched or abandoned,
    /// that ended without a file (`remove_ended_transfers`). A succeeded
    /// record is only ever removed by the handoff or album finish that found
    /// its file on disk.
    async fn remove_batch_transfers(&self, backend: &Arc<dyn DownloadBackend>) {
        let transfers: Vec<(String, String)> = (self.tracked_files.iter())
            .map(|t| (t.source.clone(), t.transfer_id.clone()))
            .chain(self.abandoned.iter().cloned())
            .collect();
        remove_ended_transfers(backend, &transfers).await;
    }

    /// Stop every watched slot whose transfer, or job, its user cancelled,
    /// and show it Cancelled. A slot whose own transfer already completed is
    /// left alone, so its file is still imported.
    async fn apply_cancels(
        &mut self,
        downloads: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let registry = TransferRegistry::global();
        let mut cancelled = Vec::new();
        for slot in 0..self.tracked_files.len() {
            let marked = self.job_cancelled(slot)
                || registry.is_cancelled(&self.slot_key(slot, backend.id()), &self.batch.owner);
            if !self.watching(slot) || !marked {
                continue;
            }
            let tracked = &self.tracked_files[slot];
            if transfer_by_id(downloads, &tracked.source, &tracked.transfer_id)
                .is_some_and(|dl| is_completed(&dl.state))
            {
                continue;
            }
            info!(
                "Cancelling {} from {} on request",
                tracked.filename, tracked.source
            );
            let row = self.cancelled_row(slot);
            self.abandon(slot, backend).await;
            self.settle(slot, SlotPhase::Settled, backend.id());
            cancelled.push(self.stamp(slot, row));
        }
        self.report(JobState::Cancelled, cancelled);
    }

    /// Process a poll result from slskd.
    /// Returns true if monitoring should stop.
    async fn process_poll_result(
        &mut self,
        downloads: Vec<DownloadProgress>,
        consecutive_empty: &mut usize,
        poll_count: u32,
        backend: &Arc<dyn DownloadBackend>,
    ) -> bool {
        self.apply_cancels(&downloads, backend).await;

        // Debug logging for first few polls
        if poll_count <= 3 {
            debug!("Looking for filenames: {:?}", self.filenames());
            let listed: Vec<_> = downloads.iter().map(|f| &f.item).collect();
            debug!(
                "{} returned {} downloads: {:?}",
                self.batch.backend,
                downloads.len(),
                listed
            );
        }

        let matched = self.find_matching_downloads(&downloads, backend);
        self.last_listed = matched.clone();

        let watched = (0..self.tracked_files.len())
            .filter(|slot| self.watching(*slot))
            .count();
        let found = matched
            .iter()
            .filter(|(slot, _)| self.watching(*slot))
            .count();
        if poll_count <= 3 || found != watched {
            info!(
                "Matched {} of {} watched downloads from {} (poll {})",
                found, watched, self.batch.backend, poll_count
            );
            self.log_unmatched_files(&matched);
        }

        if !matched.is_empty() {
            *consecutive_empty = 0;
        }

        // Handle grace period for downloads to appear. A handoff removes its
        // slot's record, so a batch that was seen before can list nothing
        // and still be under way; its absent slots are judged one by one.
        let never_seen = self.track_states.iter().all(|state| !state.seen);
        if matched.is_empty() && never_seen {
            *consecutive_empty += 1;
            if *consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                warn!(
                    "No active downloads found for batch after {} attempts ({}s), assuming completed or lost: {:?}",
                    MAX_CONSECUTIVE_EMPTY,
                    MAX_CONSECUTIVE_EMPTY * 2,
                    self.filenames()
                );
                // Every slot written off has settled: an album batch imports
                // what it was handed found.
                if self.retry_or_fail_absent_batch(backend).await
                    && self.check_completion(&[], backend).await
                {
                    return true;
                }
                // At least one slot moved to another peer, or a joining
                // request is adding slots. Neither can show up in a list
                // fetched before it was enqueued, so the grace window starts
                // again from here.
                *consecutive_empty = 0;
                return false;
            }
            if (*consecutive_empty).is_multiple_of(5) {
                info!(
                    "Waiting for downloads to appear in {}, attempt {}/{} ({}/{}s)",
                    self.batch.backend,
                    *consecutive_empty,
                    MAX_CONSECUTIVE_EMPTY,
                    *consecutive_empty * 2,
                    MAX_CONSECUTIVE_EMPTY * 2
                );
            }
            return false;
        }

        // Process individual tracks
        let sending = self.peers_sending(&downloads);
        self.process_tracks(&matched, &sending, backend).await;

        // Fail tracks that never appeared or vanished from slskd's list,
        // so one absent track cannot stall the batch forever
        self.handle_absent_tracks(&matched, backend).await;

        // After the tracks were handled, so a slot that was just handed off,
        // timed out or rebound is not shown with its stale slskd state.
        self.send_status_update(&matched);

        // Check completion
        self.check_completion(&matched, backend).await
    }

    /// Filenames currently tracked, in slot order.
    fn filenames(&self) -> Vec<String> {
        self.tracked_files
            .iter()
            .map(|tracked| tracked.filename.clone())
            .collect()
    }

    /// Find each slot's transfer in slskd's list by its id, paired with the
    /// slot.
    ///
    /// When a watched slot's own transfer is unusable (gone, or failed) but
    /// a send of its failover made a live transfer after all, the slot
    /// adopts it (`adoptable`). Only a transfer no batch owns, or one this
    /// batch already owns, may be adopted; another batch's, even the same
    /// user's, has its own monitor.
    fn find_matching_downloads(
        &mut self,
        downloads: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) -> Vec<(usize, DownloadProgress)> {
        let mut matched = Vec::new();
        for slot in 0..self.tracked_files.len() {
            let tracked = &self.tracked_files[slot];
            let own = transfer_by_id(downloads, &tracked.source, &tracked.transfer_id);
            let usable = own.is_some_and(|dl| !is_failed(&dl.state));
            if !usable && self.watching(slot) {
                if let Some((retry, sent)) = self.adoptable(slot, downloads, backend.id()) {
                    let (retry, sent) = (retry.clone(), sent.clone());
                    if self.adopt(slot, &retry, sent, backend.id()) {
                        matched.push((slot, retry));
                        continue;
                    }
                }
            }
            if let Some(dl) = own {
                matched.push((slot, dl.clone()));
            }
        }
        matched
    }

    /// A live transfer one of `slot`'s sends made (`Sent::made`) that this
    /// batch may adopt, with that send: the answer refused the send, or
    /// never came, yet the backend made the transfer. Only such a transfer
    /// is the slot's own. Another of the same file, or of one named alike,
    /// is someone else's download, from slskd's own UI or another batch.
    ///
    /// A record between attempts is not live: no watcher's clock bounds
    /// one no slot watches, and a record cancelled during slskd's retry
    /// delay stays that way for good. If slskd starts the retry, the record
    /// reads `Queued, Locally` on the next poll and is adoptable then.
    fn adoptable<'a>(
        &'a self,
        slot: usize,
        downloads: &'a [DownloadProgress],
        backend: &'static str,
    ) -> Option<(&'a DownloadProgress, &'a Sent)> {
        let registry = TransferRegistry::global();
        self.tracked_files[slot].sent.iter().find_map(|sent| {
            downloads
                .iter()
                .find(|dl| {
                    sent.made(dl)
                        && !is_terminal_state(&dl.state)
                        && dl.pending_retry.is_none()
                        && !self.owns(dl)
                        && registry.may_adopt(
                            &transfer_key(backend, &dl.source, &dl.item),
                            &self.batch.owner,
                        )
                })
                .map(|dl| (dl, sent))
        })
    }

    /// Whether `dl` is a transfer this batch watches or abandoned.
    fn owns(&self, dl: &DownloadProgress) -> bool {
        (0..self.tracked_files.len()).any(|slot| self.is_current(slot, dl))
            || self
                .abandoned
                .iter()
                .any(|(peer, id)| is_transfer(dl, peer, id))
    }

    /// Rebind `slot` to `retry`, the transfer `sent` made, taking its
    /// registry key over from the slot's own. Returns false when the key
    /// was taken meanwhile.
    fn adopt(
        &mut self,
        slot: usize,
        retry: &DownloadProgress,
        sent: Sent,
        backend: &'static str,
    ) -> bool {
        let old = self.slot_key(slot, backend);
        let new = transfer_key(backend, &retry.source, &retry.item);
        if !TransferRegistry::global().rebind(&old, new, &self.batch) {
            return false;
        }
        let tracked = &self.tracked_files[slot];
        info!(
            "Failover send of {} to {} made a transfer after all (was {}), rebinding",
            retry.item, retry.source, tracked.source
        );
        // Its record, failed or already gone, no longer belongs to a slot.
        self.abandoned
            .push((tracked.source.clone(), tracked.transfer_id.clone()));
        let job = tracked.job.clone();
        let sends = std::mem::take(&mut self.tracked_files[slot].sent);
        let previous_item = std::mem::replace(
            &mut self.tracked_files[slot],
            TrackedFile {
                source: retry.source.clone(),
                filename: retry.item.clone(),
                transfer_id: retry.id.clone(),
                // The failover enqueued it into the batch's folder.
                destination: Some(self.batch.destination.clone()),
                job,
                sent: sends,
            },
        )
        .filename;
        // Reset state so the adopted transfer is monitored and imported
        self.track_states[slot] = TrackState::bound_now();
        self.record_adoption(slot, sent);
        if previous_item != retry.item {
            self.replace_row(previous_item, self.stamp(slot, retry.clone()));
        }
        true
    }

    /// Log watched slots slskd did not list.
    fn log_unmatched_files(&self, matched: &[(usize, DownloadProgress)]) {
        for (slot, tracked) in self.tracked_files.iter().enumerate() {
            if self.watching(slot) && !matched.iter().any(|(s, _)| *s == slot) {
                debug!(
                    "Unmatched transfer {} of {}",
                    tracked.transfer_id, tracked.filename
                );
            }
        }
    }

    /// Send the rows of the slots still being watched to the UI. A slot the
    /// import queue took over would otherwise have its import row
    /// overwritten with slskd's Completed every poll. A failed transfer is
    /// not shown while its failure is being confirmed: its key is still held
    /// then, so a retry from its row would be refused. The row goes out when
    /// the slot settles.
    fn send_status_update(&self, matched: &[(usize, DownloadProgress)]) {
        let entries: Vec<DownloadProgress> = matched
            .iter()
            .filter(|(slot, dl)| {
                self.watching(*slot) && self.is_current(*slot, dl) && !is_failed(&dl.state)
            })
            .map(|(slot, dl)| self.stamp(*slot, dl.clone()))
            .collect();
        self.report(JobState::Downloading, entries);
    }

    /// Sends `rows` (each stamped) as one Progress event and records them as `state`.
    fn report(&self, state: JobState, rows: Vec<DownloadProgress>) {
        if rows.is_empty() {
            return;
        }
        self.send_rows(rows.clone());
        self.ledger.record(JobEvent::Slots { state, rows });
    }

    /// Sends the rows `write_off` gave (each stamped) as one Progress event,
    /// and records each by its own state: Cancelled when its user cancelled
    /// it or slskd shows its transfer cancelled, Failed otherwise.
    fn report_settled(&self, rows: Vec<DownloadProgress>) {
        if rows.is_empty() {
            return;
        }
        self.send_rows(rows.clone());
        let (cancelled, failed): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|row| row.state == DownloadState::Cancelled);
        for (state, rows) in [(JobState::Cancelled, cancelled), (JobState::Failed, failed)] {
            if !rows.is_empty() {
                self.ledger.record(JobEvent::Slots { state, rows });
            }
        }
    }

    /// Sends `rows` to the UI as one Progress event. A broadcast send fails
    /// only when no tab is open to receive it, which is no failure: the rows
    /// are recorded, and a tab opened later reads them.
    fn send_rows(&self, rows: Vec<DownloadProgress>) {
        let _ = self.tx.send(DownloadEvent::Progress(rows));
    }

    /// `entry` marked with the batch and its backend, and with `slot`'s job.
    /// Every row the monitor sends is built for one slot.
    fn stamp(&self, slot: usize, entry: DownloadProgress) -> DownloadProgress {
        self.batch
            .mark(entry)
            .with_job(self.tracked_files[slot].job.clone())
    }

    /// The peers sending this slskd any file on this poll, for this batch or
    /// any other: a transfer from that peer in `downloads` is in progress, or
    /// shows Completed for the first time. One slskd account downloads for
    /// every Soulful user, and a Soulseek.NET peer answers every request with
    /// `Queued` and sends one file per downloading user at a time
    /// (`MaximumConcurrentUploadsPerUser` is 1), so every other file from it
    /// waits in its queue meanwhile.
    /// Only the transfers `downloads` shows Completed stay remembered, so the
    /// set never outgrows one listing.
    fn peers_sending(&mut self, downloads: &[DownloadProgress]) -> HashSet<String> {
        let mut sending = HashSet::new();
        let mut completed = HashSet::new();
        for dl in downloads {
            let first_completion = is_completed(&dl.state) && {
                let transfer = (dl.source.clone(), dl.id.clone());
                let first = !self.completed_seen.contains(&transfer);
                completed.insert(transfer);
                first
            };
            if dl.state == DownloadState::InProgress || first_completion {
                sending.insert(dl.source.clone());
            }
        }
        self.completed_seen = completed;
        sending
    }

    /// Process each watched track, handling timeouts, completions and
    /// failures. `sending` are the peers sending this slskd a file on this
    /// poll (`peers_sending`).
    async fn process_tracks(
        &mut self,
        matched: &[(usize, DownloadProgress)],
        sending: &HashSet<String>,
        backend: &Arc<dyn DownloadBackend>,
    ) {
        for (slot, download) in matched {
            let slot = *slot;
            self.track_states[slot].seen = true;

            if !self.watching(slot) {
                continue;
            }

            if self.made_no_progress(slot, download, sending) {
                warn!(
                    "Track made no progress for {} minutes: {}",
                    PER_TRACK_TIMEOUT.as_secs() / 60,
                    download.item
                );
                self.time_out(slot, download, per_track_timeout(), backend)
                    .await;
                continue;
            }

            // A peer can hold its queue for hours; another source may send
            // the file now. Only the time the peer sends this slskd nothing
            // counts, never the time any file it sends this slskd takes.
            if download.queued_at_source && !sending.contains(&download.source) {
                let since = *self.track_states[slot]
                    .queued_at_source_since
                    .get_or_insert_with(Instant::now);
                if since.elapsed() > REMOTE_QUEUE_TIMEOUT {
                    let failure = stuck_in_remote_queue();
                    warn!("{}: {}", failure.message, download.item);
                    self.time_out(slot, download, failure, backend).await;
                    continue;
                }
            } else {
                self.track_states[slot].queued_at_source_since = None;
            }

            // Singleton mode: process completed tracks immediately
            if self.batch.import_mode != ImportMode::Album && is_completed(&download.state) {
                info!(
                    "Track completed, processing immediately (singleton mode): {}",
                    download.item
                );
                self.start_hand_off(slot, download, backend);
                continue;
            }

            // Settle terminal failures (errored/cancelled/aborted), but only
            // once the failure has persisted for FAILED_STATE_CONFIRM:
            // slskd 0.26 retries failures and briefly reports the failed
            // state before re-queueing the transfer, and a re-queued
            // transfer resets the clock.
            if is_failed(&download.state) {
                let failing_since = self.track_states[slot]
                    .failing_since
                    .get_or_insert_with(Instant::now);
                if failing_since.elapsed() >= FAILED_STATE_CONFIRM {
                    let retryable = !matches!(download.state, DownloadState::Cancelled);
                    if !retryable || !self.try_failover(slot, backend, download.clone()).await {
                        let row = self.write_off(
                            slot,
                            SlotPhase::Settled,
                            backend.id(),
                            download.clone(),
                        );
                        self.report_settled(vec![self.stamp(slot, row)]);
                    }
                }
            } else {
                self.track_states[slot].failing_since = None;
            }
        }
    }

    /// Whether `slot`'s unfinished transfer, as `download` shows it on this
    /// poll, went `PER_TRACK_TIMEOUT` without progress. Its clock restarts
    /// on a poll that shows it moved bytes, or waiting in its source's queue
    /// while that source is in `sending` (`peers_sending`): then it waits
    /// its turn, as the remote queue limit also judges it.
    fn made_no_progress(
        &mut self,
        slot: usize,
        download: &DownloadProgress,
        sending: &HashSet<String>,
    ) -> bool {
        let track = &mut self.track_states[slot];
        let moved = download.transferred > track.transferred;
        track.transferred = download.transferred;
        if moved || (download.queued_at_source && sending.contains(&download.source)) {
            track.stalled_since = None;
        }
        let since = *track.stalled_since.get_or_insert_with(Instant::now);
        since.elapsed() > PER_TRACK_TIMEOUT && !is_terminal_state(&download.state)
    }

    /// Fail `slot` over to its next source, or else cancel its transfer,
    /// settle it TimedOut and show `download` Failed with `failure`, no
    /// longer queued at its source.
    async fn time_out(
        &mut self,
        slot: usize,
        download: &DownloadProgress,
        failure: Failure,
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let row = DownloadProgress {
            queued_at_source: false,
            ..download.clone()
        }
        .failed_with(failure);
        if self.try_failover(slot, backend, row.clone()).await {
            return;
        }
        self.abandon(slot, backend).await;
        let row = self.write_off(slot, SlotPhase::TimedOut, backend.id(), row);
        self.report_settled(vec![self.stamp(slot, row)]);
    }

    /// Hand a singleton slot's finished transfer to the import queue. Its
    /// Completed row goes out first: once the queue has the row, only the
    /// queue updates it.
    fn start_hand_off(
        &mut self,
        slot: usize,
        download: &DownloadProgress,
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let row = self.stamp(slot, download.clone());
        self.report(JobState::Downloading, vec![row.clone()]);
        self.track_states[slot].phase = SlotPhase::HandedOff;
        let handoff = Handoff {
            row,
            destination: self.tracked_files[slot].destination.clone(),
            key: self.slot_key(slot, backend.id()),
            owner: self.batch.owner.clone(),
            duplicates: self.batch.duplicates.clone(),
            target: self.target_path.clone(),
            download_base: self.download_base.clone(),
            claimed: Arc::clone(&self.claimed),
            ledger: self.ledger.clone(),
        };
        // Spawned so the poll loop never waits on the file showing up.
        self.handoffs
            .push(tokio::spawn(hand_off(handoff, Arc::clone(backend))));
    }

    /// Fail tracks that are absent from slskd's transfer list: either they
    /// never appeared (rejected/lost requests) or they vanished after being
    /// seen (transfer removed). Each gets a terminal Failed event so the UI
    /// and batch completion never wait on them forever.
    async fn handle_absent_tracks(
        &mut self,
        matched: &[(usize, DownloadProgress)],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot) {
                continue;
            }

            let present = matched
                .iter()
                .any(|(s, dl)| *s == slot && self.is_current(slot, dl));
            if present {
                self.track_states[slot].missing_since = None;
                continue;
            }
            // A poll that does not list it does not show it waiting in its
            // source's queue.
            self.track_states[slot].queued_at_source_since = None;

            let absent = if self.track_states[slot].seen {
                let missing_since = self.track_states[slot]
                    .missing_since
                    .get_or_insert_with(Instant::now);
                (missing_since.elapsed() > ABSENT_TRACK_TIMEOUT)
                    .then(|| disappeared(self.service()))
            } else {
                (self.track_states[slot].bound_at.elapsed() > ABSENT_TRACK_TIMEOUT)
                    .then(|| never_appeared(self.service()))
            };

            if let Some(failure) = absent {
                // Logged only once the track is really being written off:
                // a successful failover reports its own retry instead.
                let row = make_failed_progress(&self.tracked_files[slot], failure.clone());
                if self.try_failover(slot, backend, row.clone()).await {
                    continue;
                }
                warn!(
                    "{} after {}s, marking failed: {}",
                    failure.message,
                    ABSENT_TRACK_TIMEOUT.as_secs(),
                    self.tracked_files[slot].filename
                );
                let row = self.write_off(slot, SlotPhase::Settled, backend.id(), row);
                failed.push(self.stamp(slot, row));
            }
        }

        self.report_settled(failed);
    }

    /// Give every watched slot another peer after a batch's transfers never
    /// showed up in slskd at all, and write off only the ones with nowhere
    /// left to go. Returns true when nothing was retried, i.e. the batch is
    /// finished.
    ///
    /// This is the only failover route a single-track batch has on this path:
    /// while nothing of the batch was ever seen, `process_poll_result`
    /// returns before `process_tracks` and `handle_absent_tracks` ever run,
    /// and the empty-poll fuse burns out at 30s, well inside the 120s those
    /// two wait. A batch with more than one track only reaches them because
    /// a sibling showed up.
    async fn retry_or_fail_absent_batch(&mut self, backend: &Arc<dyn DownloadBackend>) -> bool {
        let failure = never_appeared(self.service());
        let mut retried = false;
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot) {
                continue;
            }
            let row = make_failed_progress(&self.tracked_files[slot], failure.clone());
            if self.try_failover(slot, backend, row.clone()).await {
                retried = true;
                continue;
            }
            // Without this, rows whose transfer never surfaced in slskd would
            // sit at "Queued" in the UI forever.
            let row = self.write_off(slot, SlotPhase::Settled, backend.id(), row);
            failed.push(self.stamp(slot, row));
        }

        self.report_settled(failed);
        !retried
    }

    /// Ends a batch that must stop before its transfers finished, `failure`
    /// saying why: every watched slot still under way fails with it. An
    /// album batch then imports what it has as a partial album, as it does
    /// once every slot settled (`finish_album`): the slots the last readable
    /// listing showed finished, with the files it was handed found. Its
    /// other transfers may still run where Soulful cannot see, and stay
    /// failed for a retry.
    ///
    /// `backend` is the one that listed those transfers, which removes their
    /// records once their files are found. None when no backend was ever
    /// looked up: then nothing was listed, no slot finished, and only the
    /// found files are imported.
    async fn stop_early(&mut self, failure: Failure, backend: Option<&Arc<dyn DownloadBackend>>) {
        self.fail_unprocessed_tracks(failure);
        if self.batch.import_mode != ImportMode::Album {
            return;
        }
        match backend {
            Some(backend) => {
                let listed = std::mem::take(&mut self.last_listed);
                self.finish_album(&listed, backend).await;
            }
            None => {
                let found = std::mem::take(&mut self.found);
                self.import_album(found).await;
            }
        }
    }

    /// Mark every watched track as failed and notify the UI, except a
    /// finished one of an album batch (`stop_early` imports it). Used when
    /// monitoring must stop early so downloads are never left dangling. With
    /// no backend at hand their keys are released when the monitor drops.
    fn fail_unprocessed_tracks(&mut self, failure: Failure) {
        let album = self.batch.import_mode == ImportMode::Album;
        let mut failed: Vec<DownloadProgress> = Vec::new();
        for slot in 0..self.tracked_files.len() {
            if self.watching(slot) && !(album && self.finished_in(slot, &self.last_listed)) {
                self.track_states[slot].phase = SlotPhase::Settled;
                let row = make_failed_progress(&self.tracked_files[slot], failure.clone());
                failed.push(self.stamp(slot, row));
            }
        }
        self.report(JobState::Failed, failed);
    }

    /// Whether `matched` shows the transfer `slot` watches completed.
    fn finished_in(&self, slot: usize, matched: &[(usize, DownloadProgress)]) -> bool {
        matched
            .iter()
            .any(|(s, dl)| *s == slot && self.is_current(slot, dl) && is_completed(&dl.state))
    }

    /// Check if all downloads are complete. Returns true if monitoring should stop.
    /// It is the one place a batch that runs to its end is found finished, so
    /// the one place it imports its album (`stop_early` imports a partial one
    /// for a batch that cannot): its watched slots, the ones joining
    /// requests added, and the files it was handed found.
    ///
    /// A track is settled when it left `Watching` or its transfer completed
    /// successfully. Settled must be judged per track: in album mode
    /// completed tracks stay watched until the whole batch is handled.
    /// Failed states settle through the phase only, after process_tracks
    /// confirms the failure persisted (slskd 0.26 may retry it).
    async fn check_completion(
        &mut self,
        matched: &[(usize, DownloadProgress)],
        backend: &Arc<dyn DownloadBackend>,
    ) -> bool {
        let all_settled = (0..self.tracked_files.len())
            .all(|slot| !self.watching(slot) || self.finished_in(slot, matched));

        // A joining request may still be enqueueing into this batch, or its
        // slots wait to be drained: finishing now would leave them unwatched.
        if all_settled && self.batch.inbox.close_if_idle() {
            if self.batch.import_mode == ImportMode::Album {
                self.finish_album(matched, backend).await;
            }
            info!("All downloads finished");
            return true;
        }

        false
    }

    /// Import every finished slot of an album batch together, with the files
    /// it was handed found (`with_found`). The slskd records are only removed
    /// once every file was looked for on disk, and the keys only once the
    /// imports are done.
    async fn finish_album(
        &mut self,
        matched: &[(usize, DownloadProgress)],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let finished: Vec<(usize, DownloadProgress, Option<String>)> = matched
            .iter()
            .filter(|(slot, dl)| {
                self.watching(*slot) && self.is_current(*slot, dl) && is_completed(&dl.state)
            })
            .map(|(slot, dl)| {
                (
                    *slot,
                    self.stamp(*slot, dl.clone()),
                    self.tracked_files[*slot].destination.clone(),
                )
            })
            .collect();
        if finished.is_empty() && self.found.is_empty() {
            info!("Album mode: No successful downloads to process");
            return;
        }

        let base = self.download_base.clone();
        let claimed = Arc::clone(&self.claimed);
        let paths = futures::future::join_all(finished.iter().map(|(_, row, destination)| {
            resolve_on_disk(&row.item, &base, destination.as_deref(), &claimed)
        }))
        .await;

        let mut resolved = Vec::new();
        let mut resolved_slots = Vec::new();
        let mut missing = Vec::new();
        for ((slot, row, destination), path) in finished.into_iter().zip(paths) {
            match path {
                Ok(path) => {
                    resolved.push(ResolvedSlot {
                        row,
                        path,
                        destination,
                    });
                    resolved_slots.push(slot);
                }
                Err(why) => {
                    self.settle(slot, SlotPhase::Settled, backend.id());
                    missing.push(unresolved_row(row, &self.download_base, why));
                }
            }
        }
        if !missing.is_empty() {
            send_user_event(
                &self.batch.owner.user_id,
                DownloadEvent::Progress(missing.clone()),
            )
            .await;
            self.ledger.record(JobEvent::Slots {
                state: JobState::Failed,
                rows: missing,
            });
        }

        for slot in &resolved_slots {
            let tracked = &self.tracked_files[*slot];
            if let Err(e) = backend
                .cancel_download(&tracked.source, &tracked.transfer_id, true)
                .await
            {
                warn!(
                    "Failed to remove finished transfer {} from {}: {}",
                    tracked.transfer_id,
                    backend.id(),
                    e
                );
            }
            self.track_states[*slot].phase = SlotPhase::HandedOff;
        }

        let mut album = std::mem::take(&mut self.found);
        album.append(&mut resolved);
        self.import_album(album).await;
        let registry = TransferRegistry::global();
        for slot in resolved_slots {
            registry.release(&self.slot_key(slot, backend.id()), &self.batch.owner);
        }
    }

    /// Queue the imports of an album batch's finished files (`album_jobs`)
    /// and wait until they are done.
    async fn import_album(&self, resolved: Vec<ResolvedSlot>) {
        if resolved.is_empty() {
            return;
        }
        let files = resolved.len();
        let jobs = album_jobs(
            resolved,
            &self.target_path,
            &self.batch.owner.user_id,
            &self.batch.duplicates,
            &self.download_base,
        );
        info!(
            "Album mode: queueing {} import(s) of {} downloads into {:?}",
            jobs.len(),
            files,
            self.target_path
        );
        let mut handles = Vec::new();
        for job in jobs {
            handles.push(IMPORT_QUEUE.submit(job).await);
        }
        for handle in handles {
            handle.wait().await;
        }
    }
}

impl Drop for DownloadMonitor {
    /// Whatever ended the monitor, its batch takes no more slots, and its
    /// files may be downloaded again by anyone.
    fn drop(&mut self) {
        // Nothing can be cancelled here; run() already did unless it never finished.
        let _ = self.close_inbox();
        TransferRegistry::global().release_batch(&self.batch.owner);
    }
}

/// A singleton slot's finished transfer on its way to the import queue.
struct Handoff {
    /// Stamped with its batch.
    row: DownloadProgress,
    destination: Option<String>,
    key: TransferKey,
    owner: TransferOwner,
    /// How the import treats earlier copies: the batch's rule.
    duplicates: DuplicateRule,
    target: PathBuf,
    download_base: PathBuf,
    claimed: Arc<Mutex<HashSet<FileId>>>,
    /// Where a row it sends is recorded.
    ledger: Ledger,
}

/// Find the file on disk, remove its slskd record, import it and free its
/// key once the import is done. The record is only removed once the file was
/// found, so slskd has moved it out of its incomplete folder.
async fn hand_off(h: Handoff, backend: Arc<dyn DownloadBackend>) {
    let registry = TransferRegistry::global();
    let path = match resolve_on_disk(
        &h.row.item,
        &h.download_base,
        h.destination.as_deref(),
        &h.claimed,
    )
    .await
    {
        Ok(path) => path,
        Err(why) => {
            let row = unresolved_row(h.row, &h.download_base, why);
            send_user_event(&h.owner.user_id, DownloadEvent::Progress(vec![row.clone()])).await;
            h.ledger.record(JobEvent::Slots {
                state: JobState::Failed,
                rows: vec![row],
            });
            registry.release(&h.key, &h.owner);
            return;
        }
    };
    if let Err(e) = backend
        .cancel_download(&h.row.source, &h.row.id, true)
        .await
    {
        warn!(
            "Failed to remove finished transfer {} from {}: {}",
            h.row.id,
            backend.id(),
            e
        );
    }
    let job = import_job(
        Downloaded::File(path),
        vec![h.row],
        &h.target,
        &h.owner.user_id,
        &h.duplicates,
        &h.download_base,
    );
    IMPORT_QUEUE.submit(job).await.wait().await;
    registry.release(&h.key, &h.owner);
}

/// Build a synthetic terminal progress entry for a track slskd no longer
/// reports, so the UI can settle its row.
fn make_failed_progress(tracked: &TrackedFile, failure: Failure) -> DownloadProgress {
    DownloadProgress::failed(
        tracked.transfer_id.clone(),
        tracked.source.clone(),
        tracked.filename.clone(),
        failure,
    )
}

/// Whether `dl` is the transfer slskd lists under `id` from `source`.
fn is_transfer(dl: &DownloadProgress, source: &str, id: &str) -> bool {
    dl.source == source && dl.id == id
}

/// Stop `transfer_id` from `peer`, a transfer its watcher gives up on: left
/// running it would still deliver into the watcher's folder with nobody to
/// import the file. slskd cancels by transfer id, and a cancel that fails is
/// only logged.
///
/// The cancel keeps the record: the transfer may have just succeeded, and
/// removing a succeeded record before slskd moves its file strands the
/// file. `remove_ended_transfers` removes it once it shows Cancelled or
/// Failed.
pub(crate) async fn cancel_abandoned(
    backend: &Arc<dyn DownloadBackend>,
    peer: &str,
    transfer_id: &str,
) {
    if let Err(e) = backend.cancel_download(peer, transfer_id, false).await {
        warn!(
            "Failed to cancel abandoned transfer {} in {}: {}",
            transfer_id,
            backend.id(),
            e
        );
    }
}

/// How long `await_ended` waits for slskd to end transfers it was told to
/// cancel. A cancel of a live transfer only fires its token
/// (`DownloadService.cs:998-1007`); the record turns Cancelled later, when
/// the download task's catch fails it (`:1546-1550`, `:1045-1068`).
pub(crate) const CANCEL_SETTLE: Duration = Duration::from_secs(10);

/// Wait, for at most `CANCEL_SETTLE`, until slskd lists every one of
/// `transfers` as ended or no longer lists it. slskd removes only a record
/// that ended (`DownloadService.cs:977-981`), so `remove_ended_transfers`
/// right after a cancel would miss the transfers it was meant for.
pub(crate) async fn await_ended(
    backend: &Arc<dyn DownloadBackend>,
    transfers: &[(String, String)],
) {
    let peers = peers_of(transfers);
    let deadline = Instant::now() + CANCEL_SETTLE;
    loop {
        // A listing that fails is asked again on the next tick.
        if let Ok(downloads) = backend.get_downloads_for(&peers).await {
            if transfers.iter().all(|(peer, id)| {
                transfer_by_id(&downloads, peer, id).is_none_or(|dl| is_terminal_state(&dl.state))
            }) {
                return;
            }
        }
        if Instant::now() >= deadline {
            warn!(
                "{} had not ended every cancelled transfer after {:?}",
                backend.id(),
                CANCEL_SETTLE
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The peers of `transfers`, each once.
fn peers_of(transfers: &[(String, String)]) -> Vec<String> {
    let mut peers: Vec<String> = Vec::new();
    for (peer, _) in transfers {
        if !peers.contains(peer) {
            peers.push(peer.clone());
        }
    }
    peers
}

/// Remove the records of `transfers`, the `(peer, transfer id)` pairs a
/// watcher is done with, that ended without a file, and return the listing
/// they were read from, or `None` when slskd could not list them.
///
/// A succeeded record is left: removing it before its file is found on disk
/// can strand the file in slskd's incomplete folder. Only these ids: another
/// watcher's record of the same file is that watcher's to remove. A failed
/// attempt slskd says it will retry ended without a file too, and with its
/// watcher done nobody would import what a retry fetched.
pub(crate) async fn remove_ended_transfers(
    backend: &Arc<dyn DownloadBackend>,
    transfers: &[(String, String)],
) -> Option<Vec<DownloadProgress>> {
    let downloads = match backend.get_downloads_for(&peers_of(transfers)).await {
        Ok(d) => d,
        Err(e) => {
            warn!("Could not list downloads for batch cleanup: {}", e);
            return None;
        }
    };

    for entry in downloads.iter().filter(|entry| {
        (is_failed(&entry.state) || entry.pending_retry.is_some())
            && transfers
                .iter()
                .any(|(peer, id)| is_transfer(entry, peer, id))
    }) {
        if let Err(e) = backend
            .cancel_download(&entry.source, &entry.id, true)
            .await
        {
            warn!(
                "Failed to remove finished transfer {} from {}: {}",
                entry.id,
                backend.id(),
                e
            );
        }
    }
    Some(downloads)
}

/// The transfer slskd lists under `id` from `source`.
pub(crate) fn transfer_by_id<'a>(
    downloads: &'a [DownloadProgress],
    source: &str,
    id: &str,
) -> Option<&'a DownloadProgress> {
    downloads.iter().find(|dl| is_transfer(dl, source, id))
}

/// Check if a download state indicates a terminal state (complete or failed).
pub(crate) fn is_terminal_state(state: &DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Completed
            | DownloadState::Imported
            | DownloadState::ImportSkipped
            | DownloadState::Failed(_)
            | DownloadState::Cancelled
    )
}

/// Check if a download state indicates successful download.
fn is_completed(state: &DownloadState) -> bool {
    matches!(state, DownloadState::Completed)
}

/// Whether a transfer ended without its file: failed, errored, rejected,
/// timed out or cancelled in slskd.
fn is_failed(state: &DownloadState) -> bool {
    matches!(state, DownloadState::Failed(_) | DownloadState::Cancelled)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::ledger::Recorded;
    use crate::server_fns::download::enqueue::{monitor_stopped, MONITOR_STOPPED};
    use crate::server_fns::download::test_support::{
        batch, batch_on, item, key, transfer, Cancel, ScriptedOutcome, StubBackend, StubBackends,
        StubFault,
    };
    use shared::download::DownloadableItem;
    use shared::failure::{Service, UserActionCode};
    use soulbeet::error::{MissingSetting, SoulseekError};
    use std::path::Path;
    use tokio::sync::broadcast;

    fn assert_failed(row: &DownloadProgress, code: FailureCode, message: &str) {
        assert_eq!(row.state, DownloadState::Failed(message.into()));
        assert_eq!(row.error.as_deref(), Some(message));
        assert_eq!(row.failure.as_ref().map(|f| f.code), Some(code));
    }

    /// An instant `d` in the past, so a test can reach the monitor's
    /// elapsed-time windows without sleeping through them.
    fn ago(d: Duration) -> Instant {
        Instant::from_std(
            std::time::Instant::now()
                .checked_sub(d)
                .expect("monotonic clock younger than the window under test"),
        )
    }

    /// The transfer id slskd gave `filename` in these tests. It differs from
    /// the filename, so code that confuses the two fails.
    fn id_of(filename: &str) -> String {
        format!("t-{filename}")
    }

    /// slskd's record of `filename` from `source`, under its test id.
    fn listed(source: &str, filename: &str, state: DownloadState) -> DownloadProgress {
        transfer(source, filename, &id_of(filename), state)
    }

    /// slskd's record of `filename` from `source`, waiting in the peer's queue.
    fn at_source(source: &str, filename: &str) -> DownloadProgress {
        DownloadProgress {
            queued_at_source: true,
            ..listed(source, filename, DownloadState::Queued)
        }
    }

    /// slskd's record of `filename` from `source`, a failed attempt slskd
    /// says it will retry.
    fn pending_retry(source: &str, filename: &str) -> DownloadProgress {
        DownloadProgress {
            pending_retry: Some(Failure::new(
                FailureCode::TransferConnectionFailed,
                Stage::Transfer,
                RetryAdvice::OtherSource,
                "Connection reset by peer",
            )),
            ..listed(source, filename, DownloadState::Queued)
        }
    }

    /// A slskd transfer of `filename` from `source` in a failed terminal state.
    fn failed_transfer(source: &str, filename: &str) -> DownloadProgress {
        listed(
            source,
            filename,
            DownloadState::Failed("peer went away".into()),
        )
    }

    /// Slot `slot` of job `j1`.
    fn slot_of_j1(slot: u32) -> JobSlot {
        JobSlot {
            job: "j1".into(),
            slot,
        }
    }

    fn watched(source: &str, filename: &str, slot: u32) -> WatchedTransfer {
        WatchedTransfer {
            source: source.to_string(),
            filename: filename.to_string(),
            transfer_id: id_of(filename),
            job: slot_of_j1(slot),
            adopted: false,
            sent: Vec::new(),
        }
    }

    /// A monitor watching one track with two alternates behind it, plus the
    /// receiver its events land in. Holding the receiver keeps the broadcast
    /// live, so tests see what the UI would.
    fn failover_monitor() -> (
        DownloadMonitor,
        broadcast::Receiver<DownloadEvent>,
        Recorded,
    ) {
        let (tx, rx) = broadcast::channel(16);
        let (m, recorded) = monitor_on(tx, vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let m = m.with_failover(SourcePool::single(vec![
            item("peer_a", "Wake Up", "dirA/aaa.flac"),
            item("peer_b", "Wake Up", "dirB/bbb.flac"),
            item("peer_c", "Wake Up", "dirC/ccc.flac"),
        ]));
        (m, rx, recorded)
    }

    fn monitor(sources: Vec<&str>, filenames: Vec<&str>) -> (DownloadMonitor, Recorded) {
        let (tx, _rx) = broadcast::channel(16);
        monitor_on(tx, sources, filenames)
    }

    /// A monitor of job `j1`, slot n watching `filenames[n]` from
    /// `sources[n]`, recording to the ledger whose events it returns.
    fn monitor_on(
        tx: broadcast::Sender<DownloadEvent>,
        sources: Vec<&str>,
        filenames: Vec<&str>,
    ) -> (DownloadMonitor, Recorded) {
        monitor_for_job(tx, "j1", sources, filenames)
    }

    /// `monitor_on` with its slots in job `job`.
    fn monitor_for_job(
        tx: broadcast::Sender<DownloadEvent>,
        job: &str,
        sources: Vec<&str>,
        filenames: Vec<&str>,
    ) -> (DownloadMonitor, Recorded) {
        let (ledger, recorded) = Ledger::recording();
        let m = DownloadMonitor::new(
            (0..)
                .zip(sources.into_iter().zip(filenames))
                .map(|(slot, (source, filename))| WatchedTransfer {
                    job: JobSlot {
                        job: job.to_string(),
                        slot,
                    },
                    ..watched(source, filename, slot)
                })
                .collect(),
            PathBuf::from("/music"),
            tx,
            batch("X"),
            ledger,
        );
        (m, recorded)
    }

    /// Each recorded `Slots` row's item and backend, in order.
    fn backends_recorded(recorded: &mut Recorded) -> Vec<(String, Option<String>)> {
        recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots { rows, .. } => Some(rows),
                _ => None,
            })
            .flatten()
            .map(|r| (r.item, r.backend))
            .collect()
    }

    /// A recorded `Slots` event: its state, and each row's item and slot.
    type SlotsEvent = (JobState, Vec<(String, Option<JobSlot>)>);

    /// The `Slots` events recorded since the last call.
    fn slots_recorded(recorded: &mut Recorded) -> Vec<SlotsEvent> {
        recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots { state, rows } => {
                    Some((state, rows.into_iter().map(|r| (r.item, r.job)).collect()))
                }
                _ => None,
            })
            .collect()
    }

    /// The track handling and row updates of one poll, as
    /// `process_poll_result` runs them.
    async fn poll_tracks(
        m: &mut DownloadMonitor,
        listing: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let mut listing = listing.to_vec();
        m.read_pending_retries(&mut listing);
        m.apply_cancels(&listing, backend).await;
        let matched = m.find_matching_downloads(&listing, backend);
        let sending = m.peers_sending(&listing);
        m.process_tracks(&matched, &sending, backend).await;
        m.handle_absent_tracks(&matched, backend).await;
        m.send_status_update(&matched);
    }

    /// The states the monitor sent for `item`, in order.
    fn rows_for(rx: &mut broadcast::Receiver<DownloadEvent>, item: &str) -> Vec<DownloadProgress> {
        let mut sent = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DownloadEvent::Progress(rows) = event {
                sent.extend(rows.into_iter().filter(|r| r.item == item));
            }
        }
        sent
    }

    fn states_for(rx: &mut broadcast::Receiver<DownloadEvent>, item: &str) -> Vec<DownloadState> {
        rows_for(rx, item).into_iter().map(|r| r.state).collect()
    }

    fn phase(m: &DownloadMonitor, slot: usize) -> SlotPhase {
        m.track_states[slot].phase
    }

    #[tokio::test]
    async fn rows_the_monitor_sends_carry_their_slot() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) =
            monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "one.flac", DownloadState::InProgress),
                listed("peer_a", "two.flac", DownloadState::InProgress),
            ],
            &backend,
        )
        .await;

        assert_eq!(rows_for(&mut rx, "two.flac")[0].job, Some(slot_of_j1(1)));
    }

    #[tokio::test]
    async fn every_row_the_monitor_sends_is_recorded_with_its_slot_and_state() {
        let (tx, _rx) = broadcast::channel(16);
        let (mut m, mut recorded) =
            monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[1].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "one.flac", DownloadState::InProgress),
                failed_transfer("peer_a", "two.flac"),
            ],
            &backend,
        )
        .await;

        assert_eq!(
            slots_recorded(&mut recorded),
            vec![
                (
                    JobState::Failed,
                    vec![("two.flac".into(), Some(slot_of_j1(1)))]
                ),
                (
                    JobState::Downloading,
                    vec![("one.flac".into(), Some(slot_of_j1(0)))]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_slot_that_fails_over_records_its_next_source_and_the_request_it_is_sent_in() {
        let (mut m, _rx, mut recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        let attempts: Vec<(JobSlot, Option<String>, Option<String>)> = recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Attempt {
                    slot,
                    candidate,
                    request,
                } => Some((slot, candidate.map(|c| c.id), request)),
                _ => None,
            })
            .collect();
        assert_eq!(
            attempts,
            vec![(
                slot_of_j1(0),
                Some("dirB/bbb.flac".into()),
                stub.requests().first().cloned()
            )]
        );
    }

    #[tokio::test]
    async fn a_slot_that_fails_over_records_the_attempt_it_left_and_each_refused_alternate() {
        let (mut m, _rx, mut recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Rejected, ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        /// An event as (kind, source, file, state of its row or code of its failure, request).
        type Seen = (&'static str, String, String, String, Option<String>);
        let seen: Vec<Seen> = recorded
            .events()
            .into_iter()
            .map(|e| match e {
                JobEvent::Slots {
                    state: JobState::Downloading,
                    rows,
                } => {
                    let [row] = &rows[..] else {
                        panic!("one row: {rows:?}")
                    };
                    assert_eq!(row.job, Some(slot_of_j1(0)));
                    let state = match &row.state {
                        DownloadState::Failed(_) => "failed".to_string(),
                        other => format!("{other:?}"),
                    };
                    ("slots", row.source.clone(), row.item.clone(), state, None)
                }
                JobEvent::Attempt {
                    slot,
                    candidate: Some(candidate),
                    request,
                } => {
                    assert_eq!(slot, slot_of_j1(0));
                    (
                        "attempt",
                        candidate.source,
                        candidate.id,
                        String::new(),
                        request,
                    )
                }
                JobEvent::Refused {
                    slot,
                    candidate,
                    failure,
                } => {
                    assert_eq!(slot, slot_of_j1(0));
                    let code = failure.code.as_str().to_string();
                    ("refused", candidate.source, candidate.id, code, None)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let requests = stub.requests();
        let s = |text: &str| text.to_string();
        assert_eq!(
            seen,
            vec![
                ("slots", s("peer_a"), s("dirA/aaa.flac"), s("failed"), None),
                (
                    "attempt",
                    s("peer_b"),
                    s("dirB/bbb.flac"),
                    s(""),
                    Some(requests[0].clone())
                ),
                (
                    "refused",
                    s("peer_b"),
                    s("dirB/bbb.flac"),
                    s("peer_offline"),
                    None
                ),
                (
                    "attempt",
                    s("peer_c"),
                    s("dirC/ccc.flac"),
                    s(""),
                    Some(requests[1].clone())
                ),
                ("slots", s("peer_c"), s("dirC/ccc.flac"), s("Queued"), None),
            ]
        );
    }

    /// A ledger writing to a test database holding job j1 with slot 0, and that database.
    async fn stored_j1() -> (Ledger, sqlx::SqlitePool) {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                crate::ledger::NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: shared::download::DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                    picked: vec![(0, item("peer_a", "Wake Up", "dirA/aaa.flac"))],
                    kind: crate::ledger::JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        (ledger, db)
    }

    #[tokio::test]
    async fn a_failover_source_reaches_the_backend_only_once_it_is_stored() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let (ledger, db) = stored_j1().await;
        m.ledger = ledger;
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        // A slow writer: the pool's one connection is taken until the test lets it go.
        let busy = db.acquire().await.unwrap();
        let poll = tokio::spawn(async move {
            poll_tracks(
                &mut m,
                &[failed_transfer("peer_a", "dirA/aaa.flac")],
                &backend,
            )
            .await;
        });

        let early = tokio::time::timeout(Duration::from_millis(300), reached.notified());
        assert!(
            early.await.is_err(),
            "the backend heard of the source before it was stored"
        );
        drop(busy);
        reached.notified().await;
        let stored: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT candidate, request FROM job_attempts WHERE job_id = 'j1' AND slot = 0 AND n = 2",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let candidate: Option<DownloadableItem> =
            stored.0.map(|c| serde_json::from_str(&c).unwrap());
        assert_eq!(
            (candidate, stored.1),
            (
                Some(item("peer_b", "Wake Up", "dirB/bbb.flac")),
                stub.requests().first().cloned()
            )
        );
        release.notify_one();
        poll.await.unwrap();
    }

    #[tokio::test]
    async fn a_failover_source_the_ledger_cannot_store_is_not_sent() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let (ledger, db) = stored_j1().await;
        sqlx::query(
            "CREATE TRIGGER refuse_attempts BEFORE INSERT ON job_attempts \
             BEGIN SELECT RAISE(ABORT, 'this test refuses the write'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        m.ledger = ledger;
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        assert!(stub.destinations().is_empty(), "a source was sent unstored");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert_eq!(m.tracked_files[0].source, "peer_a");
        for (peer, file) in [("peer_b", "dirB/bbb.flac"), ("peer_c", "dirC/ccc.flac")] {
            assert!(
                TransferRegistry::global().may_adopt(&key(&backend, peer, file), &batch("Y").owner),
                "{file} is not held"
            );
        }
    }

    #[tokio::test]
    async fn one_failure_fails_over_once_per_poll() {
        // process_tracks and handle_absent_tracks run back to back on the SAME
        // poll. Once process_tracks has rebound the slot, the alternate's
        // transfer cannot be in that already-fetched list, so a slot whose
        // absence window ran from the batch's start would be declared "never
        // appeared" and fail over a second time in the same poll, orphaning
        // the transfer just enqueued.
        let (mut m, _rx, _recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();

        // Old enough that a batch-wide clock would consider it absent, with a
        // failure that has already outlived the confirmation window.
        m.track_states[0].bound_at = ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1));
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        assert_eq!(m.pool.attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(m.tracked_files[0].transfer_id, "t-dirB/bbb.flac");
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("dirA/aaa.flac"))],
            "the abandoned transfer is cancelled by its id, its record left for cleanup"
        );
    }

    #[tokio::test]
    async fn a_failover_enqueues_into_the_batch_folder_and_its_row_carries_the_transfer_id() {
        let (mut m, mut rx, _recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        assert_eq!(stub.destinations(), vec!["X [12345678]".to_string()]);
        assert_eq!(
            m.tracked_files[0].destination.as_deref(),
            Some("X [12345678]")
        );
        match rx.try_recv().expect("an event for the retry") {
            DownloadEvent::Replaced { entry, .. } => {
                // A cancel from this row must reach slskd by transfer id.
                assert_eq!(entry.id, "t-dirB/bbb.flac");
                assert_eq!(entry.batch_id.as_ref(), Some(&m.batch.owner.batch_id));
            }
            other => panic!("expected a row replacement, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failover_frees_the_abandoned_key() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let abandoned = key(&backend, "peer_a", "dirA/aaa.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&abandoned), &m.batch);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        let other = batch("Y");
        assert!(TransferRegistry::global().may_adopt(&abandoned, &other.owner));
        assert!(
            !TransferRegistry::global()
                .may_adopt(&key(&backend, "peer_b", "dirB/bbb.flac"), &other.owner),
            "the retry's key is the batch's"
        );
    }

    /// Notes that slot `slot` of `m` sent `file` from `source` in `request`.
    fn sent_to(m: &mut DownloadMonitor, slot: usize, source: &str, file: &str, request: &str) {
        m.tracked_files[slot].sent.push(Sent {
            candidate: item(source, "Wake Up", file),
            request: request.to_string(),
        });
    }

    /// `dl` as slskd lists a transfer made in `request`.
    fn made_in(request: &str, dl: DownloadProgress) -> DownloadProgress {
        DownloadProgress {
            request: Some(request.to_string()),
            ..dl
        }
    }

    #[tokio::test]
    async fn a_transfer_a_send_of_the_slot_made_is_adopted_into_the_batch_folder() {
        // The failover sent dirB/aaa.flac to peer_b in request r1 and its
        // answer was lost, yet slskd made the transfer, into this batch's
        // folder.
        let (mut m, mut recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        sent_to(&mut m, 0, "peer_b", "dirB/aaa.flac", "r1");
        let made = made_in(
            "r1",
            transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::InProgress),
        );

        let matched = m.find_matching_downloads(std::slice::from_ref(&made), &backend);

        assert_eq!(matched.len(), 1);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(m.tracked_files[0].transfer_id, "t-b");
        assert_eq!(
            m.tracked_files[0].destination.as_deref(),
            Some(m.batch.destination.as_str())
        );
        assert!(
            !TransferRegistry::global()
                .may_adopt(&key(&backend, "peer_b", "dirB/aaa.flac"), &batch("Y").owner),
            "the adopted transfer is now the batch's"
        );
        assert_eq!(
            recorded.events(),
            vec![
                JobEvent::Attempt {
                    slot: slot_of_j1(0),
                    candidate: Some(Box::new(item("peer_b", "Wake Up", "dirB/aaa.flac"))),
                    request: Some("r1".into()),
                },
                JobEvent::Slots {
                    state: JobState::Downloading,
                    rows: vec![m.stamp(0, made)]
                },
            ],
            "a restart finds the adopted transfer by its attempt"
        );
    }

    #[tokio::test]
    async fn an_untracked_transfer_named_like_the_slots_file_is_not_adopted() {
        // Someone downloads another album's 01 - Intro.flac in slskd's own
        // UI while the slot's transfer fails.
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["Wanted\\01 - Intro.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let theirs = transfer(
            "peer_b",
            "Other Album\\01 - Intro.flac",
            "t-b",
            DownloadState::InProgress,
        );

        let matched = m.find_matching_downloads(
            &[failed_transfer("peer_a", "Wanted\\01 - Intro.flac"), theirs],
            &backend,
        );

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].1.source, "peer_a");
        assert_eq!(m.tracked_files[0].source, "peer_a");
        assert!(TransferRegistry::global().may_adopt(
            &key(&backend, "peer_b", "Other Album\\01 - Intro.flac"),
            &batch("Y").owner
        ));
    }

    #[tokio::test]
    async fn a_transfer_of_a_sent_file_made_in_another_request_is_not_adopted() {
        // slskd already had dirB/aaa.flac from peer_b when the failover sent
        // it: that transfer is someone else's download of the same file.
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        sent_to(&mut m, 0, "peer_b", "dirB/aaa.flac", "r1");
        let theirs = made_in(
            "r0",
            transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::InProgress),
        );
        let untagged = transfer("peer_b", "dirB/aaa.flac", "t-c", DownloadState::InProgress);

        let matched = m.find_matching_downloads(&[theirs, untagged], &backend);

        assert!(matched.is_empty());
        assert_eq!(m.tracked_files[0].source, "peer_a");
    }

    #[tokio::test]
    async fn a_send_whose_answer_was_lost_is_adopted_once_the_slot_needs_it() {
        let (mut m, _rx, mut recorded) = failover_monitor();
        let stub = StubBackend::new(vec![
            ScriptedOutcome::TransportError,
            ScriptedOutcome::Enqueued,
        ]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        // peer_b's answer never came, so the slot went on to peer_c.
        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;
        assert_eq!(m.tracked_files[0].source, "peer_c");
        let to_b = stub.requests()[0].clone();
        recorded.events();

        // peer_c fails too, and peer_b turns out to be sending the file.
        let from_b = made_in(
            &to_b,
            listed("peer_b", "dirB/bbb.flac", DownloadState::InProgress),
        );
        assert_eq!(m.adoption_peers(&[]), vec!["peer_b".to_string()]);
        poll_tracks(
            &mut m,
            &[failed_transfer("peer_c", "dirC/ccc.flac"), from_b],
            &backend,
        )
        .await;

        assert_eq!(
            (
                m.tracked_files[0].source.as_str(),
                m.tracked_files[0].transfer_id.as_str()
            ),
            ("peer_b", "t-dirB/bbb.flac")
        );
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
        assert_eq!(m.pool.attempts_used(0), 3, "no source was sent again");
        let attempts: Vec<(Option<String>, Option<String>)> = recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Attempt {
                    candidate, request, ..
                } => Some((candidate.map(|c| c.id), request)),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, vec![(Some("dirB/bbb.flac".into()), Some(to_b))]);
    }

    #[tokio::test]
    async fn an_unowned_transfer_from_another_user_is_not_adopted() {
        // A live transfer the slot's send made that another batch has since
        // claimed has that batch's monitor, whether it is another user's or
        // the same user's.
        for owner in [batch("Theirs"), {
            let mut mine = batch("Mine");
            mine.owner.user_id = UserId::new("tester");
            mine
        }] {
            let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
            m.batch.owner.user_id = UserId::new("tester");
            let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
            sent_to(&mut m, 0, "peer_b", "dirB/aaa.flac", "r1");
            TransferRegistry::global().claim(&[key(&backend, "peer_b", "dirB/aaa.flac")], &owner);
            let theirs = made_in(
                "r1",
                transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::InProgress),
            );

            let matched = m.find_matching_downloads(&[theirs], &backend);

            assert!(matched.is_empty());
            assert_eq!(m.tracked_files[0].source, "peer_a");
            assert_eq!(m.tracked_files[0].transfer_id, id_of("dirA/aaa.flac"));
            assert!(!TransferRegistry::global()
                .may_adopt(&key(&backend, "peer_b", "dirB/aaa.flac"), &m.batch.owner));
        }
    }

    #[tokio::test]
    async fn a_transfer_another_slot_watches_is_not_adopted() {
        // The first slot's send made the transfer the second slot watches:
        // it is the batch's own, but not the first slot's.
        let (mut m, _recorded) = monitor(vec!["peer_a", "peer_b"], vec!["a/01.flac", "b/01.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        sent_to(&mut m, 0, "peer_b", "b/01.flac", "r1");

        let matched = m.find_matching_downloads(
            &[made_in(
                "r1",
                listed("peer_b", "b/01.flac", DownloadState::InProgress),
            )],
            &backend,
        );

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, 1);
        assert_eq!(m.tracked_files[0].source, "peer_a");
    }

    #[tokio::test]
    async fn slots_with_the_same_basename_match_their_own_ids() {
        let (mut m, _recorded) = monitor(
            vec!["peer_a", "peer_a"],
            vec!["music\\A\\01 - One.flac", "music\\B\\01 - One.flac"],
        );
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let listing = vec![
            listed(
                "peer_a",
                "music\\B\\01 - One.flac",
                DownloadState::Completed,
            ),
            listed(
                "peer_a",
                "music\\A\\01 - One.flac",
                DownloadState::InProgress,
            ),
        ];

        let mut matched = m.find_matching_downloads(&listing, &backend);
        matched.sort_by_key(|(slot, _)| *slot);

        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].1.state, DownloadState::InProgress);
        assert_eq!(matched[1].1.state, DownloadState::Completed);
    }

    #[tokio::test]
    async fn a_rebound_slot_follows_its_new_transfer_id() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        sent_to(&mut m, 0, "peer_b", "dirB/aaa.flac", "r1");
        let made = |state| made_in("r1", transfer("peer_b", "dirB/aaa.flac", "t-b", state));
        m.find_matching_downloads(&[made(DownloadState::InProgress)], &backend);

        // The abandoned peer's record shows up again: it is no longer this
        // slot's transfer.
        let matched = m.find_matching_downloads(
            &[
                listed("peer_a", "dirA/aaa.flac", DownloadState::Completed),
                made(DownloadState::Completed),
            ],
            &backend,
        );

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].1.id, "t-b");
    }

    #[tokio::test]
    async fn cleanup_removes_this_batchs_failed_transfers_by_id() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].phase = SlotPhase::Settled;
        m.abandoned.push(("peer_q".to_string(), "t-q".to_string()));
        stub.set_listing(vec![
            failed_transfer("peer_a", "dirA/aaa.flac"),
            transfer("peer_q", "q.flac", "t-q", DownloadState::Cancelled),
            transfer(
                "peer_a",
                "dirA/aaa.flac",
                "t-other",
                DownloadState::Completed,
            ),
            transfer("peer_z", "dirA/aaa.flac", "t-9", DownloadState::Completed),
        ]);

        m.remove_batch_transfers(&backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![
                Cancel::removing("peer_a", &id_of("dirA/aaa.flac")),
                Cancel::removing("peer_q", "t-q"),
            ]
        );
    }

    #[tokio::test]
    async fn cleanup_removes_this_batchs_transfers_left_pending_a_retry() {
        // A transfer cancelled during slskd's retry delay keeps reading as a
        // pending retry for good.
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].phase = SlotPhase::Settled;
        stub.set_listing(vec![pending_retry("peer_a", "dirA/aaa.flac")]);

        m.remove_batch_transfers(&backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::removing("peer_a", &id_of("dirA/aaa.flac"))]
        );
    }

    #[tokio::test]
    async fn a_timed_out_transfer_that_succeeded_late_keeps_its_record() {
        // slskd finished it between the listing and the cancel. Its file may
        // still be on its way out of the incomplete folder.
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].stalled_since = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
        poll_tracks(
            &mut m,
            &[listed("peer_a", "one.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;
        assert_eq!(phase(&m, 0), SlotPhase::TimedOut);

        stub.set_listing(vec![listed("peer_a", "one.flac", DownloadState::Completed)]);
        m.remove_batch_transfers(&backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("one.flac"))],
            "only the timeout's cancel, no removal of the succeeded record"
        );
    }

    #[tokio::test]
    async fn cleanup_leaves_a_finished_transfer_whose_file_was_never_found() {
        // The monitor stopped before handing it off: slskd may not have moved
        // the file yet, and removing the record now would strand it.
        let (m, _recorded) = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        stub.set_listing(vec![listed(
            "peer_a",
            "dirA/aaa.flac",
            DownloadState::Completed,
        )]);

        m.remove_batch_transfers(&backend).await;

        assert!(stub.cancelled().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_backend_eventually_ends_the_monitor() {
        // Every per-track deadline is evaluated from a readable transfer
        // list, so while the list cannot be read at all none of them fire.
        // Before this fuse the loop polled forever, one immortal task per
        // download, which is what grew RSS to gigabytes on a long-lived
        // instance (#78).
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a", "peer_a"], vec!["a.flac", "b.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut invalid = 0;
        let circuit_open = || SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".to_string(),
        };

        assert!(
            m.poll_failed(circuit_open().into(), &mut invalid).is_none(),
            "gave up while slskd was fine"
        );

        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));
        let failure = m
            .poll_failed(circuit_open().into(), &mut invalid)
            .expect("polled a dead backend forever");
        m.stop_early(failure, Some(&backend)).await;

        assert!(
            (0..2).all(|slot| phase(&m, slot) == SlotPhase::Settled),
            "a track was left unresolved, so its slot never reports back"
        );
        let Ok(DownloadEvent::Progress(entries)) = rx.try_recv() else {
            panic!("the UI was never told the batch died");
        };
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_failed(
                e,
                FailureCode::BackendUnreachable,
                "slskd's transfer list was unreadable for 10 minutes",
            );
            let f = e.failure.as_ref().unwrap();
            assert_eq!(f.retry, RetryAdvice::SameSourceLater);
            let cause = f.cause.as_ref().expect("the poll error as cause");
            assert_eq!(cause.status, Some(503));
            assert_eq!(
                cause.raw,
                "Circuit breaker is open - slskd appears to be unavailable"
            );
        }
    }

    /// The API key changed while the batch ran: every poll is refused, and
    /// waiting will not help, a person has to fix the key.
    #[tokio::test]
    async fn a_refused_api_key_ends_the_monitor_asking_for_the_settings() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));

        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let refused = SoulseekError::Api {
            status: 401,
            message: String::new(),
        };
        let failure = m
            .poll_failed(refused.into(), &mut 0)
            .expect("the batch stops");
        m.stop_early(failure, Some(&backend)).await;

        let Ok(DownloadEvent::Progress(entries)) = rx.try_recv() else {
            panic!("the UI was never told the batch died");
        };
        assert_failed(
            &entries[0],
            FailureCode::BackendNotConfigured,
            "slskd's transfer list was unreadable for 10 minutes",
        );
        let f = entries[0].failure.as_ref().unwrap();
        assert_eq!(f.retry, RetryAdvice::NeedsUser);
        assert_eq!(
            f.user_action.as_ref().map(|a| a.code),
            Some(UserActionCode::ConfigureBackend)
        );
        assert_eq!(f.cause.as_ref().and_then(|c| c.status), Some(401));
    }

    #[tokio::test]
    async fn any_backend_answer_that_cannot_be_read_fails_the_batch_after_its_fuse() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        let unreadable = || BackendError::from(StubFault { unreadable: true });
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut invalid = 0;
        for _ in 1..MAX_INVALID_RESPONSES {
            assert!(m.poll_failed(unreadable(), &mut invalid).is_none());
        }
        let failure = m
            .poll_failed(unreadable(), &mut invalid)
            .expect("the fuse burns");
        m.stop_early(failure, Some(&backend)).await;

        let Ok(DownloadEvent::Progress(entries)) = rx.try_recv() else {
            panic!("the UI was never told the batch died");
        };
        assert_failed(
            &entries[0],
            FailureCode::BackendResponseUnreadable,
            "Could not read slskd downloads list: stub answer unreadable",
        );
    }

    #[test]
    fn a_backend_fault_with_a_readable_answer_is_not_fused_by_count() {
        let (tx, _rx) = broadcast::channel(16);
        let (m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        let unreachable = || BackendError::from(StubFault { unreadable: false });
        let mut invalid = 0;
        for _ in 0..MAX_INVALID_RESPONSES {
            assert!(m.poll_failed(unreachable(), &mut invalid).is_none());
        }
        assert_eq!(invalid, 0);
    }

    #[tokio::test]
    async fn rows_a_monitor_makes_for_its_slots_carry_the_backend_of_its_batch() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, mut recorded) = monitor_on(tx, vec!["peer_a"], vec!["01.flac"]);
        m.batch.backend = "b".to_string();
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_b".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02".to_string(),
                alternates: vec![],
                used: 1,
                max_sources: MAX_SOURCES_PER_TRACK,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");

        // The watched slot fails without a listed row, the undrained one is stopped.
        m.fail_unprocessed_tracks(never_appeared("slskd"));
        m.close_inbox();

        let mut sent = Vec::new();
        while let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() {
            sent.extend(rows.into_iter().map(|r| (r.item, r.backend)));
        }
        let b = Some("b".to_string());
        let both = vec![
            ("01.flac".to_string(), b.clone()),
            ("02.flac".to_string(), b),
        ];
        assert_eq!(sent, both);
        assert_eq!(backends_recorded(&mut recorded), both);
    }

    // Paused, so the failed state's confirmation window and the poll interval take no real
    // time.
    #[tokio::test(start_paused = true)]
    async fn a_batch_is_polled_and_cleaned_up_only_on_its_own_backend() {
        let (a, b) = (StubBackend::new(vec![]), StubBackend::new(vec![]));
        b.set_listing(vec![failed_transfer("peer_a", "01.flac")]);
        let backends = StubBackends::default()
            .with("a", a.clone())
            .with("b", b.clone());
        let (tx, _rx) = broadcast::channel(16);
        let (ledger, _recorded) = Ledger::recording();
        let mut m = DownloadMonitor::new(
            vec![watched("peer_a", "01.flac", 0)],
            PathBuf::from("/music"),
            tx,
            batch_on("b", "X"),
            ledger,
        );

        m.run(&backends).await;

        let asked = backends.asked();
        assert!(
            !asked.is_empty() && asked.iter().all(|id| id == "b"),
            "{asked:?}"
        );
        assert!(a.listings().is_empty() && a.cancelled().is_empty());
        assert_eq!(
            b.cancelled(),
            vec![Cancel::removing("peer_a", &id_of("01.flac"))],
            "the ended record is removed where it is"
        );
    }

    #[tokio::test]
    async fn an_unparseable_listing_fails_the_batch_after_its_fuse() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, mut recorded) = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        let drift =
            || SoulseekError::InvalidResponse("JSON parse error: missing field `id`".into());
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut invalid = 0;
        for _ in 1..MAX_INVALID_RESPONSES {
            assert!(m.poll_failed(drift().into(), &mut invalid).is_none());
        }
        let failure = m
            .poll_failed(drift().into(), &mut invalid)
            .expect("the fuse burns");
        m.stop_early(failure, Some(&backend)).await;

        let Ok(DownloadEvent::Progress(entries)) = rx.try_recv() else {
            panic!("the UI was never told the batch died");
        };
        assert_failed(
            &entries[0],
            FailureCode::TransferListUnreadable,
            "Could not read slskd downloads list: Unparseable slskd response: JSON parse error: missing field `id`",
        );
        let f = entries[0].failure.as_ref().unwrap();
        assert_eq!(f.retry, RetryAdvice::NotRetryable);
        assert_eq!(
            f.user_action.as_ref().map(|a| a.code),
            Some(UserActionCode::UpdateSoulfulOrBackend)
        );
        assert_eq!(
            f.cause.as_ref().map(|c| c.raw.as_str()),
            Some("Unparseable slskd response: JSON parse error: missing field `id`")
        );
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![(
                JobState::Failed,
                vec![("a.flac".into(), Some(slot_of_j1(0)))]
            )]
        );
    }

    #[tokio::test]
    async fn a_timed_out_transfer_is_dropped_before_the_retry_starts() {
        // The abandoned peer is still sending at the per-track timeout.
        // Leaving it running would put two peers on one track, and whatever
        // it finally delivers lands under a path no slot maps to any more.
        let (mut m, _rx, _recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].stalled_since = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "dirA/aaa.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("dirA/aaa.flac"))],
            "slskd cancels by transfer id, not by filename"
        );
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
    }

    #[tokio::test]
    async fn a_timed_out_slot_is_cancelled_and_not_resent() {
        for album_mode in [false, true] {
            let (tx, mut rx) = broadcast::channel(64);
            let (mut m, mut recorded) =
                monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
            m.batch.import_mode = match album_mode {
                true => ImportMode::Album,
                false => ImportMode::Singleton,
            };
            let stub = StubBackend::new(vec![]);
            let backend: Arc<dyn DownloadBackend> = stub.clone();
            let held = key(&backend, "peer_a", "one.flac");
            TransferRegistry::global().claim(std::slice::from_ref(&held), &m.batch);
            m.track_states[0].stalled_since =
                Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
            let listing = || {
                vec![
                    listed("peer_a", "one.flac", DownloadState::InProgress),
                    listed("peer_a", "two.flac", DownloadState::InProgress),
                ]
            };

            let mut empty = 0;
            for poll in 1..=2 {
                assert!(
                    !m.process_poll_result(listing(), &mut empty, poll, &backend)
                        .await
                );
            }

            let rows = rows_for(&mut rx, "one.flac");
            assert_eq!(
                rows.iter().map(|r| r.state.clone()).collect::<Vec<_>>(),
                vec![DownloadState::Failed("Per-track timeout".into())],
                "album_mode={album_mode}"
            );
            assert_failed(&rows[0], FailureCode::TransferTimedOut, "Per-track timeout");
            assert!(stub
                .cancelled()
                .contains(&Cancel::only("peer_a", &id_of("one.flac"))));
            assert_eq!(phase(&m, 0), SlotPhase::TimedOut);
            assert!(TransferRegistry::global().may_adopt(&held, &batch("Y").owner));
            let one: Vec<JobState> = slots_recorded(&mut recorded)
                .into_iter()
                .filter(|(_, rows)| rows.iter().any(|(item, _)| item == "one.flac"))
                .map(|(state, rows)| {
                    assert_eq!(rows, vec![("one.flac".into(), Some(slot_of_j1(0)))]);
                    state
                })
                .collect();
            assert_eq!(one, vec![JobState::Failed], "album_mode={album_mode}");
        }
    }

    #[tokio::test]
    async fn a_slot_queued_behind_a_file_its_peer_sends_is_not_timed_out() {
        // The peer sends one file at a time: two.flac has waited in its
        // queue for over an hour while the tracks before it came in.
        let (mut m, _recorded) = monitor(vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[1].seen = true;
        m.track_states[1].stalled_since = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));

        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "one.flac", DownloadState::InProgress),
                at_source("peer_a", "two.flac"),
            ],
            &backend,
        )
        .await;

        assert_eq!(phase(&m, 1), SlotPhase::Watching);
        assert!(stub.cancelled().is_empty());
        assert!(m.track_states[1]
            .stalled_since
            .is_some_and(|since| since.elapsed() < PER_TRACK_TIMEOUT));
    }

    #[tokio::test]
    async fn a_transfer_is_timed_out_only_once_it_stops_moving() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let at = |transferred| DownloadProgress {
            transferred,
            ..listed("peer_a", "one.flac", DownloadState::InProgress)
        };
        m.track_states[0].transferred = 1_000;

        // A slow peer, still sending over an hour after the transfer started.
        m.track_states[0].stalled_since = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
        poll_tracks(&mut m, &[at(2_000)], &backend).await;
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
        assert!(stub.cancelled().is_empty());

        // An hour without a byte since.
        m.track_states[0].stalled_since = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
        poll_tracks(&mut m, &[at(2_000)], &backend).await;
        assert_eq!(phase(&m, 0), SlotPhase::TimedOut);
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("one.flac"))]
        );
    }

    #[tokio::test]
    async fn a_slot_waiting_in_the_source_queue_too_long_fails_over() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].seen = true;
        m.track_states[0].queued_at_source_since =
            Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(&mut m, &[at_source("peer_a", "dirA/aaa.flac")], &backend).await;

        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("dirA/aaa.flac"))]
        );
    }

    #[tokio::test]
    async fn a_slot_with_no_source_left_fails_stuck_in_remote_queue() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].seen = true;
        m.track_states[0].queued_at_source_since =
            Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(&mut m, &[at_source("peer_a", "one.flac")], &backend).await;

        let rows = rows_for(&mut rx, "one.flac");
        assert_failed(
            &rows[0],
            FailureCode::TransferStuckInRemoteQueue,
            "Waited 15 minutes in the source's queue",
        );
        assert!(!rows[0].queued_at_source);
        assert_eq!(phase(&m, 0), SlotPhase::TimedOut);
    }

    #[tokio::test]
    async fn a_slot_queued_locally_is_not_held_to_the_remote_queue_limit() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].seen = true;
        m.track_states[0].queued_at_source_since =
            Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "one.flac", DownloadState::Queued)],
            &backend,
        )
        .await;

        assert_eq!(m.track_states[0].queued_at_source_since, None);
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
        assert!(stub.cancelled().is_empty());
        assert_eq!(states_for(&mut rx, "one.flac"), vec![DownloadState::Queued]);
    }

    #[tokio::test]
    async fn a_slot_waiting_behind_the_batchs_own_file_from_its_peer_keeps_waiting() {
        // The peer sends this batch one file at a time and answers every
        // request with Queued, so two.flac waits while one.flac comes in.
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) =
            monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
        // A finished file stays watched until the album is done.
        m.batch.import_mode = ImportMode::Album;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let past_the_limit = || Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));
        let with_one = |state| {
            [
                listed("peer_a", "one.flac", state),
                at_source("peer_a", "two.flac"),
            ]
        };

        m.track_states[1].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_one(DownloadState::InProgress), &backend).await;
        assert_eq!(m.track_states[1].queued_at_source_since, None);
        assert_eq!(phase(&m, 1), SlotPhase::Watching);

        // one.flac finished since the last poll.
        m.track_states[1].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_one(DownloadState::Completed), &backend).await;
        assert_eq!(m.track_states[1].queued_at_source_since, None);
        assert_eq!(phase(&m, 1), SlotPhase::Watching);

        // Since then the peer has sent this batch nothing.
        m.track_states[1].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_one(DownloadState::Completed), &backend).await;
        assert_eq!(phase(&m, 1), SlotPhase::TimedOut);
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("two.flac"))]
        );
        assert_failed(
            rows_for(&mut rx, "two.flac").last().unwrap(),
            FailureCode::TransferStuckInRemoteQueue,
            "Waited 15 minutes in the source's queue",
        );
    }

    #[tokio::test]
    async fn a_slot_waiting_behind_another_batchs_file_from_its_peer_keeps_waiting() {
        // One slskd account downloads for every batch, and the peer sends it
        // one file at a time: other.flac is no slot's here, and one.flac
        // waits behind it all the same.
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let past_the_limit = || Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));
        let with_other = |state| {
            [
                at_source("peer_a", "one.flac"),
                listed("peer_a", "other.flac", state),
            ]
        };

        m.track_states[0].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_other(DownloadState::InProgress), &backend).await;
        assert_eq!(m.track_states[0].queued_at_source_since, None);
        assert_eq!(phase(&m, 0), SlotPhase::Watching);

        // other.flac finished since the last poll.
        m.track_states[0].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_other(DownloadState::Completed), &backend).await;
        assert_eq!(m.track_states[0].queued_at_source_since, None);
        assert_eq!(phase(&m, 0), SlotPhase::Watching);

        // Since then the peer has sent this slskd nothing.
        m.track_states[0].queued_at_source_since = past_the_limit();
        poll_tracks(&mut m, &with_other(DownloadState::Completed), &backend).await;
        assert_eq!(phase(&m, 0), SlotPhase::TimedOut);
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("one.flac"))]
        );
        let row = rows_for(&mut rx, "one.flac").pop().unwrap();
        let stuck = stuck_in_remote_queue();
        assert_failed(&row, stuck.code, &stuck.message);
    }

    #[tokio::test]
    async fn a_poll_that_does_not_list_a_slot_stops_its_remote_queue_clock() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].seen = true;
        m.track_states[0].queued_at_source_since = Some(ago(Duration::from_secs(60)));

        poll_tracks(&mut m, &[], &backend).await;

        assert_eq!(m.track_states[0].queued_at_source_since, None);
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
    }

    #[tokio::test]
    async fn the_remote_queue_clock_starts_at_the_first_sighting() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        poll_tracks(&mut m, &[at_source("peer_a", "one.flac")], &backend).await;
        assert!(m.track_states[0].queued_at_source_since.is_some());
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
    }

    #[test]
    fn a_pending_retry_reads_as_its_failure_only_30_seconds_after_it_was_first_seen() {
        let row = pending_retry("peer_a", "one.flac");
        let mut since = None;

        let first = read_pending_retry(&row, &mut since);
        assert_eq!(first, row);
        assert!(since.is_some());

        since = Some(ago(RETRY_PENDING_FOR));
        let held = read_pending_retry(&row, &mut since);
        assert_failed(
            &held,
            FailureCode::TransferConnectionFailed,
            "Connection reset by peer",
        );
        assert_eq!(held.pending_retry, None);

        let live = listed("peer_a", "one.flac", DownloadState::Queued);
        assert_eq!(read_pending_retry(&live, &mut since), live);
        assert_eq!(since, None);
    }

    #[tokio::test]
    async fn a_pending_retry_held_past_its_bound_fails_over() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        m.track_states[0].retry_pending_since = Some(ago(RETRY_PENDING_FOR));
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[pending_retry("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        assert_eq!(m.tracked_files[0].source, "peer_b");
    }

    #[tokio::test]
    async fn another_peers_record_pending_a_retry_is_not_adopted() {
        // No watcher's clock bounds a record no slot watches, so one
        // cancelled during slskd's retry delay would read Queued for good,
        // even one a send of the slot made.
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        sent_to(&mut m, 0, "peer_x", "dirX/aaa.flac", "r1");

        poll_tracks(
            &mut m,
            &[
                failed_transfer("peer_a", "dirA/aaa.flac"),
                made_in("r1", pending_retry("peer_x", "dirX/aaa.flac")),
            ],
            &backend,
        )
        .await;

        assert_eq!(m.pool.attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(m.tracked_files[0].transfer_id, "t-dirB/bbb.flac");
    }

    #[tokio::test]
    async fn the_absence_window_restarts_on_every_rebind() {
        // The same guarantee stated directly: a slot that has just been
        // rebound is young, however old the batch is.
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        m.track_states[0].bound_at = ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1));

        // Nothing for this track anywhere in slskd's list.
        m.handle_absent_tracks(&[], &backend).await;
        assert_eq!(m.pool.attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");

        // The rebind reset the window, so the next sweep leaves it alone.
        m.handle_absent_tracks(&[], &backend).await;
        assert_eq!(m.pool.attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");
    }

    #[tokio::test]
    async fn a_rebind_replaces_the_abandoned_peers_row_instead_of_adding_one() {
        // The panel keys rows by remote path and each peer serves the track
        // under its own, so a plain progress update would leave the dead
        // peer's row beside the retry's: one track, two rows.
        let (mut m, mut rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        match rx.try_recv().expect("an event for the retry") {
            DownloadEvent::Replaced {
                previous_item,
                entry,
            } => {
                assert_eq!(previous_item, "dirA/aaa.flac");
                assert_eq!(entry.item, "dirB/bbb.flac");
                assert_eq!(entry.source, "peer_b");
            }
            other => panic!("expected a row replacement, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rebound_slots_rows_carry_the_backend_of_its_batch() {
        let (mut m, mut rx, mut recorded) = failover_monitor();
        m.batch.backend = "b".to_string();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[failed_transfer("peer_a", "dirA/aaa.flac")],
            &backend,
        )
        .await;

        match rx.try_recv().expect("an event for the retry") {
            DownloadEvent::Replaced { entry, .. } => {
                assert_eq!(
                    (entry.item.as_str(), entry.backend.as_deref()),
                    ("dirB/bbb.flac", Some("b"))
                );
            }
            other => panic!("expected a row replacement, got {other:?}"),
        }
        let b = Some("b".to_string());
        assert_eq!(
            backends_recorded(&mut recorded),
            vec![
                ("dirA/aaa.flac".to_string(), b.clone()),
                ("dirB/bbb.flac".to_string(), b)
            ]
        );
    }

    #[tokio::test]
    async fn a_single_track_batch_that_never_appears_fails_over() {
        // A lone track has no sibling to keep the batch non-empty, so
        // process_tracks and handle_absent_tracks never see it: the empty
        // poll fuse is the only place its failover can fire.
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let mut consecutive_empty = MAX_CONSECUTIVE_EMPTY - 1;

        let stop = m
            .process_poll_result(vec![], &mut consecutive_empty, 99, &backend)
            .await;

        assert!(!stop, "the batch keeps polling for the new peer");
        assert_eq!(consecutive_empty, 0, "the grace window restarts");
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
    }

    #[tokio::test]
    async fn a_never_appeared_batch_with_no_peers_left_is_still_written_off() {
        // The fallback has to stay reachable, or an absent batch with a dry
        // pool would poll forever.
        let (mut m, mut rx, mut recorded) = failover_monitor();
        // Every alternate is refused, so the pool yields nothing.
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut consecutive_empty = MAX_CONSECUTIVE_EMPTY - 1;

        let stop = m
            .process_poll_result(vec![], &mut consecutive_empty, 99, &backend)
            .await;

        assert!(stop);
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        let mut rows = Vec::new();
        while let Ok(DownloadEvent::Progress(sent)) = rx.try_recv() {
            rows.extend(sent);
        }
        assert_eq!(rows.len(), 1);
        assert_failed(
            &rows[0],
            FailureCode::TransferNeverAppeared,
            "Download never appeared in slskd",
        );
        // The row it left is recorded before its alternates are refused, the
        // slot still downloading, then the slot fails.
        let left = vec![("dirA/aaa.flac".into(), Some(slot_of_j1(0)))];
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![
                (JobState::Downloading, left.clone()),
                (JobState::Failed, left)
            ]
        );
    }

    #[tokio::test]
    async fn a_vanished_transfer_fails_with_transfer_disappeared() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].seen = true;
        m.track_states[0].missing_since = Some(ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(&mut m, &[], &backend).await;

        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("the vanished transfer was never reported");
        };
        assert_eq!(rows.len(), 1);
        assert_failed(
            &rows[0],
            FailureCode::TransferDisappeared,
            "Download disappeared from slskd",
        );
    }

    #[tokio::test]
    async fn a_batch_on_another_backend_names_that_backend_when_its_track_never_appears() {
        let (tx, mut rx) = broadcast::channel(16);
        let (ledger, _recorded) = Ledger::recording();
        let mut m = DownloadMonitor::new(
            vec![
                watched("peer_a", "one.flac", 0),
                watched("peer_a", "two.flac", 1),
            ],
            PathBuf::from("/music"),
            tx,
            batch_on("stub-1", "X"),
            ledger,
        );
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        // Its sibling is listed, so the batch is not empty and the absent track is judged alone.
        m.track_states[0].bound_at = ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "two.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;

        let mut rows = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DownloadEvent::Progress(sent) = event {
                rows.extend(sent.into_iter().filter(|r| r.failure.is_some()));
            }
        }
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_failed(
            &rows[0],
            FailureCode::TransferNeverAppeared,
            "Download never appeared in stub-1",
        );
    }

    #[tokio::test]
    async fn a_batch_on_another_backend_names_that_backend_when_its_track_disappears() {
        let (tx, mut rx) = broadcast::channel(16);
        let (ledger, _recorded) = Ledger::recording();
        let mut m = DownloadMonitor::new(
            vec![watched("peer_a", "one.flac", 0)],
            PathBuf::from("/music"),
            tx,
            batch_on("stub-1", "X"),
            ledger,
        );
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].seen = true;
        m.track_states[0].missing_since = Some(ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1)));

        poll_tracks(&mut m, &[], &backend).await;

        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("the vanished transfer was never reported");
        };
        assert_failed(
            &rows[0],
            FailureCode::TransferDisappeared,
            "Download disappeared from stub-1",
        );
    }

    // Paused, so the polls between lookups take no real time.
    #[tokio::test(start_paused = true)]
    async fn a_batch_on_a_backend_turned_off_fails_saying_so() {
        let backends = StubBackends::default();
        backends.fail(
            "archive",
            BackendLookupError::Disabled("Internet Archive".into()),
        );
        let (tx, mut rx) = broadcast::channel(16);
        let (ledger, _recorded) = Ledger::recording();
        let mut m = DownloadMonitor::new(
            vec![watched("peer_a", "01.flac", 0)],
            PathBuf::from("/music"),
            tx,
            batch_on("archive", "X"),
            ledger,
        );

        m.run(&backends).await;

        // Each poll's lookup, then the cleanup's.
        assert_eq!(
            backends.asked(),
            vec!["archive".to_string(); MAX_BACKEND_FAILURES as usize + 1]
        );
        let mut rows = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DownloadEvent::Progress(sent) = event {
                rows.extend(sent);
            }
        }
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_failed(
            &rows[0],
            FailureCode::BackendNotConfigured,
            "Internet Archive is turned off in Settings > Config > Connections",
        );
        let action = rows[0]
            .failure
            .as_ref()
            .unwrap()
            .user_action
            .clone()
            .unwrap();
        assert_eq!(action.what, "Turn on Internet Archive");
    }

    #[test]
    fn monitor_failures_name_the_service_of_the_batchs_backend() {
        let e: BackendError = StubFault { unreadable: true }.into();
        for (f, message) in [
            (
                never_appeared("archive.org"),
                "Download never appeared in archive.org",
            ),
            (
                disappeared("archive.org"),
                "Download disappeared from archive.org",
            ),
            (
                stalled(&e, "AirDC++"),
                "AirDC++'s transfer list was unreadable for 10 minutes",
            ),
            (
                list_unreadable(&e, "AirDC++"),
                "Could not read AirDC++ downloads list: stub answer unreadable",
            ),
        ] {
            assert_eq!(f.message, message);
        }
    }

    #[test]
    fn monitor_failures_keep_their_text() {
        use RetryAdvice::*;
        let e: BackendError = SoulseekError::InvalidResponse("bad json".into()).into();
        let unreadable = format!("Could not read slskd downloads list: {e}");
        for (f, code, retry, message) in [
            (
                never_appeared("slskd"),
                FailureCode::TransferNeverAppeared,
                OtherSource,
                "Download never appeared in slskd",
            ),
            (
                disappeared("slskd"),
                FailureCode::TransferDisappeared,
                SameSourceLater,
                "Download disappeared from slskd",
            ),
            (
                per_track_timeout(),
                FailureCode::TransferTimedOut,
                OtherSource,
                "Per-track timeout",
            ),
            (
                stuck_in_remote_queue(),
                FailureCode::TransferStuckInRemoteQueue,
                OtherSource,
                "Waited 15 minutes in the source's queue",
            ),
            (
                stalled(&e, "slskd"),
                FailureCode::TransferListUnreadable,
                NotRetryable,
                "slskd's transfer list was unreadable for 10 minutes",
            ),
            (
                backend_unavailable(&unset_key()),
                FailureCode::BackendNotConfigured,
                NeedsUser,
                "Download backend unavailable",
            ),
            (
                list_unreadable(&e, "slskd"),
                FailureCode::TransferListUnreadable,
                NotRetryable,
                unreadable.as_str(),
            ),
            (
                monitor_stopped(),
                FailureCode::TransferCancelled,
                SameSourceLater,
                MONITOR_STOPPED,
            ),
            (
                cancelled_on_request(),
                FailureCode::TransferCancelled,
                NotRetryable,
                "Cancelled on request",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, Stage::Transfer, retry, message)
            );
        }
    }

    fn unset_key() -> BackendLookupError {
        BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::ApiKey).into())
    }

    /// The lookup error is the cause, and the backend says what to set.
    #[test]
    fn an_unavailable_backend_keeps_why_it_was_unavailable() {
        let f = backend_unavailable(&unset_key());
        assert_eq!(
            f.user_action.map(|a| a.code),
            Some(UserActionCode::ConfigureBackend)
        );
        assert_eq!(
            f.cause.map(|c| c.raw).as_deref(),
            Some("slskd API key not configured")
        );

        let locked = BackendLookupError::Settings("database is locked".into());
        let f = backend_unavailable(&locked);
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::Transfer,
                RetryAdvice::SameSourceLater,
                "Download backend unavailable"
            )
        );
        let cause = f.cause.unwrap();
        assert_eq!(
            (cause.service, cause.raw.as_str()),
            (Service::Database, "database is locked")
        );
        assert_eq!(f.user_action, None);
    }

    #[tokio::test]
    async fn a_failed_row_is_only_shown_once_its_key_is_free() {
        // A retry from a failed row re-requests the same file, and the
        // registry refuses it while this batch still holds the key.
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["dirA/fail.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let own = key(&backend, "peer_a", "dirA/fail.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&own), &m.batch);
        let other = batch("Y");
        let listing = [failed_transfer("peer_a", "dirA/fail.flac")];

        poll_tracks(&mut m, &listing, &backend).await;
        assert_eq!(states_for(&mut rx, "dirA/fail.flac"), vec![]);
        assert!(!TransferRegistry::global().may_adopt(&own, &other.owner));
        assert_eq!(phase(&m, 0), SlotPhase::Watching);

        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        poll_tracks(&mut m, &listing, &backend).await;
        assert_eq!(
            states_for(&mut rx, "dirA/fail.flac"),
            vec![DownloadState::Failed("peer went away".into())]
        );
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert!(TransferRegistry::global().may_adopt(&own, &other.owner));
    }

    #[tokio::test]
    async fn a_cancelled_transfer_settles_without_failing_over() {
        let (mut m, _rx, _recorded) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "dirA/aaa.flac", DownloadState::Cancelled)],
            &backend,
        )
        .await;

        assert_eq!(m.pool.attempts_used(0), 1);
        assert_eq!(m.tracked_files[0].source, "peer_a");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
    }

    #[tokio::test]
    async fn a_cancelled_slot_is_stopped_once_and_never_failed_over() {
        let (mut m, mut rx, mut recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let own = key(&backend, "peer_a", "dirA/aaa.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&own), &m.batch);
        assert!(TransferRegistry::global().request_cancel(&own, &m.batch.owner.user_id));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "dirA/aaa.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", &id_of("dirA/aaa.flac"))]
        );
        assert!(stub.destinations().is_empty(), "no other source was asked");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        let rows = rows_for(&mut rx, "dirA/aaa.flac");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, DownloadState::Cancelled);
        assert_eq!(
            rows[0]
                .failure
                .as_ref()
                .map(|f| (f.code, f.message.as_str())),
            Some((FailureCode::TransferCancelled, "Cancelled on request"))
        );
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![(
                JobState::Cancelled,
                vec![("dirA/aaa.flac".into(), Some(slot_of_j1(0)))]
            )]
        );
        assert!(TransferRegistry::global().may_adopt(&own, &batch("Y").owner));
    }

    #[tokio::test]
    async fn a_cancel_that_arrives_during_failover_follows_the_slot() {
        let (mut m, mut rx, _recorded) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let registry = TransferRegistry::global();
        let first = key(&backend, "peer_a", "dirA/aaa.flac");
        registry.claim(std::slice::from_ref(&first), &m.batch);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        let user = m.batch.owner.user_id.clone();
        let (reached, release) = stub.hold_downloads();
        let cancel = async {
            reached.notified().await; // the failover's enqueue is in flight
            assert!(registry.request_cancel(&first, &user));
            release.notify_one();
        };

        let failed = [failed_transfer("peer_a", "dirA/aaa.flac")];
        tokio::join!(poll_tracks(&mut m, &failed, &backend), cancel);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        poll_tracks(
            &mut m,
            &[listed("peer_b", "dirB/bbb.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;

        assert!(stub
            .cancelled()
            .contains(&Cancel::only("peer_b", &id_of("dirB/bbb.flac"))));
        assert_eq!(m.pool.attempts_used(0), 2, "no third source");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert_eq!(
            rows_for(&mut rx, "dirB/bbb.flac")
                .last()
                .map(|r| r.state.clone()),
            Some(DownloadState::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_cancel_marked_while_a_poll_writes_the_slot_off_ends_cancelled() {
        let registry = TransferRegistry::global();
        let cancelled = |rx: &mut broadcast::Receiver<DownloadEvent>| {
            let rows = rows_for(rx, "one.flac");
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].state, DownloadState::Cancelled);
            assert_eq!(
                rows[0].failure.as_ref().map(|f| f.code),
                Some(FailureCode::TransferCancelled)
            );
        };
        let one_cancelled = (
            JobState::Cancelled,
            vec![("one.flac".to_string(), Some(slot_of_j1(0)))],
        );

        // Its transfer failed and no other source is left.
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, mut recorded) = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let own = key(&backend, "peer_a", "one.flac");
        registry.claim(std::slice::from_ref(&own), &m.batch);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        let listing = [failed_transfer("peer_a", "one.flac")];
        m.apply_cancels(&listing, &backend).await;
        assert!(registry.request_cancel(&own, &m.batch.owner.user_id));
        let matched = m.find_matching_downloads(&listing, &backend);
        let sending = m.peers_sending(&listing);
        m.process_tracks(&matched, &sending, &backend).await;
        cancelled(&mut rx);
        assert_eq!(slots_recorded(&mut recorded), vec![one_cancelled.clone()]);
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert!(registry.may_adopt(&own, &batch("Y").owner));

        // It has been missing from slskd's list too long, and so has a slot
        // nobody cancelled, which is written off with it.
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, mut recorded) =
            monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
        let own = key(&backend, "peer_a", "one.flac");
        registry.claim(std::slice::from_ref(&own), &m.batch);
        for slot in 0..2 {
            m.track_states[slot].seen = true;
            m.track_states[slot].missing_since =
                Some(ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1)));
        }
        m.apply_cancels(&[], &backend).await;
        assert!(registry.request_cancel(&own, &m.batch.owner.user_id));
        m.handle_absent_tracks(&[], &backend).await;
        cancelled(&mut rx);
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![
                one_cancelled,
                (
                    JobState::Failed,
                    vec![("two.flac".to_string(), Some(slot_of_j1(1)))]
                ),
            ]
        );
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
    }

    #[tokio::test]
    async fn a_job_cancelled_before_its_transfers_exist_is_stopped_at_the_first_poll() {
        let (tx, _rx) = broadcast::channel(16);
        let job = uuid::Uuid::new_v4().to_string();
        TransferRegistry::global().cancel_job(&job);
        let (mut m, mut recorded) = monitor_for_job(tx, &job, vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        poll_tracks(
            &mut m,
            &[listed("peer_a", "one.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;
        assert_eq!(stub.cancelled(), vec![Cancel::only("peer_a", "t-one.flac")]);
        assert!(recorded.events().iter().any(|e| matches!(
            e,
            JobEvent::Slots {
                state: JobState::Cancelled,
                ..
            }
        )));
        TransferRegistry::global().reopen_job(&job);
    }

    #[tokio::test]
    async fn a_reopened_job_is_not_stopped() {
        let (tx, _rx) = broadcast::channel(16);
        let job = uuid::Uuid::new_v4().to_string();
        TransferRegistry::global().cancel_job(&job);
        TransferRegistry::global().reopen_job(&job);
        let (mut m, _) = monitor_for_job(tx, &job, vec!["peer_b"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        poll_tracks(
            &mut m,
            &[listed("peer_b", "one.flac", DownloadState::InProgress)],
            &backend,
        )
        .await;
        assert!(stub.cancelled().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_during_failover_ends_cancelled() {
        let (tx, mut rx) = broadcast::channel(16);
        let job = uuid::Uuid::new_v4().to_string();
        let (m, mut recorded) = monitor_for_job(tx, &job, vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let mut m = m.with_failover(SourcePool::single(vec![
            item("peer_a", "Wake Up", "dirA/aaa.flac"),
            item("peer_b", "Wake Up", "dirB/bbb.flac"),
        ]));
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        let listing = [failed_transfer("peer_a", "dirA/aaa.flac")];
        m.apply_cancels(&listing, &backend).await;
        // The job is cancelled after this poll's apply_cancels, as its transfer fails.
        TransferRegistry::global().cancel_job(&job);
        let matched = m.find_matching_downloads(&listing, &backend);
        let sending = m.peers_sending(&listing);
        m.process_tracks(&matched, &sending, &backend).await;
        assert!(stub.destinations().is_empty(), "no other source was asked");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert_eq!(
            slots_recorded(&mut recorded).last(),
            Some(&(
                JobState::Cancelled,
                vec![(
                    "dirA/aaa.flac".to_string(),
                    Some(JobSlot {
                        job: job.clone(),
                        slot: 0
                    })
                )]
            ))
        );
        assert_eq!(
            rows_for(&mut rx, "dirA/aaa.flac")
                .last()
                .map(|r| r.state.clone()),
            Some(DownloadState::Cancelled)
        );
        TransferRegistry::global().reopen_job(&job);
    }

    #[tokio::test]
    async fn a_finished_transfer_is_imported_even_when_cancelled() {
        let user = UserId::new("monitor-cancel-finished");
        let (mut m, _base, _rx, _recorded) =
            disk_monitor(&user, &["one.flac"], &["one.flac"]).await;
        m.batch.import_mode = ImportMode::Singleton;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let own = key(&backend, "peer_a", "one.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&own), &m.batch);
        assert!(TransferRegistry::global().request_cancel(&own, &user));

        poll_tracks(
            &mut m,
            &[listed("peer_a", "one.flac", DownloadState::Completed)],
            &backend,
        )
        .await;
        m.join_handoffs().await;

        assert_eq!(phase(&m, 0), SlotPhase::HandedOff);
        assert!(!stub
            .cancelled()
            .contains(&Cancel::only("peer_a", &id_of("one.flac"))));
    }

    /// A monitor of `user` watching `watched` from peer_a, whose finished
    /// files are found in a temporary download folder holding `on_disk`, and
    /// whose import target cannot be created, so a handed-off row comes
    /// straight back from the import queue as Failed on the user's channel
    /// without beets ever running.
    async fn disk_monitor(
        user: &UserId,
        watched: &[&str],
        on_disk: &[&str],
    ) -> (
        DownloadMonitor,
        tempfile::TempDir,
        broadcast::Receiver<DownloadEvent>,
        Recorded,
    ) {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("not-a-dir"), b"x").unwrap();
        let (tx, _rx) = broadcast::channel(64);
        let (mut m, recorded) = monitor_on(tx, vec!["peer_a"; watched.len()], watched.to_vec());
        m.batch.import_mode = ImportMode::Album;
        m.download_base = base.path().to_path_buf();
        m.target_path = base.path().join("not-a-dir").join("library");
        let folder = base.path().join(&m.batch.destination);
        std::fs::create_dir_all(&folder).unwrap();
        for file in on_disk {
            std::fs::write(folder.join(file), b"x").unwrap();
        }
        let user_tx = crate::globals::get_or_create_user_channel(user).await;
        m.batch.owner.user_id = user.clone();
        (m, base, user_tx.subscribe(), recorded)
    }

    /// The rows of each Progress event the import queue has sent so far.
    fn import_events(rx: &mut broadcast::Receiver<DownloadEvent>) -> Vec<Vec<DownloadProgress>> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DownloadEvent::Progress(rows) = event {
                events.push(rows);
            }
        }
        events
    }

    /// The items of every row the import queue has sent back so far.
    fn handed_off(rx: &mut broadcast::Receiver<DownloadEvent>) -> Vec<String> {
        let mut items: Vec<String> = import_events(rx)
            .into_iter()
            .flatten()
            .map(|r| r.item)
            .collect();
        items.sort();
        items
    }

    #[tokio::test]
    async fn a_handed_off_slot_is_not_resent() {
        let (tx, mut rx) = broadcast::channel(64);
        let (mut m, _base, _imports, _recorded) = disk_monitor(
            &UserId::new("monitor-handed-off"),
            &["one.flac", "two.flac"],
            &["one.flac"],
        )
        .await;
        m.tx = tx;
        m.batch.import_mode = ImportMode::Singleton;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let listing = || {
            vec![
                listed("peer_a", "one.flac", DownloadState::Completed),
                listed("peer_a", "two.flac", DownloadState::InProgress),
            ]
        };

        let mut empty = 0;
        assert!(
            !m.process_poll_result(listing(), &mut empty, 1, &backend)
                .await
        );
        assert!(
            !m.process_poll_result(listing(), &mut empty, 2, &backend)
                .await
        );
        m.join_handoffs().await;

        assert_eq!(
            states_for(&mut rx, "one.flac"),
            vec![DownloadState::Completed],
            "the import queue owns the row once it is handed off"
        );
        assert_eq!(phase(&m, 0), SlotPhase::HandedOff);
    }

    #[tokio::test]
    async fn a_singleton_handoff_removes_the_record_only_once_the_file_is_found() {
        let (mut m, base, _imports, _recorded) = disk_monitor(
            &UserId::new("monitor-singleton-on-disk"),
            &["one.flac"],
            &[],
        )
        .await;
        m.batch.import_mode = ImportMode::Singleton;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let held = key(&backend, "peer_a", "one.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&held), &m.batch);

        let mut empty = 0;
        m.process_poll_result(
            vec![listed("peer_a", "one.flac", DownloadState::Completed)],
            &mut empty,
            1,
            &backend,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            stub.cancelled().is_empty(),
            "removed before the file was found"
        );
        assert!(!TransferRegistry::global().may_adopt(&held, &batch("Y").owner));

        std::fs::write(
            base.path().join(&m.batch.destination).join("one.flac"),
            b"x",
        )
        .unwrap();
        m.join_handoffs().await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::removing("peer_a", &id_of("one.flac"))]
        );
        assert!(
            TransferRegistry::global().may_adopt(&held, &batch("Y").owner),
            "the key is freed once its import is done"
        );
    }

    #[tokio::test]
    async fn album_mode_deletes_nothing_until_every_file_is_on_disk() {
        let (mut m, base, mut imports, _recorded) = disk_monitor(
            &UserId::new("monitor-album-on-disk"),
            &["01.flac", "02.flac"],
            &["01.flac"],
        )
        .await;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let folder = base.path().join(&m.batch.destination);
        let listing = vec![
            listed("peer_a", "01.flac", DownloadState::Completed),
            listed("peer_a", "02.flac", DownloadState::Completed),
        ];

        let poll = tokio::spawn(async move {
            let mut empty = 0;
            let done = m
                .process_poll_result(listing, &mut empty, 1, &backend)
                .await;
            (m, done)
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            stub.cancelled().is_empty(),
            "a record was removed before every file was on disk"
        );

        std::fs::write(folder.join("02.flac"), b"x").unwrap();
        let (m, done) = poll.await.unwrap();

        assert!(done);
        let mut cancelled = stub.cancelled();
        cancelled.sort();
        assert_eq!(
            cancelled,
            vec![
                Cancel::removing("peer_a", &id_of("01.flac")),
                Cancel::removing("peer_a", &id_of("02.flac")),
            ]
        );
        let jobs: Vec<Vec<String>> = import_events(&mut imports)
            .into_iter()
            .map(|rows| rows.into_iter().map(|r| r.item).collect())
            .collect();
        assert_eq!(
            jobs,
            vec![vec!["01.flac".to_string(), "02.flac".to_string()]],
            "one album job"
        );
        assert!((0..2).all(|slot| phase(&m, slot) == SlotPhase::HandedOff));
    }

    /// `file` of slot `slot` of job j1, found in `m`'s batch folder under
    /// `base` before `m` watched anything, its record gone from slskd.
    fn found_in(m: &DownloadMonitor, base: &Path, file: &str, slot: u32) -> ResolvedSlot {
        ResolvedSlot {
            row: listed("peer_a", file, DownloadState::Completed)
                .with_batch(m.batch.owner.batch_id.clone(), m.batch.label.clone())
                .with_job(slot_of_j1(slot)),
            path: base.join(&m.batch.destination).join(file),
            destination: Some(m.batch.destination.clone()),
        }
    }

    #[tokio::test]
    async fn an_album_file_found_before_watching_is_imported_once_with_the_rest() {
        let (m, base, mut imports, mut recorded) = disk_monitor(
            &UserId::new("monitor-album-found"),
            &["02.flac"],
            &["01.flac"],
        )
        .await;
        let folder = base.path().join(&m.batch.destination);
        // Resumed with 01 already on disk, its record gone.
        let one = found_in(&m, base.path(), "01.flac", 1);
        let mut m = m.with_found(vec![one]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

        let mut empty = 0;
        let running = vec![listed("peer_a", "02.flac", DownloadState::InProgress)];
        assert!(
            !m.process_poll_result(running, &mut empty, 1, &backend)
                .await
        );
        assert!(
            import_events(&mut imports).is_empty(),
            "imported while 02 still downloads"
        );

        std::fs::write(folder.join("02.flac"), b"x").unwrap();
        let done = vec![listed("peer_a", "02.flac", DownloadState::Completed)];
        assert!(m.process_poll_result(done, &mut empty, 2, &backend).await);

        let events = import_events(&mut imports);
        assert_eq!(
            events
                .iter()
                .map(|rows| rows.iter().map(|r| r.item.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![vec!["01.flac".to_string(), "02.flac".to_string()]],
            "one album job"
        );
        assert!(
            !events.iter().flatten().any(|r| r
                .failure
                .as_ref()
                .is_some_and(|f| f.code == FailureCode::FileNotFoundAfterDownload)),
            "{events:?}"
        );
        assert!(slots_recorded(&mut recorded)
            .iter()
            .all(|(state, _)| *state != JobState::Failed));
    }

    /// Each recorded `Slots` row: its state, item and failure code.
    fn recorded_rows(recorded: &mut Recorded) -> Vec<(JobState, String, Option<FailureCode>)> {
        recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots { state, rows } => Some(
                    rows.into_iter()
                        .map(move |r| (state, r.item, r.failure.map(|f| f.code))),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// slskd answering that it is unavailable.
    fn slskd_unavailable() -> SoulseekError {
        SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".to_string(),
        }
    }

    /// The items of each import job the import queue has sent back so far.
    fn import_jobs(rx: &mut broadcast::Receiver<DownloadEvent>) -> Vec<Vec<String>> {
        import_events(rx)
            .into_iter()
            .map(|rows| rows.into_iter().map(|r| r.item).collect())
            .collect()
    }

    #[tokio::test]
    async fn a_resumed_album_cut_off_from_slskd_imports_its_found_files_and_fails_the_rest() {
        let (m, base, mut imports, mut recorded) = disk_monitor(
            &UserId::new("monitor-album-found-stalled"),
            &["02.flac"],
            &["01.flac"],
        )
        .await;
        let one = found_in(&m, base.path(), "01.flac", 1);
        let mut m = m.with_found(vec![one]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        // 02 may still be downloading in a slskd Soulful has not read for 10 minutes.
        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));

        let failure = m
            .poll_failed(slskd_unavailable().into(), &mut 0)
            .expect("the batch stops");
        m.stop_early(failure, Some(&backend)).await;

        assert_eq!(
            recorded_rows(&mut recorded),
            vec![(
                JobState::Failed,
                "02.flac".to_string(),
                Some(FailureCode::BackendUnreachable)
            )]
        );
        assert_eq!(import_jobs(&mut imports), vec![vec!["01.flac".to_string()]]);
        assert!(
            m.found.is_empty(),
            "nothing is left to import after the run"
        );
    }

    #[tokio::test]
    async fn an_album_cut_off_from_slskd_imports_the_files_it_finished() {
        let (mut m, _base, mut imports, mut recorded) = disk_monitor(
            &UserId::new("monitor-album-finished-stalled"),
            &["01.flac", "02.flac"],
            &["01.flac"],
        )
        .await;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let listing = vec![
            listed("peer_a", "01.flac", DownloadState::Completed),
            listed("peer_a", "02.flac", DownloadState::InProgress),
        ];
        let mut empty = 0;
        assert!(
            !m.process_poll_result(listing, &mut empty, 1, &backend)
                .await
        );
        assert!(
            import_jobs(&mut imports).is_empty(),
            "the album waits for 02"
        );

        // slskd then stays unreachable for 10 minutes.
        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));
        let failure = m
            .poll_failed(slskd_unavailable().into(), &mut 0)
            .expect("the batch stops");
        m.stop_early(failure, Some(&backend)).await;

        assert_eq!(import_jobs(&mut imports), vec![vec!["01.flac".to_string()]]);
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::removing("peer_a", &id_of("01.flac"))],
            "the finished record is removed once its file was found"
        );
        assert_eq!(
            (phase(&m, 0), phase(&m, 1)),
            (SlotPhase::HandedOff, SlotPhase::Settled)
        );
        let failed: Vec<_> = recorded_rows(&mut recorded)
            .into_iter()
            .filter(|(state, ..)| *state == JobState::Failed)
            .collect();
        assert_eq!(
            failed,
            vec![(
                JobState::Failed,
                "02.flac".to_string(),
                Some(FailureCode::BackendUnreachable)
            )]
        );
    }

    #[tokio::test]
    async fn an_album_that_never_had_a_backend_imports_its_found_files() {
        let (m, base, mut imports, mut recorded) = disk_monitor(
            &UserId::new("monitor-album-found-no-backend"),
            &["02.flac"],
            &["01.flac"],
        )
        .await;
        let one = found_in(&m, base.path(), "01.flac", 1);
        let mut m = m.with_found(vec![one]);

        m.stop_early(backend_unavailable(&unset_key()), None).await;

        assert_eq!(
            recorded_rows(&mut recorded),
            vec![(
                JobState::Failed,
                "02.flac".to_string(),
                Some(FailureCode::BackendNotConfigured)
            )]
        );
        assert_eq!(import_jobs(&mut imports), vec![vec!["01.flac".to_string()]]);
    }

    #[tokio::test]
    async fn a_resumed_album_whose_transfers_never_show_imports_its_found_files() {
        let (m, base, mut imports, _recorded) = disk_monitor(
            &UserId::new("monitor-album-found-absent"),
            &["02.flac"],
            &["01.flac"],
        )
        .await;
        let one = found_in(&m, base.path(), "01.flac", 1);
        let mut m = m.with_found(vec![one]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

        let mut empty = 0;
        for poll in 1..MAX_CONSECUTIVE_EMPTY {
            assert!(
                !m.process_poll_result(vec![], &mut empty, poll as u32, &backend)
                    .await
            );
        }
        // 02 never appeared and has no other source: every slot has settled.
        assert!(
            m.process_poll_result(vec![], &mut empty, MAX_CONSECUTIVE_EMPTY as u32, &backend)
                .await
        );

        let events = import_events(&mut imports);
        let imported: Vec<Vec<String>> = events
            .iter()
            .map(|rows| rows.iter().map(|r| r.item.clone()).collect())
            .collect();
        assert_eq!(imported, vec![vec!["01.flac".to_string()]], "{events:?}");
    }

    #[tokio::test]
    async fn a_resumed_album_left_nothing_to_watch_imports_its_found_files_once_none_are_coming() {
        let (m, base, mut imports, _recorded) = disk_monitor(
            &UserId::new("monitor-album-found-only"),
            &[],
            &["01.flac", "02.flac"],
        )
        .await;
        let found = vec![
            found_in(&m, base.path(), "01.flac", 0),
            found_in(&m, base.path(), "02.flac", 1),
        ];
        let mut m = m.with_found(found);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let sending = m.batch.inbox.reserve().expect("open inbox");

        assert!(m.watches_nothing());
        assert!(
            !m.check_completion(&[], &backend).await,
            "finished while a request was still sending into it"
        );
        assert!(import_events(&mut imports).is_empty());

        drop(sending);
        assert!(m.check_completion(&[], &backend).await);
        let imported: Vec<Vec<String>> = import_events(&mut imports)
            .iter()
            .map(|rows| rows.iter().map(|r| r.item.clone()).collect())
            .collect();
        assert_eq!(
            imported,
            vec![vec!["01.flac".to_string(), "02.flac".to_string()]]
        );
    }

    #[tokio::test]
    async fn a_file_sent_again_into_a_resumed_album_is_imported_with_its_found_files() {
        let user = UserId::new("monitor-album-sent-again");
        let (m, base, mut imports, mut recorded) = disk_monitor(&user, &[], &["01.flac"]).await;
        let folder = base.path().join(&m.batch.destination);
        let one = found_in(&m, base.path(), "01.flac", 0);
        let mut m = m.with_found(vec![one]);
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        // Startup holds the batch open, then sends 02, which slskd never got, into it.
        let reservation = m.batch.inbox.reserve().expect("open inbox");
        crate::core::acquire::enqueue_and_watch(
            &m.ledger.clone(),
            &crate::core::test_ctx(&user.to_string()),
            backend.clone(),
            crate::core::acquire::RequestBatch::Running(m.batch.clone(), reservation),
            crate::core::acquire::Acquisition {
                job: "j1".into(),
                items: vec![(1, item("peer_b", "Two", "q\\02.flac"))],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap();
        assert_eq!(stub.destinations(), vec![m.batch.destination.clone()]);
        // The request's own queued row of 02, on the user's channel.
        let sent: Vec<Vec<String>> = import_events(&mut imports)
            .iter()
            .map(|rows| rows.iter().map(|r| r.item.clone()).collect())
            .collect();
        assert_eq!(sent, vec![vec!["q\\02.flac".to_string()]]);

        assert!(m.drain_inbox());
        let mut empty = 0;
        let running = vec![listed("peer_b", "q\\02.flac", DownloadState::InProgress)];
        assert!(
            !m.process_poll_result(running, &mut empty, 1, &backend)
                .await
        );
        assert!(import_events(&mut imports).is_empty(), "01 went alone");

        std::fs::write(folder.join("02.flac"), b"x").unwrap();
        let done = vec![listed("peer_b", "q\\02.flac", DownloadState::Completed)];
        assert!(m.process_poll_result(done, &mut empty, 2, &backend).await);

        let events = import_events(&mut imports);
        assert_eq!(
            events
                .iter()
                .map(|rows| rows
                    .iter()
                    .map(|r| (r.item.clone(), r.job.clone()))
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![vec![
                ("01.flac".to_string(), Some(slot_of_j1(0))),
                ("q\\02.flac".to_string(), Some(slot_of_j1(1)))
            ]],
            "one album job"
        );
        let ledger = recorded.events();
        assert!(ledger.iter().any(|e| matches!(
            e,
            JobEvent::Enqueued { job, monitor_batch_id, .. }
                if job == "j1" && *monitor_batch_id == m.batch.owner.batch_id
        )));
        assert!(
            !ledger.iter().any(|e| matches!(
                e,
                JobEvent::Slots { state, rows } if *state == JobState::Failed
                    || rows.iter().any(|r| r.failure.as_ref().is_some_and(
                        |f| f.code == FailureCode::FileNotFoundAfterDownload
                    ))
            )),
            "a slot failed: {ledger:?}"
        );
    }

    #[tokio::test]
    async fn album_mode_removes_only_its_own_ids() {
        let (mut m, _base, _imports, _recorded) = disk_monitor(
            &UserId::new("monitor-own-ids"),
            &["dirA/01.flac"],
            &["01.flac"],
        )
        .await;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let listing = vec![
            listed("peer_a", "dirA/01.flac", DownloadState::Completed),
            // Another batch's finished record of the same basename.
            transfer(
                "peer_a",
                "dirB/01.flac",
                "t-other",
                DownloadState::Completed,
            ),
        ];
        stub.set_listing(listing.clone());

        let mut empty = 0;
        assert!(
            m.process_poll_result(listing, &mut empty, 1, &backend)
                .await
        );
        m.remove_batch_transfers(&backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::removing("peer_a", &id_of("dirA/01.flac"))]
        );
    }

    // Paused, so the resolve retries of a file that never shows up take no
    // real time.
    #[tokio::test(start_paused = true)]
    async fn an_album_file_that_never_appears_keeps_its_completed_record() {
        let (mut m, _base, mut imports, mut recorded) =
            disk_monitor(&UserId::new("monitor-album-missing"), &["01.flac"], &[]).await;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let held = key(&backend, "peer_a", "01.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&held), &m.batch);
        let listing = vec![listed("peer_a", "01.flac", DownloadState::Completed)];
        stub.set_listing(listing.clone());

        let mut empty = 0;
        assert!(
            m.process_poll_result(listing, &mut empty, 1, &backend)
                .await
        );
        m.remove_batch_transfers(&backend).await;

        assert_eq!(phase(&m, 0), SlotPhase::Settled);
        assert!(
            stub.cancelled().is_empty(),
            "a succeeded record was removed although its file was never found"
        );
        let rows: Vec<DownloadProgress> =
            import_events(&mut imports).into_iter().flatten().collect();
        assert_eq!(rows.len(), 1);
        assert_failed(
            &rows[0],
            FailureCode::FileNotFoundAfterDownload,
            "Could not resolve file path",
        );
        assert!(TransferRegistry::global().may_adopt(&held, &batch("Y").owner));
        let one = vec![("01.flac".to_string(), Some(slot_of_j1(0)))];
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![
                (JobState::Downloading, one.clone()),
                (JobState::Failed, one)
            ]
        );
    }

    // Paused: resolve_on_disk backs off with tokio::time::sleep.
    #[tokio::test(start_paused = true)]
    async fn a_handoff_whose_file_is_never_found_records_its_slot_failed() {
        let (mut m, _base, _imports, mut recorded) = disk_monitor(
            &UserId::new("monitor-singleton-missing"),
            &["one.flac"],
            &[],
        )
        .await;
        m.batch.import_mode = ImportMode::Singleton;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut empty = 0;
        m.process_poll_result(
            vec![listed("peer_a", "one.flac", DownloadState::Completed)],
            &mut empty,
            1,
            &backend,
        )
        .await;
        m.join_handoffs().await;
        let one = vec![("one.flac".to_string(), Some(slot_of_j1(0)))];
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![
                (JobState::Downloading, one.clone()),
                (JobState::Failed, one)
            ]
        );
    }

    #[test]
    fn only_the_transfers_the_last_listing_shows_completed_are_remembered() {
        let (mut m, _recorded) = monitor(vec!["peer_a"], vec!["one.flac"]);
        let done = listed("peer_a", "one.flac", DownloadState::Completed);
        assert!(m
            .peers_sending(std::slice::from_ref(&done))
            .contains("peer_a"));
        assert!(m.peers_sending(std::slice::from_ref(&done)).is_empty());
        assert!(m.peers_sending(&[]).is_empty());
        assert!(m.completed_seen.is_empty());
    }

    #[tokio::test]
    async fn monitor_rows_reach_the_channel_of_the_batch_owner_id() {
        let alice = UserId::new("id-alice-monitor");
        let (mut m, _base, mut imports, _recorded) =
            disk_monitor(&alice, &["01.flac"], &["01.flac"]).await;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

        let mut empty = 0;
        m.process_poll_result(
            vec![listed("peer_a", "01.flac", DownloadState::Completed)],
            &mut empty,
            1,
            &backend,
        )
        .await;
        m.join_handoffs().await;

        let rows: Vec<DownloadProgress> =
            import_events(&mut imports).into_iter().flatten().collect();
        assert!(
            rows.iter()
                .any(|r| matches!(r.state, DownloadState::Failed(_))),
            "{rows:?}"
        );
        assert!(
            !send_user_event(
                &UserId::new("alice-monitor"),
                DownloadEvent::Progress(vec![])
            )
            .await
        );
    }

    #[tokio::test]
    async fn import_rows_carry_the_batch_id() {
        for album_mode in [false, true] {
            let user = UserId::new(&format!("monitor-batch-rows-{album_mode}"));
            let (mut m, _base, mut imports, _recorded) =
                disk_monitor(&user, &["01.flac"], &["01.flac"]).await;
            m.batch.import_mode = match album_mode {
                true => ImportMode::Album,
                false => ImportMode::Singleton,
            };
            let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

            let mut empty = 0;
            assert!(
                m.process_poll_result(
                    vec![listed("peer_a", "01.flac", DownloadState::Completed)],
                    &mut empty,
                    1,
                    &backend
                )
                .await
            );
            m.join_handoffs().await;

            let rows: Vec<DownloadProgress> =
                import_events(&mut imports).into_iter().flatten().collect();
            assert!(!rows.is_empty(), "album_mode={album_mode}");
            assert!(
                rows.iter()
                    .all(|r| r.batch_id.as_ref() == Some(&m.batch.owner.batch_id)),
                "album_mode={album_mode}: {rows:?}"
            );
        }
    }

    #[tokio::test]
    async fn added_slots_are_monitored() {
        let (mut m, _base, mut imports, _recorded) = disk_monitor(
            &UserId::new("monitor-added-slots"),
            &["01.flac"],
            &["01.flac", "02.flac"],
        )
        .await;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_b".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: id_of("02.flac"),
                alternates: vec![],
                used: 1,
                max_sources: MAX_SOURCES_PER_TRACK,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");

        // The top of the next run-loop iteration.
        assert!(m.drain_inbox());
        assert_eq!(m.sources(), vec!["peer_a", "peer_b"]);
        let listing = vec![
            listed("peer_a", "01.flac", DownloadState::Completed),
            listed("peer_b", "02.flac", DownloadState::Completed),
        ];
        let mut empty = 0;
        assert!(
            m.process_poll_result(listing, &mut empty, 1, &backend)
                .await
        );

        assert_eq!(handed_off(&mut imports), vec!["01.flac", "02.flac"]);
        assert!(
            m.batch.inbox.reserve().is_none(),
            "the finished batch is closed"
        );
    }

    #[tokio::test]
    async fn a_slot_a_joining_request_added_fails_over_to_its_own_alternates() {
        let (tx, _rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_a".into(),
                filename: "dirA/two.flac".into(),
                transfer_id: id_of("dirA/two.flac"),
                alternates: vec![item("peer_b", "Two", "dirB/two.flac")],
                used: 1,
                max_sources: MAX_SOURCES_PER_TRACK,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");
        assert!(m.drain_inbox());
        m.track_states[1].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "dirA/aaa.flac", DownloadState::InProgress),
                failed_transfer("peer_a", "dirA/two.flac"),
            ],
            &backend,
        )
        .await;

        assert_eq!(
            (
                m.tracked_files[1].source.as_str(),
                m.tracked_files[1].transfer_id.as_str()
            ),
            ("peer_b", "t-dirB/two.flac")
        );
        assert_eq!((m.pool.attempts_used(0), m.pool.attempts_used(1)), (1, 2));
        assert_eq!(m.tracked_files[0].source, "peer_a");
    }

    #[tokio::test]
    async fn a_joined_slot_fails_over_within_its_own_cap() {
        let (tx, _rx) = broadcast::channel(16);
        let (mut m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["dirA/aaa.flac"]);
        assert_eq!(m.pool.max_sources(0), MAX_SOURCES_PER_TRACK);
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_a".into(),
                filename: "dirA/two.flac".into(),
                transfer_id: id_of("dirA/two.flac"),
                alternates: vec![
                    item("peer_b", "Two", "dirB/two.flac"),
                    item("peer_c", "Two", "dirC/two.flac"),
                ],
                used: 1,
                max_sources: 2,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");
        assert!(m.drain_inbox());
        assert_eq!(m.pool.max_sources(1), 2);
        m.track_states[1].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "dirA/aaa.flac", DownloadState::InProgress),
                failed_transfer("peer_a", "dirA/two.flac"),
            ],
            &backend,
        )
        .await;
        assert_eq!(m.tracked_files[1].source, "peer_b");

        m.track_states[1].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));
        poll_tracks(
            &mut m,
            &[
                listed("peer_a", "dirA/aaa.flac", DownloadState::InProgress),
                failed_transfer("peer_b", "dirB/two.flac"),
            ],
            &backend,
        )
        .await;

        assert_eq!(
            m.tracked_files[1].source, "peer_b",
            "peer_c is over the cap"
        );
        assert_eq!(m.pool.attempts_used(1), 2);
        assert_eq!(phase(&m, 1), SlotPhase::Settled);
        assert_eq!(phase(&m, 0), SlotPhase::Watching);
    }

    #[tokio::test]
    async fn the_monitor_waits_for_reserved_slots_before_finishing_an_album() {
        let (mut m, _base, mut imports, _recorded) = disk_monitor(
            &UserId::new("monitor-reserved-slots"),
            &["01.flac"],
            &["01.flac"],
        )
        .await;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let listing = || vec![listed("peer_a", "01.flac", DownloadState::Completed)];
        let reservation = m.batch.inbox.reserve().expect("open inbox");

        let mut empty = 0;
        assert!(
            !m.process_poll_result(listing(), &mut empty, 1, &backend)
                .await
        );
        assert!(handed_off(&mut imports).is_empty(), "the album waited");

        drop(reservation);
        assert!(
            m.process_poll_result(listing(), &mut empty, 2, &backend)
                .await
        );
        assert_eq!(handed_off(&mut imports), vec!["01.flac"]);
    }

    #[tokio::test]
    async fn slots_written_off_at_the_end_of_a_run_are_cancelled() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut m, mut recorded) = monitor_on(tx, vec!["peer_a"], vec!["01.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_b".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02".to_string(),
                alternates: vec![],
                used: 1,
                max_sources: MAX_SOURCES_PER_TRACK,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");

        m.close_inbox_cancelling(&backend).await;

        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("the undrained slot was never written off");
        };
        assert_failed(&rows[0], FailureCode::TransferCancelled, MONITOR_STOPPED);
        assert_eq!(stub.cancelled(), vec![Cancel::only("peer_b", "t-02")]);
        let record = transfer("peer_b", "02.flac", "t-02", DownloadState::Cancelled);
        assert!(m.owns(&record), "its record is removed at cleanup");
        assert_eq!(
            slots_recorded(&mut recorded),
            vec![(
                JobState::Failed,
                vec![("02.flac".into(), Some(slot_of_j1(1)))]
            )]
        );
    }

    #[tokio::test]
    async fn a_dropped_monitor_closes_its_inbox_and_frees_its_keys() {
        let (tx, mut rx) = broadcast::channel(16);
        let (m, _recorded) = monitor_on(tx, vec!["peer_a"], vec!["01.flac"]);
        let inbox = Arc::clone(&m.batch.inbox);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let held = key(&backend, "peer_a", "01.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&held), &m.batch);
        let late = inbox.reserve().expect("open inbox");
        inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_b".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: "t-02".to_string(),
                alternates: vec![],
                used: 1,
                max_sources: MAX_SOURCES_PER_TRACK,
                sent: vec![],
                job: slot_of_j1(1),
            }])
            .expect("delivered");

        drop(m);

        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("the undrained slot was never written off");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "t-02");
        assert!(late.deliver(vec![]).is_err(), "a late join is handed back");
        assert!(TransferRegistry::global().may_adopt(&held, &batch("Y").owner));
    }
}
