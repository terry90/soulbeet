#[cfg(feature = "server")]
use dioxus::logger::tracing::info;
#[cfg(feature = "server")]
use shared::slskd::{DownloadState, FileEntry};
#[cfg(feature = "server")]
use soulbeet::beets;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

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

    match beets::import(vec![source_path], &target_path, as_album).await {
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
        }
        Ok(beets::ImportResult::Failed(err)) => {
            info!("Beet import failed items");
            let mut failed_entries = entries.clone();
            for e in &mut failed_entries {
                e.state = vec![DownloadState::ImportFailed];
                e.state_description = format!("Beet import failed: {err}");
            }
            let _ = tx.send(failed_entries);
        }
        Err(e) => {
            info!("Beet import failed or returned unknown status: {e}");
            let mut failed_entries = entries.clone();
            for failed in &mut failed_entries {
                failed.state = vec![DownloadState::ImportFailed];
                failed.state_description = format!("Import error: {}", e);
            }
            let _ = tx.send(failed_entries);
        }
    }
}
