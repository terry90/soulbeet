use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;

#[cfg(feature = "server")]
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, EnqueueStatus, QueuedDownload,
};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
#[cfg(feature = "server")]
use soulbeet::error::SoulseekError;

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use crate::core::metadata::{hydrate_album_tracks, lastfm_key};
#[cfg(feature = "server")]
use crate::core::{backend_unavailable, sources};
#[cfg(feature = "server")]
use crate::globals::{get_or_create_user_channel, register_user_task, unregister_user_task};
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::services::download_backend;
#[cfg(feature = "server")]
use crate::transfer_registry::{BatchRef, TransferOwner};
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::enqueue::{
    another_folder, deliver, enqueue_request, failed_reason, not_enqueued_failure, stopped_rows,
    RequestEnqueue, Watcher,
};
#[cfg(feature = "server")]
use super::failover::{align_pool_tracks, SourcePool};
#[cfg(feature = "server")]
use super::monitor::{DownloadMonitor, WatchedTransfer};

/// Score threshold for automatic source selection (per D-04).
/// Below this, the client gets FallbackToManual with results attached.
#[cfg(feature = "server")]
const AUTO_SELECT_SCORE_THRESHOLD: f64 = 0.7;

/// Why no file of a request was queued, shown as the request's failure.
#[cfg(feature = "server")]
fn queue_failure(results: &[QueuedDownload]) -> Failure {
    not_enqueued_failure(results).with_message("All downloads failed to queue")
}

#[cfg(feature = "server")]
fn target_unwritable(e: &std::io::Error) -> Failure {
    Failure::new(
        FailureCode::ImportTargetUnwritable,
        Stage::Import,
        RetryAdvice::NotRetryable,
        format!("Failed to create target directory: {e}"),
    )
    .with_cause(Service::Filesystem, None, e.to_string())
}

#[cfg(feature = "server")]
fn queue_failed(e: &SoulseekError) -> Failure {
    e.failure(Stage::Enqueue, format!("Download queue failed: {e}"))
}

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
    let user = auth.0.id();

    if let Some(issue) = crate::importer_health::broken_issue().await {
        return Ok(AutoDownloadResult::Error(issue.title));
    }
    let folder = match Folder::get_by_id(&req.folder_id).await {
        Ok(Some(f))
            if f.user_id == auth.0.user_id
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

    let (tx, _) = get_or_create_user_channel(&user).await;

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

    // Search every configured backend (per D-06).
    let (first, rest) = match sources::usable_backends().await {
        Ok(backends) => backends,
        Err(failure) => {
            let batch_id = uuid::Uuid::new_v4().to_string();
            let error = failure.message.clone();
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id,
                failure,
            }));
            return Ok(AutoDownloadResult::Error(error));
        }
    };

    // Generate batch_id upfront so we can return it immediately (per D-10)
    let batch_id = uuid::Uuid::new_v4().to_string();
    let batch_id_for_response = batch_id.clone();

    // Spawn the entire search-score-pick-download pipeline onto a background task.
    // This avoids blocking the HTTP response during the search-poll loop (Research Pitfall 4).
    let folder_path = folder.path;
    let mut query = req.query;
    let ctx = auth.0.clone();

    tokio::spawn(async move {
        // Send Searching event
        let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Searching {
            batch_id: batch_id.clone(),
            query: query_desc.clone(),
            backend_count: 1 + rest.len(),
        }));

        let key = match lastfm_key(&ctx).await {
            Ok(key) => key,
            Err(failure) => {
                warn!(
                    "Auto-download: could not read settings for '{query_desc}': {}",
                    failure.cause.as_ref().map_or("", |c| c.raw.as_str())
                );
                let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    failure,
                }));
                return;
            }
        };
        if let Err(failure) = hydrate_album_tracks(&mut query, key.as_deref()).await {
            info!(
                "Auto-download: no tracklist for '{}': {}",
                query_desc, failure.message
            );
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure,
            }));
            return;
        }
        let (album, tracks) = (query.album, query.tracks);

        info!(
            "Auto-download: searching {} backends for '{}'",
            1 + rest.len(),
            query_desc
        );

        // Search all backends in parallel (per D-06), best score first (per D-07)
        let mut all_groups =
            match sources::search_ranked(&first, &rest, album.as_ref(), &tracks).await {
                Ok(groups) => groups,
                Err(failure) => {
                    let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                        batch_id: batch_id.clone(),
                        failure,
                    }));
                    info!("Auto-download: no results for '{}'", query_desc);
                    return;
                }
            };

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
                failure: target_unwritable(&e),
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
                    failure: backend_unavailable(Stage::Enqueue, &e),
                }));
                return;
            }
        };

        let fresh = BatchRef::new(
            TransferOwner {
                user_id: user.clone(),
                batch_id: batch_id.clone(),
            },
            batch_label,
            target_path_buf,
        );
        let enq = enqueue_request(&backend, items, fresh).await;
        let refused = enq.refusal().is_some();
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
                    reason,
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
            .any(|d| d.status == EnqueueStatus::AlreadyRunning);

        if let Some(e) = error {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure: queue_failed(&e),
            }));
        } else if refused {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure: another_folder(RetryAdvice::NotRetryable),
            }));
        } else if successful.is_empty() && !already_downloading {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure: queue_failure(&results),
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
        let task_cancellation = register_user_task(&user).await;

        let mut monitor = DownloadMonitor::new(
            transfers,
            batch.target.clone(),
            tx,
            task_cancellation,
            batch,
        )
        .with_failover(pool);
        monitor.run().await;
        unregister_user_task(&user).await;
    });

    Ok(AutoDownloadResult::Accepted {
        batch_id: batch_id_for_response,
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn queue_failure_names_why_the_files_failed() {
        let failed = |code| QueuedDownload {
            id: "1".into(),
            source: "p".into(),
            item: "a".into(),
            size: 0,
            status: EnqueueStatus::Failed(Failure::new(
                code,
                Stage::Enqueue,
                RetryAdvice::OtherSource,
                "peer offline",
            )),
        };
        let f = queue_failure(&[failed(FailureCode::PeerOffline)]);
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailureCode::PeerOffline, "All downloads failed to queue")
        );
        assert_eq!(queue_failure(&[]).code, FailureCode::EnqueueFailed);
    }

    #[test]
    fn pipeline_failures_keep_their_text() {
        use RetryAdvice::*;
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let slskd = SoulseekError::Api {
            status: 503,
            message: "down".into(),
        };
        let (dir, queue) = (
            format!("Failed to create target directory: {io}"),
            format!("Download queue failed: {slskd}"),
        );
        for (f, code, stage, retry, message) in [
            (
                target_unwritable(&io),
                FailureCode::ImportTargetUnwritable,
                Stage::Import,
                NotRetryable,
                dir.as_str(),
            ),
            (
                queue_failed(&slskd),
                FailureCode::BackendUnreachable,
                Stage::Enqueue,
                SameSourceLater,
                queue.as_str(),
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, stage, retry, message)
            );
        }
        assert_eq!(
            target_unwritable(&io).cause.unwrap().service,
            Service::Filesystem
        );
    }
}
