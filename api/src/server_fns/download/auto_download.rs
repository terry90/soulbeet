use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;

#[cfg(feature = "server")]
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, DownloadableGroup, SearchState,
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
use crate::services::{available_download_backends, download_backend};
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::monitor::DownloadMonitor;

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
    let (tx, _) = get_or_create_user_channel(&username).await;

    // Build search description for logging and events
    let query_desc = req
        .query
        .album
        .as_ref()
        .map(|a| {
            format!(
                "{} - {}",
                &a.artist,
                &a.title
            )
        })
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
    let folder_path = req.folder_path.clone();
    let album = req.query.album.clone();
    let tracks = req.query.tracks.clone();
    let task_username = username.clone();

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

                    // Poll until results are ready or timeout
                    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
                    loop {
                        if tokio::time::Instant::now() >= deadline {
                            warn!("Backend {} search timed out", id);
                            break;
                        }
                        tokio::time::sleep(SEARCH_POLL_INTERVAL).await;

                        match backend.poll_search(&search_id).await {
                            Ok(result) => match result.state {
                                SearchState::Completed | SearchState::TimedOut => {
                                    return (id, result.groups);
                                }
                                SearchState::NotFound => {
                                    return (id, Vec::new());
                                }
                                SearchState::InProgress => continue,
                            },
                            Err(e) => {
                                warn!("Backend {} poll error: {}", id, e);
                                return (id, Vec::new());
                            }
                        }
                    }
                    (id, Vec::new())
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
        all_groups
            .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

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

        let batch_label = picked.title.clone();

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

        let queued = match backend.download(items).await {
            Ok(q) => q,
            Err(e) => {
                let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    error: format!("Download queue failed: {}", e),
                }));
                return;
            }
        };

        let (failed, successful): (Vec<_>, Vec<_>) =
            queued.iter().cloned().partition(|d| d.error.is_some());

        // Send failed entries as progress events
        if !failed.is_empty() {
            let failed_entries: Vec<DownloadProgress> = failed
                .iter()
                .map(|d| {
                    DownloadProgress::failed(
                        d.id.clone(),
                        d.source.clone(),
                        d.item.clone(),
                        d.error.clone().unwrap_or_default(),
                    )
                    .with_batch(batch_id.clone(), batch_label.clone())
                })
                .collect();
            let _ = tx.send(DownloadEvent::Progress(failed_entries));
        }

        if successful.is_empty() {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                error: "All downloads failed to queue".to_string(),
            }));
            return;
        }

        // Send Downloading event
        let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Downloading {
            batch_id: batch_id.clone(),
        }));

        // Send initial Queued progress for each track (with batch fields per D-10, D-11)
        let queued_entries: Vec<DownloadProgress> = successful
            .iter()
            .map(|d| {
                DownloadProgress::queued(d.id.clone(), d.source.clone(), d.item.clone(), d.size)
                    .with_batch(batch_id.clone(), batch_label.clone())
            })
            .collect();
        let _ = tx.send(DownloadEvent::Progress(queued_entries));

        let download_sources: Vec<String> =
            successful.iter().map(|d| d.source.clone()).collect();
        let download_filenames: Vec<String> =
            successful.iter().map(|d| d.item.clone()).collect();

        info!(
            "Auto-download: queued {} tracks, starting monitor for '{}'",
            download_filenames.len(),
            query_desc
        );

        // Register task and run monitor (per D-05: normal DownloadProgress takes over)
        let task_cancellation = register_user_task(&task_username).await;

        let mut monitor = DownloadMonitor::new(
            download_sources,
            download_filenames,
            target_path_buf,
            tx,
            task_cancellation,
            task_username.clone(),
            Some(batch_id),
            Some(batch_label),
        );
        monitor.run().await;
        unregister_user_task(&task_username).await;
    });

    Ok(AutoDownloadResult::Accepted {
        batch_id: batch_id_for_response,
    })
}
