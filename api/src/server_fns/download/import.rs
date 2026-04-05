#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
#[cfg(feature = "server")]
use soulbeet::ImportResult;
#[cfg(feature = "server")]
use std::path::Path;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use crate::services::music_importer;

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

#[cfg(feature = "server")]
pub async fn import_group(
    entries: Vec<DownloadProgress>,
    source_path: String,
    target_path: std::path::PathBuf,
    tx: broadcast::Sender<DownloadEvent>,
    as_album: bool,
) {
    info!(
        "Importing group from: {:?} (album: {})",
        source_path, as_album
    );

    let importing_entries: Vec<_> = entries
        .iter()
        .map(|e| DownloadProgress {
            state: DownloadState::Importing,
            ..e.clone()
        })
        .collect();
    let _ = tx.send(DownloadEvent::Progress(importing_entries));

    let importer = match music_importer(None).await {
        Ok(imp) => imp,
        Err(e) => {
            warn!("Failed to get importer: {}", e);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|entry| DownloadProgress {
                    state: DownloadState::Failed(format!("No importer available: {e}")),
                    error: Some(format!("No importer available: {e}")),
                    ..entry.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));
            return;
        }
    };

    let source = Path::new(&source_path);
    match importer.import(&[source], &target_path, as_album).await {
        Ok(ImportResult::Success) => {
            info!("Import successful");
            let imported_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Imported,
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(imported_entries));

            // Clean up empty source directories left after beets moves the files
            if let Some(parent) = Path::new(&source_path).parent() {
                let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
            }
        }
        Ok(ImportResult::Skipped) => {
            info!("Import skipped items");
            let skipped_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::ImportSkipped,
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(skipped_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            if let Some(parent) = std::path::Path::new(&source_path).parent() {
                let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
            }
        }
        Ok(ImportResult::Failed(err)) => {
            info!("Import failed: {}", err);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Failed(format!("Import failed: {err}")),
                    error: Some(format!("Import failed: {err}")),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            if let Some(parent) = std::path::Path::new(&source_path).parent() {
                let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
            }
        }
        Ok(ImportResult::TimedOut) => {
            warn!("Import timed out for: {}", source_path);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Failed("Import timed out".into()),
                    error: Some("Import timed out".into()),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            if let Some(parent) = std::path::Path::new(&source_path).parent() {
                let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
            }
        }
        Err(e) => {
            warn!("Import error for {}: {}", source_path, e);
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|entry| DownloadProgress {
                    state: DownloadState::Failed(format!("Import error: {e}")),
                    error: Some(format!("Import error: {e}")),
                    ..entry.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
            if let Some(parent) = std::path::Path::new(&source_path).parent() {
                let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
            }
        }
    }
}
