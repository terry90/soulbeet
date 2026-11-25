//! The auto-download pipeline: search every backend, pick the best source
//! when it scores well enough, else wait for the user to pick one.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::metadata::hydrate_album_tracks;
use crate::core::settings;
use crate::core::sources::{self, Usable};
use crate::core::{backend_unavailable, database_failure};
use crate::globals::get_or_create_user_channel;
use crate::ledger::{
    DuplicateRule, JobEvent, JobKind, JobState, Ledger, NewJob, NewSearch, LEDGER,
};
use crate::models::folder::Folder;
use crate::models::user_settings::LossyFallback;
use crate::server_fns::download::enqueue::not_enqueued_failure;
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::{Backends, Registry};
use dioxus::logger::tracing::info;
use shared::backends::{delivery, Delivery};
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadQuery, DownloadableGroup, EnqueueStatus,
    ManualReason, QueuedDownload,
};
use shared::failure::{Failure, Stage};
use soulbeet::ImportMode;

use super::{
    backend_of, enqueue_and_watch, manual_batch_label, numbered, prepare_target, request_stopped,
    Acquisition, NewBatch,
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

/// Score threshold for automatic source selection (per D-04), and an MCP job's default
/// `min_score`. Below this, the client gets FallbackToManual with results attached.
pub(crate) const AUTO_SELECT_SCORE_THRESHOLD: f64 = 0.7;

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

    // Search every configured backend (per D-06), the lossy-only ones as the user allows.
    let usable = async {
        let lossy = settings::get(ctx).await?.lossy_fallback;
        sources::usable_backends(lossy).await
    };
    let backends = match usable.await {
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
            backends,
            &Registry,
            run,
        ));
        Ok::<_, Failure>(())
    });
    start.await.map_err(|e| request_stopped(&e))??;

    Ok(batch_id)
}

/// The lossy-only backend whose files `group` holds; None for any other group.
fn lossy_backend(group: &DownloadableGroup) -> Option<&str> {
    (group.items.first())
        .map(|item| item.backend.as_str())
        .filter(|backend| delivery(backend) == Delivery::LossyOnly)
}

/// Whether `group` holds a lossy-only backend's files.
fn lossy(group: &DownloadableGroup) -> bool {
    lossy_backend(group).is_some()
}

/// Whether the pipeline picks from `groups`, best first, without the user: one of a backend that
/// delivers files as found scores well enough.
fn picks_primary(groups: &[DownloadableGroup]) -> bool {
    (groups.iter()).any(|g| !lossy(g) && g.score >= AUTO_SELECT_SCORE_THRESHOLD)
}

/// The group the pipeline picks from `groups` without the user: the first of a backend that
/// delivers files as found that scores well enough, else, only when the user takes lossy files
/// automatically, the first lossy-only one that does. Else why the user picks: lossy-only when
/// such a group scores well enough and the user is asked before lossy files.
fn pick(
    groups: &[DownloadableGroup],
    lossy_fallback: LossyFallback,
) -> Result<usize, ManualReason> {
    let passes = |g: &DownloadableGroup| g.score >= AUTO_SELECT_SCORE_THRESHOLD;
    if let Some(i) = groups.iter().position(|g| !lossy(g) && passes(g)) {
        return Ok(i);
    }
    let confident_lossy = (groups.iter().enumerate())
        .filter(|(_, g)| passes(g))
        .find_map(|(i, g)| Some((i, lossy_backend(g)?)));
    match (confident_lossy, lossy_fallback) {
        (Some((i, _)), LossyFallback::Auto) => Ok(i),
        (Some((_, backend)), LossyFallback::Ask) => Err(ManualReason::LossyOnly {
            backend: backend.to_string(),
        }),
        _ => Err(ManualReason::NoConfidentMatch),
    }
}

