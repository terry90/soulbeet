#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::slskd::{DownloadState, FileEntry};
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
    entries: Vec<FileEntry>,
    source_path: String,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<Vec<FileEntry>>,
    as_album: bool,
) {
    info!(
        "Importing group from: {:?} (album: {})",
        source_path, as_album
    );

    // Notify Importing
    let mut importing_entries = entries.clone();
    for e in &mut importing_entries {
        e.state = vec![DownloadState::Importing];
    }
    let _ = tx.send(importing_entries.clone());

    match beets::import(vec![source_path.clone()], &target_path, as_album).await {
        Ok(beets::ImportResult::Success) => {
            info!("Beet import successful");
            let mut imported_entries = entries.clone();
            for e in &mut imported_entries {
                e.state = vec![DownloadState::Imported];
            }
            let _ = tx.send(imported_entries);
        }
        Ok(beets::ImportResult::Skipped) => {
            info!("Beet import skipped items");
            let mut skipped_entries = entries.clone();
            for e in &mut skipped_entries {
                e.state = vec![DownloadState::ImportSkipped];
            }
            let _ = tx.send(skipped_entries);

            // Clean up skipped files to prevent accumulation
            for entry in &entries {
                cleanup_failed_file(&entry.filename).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Ok(beets::ImportResult::Failed(err)) => {
            info!("Beet import failed: {}", err);
            let mut failed_entries = entries.clone();
            for e in &mut failed_entries {
                e.state = vec![DownloadState::ImportFailed];
                e.state_description = format!("Beet import failed: {err}");
            }
            let _ = tx.send(failed_entries);

            // Clean up failed files
            for entry in &entries {
                cleanup_failed_file(&entry.filename).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Ok(beets::ImportResult::TimedOut) => {
            warn!("Beet import timed out for: {}", source_path);
            let mut failed_entries = entries.clone();
            for e in &mut failed_entries {
                e.state = vec![DownloadState::ImportFailed];
                e.state_description = "Import timed out - beets process took too long".to_string();
            }
            let _ = tx.send(failed_entries);

            // Clean up timed out files
            for entry in &entries {
                cleanup_failed_file(&entry.filename).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
        Err(e) => {
            warn!("Beet import error for {}: {}", source_path, e);
            let mut failed_entries = entries.clone();
            for failed in &mut failed_entries {
                failed.state = vec![DownloadState::ImportFailed];
                failed.state_description = format!("Import error: {}", e);
            }
            let _ = tx.send(failed_entries);

            // Clean up on error
            for entry in &entries {
                cleanup_failed_file(&entry.filename).await;
            }
            cleanup_empty_parent_dir(&source_path).await;
        }
    }
}
