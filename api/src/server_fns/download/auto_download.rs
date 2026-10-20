use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;

#[cfg(feature = "server")]
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, DownloadableGroup, EnqueueStatus,
    QueuedDownload, SearchState,
};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use std::sync::Arc;
#[cfg(feature = "server")]
use std::time::Duration;

#[cfg(feature = "server")]
use crate::globals::{get_or_create_user_channel, register_user_task, unregister_user_task};
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::services::{available_download_backends, download_backend};
#[cfg(feature = "server")]
use crate::transfer_registry::{BatchRef, TransferOwner};
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::enqueue::{
    deliver, enqueue_request, failed_reason, stopped_rows, RequestEnqueue, Watcher,
};
#[cfg(feature = "server")]
use super::failover::{align_pool_tracks, SourcePool};
#[cfg(feature = "server")]
use super::monitor::{DownloadMonitor, WatchedTransfer};

/// Score threshold for automatic source selection (per D-04).
/// Below this, the client gets FallbackToManual with results attached.
#[cfg(feature = "server")]
const AUTO_SELECT_SCORE_THRESHOLD: f64 = 0.7;

/// Maximum time to wait for search results from a single backend.
#[cfg(feature = "server")]
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll interval when waiting for search results.
#[cfg(feature = "server")]
const SEARCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRequest {
    pub query: DownloadQuery,
    /// Folder ID used for persisting as default_download_folder_id (per D-01)
    pub folder_id: String,
    /// Resolved folder path for the actual download target directory
    pub folder_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AutoDownloadResult {
    /// Pipeline spawned, all further results delivered via WebSocket AutoDownloadEvent
    Accepted { batch_id: String },
    /// Error before pipeline could start (no backends, bad input, etc.)
    Error(String),
}

#[post("/api/auto-download", auth: AuthSession)]
pub async fn auto_download(req: AutoDownloadRequest) -> Result<AutoDownloadResult, ServerFnError> {
    let username = auth.0.username;

    if let Some(issue) = crate::importer_health::broken_issue().await {
        return Ok(AutoDownloadResult::Error(issue.title));
    }
    let folder = match Folder::get_by_id(&req.folder_id).await {
        Ok(Some(f))
            if f.user_id == auth.0.sub
                && std::path::Path::new(&req.folder_path) == std::path::Path::new(&f.path) =>
        {
            f
        }
        Ok(_) => return Ok(AutoDownloadResult::Error("Folder not found".to_string())),
        Err(e) => {
            warn!("Folder lookup for auto-download failed: {e}");
            return Ok(AutoDownloadResult::Error("Folder not found".to_string()));
        }
    };

    let (tx, _) = get_or_create_user_channel(&username).await;

    let mut req = req;
    if let Err(e) = crate::server_fns::search::hydrate_album_tracks(&mut req.query).await {
        return Ok(AutoDownloadResult::Error(e));
    }

    // Build search description for logging and events
    let query_desc = req
        .query
        .album
        .as_ref()
        .map(|a| format!("{} - {}", &a.artist, &a.title))
        .unwrap_or_else(|| {
            req.query
                .tracks
                .first()
                .map(|t| format!("{} - {}", &t.artist, &t.title))
                .unwrap_or_else(|| "Unknown query".to_string())
        });

    // Collect available backends (per D-06: search ALL configured backends)
    let backend_ids: Vec<String> = available_download_backends()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();

    let mut backends = Vec::new();
    for id in &backend_ids {
        match download_backend(Some(id)).await {
            Ok(b) => backends.push((id.clone(), b)),
            Err(e) => {
                warn!("Backend {} unavailable for auto-download: {}", id, e);
            }
        }
    }

    if backends.is_empty() {
        let batch_id = uuid::Uuid::new_v4().to_string();
        let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
            batch_id,
            error: "No download backends available".to_string(),
        }));
        return Ok(AutoDownloadResult::Error(
            "No download backends available".to_string(),
        ));
    }

    // Generate batch_id upfront so we can return it immediately (per D-10)
    let batch_id = uuid::Uuid::new_v4().to_string();
    let batch_id_for_response = batch_id.clone();

    // Spawn the entire search-score-pick-download pipeline onto a background task.
    // This avoids blocking the HTTP response during the search-poll loop (Research Pitfall 4).
    let folder_path = folder.path;
    let album = req.query.album.clone();
    let tracks = req.query.tracks.clone();
    let task_username = username.clone();
    let user_id = auth.0.sub;

    tokio::spawn(async move {
        // Send Searching event
        let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Searching {
            batch_id: batch_id.clone(),
            query: query_desc.clone(),
            backend_count: backends.len(),
        }));

        info!(
            "Auto-download: searching {} backends for '{}'",
            backends.len(),
            query_desc
        );

        // Search all backends in parallel (per D-06)
        let search_futures: Vec<_> = backends
            .iter()
            .map(|(id, backend)| {
                let id = id.clone();
                let backend = Arc::clone(backend);
                let album = album.clone();
                let tracks = tracks.clone();
                async move {
                    // Start search
                    let search_id = match backend.start_search(album.as_ref(), &tracks).await {
                        Ok(sid) => sid,
                        Err(e) => {
                            warn!("Backend {} search start failed: {}", id, e);
                            return (id, Vec::<DownloadableGroup>::new());
                        }
                    };

                    // Poll until results are ready or timeout. Polls that report
                    // InProgress carry the groups processed so far (the backend
                    // rescores the full response set each time), so keep the
                    // latest non-empty batch: the final Completed poll is empty
                    // when the search ends by timeout instead of result volume.
                    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
                    let mut latest_groups = Vec::<DownloadableGroup>::new();
                    loop {
                        if tokio::time::Instant::now() >= deadline {
                            warn!("Backend {} search timed out", id);
                            break;
                        }
                        tokio::time::sleep(SEARCH_POLL_INTERVAL).await;

                        match backend.poll_search(&search_id).await {
                            Ok(result) => match result.state {
                                SearchState::Completed | SearchState::TimedOut => {
                                    if !result.groups.is_empty() {
                                        latest_groups = result.groups;
                                    }
                                    return (id, latest_groups);
                                }
                                SearchState::NotFound => {
                                    return (id, latest_groups);
                                }
                                SearchState::InProgress => {
                                    if !result.groups.is_empty() {
                                        latest_groups = result.groups;
                                    }
                                }
                            },
                            Err(e) => {
                                warn!("Backend {} poll error: {}", id, e);
                                return (id, latest_groups);
                            }
                        }
                    }
                    (id, latest_groups)
                }
            })
            .collect();

        let results = futures::future::join_all(search_futures).await;

        // Merge all results into a single list sorted by score (per D-07)
        let mut all_groups: Vec<DownloadableGroup> = results
            .into_iter()
            .flat_map(|(_backend_id, groups)| groups)
            .collect();

        if all_groups.is_empty() {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: "No results found".to_string(),
            }));
            info!("Auto-download: no results for '{}'", query_desc);
            return;
        }

        // Sort by score descending
        all_groups.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let best_score = all_groups[0].score;

        // Send ScoringResults event
        let _ = tx.send(DownloadEvent::AutoDownload(
            AutoDownloadEvent::ScoringResults {
                batch_id: batch_id.clone(),
                result_count: all_groups.len(),
                best_score,
            },
        ));

        info!(
            "Auto-download: {} results, best score {:.2} for '{}'",
            all_groups.len(),
            best_score,
            query_desc
        );

        // Decide: auto-pick or fallback (per D-04)
        if best_score < AUTO_SELECT_SCORE_THRESHOLD {
            let _ = tx.send(DownloadEvent::AutoDownload(
                AutoDownloadEvent::FallbackToManual {
                    batch_id: batch_id.clone(),
                    results: all_groups,
                    best_score,
                    threshold: AUTO_SELECT_SCORE_THRESHOLD,
                },
            ));
            info!(
                "Auto-download: score {:.2} < threshold {:.1}, falling back to manual for '{}'",
                best_score, AUTO_SELECT_SCORE_THRESHOLD, query_desc
            );
            return;
        }

        // Pick the best source
        let picked = all_groups.remove(0);

        let _ = tx.send(DownloadEvent::AutoDownload(
            AutoDownloadEvent::PickedSource {
                batch_id: batch_id.clone(),
                source: picked.source.clone(),
                score: picked.score,
                quality: picked.quality.clone(),
                track_count: picked.items.len(),
            },
        ));

        info!(
            "Auto-download: picked source '{}' (score {:.2}, {} tracks) for '{}'",
            picked.source,
            picked.score,
            picked.items.len(),
            query_desc
        );

        let batch_label = super::auto_batch_label(&picked);

        // Create target directory
        let target_path_buf = std::path::Path::new(&folder_path).to_path_buf();
        if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: format!("Failed to create target directory: {}", e),
            }));
            return;
        }

        // Queue download with the backend
        let items = picked.items.clone();
        let backend = match download_backend(None).await {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    error: format!("Download backend not available: {}", e),
                }));
                return;
            }
        };

        let fresh = BatchRef::new(
            TransferOwner {
                user_id,
                batch_id: batch_id.clone(),
            },
            batch_label,
            target_path_buf,
        );
        let enq = enqueue_request(&backend, items, fresh).await;
        let refusal = enq.refusal();
        let RequestEnqueue {
            batch,
            results,
            watcher,
            error,
            ..
        } = enq;
        let row =
            |p: DownloadProgress| p.with_batch(batch.owner.batch_id.clone(), batch.label.clone());

        // Send failed entries as progress events
        let failed_entries: Vec<DownloadProgress> = results
            .iter()
            .filter_map(|d| {
                let reason = failed_reason(&d.status)?;
                Some(row(DownloadProgress::failed(
                    d.id.clone(),
                    d.source.clone(),
                    d.item.clone(),
                    reason.to_string(),
                )))
            })
            .collect();
        if !failed_entries.is_empty() {
            let _ = tx.send(DownloadEvent::Progress(failed_entries));
        }

        let successful: Vec<QueuedDownload> = results
            .iter()
            .filter(|d| d.status == EnqueueStatus::Enqueued)
            .cloned()
            .collect();
        let already_downloading = results
            .iter()
            .any(|d| d.status == EnqueueStatus::AlreadyInFlight);

        if let Some(e) = error {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: format!("Download queue failed: {}", e),
            }));
        } else if let Some(reason) = refusal {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: reason.to_string(),
            }));
        } else if successful.is_empty() && !already_downloading {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: "All downloads failed to queue".to_string(),
            }));
        } else {
            // Send Downloading event
            let _ = tx.send(DownloadEvent::AutoDownload(
                AutoDownloadEvent::Downloading {
                    batch_id: batch_id.clone(),
                },
            ));
        }

        // Send initial Queued progress for each track (with batch fields per D-10, D-11)
        let queued_entries: Vec<DownloadProgress> = successful
            .iter()
            .map(|d| {
                row(DownloadProgress::queued(
                    d.id.clone(),
                    d.source.clone(),
                    d.item.clone(),
                    d.size,
                ))
            })
            .collect();
        if !queued_entries.is_empty() {
            let _ = tx.send(DownloadEvent::Progress(queued_entries));
        }

        match watcher {
            Watcher::Joined(reservation) => {
                info!(
                    "Auto-download: added {} tracks of '{}' to running batch {}",
                    successful.len(),
                    query_desc,
                    batch.owner.batch_id
                );
                let lost = deliver(reservation, &successful, &backend, &batch).await;
                if !lost.is_empty() {
                    let _ = tx.send(DownloadEvent::Progress(stopped_rows(&lost, &batch)));
                }
                return;
            }
            Watcher::Own if successful.is_empty() && batch.inbox.close_if_idle() => return,
            // Starts empty when nothing was enqueued but another request
            // joined the batch while this one was enqueueing.
            Watcher::Own => {}
        }

        let transfers: Vec<WatchedTransfer> =
            successful.iter().map(WatchedTransfer::from).collect();

        // Pool slots must line up with the monitor's slots, which follow
        // `successful`, not `picked.items`.
        let pool =
            SourcePool::from_tracks(&align_pool_tracks(&successful, &picked.items), &all_groups);

        info!(
            "Auto-download: queued {} tracks, starting monitor for '{}'",
            transfers.len(),
            query_desc
        );

        // Register task and run monitor (per D-05: normal DownloadProgress takes over)
        let task_cancellation = register_user_task(&task_username).await;

        let mut monitor = DownloadMonitor::new(
            transfers,
            batch.target.clone(),
            tx,
            task_cancellation,
            task_username.clone(),
            batch,
        )
        .with_failover(pool);
        monitor.run().await;
        unregister_user_task(&task_username).await;
    });

    Ok(AutoDownloadResult::Accepted {
        batch_id: batch_id_for_response,
    })
}
