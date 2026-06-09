//! Download monitoring logic for tracking slskd download progress.
//!
//! This module encapsulates the polling loop that monitors downloads from slskd,
//! handles per-track timeouts, and triggers processing when downloads complete.

use dioxus::logger::tracing::{debug, info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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

/// State tracking for individual track downloads.
struct TrackState {
    /// When the track was first seen in slskd's download list.
    first_seen: Option<Instant>,
    /// When the track went missing from slskd's list after being seen.
    missing_since: Option<Instant>,
    /// Whether this track has been processed (imported or marked as failed).
    processed: bool,
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
    /// Filenames only (for legacy compatibility with process_downloads).
    filenames: Vec<String>,
    /// Target directory for imports.
    target_path: PathBuf,
    /// Broadcast sender for UI updates.
    tx: broadcast::Sender<DownloadEvent>,
    /// Per-track state tracking.
    track_states: HashMap<String, TrackState>,
    /// Whether album mode is enabled.
    album_mode: bool,
    /// Cancellation token for graceful shutdown.
    cancellation_token: CancellationToken,
    /// When monitoring started, for the never-appeared timeout.
    started_at: Instant,
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

        let track_states = filenames
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    TrackState {
                        first_seen: None,
                        missing_since: None,
                        processed: false,
                    },
                )
            })
            .collect();

        Self {
            tracked_files,
            filenames,
            target_path,
            tx,
            track_states,
            album_mode: CONFIG.is_album_mode(),
            cancellation_token,
            started_at: Instant::now(),
            username,
            batch_id,
            batch_label,
        }
    }

    /// Run the monitoring loop until all downloads complete or timeout.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        let mut consecutive_empty = 0;
        let mut poll_count = 0;
        let mut backend_failures: u32 = 0;

        // Poll immediately on first iteration
        interval.tick().await;

        loop {
            if self.cancellation_token.is_cancelled() {
                info!(
                    "Download monitoring cancelled for batch {:?}",
                    self.filenames
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
                    let should_break = self
                        .process_poll_result(downloads, &mut consecutive_empty, poll_count)
                        .await;
                    if should_break {
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error fetching download status from slskd: {}", e);
                    // Don't break on transient errors - slskd might recover
                }
            }

            interval.tick().await;
        }

        // Clear completed transfers from slskd so they don't interfere with future downloads
        if let Ok(backend) = download_backend(None).await {
            if let Err(e) = backend.clear_completed_downloads().await {
                warn!("Failed to clear completed downloads from slskd: {}", e);
            }
        }

        info!(
            "Download monitoring task completed for user: {}",
            self.username
        );
    }

    /// Process a poll result from slskd.
    /// Returns true if monitoring should stop.
    async fn process_poll_result(
        &mut self,
        downloads: Vec<DownloadProgress>,
        consecutive_empty: &mut usize,
        poll_count: u32,
    ) -> bool {
        // Debug logging for first few polls
        if poll_count <= 3 {
            debug!("Looking for filenames: {:?}", self.filenames);
            let slskd_filenames: Vec<_> = downloads.iter().map(|f| &f.item).collect();
            debug!(
                "slskd returned {} downloads: {:?}",
                downloads.len(),
                slskd_filenames
            );
        }

        // Match downloads using fuzzy filename matching
        let batch_status = self.find_matching_downloads(&downloads);

        if poll_count <= 3 || batch_status.len() != self.filenames.len() {
            info!(
                "Matched {} of {} downloads from slskd (poll {})",
                batch_status.len(),
                self.filenames.len(),
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
                    self.filenames
                );
                return true;
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
        self.process_tracks(&batch_status).await;

        // Fail tracks that never appeared or vanished from slskd's list,
        // so one absent track cannot stall the batch forever
        self.handle_absent_tracks(&batch_status);

        // Check completion
        self.check_completion(&batch_status).await
    }

    /// Find downloads matching our tracked files.
    ///
    /// Matches by source (peer username) AND filename. This prevents the
    /// monitor from confusing a stale completed transfer from a different
    /// peer with the active download being tracked.
    ///
    /// When the same file exists multiple times from the same peer (e.g.
    /// re-downloading a track), prefer the active entry over the stale one.
    ///
    /// When the tracked peer has no usable transfer (nothing, or only a
    /// failed one) but another peer has an active transfer of the same file,
    /// the download was retried from a different source: rebind tracking to
    /// the new peer. Stale terminal transfers from other peers stay ignored.
    fn find_matching_downloads(&mut self, downloads: &[DownloadProgress]) -> Vec<DownloadProgress> {
        let mut matched = Vec::new();
        let mut rebinds: Vec<(usize, String)> = Vec::new();
        for (idx, tracked) in self.tracked_files.iter().enumerate() {
            let mut best: Option<&DownloadProgress> = None;
            for dl in downloads {
                if dl.source != tracked.source || !filenames_match(&dl.item, &tracked.filename) {
                    continue;
                }
                match best {
                    None => best = Some(dl),
                    Some(prev)
                        if is_terminal_state(&prev.state)
                            && !is_terminal_state(&dl.state) =>
                    {
                        best = Some(dl);
                    }
                    _ => {}
                }
            }

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
            if let Some(state) = self.track_states.get_mut(&tracked.filename) {
                state.processed = false;
                state.first_seen = None;
                state.missing_since = None;
            }
        }

        matched
    }

    /// Log any unmatched files for debugging.
    fn log_unmatched_files(
        &self,
        downloads: &[DownloadProgress],
        batch_status: &[DownloadProgress],
    ) {
        if batch_status.len() < self.filenames.len() {
            for target in &self.filenames {
                let found = downloads.iter().any(|d| filenames_match(&d.item, target));
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
    async fn process_tracks(&mut self, batch_status: &[DownloadProgress]) {
        for download in batch_status {
            let matching_key = self
                .track_states
                .keys()
                .find(|k| filenames_match(k, &download.item))
                .cloned();

            if let Some(key) = matching_key {
                // Record first seen time
                if self.track_states[&key].first_seen.is_none() {
                    self.track_states.get_mut(&key).unwrap().first_seen = Some(Instant::now());
                }

                // Skip already processed tracks
                if self.track_states[&key].processed {
                    continue;
                }

                // Check per-track timeout
                if let Some(first_seen) = self.track_states[&key].first_seen {
                    if first_seen.elapsed() > PER_TRACK_TIMEOUT
                        && !is_terminal_state(&download.state)
                    {
                        warn!(
                            "Track timed out after {} minutes: {}",
                            first_seen.elapsed().as_secs() / 60,
                            download.item
                        );
                        let timeout_entry = DownloadProgress {
                            state: DownloadState::Failed("Download timed out after 1 hour".into()),
                            error: Some("Per-track timeout".into()),
                            ..download.clone()
                        };
                        let entries = self.stamp_batch(vec![timeout_entry]);
                        let _ = self.tx.send(DownloadEvent::Progress(entries));
                        self.track_states.get_mut(&key).unwrap().processed = true;
                        continue;
                    }
                }

                // Singleton mode: process completed tracks immediately
                if !self.album_mode && is_completed(&download.state) {
                    info!(
                        "Track completed, processing immediately (singleton mode): {}",
                        download.item
                    );
                    self.track_states.get_mut(&key).unwrap().processed = true;
                    let dl = download.clone();
                    let tp = self.target_path.clone();
                    let tx_clone = self.tx.clone();
                    tokio::spawn(async move {
                        process_downloads(vec![dl], tp, tx_clone).await;
                    });
                }

                // Mark terminal states (errored/cancelled/aborted) as processed
                if is_terminal_state(&download.state) && !is_completed(&download.state) {
                    self.track_states.get_mut(&key).unwrap().processed = true;
                }
            }
        }
    }

    /// Fail tracks that are absent from slskd's transfer list: either they
    /// never appeared (rejected/lost requests) or they vanished after being
    /// seen (transfer removed). Each gets a terminal Failed event so the UI
    /// and batch completion never wait on them forever.
    fn handle_absent_tracks(&mut self, batch_status: &[DownloadProgress]) {
        let mut failed: Vec<DownloadProgress> = Vec::new();

        for tracked in &self.tracked_files {
            let Some(state) = self.track_states.get_mut(&tracked.filename) else {
                continue;
            };
            if state.processed {
                continue;
            }

            let present = batch_status
                .iter()
                .any(|d| filenames_match(&d.item, &tracked.filename));
            if present {
                state.missing_since = None;
                continue;
            }

            let absent_reason = match state.first_seen {
                None if self.started_at.elapsed() > ABSENT_TRACK_TIMEOUT => {
                    Some("Download never appeared in slskd")
                }
                Some(_) => {
                    let missing_since = state.missing_since.get_or_insert_with(Instant::now);
                    (missing_since.elapsed() > ABSENT_TRACK_TIMEOUT)
                        .then_some("Download disappeared from slskd")
                }
                None => None,
            };

            if let Some(reason) = absent_reason {
                warn!(
                    "{} after {}s, marking failed: {}",
                    reason,
                    ABSENT_TRACK_TIMEOUT.as_secs(),
                    tracked.filename
                );
                state.processed = true;
                failed.push(make_failed_progress(tracked, reason));
            }
        }

        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
    }

    /// Mark every unprocessed track as failed and notify the UI. Used when
    /// monitoring must stop early so downloads are never left dangling.
    fn fail_unprocessed_tracks(&mut self, reason: &str) {
        let mut failed: Vec<DownloadProgress> = Vec::new();
        for tracked in &self.tracked_files {
            if let Some(state) = self.track_states.get_mut(&tracked.filename) {
                if !state.processed {
                    state.processed = true;
                    failed.push(make_failed_progress(tracked, reason));
                }
            }
        }
        if !failed.is_empty() {
            let entries = self.stamp_batch(failed);
            let _ = self.tx.send(DownloadEvent::Progress(entries));
        }
    }

    /// Check if all downloads are complete. Returns true if monitoring should stop.
    async fn check_completion(&mut self, batch_status: &[DownloadProgress]) -> bool {
        let all_processed = self.track_states.values().all(|s| s.processed);
        let all_terminal = self.filenames.iter().all(|fname| {
            batch_status
                .iter()
                .find(|d| filenames_match(&d.item, fname))
                .map(|d| is_terminal_state(&d.state))
                .unwrap_or(false)
        });

        if all_processed || all_terminal {
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
                        .track_states
                        .keys()
                        .find(|k| filenames_match(k, &d.item))
                        .and_then(|k| self.track_states.get(k))
                        .map(|s| !s.processed)
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

/// Check if a download state indicates a terminal state (complete or failed).
fn is_terminal_state(state: &DownloadState) -> bool {
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
