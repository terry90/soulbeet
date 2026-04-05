#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::path::Path;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use super::import::import_group;
#[cfg(feature = "server")]
use super::utils::resolve_download_path;
#[cfg(feature = "server")]
use crate::config::CONFIG;

/// Maximum number of retries when waiting for a downloaded file to appear on disk.
/// With exponential backoff (500ms, 1s, 2s, 4s, 8s), this covers ~15.5s total.
#[cfg(feature = "server")]
const FILE_RESOLVE_MAX_RETRIES: u32 = 5;

/// Initial delay between file resolution retries.
#[cfg(feature = "server")]
const FILE_RESOLVE_INITIAL_DELAY_MS: u64 = 500;

/// Resolve a download path with retries. When a download completes very fast
/// (e.g. file already cached in slskd), the API may report completion before
/// the file is visible on disk via shared volume mounts. This retries with
/// exponential backoff to handle that lag.
#[cfg(feature = "server")]
async fn resolve_download_path_with_retry(filename: &str, download_base: &Path) -> Option<String> {
    if let Some(path) = resolve_download_path(filename, download_base) {
        return Some(path);
    }

    let mut delay_ms = FILE_RESOLVE_INITIAL_DELAY_MS;
    for attempt in 1..=FILE_RESOLVE_MAX_RETRIES {
        warn!(
            "File not found on disk (attempt {}), retrying in {}ms: {}",
            attempt, delay_ms, filename
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

        if let Some(path) = resolve_download_path(filename, download_base) {
            info!(
                "File appeared on disk after {}ms wait: {}",
                delay_ms, filename
            );
            return Some(path);
        }

        delay_ms *= 2;
    }

    None
}

#[cfg(feature = "server")]
pub async fn process_downloads(
    successful_downloads: Vec<DownloadProgress>,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<DownloadEvent>,
) {
    if !successful_downloads.is_empty() {
        info!(
            "Downloads completed ({} successful). Starting import to {:?}",
            successful_downloads.len(),
            target_path
        );

        let download_path_buf = CONFIG.download_path().clone();
        let album_mode = CONFIG.is_album_mode();

        if album_mode {
            let mut pending_imports: HashMap<String, Vec<DownloadProgress>> = HashMap::new();
            // safety net for single files not in an album folder
            let mut singletons: Vec<DownloadProgress> = Vec::new();

            for download in successful_downloads {
                if let Some(path) =
                    resolve_download_path_with_retry(&download.item, &download_path_buf).await
                {
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
                    let failed_entry = DownloadProgress {
                        state: DownloadState::Failed("Could not resolve file path".into()),
                        error: Some("Could not resolve file path".into()),
                        ..download
                    };
                    let _ = tx.send(DownloadEvent::Progress(vec![failed_entry]));
                }
            }

            for (source_path, entries) in pending_imports {
                import_group(entries, source_path, target_path.clone(), tx.clone(), true).await;
            }

            for download in singletons {
                if let Some(path) =
                    resolve_download_path_with_retry(&download.item, &download_path_buf).await
                {
                    import_group(vec![download], path, target_path.clone(), tx.clone(), false)
                        .await;
                }
            }
        } else {
            // singleton mode
            for download in successful_downloads {
                if let Some(path) =
                    resolve_download_path_with_retry(&download.item, &download_path_buf).await
                {
                    import_group(vec![download], path, target_path.clone(), tx.clone(), false)
                        .await;
                } else {
                    let failed_entry = DownloadProgress {
                        state: DownloadState::Failed("Could not resolve file path".into()),
                        error: Some("Could not resolve file path".into()),
                        ..download
                    };
                    let _ = tx.send(DownloadEvent::Progress(vec![failed_entry]));
                }
            }
        }
    } else {
        info!("Downloads finished but none succeeded. Skipping import.");
    }
}
