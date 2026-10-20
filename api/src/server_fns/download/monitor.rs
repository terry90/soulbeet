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
use tokio_util::sync::CancellationToken;

use super::enqueue::{stopped_rows, transfer_key};
use super::failover::{SourcePool, MAX_SOURCES_PER_TRACK};
use super::process::{album_jobs, import_job, resolve_on_disk, unresolved_row, ResolvedSlot};
use super::utils::FileId;
use crate::config::CONFIG;
use crate::globals::send_user_event;
use crate::import_queue::IMPORT_QUEUE;
use crate::services::download_backend;
use crate::transfer_registry::{AddedSlot, BatchRef, TransferKey, TransferOwner, TransferRegistry};

/// Poll interval for checking download status (2 seconds).
const POLL_INTERVAL_SECS: u64 = 2;

/// Grace period for downloads to appear in slskd (30 seconds = 15 * 2s intervals).
const MAX_CONSECUTIVE_EMPTY: usize = 15;

/// Per-track timeout duration (1 hour).
const PER_TRACK_TIMEOUT: Duration = Duration::from_secs(60 * 60);

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
/// final. slskd 0.26 auto-retries failed downloads (3 attempts by default):
/// the failed state is persisted only briefly before the retry re-queues
/// the transfer as "Queued, Locally". Acting on a single sighting would
/// abandon a transfer slskd is about to retry.
///
/// Shared with discovery's poll loop, which waits on the same transfers and
/// so must judge them by the same rule.
pub(crate) const FAILED_STATE_CONFIRM: Duration = Duration::from_secs(4);

/// Row text for a slot that ran past `PER_TRACK_TIMEOUT` with no peer left.
const TIMED_OUT: &str = "Download timed out after 1 hour";

