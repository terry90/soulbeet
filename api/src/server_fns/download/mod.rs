use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use shared::slskd::{DownloadResponse, FileEntry, TrackResult};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{debug, info, warn};
#[cfg(feature = "server")]
use shared::slskd::DownloadState;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

use crate::server_fns::server_error;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::{
    cleanup_stale_channels, get_or_create_user_channel, register_user_task, unregister_user_task,
    SLSKD_CLIENT, USER_CHANNELS,
};

#[cfg(feature = "server")]
use chrono::Utc;

#[cfg(feature = "server")]
use uuid::Uuid;

// Import local modules
#[cfg(feature = "server")]
pub mod import;
#[cfg(feature = "server")]
pub mod process;
#[cfg(feature = "server")]
pub mod utils;

#[cfg(feature = "server")]
use self::process::process_downloads;

/// Normalize a filename for comparison purposes.
/// Handles Windows/Unix path separator differences and case sensitivity.
#[cfg(feature = "server")]
fn normalize_filename(filename: &str) -> String {
    filename
        .replace('\\', "/")
        .to_lowercase()
        .trim()
        .to_string()
}

/// Check if two filenames match, accounting for path normalization.
#[cfg(feature = "server")]
fn filenames_match(a: &str, b: &str) -> bool {
    let norm_a = normalize_filename(a);
    let norm_b = normalize_filename(b);

    // Exact match after normalization
    if norm_a == norm_b {
        return true;
    }

    // Check if one ends with the other (handles partial paths)
    // e.g., "Music/Artist/Album/track.mp3" matches "Artist/Album/track.mp3"
    if norm_a.ends_with(&norm_b) || norm_b.ends_with(&norm_a) {
        return true;
    }

    // Extract just the filename portion and compare
    let file_a = norm_a.rsplit('/').next().unwrap_or(&norm_a);
    let file_b = norm_b.rsplit('/').next().unwrap_or(&norm_b);

    file_a == file_b
}

/// Find matching downloads from slskd response for our queued files.
/// Uses fuzzy matching to handle path normalization issues.
#[cfg(feature = "server")]
fn find_matching_downloads<'a>(
    downloads: &'a [FileEntry],
    target_filenames: &[String],
) -> Vec<&'a FileEntry> {
    let mut matched = Vec::new();

    for download in downloads {
        for target in target_filenames {
            if filenames_match(&download.filename, target) {
                matched.push(download);
                break;
            }
        }
    }

    matched
}

