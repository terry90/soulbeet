//! Getting files: batches, enqueueing a request's files and watching them,
//! the manual download, the auto-download pipeline and cancelling a download.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::metadata::{hydrate_album_tracks, lastfm_key};
use crate::core::sources::{self, Searched};
use crate::core::{backend_unavailable, database_failure, target_unwritable};
use crate::globals::{get_or_create_user_channel, register_user_task, unregister_user_task};
use crate::ledger::store::StoredSearch;
use crate::ledger::{JobEvent, JobState, Ledger, NewJob, NewSearch, LEDGER};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::{
    another_folder, deliver, enqueue_request, failed_reason, not_enqueued_failure,
    nothing_enqueued, stopped_rows, transfer_key, RequestEnqueue, Watcher,
};
use crate::server_fns::download::failover::{align_pool_tracks, item_index, SourcePool};
use crate::server_fns::download::monitor::{DownloadMonitor, WatchedTransfer};
use crate::services::{download_backend, BackendLookupError};
use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner, TransferRegistry};
use dioxus::logger::tracing::{info, warn};
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, DownloadQuery, DownloadableGroup,
    DownloadableItem, EnqueueStatus, JobSlot, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::SoulseekError;
use soulbeet::{DownloadBackend, ImportMode};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

/// Name for a manual download's batch: the album its items share, or the
/// first item's title when they span albums.
pub(crate) fn manual_batch_label(items: &[DownloadableItem]) -> String {
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

/// The backend call that enqueues a request's files failed.
fn queue_failed(e: &SoulseekError) -> Failure {
    e.failure(Stage::Enqueue, format!("Download queue failed: {e}"))
}

/// The task that ran a request's enqueue and watch ended without an answer.
fn request_stopped(e: &tokio::task::JoinError) -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        format!("Download request stopped: {e}"),
    )
}

/// The batch a request starts when it joins no running one. It downloads
/// into `batch_folder(label, batch_id)` and imports into `target` in
/// `import_mode`; the request's user owns it.
pub struct NewBatch {
    pub batch_id: String,
    pub label: String,
    pub target: PathBuf,
    pub import_mode: ImportMode,
}

impl NewBatch {
    fn owned_by(self, ctx: &UserCtx) -> BatchRef {
        BatchRef::new(
            TransferOwner {
                user_id: ctx.id(),
                batch_id: self.batch_id,
            },
            self.label,
            self.target,
            self.import_mode,
        )
    }
}

/// What one job asks the backend for: its files by slot, and other sources
/// for them.
pub struct Acquisition {
    pub job: String,
    pub items: Vec<(u32, DownloadableItem)>,
    pub alternates: Vec<DownloadableGroup>,
}

/// `items` as the slots 0..n of a job.
fn numbered(items: Vec<DownloadableItem>) -> Vec<(u32, DownloadableItem)> {
    (0..).zip(items).collect()
}

/// Creates `folder`'s directory, and returns its path as the import target.
async fn prepare_target(folder: &Folder) -> Result<PathBuf, Failure> {
    let target = PathBuf::from(&folder.path);
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|e| target_unwritable(&e, format!("Failed to create target directory: {e}")))?;
    Ok(target)
}

/// Claims and enqueues the files of `acquisition`'s job for `ctx`, shows
/// their rows on its user's channel, and hands the enqueued ones to a
/// monitor: the running batch's when the request joined one of the user's,
/// else one of `fresh`. Either fails a slot over to its track's alternates.
///
/// Every row it sends carries its slot and is recorded in `ledger`: failed
/// files Failed, queued ones Downloading. A file one of the user's batches
/// already downloads into this folder dismisses its slot, as that batch
/// pursues it. A slot the request got no answer for is recorded Failed but
/// not sent, the request's own answer showing why: the same user downloads
/// it into another folder, the backend call failed, or the backend answered
/// nothing for it.
///
/// Runs as its own task. Registry keys are claimed from its start, and the
/// client may disconnect at any await and drop the caller: dropping the
/// task's handle does not stop it, so the request always gets as far as a
/// monitor or a release.
pub(crate) async fn enqueue_and_watch(
    ledger: &Ledger,
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    fresh: NewBatch,
    acquisition: Acquisition,
) -> Result<Vec<QueuedDownload>, Failure> {
    tokio::spawn(queue_and_watch(
        ledger.clone(),
        ctx.clone(),
        backend,
        fresh,
        acquisition,
    ))
    .await
    .map_err(|e| request_stopped(&e))?
}

