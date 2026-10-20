#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
#[cfg(feature = "server")]
use soulbeet::{ImportMode, ImportOutcome};
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
    let mode = if as_album {
        ImportMode::Album
    } else {
        ImportMode::Singleton
    };
    let result = importer.import(&[source], &target_path, mode).await;
    match &result.outcome {
        ImportOutcome::Imported { .. } => {
            info!("Import successful: {result}");
            let imported_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Imported,
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(imported_entries));
        }
        ImportOutcome::Skipped(_) => {
            info!("Import skipped items: {result}");
            let skipped_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::ImportSkipped,
                    // Carry the reason to the UI: a skipped row that explains
                    // nothing is the whole of issue #69.
                    error: Some(result.to_string()),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(skipped_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
        }
        ImportOutcome::LookupFailed { .. }
        | ImportOutcome::Failed { .. }
        | ImportOutcome::TimedOut { .. }
        | ImportOutcome::Refused { .. } => {
            warn!("Import failed for {source_path}: {result}");
            let failed_entries: Vec<_> = entries
                .iter()
                .map(|e| DownloadProgress {
                    state: DownloadState::Failed(result.to_string()),
                    error: Some(result.to_string()),
                    ..e.clone()
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));

            for entry in &entries {
                cleanup_failed_file(&entry.item).await;
            }
        }
    }

    // beets moves the files out but never prunes the folder they came from:
    // the import path is that folder (album) or the file itself (singleton).
    let source_dir = if as_album {
        Some(source)
    } else {
        source.parent()
    };
    if let Some(dir) = source_dir {
        let _ =
            crate::server_fns::cleanup_empty_ancestors(dir, crate::config::CONFIG.download_path())
                .await;
    }
}