/// Where a slot is in its life. Only a `Watching` slot is looked at, shown
/// or rebound: the others belong to the import queue or are finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    /// Its transfer is being watched.
    Watching,
    /// Its file went to the import queue, which shows its row from then on.
    HandedOff,
    /// It ran past the per-track timeout and its transfer was cancelled.
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
    /// Alternate sources per slot; `None` disables failover.
    pool: Option<SourcePool>,
    /// Whether album mode is enabled.
    album_mode: bool,
    /// Cancellation token for graceful shutdown.
    cancellation_token: CancellationToken,
    /// Whose rows these are; imports report to this user's channel.
    username: String,
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
        cancellation_token: CancellationToken,
        username: String,
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

        Self {
            tracked_files,
            target_path,
            tx,
            track_states,
            pool: None,
            album_mode: CONFIG.is_album_mode(),
            cancellation_token,
            username,
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

    /// Give up when slskd's transfer list has been unreadable for too long.
    ///
    /// Transport errors are deliberately not fused by count, because a slskd
    /// restart resolves on its own, but they must be fused by time: while no
    /// list can be read, no per-track deadline is ever evaluated and the
    /// monitor polls for the life of the process (#78).
    fn fail_if_backend_stalled(&mut self) -> bool {
        if self.last_readable_poll.elapsed() <= BACKEND_STALL_TIMEOUT {
            return false;
        }
        let reason = format!(
            "slskd's transfer list was unreadable for {} minutes",
            BACKEND_STALL_TIMEOUT.as_secs() / 60
        );
        warn!("{} - giving up on batch {:?}", reason, self.filenames());
        self.fail_unprocessed_tracks(&reason);
        true
    }

    /// Attach alternate sources so failed transfers retry from another peer.
    pub fn with_failover(mut self, pool: SourcePool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Re-enqueue this slot from its next-best peer. Returns true when the
    /// slot was rebound, in which case the caller must not mark it failed.
    ///
    /// Never fires for cancellations: a user cancelling a download, or the
    /// monitor's own cleanup, must not spawn a fresh transfer.
    async fn try_failover(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        reason: &str,
    ) -> bool {
        if self.cancellation_token.is_cancelled() {
            return false;
        }
        let Some(pool) = self.pool.as_mut() else {
            return false;
        };
        let Some(queued) = pool.enqueue_next(slot, backend, &self.batch).await else {
            return false;
        };

        let attempt = pool.attempts_used(slot);
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
        if old_key != transfer_key(backend.id(), &queued.source, &queued.item) {
            TransferRegistry::global().release(&old_key, &self.batch.owner);
        }

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
            entry,
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

            if self.cancellation_token.is_cancelled() {
                info!(
                    "Download monitoring cancelled for batch {:?}",
                    self.filenames()
                );
                break;
            }

            if self.fail_if_backend_stalled() {
                break;
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
                        self.fail_unprocessed_tracks("Download backend unavailable");
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

                    // Widen to every peer only while a slot has nothing
                    // usable, so the unbounded list is read when it can
                    // actually tell us something new rather than every poll.
                    if self.any_slot_unresolved(&downloads) {
                        match backend.get_downloads().await {
                            Ok(all) => downloads = all,
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
                Err(e @ SoulseekError::InvalidResponse(_)) => {
                    // Schema drift: slskd answered but its downloads list no
                    // longer parses. That will not fix itself, so fail the
                    // batch with the real error instead of polling forever
                    // (pre-fix this surfaced as the misleading "Download
                    // never appeared in slskd", issue #73).
                    invalid_responses += 1;
                    warn!(
                        "Unparseable slskd downloads list ({}/{}): {}",
                        invalid_responses, MAX_INVALID_RESPONSES, e
                    );
                    if invalid_responses >= MAX_INVALID_RESPONSES {
                        self.fail_unprocessed_tracks(&format!(
                            "Could not read slskd downloads list: {e}"
                        ));
                        break;
                    }
                }
                Err(e) => {
                    // Transport errors: don't break, slskd might recover
                    warn!("Error fetching download status from slskd: {}", e);
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
            self.username
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
    /// that batch's to remove.
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

        for entry in downloads
            .iter()
            .filter(|entry| is_failed(&entry.state) && self.owns(entry))
        {
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

    /// Process a poll result from slskd.
    /// Returns true if monitoring should stop.
    async fn process_poll_result(
        &mut self,
        downloads: Vec<DownloadProgress>,
        consecutive_empty: &mut usize,
        poll_count: u32,
        backend: &Arc<dyn DownloadBackend>,
    ) -> bool {
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
    /// adopt.
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
                entry: self.stamp(retry.clone()),
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
                    if self.try_failover(slot, backend, "Transfer timed out").await {
                        continue;
                    }
                    self.abandon(slot, backend).await;
                    self.settle(slot, SlotPhase::TimedOut, backend.id());
                    let timeout_entry = DownloadProgress {
                        state: DownloadState::Failed(TIMED_OUT.into()),
                        error: Some("Per-track timeout".into()),
                        ..download.clone()
                    };
                    let entries = self.stamp_batch(vec![timeout_entry]);
                    let _ = self.tx.send(DownloadEvent::Progress(entries));
                    continue;
                }
            }

            // Singleton mode: process completed tracks immediately
            if !self.album_mode && is_completed(&download.state) {
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
            username: self.username.clone(),
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

            let absent_reason = match self.track_states[slot].first_seen {
                None if self.track_states[slot].bound_at.elapsed() > ABSENT_TRACK_TIMEOUT => {
                    Some("Download never appeared in slskd")
                }
                Some(_) => {
                    let missing_since = self.track_states[slot]
                        .missing_since
                        .get_or_insert_with(Instant::now);
                    (missing_since.elapsed() > ABSENT_TRACK_TIMEOUT)
                        .then_some("Download disappeared from slskd")
                }
                None => None,
            };

            if let Some(reason) = absent_reason {
                // Logged only once the track is really being written off:
                // a successful failover reports its own retry instead.
                if self.try_failover(slot, backend, reason).await {
                    continue;
                }
                warn!(
                    "{} after {}s, marking failed: {}",
                    reason,
                    ABSENT_TRACK_TIMEOUT.as_secs(),
                    self.tracked_files[slot].filename
                );
                self.settle(slot, SlotPhase::Settled, backend.id());
                failed.push(make_failed_progress(&self.tracked_files[slot], reason));
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
        const REASON: &str = "Download never appeared in slskd";
        let mut retried = false;
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if !self.watching(slot) {
                continue;
            }
            if self.try_failover(slot, backend, REASON).await {
                retried = true;
                continue;
            }
            // Without this, rows whose transfer never surfaced in slskd would
            // sit at "Queued" in the UI forever.
            self.settle(slot, SlotPhase::Settled, backend.id());
            failed.push(make_failed_progress(&self.tracked_files[slot], REASON));
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
    fn fail_unprocessed_tracks(&mut self, reason: &str) {
        let mut failed: Vec<DownloadProgress> = Vec::new();
        for slot in 0..self.tracked_files.len() {
            if self.watching(slot) {
                self.track_states[slot].phase = SlotPhase::Settled;
                failed.push(make_failed_progress(&self.tracked_files[slot], reason));
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
            if self.album_mode {
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
                Some(path) => {
                    resolved.push(ResolvedSlot {
                        row,
                        path,
                        destination,
                    });
                    resolved_slots.push(slot);
                }
                None => {
                    self.settle(slot, SlotPhase::Settled, backend.id());
                    missing.push(unresolved_row(row));
                }
            }
        }
        if !missing.is_empty() {
            send_user_event(&self.username, DownloadEvent::Progress(missing)).await;
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
            &self.username,
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
    username: String,
    download_base: PathBuf,
    claimed: Arc<Mutex<HashSet<FileId>>>,
}

/// Find the file on disk, remove its slskd record, import it and free its
/// key once the import is done. The record is only removed once the file was
/// found, so slskd has moved it out of its incomplete folder.
async fn hand_off(h: Handoff, backend: Arc<dyn DownloadBackend>) {
    let registry = TransferRegistry::global();
    let Some(path) = resolve_on_disk(
        &h.row.item,
        &h.download_base,
        h.destination.as_deref(),
        &h.claimed,
    )
    .await
    else {
        send_user_event(
            &h.username,
            DownloadEvent::Progress(vec![unresolved_row(h.row)]),
        )
        .await;
        registry.release(&h.key, &h.owner);
        return;
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
        &h.username,
        &h.download_base,
    );
    IMPORT_QUEUE.submit(job).await.wait().await;
    registry.release(&h.key, &h.owner);
}

/// Build a synthetic terminal progress entry for a track slskd no longer
/// reports, so the UI can settle its row.
fn make_failed_progress(tracked: &TrackedFile, reason: &str) -> DownloadProgress {
    DownloadProgress::failed(
        tracked.transfer_id.clone(),
        tracked.source.clone(),
        tracked.filename.clone(),
        reason.to_string(),
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
    use crate::server_fns::download::test_support::{
        batch, item, key, transfer, Cancel, ScriptedOutcome, StubBackend,
    };
    use tokio::sync::broadcast;

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
            CancellationToken::new(),
            "tester".to_string(),
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
        let matched = m.find_matching_downloads(listing, backend);
        m.process_tracks(&matched, backend).await;
        m.handle_absent_tracks(&matched, backend).await;
        m.send_status_update(&matched);
    }

    /// The states the monitor sent for `item`, in order.
    fn states_for(rx: &mut broadcast::Receiver<DownloadEvent>, item: &str) -> Vec<DownloadState> {
        let mut states = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DownloadEvent::Progress(rows) = event {
                states.extend(rows.into_iter().filter(|r| r.item == item).map(|r| r.state));
            }
        }
        states
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

        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 2);
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
            mine.owner.user_id = "tester".to_string();
            mine
        }] {
            let mut m = monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]);
            m.batch.owner.user_id = "tester".to_string();
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

        assert!(!m.fail_if_backend_stalled(), "gave up while slskd was fine");

        m.last_readable_poll = ago(BACKEND_STALL_TIMEOUT + Duration::from_secs(1));
        assert!(m.fail_if_backend_stalled(), "polled a dead backend forever");

        assert!(
            (0..2).all(|slot| phase(&m, slot) == SlotPhase::Settled),
            "a track was left unresolved, so its slot never reports back"
        );
        let Ok(DownloadEvent::Progress(entries)) = rx.try_recv() else {
            panic!("the UI was never told the batch died");
        };
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e
            .error
            .as_deref()
            .is_some_and(|msg| msg.contains("unreadable"))));
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
            m.album_mode = album_mode;
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

            assert_eq!(
                states_for(&mut rx, "one.flac"),
                vec![DownloadState::Failed(
                    "Download timed out after 1 hour".into()
                )],
                "album_mode={album_mode}"
            );
            assert!(stub
                .cancelled()
                .contains(&Cancel::only("peer_a", &id_of("one.flac"))));
            assert_eq!(phase(&m, 0), SlotPhase::TimedOut);
            assert!(TransferRegistry::global().may_adopt(&held, &batch("Y").owner));
        }
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
        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");

        // The rebind reset the window, so the next sweep leaves it alone.
        m.handle_absent_tracks(&[], &backend).await;
        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 2);
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
        let (mut m, _rx) = failover_monitor();
        // Every alternate is refused, so the pool yields nothing.
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut consecutive_empty = MAX_CONSECUTIVE_EMPTY - 1;

        let stop = m
            .process_poll_result(vec![], &mut consecutive_empty, 99, &backend)
            .await;

        assert!(stop);
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
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

        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 1);
        assert_eq!(m.tracked_files[0].source, "peer_a");
        assert_eq!(phase(&m, 0), SlotPhase::Settled);
    }

    /// A monitor of `username` watching `watched` from peer_a, whose finished
    /// files are found in a temporary download folder holding `on_disk`, and
    /// whose import target cannot be created, so a handed-off row comes
    /// straight back from the import queue as Failed on the user's channel
    /// without beets ever running.
    async fn disk_monitor(
        username: &str,
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
        m.album_mode = true;
        m.download_base = base.path().to_path_buf();
        m.target_path = base.path().join("not-a-dir").join("library");
        let folder = base.path().join(&m.batch.destination);
        std::fs::create_dir_all(&folder).unwrap();
        for file in on_disk {
            std::fs::write(folder.join(file), b"x").unwrap();
        }
        let (user_tx, _) = crate::globals::get_or_create_user_channel(username).await;
        m.username = username.to_string();
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
            "monitor-handed-off",
            &["one.flac", "two.flac"],
            &["one.flac"],
        )
        .await;
        m.tx = tx;
        m.album_mode = false;
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
        let (mut m, base, _imports) =
            disk_monitor("monitor-singleton-on-disk", &["one.flac"], &[]).await;
        m.album_mode = false;
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
            "monitor-album-on-disk",
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
        let (mut m, _base, _imports) =
            disk_monitor("monitor-own-ids", &["dirA/01.flac"], &["01.flac"]).await;
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
            disk_monitor("monitor-album-missing", &["01.flac"], &[]).await;
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
        assert_eq!(
            import_events(&mut imports)
                .into_iter()
                .flatten()
                .map(|r| r.state)
                .collect::<Vec<_>>(),
            vec![DownloadState::Failed("Could not resolve file path".into())]
        );
        assert!(TransferRegistry::global().may_adopt(&held, &batch("Y").owner));
    }

    #[tokio::test]
    async fn import_rows_carry_the_batch_id() {
        for album_mode in [false, true] {
            let username = format!("monitor-batch-rows-{album_mode}");
            let (mut m, _base, mut imports) =
                disk_monitor(&username, &["01.flac"], &["01.flac"]).await;
            m.album_mode = album_mode;
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
        let (mut m, _base, mut imports) =
            disk_monitor("monitor-added-slots", &["01.flac"], &["01.flac", "02.flac"]).await;
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        m.batch
            .inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![AddedSlot {
                source: "peer_b".to_string(),
                filename: "02.flac".to_string(),
                transfer_id: id_of("02.flac"),
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
    async fn the_monitor_waits_for_reserved_slots_before_finishing_an_album() {
        let (mut m, _base, mut imports) =
            disk_monitor("monitor-reserved-slots", &["01.flac"], &["01.flac"]).await;
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
            }])
            .expect("delivered");

        m.close_inbox_cancelling(&backend).await;

        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("the undrained slot was never written off");
        };
        assert_eq!(
            rows[0].error.as_deref(),
            Some(crate::server_fns::download::enqueue::MONITOR_STOPPED)
        );
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
