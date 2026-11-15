use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;

#[cfg(feature = "server")]
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, DownloadableGroup, EnqueueStatus,
    QueuedDownload, SearchState,
};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
#[cfg(feature = "server")]
use shared::metadata::{Album, Track};
#[cfg(feature = "server")]
use soulbeet::error::SoulseekError;
#[cfg(feature = "server")]
use soulbeet::DownloadBackend;

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
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::server_fns::search::{hydrate_album_tracks, TracklistError};
#[cfg(feature = "server")]
use crate::services::{
    available_download_backends, download_backend, BackendLookupError, DEFAULT_DOWNLOAD_BACKEND,
};
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

/// Maximum time to wait for search results from a single backend. A safety
/// net above the slskd client's own deadline, which cancels a search at 120
/// seconds and gives it 15 more to end.
#[cfg(feature = "server")]
const SEARCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Poll interval when waiting for search results.
#[cfg(feature = "server")]
const SEARCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How one backend's search ended.
#[cfg(feature = "server")]
enum SearchStop {
    /// The backend finished the search, its responses holding `files_seen`
    /// files.
    Finished { files_seen: u32 },
    /// SEARCH_TIMEOUT passed before the backend finished.
    TimedOut,
    /// Starting or polling the search failed.
    Failed(Failure),
}

/// One backend's search, `id` naming it in the logs: the groups it found and
/// how it ended.
#[cfg(feature = "server")]
async fn search_backend(
    id: &str,
    backend: &Arc<dyn DownloadBackend>,
    album: Option<&Album>,
    tracks: &[Track],
) -> (Vec<DownloadableGroup>, SearchStop) {
    let search_id = match backend.start_search(album, tracks).await {
        Ok(sid) => sid,
        Err(e) => {
            warn!("Backend {} search start failed: {}", id, e);
            let stop = SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()));
            return (Vec::new(), stop);
        }
    };

    // Poll until results are ready or timeout. A backend may send
    // groups while the search is in progress, so keep the latest
    // non-empty batch. slskd sends its groups only in the completing
    // poll.
    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
    let mut latest_groups = Vec::<DownloadableGroup>::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            warn!("Backend {} search timed out", id);
            return (latest_groups, SearchStop::TimedOut);
        }
        tokio::time::sleep(SEARCH_POLL_INTERVAL).await;

        match backend.poll_search(&search_id).await {
            Ok(result) => match result.state {
                SearchState::Completed | SearchState::TimedOut => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                    let stop = SearchStop::Finished {
                        files_seen: result.files_seen,
                    };
                    return (latest_groups, stop);
                }
                SearchState::InProgress => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                }
            },
            Err(e) => {
                warn!("Backend {} poll error: {}", id, e);
                let stop = SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()));
                return (latest_groups, stop);
            }
        }
    }
}

