//! Getting files: batches, enqueueing a request's files and watching them,
//! the manual download and the auto-download pipeline.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::metadata::{hydrate_album_tracks, lastfm_key};
use crate::core::{backend_unavailable, sources, target_unwritable};
use crate::globals::{get_or_create_user_channel, register_user_task, unregister_user_task};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::{
    another_folder, deliver, enqueue_request, failed_reason, not_enqueued_failure, stopped_rows,
    RequestEnqueue, Watcher,
};
use crate::server_fns::download::failover::{align_pool_tracks, SourcePool};
use crate::server_fns::download::monitor::{DownloadMonitor, WatchedTransfer};
use crate::services::download_backend;
use crate::transfer_registry::{BatchRef, TransferOwner};
use dioxus::logger::tracing::info;
use shared::download::{
    AutoDownloadEvent, DownloadEvent, DownloadProgress, DownloadQuery, DownloadableGroup,
    DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::SoulseekError;
use soulbeet::DownloadBackend;
use std::path::PathBuf;
use std::sync::Arc;

/// Name for a manual download's batch: the album its items share, or the
/// first item's title when they span albums.
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

/// A new batch of `ctx`'s user, downloading into `batch_folder(label,
/// batch_id)` and importing into `target`.
fn fresh_batch(ctx: &UserCtx, batch_id: String, label: String, target: PathBuf) -> BatchRef {
    BatchRef::new(
        TransferOwner {
            user_id: ctx.id(),
            batch_id,
        },
        label,
        target,
        CONFIG.import_mode(),
    )
}

/// Creates `folder`'s directory, and returns its path as the import target.
async fn prepare_target(folder: &Folder) -> Result<PathBuf, Failure> {
    let target = PathBuf::from(&folder.path);
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|e| target_unwritable(&e, format!("Failed to create target directory: {e}")))?;
    Ok(target)
}

/// Claims and enqueues a request's files, shows their rows, and hands the
/// enqueued ones to a monitor: the running batch's when the request joined
/// one, else a new one. Either fails a slot over to its track's
/// `alternates`.
///
/// Runs as its own task. Registry keys are claimed from its start, and the
/// client may disconnect at any await and drop the caller: dropping the
/// task's handle does not stop it, so the request always gets as far as a
/// monitor or a release.
pub async fn enqueue_and_watch(
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
    alternates: Vec<DownloadableGroup>,
) -> Result<Vec<QueuedDownload>, Failure> {
    tokio::spawn(queue_and_watch(
        ctx.clone(),
        backend,
        items,
        fresh,
        alternates,
    ))
    .await
    .map_err(|e| request_stopped(&e))?
}

/// `enqueue_and_watch`'s work, in the task it runs as.
async fn queue_and_watch(
    ctx: UserCtx,
    backend: Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
    alternates: Vec<DownloadableGroup>,
) -> Result<Vec<QueuedDownload>, Failure> {
    let enq = enqueue_request(&backend, items.clone(), fresh).await;
    let refused = enq.refusal().is_some();
    let RequestEnqueue {
        batch,
        results,
        watcher,
        error,
        ..
    } = enq;

    let user = ctx.id();
    let (tx, _) = get_or_create_user_channel(&user).await;
    let row = |p: DownloadProgress| {
        let mut p = p.with_batch(batch.owner.batch_id.clone(), batch.label.clone());
        p.backend = Some(backend.id().to_string());
        p
    };

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

    let enqueued: Vec<QueuedDownload> = results
        .iter()
        .filter(|d| d.status == EnqueueStatus::Enqueued)
        .cloned()
        .collect();

    // Send initial "Queued" state immediately so UI shows the downloads right away
    let queued_entries: Vec<DownloadProgress> = enqueued
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
                "Added {} downloads to running batch {}",
                enqueued.len(),
                batch.owner.batch_id
            );
            let lost = deliver(
                reservation,
                &enqueued,
                &items,
                &alternates,
                &backend,
                &batch,
            )
            .await;
            if !lost.is_empty() {
                let _ = tx.send(DownloadEvent::Progress(stopped_rows(&lost, &batch)));
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
            let transfers = enqueued.iter().map(WatchedTransfer::from).collect();
            // Pool slots must line up with the monitor's slots, which follow
            // `enqueued`, not `items`.
            let pool = SourcePool::from_tracks(&align_pool_tracks(&enqueued, &items), &alternates);
            let task_cancellation = register_user_task(&user).await;
            tokio::spawn(async move {
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
        }
    }

    if let Some(e) = error {
        return Err(queue_failed(&e));
    }
    if refused {
        return Err(another_folder(RetryAdvice::NotRetryable));
    }
    Ok(results)
}

/// A hand-picked download of `items` into `folder`, from the named backend
/// or the default one. The picks carry no alternates.
pub async fn download(
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    backend: Option<&str>,
) -> Result<Vec<QueuedDownload>, Failure> {
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }
    let target = prepare_target(folder).await?;
    let backend = download_backend(backend)
        .await
        .map_err(|e| backend_unavailable(Stage::Enqueue, &e))?;
    let fresh = fresh_batch(
        ctx,
        uuid::Uuid::new_v4().to_string(),
        manual_batch_label(&items),
        target,
    );
    enqueue_and_watch(ctx, backend, items, fresh, Vec::new()).await
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

