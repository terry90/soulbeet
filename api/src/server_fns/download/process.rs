#[cfg(feature = "server")]
use dioxus::logger::tracing::info;
#[cfg(feature = "server")]
use shared::slskd::{DownloadState, FileEntry};
#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use super::import::import_group;
#[cfg(feature = "server")]
use super::utils::resolve_download_path;
#[cfg(feature = "server")]
use crate::config::CONFIG;

#[cfg(feature = "server")]
pub async fn process_downloads(
    successful_downloads: Vec<FileEntry>,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<Vec<FileEntry>>,
) {
    if !successful_downloads.is_empty() {
        info!(
            "Downloads completed ({} successful). Starting import to {:?}",
            successful_downloads.len(),
            target_path
        );

        let download_path_buf = CONFIG.slskd_download_path().clone();
        let album_mode = CONFIG.is_album_mode();

        if album_mode {
            let mut pending_imports: HashMap<String, Vec<FileEntry>> = HashMap::new();
            // safety net for single files not in an album folder
            let mut singletons: Vec<FileEntry> = Vec::new();

            for download in successful_downloads {
                if let Some(path) = resolve_download_path(&download.filename, &download_path_buf) {
                    let p = std::path::Path::new(&path);
                    // group by parent directory (album or release)
                    if let Some(parent) = p.parent() {
                        if parent == download_path_buf {
                            singletons.push(download);
                        } else {
                            let parent_str = parent.to_string_lossy().to_string();
                            pending_imports
                                .entry(parent_str)
                                .or_default()
                                .push(download);
                        }
                    } else {
                        singletons.push(download);
                    }
                } else {
                    // Handle resolution error
                    let mut failed_entry = download.clone();
                    failed_entry.state = vec![DownloadState::ImportFailed];
                    failed_entry.state_description = "Could not resolve file path".to_string();
                    let _ = tx.send(vec![failed_entry]);
                }
            }

            for (source_path, entries) in pending_imports {
                import_group(entries, source_path, target_path.clone(), tx.clone(), true).await;
            }

            for download in singletons {
                if let Some(path) = resolve_download_path(&download.filename, &download_path_buf) {
                    import_group(vec![download], path, target_path.clone(), tx.clone(), false)
                        .await;
                }
            }
        } else {
            // singleton mode
            for download in successful_downloads {
                if let Some(path) = resolve_download_path(&download.filename, &download_path_buf) {
                    import_group(vec![download], path, target_path.clone(), tx.clone(), false)
                        .await;
                } else {
                    let mut failed_entry = download.clone();
                    failed_entry.state = vec![DownloadState::ImportFailed];
                    failed_entry.state_description = "Could not resolve file path".to_string();
                    let _ = tx.send(vec![failed_entry]);
                }
            }
        }
    } else {
        info!("Downloads finished but none succeeded. Skipping import.");
    }
}
