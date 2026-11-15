//! Download monitoring logic for tracking slskd download progress.
//!
//! This module encapsulates the polling loop that monitors downloads from slskd,
//! handles per-track timeouts, and hands finished files to the import queue.
//! Every slot is found in slskd's list by its transfer id, and a transfer's
//! record is only removed once its file was found on disk: removing it
//! earlier can cancel slskd between persisting the transfer as finished and
//! moving the file, which strands the file in slskd's incomplete folder.

use dioxus::logger::tracing::{debug, info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState, QueuedDownload};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::SoulseekError;
use soulbeet::{DownloadBackend, ImportMode};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::enqueue::{stopped_rows, transfer_key};
use super::failover::{SourcePool, MAX_SOURCES_PER_TRACK};
use super::process::{album_jobs, import_job, resolve_on_disk, unresolved_row, ResolvedSlot};
use super::utils::FileId;
use crate::config::CONFIG;
use crate::globals::send_user_event;
use crate::import_queue::IMPORT_QUEUE;
use crate::services::{download_backend, BackendLookupError};
use crate::transfer_registry::{AddedSlot, BatchRef, TransferKey, TransferOwner, TransferRegistry};

/// Poll interval for checking download status (2 seconds).
const POLL_INTERVAL_SECS: u64 = 2;

/// Grace period for downloads to appear in slskd (30 seconds = 15 * 2s intervals).
const MAX_CONSECUTIVE_EMPTY: usize = 15;

/// Per-track timeout duration (1 hour).
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
/// batch. Scoped to schema drift (`SoulseekError::InvalidResponse`): slskd
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

/// A transfer slskd never listed.
fn never_appeared() -> Failure {
    transfer_failure(
        FailureCode::TransferNeverAppeared,
        RetryAdvice::OtherSource,
        "Download never appeared in slskd",
    )
}

/// A transfer slskd listed and then dropped.
fn disappeared() -> Failure {
    transfer_failure(
        FailureCode::TransferDisappeared,
        RetryAdvice::SameSourceLater,
        "Download disappeared from slskd",
    )
}

/// A slot that ran past `PER_TRACK_TIMEOUT` with no peer left.
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
fn cancelled_on_request() -> Failure {
    transfer_failure(
        FailureCode::TransferCancelled,
        RetryAdvice::NotRetryable,
        "Cancelled on request",
    )
}

/// slskd's transfer list stayed unreadable for `BACKEND_STALL_TIMEOUT`, the
/// last read failing with `e`. Its code, advice and cause are `e`'s: a
/// refused API key needs a person, an unreachable slskd needs time.
fn stalled(e: &SoulseekError) -> Failure {
    e.failure(
        Stage::Transfer,
        format!(
            "slskd's transfer list was unreadable for {} minutes",
            BACKEND_STALL_TIMEOUT.as_secs() / 60
        ),
    )
}

/// No download backend was available for `MAX_BACKEND_FAILURES` polls, the
/// last lookup failing with `e`, whose failure this is.
fn backend_unavailable(e: &BackendLookupError) -> Failure {
    e.failure(Stage::Transfer, "Download backend unavailable")
}