/// `enqueue_and_watch`'s work, in the task it runs as.
async fn queue_and_watch(
    ledger: Ledger,
    ctx: UserCtx,
    backend: Arc<dyn DownloadBackend>,
    fresh: NewBatch,
    acquisition: Acquisition,
) -> Result<Vec<QueuedDownload>, Failure> {
    let Acquisition {
        job,
        items: picked,
        alternates,
    } = acquisition;
    let items: Vec<DownloadableItem> = picked.iter().map(|(_, item)| item.clone()).collect();
    let enq = enqueue_request(&backend, items.clone(), fresh.owned_by(&ctx)).await;
    let refusal = enq.refusal();
    let RequestEnqueue {
        batch,
        results,
        watcher,
        error,
        elsewhere,
    } = enq;

    // The answer for each slot, by its index in `picked`.
    let mut answers: Vec<Option<&QueuedDownload>> = vec![None; picked.len()];
    for q in &results {
        match item_index(q, &items) {
            Some(at) => answers[at] = Some(q),
            None => warn!(
                "{} answered for {} from {}, which the request did not ask for",
                backend.name(),
                q.item,
                q.source
            ),
        }
    }
    let slot = |n: u32| JobSlot {
        job: job.clone(),
        slot: n,
    };
    let (enqueued_slots, enqueued): (Vec<u32>, Vec<QueuedDownload>) = picked
        .iter()
        .zip(&answers)
        .filter_map(|((n, _), answer)| {
            answer
                .filter(|q| q.status == EnqueueStatus::Enqueued)
                .map(|q| (*n, q.clone()))
        })
        .unzip();
    // Before any row, so a slot recorded downloading always has its batch.
    if !enqueued.is_empty() {
        ledger.record(JobEvent::Enqueued {
            job: job.clone(),
            monitor_batch_id: batch.owner.batch_id.clone(),
            batch_label: batch.label.clone(),
        });
    }

    // A request only ever joins a batch of its own user, so every row of it
    // goes to that user's channel.
    let user = ctx.id();
    let tx = get_or_create_user_channel(&user).await;
    let row = |p: DownloadProgress, n: u32| {
        p.with_batch(batch.owner.batch_id.clone(), batch.label.clone())
            .with_backend(backend.id())
            .with_job(slot(n))
    };

    let mut failed = Vec::new();
    let mut unanswered = Vec::new();
    let mut queued = Vec::new();
    let mut dismissed = Vec::new();
    for ((n, item), answer) in picked.iter().zip(answers) {
        let Some(q) = answer else {
            let failure = if elsewhere.contains(&transfer_key(backend.id(), &item.source, &item.id))
            {
                another_folder(RetryAdvice::OtherSource)
            } else if let Some(e) = &error {
                queue_failed(e)
            } else {
                nothing_enqueued()
            };
            let p = DownloadProgress::failed(
                item.id.clone(),
                item.source.clone(),
                item.id.clone(),
                failure,
            );
            unanswered.push(row(p, *n));
            continue;
        };
        match failed_reason(&q.status) {
            Some(reason) => failed.push(row(
                DownloadProgress::failed(q.id.clone(), q.source.clone(), q.item.clone(), reason),
                *n,
            )),
            None if q.status == EnqueueStatus::Enqueued => queued.push(row(
                DownloadProgress::queued(q.id.clone(), q.source.clone(), q.item.clone(), q.size),
                *n,
            )),
            None => dismissed.push(slot(*n)),
        }
    }

    if !failed.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(failed.clone()));
    }
    failed.extend(unanswered);
    if !failed.is_empty() {
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: failed,
        });
    }
    // Send initial "Queued" state immediately so UI shows the downloads right away
    if !queued.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(queued.clone()));
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: queued,
        });
    }
    if !dismissed.is_empty() {
        ledger.record(JobEvent::Dismiss(dismissed));
    }

    match watcher {
        Watcher::Joined(reservation) => {
            info!(
                "Added {} downloads to running batch {}",
                enqueued.len(),
                batch.owner.batch_id
            );
            let lost = deliver(
                reservation,
                &enqueued,
                &picked,
                &alternates,
                &backend,
                &batch,
                &job,
            )
            .await;
            if !lost.is_empty() {
                let rows = stopped_rows(&lost, &batch);
                let _ = tx.send(DownloadEvent::Progress(rows.clone()));
                ledger.record(JobEvent::Slots {
                    state: JobState::Failed,
                    rows,
                });
            }
        }
        Watcher::Own if enqueued.is_empty() && batch.inbox.close_if_idle() => {}
        // Starts empty when nothing was enqueued but another request joined
        // the batch while this one was enqueueing.
        Watcher::Own => {
            info!(
                "Started monitoring downloads: {:?}",
                enqueued.iter().map(|d| &d.item).collect::<Vec<_>>()
            );
            let transfers = enqueued
                .iter()
                .zip(enqueued_slots)
                .map(|(q, n)| WatchedTransfer::enqueued(q, slot(n)))
                .collect();
            // Pool slots must line up with the monitor's slots, which follow
            // `enqueued`, not `items`.
            let pool = SourcePool::from_tracks(&align_pool_tracks(&enqueued, &items), &alternates);
            let target = batch.target.clone();
            spawn_monitor(
                &ctx,
                DownloadMonitor::new(transfers, target, tx, batch, ledger).with_failover(pool),
            );
        }
    }

    if let Some(e) = error {
        return Err(queue_failed(&e));
    }
    if let Some(failure) = refusal {
        return Err(failure);
    }
    Ok(results)
}

