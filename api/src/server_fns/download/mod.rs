use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::{DownloadProgress, DownloadableItem, QueuedDownload};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use crate::{server_fns::server_error, AuthSession};

#[cfg(feature = "server")]
use crate::globals::{
    cleanup_stale_channels, get_or_create_user_channel, register_user_task, unregister_user_task,
    SERVICES, USER_CHANNELS,
};

// Local modules
#[cfg(feature = "server")]
pub mod import;
#[cfg(feature = "server")]
mod monitor;
#[cfg(feature = "server")]
pub mod process;
#[cfg(feature = "server")]
pub mod utils;

#[cfg(feature = "server")]
use self::monitor::DownloadMonitor;

#[cfg(feature = "server")]
async fn do_download(
    items: Vec<DownloadableItem>,
    backend_id: Option<&str>,
) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let backend = SERVICES
        .download(backend_id)
        .ok_or_else(|| server_error("download backend not found"))?;

    backend.download(items).await.map_err(server_error)
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
#[get("/api/downloads/updates", auth: AuthSession)]
pub async fn download_updates_ws(
    options: WebSocketOptions,
) -> Result<Websocket<(), Vec<DownloadProgress>>, ServerFnError> {
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
pub struct DownloadRequest {
    pub items: Vec<DownloadableItem>,
    pub target_folder: String,
    #[serde(default)]
    pub backend: Option<String>,
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(req: DownloadRequest) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let username = auth.0.username;

    let target_path_buf = std::path::Path::new(&req.target_folder).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let res = do_download(req.items, req.backend.as_deref()).await?;

    let (failed, successful): (Vec<_>, Vec<_>) =
        res.iter().cloned().partition(|d| d.error.is_some());

    let (tx, _) = get_or_create_user_channel(&username).await;

    if !failed.is_empty() {
        let failed_entries: Vec<DownloadProgress> = failed
            .iter()
            .map(|d| DownloadProgress::failed(d.id.clone(), d.source.clone(), d.item.clone(), d.error.clone().unwrap_or_default()))
            .collect();
        let _ = tx.send(failed_entries);
    }

    let download_filenames: Vec<String> = successful.iter().map(|d| d.item.clone()).collect();
    let target_path = target_path_buf;

    if download_filenames.is_empty() {
        return Ok(res);
    }

    // Send initial "Queued" state immediately so UI shows the downloads right away
    let queued_entries: Vec<DownloadProgress> = successful
        .iter()
        .map(|d| DownloadProgress::queued(d.id.clone(), d.source.clone(), d.item.clone(), d.size))
        .collect();
    let _ = tx.send(queued_entries);

    info!("Started monitoring downloads: {:?}", download_filenames);

    // Register this task for cleanup tracking
    let task_username = username.clone();
    let task_cancellation = register_user_task(&username).await;

    // Spawn the monitoring task
    tokio::spawn(async move {
        let mut monitor = DownloadMonitor::new(
            download_filenames,
            target_path,
            tx,
            task_cancellation,
            task_username.clone(),
        );
        monitor.run().await;
        unregister_user_task(&task_username).await;
    });

    Ok(res)
}
