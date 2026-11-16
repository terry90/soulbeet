//! The auto-download pipeline: search every backend, pick the best source
//! when it scores well enough, else wait for the user to pick one.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::metadata::{hydrate_album_tracks, lastfm_key};
use crate::core::sources::{self, Searched};
use crate::core::{backend_unavailable, database_failure};
use crate::globals::get_or_create_user_channel;
use crate::ledger::{
    DuplicateRule, JobEvent, JobKind, JobState, Ledger, NewJob, NewSearch, LEDGER,
};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::not_enqueued_failure;
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::download_backend;
use dioxus::logger::tracing::info;
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadQuery, DownloadableGroup, EnqueueStatus,
    QueuedDownload,
};
use shared::failure::{Failure, Stage};
use soulbeet::ImportMode;

use super::{
    enqueue_and_watch, manual_batch_label, numbered, prepare_target, request_stopped, Acquisition,
    NewBatch,
};

/// Name for an auto download's batch: the picked group's title, which a
/// track search leaves empty since no album was searched for, else the same
/// name a manual download of its items gets.
fn auto_batch_label(group: &DownloadableGroup) -> String {
    if group.title.trim().is_empty() {
        manual_batch_label(&group.items)
    } else {
        group.title.clone()
    }
}

/// Score threshold for automatic source selection (per D-04).
/// Below this, the client gets FallbackToManual with results attached.
const AUTO_SELECT_SCORE_THRESHOLD: f64 = 0.7;

/// Why no file of a request was queued, shown as the request's failure.
fn queue_failure(results: &[QueuedDownload]) -> Failure {
    not_enqueued_failure(results).with_message("All downloads failed to queue")
}

/// What an auto download's enqueue means for its search row: Downloading
/// when a file was enqueued or is already running into this folder, else
/// the failure of the request or of its files.
fn enqueue_outcome(
    batch_id: String,
    outcome: &Result<Vec<QueuedDownload>, Failure>,
) -> AutoDownloadEvent {
    let on_its_way = |d: &QueuedDownload| {
        matches!(
            d.status,
            EnqueueStatus::Enqueued | EnqueueStatus::AlreadyRunning
        )
    };
    match outcome {
        Ok(results) if results.iter().any(on_its_way) => {
            AutoDownloadEvent::Downloading { batch_id }
        }
        Ok(results) => AutoDownloadEvent::Failed {
            batch_id,
            failure: queue_failure(results),
        },
        Err(failure) => AutoDownloadEvent::Failed {
            batch_id,
            failure: failure.clone(),
        },
    }
}

/// Where a search pipeline reports: the UI's batch id, the job, its stored
/// search, and the import mode the job was recorded with.
pub(crate) struct AutoRun {
    pub batch_id: String,
    pub job: String,
    pub search_id: String,
    pub import_mode: ImportMode,
    /// Candidates the search already stored: the pipeline picks from them
    /// without hydrating the query or searching again.
    pub found: Option<Vec<DownloadableGroup>>,
}