/// Registers `ctx`'s user task and runs `monitor` in it.
pub(crate) fn spawn_monitor(ctx: &UserCtx, mut monitor: DownloadMonitor) {
    let user = ctx.id();
    tokio::spawn(async move {
        register_user_task(&user).await;
        monitor.run().await;
        unregister_user_task(&user).await;
    });
}

/// The search's groups without the picked files, best score first, empty
/// groups dropped.
pub(crate) fn alternates_from(
    groups: &[DownloadableGroup],
    picked: &[DownloadableItem],
) -> Vec<DownloadableGroup> {
    let mut left: Vec<DownloadableGroup> = groups
        .iter()
        .map(|group| DownloadableGroup {
            items: group
                .items
                .iter()
                .filter(|item| {
                    !picked
                        .iter()
                        .any(|p| p.id == item.id && p.source == item.source)
                })
                .cloned()
                .collect(),
            ..group.clone()
        })
        .filter(|group| !group.items.is_empty())
        .collect();
    left.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    left
}

/// A hand-picked download of `items` into `folder` from `ctx`'s search
/// `search_id`, from the named backend or the default one. It continues the
/// job waiting for a source from that search, else starts one, and the
/// search's other candidates stand by as alternates.
pub async fn download(
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    backend: Option<&str>,
    search_id: &str,
) -> Result<Vec<QueuedDownload>, Failure> {
    download_with(
        &LEDGER,
        ctx,
        folder,
        items,
        crate::importer_health::blocking_failure(),
        sources::owned_search(&crate::db::DB, ctx, search_id),
        download_backend(backend),
    )
    .await
}