/// Why a search of `first` and `rest` found nothing: when any backend
/// finished, an empty search, or one whose files were all scored out when its
/// responses held files; else a timeout when any timed out, else the first
/// backend's own failure.
#[cfg(feature = "server")]
fn no_results_failure(first: &SearchStop, rest: &[SearchStop]) -> Failure {
    const MESSAGE: &str = "No results found";
    let rank = |stop: &SearchStop| match stop {
        SearchStop::Finished { .. } => 2,
        SearchStop::TimedOut => 1,
        SearchStop::Failed(_) => 0,
    };
    // The earliest of the highest rank.
    let stop = rest
        .iter()
        .fold(first, |best, s| if rank(s) > rank(best) { s } else { best });
    match stop {
        SearchStop::Finished { files_seen: 1.. } => Failure::new(
            FailureCode::SearchAllFiltered,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::Finished { files_seen: 0 } => Failure::new(
            FailureCode::SearchNoResults,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::TimedOut => Failure::new(
            FailureCode::SearchTimeout,
            Stage::SourceSearch,
            RetryAdvice::SameSourceLater,
            MESSAGE,
        ),
        SearchStop::Failed(f) => f.clone().with_message(MESSAGE),
    }
}

/// Why no file of a request was queued, shown as the request's failure.
#[cfg(feature = "server")]
fn queue_failure(results: &[QueuedDownload]) -> Failure {
    not_enqueued_failure(results).with_message("All downloads failed to queue")
}

/// No backend was available, `e` saying why the default one was not.
#[cfg(feature = "server")]
fn no_backends(e: &BackendLookupError) -> Failure {
    e.failure(Stage::SourceSearch, "No download backends available")
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
fn backend_unavailable(e: &BackendLookupError) -> Failure {
    e.failure(
        Stage::Enqueue,
        format!("Download backend not available: {e}"),
    )
}

/// A backend an auto-download searches, by its id in the settings.
#[cfg(feature = "server")]
type Searched = (String, Arc<dyn DownloadBackend>);

/// The backends to search, the default one first when it is available, or
/// the default one's lookup error when none is.
#[cfg(feature = "server")]
async fn usable_backends() -> Result<(Searched, Vec<Searched>), BackendLookupError> {
    let mut others = Vec::new();
    for (id, _) in available_download_backends() {
        if id == DEFAULT_DOWNLOAD_BACKEND {
            continue;
        }
        match download_backend(Some(id)).await {
            Ok(b) => others.push((id.to_string(), b)),
            Err(e) => warn!("Backend {} unavailable for auto-download: {}", id, e),
        }
    }
    match download_backend(None).await {
        Ok(b) => Ok(((DEFAULT_DOWNLOAD_BACKEND.to_string(), b), others)),
        Err(e) => {
            warn!(
                "Backend {} unavailable for auto-download: {}",
                DEFAULT_DOWNLOAD_BACKEND, e
            );
            let mut others = others.into_iter();
            match others.next() {
                Some(first) => Ok((first, others.collect())),
                None => Err(e),
            }
        }
    }
}

/// The tracklist of the album could not be looked up.
#[cfg(feature = "server")]
fn tracklist_failure(e: &TracklistError) -> Failure {
    e.error.failure(Stage::Metadata, e.to_string())
}

/// The user's settings could not be read, which the tracklist lookup needs.
#[cfg(feature = "server")]
fn settings_unreadable(e: &str) -> Failure {
    Failure::new(
        FailureCode::SettingsUnreadable,
        Stage::Metadata,
        RetryAdvice::SameSourceLater,
        "Could not read your settings",
    )
    .with_cause(Service::Database, None, e)
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
    let (first, rest) = match usable_backends().await {
        Ok(backends) => backends,
        Err(e) => {
            let batch_id = uuid::Uuid::new_v4().to_string();
            let failure = no_backends(&e);
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
    let auth_user_id = auth.0.user_id.clone();

    tokio::spawn(async move {
        // Send Searching event
        let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Searching {
            batch_id: batch_id.clone(),
            query: query_desc.clone(),
            backend_count: 1 + rest.len(),
        }));

        let settings = match UserSettings::get(&auth_user_id).await {
            Ok(settings) => settings,
            Err(e) => {
                let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    failure: settings_unreadable(&e),
                }));
                warn!("Auto-download: could not read settings for '{query_desc}': {e}");
                return;
            }
        };
        if let Err(e) = hydrate_album_tracks(&mut query, settings.lastfm_api_key.as_deref()).await {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure: tracklist_failure(&e),
            }));
            info!(
                "Auto-download: no tracklist for '{}': {}",
                query_desc, e.error
            );
            return;
        }
        let (album, tracks) = (query.album, query.tracks);

        info!(
            "Auto-download: searching {} backends for '{}'",
            1 + rest.len(),
            query_desc
        );

        // Search all backends in parallel (per D-06)
        let ((first_groups, first_stop), results) = futures::future::join(
            search_backend(&first.0, &first.1, album.as_ref(), &tracks),
            futures::future::join_all(
                rest.iter()
                    .map(|(id, backend)| search_backend(id, backend, album.as_ref(), &tracks)),
            ),
        )
        .await;

        // Merge all results into a single list sorted by score (per D-07)
        let (groups, stops): (Vec<_>, Vec<_>) = results.into_iter().unzip();
        let mut all_groups: Vec<DownloadableGroup> = first_groups
            .into_iter()
            .chain(groups.into_iter().flatten())
            .collect();

        if all_groups.is_empty() {
            let _ = tx.send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure: no_results_failure(&first_stop, &stops),
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
                    failure: backend_unavailable(&e),
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
    use crate::server_fns::download::test_support::{group, item, search_result, StubBackend};
    use shared::download::SearchResult;
    use shared::failure::UserActionCode;
    use soulbeet::error::MissingSetting;
    use soulbeet::MetadataError;

    /// The groups `stub`'s search found and the failure an empty one gives.
    async fn searched(stub: Arc<StubBackend>) -> (Vec<DownloadableGroup>, Failure) {
        let backend: Arc<dyn DownloadBackend> = stub;
        let (groups, stop) = search_backend("stub", &backend, None, &[]).await;
        (groups, no_results_failure(&stop, &[]))
    }

    fn advice(f: &Failure) -> (FailureCode, Stage, RetryAdvice, &str) {
        (f.code, f.stage, f.retry, f.message.as_str())
    }

    // Paused: every poll waits SEARCH_POLL_INTERVAL first.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_refuses_to_start_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        let refused = "The server connection must be connected and logged in to perform a search (currently: Disconnected)";
        stub.fail_search_start(SoulseekError::Api {
            status: 409,
            message: refused.into(),
        });

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        let cause = f.cause.expect("slskd's answer");
        assert_eq!((cause.status, cause.raw.as_str()), (Some(409), refused));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_whose_poll_fails_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".into(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(503));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_that_finishes_empty_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            Vec::new(),
        ))]);

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchNoResults,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(f.cause, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_still_running_at_the_deadline_is_a_timeout() {
        let stub = StubBackend::new(vec![]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchTimeout,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
    }

    /// A backend may send groups while the search runs: those an earlier
    /// poll found are the result when the completing poll is empty.
    #[tokio::test(start_paused = true)]
    async fn a_search_keeps_the_groups_found_before_it_completed() {
        let stub = StubBackend::new(vec![]);
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "Wake Up", "a.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(search_result(
                "search",
                SearchState::InProgress,
                found.clone(),
            )),
            Ok(search_result("search", SearchState::Completed, Vec::new())),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub;

        let (groups, stop) = search_backend("stub", &backend, None, &[]).await;

        assert_eq!(groups, found);
        assert!(matches!(stop, SearchStop::Finished { .. }));
    }

    #[test]
    fn a_search_whose_files_were_all_scored_out_is_all_filtered() {
        let f = no_results_failure(
            &SearchStop::Finished { files_seen: 7 },
            &[SearchStop::TimedOut],
        );
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SearchAllFiltered,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(
            no_results_failure(&SearchStop::Finished { files_seen: 0 }, &[]).code,
            FailureCode::SearchNoResults
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_search_reports_the_files_it_saw() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(SearchResult {
            files_seen: 7,
            ..search_result("search", SearchState::Completed, Vec::new())
        })]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let (_, stop) = search_backend("stub", &backend, None, &[]).await;
        assert!(matches!(stop, SearchStop::Finished { files_seen: 7 }));
    }

    #[test]
    fn no_results_failure_tells_an_empty_search_from_a_dead_backend() {
        let dead = |message: &str| {
            SearchStop::Failed(
                SoulseekError::Api {
                    status: 503,
                    message: message.into(),
                }
                .failure(Stage::SourceSearch, "x"),
            )
        };
        let advice = |first: SearchStop, rest: Vec<SearchStop>| {
            let f = no_results_failure(&first, &rest);
            (f.code, f.retry)
        };
        use FailureCode::*;
        use RetryAdvice::*;
        assert_eq!(
            advice(SearchStop::Finished { files_seen: 0 }, vec![]),
            (SearchNoResults, OtherQuery)
        );
        assert_eq!(
            advice(SearchStop::TimedOut, vec![]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![]),
            (BackendUnreachable, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![SearchStop::Finished { files_seen: 0 }]),
            (SearchNoResults, OtherQuery)
        );
        // A timeout says more than a dead backend, an empty search more than both.
        assert_eq!(
            advice(dead("down"), vec![SearchStop::TimedOut]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(
                SearchStop::TimedOut,
                vec![SearchStop::Finished { files_seen: 0 }]
            ),
            (SearchNoResults, OtherQuery)
        );
        let f = no_results_failure(&dead("first"), &[dead("second")]);
        assert_eq!(f.message, "No results found");
        assert_eq!(f.stage, Stage::SourceSearch);
        assert_eq!(f.cause.unwrap().raw, "first");
    }

    /// slskd lost the search: its 404 on the responses is a failed search
    /// to run again, not an empty one.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_no_longer_has_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::SearchGone {
            body: String::new(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchLost,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(404));
    }

    /// slskd refused the query itself: another query, not a wait.
    #[tokio::test(start_paused = true)]
    async fn a_query_slskd_refuses_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        let body = "Search query must contain at least one non-exclusion term (Parameter 'query')";
        stub.fail_search_start(SoulseekError::SearchQueryRejected { body: body.into() });

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchQueryRejected,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        let cause = f.cause.unwrap();
        assert_eq!((cause.status, cause.raw.as_str()), (Some(400), body));
    }

    #[test]
    fn a_tracklist_that_cannot_be_read_fails_at_the_metadata_stage() {
        let e = MetadataError::Unavailable {
            service: Service::Musicbrainz,
            status: Some(503),
            raw: "no answer within 15 s".into(),
        };
        let f = tracklist_failure(&TracklistError {
            album_title: "Winter Songs".into(),
            error: e,
        });
        assert_eq!(
            (f.code, f.stage),
            (FailureCode::MetadataUnavailable, Stage::Metadata)
        );
        assert_eq!(
            f.message,
            "could not resolve tracklist for 'Winter Songs': MusicBrainz is unavailable: no answer within 15 s"
        );
    }

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
        let unset = BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url));
        let (dir, backend, queue) = (
            format!("Failed to create target directory: {io}"),
            "Download backend not available: slskd URL not configured".to_string(),
            format!("Download queue failed: {slskd}"),
        );
        for (f, code, stage, retry, message) in [
            (
                no_backends(&unset),
                FailureCode::BackendNotConfigured,
                Stage::SourceSearch,
                NeedsUser,
                "No download backends available",
            ),
            (
                target_unwritable(&io),
                FailureCode::ImportTargetUnwritable,
                Stage::Import,
                NotRetryable,
                dir.as_str(),
            ),
            (
                backend_unavailable(&unset),
                FailureCode::BackendNotConfigured,
                Stage::Enqueue,
                NeedsUser,
                backend.as_str(),
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
        for f in [no_backends(&unset), backend_unavailable(&unset)] {
            assert_eq!(
                f.user_action.unwrap().code,
                UserActionCode::ConfigureBackend
            );
            assert_eq!(f.cause.unwrap().raw, "slskd URL not configured");
        }
    }
}
