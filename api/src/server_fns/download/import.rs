#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::download::{DownloadProgress, DownloadState};
#[cfg(feature = "server")]
use soulbeet::beets;
#[cfg(feature = "server")]
use std::path::Path;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

/// Attempt to clean up a failed download/import file
#[cfg(feature = "server")]
async fn cleanup_failed_file(file_path: &str) {
    let path = Path::new(file_path);
    if path.exists() {
        match tokio::fs::remove_file(path).await {
            Ok(_) => info!("Cleaned up failed file: {}", file_path),
            Err(e) => warn!("Failed to clean up file {}: {}", file_path, e),
        }
    }
}

/// Attempt to clean up a directory if it's empty after cleanup
#[cfg(feature = "server")]
async fn cleanup_empty_parent_dir(file_path: &str) {
    let path = Path::new(file_path);
    if let Some(parent) = path.parent() {
        if parent.exists() {
            // Only remove if directory is empty
            match tokio::fs::read_dir(parent).await {
                Ok(mut entries) => {
                    if entries.next_entry().await.ok().flatten().is_none() {
                        match tokio::fs::remove_dir(parent).await {
                            Ok(_) => info!("Cleaned up empty directory: {:?}", parent),
                            Err(e) => {
                                warn!("Failed to clean up empty directory {:?}: {}", parent, e)
                            }
                        }
                    }
                }
                Err(e) => warn!("Failed to check directory {:?}: {}", parent, e),
            }
        }
    }
}

#[cfg(feature = "server")]
pub async fn import_group(
    entries: Vec<DownloadProgress>,
    source_path: String,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<Vec<DownloadProgress>>,
    as_album: bool,
) {
    info!(
        "Importing group from: {:?} (album: {})",
        source_path, as_album
    );

    // Notify Importing
    let importing_entries: Vec<_> = entries
        .iter()
        .map(|e| DownloadProgress {
            state: DownloadState::Importing,
            ..e.clone()
        })
        .collect();
    let _ = tx.send(importing_entries);

    match beets::import(vec![source_path.clone()], &target_path, as_album).await {
        Ok(beets::ImportResult::Success) => {
            info!("Beet import successful");
            let imported_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Imported,
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(imported_entries);
        }
        Ok(beets::ImportResult::Skipped) => {
            info!("Beet import skipped items");
            let skipped_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::ImportSkipped,
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(skipped_entries);

            // Clean up skipped files to prevent accumulation
            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Ok(beets::ImportResult::Failed(err)) => {
            info!("Beet import failed: {}", err);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Failed(format!("Beet import failed: {err}")),
                    error: Some(format!("Beet import failed: {err}")),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(failed_entries);

            // Clean up failed files
            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Ok(beets::ImportResult::TimedOut) => {
            warn!("Beet import timed out for: {}", source_path);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Failed("Import timed out - beets process took too long".into()),
                    error: Some("Import timed out".into()),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(failed_entries);

            // Clean up timed out files
            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Err(e) => {
            warn!("Beet import error for {}: {}", source_path, e);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|entry| DownloadProgress {
                    state: DownloadState::Failed(format!("Import error: {e}")),
                    error: Some(format!("Import error: {e}")),
                    ..entry.clone()
                })
                .collect();
            let _ = tx.send(failed_entries);

            // Clean up on error
            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
    }
}