#[cfg(feature = "server")]
async fn slskd_download(tracks: Vec<TrackResult>) -> Result<Vec<DownloadResponse>, ServerFnError> {
    SLSKD_CLIENT.download(tracks).await.map_err(server_error)
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
#[get("/api/downloads/updates", auth: AuthSession)]
pub async fn download_updates_ws(
    options: WebSocketOptions,
) -> Result<Websocket<(), Vec<FileEntry>>, ServerFnError> {
    let username = auth.0.username;

    let rx = {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(&username) {
            channel.sender.subscribe()
        } else {
            drop(map);
            let mut map = USER_CHANNELS.write().await;
            let channel = map
                .entry(username.clone())
                .or_insert_with(crate::globals::UserChannel::new);
            channel.sender.subscribe()
        }
    };

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut rx = rx;
        info!("WebSocket connected for user: {}", username);

        loop {
            // handle both broadcast messages and potential socket closure
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(downloads) => {
                            if socket.send(downloads).await.is_err() {
                                info!("WebSocket closed (client disconnected)");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                "Download updates lagged, skipped {} messages. Consider reducing download batch size.",
                                skipped
                            );
                            // lagged is recoverable - continue receiving
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Broadcast channel closed");
                            break;
                        }
                    }
                }
                _ = socket.recv() => {
                    // Client sent something or disconnected
                    // We don't expect client messages, but this helps detect closure
                }
            }
        }

        info!("WebSocket disconnected for user: {}", username);

        // Trigger cleanup of stale channels periodically
        cleanup_stale_channels().await;
    }))
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(
    tracks: Vec<TrackResult>,
    target_folder: String,
) -> Result<Vec<DownloadResponse>, ServerFnError> {
    let username = auth.0.username;

    let target_path_buf = std::path::Path::new(&target_folder).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let res = slskd_download(tracks).await?;

    let (failed, successful): (Vec<_>, Vec<_>) =
        res.iter().cloned().partition(|d| d.error.is_some());

    // Get or create user channel with cancellation support
    let (tx, cancellation_token) = get_or_create_user_channel(&username).await;

    if !failed.is_empty() {
        let failed_entries: Vec<FileEntry> = failed
            .iter()
            .map(|d| FileEntry {
                id: Uuid::new_v4().to_string(),
                username: d.username.clone(),
                direction: "Download".to_string(),
                filename: d.filename.clone(),
                size: d.size,
                start_offset: 0,
                state: vec![DownloadState::Errored],
                state_description: d.error.clone().unwrap_or_default(),
                requested_at: Utc::now().to_rfc3339(),
                enqueued_at: None,
                started_at: None,
                ended_at: None,
                bytes_transferred: 0,
                average_speed: 0.0,
                bytes_remaining: d.size,
                elapsed_time: None,
                percent_complete: 0.0,
                remaining_time: None,
                exception: d.error.clone(),
            })
            .collect();

        let _ = tx.send(failed_entries);
    }

    let download_filenames: Vec<String> = successful.iter().map(|d| d.filename.clone()).collect();
    let target_path = target_path_buf;

    if download_filenames.is_empty() {
        return Ok(res);
    }

    // Send initial "Queued" state immediately so UI shows the downloads right away
    let queued_entries: Vec<FileEntry> = successful
        .iter()
        .map(|d| FileEntry {
            id: Uuid::new_v4().to_string(),
            username: d.username.clone(),
            direction: "Download".to_string(),
            filename: d.filename.clone(),
            size: d.size,
            start_offset: 0,
            state: vec![DownloadState::Queued],
            state_description: "Queued for download".to_string(),
            requested_at: Utc::now().to_rfc3339(),
            enqueued_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            ended_at: None,
            bytes_transferred: 0,
            average_speed: 0.0,
            bytes_remaining: d.size,
            elapsed_time: None,
            percent_complete: 0.0,
            remaining_time: None,
            exception: None,
        })
        .collect();

    let _ = tx.send(queued_entries);

    info!("Started monitoring downloads: {:?}", download_filenames);

    // Register this task for cleanup tracking
    let task_username = username.clone();
    let task_cancellation = register_user_task(&username).await;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut attempts = 0;
        let mut consecutive_empty = 0;
        const MAX_ATTEMPTS: usize = 600; // ~20 minutes timeout
        // Increased grace period: slskd can take time to queue downloads, especially for busy peers
        const MAX_CONSECUTIVE_EMPTY: usize = 15; // 30 seconds grace period (15 * 2s interval)

        // Poll immediately on first iteration, then wait for interval
        interval.tick().await;

        loop {
            // Check for cancellation
            if task_cancellation.is_cancelled() {
                info!("Download monitoring cancelled for batch {:?}", download_filenames);
                break;
            }

            attempts += 1;

            if attempts > MAX_ATTEMPTS {
                warn!(
                    "Download monitoring timed out after {} attempts for batch {:?}",
                    MAX_ATTEMPTS, download_filenames
                );
                // Send timeout notification
                let timeout_entries: Vec<FileEntry> = download_filenames
                    .iter()
                    .map(|filename| FileEntry {
                        id: Uuid::new_v4().to_string(),
                        username: String::new(),
                        direction: "Download".to_string(),
                        filename: filename.clone(),
                        size: 0,
                        start_offset: 0,
                        state: vec![DownloadState::Errored],
                        state_description: "Download monitoring timed out".to_string(),
                        requested_at: Utc::now().to_rfc3339(),
                        enqueued_at: None,
                        started_at: None,
                        ended_at: Some(Utc::now().to_rfc3339()),
                        bytes_transferred: 0,
                        average_speed: 0.0,
                        bytes_remaining: 0,
                        elapsed_time: None,
                        percent_complete: 0.0,
                        remaining_time: None,
                        exception: Some("Monitoring timed out".to_string()),
                    })
                    .collect();
                let _ = tx.send(timeout_entries);
                break;
            }

            match SLSKD_CLIENT.get_all_downloads().await {
                Ok(downloads) => {
                    // Debug: log what slskd returns vs what we're looking for (only first few attempts)
                    if attempts <= 3 {
                        debug!("Looking for filenames: {:?}", download_filenames);
                        let slskd_filenames: Vec<_> = downloads.iter().map(|f| &f.filename).collect();
                        debug!("slskd returned {} downloads: {:?}", downloads.len(), slskd_filenames);
                    }

                    // Use fuzzy filename matching to handle path normalization differences
                    let matched_downloads = find_matching_downloads(&downloads, &download_filenames);
                    let batch_status: Vec<FileEntry> = matched_downloads.into_iter().cloned().collect();

                    if attempts <= 3 || batch_status.len() != download_filenames.len() {
                        info!(
                            "Matched {} of {} downloads from slskd (attempt {})",
                            batch_status.len(),
                            download_filenames.len(),
                            attempts
                        );
                        // Log any unmatched files for debugging
                        if batch_status.len() < download_filenames.len() {
                            for target in &download_filenames {
                                let found = downloads.iter().any(|d| filenames_match(&d.filename, target));
                                if !found {
                                    debug!("Unmatched file: {}", target);
                                }
                            }
                        }
                    }

                    if !batch_status.is_empty() {
                        if let Err(e) = tx.send(batch_status.clone()) {
                            // Log but don't fail - receivers might have disconnected
                            if tx.receiver_count() == 0 {
                                info!("No receivers for download updates, but continuing monitoring");
                            } else {
                                warn!("Failed to send download status update: {:?}", e);
                            }
                        }
                        consecutive_empty = 0; // Reset counter when we find downloads
                    }

                    // Allow grace period for downloads to appear in slskd
                    if batch_status.is_empty() {
                        consecutive_empty += 1;
                        if consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                            warn!(
                                "No active downloads found for batch after {} attempts ({}s), assuming completed or lost: {:?}",
                                MAX_CONSECUTIVE_EMPTY,
                                MAX_CONSECUTIVE_EMPTY * 2,
                                download_filenames
                            );
                            break;
                        }
                        if consecutive_empty % 5 == 0 {
                            info!(
                                "Waiting for downloads to appear in slskd, attempt {}/{} ({}/{}s)",
                                consecutive_empty,
                                MAX_CONSECUTIVE_EMPTY,
                                consecutive_empty * 2,
                                MAX_CONSECUTIVE_EMPTY * 2
                            );
                        }
                        interval.tick().await;
                        continue;
                    }

                    // Check if all downloads are finished
                    let all_finished = batch_status.iter().all(|d| {
                        d.state.iter().any(|s| {
                            matches!(
                                s,
                                DownloadState::Downloaded
                                    | DownloadState::Aborted
                                    | DownloadState::Cancelled
                                    | DownloadState::Errored
                            )
                        })
                    });

                    if all_finished {
                        info!("All downloads finished, starting import process");
                        let successful_downloads: Vec<_> = batch_status
                            .iter()
                            .filter(|d| {
                                d.state
                                    .iter()
                                    .any(|s| matches!(s, DownloadState::Downloaded))
                            })
                            .cloned()
                            .collect();

                        if successful_downloads.is_empty() {
                            warn!("All downloads failed or were cancelled, skipping import");
                        } else {
                            info!("Processing {} successful downloads", successful_downloads.len());
                            process_downloads(successful_downloads, target_path.clone(), tx.clone())
                                .await;
                        }
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error fetching download status from slskd: {}", e);
                    // Don't break on transient errors - slskd might recover
                }
            }

            // Wait before next poll
            interval.tick().await;
        }

        // Unregister this task
        unregister_user_task(&task_username).await;
        info!("Download monitoring task completed for user: {}", task_username);
    });

    Ok(res)
}
