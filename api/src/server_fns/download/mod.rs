use dioxus::fullstack::{CborEncoding, Streaming};
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::slskd::{DownloadResponse, DownloadState, FileEntry, TrackResult};

#[cfg(feature = "server")]
use tokio::sync::broadcast;

use crate::server_fns::server_error;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::{SLSKD_CLIENT, USER_CHANNELS};

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

#[cfg(feature = "server")]
async fn slskd_download(tracks: Vec<TrackResult>) -> Result<Vec<DownloadResponse>, ServerFnError> {
    SLSKD_CLIENT.download(tracks).await.map_err(server_error)
}

#[get("/api/downloads/updates", auth: AuthSession)]
pub async fn download_updates_stream(
) -> Result<Streaming<Vec<FileEntry>, CborEncoding>, ServerFnError> {
    let username = auth.0.username;

    let rx = {
        let mut map = USER_CHANNELS.write().await;
        let tx = map.entry(username.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        tx.subscribe()
    };

    Ok(Streaming::spawn(move |tx_stream| async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(downloads) => {
                    if tx_stream.unbounded_send(downloads).is_err() {
                        // Client disconnected
                        info!("Download updates stream closed (client disconnected)");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Client couldn't keep up, some messages were dropped
                    // This is recoverable - just continue receiving
                    warn!(
                        "Download updates stream lagged, skipped {} messages",
                        skipped
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // All senders have been dropped - channel is closed
                    // This shouldn't normally happen since we keep senders in USER_CHANNELS
                    info!("Download updates broadcast channel closed");
                    break;
                }
            }
        }
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

    let tx = {
        let mut map = USER_CHANNELS.write().await;
        map.entry(username.clone())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(100);
                tx
            })
            .clone()
    };

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

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut attempts = 0;
        let mut consecutive_empty = 0;
        const MAX_ATTEMPTS: usize = 600; // ~20 minutes timeout
        const MAX_CONSECUTIVE_EMPTY: usize = 5; // Allow some grace period for downloads to appear

        // Poll immediately on first iteration, then wait for interval
        interval.tick().await;

        loop {
            attempts += 1;

            if attempts > MAX_ATTEMPTS {
                info!(
                    "Download monitoring timed out for batch {:?}",
                    download_filenames
                );
                break;
            }

            match SLSKD_CLIENT.get_all_downloads().await {
                Ok(downloads) => {
                    // Debug: log what slskd returns vs what we're looking for
                    if attempts <= 2 {
                        info!("Looking for filenames: {:?}", download_filenames);
                        let slskd_filenames: Vec<_> = downloads.iter().map(|f| &f.filename).collect();
                        info!("slskd returned {} downloads: {:?}", downloads.len(), slskd_filenames);
                    }

                    let batch_status: Vec<_> = downloads
                        .iter()
                        .filter(|file| download_filenames.contains(&file.filename))
                        .cloned()
                        .collect();

                    info!("batch_status has {} items", batch_status.len());

                    if !batch_status.is_empty() {
                        let send_result = tx.send(batch_status.clone());
                        info!("tx.send result: {:?} (receiver count)", send_result);
                        consecutive_empty = 0; // Reset counter when we find downloads
                    }

                    // Allow grace period for downloads to appear in slskd
                    if batch_status.is_empty() {
                        consecutive_empty += 1;
                        if consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                            info!("No active downloads found for batch after {} attempts, assuming completed or lost.", MAX_CONSECUTIVE_EMPTY);
                            break;
                        }
                        info!("No downloads found yet, attempt {}/{}", consecutive_empty, MAX_CONSECUTIVE_EMPTY);
                        interval.tick().await;
                        continue;
                    }

                    // TODO: Parallelize imports, do not wait for all to finish downloading before starting imports
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
                        let successful_downloads: Vec<_> = batch_status
                            .iter()
                            .filter(|d| {
                                d.state
                                    .iter()
                                    .any(|s| matches!(s, DownloadState::Downloaded))
                            })
                            .cloned()
                            .collect();

                        process_downloads(successful_downloads, target_path.clone(), tx.clone())
                            .await;
                        break;
                    }
                }
                Err(e) => {
                    info!("Error fetching download status: {}", e);
                }
            }

            // Wait before next poll
            interval.tick().await;
        }
    });

    Ok(res)
}