/// The spawned part of `auto_download`: searches, stores what it found as
/// the search's candidates, then waits for a choice from them when no source
/// scores well enough, else downloads the best one with the rest as
/// alternates, on the backend its files name, looked up in `backends`. Given
/// `run.found`, it picks from those instead of searching.
///
/// The lossy-only backends are searched only when no other group would be picked, their groups
/// after every other one; one of them is picked only when the user takes lossy files
/// automatically.
pub(crate) async fn run_search(
    ledger: Ledger,
    ctx: UserCtx,
    folder: Folder,
    mut query: DownloadQuery,
    usable: Usable,
    backends: &dyn Backends,
    mut run: AutoRun,
) {
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
        Some(mut groups) => {
            if usable.lossy == LossyFallback::Never {
                groups.retain(|g| !lossy(g));
            }
            if groups.is_empty() {
                return fail_job(&ledger, &ctx, &run, sources::no_candidate_passed()).await;
            }
            groups
        }
        None => {
            send(AutoDownloadEvent::Searching {
                batch_id: run.batch_id.clone(),
                query: query_desc.clone(),
                backend_count: usable.primary.len(),
            });
            if let Err(failure) = hydrate_album_tracks(&mut query).await {
                info!(
                    "Auto-download: no tracklist for '{}': {}",
                    query_desc, failure.message
                );
                return fail_job(&ledger, &ctx, &run, failure).await;
            }
            let (album, tracks) = (query.album, query.tracks);

            info!(
                "Auto-download: searching {} backends for '{}'",
                usable.primary.len(),
                query_desc
            );

            // Search all backends in parallel (per D-06), best score first (per D-07)
            let mut found = sources::search_tier(&usable.primary, album.as_ref(), &tracks).await;
            if !picks_primary(&found.groups) && !usable.last_resort.is_empty() {
                info!(
                    "Auto-download: no source to pick for '{}'; searching {} lossy-only backends",
                    query_desc,
                    usable.last_resort.len()
                );
                send(AutoDownloadEvent::Searching {
                    batch_id: run.batch_id.clone(),
                    query: query_desc.clone(),
                    backend_count: usable.primary.len() + usable.last_resort.len(),
                });
                let last = sources::search_tier(&usable.last_resort, album.as_ref(), &tracks).await;
                found = found.then(last);
            }
            if found.groups.is_empty() {
                info!("Auto-download: no results for '{}'", query_desc);
                return fail_job(&ledger, &ctx, &run, found.failure()).await;
            }
            let groups = found.groups;
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
    let picked = match pick(&all_groups, usable.lossy) {
        Ok(picked) => picked,
        Err(reason) => {
            info!(
                "Auto-download: no source to pick at threshold {:.1} ({:?}), falling back to manual for '{}'",
                AUTO_SELECT_SCORE_THRESHOLD, reason, query_desc
            );
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
                reason,
            });
            return;
        }
    };
    let picked = all_groups.remove(picked);

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

    let routed = match backend_of(&picked.items) {
        Ok(id) => id.to_string(),
        Err(failure) => return fail_job(&ledger, &ctx, &run, failure).await,
    };
    let backend = match backends.get(&routed).await {
        Ok(b) => b,
        Err(e) => {
            let failure = backend_unavailable(Stage::Enqueue, &e);
            return fail_job(&ledger, &ctx, &run, failure).await;
        }
    };

    let duplicates = DuplicateRule::Guarded.for_delivery(delivery(&routed));
    let fresh = NewBatch {
        batch_id: run.batch_id.clone(),
        label: auto_batch_label(&picked),
        backend: routed,
        target,
        import_mode: run.import_mode,
        duplicates,
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

/// Reports `failure` as the pipeline's and ends its job Failed with it, so get_jobs shows it
/// when no tab knows the batch, as after a restart.
pub(crate) async fn fail_job(ledger: &Ledger, ctx: &UserCtx, run: &AutoRun, failure: Failure) {
    ledger.record(JobEvent::Ended {
        job: run.job.clone(),
        state: JobState::Failed,
        failure: Some(failure.clone()),
    });
    let _ = get_or_create_user_channel(&ctx.id())
        .await
        .send(DownloadEvent::AutoDownload(AutoDownloadEvent::Failed {
            batch_id: run.batch_id.clone(),
            failure,
        }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::server_fns::download::enqueue::{another_folder, other_user};
    use crate::server_fns::download::test_support::{
        group, item, item_on, search_result, ScriptedOutcome, StubBackend, StubBackends,
    };
    use crate::services::BackendLookupError;
    use shared::download::SearchState;
    use shared::failure::{FailureCode, RetryAdvice};
    use soulbeet::error::{MissingSetting, SoulseekError};
    use soulbeet::DownloadBackend;
    use std::sync::Arc;

    /// The backends a user who is asked before lossy files searches: `primary` only.
    fn usable(primary: Vec<crate::core::sources::Searched>) -> Usable {
        Usable {
            primary,
            last_resort: vec![],
            lossy: LossyFallback::Ask,
        }
    }

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
            usable(vec![("a".into(), a)]),
            &StubBackends::default(),
            run,
        )
        .await;

        assert!(recorded
            .events()
            .contains(&JobEvent::NeedsSourceChoice { job: "j1".into() }));
        let fallback = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|e| match e {
            DownloadEvent::AutoDownload(AutoDownloadEvent::FallbackToManual {
                search_id,
                reason,
                ..
            }) => Some((search_id, reason)),
            _ => None,
        });
        assert_eq!(
            fallback,
            Some(("s1".to_string(), ManualReason::NoConfidentMatch))
        );
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
            usable(vec![("a".into(), a)]),
            &StubBackends::default(),
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

    /// `ctx`'s folder at `dir`.
    fn folder_at(dir: &tempfile::TempDir, ctx: &UserCtx) -> Folder {
        Folder {
            id: "f1".into(),
            user_id: ctx.user_id.clone(),
            name: "Music".into(),
            path: dir.path().to_string_lossy().into_owned(),
            shared: false,
        }
    }

    // Paused: search_ranked waits SEARCH_POLL_INTERVAL before each poll.
    #[tokio::test(start_paused = true)]
    async fn an_auto_download_sends_the_best_source_to_the_backend_that_found_it() {
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = test_ctx("auto-routed");
        let dir = tempfile::tempdir().unwrap();
        let found = |backend: &str, peer: &str, score| {
            let items = vec![item_on(backend, peer, "Wake Up", "07.flac")];
            vec![group(peer, items, score)]
        };
        let (a, b) = (
            StubBackend::new(vec![ScriptedOutcome::Enqueued]),
            StubBackend::new(vec![ScriptedOutcome::Enqueued]),
        );
        a.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("a", "peer_a", 0.8),
        ))]);
        b.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("b", "peer_b", 0.9),
        ))]);
        let backends = StubBackends::default()
            .with("a", a.clone())
            .with("b", b.clone());
        let run = AutoRun {
            batch_id: "b1".into(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found: None,
        };

        run_search(
            ledger,
            ctx.clone(),
            folder_at(&dir, &ctx),
            DownloadQuery::new(vec![]),
            usable(vec![
                ("a".into(), a.clone() as Arc<dyn DownloadBackend>),
                ("b".into(), b.clone() as Arc<dyn DownloadBackend>),
            ]),
            &backends,
            run,
        )
        .await;

        assert_eq!(backends.asked(), vec!["b".to_string()]);
        assert_eq!(b.requests().len(), 1);
        assert!(a.requests().is_empty());
        let watched: Vec<(Option<String>, Option<String>)> = recorded
            .events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots {
                    state: JobState::Downloading,
                    rows,
                } => Some(rows),
                _ => None,
            })
            .flatten()
            .map(|row| (row.batch_id, row.backend))
            .collect();
        assert_eq!(watched, vec![(Some("b1".into()), Some("b".into()))]);
    }

    #[tokio::test]
    async fn an_auto_download_whose_backend_cannot_be_looked_up_fails_at_enqueue() {
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = test_ctx("auto-unresolved");
        let dir = tempfile::tempdir().unwrap();
        let stored = vec![group(
            "peer_b",
            vec![item_on("b", "peer_b", "Wake Up", "b/07.flac")],
            0.9,
        )];
        let a = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backends = StubBackends::default().with("a", a.clone());
        let unset =
            BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url).into());
        backends.fail("b", unset.clone());
        let run = AutoRun {
            batch_id: "b1".into(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found: Some(stored),
        };

        run_search(
            ledger,
            ctx.clone(),
            folder_at(&dir, &ctx),
            DownloadQuery::new(vec![]),
            usable(vec![("a".into(), a.clone() as Arc<dyn DownloadBackend>)]),
            &backends,
            run,
        )
        .await;

        assert_eq!(
            recorded.events(),
            vec![JobEvent::Ended {
                job: "j1".into(),
                state: JobState::Failed,
                failure: Some(backend_unavailable(Stage::Enqueue, &unset)),
            }]
        );
        assert_eq!(backends.asked(), vec!["b".to_string()]);
        assert!(a.specs().is_empty() && a.requests().is_empty());
    }

    #[tokio::test]
    async fn a_resumed_auto_download_of_a_lastfm_album_fails_with_a_message_to_search_again() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "auto-lastfm").await;
        let folder = Folder::get_by_id_in(&db, &folder_id)
            .await
            .unwrap()
            .unwrap();
        let ledger = Ledger::start(db.clone());
        let ctx = test_ctx("auto-lastfm");
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: ctx.id(),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Searching,
                    search_id: None,
                    picked: vec![],
                    kind: JobKind::Album,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        // A search of this backend would fail: starting one is refused.
        let stub = StubBackend::new(vec![]);
        stub.fail_search_start(SoulseekError::InvalidResponse("refused".into()));
        let a: Arc<dyn DownloadBackend> = stub;
        let run = AutoRun {
            batch_id: "b1".into(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found: None,
        };
        let album = shared::metadata::Album {
            id: "lastfm:album:marrow choir:winter songs".into(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
            first_release_date: None,
        };

        run_search(
            ledger.clone(),
            ctx,
            folder,
            DownloadQuery::new(vec![]).album(album),
            usable(vec![("a".into(), a)]),
            &StubBackends::default(),
            run,
        )
        .await;
        ledger.settled().await;

        // No tab knows a resumed job's batch, so get_jobs is where the message is read.
        let (state, stored): (String, String) =
            sqlx::query_as("SELECT state, failure FROM jobs WHERE id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        let stored: Failure = serde_json::from_str(&stored).unwrap();
        assert_eq!(state, JobState::Failed.as_str());
        let failed = std::iter::from_fn(|| rx.try_recv().ok())
            .find_map(|e| match e {
                DownloadEvent::AutoDownload(AutoDownloadEvent::Failed { failure, .. }) => {
                    Some(failure)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (failed.code, failed.message.as_str()),
            (
                FailureCode::MetadataNotFound,
                "'Winter Songs' came from a Last.fm search, which Soulbeet no longer uses: search for it again"
            )
        );
        assert_eq!(stored, failed);
    }

    /// A stub's search that finds one group of `peer` on `backend`, scoring `score`.
    fn finds(stub: &StubBackend, backend: &str, peer: &str, score: f64) {
        let items = vec![item_on(backend, peer, "Wake Up", "07.opus")];
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(peer, items, score)],
        ))]);
    }

    /// The results of the run's FallbackToManual event, by source, and its reason; None when
    /// there was none.
    fn offered(
        rx: &mut tokio::sync::broadcast::Receiver<DownloadEvent>,
    ) -> Option<(Vec<String>, ManualReason)> {
        std::iter::from_fn(|| rx.try_recv().ok()).find_map(|e| match e {
            DownloadEvent::AutoDownload(AutoDownloadEvent::FallbackToManual {
                results,
                reason,
                ..
            }) => Some((results.into_iter().map(|g| g.source).collect(), reason)),
            _ => None,
        })
    }

    /// What `pick` says of one slskd group scoring `primary` and one YouTube Music group scoring
    /// `lossy`, for a user who takes lossy files as `fallback`.
    fn picked(primary: f64, lossy: f64, fallback: LossyFallback) -> Result<usize, ManualReason> {
        let groups = vec![
            group(
                "peer_a",
                vec![item_on("slskd", "peer_a", "Wake Up", "07.flac")],
                primary,
            ),
            group(
                "ytm:album",
                vec![item_on("ytdlp", "ytm:album", "Wake Up", "07.opus")],
                lossy,
            ),
        ];
        pick(&groups, fallback)
    }

    #[test]
    fn the_pick_says_lossy_only_when_only_a_lossy_group_is_confident_and_the_user_is_asked() {
        let lossy_only = Err(ManualReason::LossyOnly {
            backend: "ytdlp".into(),
        });
        assert_eq!(picked(0.5, 0.9, LossyFallback::Ask), lossy_only);
        assert_eq!(picked(0.5, 0.7, LossyFallback::Ask), lossy_only);
        assert_eq!(picked(0.5, 0.9, LossyFallback::Auto), Ok(1));
        assert_eq!(picked(0.8, 0.9, LossyFallback::Ask), Ok(0));
        let none = Err(ManualReason::NoConfidentMatch);
        assert_eq!(picked(0.5, 0.69, LossyFallback::Ask), none);
        assert_eq!(picked(0.5, 0.6, LossyFallback::Auto), none);
        assert_eq!(picked(0.5, 0.9, LossyFallback::Never), none);
    }

    fn auto_run(found: Option<Vec<DownloadableGroup>>) -> AutoRun {
        AutoRun {
            batch_id: uuid::Uuid::new_v4().to_string(),
            job: "j1".into(),
            search_id: "s1".into(),
            import_mode: ImportMode::Singleton,
            found,
        }
    }

    /// slskd's stub and YouTube Music's, searched as the tiers they are in, for a user who takes
    /// lossy files as `lossy`.
    fn tiers(
        slskd: &Arc<StubBackend>,
        youtube: &Arc<StubBackend>,
        lossy: LossyFallback,
    ) -> (Usable, StubBackends) {
        let usable = Usable {
            primary: vec![("slskd".into(), slskd.clone() as Arc<dyn DownloadBackend>)],
            last_resort: vec![("ytdlp".into(), youtube.clone() as Arc<dyn DownloadBackend>)],
            lossy,
        };
        let backends = StubBackends::default()
            .with("slskd", slskd.clone())
            .with("ytdlp", youtube.clone());
        (usable, backends)
    }

    // Paused: a tier's search waits SEARCH_POLL_INTERVAL before each poll.
    #[tokio::test(start_paused = true)]
    async fn a_confident_source_is_picked_and_the_lossy_backend_never_searched() {
        let (ledger, _recorded) = Ledger::recording();
        let ctx = test_ctx("tier-confident");
        let dir = tempfile::tempdir().unwrap();
        let slskd = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        finds(&slskd, "slskd", "peer_a", 0.9);
        finds(&youtube, "ytdlp", "ytm:album", 0.95);
        let (usable, backends) = tiers(&slskd, &youtube, LossyFallback::Auto);

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        run_search(
            ledger,
            ctx,
            folder,
            query,
            usable,
            &backends,
            auto_run(None),
        )
        .await;

        assert!(youtube.specs().is_empty(), "YouTube Music was searched");
        assert_eq!(slskd.requests().len(), 1);
        assert!(youtube.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_confident_source_the_lossy_groups_come_after_every_other_one() {
        let (ledger, _recorded) = Ledger::recording();
        let ctx = test_ctx("tier-ask");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let dir = tempfile::tempdir().unwrap();
        let slskd = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        finds(&slskd, "slskd", "peer_a", 0.5);
        // It scores better, and still comes last.
        finds(&youtube, "ytdlp", "ytm:album", 0.95);
        let (usable, backends) = tiers(&slskd, &youtube, LossyFallback::Ask);

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        run_search(
            ledger,
            ctx,
            folder,
            query,
            usable,
            &backends,
            auto_run(None),
        )
        .await;

        assert_eq!(youtube.specs().len(), 1);
        assert_eq!(
            offered(&mut rx),
            Some((
                vec!["peer_a".to_string(), "ytm:album".to_string()],
                ManualReason::LossyOnly {
                    backend: "ytdlp".into()
                }
            ))
        );
        assert!(slskd.requests().is_empty() && youtube.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_who_takes_lossy_files_automatically_gets_one_that_never_replaces_a_copy() {
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = test_ctx("tier-auto");
        let dir = tempfile::tempdir().unwrap();
        let slskd = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        finds(&slskd, "slskd", "peer_a", 0.5);
        finds(&youtube, "ytdlp", "ytm:album", 0.8);
        let (usable, backends) = tiers(&slskd, &youtube, LossyFallback::Auto);

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        let user = ctx.id();
        run_search(
            ledger,
            ctx,
            folder,
            query,
            usable,
            &backends,
            auto_run(None),
        )
        .await;

        assert_eq!(youtube.requests().len(), 1);
        assert!(slskd.requests().is_empty());
        let watched: Vec<Option<String>> = (recorded.events().into_iter())
            .filter_map(|e| match e {
                JobEvent::Slots {
                    state: JobState::Downloading,
                    rows,
                } => Some(rows),
                _ => None,
            })
            .flatten()
            .map(|row| row.backend)
            .collect();
        assert_eq!(watched, vec![Some("ytdlp".to_string())]);
        // Its batch skips what the library has, so beets never removes a copy for it.
        let youtube: Arc<dyn DownloadBackend> = youtube;
        let key = crate::server_fns::download::test_support::key(&youtube, "ytm:album", "07.opus");
        let probe = crate::transfer_registry::BatchRef::new(
            crate::transfer_registry::TransferOwner {
                user_id: user,
                batch_id: "probe".into(),
            },
            "probe".into(),
            dir.path().to_path_buf(),
            ImportMode::Singleton,
            "ytdlp".into(),
        );
        let claim = crate::transfer_registry::TransferRegistry::global().claim(&[key], &probe);
        assert_eq!(claim.mine.len(), 1);
        assert_eq!(claim.mine[0].1.duplicates, DuplicateRule::Skip);
    }

    #[tokio::test(start_paused = true)]
    async fn with_slskd_down_a_user_who_is_asked_gets_the_lossy_groups_to_pick_from() {
        let (ledger, _recorded) = Ledger::recording();
        let ctx = test_ctx("tier-alone");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let dir = tempfile::tempdir().unwrap();
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        finds(&youtube, "ytdlp", "ytm:album", 0.9);
        let usable = Usable {
            primary: vec![],
            last_resort: vec![("ytdlp".into(), youtube.clone() as Arc<dyn DownloadBackend>)],
            lossy: LossyFallback::Ask,
        };
        let backends = StubBackends::default().with("ytdlp", youtube.clone());

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        run_search(
            ledger,
            ctx,
            folder,
            query,
            usable,
            &backends,
            auto_run(None),
        )
        .await;

        assert_eq!(
            offered(&mut rx),
            Some((
                vec!["ytm:album".to_string()],
                ManualReason::LossyOnly {
                    backend: "ytdlp".into()
                }
            ))
        );
        assert!(youtube.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_who_is_asked_gets_no_confident_match_when_the_lossy_group_scores_under_the_floor(
    ) {
        let (ledger, _recorded) = Ledger::recording();
        let ctx = test_ctx("tier-ask-weak");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let dir = tempfile::tempdir().unwrap();
        let slskd = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        finds(&slskd, "slskd", "peer_a", 0.5);
        finds(&youtube, "ytdlp", "ytm:album", 0.6);
        let (usable, backends) = tiers(&slskd, &youtube, LossyFallback::Ask);

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        run_search(
            ledger,
            ctx,
            folder,
            query,
            usable,
            &backends,
            auto_run(None),
        )
        .await;

        assert_eq!(youtube.specs().len(), 1);
        assert_eq!(
            offered(&mut rx),
            Some((
                vec!["peer_a".to_string(), "ytm:album".to_string()],
                ManualReason::NoConfidentMatch
            ))
        );
    }

    #[tokio::test]
    async fn stored_lossy_candidates_are_left_out_for_a_user_who_never_takes_them() {
        let (ledger, _recorded) = Ledger::recording();
        let ctx = test_ctx("tier-never");
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let dir = tempfile::tempdir().unwrap();
        let slskd = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let youtube = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let stored = vec![
            group(
                "peer_a",
                vec![item_on("slskd", "peer_a", "Wake Up", "07.flac")],
                0.4,
            ),
            group(
                "ytm:album",
                vec![item_on("ytdlp", "ytm:album", "Wake Up", "07.opus")],
                0.9,
            ),
        ];
        let (usable, backends) = tiers(&slskd, &youtube, LossyFallback::Never);
        let usable = Usable {
            last_resort: vec![],
            ..usable
        };

        let folder = folder_at(&dir, &ctx);
        let query = DownloadQuery::new(vec![]);
        let run = auto_run(Some(stored));
        run_search(ledger, ctx, folder, query, usable, &backends, run).await;

        assert_eq!(
            offered(&mut rx),
            Some((vec!["peer_a".to_string()], ManualReason::NoConfidentMatch))
        );
        assert!(youtube.requests().is_empty());
    }
}
