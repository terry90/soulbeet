use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use shared::download::{batch_folder, DownloadProgress, EnqueueStatus};
use shared::download::{DownloadEvent, DownloadableItem, QueuedDownload};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use crate::{
    server_fns::{refusal_error, server_error},
    AuthSession,
};

#[cfg(feature = "server")]
use crate::globals::{
    cleanup_stale_channels, get_or_create_user_channel, register_user_task, unregister_user_task,
    USER_CHANNELS,
};
#[cfg(feature = "server")]
use crate::services::download_backend;

// Local modules
pub mod auto_download;
pub use auto_download::{auto_download, AutoDownloadRequest, AutoDownloadResult};

#[cfg(feature = "server")]
pub mod failover;
#[cfg(feature = "server")]
pub mod monitor;
#[cfg(feature = "server")]
pub mod process;
#[cfg(feature = "server")]
pub mod utils;

// Shared with the tests of sibling modules outside `download`, so it cannot
// stay private to this one.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support;

#[cfg(feature = "server")]
use self::monitor::DownloadMonitor;

#[cfg(feature = "server")]
async fn do_download(
    items: Vec<DownloadableItem>,
    destination: &str,
    backend_id: Option<&str>,
) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let backend = download_backend(backend_id)
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    backend
        .download(items, destination)
        .await
        .map_err(server_error)
}

/// Why a file was not enqueued, as its row shows it; `None` when it was.
#[cfg(feature = "server")]
pub(crate) fn not_enqueued_reason(status: &EnqueueStatus) -> Option<String> {
    match status {
        EnqueueStatus::Enqueued => None,
        EnqueueStatus::AlreadyInFlight => Some("already downloading in slskd".to_string()),
        EnqueueStatus::Failed(message) => Some(message.clone()),
    }
}

/// Name for a manual download's batch: the album its items share, or the
/// first item's title when they span albums.
#[cfg(feature = "server")]
fn manual_batch_label(items: &[DownloadableItem]) -> String {
    match items.split_first() {
        Some((first, rest))
            if !first.album.trim().is_empty() && rest.iter().all(|i| i.album == first.album) =>
        {
            first.album.clone()
        }
        Some((first, _)) => first.title.clone(),
        None => String::new(),
    }
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
#[get("/api/downloads/updates", auth: AuthSession)]
pub async fn download_updates_ws(
    options: WebSocketOptions,
) -> Result<Websocket<(), DownloadEvent>, ServerFnError> {
    let username = auth.0.username;

    let rx = {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(&username) {
            channel.sender.subscribe()
        } else {
            drop(map);
            let mut map = USER_CHANNELS.write().await;
            let channel = map
                .entry(username.clone())
                .or_insert_with(crate::globals::UserChannel::new);
            channel.sender.subscribe()
        }
    };

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut rx = rx;
        info!("WebSocket connected for user: {}", username);

        loop {
            // handle both broadcast messages and potential socket closure
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(downloads) => {
                            if socket.send(downloads).await.is_err() {
                                info!("WebSocket closed (client disconnected)");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                "Download updates lagged, skipped {} messages. Consider reducing download batch size.",
                                skipped
                            );
                            // lagged is recoverable - continue receiving
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Broadcast channel closed");
                            break;
                        }
                    }
                }
                result = socket.recv() => {
                    if result.is_err() {
                        info!("WebSocket client disconnected");
                        break;
                    }
                }
            }
        }

        info!("WebSocket disconnected for user: {}", username);

        // Trigger cleanup of stale channels periodically
        cleanup_stale_channels().await;
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelDownloadRequest {
    pub id: String,
    pub source: String,
    pub item: String,
    pub backend: Option<String>,
}

