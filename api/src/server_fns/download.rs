use dioxus::fullstack::{CborEncoding, Streaming};
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::slskd::{DownloadResponse, DownloadState, FileEntry, TrackResult};

#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use soulbeet::beets;

use super::server_error;

// Import the extractor we created in the previous step
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::{SLSKD_CLIENT, USER_CHANNELS};

#[cfg(feature = "server")]
use chrono::Utc;

#[cfg(feature = "server")]
use uuid::Uuid;

#[cfg(feature = "server")]
fn resolve_download_path(filename: &str, download_base: &std::path::Path) -> Option<String> {
    // Normalize path separators (win -> linux)
    let filename_str = filename.replace('\\', "/");
    let path = std::path::Path::new(&filename_str);
    let components: Vec<_> = path.components().collect();

    // Keep only the last directory and filename (d1/d2/d3/file -> d3/file)
    if components.len() >= 2 {
        let len = components.len();
        let last_dir = components[len - 2].as_os_str();
        let file_name = components[len - 1].as_os_str();

        let relative_path = std::path::PathBuf::from(last_dir).join(file_name);
        let full_path = download_base.join(relative_path);

        Some(full_path.to_string_lossy().to_string())
    } else {
        // Fallback
        let full_path = download_base.join(path);
        Some(full_path.to_string_lossy().to_string())
    }
}

#[cfg(feature = "server")]
async fn import_track(
    entry: FileEntry,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<Vec<FileEntry>>,
) {
    let download_path_base =
        std::env::var("SLSKD_DOWNLOAD_PATH").unwrap_or_else(|_| "/downloads".to_string());
    let download_path_buf = std::path::PathBuf::from(&download_path_base);

    if let Some(path) = resolve_download_path(&entry.filename, &download_path_buf) {
        info!("Importing path: {:?}", path);

        // Notify Importing
        let mut importing_entry = entry.clone();
        importing_entry.state = vec![DownloadState::Importing];
        let _ = tx.send(vec![importing_entry.clone()]);

        match beets::import(vec![path], &target_path).await {
            Ok(beets::ImportResult::Success) => {
                info!("Beet import successful");
                let mut imported_entry = entry.clone();
                imported_entry.state = vec![DownloadState::Imported];
                let _ = tx.send(vec![imported_entry]);
            }
            Ok(beets::ImportResult::Skipped) => {
                info!("Beet import skipped item");
                let mut skipped_entry = entry.clone();
                skipped_entry.state = vec![DownloadState::ImportSkipped];
                let _ = tx.send(vec![skipped_entry]);
            }
            Ok(beets::ImportResult::Failed) => {
                info!("Beet import failed item");
                let mut failed_entry = entry.clone();
                failed_entry.state = vec![DownloadState::ImportFailed];
                let _ = tx.send(vec![failed_entry]);
            }
            Err(e) => {
                info!("Beet import failed or returned unknown status: {e}");
                let mut failed_entry = entry.clone();
                failed_entry.state = vec![DownloadState::ImportFailed];
                let _ = tx.send(vec![failed_entry]);
            }
        }
    } else {
        warn!("Could not resolve path for file: {}", entry.filename);
        let mut failed_entry = entry.clone();
        failed_entry.state = vec![DownloadState::ImportFailed];
        let _ = tx.send(vec![failed_entry]);
    }
}

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
                        break;
                    }
                }
                Err(e) => {
                    // unexpected error or lag
                    warn!("Broadcast receive error: {}", e);
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

    info!("Started monitoring downloads: {:?}", download_filenames);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut attempts = 0;
        const MAX_ATTEMPTS: usize = 600; // ~20 minutes timeout

        loop {
            interval.tick().await;
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
                    let batch_status: Vec<_> = downloads
                        .iter()
                        .filter(|file| download_filenames.contains(&file.filename))
                        .cloned()
                        .collect();

                    if !batch_status.is_empty() {
                        let _ = tx.send(batch_status.clone());
                    }

                    // If we can't find any of our downloads, they might have been cleared or invalid
                    if batch_status.is_empty() {
                        info!("No active downloads found for batch, assuming completed or lost.");
                        break;
                    }

                    // TODO: Parallelize imports, do not wait for all to finish downloading before starting imports
                    let all_finished = batch_status.iter().all(|d| {
                        d.state.iter().any(|s| {
                            matches!(
                                s,
                                DownloadState::Completed
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
                                    .any(|s| matches!(s, DownloadState::Completed))
                            })
                            .cloned()
                            .collect();

                        if !successful_downloads.is_empty() {
                            info!(
                                "Downloads completed ({} successful). Starting granular import to {:?}",
                                successful_downloads.len(),
                                target_path
                            );

                            for download in successful_downloads {
                                import_track(download, target_path.clone(), tx.clone()).await;
                            }
                        } else {
                            info!("Downloads finished but none succeeded. Skipping import.");
                        }
                        break;
                    }
                }
                Err(e) => {
                    info!("Error fetching download status: {}", e);
                }
            }
        }
    });

    Ok(res)
}
