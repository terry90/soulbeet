//! Download monitoring logic for tracking slskd download progress.
//!
//! This module encapsulates the polling loop that monitors downloads from slskd,
//! handles per-track timeouts, and triggers processing when downloads complete.

use dioxus::logger::tracing::{debug, info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use soulbeet::error::SoulseekError;
use soulbeet::DownloadBackend;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::failover::{SourcePool, MAX_SOURCES_PER_TRACK};
use super::process::process_downloads;
use crate::config::CONFIG;
use crate::services::download_backend;

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

/// How long a failed transfer state must persist before it is treated as
/// final. slskd 0.26 auto-retries failed downloads (3 attempts by default):
/// the failed state is persisted only briefly before the retry re-queues
/// the transfer as "Queued, Locally". Acting on a single sighting would
/// abandon a transfer slskd is about to retry.
///
/// Shared with discovery's poll loop, which waits on the same transfers and
/// so must judge them by the same rule.
pub(crate) const FAILED_STATE_CONFIRM: Duration = Duration::from_secs(4);

/// State tracking for individual track downloads.
struct TrackState {
    /// When this slot was bound to its current peer and remote path. Reset on
    /// every rebind, so the never-appeared window measures the age of THIS
    /// binding. A batch-wide clock would declare a slot that just failed over
    /// absent on the same poll, because the alternate's path cannot appear in
    /// a transfer list that was fetched before it was enqueued.
    bound_at: Instant,
    /// When the track was first seen in slskd's download list.
    first_seen: Option<Instant>,
    /// When the track went missing from slskd's list after being seen.
    missing_since: Option<Instant>,
    /// When the track's transfer was first seen in a failed terminal state.
    failing_since: Option<Instant>,
    /// Whether this track has been processed (imported or marked as failed).
    processed: bool,
}

impl TrackState {
    /// Fresh state for a slot bound to a peer right now. Every construction
    /// site is a binding or a rebinding, so there is no meaningful `Default`.
    fn bound_now() -> Self {
        Self {
            bound_at: Instant::now(),
            first_seen: None,
            missing_since: None,
            failing_since: None,
            processed: false,
        }
    }
}

/// A tracked download identified by source peer and filename.
#[derive(Clone, Debug)]
struct TrackedFile {
    source: String,
    filename: String,
}

/// Monitors download progress from slskd and triggers processing on completion.
pub struct DownloadMonitor {
    /// Files being monitored (source + filename pairs).
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
    /// Username for logging.
    username: String,
    /// Batch identifier for grouping downloads.
    batch_id: Option<String>,
    /// Human-readable batch label (album name).
    batch_label: Option<String>,
}

impl DownloadMonitor {
    /// Create a new download monitor.
    pub fn new(
        sources: Vec<String>,
        filenames: Vec<String>,
        target_path: PathBuf,
        tx: broadcast::Sender<DownloadEvent>,
        cancellation_token: CancellationToken,
        username: String,
        batch_id: Option<String>,
        batch_label: Option<String>,
    ) -> Self {
        let tracked_files: Vec<TrackedFile> = sources
            .into_iter()
            .zip(filenames.iter().cloned())
            .map(|(source, filename)| TrackedFile { source, filename })
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
            batch_id,
            batch_label,
        }
    }

    /// Attach alternate sources so failed transfers retry from another peer.
    pub fn with_failover(mut self, pool: SourcePool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Re-enqueue this slot from its next-best peer. Returns true when the
    /// slot was rebound, in which case the caller must not mark it failed.
    ///
    /// `transfers` is the current poll's view of slskd, used to resolve the
    /// transfer being abandoned so it can be stopped and removed.
    ///
    /// Never fires for cancellations: a user cancelling a download, or the
    /// monitor's own cleanup, must not spawn a fresh transfer.
    async fn try_failover(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        reason: &str,
        transfers: &[DownloadProgress],
    ) -> bool {
        if self.cancellation_token.is_cancelled() {
            return false;
        }
        let Some(pool) = self.pool.as_mut() else {
            return false;
        };
        let Some(queued) = pool.enqueue_next(slot, backend).await else {
            return false;
        };

        let attempt = pool.attempts_used(slot);
        let previous = self.tracked_files[slot].source.clone();
        info!(
            "{} for {} from {}, retrying from {} ({}/{})",
            reason,
            self.tracked_files[slot].filename,
            previous,
            queued.source,
            attempt,
            MAX_SOURCES_PER_TRACK
        );

        self.drop_abandoned_transfer(slot, backend, transfers).await;

        // The new peer serves this track under a completely different remote
        // path, and filenames_match will not bridge the two, so the slot must
        // track the new path outright or the monitor loses sight of it.
        self.tracked_files[slot] = TrackedFile {
            source: queued.source.clone(),
            filename: queued.item.clone(),
        };
        self.track_states[slot] = TrackState::bound_now();

        let entry = DownloadProgress::queued(queued.id, queued.source, queued.item, queued.size);
        let entries = self.stamp_batch(vec![entry]);
        let _ = self.tx.send(DownloadEvent::Progress(entries));
        true
    }

    /// Stop and delete the transfer a slot is about to stop tracking.
    ///
    /// At the per-track timeout that transfer is still running, so leaving it
    /// alone would have two peers sending the same track and only one of the
    /// two files reachable from a slot. Even a dead transfer has to go: once
    /// the slot no longer points at it nothing else prunes it, and it stays in
    /// slskd's list competing with the retry for the slot's own lookups.
    ///
    /// slskd deletes by transfer id, not by filename, so the id comes from the
    /// poll that triggered the failover. A slot that never appeared has no id
    /// to resolve, and a delete that fails is not worth losing the retry over.
    async fn drop_abandoned_transfer(
        &self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        transfers: &[DownloadProgress],
    ) {
        let tracked = &self.tracked_files[slot];
        let Some(abandoned) = best_match(transfers, &tracked.source, &tracked.filename) else {
            debug!(
                "No slskd transfer to remove for {} from {}",
                tracked.filename, tracked.source
            );
            return;
        };
        if let Err(e) = backend
            .cancel_download(&abandoned.source, &abandoned.id, true)
            .await
        {
            warn!(
                "Failed to remove abandoned transfer {} from slskd: {}",
                abandoned.id, e
            );
        }
    }

    /// Run the monitoring loop until all downloads complete or timeout.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        let mut consecutive_empty = 0;
        let mut poll_count = 0;
        let mut backend_failures: u32 = 0;
        let mut invalid_responses: u32 = 0;

        // Poll immediately on first iteration
        interval.tick().await;

        loop {
            if self.cancellation_token.is_cancelled() {
                info!(
                    "Download monitoring cancelled for batch {:?}",
                    self.filenames()
                );
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
            match backend.get_downloads().await {
                Ok(downloads) => {
                    invalid_responses = 0;
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

        // Remove this batch's terminal transfers from slskd so they don't
        // interfere with future downloads of the same files. Only this
        // batch's: clearing ALL completed transfers would race concurrent
        // batches and delete their finished-but-unprocessed entries.
        if let Ok(backend) = download_backend(None).await {
            self.remove_batch_transfers(&backend).await;
        }

        info!(
            "Download monitoring task completed for user: {}",
            self.username
        );
    }

    /// Remove the terminal slskd transfer records belonging to this batch.
    async fn remove_batch_transfers(&mut self, backend: &Arc<dyn DownloadBackend>) {
        let downloads = match backend.get_downloads().await {
            Ok(d) => d,
            Err(e) => {
                warn!("Could not list downloads for batch cleanup: {}", e);
                return;
            }
        };

        for entry in self.find_matching_downloads(&downloads) {
            if !is_terminal_state(&entry.state) {
                continue;
            }
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

        // Match downloads using fuzzy filename matching
        let batch_status = self.find_matching_downloads(&downloads);

        if poll_count <= 3 || batch_status.len() != self.tracked_files.len() {
            info!(
                "Matched {} of {} downloads from slskd (poll {})",
                batch_status.len(),
                self.tracked_files.len(),
                poll_count
            );
            self.log_unmatched_files(&downloads, &batch_status);
        }

        // Send status update to UI
        if !batch_status.is_empty() {
            self.send_status_update(&batch_status);
            *consecutive_empty = 0;
        }

        // Handle grace period for downloads to appear
        if batch_status.is_empty() {
            *consecutive_empty += 1;
            if *consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                warn!(
                    "No active downloads found for batch after {} attempts ({}s), assuming completed or lost: {:?}",
                    MAX_CONSECUTIVE_EMPTY,
                    MAX_CONSECUTIVE_EMPTY * 2,
                    self.filenames()
                );
                if self.retry_or_fail_absent_batch(backend).await {
                    return true;
                }
                // At least one slot moved to another peer. Its alternate
                // cannot show up in a list fetched before it was enqueued, so
                // the grace window starts again from here.
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
        self.process_tracks(&batch_status, backend).await;

        // Fail tracks that never appeared or vanished from slskd's list,
        // so one absent track cannot stall the batch forever
        self.handle_absent_tracks(&batch_status, backend).await;

        // Check completion
        self.check_completion(&batch_status).await
    }

    /// The slot a slskd transfer belongs to, by fuzzy filename match.
    fn slot_for_item(&self, item: &str) -> Option<usize> {
        self.tracked_files
            .iter()
            .position(|tracked| filenames_match(&tracked.filename, item))
    }

    /// Filenames currently tracked, in slot order.
    fn filenames(&self) -> Vec<String> {
        self.tracked_files
            .iter()
            .map(|tracked| tracked.filename.clone())
            .collect()
    }

    /// Find downloads matching our tracked files.
    ///
    /// Matches by source (peer username) AND filename. This prevents the
    /// monitor from confusing a stale completed transfer from a different
    /// peer with the active download being tracked.
    ///
    /// When the same file exists multiple times from the same peer (e.g.
    /// re-downloading a track), the entries are ranked by `best_match`, so a
    /// finished transfer wins over the failed one it replaced.
    ///
    /// When the tracked peer has no usable transfer (nothing, or only a
    /// failed one) but another peer has an active transfer of the same file,
    /// the download was retried from a different source: rebind tracking to
    /// the new peer. Stale terminal transfers from other peers stay ignored.
    fn find_matching_downloads(&mut self, downloads: &[DownloadProgress]) -> Vec<DownloadProgress> {
        let mut matched = Vec::new();
        let mut rebinds: Vec<(usize, String)> = Vec::new();
        for (idx, tracked) in self.tracked_files.iter().enumerate() {
            let mut best = best_match(downloads, &tracked.source, &tracked.filename);

            let unusable =
                best.is_none_or(|b| is_terminal_state(&b.state) && !is_completed(&b.state));
            if unusable {
                if let Some(retry) = downloads.iter().find(|dl| {
                    dl.source != tracked.source
                        && !is_terminal_state(&dl.state)
                        && filenames_match(&dl.item, &tracked.filename)
                }) {
                    rebinds.push((idx, retry.source.clone()));
                    best = Some(retry);
                }
            }

            if let Some(dl) = best {
                matched.push(dl.clone());
            }
        }

        for (idx, new_source) in rebinds {
            let tracked = &mut self.tracked_files[idx];
            info!(
                "Download of {} retried from new peer {} (was {}), rebinding",
                tracked.filename, new_source, tracked.source
            );
            tracked.source = new_source;
            // Reset state so the retried transfer is monitored and imported
            self.track_states[idx] = TrackState::bound_now();
        }

        matched
    }

    /// Log any unmatched files for debugging.
    fn log_unmatched_files(
        &self,
        downloads: &[DownloadProgress],
        batch_status: &[DownloadProgress],
    ) {
        if batch_status.len() < self.tracked_files.len() {
            for target in self.filenames() {
                let found = downloads.iter().any(|d| filenames_match(&d.item, &target));
                if !found {
                    debug!("Unmatched file: {}", target);
                }
            }
        }
    }

    /// Send status update to UI via broadcast channel.
    fn send_status_update(&self, batch_status: &[DownloadProgress]) {
        let entries = self.stamp_batch(batch_status.to_vec());
        if let Err(e) = self.tx.send(DownloadEvent::Progress(entries)) {
            if self.tx.receiver_count() == 0 {
                info!("No receivers for download updates, but continuing monitoring");
            } else {
                warn!("Failed to send download status update: {:?}", e);
            }
        }
    }

    /// Apply batch_id and batch_label to a set of progress entries.
    fn stamp_batch(&self, mut entries: Vec<DownloadProgress>) -> Vec<DownloadProgress> {
        if self.batch_id.is_some() || self.batch_label.is_some() {
            for entry in &mut entries {
                entry.batch_id.clone_from(&self.batch_id);
                entry.batch_label.clone_from(&self.batch_label);
            }
        }
        entries
    }

    /// Process each track, handling timeouts and completions.
    async fn process_tracks(
        &mut self,
        batch_status: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        for download in batch_status {
            let Some(slot) = self.slot_for_item(&download.item) else {
                continue;
            };

            if self.track_states[slot].first_seen.is_none() {
                self.track_states[slot].first_seen = Some(Instant::now());
            }

            if self.track_states[slot].processed {
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
                    if self
                        .try_failover(slot, backend, "Transfer timed out", batch_status)
                        .await
                    {
                        continue;
                    }
                    let timeout_entry = DownloadProgress {
                        state: DownloadState::Failed("Download timed out after 1 hour".into()),
                        error: Some("Per-track timeout".into()),
                        ..download.clone()
                    };
                    let entries = self.stamp_batch(vec![timeout_entry]);
                    let _ = self.tx.send(DownloadEvent::Progress(entries));
                    self.track_states[slot].processed = true;
                    continue;
                }
            }

            // Singleton mode: process completed tracks immediately
            if !self.album_mode && is_completed(&download.state) {
                info!(
                    "Track completed, processing immediately (singleton mode): {}",
                    download.item
                );
                self.track_states[slot].processed = true;
                let dl = download.clone();
                let tp = self.target_path.clone();
                let tx_clone = self.tx.clone();
                tokio::spawn(async move {
                    process_downloads(vec![dl], tp, tx_clone).await;
                });
            }

            // Mark terminal failures (errored/cancelled/aborted) as
            // processed, but only once the failure has persisted for
            // FAILED_STATE_CONFIRM: slskd 0.26 retries failures and
            // briefly reports the failed state before re-queueing the
            // transfer, and a re-queued transfer resets the clock.
            let failed_now = is_terminal_state(&download.state) && !is_completed(&download.state);
            if failed_now {
                let failing_since = self.track_states[slot]
                    .failing_since
                    .get_or_insert_with(Instant::now);
                if failing_since.elapsed() >= FAILED_STATE_CONFIRM {
                    let retryable = !matches!(download.state, DownloadState::Cancelled);
                    if !retryable
                        || !self
                            .try_failover(slot, backend, "Transfer failed", batch_status)
                            .await
                    {
                        self.track_states[slot].processed = true;
                    }
                }
            } else {
                self.track_states[slot].failing_since = None;
            }
        }
    }

    /// Fail tracks that are absent from slskd's transfer list: either they
    /// never appeared (rejected/lost requests) or they vanished after being
    /// seen (transfer removed). Each gets a terminal Failed event so the UI
    /// and batch completion never wait on them forever.
    async fn handle_absent_tracks(
        &mut self,
        batch_status: &[DownloadProgress],
        backend: &Arc<dyn DownloadBackend>,
    ) {
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if self.track_states[slot].processed {
                continue;
            }
            let filename = self.tracked_files[slot].filename.clone();

            let present = batch_status
                .iter()
                .any(|d| filenames_match(&d.item, &filename));
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
                if self.try_failover(slot, backend, reason, batch_status).await {
                    continue;
                }
                warn!(
                    "{} after {}s, marking failed: {}",
                    reason,
                    ABSENT_TRACK_TIMEOUT.as_secs(),
                    filename
                );
                self.track_states[slot].processed = true;
                failed.push(make_failed_progress(&self.tracked_files[slot], reason));
            }
        }

        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
    }

    /// Give every unsettled slot another peer after a batch's transfers never
    /// showed up in slskd at all, and write off only the ones with nowhere
    /// left to go. Returns true when nothing was retried, i.e. the batch is
    /// finished.
    ///
    /// This is the only failover route a single-track batch has on this path:
    /// while nothing of the batch matches, `process_poll_result` returns
    /// before `process_tracks` and `handle_absent_tracks` ever run, and the
    /// empty-poll fuse burns out at 30s, well inside the 120s those two wait.
    /// A batch with more than one track only reaches them because a sibling
    /// keeps the batch non-empty.
    async fn retry_or_fail_absent_batch(&mut self, backend: &Arc<dyn DownloadBackend>) -> bool {
        const REASON: &str = "Download never appeared in slskd";
        let mut retried = false;
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for slot in 0..self.tracked_files.len() {
            if self.track_states[slot].processed {
                continue;
            }
            // Nothing of this batch is in slskd's list, so there is no
            // transfer for the failover to drop.
            if self.try_failover(slot, backend, REASON, &[]).await {
                retried = true;
                continue;
            }
            // Without this, rows whose transfer never surfaced in slskd would
            // sit at "Queued" in the UI forever.
            self.track_states[slot].processed = true;
            failed.push(make_failed_progress(&self.tracked_files[slot], REASON));
        }

        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
        !retried
    }

    /// Mark every unprocessed track as failed and notify the UI. Used when
    /// monitoring must stop early so downloads are never left dangling.
    fn fail_unprocessed_tracks(&mut self, reason: &str) {
        let mut failed: Vec<DownloadProgress> = Vec::new();
        for slot in 0..self.tracked_files.len() {
            if !self.track_states[slot].processed {
                self.track_states[slot].processed = true;
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
    /// A track is settled when it was processed (imported, failed, or timed
    /// out) or its transfer completed successfully. Settled must be judged
    /// per track: in album mode completed tracks stay unprocessed until the
    /// whole batch is handled, so requiring all-processed or all-terminal
    /// across the batch would poll forever once one track is processed via
    /// timeout while the rest sit completed. Failed states settle through
    /// `processed` only, after process_tracks confirms the failure persisted
    /// (slskd 0.26 may retry it).
    async fn check_completion(&mut self, batch_status: &[DownloadProgress]) -> bool {
        let all_settled = (0..self.tracked_files.len()).all(|slot| {
            let processed = self.track_states[slot].processed;
            let completed = batch_status
                .iter()
                .find(|d| filenames_match(&d.item, &self.tracked_files[slot].filename))
                .map(|d| is_completed(&d.state))
                .unwrap_or(false);
            processed || completed
        });

        if all_settled {
            if self.album_mode {
                self.process_album_mode(batch_status).await;
            }
            info!("All downloads finished");
            return true;
        }

        false
    }

    /// Process all successful downloads together in album mode.
    async fn process_album_mode(&mut self, batch_status: &[DownloadProgress]) {
        let successful: Vec<_> = batch_status
            .iter()
            .filter(|d| {
                is_completed(&d.state)
                    && self
                        .slot_for_item(&d.item)
                        .map(|slot| !self.track_states[slot].processed)
                        .unwrap_or(false)
            })
            .cloned()
            .collect();

        if !successful.is_empty() {
            info!(
                "Album mode: Processing {} successful downloads together",
                successful.len()
            );
            process_downloads(successful, self.target_path.clone(), self.tx.clone()).await;
        } else {
            info!("Album mode: No successful downloads to process");
        }
    }
}

/// Build a synthetic terminal progress entry for a track slskd no longer
/// reports, so the UI can settle its row.
fn make_failed_progress(tracked: &TrackedFile, reason: &str) -> DownloadProgress {
    DownloadProgress {
        id: tracked.filename.clone(),
        source: tracked.source.clone(),
        item: tracked.filename.clone(),
        size: 0,
        transferred: 0,
        state: DownloadState::Failed(reason.to_string()),
        percent: 0.0,
        speed: 0.0,
        error: Some(reason.to_string()),
        backend: None,
        batch_id: None,
        batch_label: None,
    }
}

/// How good a slskd transfer is as the match for a tracked file, highest
/// first: still running beats finished, and finished beats failed.
///
/// Ranking failures last is what keeps a completed retry from being read as a
/// failure. A slot can fail over to the *same* peer under another path, since
/// a peer holding the track in two directories contributes two ranked items,
/// and `filenames_match` bridges those two paths by basename. If the dead
/// peer's transfer is still in slskd's list when the retry finishes, both
/// entries are terminal and both match the slot: preferring only non-terminal
/// entries would leave the order of slskd's list deciding, and a corpse
/// listed first would fail the track over again with its file already on disk.
pub(crate) fn transfer_match_rank(state: &DownloadState) -> u8 {
    match state {
        DownloadState::Failed(_) | DownloadState::Cancelled => 0,
        s if is_terminal_state(s) => 1,
        _ => 2,
    }
}

/// The transfer a peer/path pair is best matched by, or `None` when slskd
/// lists none. Ties keep the first entry so the choice is deterministic.
pub(crate) fn best_match<'a>(
    downloads: &'a [DownloadProgress],
    source: &str,
    filename: &str,
) -> Option<&'a DownloadProgress> {
    downloads
        .iter()
        .filter(|dl| dl.source == source && filenames_match(&dl.item, filename))
        .min_by_key(|dl| std::cmp::Reverse(transfer_match_rank(&dl.state)))
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
    use crate::server_fns::download::test_support::{item, ScriptedOutcome, StubBackend};
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

    /// A slskd transfer of `filename` from `source` in a failed terminal state.
    fn failed_transfer(source: &str, filename: &str) -> DownloadProgress {
        DownloadProgress {
            state: DownloadState::Failed("peer went away".into()),
            ..DownloadProgress::queued(
                filename.to_string(),
                source.to_string(),
                filename.to_string(),
                0,
            )
        }
    }

    /// A slskd transfer of `filename` from `source` that finished cleanly.
    fn completed_transfer(source: &str, filename: &str) -> DownloadProgress {
        DownloadProgress {
            state: DownloadState::Completed,
            ..DownloadProgress::queued(
                filename.to_string(),
                source.to_string(),
                filename.to_string(),
                0,
            )
        }
    }

    /// A monitor watching one track, with two alternates behind it.
    fn failover_monitor() -> DownloadMonitor {
        monitor(vec!["peer_a"], vec!["dirA/aaa.flac"]).with_failover(SourcePool::single(vec![
            item("peer_a", "Wake Up", "dirA/aaa.flac"),
            item("peer_b", "Wake Up", "dirB/bbb.flac"),
            item("peer_c", "Wake Up", "dirC/ccc.flac"),
        ]))
    }

    fn monitor(sources: Vec<&str>, filenames: Vec<&str>) -> DownloadMonitor {
        let (tx, _rx) = broadcast::channel(16);
        DownloadMonitor::new(
            sources.into_iter().map(String::from).collect(),
            filenames.into_iter().map(String::from).collect(),
            PathBuf::from("/tmp"),
            tx,
            CancellationToken::new(),
            "tester".to_string(),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn one_failure_fails_over_once_per_poll() {
        // process_tracks and handle_absent_tracks run back to back on the SAME
        // batch_status. Once process_tracks has rebound the slot, the
        // alternate's path cannot be in that already-fetched list, so a slot
        // whose absence window ran from the batch's start would be declared
        // "never appeared" and fail over a second time in the same poll,
        // orphaning the transfer just enqueued.
        let mut m = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();

        // Old enough that a batch-wide clock would consider it absent, with a
        // failure that has already outlived the confirmation window.
        m.track_states[0].bound_at = ago(ABSENT_TRACK_TIMEOUT + Duration::from_secs(1));
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        let batch_status = vec![failed_transfer("peer_a", "dirA/aaa.flac")];
        m.process_tracks(&batch_status, &backend).await;
        m.handle_absent_tracks(&batch_status, &backend).await;

        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 2);
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert_eq!(m.tracked_files[0].filename, "dirB/bbb.flac");
        assert!(!m.track_states[0].processed);
        // Only the first rebind had a transfer to drop: the second slot had
        // nothing in slskd's list to begin with, which is why it failed over.
        assert_eq!(
            stub.cancelled(),
            vec![("peer_a".to_string(), "dirA/aaa.flac".to_string())]
        );
    }

    #[tokio::test]
    async fn a_timed_out_transfer_is_dropped_before_the_retry_starts() {
        // The abandoned peer is still sending at the per-track timeout.
        // Leaving it running would put two peers on one track, and whatever
        // it finally delivers lands under a path no slot maps to any more.
        let mut m = failover_monitor();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        m.track_states[0].first_seen = Some(ago(PER_TRACK_TIMEOUT + Duration::from_secs(1)));

        let live = DownloadProgress {
            state: DownloadState::InProgress,
            ..DownloadProgress::queued(
                "transfer-1".to_string(),
                "peer_a".to_string(),
                "dirA/aaa.flac".to_string(),
                0,
            )
        };
        m.process_tracks(&[live], &backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![("peer_a".to_string(), "transfer-1".to_string())],
            "slskd cancels by transfer id, not by filename"
        );
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert!(!m.track_states[0].processed);
    }

    #[tokio::test]
    async fn the_absence_window_restarts_on_every_rebind() {
        // The same guarantee stated directly: a slot that has just been
        // rebound is young, however old the batch is.
        let mut m = failover_monitor();
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
    async fn a_single_track_batch_that_never_appears_fails_over() {
        // A lone track has no sibling to keep the batch non-empty, so
        // process_tracks and handle_absent_tracks never see it: the empty
        // poll fuse is the only place its failover can fire.
        let mut m = failover_monitor();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let mut consecutive_empty = MAX_CONSECUTIVE_EMPTY - 1;

        let stop = m
            .process_poll_result(vec![], &mut consecutive_empty, 99, &backend)
            .await;

        assert!(!stop, "the batch keeps polling for the new peer");
        assert_eq!(consecutive_empty, 0, "the grace window restarts");
        assert_eq!(m.tracked_files[0].source, "peer_b");
        assert!(!m.track_states[0].processed);
    }

    #[tokio::test]
    async fn a_never_appeared_batch_with_no_peers_left_is_still_written_off() {
        // The fallback has to stay reachable, or an absent batch with a dry
        // pool would poll forever.
        let mut m = failover_monitor();
        // Every alternate is refused, so the pool yields nothing.
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let mut consecutive_empty = MAX_CONSECUTIVE_EMPTY - 1;

        let stop = m
            .process_poll_result(vec![], &mut consecutive_empty, 99, &backend)
            .await;

        assert!(stop);
        assert!(m.track_states[0].processed);
    }

    #[tokio::test]
    async fn a_cancelled_transfer_settles_without_failing_over() {
        let mut m = failover_monitor();
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::Enqueued, ScriptedOutcome::Enqueued]);
        m.track_states[0].failing_since = Some(ago(FAILED_STATE_CONFIRM + Duration::from_secs(1)));

        let cancelled = DownloadProgress {
            state: DownloadState::Cancelled,
            ..failed_transfer("peer_a", "dirA/aaa.flac")
        };
        m.process_tracks(&[cancelled], &backend).await;

        assert_eq!(m.pool.as_ref().unwrap().attempts_used(0), 1);
        assert_eq!(m.tracked_files[0].source, "peer_a");
        assert!(m.track_states[0].processed);
    }

    #[test]
    fn a_completed_retry_outranks_the_dead_peer_it_replaced() {
        // The slot failed over to the same peer under a different path, so
        // both transfers match it on source and, through filenames_match's
        // basename rule, on path too. Both are terminal by the time the retry
        // lands, and the corpse is listed first. Picking it would fail the
        // track over again or write it off with its file already on disk.
        let mut m = monitor(vec!["peer_a"], vec!["b/07 - Wake Up.flac"]);
        let downloads = vec![
            failed_transfer("peer_a", "a/07 - Wake Up.flac"),
            completed_transfer("peer_a", "b/07 - Wake Up.flac"),
        ];

        let matched = m.find_matching_downloads(&downloads);

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].item, "b/07 - Wake Up.flac");
        assert_eq!(matched[0].state, DownloadState::Completed);
        assert_eq!(m.tracked_files[0].source, "peer_a", "no rebind was needed");
    }

    #[test]
    fn slot_lookup_survives_path_separator_and_case_drift() {
        let m = monitor(
            vec!["peer_a", "peer_b"],
            vec!["music\\A\\01 - One.flac", "music\\A\\02 - Two.flac"],
        );

        assert_eq!(m.slot_for_item("music/a/01 - one.flac"), Some(0));
        assert_eq!(m.slot_for_item("music\\A\\02 - Two.flac"), Some(1));
        assert_eq!(m.slot_for_item("music\\A\\03 - Three.flac"), None);
    }

    #[test]
    fn distinct_filenames_resolve_to_their_own_slots() {
        let m = monitor(
            vec!["peer_a", "peer_b"],
            vec![
                "music\\Kowloon\\Come Over (2021)\\07 - Wake Up.flac",
                "shared\\kowloon\\wake up.flac",
            ],
        );

        assert_eq!(
            m.slot_for_item("music\\Kowloon\\Come Over (2021)\\07 - Wake Up.flac"),
            Some(0)
        );
        assert_eq!(m.slot_for_item("shared\\kowloon\\wake up.flac"), Some(1));
    }

    #[test]
    fn ambiguous_suffix_match_resolves_to_the_lowest_slot() {
        // A short tracked path and a longer one that ends with it both
        // match the same incoming item via filenames_match's suffix rule.
        // The lookup has no way to prefer the more specific match, so it
        // must at least be deterministic: lowest slot wins, every time.
        let m = monitor(
            vec!["peer_a", "peer_b"],
            vec!["01 - One.flac", "music\\A\\01 - One.flac"],
        );

        assert_eq!(m.slot_for_item("music/A/01 - One.flac"), Some(0));
    }

    #[test]
    fn state_stays_bound_to_its_slot_after_a_filename_rebind() {
        // This is the property the whole refactor exists for: TrackState
        // lives at a slot index, so changing the remote path a slot tracks
        // neither strands its state nor migrates it to another slot, and the
        // slot is still found under the new path. What try_failover then does
        // with that state is its own decision: it replaces it deliberately,
        // to start the retry's windows from zero.
        let mut m = monitor(
            vec!["peer_a", "peer_b"],
            vec!["music\\A\\01 - One.flac", "music\\A\\02 - Two.flac"],
        );

        m.track_states[0].processed = true;
        m.track_states[0].first_seen = Some(Instant::now());

        // Simulate a failover rebind: the same slot now tracks a
        // completely different remote path from a different peer.
        m.tracked_files[0].filename = "totally\\different\\peer\\path.flac".to_string();

        assert!(m.track_states[0].processed);
        assert!(m.track_states[0].first_seen.is_some());
        assert_eq!(
            m.slot_for_item("totally\\different\\peer\\path.flac"),
            Some(0)
        );
    }
}