/// `download`, given the ledger, the importer's gate, the search lookup and
/// the backend lookup. Each is awaited only once the steps before it passed:
/// the gate, then the search, then the target folder, then the backend.
async fn download_with(
    ledger: &Ledger,
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    gate: impl Future<Output = Option<Failure>>,
    search: impl Future<Output = Result<StoredSearch, Failure>>,
    backend: impl Future<Output = Result<Arc<dyn DownloadBackend>, BackendLookupError>>,
) -> Result<Vec<QueuedDownload>, Failure> {
    if let Some(failure) = gate.await {
        return Err(failure);
    }
    let search = search.await?;
    let target = prepare_target(folder).await?;
    let backend = backend
        .await
        .map_err(|e| backend_unavailable(Stage::Enqueue, &e))?;

    let claimed = ledger
        .claim_source_choice(&ctx.id(), &search.id, &folder.id)
        .await
        .map_err(|e| database_failure(Stage::Enqueue, e))?;
    let (job, import_mode) = match claimed {
        Some(claimed) => (claimed.id, claimed.import_mode),
        None => {
            let (id, import_mode) = (uuid::Uuid::new_v4().to_string(), CONFIG.import_mode());
            ledger
                .create(
                    NewJob {
                        id: id.clone(),
                        user: ctx.id(),
                        folder_id: folder.id.clone(),
                        wanted: search.query,
                        import_mode,
                        state: JobState::Queued,
                        search_id: Some(search.id),
                    },
                    None,
                )
                .await
                .map_err(|e| database_failure(Stage::Enqueue, e))?;
            (id, import_mode)
        }
    };

    let alternates = alternates_from(search.candidates.as_deref().unwrap_or_default(), &items);
    let fresh = NewBatch {
        batch_id: uuid::Uuid::new_v4().to_string(),
        label: manual_batch_label(&items),
        target,
        import_mode,
    };
    let items = numbered(items);
    ledger.record(JobEvent::Picked {
        job: job.clone(),
        items: items.clone(),
    });
    let acquisition = Acquisition {
        job,
        items,
        alternates,
    };
    enqueue_and_watch(ledger, ctx, backend, fresh, acquisition).await
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
    LEDGER
        .create(
            NewJob {
                id: run.job.clone(),
                user: ctx.id(),
                folder_id: folder.id.clone(),
                wanted: query.clone(),
                import_mode: run.import_mode,
                state: JobState::Searching,
                search_id: Some(run.search_id.clone()),
            },
            Some(NewSearch {
                id: run.search_id.clone(),
                user: ctx.id(),
                backend: None,
                query: query.clone(),
            }),
        )
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    let batch_id = run.batch_id.clone();

    // Spawn the entire search-score-pick-download pipeline onto a background task.
    // This avoids blocking the HTTP response during the search-poll loop (Research Pitfall 4).
    tokio::spawn(run_search(
        LEDGER.clone(),
        ctx.clone(),
        folder,
        query,
        key,
        backends,
        run,
    ));

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

    send(AutoDownloadEvent::Searching {
        batch_id: run.batch_id.clone(),
        query: query_desc.clone(),
        backend_count: 1 + rest.len(),
    });

    let mut all_groups = match run.found.take() {
        Some(groups) => groups,
        None => {
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
        send(AutoDownloadEvent::FallbackToManual {
            batch_id: run.batch_id.clone(),
            search_id: run.search_id.clone(),
            results: all_groups,
            best_score,
            threshold: AUTO_SELECT_SCORE_THRESHOLD,
        });
        ledger.record(JobEvent::NeedsSourceChoice {
            job: run.job.clone(),
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
    });
}

/// No transfer of the user's has that id on that peer.
fn download_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Download not found",
    )
}

/// The registry key of the transfer the backend lists with exactly this peer
/// and id, built from the backend's own filename. `None` when it lists no
/// such transfer.
fn cancel_key(
    listed: &[DownloadProgress],
    backend: &'static str,
    source: &str,
    id: &str,
) -> Option<TransferKey> {
    listed
        .iter()
        .find(|row| row.source == source && row.id == id)
        .map(|row| transfer_key(backend, &row.source, &row.item))
}

/// Cancels the transfer the named backend, or the default one, lists under
/// `transfer_id` from `source`, when one of `ctx`'s batches holds it. Its
/// watcher stops it on its next poll: a monitor shows it Cancelled, and a
/// discovery run moves the track on.
pub async fn cancel(
    ctx: &UserCtx,
    backend: Option<&str>,
    source: &str,
    transfer_id: &str,
) -> Result<(), Failure> {
    let backend = download_backend(backend)
        .await
        .map_err(|e| backend_unavailable(Stage::Transfer, &e))?;
    cancel_on(ctx, &backend, source, transfer_id).await
}

