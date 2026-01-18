//! Download monitoring logic for tracking slskd download progress.
//!
//! This module encapsulates the polling loop that monitors downloads from slskd,
//! handles per-track timeouts, and triggers processing when downloads complete.

use dioxus::logger::tracing::{debug, info, warn};
use shared::slskd::{DownloadState, FileEntry};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::process::process_downloads;
use crate::config::CONFIG;
use crate::globals::SLSKD_CLIENT;

/// Poll interval for checking download status (2 seconds).
const POLL_INTERVAL_SECS: u64 = 2;

/// Grace period for downloads to appear in slskd (30 seconds = 15 * 2s intervals).
const MAX_CONSECUTIVE_EMPTY: usize = 15;

/// Per-track timeout duration (1 hour).
const PER_TRACK_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// State tracking for individual track downloads.
struct TrackState {
    /// When the track was first seen in slskd's download list.
    first_seen: Option<Instant>,
    /// Whether this track has been processed (imported or marked as failed).
    processed: bool,
}

/// Monitors download progress from slskd and triggers processing on completion.
pub struct DownloadMonitor {
    /// Filenames being monitored.
    filenames: Vec<String>,
    /// Target directory for imports.
    target_path: PathBuf,
    /// Broadcast sender for UI updates.
    tx: broadcast::Sender<Vec<FileEntry>>,
    /// Per-track state tracking.
    track_states: HashMap<String, TrackState>,
    /// Whether album mode is enabled.
    album_mode: bool,
    /// Cancellation token for graceful shutdown.
    cancellation_token: CancellationToken,
    /// Username for logging.
    username: String,
}

impl DownloadMonitor {
    /// Create a new download monitor.
    pub fn new(
        filenames: Vec<String>,
        target_path: PathBuf,
        tx: broadcast::Sender<Vec<FileEntry>>,
        cancellation_token: CancellationToken,
        username: String,
    ) -> Self {
        let track_states = filenames
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    TrackState {
                        first_seen: None,
                        processed: false,
                    },
                )
            })
            .collect();

        Self {
            filenames,
            target_path,
            tx,
            track_states,
            album_mode: CONFIG.is_album_mode(),
            cancellation_token,
            username,
        }
    }

    /// Run the monitoring loop until all downloads complete or timeout.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        let mut consecutive_empty = 0;
        let mut poll_count = 0;

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

            match SLSKD_CLIENT.get_all_downloads().await {
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

        info!(
            "Download monitoring task completed for user: {}",
            self.username
        );
    }

    /// Process a poll result from slskd.
    /// Returns true if monitoring should stop.
    async fn process_poll_result(
        &mut self,
        downloads: Vec<FileEntry>,
        consecutive_empty: &mut usize,
        poll_count: u32,
    ) -> bool {
        // Debug logging for first few polls
        if poll_count <= 3 {
            debug!("Looking for filenames: {:?}", self.filenames);
            let slskd_filenames: Vec<_> = downloads.iter().map(|f| &f.filename).collect();
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
            if *consecutive_empty % 5 == 0 {
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

        // Check completion
        self.check_completion(&batch_status).await
    }

    /// Find downloads matching our tracked filenames.
    fn find_matching_downloads(&self, downloads: &[FileEntry]) -> Vec<FileEntry> {
        let mut matched = Vec::new();
        for download in downloads {
            for target in &self.filenames {
                if filenames_match(&download.filename, target) {
                    matched.push(download.clone());
                    break;
                }
            }
        }
        matched
    }

    /// Log any unmatched files for debugging.
    fn log_unmatched_files(&self, downloads: &[FileEntry], batch_status: &[FileEntry]) {
        if batch_status.len() < self.filenames.len() {
            for target in &self.filenames {
                let found = downloads
                    .iter()
                    .any(|d| filenames_match(&d.filename, target));
                if !found {
                    debug!("Unmatched file: {}", target);
                }
            }
        }
    }

    /// Send status update to UI via broadcast channel.
    fn send_status_update(&self, batch_status: &[FileEntry]) {
        if let Err(e) = self.tx.send(batch_status.to_vec()) {
            if self.tx.receiver_count() == 0 {
                info!("No receivers for download updates, but continuing monitoring");
            } else {
                warn!("Failed to send download status update: {:?}", e);
            }
        }
    }

    /// Process each track, handling timeouts and completions.
    async fn process_tracks(&mut self, batch_status: &[FileEntry]) {
        for download in batch_status {
            let matching_key = self
                .track_states
                .keys()
                .find(|k| filenames_match(k, &download.filename))
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
                    if first_seen.elapsed() > PER_TRACK_TIMEOUT && !is_terminal_state(&download.state) {
                        warn!(
                            "Track timed out after {} minutes: {}",
                            first_seen.elapsed().as_secs() / 60,
                            download.filename
                        );
                        let _ = self.tx.send(vec![download.as_timeout()]);
                        self.track_states.get_mut(&key).unwrap().processed = true;
                        continue;
                    }
                }

                // Singleton mode: process completed tracks immediately
                if !self.album_mode && is_downloaded(&download.state) {
                    info!(
                        "Track completed, processing immediately (singleton mode): {}",
                        download.filename
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
                if is_terminal_state(&download.state) && !is_downloaded(&download.state) {
                    self.track_states.get_mut(&key).unwrap().processed = true;
                }
            }
        }
    }

    /// Check if all downloads are complete. Returns true if monitoring should stop.
    async fn check_completion(&mut self, batch_status: &[FileEntry]) -> bool {
        let all_processed = self.track_states.values().all(|s| s.processed);
        let all_terminal = self.filenames.iter().all(|fname| {
            batch_status
                .iter()
                .find(|d| filenames_match(&d.filename, fname))
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
    async fn process_album_mode(&mut self, batch_status: &[FileEntry]) {
        let successful: Vec<_> = batch_status
            .iter()
            .filter(|d| {
                is_downloaded(&d.state)
                    && self
                        .track_states
                        .keys()
                        .find(|k| filenames_match(k, &d.filename))
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

/// Check if a download state indicates a terminal state (complete or failed).
fn is_terminal_state(state: &[DownloadState]) -> bool {
    state.iter().any(|s| {
        matches!(
            s,
            DownloadState::Downloaded
                | DownloadState::Aborted
                | DownloadState::Cancelled
                | DownloadState::Errored
        )
    })
}

/// Check if a download state indicates successful download.
fn is_downloaded(state: &[DownloadState]) -> bool {
    state.iter().any(|s| matches!(s, DownloadState::Downloaded))
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