/// Searches every backend for `query`, picks the best source when it scores
/// well enough, and downloads it into `folder`, the other sources standing by
/// as alternates. Everything after the backend lookup runs in the background
/// and reports on the user's channel; the batch id is returned at once.
pub async fn auto_download(
    ctx: &UserCtx,
    folder: Folder,
    mut query: DownloadQuery,
) -> Result<String, Failure> {
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }
    let key = lastfm_key(ctx).await?;

    let (tx, _) = get_or_create_user_channel(&ctx.id()).await;
    let send = move |event| {
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

    // Search every configured backend (per D-06).
    let (first, rest) = match sources::usable_backends().await {
        Ok(backends) => backends,
        Err(failure) => {
            send(AutoDownloadEvent::Failed {
                batch_id: uuid::Uuid::new_v4().to_string(),
                failure: failure.clone(),
            });
            return Err(failure);
        }
    };

    // Generate batch_id upfront so we can return it immediately (per D-10)
    let batch_id = uuid::Uuid::new_v4().to_string();
    let batch_id_for_response = batch_id.clone();

    // Spawn the entire search-score-pick-download pipeline onto a background task.
    // This avoids blocking the HTTP response during the search-poll loop (Research Pitfall 4).
    let ctx = ctx.clone();
    tokio::spawn(async move {
        send(AutoDownloadEvent::Searching {
            batch_id: batch_id.clone(),
            query: query_desc.clone(),
            backend_count: 1 + rest.len(),
        });

        if let Err(failure) = hydrate_album_tracks(&mut query, key.as_deref()).await {
            info!(
                "Auto-download: no tracklist for '{}': {}",
                query_desc, failure.message
            );
            send(AutoDownloadEvent::Failed {
                batch_id: batch_id.clone(),
                failure,
            });
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
                    send(AutoDownloadEvent::Failed {
                        batch_id: batch_id.clone(),
                        failure,
                    });
                    info!("Auto-download: no results for '{}'", query_desc);
                    return;
                }
            };

        let best_score = all_groups[0].score;

        send(AutoDownloadEvent::ScoringResults {
            batch_id: batch_id.clone(),
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
                batch_id: batch_id.clone(),
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
            batch_id: batch_id.clone(),
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
            Err(failure) => {
                send(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    failure,
                });
                return;
            }
        };

        let backend = match download_backend(None).await {
            Ok(b) => b,
            Err(e) => {
                send(AutoDownloadEvent::Failed {
                    batch_id: batch_id.clone(),
                    failure: backend_unavailable(Stage::Enqueue, &e),
                });
                return;
            }
        };

        let fresh = fresh_batch(&ctx, batch_id.clone(), auto_batch_label(&picked), target);
        let outcome = enqueue_and_watch(&ctx, backend, picked.items, fresh, all_groups).await;
        send(enqueue_outcome(batch_id, &outcome));
    });

    Ok(batch_id_for_response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::server_fns::download::enqueue::other_user;
    use crate::server_fns::download::test_support::{
        batch, group, item, key, same_user_into, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferRegistry;
    use shared::failure::{Cause, Service};

    fn ctx_of(batch: &BatchRef) -> UserCtx {
        test_ctx(&batch.owner.user_id.to_string())
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
        let fresh = same_user_into(&running, running.target.clone());
        let items = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];

        // The handler is dropped while slskd is still answering the enqueue,
        // as when the client disconnects.
        let ctx = ctx_of(&running);
        let handler = tokio::spawn(async move {
            enqueue_and_watch(&ctx, backend, items, fresh, Vec::new()).await
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
        let fresh = same_user_into(&running, running.target.clone());
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Two", "b/02.flac")],
            0.8,
        )];

        enqueue_and_watch(
            &ctx_of(&running),
            backend,
            vec![
                item("peer_a", "One", "01.flac"),
                item("peer_a", "Two", "02.flac"),
            ],
            fresh,
            rest,
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
        let fresh = same_user_into(&running, PathBuf::from("/other-library"));
        let got = enqueue_and_watch(
            &ctx_of(&running),
            backend,
            vec![item("peer_a", "One", "01.flac")],
            fresh,
            Vec::new(),
        )
        .await;
        assert_eq!(got.unwrap_err(), another_folder(RetryAdvice::NotRetryable));
        assert!(stub.destinations().is_empty());
    }

    #[tokio::test]
    async fn a_failed_backend_call_is_the_requests_failure() {
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        let fresh = batch("X");
        let f = enqueue_and_watch(
            &ctx_of(&fresh),
            backend,
            vec![item("peer_a", "One", "01.flac")],
            fresh.clone(),
            Vec::new(),
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
        let fresh = batch("Come Over");
        let mut rx = crate::globals::get_or_create_user_channel(&fresh.owner.user_id)
            .await
            .0
            .subscribe();
        let results = enqueue_and_watch(
            &ctx_of(&fresh),
            backend.clone(),
            vec![item("peer_a", "One", "01.flac")],
            fresh.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert!(matches!(results[0].status, EnqueueStatus::Failed(_)));
        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("no row sent")
        };
        assert_eq!(
            (rows[0].batch_id.as_deref(), rows[0].backend.as_deref()),
            (Some(fresh.owner.batch_id.as_str()), Some(backend.id()))
        );
        assert_eq!(
            rows[0].failure.as_ref().map(|f| f.code),
            Some(FailureCode::PeerOffline)
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
