use dioxus::fullstack::{CborEncoding, Streaming};
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::slskd::{DownloadResponse, DownloadState, FileEntry, TrackResult};

#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use soulbeet::beets;

use super::server_error;
use crate::auth;
#[cfg(feature = "server")]
use crate::globals::{SLSKD_CLIENT, USER_CHANNELS};

#[cfg(feature = "server")]
async fn slskd_download(tracks: Vec<TrackResult>) -> Result<Vec<DownloadResponse>, ServerFnError> {
    SLSKD_CLIENT.download(tracks).await.map_err(server_error)
}

#[server(output = CborEncoding)]
pub async fn download_updates_stream(
    token: String,
) -> Result<Streaming<Vec<FileEntry>, CborEncoding>, ServerFnError> {
    let claims = auth::verify_token(&token, "access").map_err(server_error)?;
    let username = claims.username;

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

#[server]
pub async fn download(
    token: String,
    tracks: Vec<TrackResult>,
    target_folder: String,
) -> Result<Vec<DownloadResponse>, ServerFnError> {
    let claims = auth::verify_token(&token, "access").map_err(server_error)?;
    let username = claims.username;

    let target_path_buf = std::path::Path::new(&target_folder).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let res = slskd_download(tracks).await?;
    let download_filenames: Vec<String> = res.iter().map(|d| d.filename.clone()).collect();
    let target_path = target_path_buf;

    info!("Started monitoring downloads: {:?}", download_filenames);

    let tx = {
        let mut map = USER_CHANNELS.write().await;
        map.entry(username)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(100);
                tx
            })
            .clone()
    };

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

                    let all_finished = batch_status.iter().all(|d| {
                        d.state.iter().any(|s| {
                            matches!(
                                s,
                                DownloadState::Succeeded
                                    | DownloadState::Completed
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
                                d.state.iter().any(|s| {
                                    matches!(s, DownloadState::Succeeded | DownloadState::Completed)
                                })
                            })
                            .collect();

                        if !successful_downloads.is_empty() {
                            info!(
                                "Downloads completed ({} successful). Starting import to {:?}",
                                successful_downloads.len(),
                                target_path
                            );

                            let download_path_base = std::env::var("SLSKD_DOWNLOAD_PATH")
                                .unwrap_or_else(|_| "/downloads".to_string());
                            let download_path_buf = std::path::PathBuf::from(&download_path_base);

                            let paths: Vec<String> = successful_downloads
                                .iter()
                                .filter_map(|d| {
                                    // Normalize path separators (win -> linux)
                                    let filename_str = d.filename.replace('\\', "/");
                                    let path = std::path::Path::new(&filename_str);
                                    let components: Vec<_> = path.components().collect();

                                    // Keep only the last directory and filename (d1/d2/d3/file -> d3/file)
                                    if components.len() >= 2 {
                                        let len = components.len();
                                        let last_dir = components[len - 2].as_os_str();
                                        let file_name = components[len - 1].as_os_str();

                                        let relative_path =
                                            std::path::PathBuf::from(last_dir).join(file_name);
                                        let full_path = download_path_buf.join(relative_path);

                                        Some(full_path.to_string_lossy().to_string())
                                    } else {
                                        // Fallback
                                        let full_path = download_path_buf.join(path);
                                        Some(full_path.to_string_lossy().to_string())
                                    }
                                })
                                .collect::<std::collections::HashSet<_>>()
                                .into_iter()
                                .collect();

                            info!("Importing paths: {:?}", paths);

                            let importing_update: Vec<_> = successful_downloads
                                .iter()
                                .map(|d| {
                                    let mut d = (*d).clone();
                                    d.state = vec![DownloadState::Importing];
                                    d
                                })
                                .collect();
                            let _ = tx.send(importing_update);

                            match beets::import(paths, &target_path).await {
                                Ok(beets::ImportResult::Success) => {
                                    info!("Beet import successful");
                                    let imported_update: Vec<_> = successful_downloads
                                        .iter()
                                        .map(|d| {
                                            let mut d = (*d).clone();
                                            d.state = vec![DownloadState::Imported];
                                            d
                                        })
                                        .collect();
                                    let _ = tx.send(imported_update);
                                }
                                Ok(beets::ImportResult::Skipped) => {
                                    info!("Beet import skipped items");
                                    let skipped_update: Vec<_> = successful_downloads
                                        .iter()
                                        .map(|d| {
                                            let mut d = (*d).clone();
                                            d.state = vec![DownloadState::ImportSkipped];
                                            d
                                        })
                                        .collect();
                                    let _ = tx.send(skipped_update);
                                }
                                _ => {
                                    info!("Beet import failed or returned unknown status");
                                    let failed_update: Vec<_> = successful_downloads
                                        .iter()
                                        .map(|d| {
                                            let mut d = (*d).clone();
                                            d.state = vec![DownloadState::ImportFailed];
                                            d
                                        })
                                        .collect();
                                    let _ = tx.send(failed_update);
                                }
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