/// Searches every backend for `query`, picks the best source when it scores
/// well enough, and downloads it into `folder`, the other sources standing by
/// as alternates. The request is a job from the start. Everything after the
/// backend lookup runs in the background and reports on the user's channel;
/// the batch id is returned at once.
pub async fn auto_download(
    ctx: &UserCtx,
    folder: Folder,
    query: DownloadQuery,
) -> Result<String, Failure> {
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }
    let key = lastfm_key(ctx).await?;

    // Search every configured backend (per D-06).
    let backends = match sources::usable_backends().await {
        Ok(backends) => backends,
        Err(failure) => {
            let _ = get_or_create_user_channel(&ctx.id())
                .await
                .send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
                    batch_id: uuid::Uuid::new_v4().to_string(),
                    failure: failure.clone(),
                }));
            return Err(failure);
        }
    };

    // Generate batch_id upfront so we can return it immediately (per D-10)
    let run = AutoRun {
        batch_id: uuid::Uuid::new_v4().to_string(),
        job: uuid::Uuid::new_v4().to_string(),
        search_id: uuid::Uuid::new_v4().to_string(),
        import_mode: CONFIG.import_mode(),
        found: None,
    };
    let batch_id = run.batch_id.clone();
    let (ctx, user) = (ctx.clone(), ctx.id());
    // Its own task, so a client that disconnects cannot leave a job whose
    // pipeline never started.
    let start = tokio::spawn(async move {
        LEDGER
            .create(
                NewJob {
                    id: run.job.clone(),
                    user: user.clone(),
                    folder_id: folder.id.clone(),
                    wanted: query.clone(),
                    import_mode: run.import_mode,
                    state: JobState::Searching,
                    search_id: Some(run.search_id.clone()),
                    picked: vec![],
                    kind: JobKind::of(&query),
                    options: None,
                },
                Some(NewSearch {
                    id: run.search_id.clone(),
                    user,
                    backend: None,
                    query: query.clone(),
                    spec: None,
                }),
            )
            .await
            .map_err(|e| database_failure(Stage::SourceSearch, e))?;
        // Spawn the entire search-score-pick-download pipeline onto a background task.
        // This avoids blocking the HTTP response during the search-poll loop (Research Pitfall 4).
        tokio::spawn(run_search(
            LEDGER.clone(),
            ctx,
            folder,
            query,
            key,
            backends,
            run,
        ));
        Ok::<_, Failure>(())
    });
    start.await.map_err(|e| request_stopped(&e))??;

    Ok(batch_id)
}

/// The spawned part of `auto_download`: searches, stores what it found as
/// the search's candidates, then waits for a choice from them when no source
/// scores well enough, else downloads the best one with the rest as
/// alternates. Given `run.found`, it picks from those instead of searching.
pub(crate) async fn run_search(
    ledger: Ledger,
    ctx: UserCtx,
    folder: Folder,
    mut query: DownloadQuery,
    lastfm_key: Option<String>,
    backends: (Searched, Vec<Searched>),
    mut run: AutoRun,
) {
    let (first, rest) = backends;
    let tx = get_or_create_user_channel(&ctx.id()).await;
    let send = |event| {
        let _ = tx.send(DownloadEvent::AutoDownload(event));
    };

    // Build search description for logging and events
    let query_desc = query
        .album
        .as_ref()
        .map(|a| format!("{} - {}", a.artist, a.title))
        .unwrap_or_else(|| {
            query
                .tracks
                .first()
                .map(|t| format!("{} - {}", t.artist, t.title))
                .unwrap_or_else(|| "Unknown query".to_string())
        });

    let mut all_groups = match run.found.take() {
        Some(groups) => groups,
        None => {
            send(AutoDownloadEvent::Searching {
                batch_id: run.batch_id.clone(),
                query: query_desc.clone(),
                backend_count: 1 + rest.len(),
            });
            if let Err(failure) = hydrate_album_tracks(&mut query, lastfm_key.as_deref()).await {
                info!(
                    "Auto-download: no tracklist for '{}': {}",
                    query_desc, failure.message
                );
                return fail_job(&ledger, &ctx, &run, failure).await;
            }
            let (album, tracks) = (query.album, query.tracks);

            info!(
                "Auto-download: searching {} backends for '{}'",
                1 + rest.len(),
                query_desc
            );

            // Search all backends in parallel (per D-06), best score first (per D-07)
            let groups = match sources::search_ranked(&first, &rest, album.as_ref(), &tracks).await
            {
                Ok(groups) => groups,
                Err(failure) => {
                    info!("Auto-download: no results for '{}'", query_desc);
                    return fail_job(&ledger, &ctx, &run, failure).await;
                }
            };
            if let Err(e) = ledger
                .store_candidates(&run.search_id, groups.clone())
                .await
            {
                return fail_job(
                    &ledger,
                    &ctx,
                    &run,
                    database_failure(Stage::SourceSearch, e),
                )
                .await;
            }
            groups
        }
    };

    let best_score = all_groups[0].score;

    send(AutoDownloadEvent::ScoringResults {
        batch_id: run.batch_id.clone(),
        result_count: all_groups.len(),
        best_score,
    });

    info!(
        "Auto-download: {} results, best score {:.2} for '{}'",
        all_groups.len(),
        best_score,
        query_desc
    );

    // Decide: auto-pick or fallback (per D-04)
    if best_score < AUTO_SELECT_SCORE_THRESHOLD {
        // Before the toast, so a pick from it finds the job waiting.
        ledger.record(JobEvent::NeedsSourceChoice {
            job: run.job.clone(),
        });
        send(AutoDownloadEvent::FallbackToManual {
            batch_id: run.batch_id.clone(),
            search_id: run.search_id.clone(),
            results: all_groups,
            best_score,
            threshold: AUTO_SELECT_SCORE_THRESHOLD,
        });
        info!(
            "Auto-download: score {:.2} < threshold {:.1}, falling back to manual for '{}'",
            best_score, AUTO_SELECT_SCORE_THRESHOLD, query_desc
        );
        return;
    }

    // Pick the best source
    let picked = all_groups.remove(0);

    send(AutoDownloadEvent::PickedSource {
        batch_id: run.batch_id.clone(),
        source: picked.source.clone(),
        score: picked.score,
        quality: picked.quality.clone(),
        track_count: picked.items.len(),
    });

    info!(
        "Auto-download: picked source '{}' (score {:.2}, {} tracks) for '{}'",
        picked.source,
        picked.score,
        picked.items.len(),
        query_desc
    );

    let target = match prepare_target(&folder).await {
        Ok(target) => target,
        Err(failure) => return fail_job(&ledger, &ctx, &run, failure).await,
    };

    let backend = match download_backend(None).await {
        Ok(b) => b,
        Err(e) => {
            let failure = backend_unavailable(Stage::Enqueue, &e);
            return fail_job(&ledger, &ctx, &run, failure).await;
        }
    };

    let fresh = NewBatch {
        batch_id: run.batch_id.clone(),
        label: auto_batch_label(&picked),
        target,
        import_mode: run.import_mode,
        duplicates: DuplicateRule::Guarded,
    };
    let items = numbered(picked.items);
    ledger.record(JobEvent::Picked {
        job: run.job.clone(),
        items: items.clone(),
    });
    let acquisition = Acquisition {
        job: run.job.clone(),
        items,
        alternates: all_groups,
        max_sources: MAX_SOURCES_PER_TRACK,
    };
    let outcome = enqueue_and_watch(&ledger, &ctx, backend, fresh, acquisition).await;
    match enqueue_outcome(run.batch_id.clone(), &outcome) {
        AutoDownloadEvent::Failed { failure, .. } => fail_job(&ledger, &ctx, &run, failure).await,
        event => send(event),
    }
}