/// slskd's downloads list kept failing to parse.
fn list_unreadable(e: &SoulseekError) -> Failure {
    e.failure(
        Stage::Transfer,
        format!("Could not read slskd downloads list: {e}"),
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
    /// When the track was first seen in slskd's download list.
    first_seen: Option<Instant>,
    /// When the track went missing from slskd's list after being seen.
    missing_since: Option<Instant>,
    /// When the track's transfer was first seen in a failed terminal state.
    failing_since: Option<Instant>,
    /// When the track's transfer was first seen waiting in its source's
    /// queue, cleared the moment it is seen in any other state.
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
            first_seen: None,
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
}

impl From<&QueuedDownload> for WatchedTransfer {
    fn from(queued: &QueuedDownload) -> Self {
        Self {
            source: queued.source.clone(),
            filename: queued.item.clone(),
            transfer_id: queued.id.clone(),
        }
    }
}

/// The transfer a slot currently watches.
#[derive(Clone, Debug)]
struct TrackedFile {
    source: String,
    filename: String,
    transfer_id: String,
    /// The batch folder the transfer was enqueued into, or `None` for a
    /// transfer this batch adopted, which slskd saves wherever its own
    /// enqueue said.
    destination: Option<String>,
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
}

impl DownloadMonitor {
    /// Create a new download monitor. The transfers must have been enqueued
    /// into `batch.destination`. Dropping the monitor closes the batch's
    /// inbox and releases every registry key the batch holds.
    pub fn new(
        transfers: Vec<WatchedTransfer>,
        target_path: PathBuf,
        tx: broadcast::Sender<DownloadEvent>,
        batch: BatchRef,
    ) -> Self {
        let tracked_files: Vec<TrackedFile> = transfers
            .into_iter()
            .map(|t| TrackedFile {
                source: t.source,
                filename: t.filename,
                transfer_id: t.transfer_id,
                destination: Some(batch.destination.clone()),
            })
            .collect();

        let track_states = (0..tracked_files.len())
            .map(|_| TrackState::bound_now())
            .collect();
        let pool = SourcePool::from_tracks(&vec![None; tracked_files.len()], &[]);

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
        }
    }

    fn watching(&self, slot: usize) -> bool {
        self.track_states[slot].phase == SlotPhase::Watching
    }

    /// Whether `dl` is the transfer `slot` watches now. A slot rebound during
    /// a poll no longer owns the entry that poll matched it with.
    fn is_current(&self, slot: usize, dl: &DownloadProgress) -> bool {
        let tracked = &self.tracked_files[slot];
        dl.source == tracked.source && dl.id == tracked.transfer_id
    }

    /// The registry key of the transfer `slot` watches.
    fn slot_key(&self, slot: usize, backend: &'static str) -> TransferKey {
        let tracked = &self.tracked_files[slot];
        transfer_key(backend, &tracked.source, &tracked.filename)
    }

    /// Ends `slot` without a file and frees its transfer for anyone.
    fn settle(&mut self, slot: usize, phase: SlotPhase, backend: &'static str) {
        self.track_states[slot].phase = phase;
        TransferRegistry::global().release(&self.slot_key(slot, backend), &self.batch.owner);
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

    /// Whether some watched slot has no usable transfer from its own peer.
    /// Such a slot may have been retried from a peer this batch never asked
    /// (#62), and only the unscoped list can show that.
    fn any_slot_unresolved(&self, downloads: &[DownloadProgress]) -> bool {
        (0..self.tracked_files.len())
            .filter(|slot| self.watching(*slot))
            .any(|slot| {
                let tracked = &self.tracked_files[slot];
                transfer_by_id(downloads, &tracked.source, &tracked.transfer_id)
                    .is_none_or(|dl| is_failed(&dl.state))
            })
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
                .find(|dl| dl.source == tracked.source && dl.id == tracked.transfer_id)
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
            });
            self.track_states.push(TrackState::bound_now());
            self.pool.push_slot(slot.alternates);
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
            let _ = self.tx.send(DownloadEvent::Progress(stopped_rows(
                &unwatched,
                &self.batch,
            )));
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
                    "Failed to cancel unwatched transfer {} in slskd: {}",
                    slot.transfer_id, e
                );
            }
            self.abandoned.push((slot.source, slot.transfer_id));
        }
    }

    /// Count a failed read of slskd's transfer list. Returns whether the
    /// batch was failed, so the monitor must stop.
    ///
    /// An unparseable list is fused by count (`MAX_INVALID_RESPONSES`).
    /// Every error is also fused by time: transport errors are deliberately
    /// not fused by count, because a slskd restart resolves on its own, but
    /// while no list can be read no per-track deadline is ever evaluated and
    /// the monitor would poll for the life of the process (#78). The batch
    /// fails with the error that ended it.
    fn poll_failed(&mut self, e: SoulseekError, invalid_responses: &mut u32) -> bool {
        if let SoulseekError::InvalidResponse(_) = e {
            // Schema drift: slskd answered but its downloads list no
            // longer parses. That will not fix itself, so fail the
            // batch with the real error instead of polling forever
            // (pre-fix this surfaced as the misleading "Download
            // never appeared in slskd", issue #73).
            *invalid_responses += 1;
            warn!(
                "Unparseable slskd downloads list ({}/{}): {}",
                invalid_responses, MAX_INVALID_RESPONSES, e
            );
            if *invalid_responses >= MAX_INVALID_RESPONSES {
                self.fail_unprocessed_tracks(list_unreadable(&e));
                return true;
            }
        } else {
            warn!("Error fetching download status from slskd: {}", e);
        }
        if self.last_readable_poll.elapsed() <= BACKEND_STALL_TIMEOUT {
            return false;
        }
        let failure = stalled(&e);
        warn!(
            "{} - giving up on batch {:?}",
            failure.message,
            self.filenames()
        );
        self.fail_unprocessed_tracks(failure);
        true
    }

    /// Attach alternate sources so failed transfers retry from another peer.
    /// `pool` has a slot per transfer the monitor was created with.
    pub fn with_failover(mut self, pool: SourcePool) -> Self {
        self.pool = pool;
        self
    }

    /// Re-enqueue this slot from its next-best peer. Returns true when the
    /// slot was rebound, in which case the caller must not mark it failed.
    ///
    /// A slot its user cancelled is settled by `apply_cancels` before any
    /// failover runs. A cancel that arrives while the next peer is being
    /// enqueued moves to the new transfer with the slot.
    async fn try_failover(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        reason: &str,
    ) -> bool {
        let next = self.pool.enqueue_next(slot, backend, &self.batch).await;
        for refused in &next.refused {
            warn!(
                "Failover source {} refused {} for {}: {}: {}",
                refused.source,
                refused.item,
                self.tracked_files[slot].filename,
                refused.failure.code.as_str(),
                refused.failure.message
            );
        }
        let Some(queued) = next.queued else {
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
            MAX_SOURCES_PER_TRACK
        );

        self.abandon(slot, backend).await;
        let old_key = self.slot_key(slot, backend.id());
        let new_key = transfer_key(backend.id(), &queued.source, &queued.item);
        TransferRegistry::global().hand_over(&old_key, &new_key, &self.batch.owner);

        // The new peer serves this track under a completely different remote
        // path and transfer id, so the slot must track both outright or the
        // monitor loses sight of it.
        let previous_item = std::mem::replace(
            &mut self.tracked_files[slot],
            TrackedFile {
                source: queued.source.clone(),
                filename: queued.item.clone(),
                transfer_id: queued.id.clone(),
                destination: Some(self.batch.destination.clone()),
            },
        )
        .filename;
        self.track_states[slot] = TrackState::bound_now();

        let entry = DownloadProgress::queued(queued.id, queued.source, queued.item, queued.size);
        let entry = self.stamp(entry);
        // One slot is still one download, so the panel replaces the abandoned
        // peer's row instead of gaining a second one for the same track.
        let _ = self.tx.send(DownloadEvent::Replaced {
            previous_item,
            entry: Box::new(entry),
        });
        true
    }

    /// Stop the transfer a slot is about to stop watching, and remember it
    /// so its record is removed at the end.
    ///
    /// At the per-track timeout that transfer is still running, so leaving it
    /// alone would have two peers sending the same track, the abandoned one
    /// into this batch's folder. Even a dead transfer has to go: once no slot
    /// points at it nothing else prunes it. slskd cancels by transfer id, and
    /// a cancel that fails is not worth losing the retry over.
    ///
    /// The cancel keeps the record: the transfer may have just succeeded, and
    /// removing a succeeded record before slskd moves its file strands the
    /// file. End-of-run cleanup removes it once it shows Cancelled or Failed.
    async fn abandon(&mut self, slot: usize, backend: &Arc<dyn DownloadBackend>) {
        let tracked = &self.tracked_files[slot];
        if let Err(e) = backend
            .cancel_download(&tracked.source, &tracked.transfer_id, false)
            .await
        {
            warn!(
                "Failed to cancel abandoned transfer {} in slskd: {}",
                tracked.transfer_id, e
            );
        }
        self.abandoned
            .push((tracked.source.clone(), tracked.transfer_id.clone()));
    }

    /// Run the monitoring loop until all downloads complete or timeout.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        let mut consecutive_empty = 0;
        let mut poll_count = 0;
        let mut backend_failures: u32 = 0;
        let mut invalid_responses: u32 = 0;
        self.last_readable_poll = Instant::now();

        // Poll immediately on first iteration
        interval.tick().await;

        loop {
            if self.drain_inbox() {
                // The new transfers get the whole window to show up.
                consecutive_empty = 0;
            }

            poll_count += 1;

            let backend = match download_backend(None).await {
                Ok(b) => {
                    backend_failures = 0;
                    b
                }
                Err(e) => {
                    backend_failures += 1;
                    warn!(
                        "No download backend available for monitoring ({}/{}): {}",
                        backend_failures, MAX_BACKEND_FAILURES, e
                    );
                    if backend_failures >= MAX_BACKEND_FAILURES {
                        self.fail_unprocessed_tracks(backend_unavailable(&e));
                        break;
                    }
                    interval.tick().await;
                    continue;
                }
            };
            match backend.get_downloads_for(&self.sources()).await {
                Ok(mut downloads) => {
                    invalid_responses = 0;
                    self.last_readable_poll = Instant::now();
                    self.read_pending_retries(&mut downloads);

                    // Widen to every peer only while a slot has nothing
                    // usable, so the unbounded list is read when it can
                    // actually tell us something new rather than every poll.
                    if self.any_slot_unresolved(&downloads) {
                        match backend.get_downloads().await {
                            Ok(mut all) => {
                                self.read_pending_retries(&mut all);
                                downloads = all;
                            }
                            Err(e) => debug!("Could not widen the transfer view: {}", e),
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
                    if self.poll_failed(e, &mut invalid_responses) {
                        break;
                    }
                }
            }

            interval.tick().await;
        }

        // Joiners must not enqueue into a batch that is only cleaning up.
        match download_backend(None).await {
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
    /// that ended without a file. A succeeded record is only ever removed
    /// by the handoff or album finish that found its file on disk: removing
    /// it before that can strand the file in slskd's incomplete folder.
    /// Only this batch's ids: another batch's record of the same file is
    /// that batch's to remove. A failed attempt slskd says it will retry
    /// ended without a file too, and with the batch done nobody would import
    /// what a retry fetched.
    async fn remove_batch_transfers(&mut self, backend: &Arc<dyn DownloadBackend>) {
        let mut peers = self.sources();
        for (peer, _) in &self.abandoned {
            if !peers.contains(peer) {
                peers.push(peer.clone());
            }
        }
        let downloads = match backend.get_downloads_for(&peers).await {
            Ok(d) => d,
            Err(e) => {
                warn!("Could not list downloads for batch cleanup: {}", e);
                return;
            }
        };

        for entry in downloads.iter().filter(|entry| {
            (is_failed(&entry.state) || entry.pending_retry.is_some()) && self.owns(entry)
        }) {
            if let Err(e) = backend
                .cancel_download(&entry.source, &entry.id, true)
                .await
            {
                warn!(
                    "Failed to remove finished transfer {} from slskd: {}",
                    entry.id, e
                );
            }
        }
    }

    /// Stop every watched slot whose transfer its user cancelled, and show
    /// it Cancelled. A slot whose own transfer already completed is left
    /// alone, so its file is still imported.
    async fn apply_cancels(
        &mut self,
        downloads: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let registry = TransferRegistry::global();
        let mut cancelled = Vec::new();
        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot)
                || !registry.is_cancelled(&self.slot_key(slot, backend.id()), &self.batch.owner)
            {
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
            let row = DownloadProgress::queued(
                tracked.transfer_id.clone(),
                tracked.source.clone(),
                tracked.filename.clone(),
                0,
            )
            .with_state(DownloadState::Cancelled)
            .with_failure(cancelled_on_request())
            .with_backend(backend.id());
            self.abandon(slot, backend).await;
            self.settle(slot, SlotPhase::Settled, backend.id());
            cancelled.push(self.stamp(row));
        }
        if !cancelled.is_empty() {
            let _ = self.tx.send(DownloadEvent::Progress(cancelled));
        }
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
            let slskd_filenames: Vec<_> = downloads.iter().map(|f| &f.item).collect();
            debug!(
                "slskd returned {} downloads: {:?}",
                downloads.len(),
                slskd_filenames
            );
        }

        let matched = self.find_matching_downloads(&downloads, backend);

        let watched = (0..self.tracked_files.len())
            .filter(|slot| self.watching(*slot))
            .count();
        let found = matched
            .iter()
            .filter(|(slot, _)| self.watching(*slot))
            .count();
        if poll_count <= 3 || found != watched {
            info!(
                "Matched {} of {} watched downloads from slskd (poll {})",
                found, watched, poll_count
            );
            self.log_unmatched_files(&matched);
        }

        if !matched.is_empty() {
            *consecutive_empty = 0;
        }

        // Handle grace period for downloads to appear. A handoff removes its
        // slot's record, so a batch that was seen before can list nothing
        // and still be under way; its absent slots are judged one by one.
        let never_seen = self
            .track_states
            .iter()
            .all(|state| state.first_seen.is_none());
        if matched.is_empty() && never_seen {
            *consecutive_empty += 1;
            if *consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                warn!(
                    "No active downloads found for batch after {} attempts ({}s), assuming completed or lost: {:?}",
                    MAX_CONSECUTIVE_EMPTY,
                    MAX_CONSECUTIVE_EMPTY * 2,
                    self.filenames()
                );
                if self.retry_or_fail_absent_batch(backend).await
                    && self.batch.inbox.close_if_idle()
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
                    "Waiting for downloads to appear in slskd, attempt {}/{} ({}/{}s)",
                    *consecutive_empty,
                    MAX_CONSECUTIVE_EMPTY,
                    *consecutive_empty * 2,
                    MAX_CONSECUTIVE_EMPTY * 2
                );
            }
            return false;
        }

        // Process individual tracks
        self.process_tracks(&matched, backend).await;

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
    /// another peer has a live transfer of the same file, the download was
    /// retried from a different source: the slot adopts it. Only a transfer
    /// no batch owns, or one this batch already owns, may be adopted;
    /// another batch's, even the same user's, has its own monitor.
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
                if let Some(retry) = self.adoptable(slot, downloads, backend.id()) {
                    let retry = retry.clone();
                    if self.adopt(slot, &retry, backend.id()) {
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

    /// Another peer's live transfer of `slot`'s file that this batch may
    /// adopt. A record between attempts is not live: no watcher's clock
    /// bounds a foreign one, and a record cancelled during slskd's retry
    /// delay stays that way for good. If slskd starts the retry, the record
    /// reads `Queued, Locally` on the next poll and is adoptable then.
    fn adoptable<'a>(
        &self,
        slot: usize,
        downloads: &'a [DownloadProgress],
        backend: &'static str,
    ) -> Option<&'a DownloadProgress> {
        let tracked = &self.tracked_files[slot];
        let registry = TransferRegistry::global();
        downloads.iter().find(|dl| {
            dl.source != tracked.source
                && !is_terminal_state(&dl.state)
                && dl.pending_retry.is_none()
                && filenames_match(&dl.item, &tracked.filename)
                && !self.owns(dl)
                && registry.may_adopt(
                    &transfer_key(backend, &dl.source, &dl.item),
                    &self.batch.owner,
                )
        })
    }

    /// Whether `dl` is a transfer this batch watches or abandoned.
    fn owns(&self, dl: &DownloadProgress) -> bool {
        (0..self.tracked_files.len()).any(|slot| self.is_current(slot, dl))
            || self
                .abandoned
                .iter()
                .any(|(peer, id)| *peer == dl.source && *id == dl.id)
    }

    /// Rebind `slot` to `retry`, taking its registry key over from the
    /// slot's own. Returns false when the key was taken meanwhile.
    fn adopt(&mut self, slot: usize, retry: &DownloadProgress, backend: &'static str) -> bool {
        let old = self.slot_key(slot, backend);
        let new = transfer_key(backend, &retry.source, &retry.item);
        if !TransferRegistry::global().rebind(&old, new, &self.batch) {
            return false;
        }
        let tracked = &self.tracked_files[slot];
        info!(
            "Download of {} retried from new peer {} (was {}), rebinding",
            tracked.filename, retry.source, tracked.source
        );
        // Its record, failed or already gone, no longer belongs to a slot.
        self.abandoned
            .push((tracked.source.clone(), tracked.transfer_id.clone()));
        let previous_item = std::mem::replace(
            &mut self.tracked_files[slot],
            TrackedFile {
                source: retry.source.clone(),
                filename: retry.item.clone(),
                transfer_id: retry.id.clone(),
                destination: None,
            },
        )
        .filename;
        // Reset state so the retried transfer is monitored and imported
        self.track_states[slot] = TrackState::bound_now();
        if previous_item != retry.item {
            let _ = self.tx.send(DownloadEvent::Replaced {
                previous_item,
                entry: Box::new(self.stamp(retry.clone())),
            });
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
            .map(|(_, dl)| self.stamp(dl.clone()))
            .collect();
        if entries.is_empty() {
            return;
        }
        if let Err(e) = self.tx.send(DownloadEvent::Progress(entries)) {
            if self.tx.receiver_count() == 0 {
                info!("No receivers for download updates, but continuing monitoring");
            } else {
                warn!("Failed to send download status update: {:?}", e);
            }
        }
    }

    /// Apply batch_id and batch_label to a progress entry.
    fn stamp(&self, entry: DownloadProgress) -> DownloadProgress {
        entry.with_batch(self.batch.owner.batch_id.clone(), self.batch.label.clone())
    }

    /// Apply batch_id and batch_label to a set of progress entries.
    fn stamp_batch(&self, entries: Vec<DownloadProgress>) -> Vec<DownloadProgress> {
        entries.into_iter().map(|entry| self.stamp(entry)).collect()
    }

    /// Process each watched track, handling timeouts, completions and
    /// failures.
    async fn process_tracks(
        &mut self,
        matched: &[(usize, DownloadProgress)],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        for (slot, download) in matched {
            let slot = *slot;
            if self.track_states[slot].first_seen.is_none() {
                self.track_states[slot].first_seen = Some(Instant::now());
            }

            if !self.watching(slot) {
                continue;
            }

            // Check per-track timeout
            if let Some(first_seen) = self.track_states[slot].first_seen {
                if first_seen.elapsed() > PER_TRACK_TIMEOUT && !is_terminal_state(&download.state) {
                    warn!(
                        "Track timed out after {} minutes: {}",
                        first_seen.elapsed().as_secs() / 60,
                        download.item
                    );
                    self.time_out(slot, download, per_track_timeout(), backend)
                        .await;
                    continue;
                }
            }

            // A peer can hold its queue for hours; another source may send
            // the file now.
            if download.queued_at_source {
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
                    if !retryable || !self.try_failover(slot, backend, "Transfer failed").await {
                        self.settle(slot, SlotPhase::Settled, backend.id());
                        let row = self.stamp(download.clone());
                        let _ = self.tx.send(DownloadEvent::Progress(vec![row]));
                    }
                }
            } else {
                self.track_states[slot].failing_since = None;
            }
        }
    }

    /// Fail `slot` over to its next source, or else cancel its transfer,
    /// settle it TimedOut and show `download` Failed with `failure`.
    async fn time_out(
        &mut self,
        slot: usize,
        download: &DownloadProgress,
        failure: Failure,
        backend: &Arc<dyn DownloadBackend>,
    ) {
        if self.try_failover(slot, backend, &failure.message).await {
            return;
        }
        self.abandon(slot, backend).await;
        self.settle(slot, SlotPhase::TimedOut, backend.id());
        let row = DownloadProgress {
            queued_at_source: false,
            ..download.clone()
        }
        .with_state(DownloadState::Failed(String::new()))
        .with_failure(failure);
        let entries = self.stamp_batch(vec![row]);
        let _ = self.tx.send(DownloadEvent::Progress(entries));
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
        let row = self.stamp(download.clone());
        let _ = self.tx.send(DownloadEvent::Progress(vec![row.clone()]));
        self.track_states[slot].phase = SlotPhase::HandedOff;
        let handoff = Handoff {
            row,
            destination: self.tracked_files[slot].destination.clone(),
            key: self.slot_key(slot, backend.id()),
            owner: self.batch.owner.clone(),
            target: self.target_path.clone(),
            download_base: self.download_base.clone(),
            claimed: Arc::clone(&self.claimed),
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

            let absent = match self.track_states[slot].first_seen {
                None if self.track_states[slot].bound_at.elapsed() > ABSENT_TRACK_TIMEOUT => {
                    Some(never_appeared())
                }
                Some(_) => {
                    let missing_since = self.track_states[slot]
                        .missing_since
                        .get_or_insert_with(Instant::now);
                    (missing_since.elapsed() > ABSENT_TRACK_TIMEOUT).then(disappeared)
                }
                None => None,
            };

            if let Some(failure) = absent {
                // Logged only once the track is really being written off:
                // a successful failover reports its own retry instead.
                if self.try_failover(slot, backend, &failure.message).await {
                    continue;
                }
                warn!(
                    "{} after {}s, marking failed: {}",
                    failure.message,
                    ABSENT_TRACK_TIMEOUT.as_secs(),
                    self.tracked_files[slot].filename
                );
                self.settle(slot, SlotPhase::Settled, backend.id());
                failed.push(make_failed_progress(&self.tracked_files[slot], failure));
            }
        }

        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
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
        let failure = never_appeared();
        let mut retried = false;
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot) {
                continue;
            }
            if self.try_failover(slot, backend, &failure.message).await {
                retried = true;
                continue;
            }
            // Without this, rows whose transfer never surfaced in slskd would
            // sit at "Queued" in the UI forever.
            self.settle(slot, SlotPhase::Settled, backend.id());
            failed.push(make_failed_progress(
                &self.tracked_files[slot],
                failure.clone(),
            ));
        }

        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
        !retried
    }

    /// Mark every watched track as failed and notify the UI. Used when
    /// monitoring must stop early so downloads are never left dangling. With
    /// no backend at hand their keys are released when the monitor drops.
    fn fail_unprocessed_tracks(&mut self, failure: Failure) {
        let mut failed: Vec<DownloadProgress> = Vec::new();
        for slot in 0..self.tracked_files.len() {
            if self.watching(slot) {
                self.track_states[slot].phase = SlotPhase::Settled;
                failed.push(make_failed_progress(
                    &self.tracked_files[slot],
                    failure.clone(),
                ));
            }
        }
        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
    }

    /// Check if all downloads are complete. Returns true if monitoring should stop.
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
        let all_settled = (0..self.tracked_files.len()).all(|slot| {
            !self.watching(slot)
                || matched.iter().any(|(s, dl)| {
                    *s == slot && self.is_current(slot, dl) && is_completed(&dl.state)
                })
        });

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

    /// Import every finished slot of an album batch together. The slskd
    /// records are only removed once every file was looked for on disk, and
    /// the keys only once the imports are done.
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
                    self.stamp(dl.clone()),
                    self.tracked_files[*slot].destination.clone(),
                )
            })
            .collect();
        if finished.is_empty() {
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
            send_user_event(&self.batch.owner.user_id, DownloadEvent::Progress(missing)).await;
        }

        for slot in &resolved_slots {
            let tracked = &self.tracked_files[*slot];
            if let Err(e) = backend
                .cancel_download(&tracked.source, &tracked.transfer_id, true)
                .await
            {
                warn!(
                    "Failed to remove finished transfer {} from slskd: {}",
                    tracked.transfer_id, e
                );
            }
            self.track_states[*slot].phase = SlotPhase::HandedOff;
        }

        let jobs = album_jobs(
            resolved,
            &self.target_path,
            &self.batch.owner.user_id,
            &self.download_base,
        );
        info!(
            "Album mode: queueing {} import(s) of {} downloads into {:?}",
            jobs.len(),
            resolved_slots.len(),
            self.target_path
        );
        let mut handles = Vec::new();
        for job in jobs {
            handles.push(IMPORT_QUEUE.submit(job).await);
        }
        for handle in handles {
            handle.wait().await;
        }
        let registry = TransferRegistry::global();
        for slot in resolved_slots {
            registry.release(&self.slot_key(slot, backend.id()), &self.batch.owner);
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
    target: PathBuf,
    download_base: PathBuf,
    claimed: Arc<Mutex<HashSet<FileId>>>,
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
            send_user_event(
                &h.owner.user_id,
                DownloadEvent::Progress(vec![unresolved_row(h.row, &h.download_base, why)]),
            )
            .await;
            registry.release(&h.key, &h.owner);
            return;
        }
    };
    if let Err(e) = backend
        .cancel_download(&h.row.source, &h.row.id, true)
        .await
    {
        warn!(
            "Failed to remove finished transfer {} from slskd: {}",
            h.row.id, e
        );
    }
    let job = import_job(
        path,
        ImportMode::Singleton,
        vec![h.row],
        &h.target,
        &h.owner.user_id,
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

/// The transfer slskd lists under `id` from `source`.
pub(crate) fn transfer_by_id<'a>(
    downloads: &'a [DownloadProgress],
    source: &str,
    id: &str,
) -> Option<&'a DownloadProgress> {
    downloads
        .iter()
        .find(|dl| dl.source == source && dl.id == id)
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