/// `cancel` on `backend`.
async fn cancel_on(
    ctx: &UserCtx,
    backend: &Arc<dyn DownloadBackend>,
    source: &str,
    transfer_id: &str,
) -> Result<(), Failure> {
    // The key comes from the transfer the backend lists under that id, never
    // from text the client sends, so a user cannot pair their own file with
    // another user's transfer id on the same peer.
    let listed = backend
        .get_downloads_for(&[source.to_string()])
        .await
        .map_err(|e| e.failure(Stage::Transfer, e.to_string()))?;
    let user = ctx.id();
    let marked = cancel_key(&listed, backend.id(), source, transfer_id)
        .is_some_and(|key| TransferRegistry::global().request_cancel(&key, &user));
    if !marked {
        return Err(download_not_found());
    }
    info!("User {user} cancelled download {transfer_id} from {source}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::core::test_ctx;
    use crate::server_fns::download::enqueue::other_user;
    use crate::server_fns::download::test_support::{
        batch, group, item, key, search_result, transfer, ScriptedOutcome, StubBackend,
    };
    use shared::download::SearchState;
    use shared::failure::{Cause, Service};

    fn ctx_of(batch: &BatchRef) -> UserCtx {
        test_ctx(&batch.owner.user_id.to_string())
    }

    /// A user of its own, so tests sharing the process-wide registry never
    /// see each other's claims.
    fn own_ctx() -> UserCtx {
        test_ctx(&format!("tester-{}", uuid::Uuid::new_v4()))
    }

    /// A new batch importing into `target` in `running`'s import mode.
    fn new_batch_beside(running: &BatchRef, target: PathBuf) -> NewBatch {
        NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: "Come Over".to_string(),
            target,
            import_mode: running.import_mode,
        }
    }

    /// Job `j1` asking for `items` as slots 0..n, with `alternates`.
    fn acquisition(
        items: Vec<DownloadableItem>,
        alternates: Vec<DownloadableGroup>,
    ) -> Acquisition {
        Acquisition {
            job: "j1".into(),
            items: numbered(items),
            alternates,
        }
    }

    fn new_batch(label: &str) -> NewBatch {
        NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: label.to_string(),
            target: PathBuf::from("/music"),
            import_mode: ImportMode::Singleton,
        }
    }

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

    #[tokio::test]
    async fn a_request_dropped_mid_enqueue_still_hands_its_transfers_over() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "p", "1.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        let items = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];

        // The handler is dropped while slskd is still answering the enqueue,
        // as when the client disconnects.
        let ctx = ctx_of(&running);
        let handler = tokio::spawn(async move {
            enqueue_and_watch(
                &Ledger::recording().0,
                &ctx,
                backend,
                fresh,
                acquisition(items, Vec::new()),
            )
            .await
        });
        reached.notified().await;
        handler.abort();
        assert!(handler.await.unwrap_err().is_cancelled());
        release.notify_one();

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let slots = running.inbox.take();
                if !slots.is_empty() {
                    break slots;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the enqueued transfer never reached the running batch");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].filename, "2.flac");
        assert_eq!(delivered[0].transfer_id, "t-2.flac");
    }

    #[tokio::test]
    async fn a_request_that_joins_a_running_batch_hands_over_its_alternates() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Two", "b/02.flac")],
            0.8,
        )];

        enqueue_and_watch(
            &Ledger::recording().0,
            &ctx_of(&running),
            backend,
            fresh,
            acquisition(
                vec![
                    item("peer_a", "One", "01.flac"),
                    item("peer_a", "Two", "02.flac"),
                ],
                rest,
            ),
        )
        .await
        .unwrap();

        let slots = running.inbox.take();
        assert_eq!(slots.len(), 1);
        assert_eq!(
            (slots[0].filename.as_str(), slots[0].alternates.clone()),
            ("02.flac", vec![item("peer_b", "Two", "b/02.flac")])
        );
    }

    #[tokio::test]
    async fn a_request_only_for_files_downloading_elsewhere_fails_as_another_folder() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, PathBuf::from("/other-library"));
        let got = enqueue_and_watch(
            &Ledger::recording().0,
            &ctx_of(&running),
            backend,
            fresh,
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await;
        assert_eq!(got.unwrap_err(), another_folder(RetryAdvice::NotRetryable));
        assert!(stub.destinations().is_empty());
    }

    #[tokio::test]
    async fn a_failed_backend_call_is_the_requests_failure() {
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        let f = enqueue_and_watch(
            &Ledger::recording().0,
            &own_ctx(),
            backend,
            new_batch("X"),
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await
        .unwrap_err();
        let e = SoulseekError::InvalidResponse(
            "batch enqueue response: expected value at line 1 column 1 (body 20 bytes)".into(),
        );
        assert_eq!(
            (f.code, f.stage, f.message),
            (
                FailureCode::BackendResponseUnreadable,
                Stage::Enqueue,
                format!("Download queue failed: {e}")
            )
        );
    }

    #[tokio::test]
    async fn failed_rows_reach_the_users_channel_stamped_with_batch_and_backend() {
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        let ctx = own_ctx();
        let fresh = new_batch("Come Over");
        let batch_id = fresh.batch_id.clone();
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let results = enqueue_and_watch(
            &Ledger::recording().0,
            &ctx,
            backend.clone(),
            fresh,
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await
        .unwrap();
        assert!(matches!(results[0].status, EnqueueStatus::Failed(_)));
        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("no row sent")
        };
        assert_eq!(
            (rows[0].batch_id.as_deref(), rows[0].backend.as_deref()),
            (Some(batch_id.as_str()), Some(backend.id()))
        );
        assert_eq!(
            rows[0].failure.as_ref().map(|f| f.code),
            Some(FailureCode::PeerOffline)
        );
    }

    fn listed() -> Vec<DownloadProgress> {
        vec![transfer(
            "peer_a",
            "Music\\A\\01.flac",
            "t-1",
            shared::download::DownloadState::Queued,
        )]
    }

    #[test]
    fn cancel_uses_the_listed_filename_not_the_clients_text() {
        assert_eq!(
            cancel_key(&listed(), "slskd", "peer_a", "t-1"),
            Some(transfer_key("slskd", "peer_a", "Music\\A\\01.flac"))
        );
    }

    #[test]
    fn cancel_needs_a_listed_transfer_with_that_id_and_peer() {
        assert_eq!(cancel_key(&listed(), "slskd", "peer_a", "t-2"), None);
        assert_eq!(cancel_key(&listed(), "slskd", "peer_b", "t-1"), None);
    }

    #[tokio::test]
    async fn another_users_transfer_id_cannot_be_cancelled() {
        let stub = StubBackend::new(vec![]);
        stub.set_listing(listed());
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let theirs = batch("Come Over");
        let held = transfer_key(backend.id(), "peer_a", "Music\\A\\01.flac");
        let registry = TransferRegistry::global();
        registry.claim(std::slice::from_ref(&held), &theirs);

        assert_eq!(
            cancel_on(&own_ctx(), &backend, "peer_a", "t-1").await,
            Err(download_not_found())
        );
        assert!(!registry.is_cancelled(&held, &theirs.owner));

        assert_eq!(
            cancel_on(&ctx_of(&theirs), &backend, "peer_a", "t-1").await,
            Ok(())
        );
        assert!(registry.is_cancelled(&held, &theirs.owner));
        assert!(stub.cancelled().is_empty(), "its watcher stops it");
    }

    /// A lookup the code under test must never reach.
    async fn unreached_backend() -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
        panic!("the backend was looked up")
    }

    fn folder_at(path: &std::path::Path) -> Folder {
        Folder {
            id: "f1".into(),
            user_id: "user-a".into(),
            name: "Music".into(),
            path: path.to_string_lossy().into_owned(),
            shared: false,
        }
    }

    #[tokio::test]
    async fn a_download_checks_the_importer_then_the_search_then_the_folder_then_the_backend() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("music");
        std::fs::write(&file, b"").unwrap();
        let unwritable = folder_at(&file.join("Come Over"));
        let items = || vec![item("peer_a", "One", "01.flac")];
        let importer = Failure::new(
            FailureCode::ImporterBroken,
            Stage::Import,
            RetryAdvice::NeedsUser,
            "No metadata source plugin loaded",
        );
        let stored = || StoredSearch {
            id: "s1".into(),
            backend: Some("slskd".into()),
            query: DownloadQuery::new(vec![]),
            candidates: None,
            expires_at: i64::MAX,
        };
        let ledger = Ledger::recording().0;

        let gated = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(Some(importer.clone())),
            std::future::ready(Err(sources::search_not_found())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(gated, importer);

        let no_search = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(None),
            std::future::ready(Err(sources::search_not_found())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(no_search, sources::search_not_found());

        let unwritable_target = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(None),
            std::future::ready(Ok(stored())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(unwritable_target.code, FailureCode::ImportTargetUnwritable);

        let unknown = BackendLookupError::Unknown("nicotine".into());
        let no_backend = download_with(
            &ledger,
            &own_ctx(),
            &folder_at(&dir.path().join("library")),
            items(),
            std::future::ready(None),
            std::future::ready(Ok(stored())),
            std::future::ready(Err(BackendLookupError::Unknown("nicotine".into()))),
        )
        .await
        .unwrap_err();
        assert_eq!(no_backend, backend_unavailable(Stage::Enqueue, &unknown));
    }

    #[test]
    fn a_hand_picked_download_keeps_the_other_sources_of_its_search() {
        let groups = vec![
            group(
                "peer_a",
                vec![
                    item("peer_a", "One", "a/01.flac"),
                    item("peer_a", "Two", "a/02.flac"),
                ],
                0.6,
            ),
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "Two", "c/02.flac")], 0.4),
        ];
        let picked = vec![
            item("peer_a", "One", "a/01.flac"),
            item("peer_a", "Two", "a/02.flac"),
            item("peer_c", "Two", "c/02.flac"),
        ];
        let left = alternates_from(&groups, &picked);
        assert_eq!(
            left.iter()
                .map(|g| (g.source.as_str(), g.items.len()))
                .collect::<Vec<_>>(),
            vec![("peer_b", 1)]
        );
    }

    /// Each row's slot and failure.
    type SlotRows = Vec<(Option<JobSlot>, Option<Failure>)>;

    /// The Slots events of `events`: their state, and each row's slot and failure.
    fn slot_events(events: Vec<JobEvent>) -> Vec<(JobState, SlotRows)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots { state, rows } => Some((
                    state,
                    rows.into_iter().map(|r| (r.job, r.failure)).collect(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_file_of_a_request_is_recorded_on_its_slot() {
        let (ledger, mut recorded) = Ledger::recording();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: vec![],
            },
        )
        .await
        .unwrap();
        let failed: Vec<(JobState, Vec<Option<FailureCode>>)> = slot_events(recorded.events())
            .into_iter()
            .map(|(state, rows)| {
                (
                    state,
                    rows.into_iter().map(|(_, f)| f.map(|f| f.code)).collect(),
                )
            })
            .collect();
        assert_eq!(
            failed,
            vec![(JobState::Failed, vec![Some(FailureCode::PeerOffline)])]
        );
    }

    #[tokio::test]
    async fn files_a_request_got_no_answer_for_fail_their_slots() {
        // The backend call fails: both slots fail with the request's own failure.
        let (ledger, mut recorded) = Ledger::recording();
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        let returned = enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items: vec![
                    (0, item("peer_a", "One", "01.flac")),
                    (1, item("peer_a", "Two", "02.flac")),
                ],
                alternates: vec![],
            },
        )
        .await
        .unwrap_err();
        let slot = |n| {
            Some(JobSlot {
                job: "j1".into(),
                slot: n,
            })
        };
        assert_eq!(
            slot_events(recorded.events()),
            vec![(
                JobState::Failed,
                vec![(slot(0), Some(returned.clone())), (slot(1), Some(returned))]
            )]
        );

        // The same user downloads the only file into another folder.
        let (ledger, mut recorded) = Ledger::recording();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, PathBuf::from("/elsewhere"));
        let refused = enqueue_and_watch(
            &ledger,
            &ctx_of(&running),
            backend,
            fresh,
            Acquisition {
                job: "j2".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: vec![],
            },
        )
        .await
        .unwrap_err();
        assert_eq!(refused, another_folder(RetryAdvice::NotRetryable));
        assert_eq!(
            slot_events(recorded.events()),
            vec![(
                JobState::Failed,
                vec![(
                    Some(JobSlot {
                        job: "j2".into(),
                        slot: 0
                    }),
                    Some(another_folder(RetryAdvice::OtherSource))
                )]
            )]
        );
        assert!(stub.destinations().is_empty());
    }

    #[tokio::test]
    async fn a_request_whose_enqueue_fails_ends_its_job() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                },
                None,
            )
            .await
            .unwrap();
        let items = vec![
            (0, item("peer_a", "One", "01.flac")),
            (1, item("peer_a", "Two", "02.flac")),
        ];
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: items.clone(),
        });
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items,
                alternates: vec![],
            },
        )
        .await
        .unwrap_err();
        ledger.settled().await;
        // Terminal, so Task 12's `unfinished` never returns it.
        assert_eq!(
            crate::ledger::store::job_state(&db, "j1").await,
            Some(JobState::Failed)
        );
    }

    #[tokio::test]
    async fn a_file_another_batch_already_downloads_dismisses_its_slot_and_names_that_batch() {
        let (ledger, mut recorded) = Ledger::recording();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        enqueue_and_watch(
            &ledger,
            &ctx_of(&running),
            backend,
            fresh,
            Acquisition {
                job: "j2".into(),
                items: vec![
                    (0, item("peer_a", "One", "01.flac")),
                    (1, item("peer_a", "Two", "02.flac")),
                ],
                alternates: vec![],
            },
        )
        .await
        .unwrap();
        let events = recorded.events();
        assert!(events.contains(&JobEvent::Dismiss(vec![JobSlot {
            job: "j2".into(),
            slot: 0
        }])));
        assert!(events.contains(&JobEvent::Enqueued {
            job: "j2".into(),
            monitor_batch_id: running.owner.batch_id.clone(),
            batch_label: "Come Over".into()
        }));
        assert_eq!(
            running
                .inbox
                .take()
                .iter()
                .map(|s| s.job.clone())
                .collect::<Vec<_>>(),
            vec![JobSlot {
                job: "j2".into(),
                slot: 1
            }]
        );
    }

    /// `alice`'s folder `folder_id` at a new temporary directory.
    fn alices_folder(folder_id: String, dir: &tempfile::TempDir) -> Folder {
        Folder {
            id: folder_id,
            user_id: "alice".into(),
            name: "f".into(),
            path: dir.path().to_string_lossy().into_owned(),
            shared: false,
        }
    }

    #[tokio::test]
    async fn a_pick_from_a_search_that_is_gone_is_refused_before_anything_downloads() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let dir = tempfile::tempdir().unwrap();
        let folder = alices_folder(folder_id, &dir);
        let ledger = Ledger::start(db.clone());
        let f = download_with(
            &ledger,
            &test_ctx("alice"),
            &folder,
            vec![item("peer_a", "One", "01.flac")],
            std::future::ready(None),
            sources::owned_search(&db, &test_ctx("alice"), "no-such-search"),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(f, sources::search_not_found());
    }

    #[tokio::test]
    async fn a_pick_from_the_fallback_continues_its_job_with_the_searchs_other_sources() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let dir = tempfile::tempdir().unwrap();
        let folder = alices_folder(folder_id.clone(), &dir);
        let ledger = Ledger::start(db.clone());
        let candidates = vec![
            group(
                "peer_a",
                vec![
                    item("peer_a", "One", "01.flac"),
                    item("peer_a", "Two", "02.flac"),
                ],
                0.5,
            ),
            group("peer_b", vec![item("peer_b", "Two", "b/02.flac")], 0.4),
        ];
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::NeedsSourceChoice,
                    search_id: Some("s1".into()),
                },
                Some(NewSearch {
                    id: "s1".into(),
                    user: UserId::new("alice"),
                    backend: None,
                    query: DownloadQuery::new(vec![]),
                }),
            )
            .await
            .unwrap();
        ledger.store_candidates("s1", candidates).await.unwrap();
        // Alice already downloads One into this folder, so Two joins that batch.
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = BatchRef::new(
            TransferOwner {
                user_id: UserId::new("alice"),
                batch_id: uuid::Uuid::new_v4().to_string(),
            },
            "Come Over".into(),
            dir.path().to_path_buf(),
            ImportMode::Singleton,
        );
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);

        let ctx = test_ctx("alice");
        download_with(
            &ledger,
            &ctx,
            &folder,
            vec![
                item("peer_a", "One", "01.flac"),
                item("peer_a", "Two", "02.flac"),
            ],
            std::future::ready(None),
            sources::owned_search(&db, &ctx, "s1"),
            std::future::ready(Ok(backend)),
        )
        .await
        .unwrap();
        ledger.settled().await;

        assert_eq!(
            crate::ledger::store::job_state(&db, "j1").await,
            Some(JobState::Downloading)
        );
        let joined = running.inbox.take();
        assert_eq!(
            joined
                .iter()
                .map(|s| (s.job.clone(), s.alternates.clone()))
                .collect::<Vec<_>>(),
            vec![(
                JobSlot {
                    job: "j1".into(),
                    slot: 1
                },
                vec![item("peer_b", "Two", "b/02.flac")]
            )]
        );
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
        let offered = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|e| match e {
            DownloadEvent::AutoDownload(AutoDownloadEvent::FallbackToManual {
                results, ..
            }) => Some(results),
            _ => None,
        });
        assert_eq!(offered, Some(stored));
    }

    #[test]
    fn a_transfer_nobody_of_the_user_holds_is_not_found() {
        let f = download_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Download not found"
            )
        );
    }

    #[tokio::test]
    async fn a_folder_that_cannot_be_created_is_an_unwritable_target() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("music");
        std::fs::write(&file, b"").unwrap();
        let folder = Folder {
            id: "f1".into(),
            user_id: "user-a".into(),
            name: "Music".into(),
            path: file.join("Come Over").to_string_lossy().into_owned(),
            shared: false,
        };
        let f = prepare_target(&folder).await.unwrap_err();
        let os = std::io::Error::from_raw_os_error(20); // ENOTDIR: "Not a directory (os error 20)"
        assert_eq!(
            (f.code, f.stage, f.retry, f.message),
            (
                FailureCode::ImportTargetUnwritable,
                Stage::Import,
                RetryAdvice::NotRetryable,
                format!("Failed to create target directory: {os}")
            )
        );
        assert_eq!(
            f.cause,
            Some(Cause::new(Service::Filesystem, None, os.to_string()))
        );
    }
}