/// Reports `failure` as the pipeline's and ends its job Failed.
pub(crate) async fn fail_job(ledger: &Ledger, ctx: &UserCtx, run: &AutoRun, failure: Failure) {
    let _ = get_or_create_user_channel(&ctx.id())
        .await
        .send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
            batch_id: run.batch_id.clone(),
            failure,
        }));
    ledger.record(JobEvent::Ended {
        job: run.job.clone(),
        state: JobState::Failed,
        failure: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::server_fns::download::enqueue::{another_folder, other_user};
    use crate::server_fns::download::test_support::{group, item, search_result, StubBackend};
    use shared::download::SearchState;
    use shared::failure::{FailureCode, RetryAdvice};
    use soulbeet::error::SoulseekError;
    use soulbeet::DownloadBackend;
    use std::sync::Arc;

    #[test]
    fn auto_label_is_the_group_title_else_its_items_name() {
        let mut group = DownloadableGroup {
            source: "p".to_string(),
            group_id: "g".to_string(),
            title: "Glass Atlas".to_string(),
            artist: None,
            item_count: 1,
            total_size: 0,
            items: vec![item("p", "Northern Line", "1.flac")],
            quality: "flac".to_string(),
            score: 1.0,
        };
        assert_eq!(auto_batch_label(&group), "Glass Atlas");

        // A track search: no album was searched for, so neither the group
        // nor its items name one.
        group.title = String::new();
        group.items[0].album = String::new();
        assert_eq!(auto_batch_label(&group), "Northern Line");
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
    fn the_pipeline_says_downloading_only_when_a_file_is_on_its_way() {
        let q = |status| QueuedDownload {
            id: "t-01.flac".into(),
            source: "peer_a".into(),
            item: "01.flac".into(),
            size: 27_213_250,
            status,
        };
        let downloading = |e: AutoDownloadEvent| matches!(e, AutoDownloadEvent::Downloading { batch_id } if batch_id == "b1");
        let failed = |e: AutoDownloadEvent| match e {
            AutoDownloadEvent::Failed { failure, .. } => failure,
            other => panic!("{other:?}"),
        };
        assert!(downloading(enqueue_outcome(
            "b1".into(),
            &Ok(vec![q(EnqueueStatus::Enqueued)])
        )));
        assert!(downloading(enqueue_outcome(
            "b1".into(),
            &Ok(vec![q(EnqueueStatus::AlreadyRunning)])
        )));
        let held = failed(enqueue_outcome(
            "b1".into(),
            &Ok(vec![q(EnqueueStatus::Failed(other_user()))]),
        ));
        assert_eq!(
            (held.code, held.message.as_str()),
            (other_user().code, "All downloads failed to queue")
        );
        let refused = failed(enqueue_outcome(
            "b1".into(),
            &Err(another_folder(RetryAdvice::NotRetryable)),
        ));
        assert_eq!(refused, another_folder(RetryAdvice::NotRetryable));
    }

    // Paused: search_ranked waits SEARCH_POLL_INTERVAL before each poll.
    #[tokio::test(start_paused = true)]
    async fn an_auto_download_with_no_confident_source_waits_for_a_choice_from_its_search() {
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = test_ctx("auto-fallback");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let folder = Folder {
            id: "f1".into(),
            user_id: ctx.user_id.clone(),
            name: "Music".into(),
            path: "/music".into(),
            shared: false,
        };
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(
                "peer_a",
                vec![item("peer_a", "Wake Up", "a/07.flac")],
                0.2,
            )],
        ))]);
        let a: Arc<dyn DownloadBackend> = stub;
        let run = AutoRun {
            batch_id: "b1".into(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found: None,
        };

        run_search(
            ledger,
            ctx,
            folder,
            DownloadQuery::new(vec![]),
            None,
            (("a".into(), a), vec![]),
            run,
        )
        .await;

        assert!(recorded
            .events()
            .contains(&JobEvent::NeedsSourceChoice { job: "j1".into() }));
        let fallback = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|e| match e {
            DownloadEvent::AutoDownload(AutoDownloadEvent::FallbackToManual {
                search_id, ..
            }) => Some(search_id),
            _ => None,
        });
        assert_eq!(fallback, Some("s1".to_string()));
    }

    #[tokio::test]
    async fn an_auto_download_given_stored_candidates_picks_from_them_without_searching() {
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = test_ctx("auto-found");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let folder = Folder {
            id: "f1".into(),
            user_id: ctx.user_id.clone(),
            name: "Music".into(),
            path: "/music".into(),
            shared: false,
        };
        let stored = vec![group(
            "peer_a",
            vec![item("peer_a", "Wake Up", "a/07.flac")],
            0.2,
        )];
        // A search of this backend would fail: starting one is refused.
        let stub = StubBackend::new(vec![]);
        stub.fail_search_start(SoulseekError::InvalidResponse("refused".into()));
        let a: Arc<dyn DownloadBackend> = stub;
        let run = AutoRun {
            batch_id: "b1".into(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found: Some(stored.clone()),
        };

        run_search(
            ledger,
            ctx,
            folder,
            DownloadQuery::new(vec![]),
            None,
            (("a".into(), a), vec![]),
            run,
        )
        .await;

        assert_eq!(
            recorded.events(),
            vec![JobEvent::NeedsSourceChoice { job: "j1".into() }]
        );
        let events: Vec<AutoDownloadEvent> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                DownloadEvent::AutoDownload(event) => Some(event),
                _ => None,
            })
            .collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AutoDownloadEvent::Searching { .. })),
            "no search runs"
        );
        let offered = events.into_iter().find_map(|e| match e {
            AutoDownloadEvent::FallbackToManual { results, .. } => Some(results),
            _ => None,
        });
        assert_eq!(offered, Some(stored));
    }
}