/// Normalize a filename for comparison purposes.
/// Handles Windows/Unix path separator differences and case sensitivity.
fn normalize_filename(filename: &str) -> String {
    filename
        .replace('\\', "/")
        .to_lowercase()
        .trim()
        .to_string()
}

/// Check if two filenames match, accounting for path normalization.
pub fn filenames_match(a: &str, b: &str) -> bool {
    let norm_a = normalize_filename(a);
    let norm_b = normalize_filename(b);

    // Exact match after normalization
    if norm_a == norm_b {
        return true;
    }

    // Check if one ends with the other (handles partial paths)
    if norm_a.ends_with(&norm_b) || norm_b.ends_with(&norm_a) {
        return true;
    }

    // Extract just the filename portion and compare
    let file_a = norm_a.rsplit('/').next().unwrap_or(&norm_a);
    let file_b = norm_b.rsplit('/').next().unwrap_or(&norm_b);

    file_a == file_b
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::server_fns::download::enqueue::{monitor_stopped, MONITOR_STOPPED};
    use crate::server_fns::download::test_support::{
        batch, item, key, transfer, Cancel, ScriptedOutcome, StubBackend,
    };
    use shared::failure::{Service, UserActionCode};
    use soulbeet::error::MissingSetting;
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

    fn watched(source: &str, filename: &str) -> WatchedTransfer {
        WatchedTransfer {
            source: source.to_string(),
            filename: filename.to_string(),
            transfer_id: id_of(filename),
        }
    }

    /// A monitor watching one track with two alternates behind it, plus the
    /// receiver its events land in. Holding the receiver keeps the broadcast
    /// live, so tests see what the UI would.
    fn failover_monitor() -> (DownloadMonitor, broadcast::Receiver<DownloadEvent>) {
        let (tx, rx) = broadcast::channel(16);
        let m = monitor_on(tx, vec!["peer_a"], vec!["dirA/aaa.flac"]).with_failover(
            SourcePool::single(vec![
                item("peer_a", "Wake Up", "dirA/aaa.flac"),
                item("peer_b", "Wake Up", "dirB/bbb.flac"),
                item("peer_c", "Wake Up", "dirC/ccc.flac"),
            ]),
        );
        (m, rx)
    }

    fn monitor(sources: Vec<&str>, filenames: Vec<&str>) -> DownloadMonitor {
        let (tx, _rx) = broadcast::channel(16);
        monitor_on(tx, sources, filenames)
    }

    fn monitor_on(
        tx: broadcast::Sender<DownloadEvent>,
        sources: Vec<&str>,
        filenames: Vec<&str>,
    ) -> DownloadMonitor {
        DownloadMonitor::new(
            sources
                .into_iter()
                .zip(filenames)
                .map(|(source, filename)| watched(source, filename))
                .collect(),
            PathBuf::from("/music"),
            tx,
            batch("X"),
        )
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
        m.process_tracks(&matched, backend).await;
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
    async fn one_failure_fails_over_once_per_poll() {
        // process_tracks and handle_absent_tracks run back to back on the SAME
        // poll. Once process_tracks has rebound the slot, the alternate's
        // transfer cannot be in that already-fetched list, so a slot whose
        // absence window ran from the batch's start would be declared "never
        // appeared" and fail over a second time in the same poll, orphaning
        // the transfer just enqueued.
        let (mut m, _rx) = failover_monitor();
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
        let (mut m, mut rx) = failover_monitor();
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
        let (mut m, _rx) = failover_monitor();
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

    #[tokio::test]
    async fn an_adopted_transfer_is_not_looked_for_in_the_batch_folder() {
        // Another peer's live transfer of the file was not enqueued by this
        // batch, so slskd saves it wherever its own enqueue said.
        let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let retried = transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::InProgress);

        let matched = m.find_matching_downloads(&[retried], &backend);

        assert_eq!(matched.len(), 1);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(m.tracked_files[0].transfer_id, "t-b");
        assert_eq!(m.tracked_files[0].destination, None);
        assert!(
            !TransferRegistry::global()
                .may_adopt(&key(&backend, "peer_b", "dirB/aaa.flac"), &batch("Y").owner),
            "the adopted transfer is now the batch's"
        );
    }

    #[tokio::test]
    async fn an_unowned_transfer_from_another_user_is_not_adopted() {
        // A live transfer of the same file that another batch owns has its
        // own monitor, whether it is another user's or the same user's.
        for owner in [batch("Theirs"), {
            let mut mine = batch("Mine");
            mine.owner.user_id = UserId::new("tester");
            mine
        }] {
            let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
            m.batch.owner.user_id = UserId::new("tester");
            let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
            TransferRegistry::global().claim(&[key(&backend, "peer_b", "dirB/aaa.flac")], &owner);
            let theirs = transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::InProgress);

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
        // Two slots of one batch hold the same basename from different peers.
        // The second slot's transfer is the batch's own, but not the first's.
        let mut m = monitor(vec!["peer_a", "peer_b"], vec!["a/01.flac", "b/01.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);

        let matched = m.find_matching_downloads(
            &[listed("peer_b", "b/01.flac", DownloadState::InProgress)],
            &backend,
        );

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, 1);
        assert_eq!(m.tracked_files[0].source, "peer_a");
    }

    #[tokio::test]
    async fn slots_with_the_same_basename_match_their_own_ids() {
        let mut m = monitor(
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
        let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.find_matching_downloads(
            &[transfer(
                "peer_b",
                "dirB/aaa.flac",
                "t-b",
                DownloadState::InProgress,
            )],
            &backend,
        );

        // The abandoned peer's record shows up again: it is no longer this
        // slot's transfer.
        let matched = m.find_matching_downloads(
            &[
                listed("peer_a", "dirA/aaa.flac", DownloadState::Completed),
                transfer("peer_b", "dirB/aaa.flac", "t-b", DownloadState::Completed),
            ],
            &backend,
        );

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].1.id, "t-b");
    }

    #[tokio::test]
    async fn cleanup_removes_this_batchs_failed_transfers_by_id() {
        let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
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
        let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
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
        let mut m = monitor(vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].first_seen = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
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
        let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
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
        let mut m = monitor_on(tx, vec!["peer_a", "peer_a"], vec!["a.flac", "b.flac"]);
        let mut invalid = 0;
        let circuit_open = || SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".to_string(),
        };

        assert!(
            !m.poll_failed(circuit_open(), &mut invalid),
            "gave up while slskd was fine"
        );

        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));
        assert!(
            m.poll_failed(circuit_open(), &mut invalid),
            "polled a dead backend forever"
        );

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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));

        let refused = SoulseekError::Api {
            status: 401,
            message: String::new(),
        };
        assert!(m.poll_failed(refused, &mut 0));

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
    async fn an_unparseable_listing_fails_the_batch_after_its_fuse() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["a.flac"]);
        let drift =
            || SoulseekError::InvalidResponse("JSON parse error: missing field `id`".into());
        let mut invalid = 0;
        for _ in 1..MAX_INVALID_RESPONSES {
            assert!(!m.poll_failed(drift(), &mut invalid));
        }
        assert!(m.poll_failed(drift(), &mut invalid));

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
    }

    #[tokio::test]
    async fn a_timed_out_transfer_is_dropped_before_the_retry_starts() {
        // The abandoned peer is still sending at the per-track timeout.
        // Leaving it running would put two peers on one track, and whatever
        // it finally delivers lands under a path no slot maps to any more.
        let (mut m, _rx) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].first_seen = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(1)));

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
            let mut m = monitor_on(tx, vec!["peer_a", "peer_a"], vec!["one.flac", "two.flac"]);
            m.batch.import_mode = match album_mode {
                true => ImportMode::Album,
                false => ImportMode::Singleton,
            };
            let stub = StubBackend::new(vec![]);
            let backend: Arc<dyn DownloadBackend> = stub.clone();
            let held = key(&backend, "peer_a", "one.flac");
            TransferRegistry::global().claim(std::slice::from_ref(&held), &m.batch);
            m.track_states[0].first_seen = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(60)));
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
        }
    }

    #[tokio::test]
    async fn a_slot_waiting_in_the_source_queue_too_long_fails_over() {
        let (mut m, _rx) = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].first_seen = Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));
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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].first_seen = Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));
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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].first_seen = Some(ago(REMOTE_QUEUE_TIMEOUT + Duration::from_secs(1)));
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
    async fn the_remote_queue_clock_starts_at_the_first_sighting() {
        let mut m = monitor(vec!["peer_a"], vec!["one.flac"]);
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
        let (mut m, _rx) = failover_monitor();
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
        // No watcher's clock bounds another peer's record, so one cancelled
        // during slskd's retry delay would read Queued for good.
        let (mut m, _rx) = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        poll_tracks(
            &mut m,
            &[
                failed_transfer("peer_a", "dirA/aaa.flac"),
                pending_retry("peer_x", "dirX/aaa.flac"),
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
        let (mut m, _rx) = failover_monitor();
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
        let (mut m, mut rx) = failover_monitor();
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
    async fn a_single_track_batch_that_never_appears_fails_over() {
        // A lone track has no sibling to keep the batch non-empty, so
        // process_tracks and handle_absent_tracks never see it: the empty
        // poll fuse is the only place its failover can fire.
        let (mut m, _rx) = failover_monitor();
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
        let (mut m, mut rx) = failover_monitor();
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
    }

    #[tokio::test]
    async fn a_vanished_transfer_fails_with_transfer_disappeared() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["one.flac"]);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.track_states[0].first_seen = Some(ago(Duration::from_secs(5)));
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

    #[test]
    fn monitor_failures_keep_their_text() {
        use RetryAdvice::*;
        let e = SoulseekError::InvalidResponse("bad json".into());
        let unreadable = format!("Could not read slskd downloads list: {e}");
        for (f, code, retry, message) in [
            (
                never_appeared(),
                FailureCode::TransferNeverAppeared,
                OtherSource,
                "Download never appeared in slskd",
            ),
            (
                disappeared(),
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
                stalled(&e),
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
                list_unreadable(&e),
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
        BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::ApiKey))
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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["dirA/fail.flac"]);
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
        let (mut m, _rx) = failover_monitor();
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
        let (mut m, mut rx) = failover_monitor();
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
        assert!(TransferRegistry::global().may_adopt(&own, &batch("Y").owner));
    }

    #[tokio::test]
    async fn a_cancel_that_arrives_during_failover_follows_the_slot() {
        let (mut m, mut rx) = failover_monitor();
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
    async fn a_finished_transfer_is_imported_even_when_cancelled() {
        let user = UserId::new("monitor-cancel-finished");
        let (mut m, _base, _rx) = disk_monitor(&user, &["one.flac"], &["one.flac"]).await;
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
    ) {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("not-a-dir"), b"x").unwrap();
        let (tx, _rx) = broadcast::channel(64);
        let mut m = monitor_on(tx, vec!["peer_a"; watched.len()], watched.to_vec());
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
        (m, base, user_tx.subscribe())
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
        let (mut m, _base, _imports) = disk_monitor(
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
        let (mut m, base, _imports) = disk_monitor(
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
        let (mut m, base, mut imports) = disk_monitor(
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

    #[tokio::test]
    async fn album_mode_removes_only_its_own_ids() {
        let (mut m, _base, _imports) = disk_monitor(
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
        let (mut m, _base, mut imports) =
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
    }

    #[tokio::test]
    async fn monitor_rows_reach_the_channel_of_the_batch_owner_id() {
        let alice = UserId::new("id-alice-monitor");
        let (mut m, _base, mut imports) = disk_monitor(&alice, &["01.flac"], &["01.flac"]).await;
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
            let (mut m, _base, mut imports) = disk_monitor(&user, &["01.flac"], &["01.flac"]).await;
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
        let (mut m, _base, mut imports) = disk_monitor(
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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["dirA/aaa.flac"]);
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
    async fn the_monitor_waits_for_reserved_slots_before_finishing_an_album() {
        let (mut m, _base, mut imports) = disk_monitor(
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
        let mut m = monitor_on(tx, vec!["peer_a"], vec!["01.flac"]);
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
    }

    #[tokio::test]
    async fn a_dropped_monitor_closes_its_inbox_and_frees_its_keys() {
        let (tx, mut rx) = broadcast::channel(16);
        let m = monitor_on(tx, vec!["peer_a"], vec!["01.flac"]);
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