#[post("/api/downloads/cancel", auth: AuthSession)]
pub async fn cancel_download(req: CancelDownloadRequest) -> Result<(), ServerFnError> {
    let username = auth.0.username;

    let backend = download_backend(req.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    backend
        .cancel_download(&req.source, &req.id, false)
        .await
        .map_err(server_error)?;

    info!(
        "User {} cancelled download {} from {}",
        username, req.id, req.source
    );

    // Send cancelled state to UI via broadcast channel
    let (tx, _) = get_or_create_user_channel(&username).await;
    let cancelled = DownloadProgress {
        id: req.id,
        source: req.source,
        item: req.item,
        size: 0,
        transferred: 0,
        state: shared::download::DownloadState::Cancelled,
        percent: 0.0,
        speed: 0.0,
        error: None,
        backend: req.backend,
        batch_id: None,
        batch_label: None,
        local_path: None,
        import_output: None,
    };
    let _ = tx.send(DownloadEvent::Progress(vec![cancelled]));

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub items: Vec<DownloadableItem>,
    pub target_folder: String,
    #[serde(default)]
    pub backend: Option<String>,
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(req: DownloadRequest) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let username = auth.0.username;

    if let Some(issue) = crate::importer_health::broken_issue().await {
        return Err(refusal_error(issue.title));
    }
    let folder =
        crate::models::folder::Folder::find_for_user_by_path(&auth.0.sub, &req.target_folder)
            .await
            .map_err(server_error)?
            .ok_or_else(|| refusal_error("Folder not found"))?;

    let target_path_buf = std::path::Path::new(&folder.path).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let batch_id = uuid::Uuid::new_v4().to_string();
    let batch_label = manual_batch_label(&req.items);
    let destination = batch_folder(&batch_label, &batch_id);
    let res = do_download(req.items, &destination, req.backend.as_deref()).await?;

    let (tx, _) = get_or_create_user_channel(&username).await;

    let backend_id = req.backend;

    let failed_entries: Vec<DownloadProgress> = res
        .iter()
        .filter_map(|d| {
            let reason = not_enqueued_reason(&d.status)?;
            let mut p =
                DownloadProgress::failed(d.id.clone(), d.source.clone(), d.item.clone(), reason)
                    .with_batch(batch_id.clone(), batch_label.clone());
            p.backend = backend_id.clone();
            Some(p)
        })
        .collect();
    if !failed_entries.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(failed_entries));
    }

    let successful: Vec<&QueuedDownload> = res
        .iter()
        .filter(|d| d.status == EnqueueStatus::Enqueued)
        .collect();

    let download_sources: Vec<String> = successful.iter().map(|d| d.source.clone()).collect();
    let download_filenames: Vec<String> = successful.iter().map(|d| d.item.clone()).collect();
    let target_path = target_path_buf;

    if download_filenames.is_empty() {
        return Ok(res);
    }

    // Send initial "Queued" state immediately so UI shows the downloads right away
    let queued_entries: Vec<DownloadProgress> = successful
        .iter()
        .map(|d| {
            let mut p =
                DownloadProgress::queued(d.id.clone(), d.source.clone(), d.item.clone(), d.size)
                    .with_batch(batch_id.clone(), batch_label.clone());
            p.backend = backend_id.clone();
            p
        })
        .collect();
    let _ = tx.send(DownloadEvent::Progress(queued_entries));

    info!("Started monitoring downloads: {:?}", download_filenames);

    // Register this task for cleanup tracking
    let task_username = username.clone();
    let task_cancellation = register_user_task(&username).await;

    // Spawn the monitoring task
    tokio::spawn(async move {
        let mut monitor = DownloadMonitor::new(
            download_sources,
            download_filenames,
            target_path,
            tx,
            task_cancellation,
            task_username.clone(),
            batch_id,
            batch_label,
        );
        monitor.run().await;
        unregister_user_task(&task_username).await;
    });

    Ok(res)
}

/// Imports a failed or skipped row again, from the job kept for it.
#[post("/api/downloads/retry-import", auth: AuthSession)]
pub async fn retry_import(item: String) -> Result<(), ServerFnError> {
    crate::import_queue::retry_import_for(&auth.0.username, &item)
        .await
        .map_err(refusal_error)
}

/// Drops the stored imports of rows the user cleared.
#[post("/api/downloads/forget-retries", auth: AuthSession)]
pub async fn forget_import_retries(items: Vec<String>) -> Result<(), ServerFnError> {
    crate::import_queue::forget_retries(&auth.0.username, &items);
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use test_support::item;

    #[test]
    fn manual_label_is_the_shared_album_else_the_first_title() {
        let same_album = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];
        assert_eq!(manual_batch_label(&same_album), "Come Over");

        let mut mixed = same_album.clone();
        mixed[1].album = "Other".to_string();
        assert_eq!(manual_batch_label(&mixed), "One");

        let mut no_album = same_album;
        for i in &mut no_album {
            i.album = " ".to_string();
        }
        assert_eq!(manual_batch_label(&no_album), "One");

        assert_eq!(not_enqueued_reason(&EnqueueStatus::Enqueued), None);
        assert_eq!(
            not_enqueued_reason(&EnqueueStatus::AlreadyInFlight).as_deref(),
            Some("already downloading in slskd")
        );
    }
}
