//! 1-tap Acquire & Play pipeline:
//!
//! 1. Checks the caller's Navidrome library first — if an exact/MBID match is already
//!    owned, emits `AcquireAndPlayStage::Ready { already_owned: true }` immediately with
//!    zero downloads.
//! 2. Otherwise orders enabled `DownloadBackend` instances (fast secondary backends first
//!    when `prefer_fast == true`, falling back to the default archival backend e.g. Soulseek).
//! 3. Searches the prioritized tier first so an instant secondary backend starts downloading
//!    without waiting for a slow P2P sweep; falls back to the archival tier when fast search
//!    yields no high-confidence match.
//! 4. Streams real-time stage events over the user's download WebSocket channel:
//!    `Acquiring -> Importing -> Indexing -> Ready { navidrome_song_id, codec, is_lossy }`,
//!    allowing clients to animate the `seam-light` sweep and start streaming immediately.
//! 5. Records lossy fast acquisitions in `fast_acquisitions` WITHOUT scheduling an archival
//!    FLAC upgrade — archival upgrades trigger strictly on explicit Keep/Star/Promote.

use std::sync::Arc;
use std::time::Duration;

use shared::backends::{BackendMeta, Delivery};
use shared::download::{
    is_lossy_format, AcquireAndPlayEvent, AcquireAndPlayInitialStatus, AcquireAndPlayRequest,
    AcquireAndPlayResponse, AcquireAndPlayStage, DownloadEvent, DownloadProgress, DownloadQuery,
    DownloadState, DownloadableGroup, EnqueueStatus, IndexState, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::Track;
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::auth::UserCtx;
use crate::core::acquire::{
    backend_of, enqueue_and_watch, numbered, prepare_target, Acquisition, NewBatch,
    AUTO_SELECT_SCORE_THRESHOLD,
};
use crate::core::archival::record_fast_acquisition_in;
use crate::core::database_failure;
use crate::core::folders;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::library::{self, MatchKind, Reference};
use crate::core::sources::{self, Searched};
use crate::globals::{
    get_or_create_user_channel, register_user_task, send_user_event, unregister_user_task,
};
use crate::ledger::{
    store, DuplicateRule, JobEvent, JobKind, JobState, Ledger, NewJob, NewSearch, SlotResult,
    LEDGER,
};
use crate::models::archival_upgrade::NewFastAcquisition;
use crate::models::folder::Folder;
use crate::models::user_settings::{LossyFallback, UserSettings};
use crate::server_fns::download::enqueue::{already_running, not_enqueued_failure};
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::{
    enabled_download_backends, Backends, Registry, DEFAULT_DOWNLOAD_BACKEND,
};

/// Maximum duration the stage relay waits for an Acquire & Play job to reach Ready/Failed.
const STAGE_WATCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Polling interval used as a safety net alongside live broadcast updates when watching
/// a job's ledger slot reach `Done` / `Failed`.
const STAGE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Short retry settings used in `finalize_ready` to ensure the ledger writer has flushed
/// `SlotResult::Imported { listed: Some(_) }` before emitting `Ready`.
const FINALIZE_FLUSH_RETRIES: usize = 5;
const FINALIZE_FLUSH_RETRY_DELAY: Duration = Duration::from_millis(40);

fn meta_for<'a>(id: &str, enabled_meta: &[&'a BackendMeta]) -> Option<&'a BackendMeta> {
    enabled_meta
        .iter()
        .copied()
        .find(|m| m.id == id)
        .or_else(|| shared::backends::meta(id))
}

/// Splits enabled backends into `(primary_tier, fallback_tier)` according to `prefer_fast`.
///
/// When `prefer_fast == true`: ONLY explicit `Delivery::LossyOnly` backends run in the fast
/// tier, and `Delivery::AsFound` backends (including Soulseek and Internet Archive) stay in
/// the archival/fallback tier so non-lossy secondary sources never block Soulseek searches.
/// When `prefer_fast == false`: `Delivery::AsFound` backends run first and `Delivery::LossyOnly`
/// backends form the fallback tier.
pub fn partition_backends(
    backends: Vec<Searched>,
    enabled_meta: &[&BackendMeta],
    prefer_fast: bool,
) -> (Vec<Searched>, Vec<Searched>) {
    let mut fast = Vec::new();
    let mut archival = Vec::new();
    for entry in backends {
        let is_lossy_only =
            meta_for(&entry.0, enabled_meta).is_some_and(|m| m.delivery == Delivery::LossyOnly);
        if prefer_fast {
            if is_lossy_only {
                fast.push(entry);
            } else {
                archival.push(entry);
            }
        } else if !is_lossy_only {
            fast.push(entry);
        } else {
            archival.push(entry);
        }
    }
    if fast.is_empty() {
        (archival, Vec::new())
    } else {
        (fast, archival)
    }
}

/// Resolves the target folder for Acquire & Play: explicit `folder_id`, else user's
/// `default_download_folder_id`, else their first owned folder.
pub async fn resolve_target_folder_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    requested_folder_id: Option<&str>,
) -> Result<Folder, Failure> {
    if let Some(folder_id) = requested_folder_id.filter(|id| !id.trim().is_empty()) {
        return folders::owned_in(db, ctx, folder_id).await;
    }

    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    if let Some(default_id) = settings
        .default_download_folder_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        if let Ok(folder) = folders::owned_in(db, ctx, default_id).await {
            return Ok(folder);
        }
    }

    let all = Folder::get_all_by_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    all.into_iter().next().ok_or_else(|| {
        Failure::new(
            FailureCode::FolderRequired,
            Stage::Input,
            RetryAdvice::NeedsUser,
            "No download folder configured for this user",
        )
    })
}

/// Entry point called by `POST /api/acquire-and-play`.
pub async fn acquire_and_play(
    ctx: &UserCtx,
    req: AcquireAndPlayRequest,
) -> Result<AcquireAndPlayResponse, Failure> {
    let enabled = enabled_download_backends().await;
    let importer_failure = crate::importer_health::blocking_failure().await;
    acquire_and_play_in(
        &crate::db::DB,
        &LEDGER,
        Arc::new(ServerNavidrome),
        &Registry,
        &enabled,
        importer_failure,
        ctx,
        req,
    )
    .await
}

/// Testable core of `acquire_and_play`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn acquire_and_play_in(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: Arc<dyn NavidromeOf>,
    backends: &dyn Backends,
    enabled_meta: &[&BackendMeta],
    importer_failure: Option<Failure>,
    ctx: &UserCtx,
    req: AcquireAndPlayRequest,
) -> Result<AcquireAndPlayResponse, Failure> {
    let artist = req.artist.trim().to_string();
    let title = req.title.trim().to_string();
    if artist.is_empty() || title.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Both artist and title are required for Acquire & Play",
        ));
    }

    let request_id = uuid::Uuid::new_v4().to_string();

    // 1. Check Navidrome first — instant playback if the track is already owned,
    // even if beets/importer health is currently degraded.
    let reference = Reference {
        artist: artist.clone(),
        title: title.clone(),
        album: req.album.clone(),
        recording_mbid: req.recording_mbid.clone(),
        duration_s: req.duration_s,
    };
    if let Ok(navi) = navidrome.navidrome_of(&ctx.id()).await {
        match library::find(&navi, &reference).await {
            Ok(found) => {
                if let Some(owned) = found
                    .matches
                    .into_iter()
                    .find(|m| matches!(m.how.kind, MatchKind::Mbid | MatchKind::Exact))
                {
                    let codec = owned.song.format.clone();
                    let is_lossy = codec.as_deref().is_some_and(is_lossy_format);
                    let song_id = owned.song.song_id;
                    emit_stage(
                        &ctx.id(),
                        &request_id,
                        &artist,
                        &title,
                        req.recording_mbid.as_deref(),
                        AcquireAndPlayStage::Ready {
                            navidrome_song_id: Some(song_id.clone()),
                            codec: codec.clone(),
                            is_lossy,
                            already_owned: true,
                            backend_id: None,
                        },
                    )
                    .await;
                    return Ok(AcquireAndPlayResponse {
                        request_id,
                        status: AcquireAndPlayInitialStatus::AlreadyOwned {
                            navidrome_song_id: song_id,
                            codec,
                            is_lossy,
                        },
                    });
                }
            }
            Err(f) => {
                warn!(
                    "Acquire & Play: library pre-check failed for '{} - {}': {}",
                    artist,
                    title,
                    crate::core::logged(&f)
                );
            }
        }
    }

    // Only block new downloads/imports when beets is degraded; already-owned tracks above
    // return immediately without touching the importer.
    if let Some(failure) = importer_failure {
        return Err(failure);
    }

    // 2. Resolve target library folder, user lossy-fallback preference, and usable backends.
    //    - `LossyFallback::Never`: lossy-only backends are strictly excluded from search.
    //    - `LossyFallback::Ask` + `prefer_fast == false`: lossy-only backends may still be
    //      searched as fallback, but a lossy-only candidate is never auto-picked without
    //      explicit user confirmation (`NeedsManualChoice`).
    //    - `LossyFallback::Ask` + `prefer_fast == true` (the explicit 1-tap fast-play path)
    //      or `LossyFallback::Auto`: lossy-only fast backends may be auto-picked immediately
    //      (and upgraded to archival FLAC later only on explicit Keep/Star/Promote).
    let folder = resolve_target_folder_in(db, ctx, req.folder_id.as_deref()).await?;
    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    let lossy_fallback = settings.lossy_fallback;
    let usable = sources::usable_from(backends, enabled_meta, lossy_fallback).await?;
    let lossy_only_ids: Vec<String> = usable
        .last_resort
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    let mut all_usable = Vec::with_capacity(usable.primary.len() + usable.last_resort.len());
    all_usable.extend(usable.primary);
    all_usable.extend(usable.last_resort);

    let (fast_tier, fallback_tier) = partition_backends(all_usable, enabled_meta, req.prefer_fast);
    let backend_ids: Vec<String> = fast_tier
        .iter()
        .chain(fallback_tier.iter())
        .map(|(id, _)| id.clone())
        .collect();

    let batch_id = uuid::Uuid::new_v4().to_string();
    let job_id = uuid::Uuid::new_v4().to_string();
    let search_id = uuid::Uuid::new_v4().to_string();

    let track = Track {
        id: req.recording_mbid.clone().unwrap_or_default(),
        title: title.clone(),
        artist: artist.clone(),
        album_id: None,
        album_title: req.album.clone(),
        release_date: None,
        duration: req
            .duration_s
            .map(|s| format!("{}:{:02}", s / 60, s % 60)),
        mbid: req.recording_mbid.clone(),
        release_mbid: req.release_mbid.clone(),
    };
    let query = DownloadQuery::new(vec![track.clone()]);

    ledger
        .create(
            NewJob {
                id: job_id.clone(),
                user: ctx.id(),
                folder_id: folder.id.clone(),
                wanted: query.clone(),
                import_mode: ImportMode::Singleton,
                state: JobState::Searching,
                search_id: Some(search_id.clone()),
                picked: vec![],
                kind: JobKind::Track,
                options: None,
            },
            Some(NewSearch {
                id: search_id.clone(),
                user: ctx.id(),
                backend: None,
                query: query.clone(),
                spec: None,
            }),
        )
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;

    // Ensure channel exists and keep it alive for the duration of the background pipeline.
    let _ = get_or_create_user_channel(&ctx.id()).await;
    register_user_task(&ctx.id()).await;

    let bg_db = db.clone();
    let bg_ledger = ledger.clone();
    let bg_navidrome = Arc::clone(&navidrome);
    let bg_ctx = ctx.clone();
    let bg_request_id = request_id.clone();
    let bg_batch_id = batch_id.clone();
    let bg_job_id = job_id.clone();
    let bg_search_id = search_id.clone();
    let bg_req = req.clone();
    let bg_backends: Vec<Searched> = fast_tier
        .iter()
        .chain(fallback_tier.iter())
        .cloned()
        .collect();
    let task_user = ctx.id();

    tokio::spawn(async move {
        run_acquire_and_play_pipeline(
            bg_db,
            bg_ledger,
            bg_navidrome,
            bg_ctx,
            folder,
            bg_req,
            track,
            bg_request_id,
            bg_batch_id,
            bg_job_id,
            bg_search_id,
            fast_tier,
            fallback_tier,
            bg_backends,
            lossy_only_ids,
            lossy_fallback,
        )
        .await;
        unregister_user_task(&task_user).await;
    });

    Ok(AcquireAndPlayResponse {
        request_id,
        status: AcquireAndPlayInitialStatus::Started {
            batch_id,
            job_id,
            backend_ids,
        },
    })
}

enum EnqueueAttempt {
    Enqueued {
        routed_id: String,
        picked_quality: String,
        rx: broadcast::Receiver<DownloadEvent>,
    },
    AlreadyRunning(Failure),
    Failed(Failure),
}

#[allow(clippy::too_many_arguments)]
async fn try_enqueue_picked(
    ledger: &Ledger,
    ctx: &UserCtx,
    folder: &Folder,
    request_id: &str,
    batch_id: &str,
    job_id: &str,
    artist: &str,
    title: &str,
    mbid: Option<&str>,
    picked: DownloadableGroup,
    alternates: Vec<DownloadableGroup>,
    all_backends: &[Searched],
    lossy_only_ids: &[String],
) -> EnqueueAttempt {
    let user = ctx.id();
    let picked_quality = picked.quality.clone();
    let picked_source = picked.source.clone();

    let routed_id = match backend_of(&picked.items) {
        Ok(id) => id.to_string(),
        Err(failure) => return EnqueueAttempt::Failed(failure),
    };

    let Some((_, backend)) = all_backends.iter().find(|(id, _)| id == &routed_id) else {
        return EnqueueAttempt::Failed(Failure::new(
            FailureCode::BackendNotConfigured,
            Stage::Enqueue,
            RetryAdvice::NotRetryable,
            format!("Picked backend '{routed_id}' is not available"),
        ));
    };

    emit_stage(
        &user,
        request_id,
        artist,
        title,
        mbid,
        AcquireAndPlayStage::Acquiring {
            backend_id: routed_id.clone(),
            backend_name: backend.name().to_string(),
            source: Some(picked_source),
            quality: Some(picked_quality.clone()),
        },
    )
    .await;

    let target = match prepare_target(folder).await {
        Ok(t) => t,
        Err(failure) => return EnqueueAttempt::Failed(failure),
    };

    let picked_items = numbered(picked.items);
    ledger.record(JobEvent::Picked {
        job: job_id.to_string(),
        items: picked_items.clone(),
    });

    // Subscribe to the user's download event channel BEFORE enqueueing so we cannot miss
    // rapid transitions (Importing -> Indexing -> Ready).
    let tx = get_or_create_user_channel(&user).await;
    let rx = tx.subscribe();

    let delivery_mode = if lossy_only_ids.iter().any(|id| id == &routed_id) {
        Delivery::LossyOnly
    } else {
        shared::backends::delivery(&routed_id)
    };
    let duplicates = DuplicateRule::Guarded.for_delivery(delivery_mode);

    let fresh = NewBatch {
        batch_id: batch_id.to_string(),
        label: format!("{artist} - {title}"),
        backend: routed_id.clone(),
        target,
        import_mode: ImportMode::Singleton,
        duplicates,
        verify_policy: soulbeet::VerifyPolicy::Fast,
    };
    let acquisition = Acquisition {
        job: job_id.to_string(),
        items: picked_items,
        alternates,
        max_sources: MAX_SOURCES_PER_TRACK,
    };

    let enqueued = enqueue_and_watch(ledger, ctx, Arc::clone(backend), fresh, acquisition).await;
    match &enqueued {
        Ok(results) if results.iter().any(|d| d.status == EnqueueStatus::Enqueued) => {
            info!(
                "Acquire & Play {} enqueued on '{}' for '{} - {}'",
                request_id, routed_id, artist, title
            );
            EnqueueAttempt::Enqueued {
                routed_id,
                picked_quality,
                rx,
            }
        }
        Ok(results)
            if results
                .iter()
                .any(|d: &QueuedDownload| d.status == EnqueueStatus::AlreadyRunning) =>
        {
            EnqueueAttempt::AlreadyRunning(
                already_running().with_message("Track is already downloading in another batch"),
            )
        }
        Ok(results) => EnqueueAttempt::Failed(
            not_enqueued_failure(results).with_message("Acquire & Play failed to queue download"),
        ),
        Err(failure) => EnqueueAttempt::Failed(failure.clone()),
    }
}

fn is_lossy_only_group(group: &DownloadableGroup, lossy_only_ids: &[String]) -> bool {
    group.items.first().is_some_and(|item| {
        lossy_only_ids.iter().any(|id| id == &item.backend)
            || shared::backends::delivery(&item.backend) == Delivery::LossyOnly
    })
}

fn may_auto_pick_lossy(lossy_fallback: LossyFallback, prefer_fast: bool) -> bool {
    match lossy_fallback {
        LossyFallback::Never => false,
        LossyFallback::Ask => prefer_fast,
        LossyFallback::Auto => true,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_acquire_and_play_pipeline(
    db: SqlitePool,
    ledger: Ledger,
    navidrome: Arc<dyn NavidromeOf>,
    ctx: UserCtx,
    folder: Folder,
    req: AcquireAndPlayRequest,
    track: Track,
    request_id: String,
    batch_id: String,
    job_id: String,
    search_id: String,
    fast_tier: Vec<Searched>,
    fallback_tier: Vec<Searched>,
    all_backends: Vec<Searched>,
    lossy_only_ids: Vec<String>,
    lossy_fallback: LossyFallback,
) {
    let user = ctx.id();
    let artist = track.artist.clone();
    let title = track.title.clone();
    let mbid = req.recording_mbid.clone();

    let (first_fast_id, first_fast_name) = fast_tier
        .first()
        .map(|(id, b)| (id.clone(), b.name().to_string()))
        .unwrap_or_else(|| (DEFAULT_DOWNLOAD_BACKEND.to_string(), "Soulseek".to_string()));

    emit_stage(
        &user,
        &request_id,
        &artist,
        &title,
        mbid.as_deref(),
        AcquireAndPlayStage::Acquiring {
            backend_id: first_fast_id,
            backend_name: first_fast_name,
            source: None,
            quality: None,
        },
    )
    .await;

    let (mut all_groups, mut fallback_searched) =
        match search_tiered(&fast_tier, &fallback_tier, &track).await {
            Ok(outcome) => outcome,
            Err(failure) => {
                ledger.record(JobEvent::Ended {
                    job: job_id,
                    state: JobState::Failed,
                    failure: Some(failure.clone()),
                });
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::Failed { failure },
                )
                .await;
                return;
            }
        };

    if let Err(e) = ledger.store_candidates(&search_id, all_groups.clone()).await {
        let failure = database_failure(Stage::SourceSearch, e);
        ledger.record(JobEvent::Ended {
            job: job_id,
            state: JobState::Failed,
            failure: Some(failure.clone()),
        });
        emit_stage(
            &user,
            &request_id,
            &artist,
            &title,
            mbid.as_deref(),
            AcquireAndPlayStage::Failed { failure },
        )
        .await;
        return;
    }

    let best_score = all_groups[0].score;
    let requires_lossy_confirmation = !may_auto_pick_lossy(lossy_fallback, req.prefer_fast)
        && is_lossy_only_group(&all_groups[0], &lossy_only_ids);
    if best_score < AUTO_SELECT_SCORE_THRESHOLD || requires_lossy_confirmation {
        ledger.record(JobEvent::NeedsSourceChoice {
            job: job_id.clone(),
        });
        emit_stage(
            &user,
            &request_id,
            &artist,
            &title,
            mbid.as_deref(),
            AcquireAndPlayStage::NeedsManualChoice {
                search_id,
                results: all_groups,
                best_score,
                threshold: AUTO_SELECT_SCORE_THRESHOLD,
            },
        )
        .await;
        return;
    }

    let picked = all_groups.remove(0);
    let mut active_job_id = job_id.clone();
    let mut active_batch_id = batch_id.clone();

    let (routed_id, picked_quality, rx) = match try_enqueue_picked(
        &ledger,
        &ctx,
        &folder,
        &request_id,
        &active_batch_id,
        &active_job_id,
        &artist,
        &title,
        mbid.as_deref(),
        picked,
        all_groups,
        &all_backends,
        &lossy_only_ids,
    )
    .await
    {
        EnqueueAttempt::Enqueued {
            routed_id,
            picked_quality,
            rx,
        } => (routed_id, picked_quality, rx),
        EnqueueAttempt::AlreadyRunning(failure) => {
            emit_stage(
                &user,
                &request_id,
                &artist,
                &title,
                mbid.as_deref(),
                AcquireAndPlayStage::Failed { failure },
            )
            .await;
            return;
        }
        EnqueueAttempt::Failed(first_failure) => {
            ledger.record(JobEvent::Ended {
                job: active_job_id.clone(),
                state: JobState::Failed,
                failure: Some(first_failure.clone()),
            });

            // If fast-tier enqueue failed and fallback_tier hasn't been tried yet, fall back to it!
            if fallback_searched || fallback_tier.is_empty() {
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::Failed {
                        failure: first_failure,
                    },
                )
                .await;
                return;
            }
            fallback_searched = true;
            let _ = fallback_searched;
            warn!(
                "Acquire & Play {} fast-tier enqueue failed ({}); falling back to archival tier",
                request_id, first_failure.message
            );

            let fb_first = fallback_tier.first().expect("non-empty fallback_tier");
            emit_stage(
                &user,
                &request_id,
                &artist,
                &title,
                mbid.as_deref(),
                AcquireAndPlayStage::Acquiring {
                    backend_id: fb_first.0.clone(),
                    backend_name: fb_first.1.name().to_string(),
                    source: None,
                    quality: None,
                },
            )
            .await;

            let fb_found =
                sources::search_tier(&fallback_tier, None, std::slice::from_ref(&track)).await;
            if fb_found.groups.is_empty() {
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::Failed {
                        failure: fb_found.failure(),
                    },
                )
                .await;
                return;
            }
            let mut fb_groups = fb_found.groups;

            let fb_job_id = uuid::Uuid::new_v4().to_string();
            let fb_batch_id = uuid::Uuid::new_v4().to_string();
            let fb_search_id = uuid::Uuid::new_v4().to_string();
            let query = DownloadQuery::new(vec![track.clone()]);
            if let Err(e) = ledger
                .create(
                    NewJob {
                        id: fb_job_id.clone(),
                        user: ctx.id(),
                        folder_id: folder.id.clone(),
                        wanted: query.clone(),
                        import_mode: ImportMode::Singleton,
                        state: JobState::Searching,
                        search_id: Some(fb_search_id.clone()),
                        picked: vec![],
                        kind: JobKind::Track,
                        options: None,
                    },
                    Some(NewSearch {
                        id: fb_search_id.clone(),
                        user: ctx.id(),
                        backend: None,
                        query,
                        spec: None,
                    }),
                )
                .await
            {
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::Failed {
                        failure: database_failure(Stage::SourceSearch, e),
                    },
                )
                .await;
                return;
            }
            if let Err(e) = ledger
                .store_candidates(&fb_search_id, fb_groups.clone())
                .await
            {
                let failure = database_failure(Stage::SourceSearch, e);
                ledger.record(JobEvent::Ended {
                    job: fb_job_id,
                    state: JobState::Failed,
                    failure: Some(failure.clone()),
                });
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::Failed { failure },
                )
                .await;
                return;
            }

            let fb_best = fb_groups[0].score;
            let fb_requires_lossy_confirmation =
                !may_auto_pick_lossy(lossy_fallback, req.prefer_fast)
                    && is_lossy_only_group(&fb_groups[0], &lossy_only_ids);
            if fb_best < AUTO_SELECT_SCORE_THRESHOLD || fb_requires_lossy_confirmation {
                ledger.record(JobEvent::NeedsSourceChoice {
                    job: fb_job_id,
                });
                emit_stage(
                    &user,
                    &request_id,
                    &artist,
                    &title,
                    mbid.as_deref(),
                    AcquireAndPlayStage::NeedsManualChoice {
                        search_id: fb_search_id,
                        results: fb_groups,
                        best_score: fb_best,
                        threshold: AUTO_SELECT_SCORE_THRESHOLD,
                    },
                )
                .await;
                return;
            }

            let fb_picked = fb_groups.remove(0);
            active_job_id = fb_job_id;
            active_batch_id = fb_batch_id;
            match try_enqueue_picked(
                &ledger,
                &ctx,
                &folder,
                &request_id,
                &active_batch_id,
                &active_job_id,
                &artist,
                &title,
                mbid.as_deref(),
                fb_picked,
                fb_groups,
                &all_backends,
                &lossy_only_ids,
            )
            .await
            {
                EnqueueAttempt::Enqueued {
                    routed_id,
                    picked_quality,
                    rx,
                } => (routed_id, picked_quality, rx),
                EnqueueAttempt::AlreadyRunning(failure) => {
                    emit_stage(
                        &user,
                        &request_id,
                        &artist,
                        &title,
                        mbid.as_deref(),
                        AcquireAndPlayStage::Failed { failure },
                    )
                    .await;
                    return;
                }
                EnqueueAttempt::Failed(failure) => {
                    ledger.record(JobEvent::Ended {
                        job: active_job_id,
                        state: JobState::Failed,
                        failure: Some(failure.clone()),
                    });
                    emit_stage(
                        &user,
                        &request_id,
                        &artist,
                        &title,
                        mbid.as_deref(),
                        AcquireAndPlayStage::Failed { failure },
                    )
                    .await;
                    return;
                }
            }
        }
    };

    let backend_lossy_only = lossy_only_ids.iter().any(|id| id == &routed_id)
        || shared::backends::meta(&routed_id).is_some_and(|m| m.delivery == Delivery::LossyOnly);

    relay_job_stages_until_terminal(
        &db,
        &ledger,
        &*navidrome,
        &ctx,
        &folder,
        rx,
        StageRelayContext {
            request_id,
            batch_id: active_batch_id,
            job_id: active_job_id,
            artist,
            title,
            album: req.album,
            recording_mbid: mbid,
            duration_s: req.duration_s,
            backend_id: routed_id,
            backend_lossy_only,
            picked_quality,
        },
    )
    .await;
}

/// Searches `fast_tier` first; if a group reaches `AUTO_SELECT_SCORE_THRESHOLD`, returns
/// `(groups, false)` immediately without searching `fallback_tier`. Otherwise searches
/// `fallback_tier` and returns `(merged_groups, true)`.
async fn search_tiered(
    fast_tier: &[Searched],
    fallback_tier: &[Searched],
    track: &Track,
) -> Result<(Vec<DownloadableGroup>, bool), Failure> {
    let tracks = std::slice::from_ref(track);
    let mut found = if fast_tier.is_empty() {
        sources::Found::default()
    } else {
        let fast = sources::search_tier(fast_tier, None, tracks).await;
        if fast
            .groups
            .first()
            .is_some_and(|g| g.score >= AUTO_SELECT_SCORE_THRESHOLD)
        {
            return Ok((fast.groups, false));
        }
        fast
    };

    let mut searched_fallback = false;
    if !fallback_tier.is_empty() {
        searched_fallback = true;
        let fallback = sources::search_tier(fallback_tier, None, tracks).await;
        // When the fast tier returned nothing, put fallback first so archival (e.g. slskd)
        // diagnostics (`SearchAllFiltered`, `unanswered_note`, backend failures) are not
        // masked by an empty fast-tier finish.
        found = if found.groups.is_empty() {
            fallback.then(found)
        } else {
            found.then(fallback)
        };
    }

    if found.groups.is_empty() {
        return Err(found.failure());
    }

    found.groups.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((found.groups, searched_fallback))
}

/// Context needed by the stage relay loop.
pub(crate) struct StageRelayContext {
    pub request_id: String,
    pub batch_id: String,
    pub job_id: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub recording_mbid: Option<String>,
    pub duration_s: Option<u32>,
    pub backend_id: String,
    pub backend_lossy_only: bool,
    pub picked_quality: String,
}

/// Watches live `DownloadEvent`s and ledger slot state for `relay.job_id`, emitting
/// deduplicated `AcquireAndPlayStage::{Importing, Indexing, Ready, Failed}` frames
/// and recording lossy fast acquisitions in SQLite (without scheduling archival upgrades).
pub(crate) async fn relay_job_stages_until_terminal(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    user_ctx: &UserCtx,
    folder: &Folder,
    rx: broadcast::Receiver<DownloadEvent>,
    relay: StageRelayContext,
) {
    let user = user_ctx.id();
    let deadline = tokio::time::Instant::now() + STAGE_WATCH_TIMEOUT;
    let mut poll = tokio::time::interval(STAGE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut rx = Some(rx);
    let mut emitted_importing = false;
    let mut emitted_indexing = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            let (code, stage) = if emitted_indexing {
                (FailureCode::IndexNotFoundAfterScan, Stage::Index)
            } else if emitted_importing {
                (FailureCode::ImportTotalTimeout, Stage::Import)
            } else {
                (FailureCode::TransferTimedOut, Stage::Transfer)
            };
            emit_stage(
                &user,
                &relay.request_id,
                &relay.artist,
                &relay.title,
                relay.recording_mbid.as_deref(),
                AcquireAndPlayStage::Failed {
                    failure: Failure::new(
                        code,
                        stage,
                        RetryAdvice::SameSourceLater,
                        "Timed out waiting for track acquisition and indexing",
                    ),
                },
            )
            .await;
            return;
        }

        tokio::select! {
            recv = async {
                match rx.as_mut() {
                    Some(channel) => channel.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match recv {
                    Ok(DownloadEvent::Progress(rows)) => {
                        for row in rows {
                            if !row_matches_job(&row, &relay.batch_id, &relay.job_id) {
                                continue;
                            }
                            if let Some(terminal) = process_progress_row(
                                db,
                                ledger,
                                navidrome,
                                user_ctx,
                                folder,
                                &relay,
                                &row,
                                &mut emitted_importing,
                                &mut emitted_indexing,
                            ).await {
                                emit_stage(
                                    &user,
                                    &relay.request_id,
                                    &relay.artist,
                                    &relay.title,
                                    relay.recording_mbid.as_deref(),
                                    terminal,
                                ).await;
                                return;
                            }
                        }
                    }
                    Ok(DownloadEvent::Replaced { entry, .. }) => {
                        if row_matches_job(&entry, &relay.batch_id, &relay.job_id) {
                            if let Some(terminal) = process_progress_row(
                                db,
                                ledger,
                                navidrome,
                                user_ctx,
                                folder,
                                &relay,
                                &entry,
                                &mut emitted_importing,
                                &mut emitted_indexing,
                            ).await {
                                emit_stage(
                                    &user,
                                    &relay.request_id,
                                    &relay.artist,
                                    &relay.title,
                                    relay.recording_mbid.as_deref(),
                                    terminal,
                                ).await;
                                return;
                            }
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        // Keep polling the ledger until terminal even if the broadcast channel closes.
                        rx = None;
                    }
                }
            }
            _ = poll.tick() => {
                if let Some(terminal) = poll_ledger_slot(
                    db,
                    ledger,
                    navidrome,
                    user_ctx,
                    folder,
                    &relay,
                    &mut emitted_importing,
                    &mut emitted_indexing,
                ).await {
                    emit_stage(
                        &user,
                        &relay.request_id,
                        &relay.artist,
                        &relay.title,
                        relay.recording_mbid.as_deref(),
                        terminal,
                    ).await;
                    return;
                }
            }
        }
    }
}

fn row_matches_job(row: &DownloadProgress, batch_id: &str, job_id: &str) -> bool {
    row.job.as_ref().is_some_and(|slot| slot.job == job_id)
        || row.batch_id.as_deref() == Some(batch_id)
}

#[allow(clippy::too_many_arguments)]
async fn process_progress_row(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    user_ctx: &UserCtx,
    folder: &Folder,
    relay: &StageRelayContext,
    row: &DownloadProgress,
    emitted_importing: &mut bool,
    emitted_indexing: &mut bool,
) -> Option<AcquireAndPlayStage> {
    let user = user_ctx.id();
    match (&row.state, &row.index) {
        (DownloadState::ImportQueued(_) | DownloadState::Importing, _) => {
            if !*emitted_importing {
                *emitted_importing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Importing,
                )
                .await;
            }
            None
        }
        (DownloadState::Imported, Some(IndexState::Waiting)) => {
            if !*emitted_importing {
                *emitted_importing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Importing,
                )
                .await;
            }
            if !*emitted_indexing {
                *emitted_indexing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Indexing,
                )
                .await;
            }
            None
        }
        (DownloadState::Imported, Some(IndexState::Listed)) => {
            if !*emitted_importing {
                *emitted_importing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Importing,
                )
                .await;
            }
            if !*emitted_indexing {
                *emitted_indexing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Indexing,
                )
                .await;
            }
            Some(finalize_ready(db, ledger, navidrome, user_ctx, folder, relay, false).await)
        }
        (DownloadState::Imported, Some(IndexState::Failed(failure))) => {
            Some(AcquireAndPlayStage::Failed {
                failure: failure.clone(),
            })
        }
        (DownloadState::Failed(msg), _) => {
            let failure = row.failure.clone().unwrap_or_else(|| {
                Failure::new(
                    FailureCode::TransferAborted,
                    Stage::Transfer,
                    RetryAdvice::OtherSource,
                    msg.clone(),
                )
            });
            Some(AcquireAndPlayStage::Failed { failure })
        }
        (DownloadState::Cancelled, _) => Some(AcquireAndPlayStage::Failed {
            failure: Failure::new(
                FailureCode::TransferCancelled,
                Stage::Transfer,
                RetryAdvice::NotRetryable,
                "Download cancelled",
            ),
        }),
        (DownloadState::ImportSkipped, _) => {
            if matches!(
                row.skip,
                Some(shared::download::SkipKind::AlreadyInLibrary)
            ) {
                Some(finalize_ready(db, ledger, navidrome, user_ctx, folder, relay, true).await)
            } else {
                let failure = row.failure.clone().unwrap_or_else(|| {
                    Failure::new(
                        FailureCode::ImportNoMatch,
                        Stage::Import,
                        RetryAdvice::NeedsDecision,
                        "Import requires manual match decision",
                    )
                });
                Some(AcquireAndPlayStage::Failed { failure })
            }
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_ledger_slot(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    user_ctx: &UserCtx,
    folder: &Folder,
    relay: &StageRelayContext,
    emitted_importing: &mut bool,
    emitted_indexing: &mut bool,
) -> Option<AcquireAndPlayStage> {
    let tracks = store::tracks_of(db, &relay.job_id).await.ok()?;
    let first = tracks.first()?;
    let user = user_ctx.id();
    match first.state {
        JobState::Importing => {
            if !*emitted_importing {
                *emitted_importing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Importing,
                )
                .await;
            }
            None
        }
        JobState::Indexing => {
            if !*emitted_importing {
                *emitted_importing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Importing,
                )
                .await;
            }
            if !*emitted_indexing {
                *emitted_indexing = true;
                emit_stage(
                    &user,
                    &relay.request_id,
                    &relay.artist,
                    &relay.title,
                    relay.recording_mbid.as_deref(),
                    AcquireAndPlayStage::Indexing,
                )
                .await;
            }
            None
        }
        JobState::Done => {
            let is_already_in_library =
                matches!(first.result, Some(SlotResult::AlreadyInLibrary { .. }));
            if !is_already_in_library {
                if !*emitted_importing {
                    *emitted_importing = true;
                    emit_stage(
                        &user,
                        &relay.request_id,
                        &relay.artist,
                        &relay.title,
                        relay.recording_mbid.as_deref(),
                        AcquireAndPlayStage::Importing,
                    )
                    .await;
                }
                if !*emitted_indexing {
                    *emitted_indexing = true;
                    emit_stage(
                        &user,
                        &relay.request_id,
                        &relay.artist,
                        &relay.title,
                        relay.recording_mbid.as_deref(),
                        AcquireAndPlayStage::Indexing,
                    )
                    .await;
                }
            }
            Some(
                finalize_ready(
                    db,
                    ledger,
                    navidrome,
                    user_ctx,
                    folder,
                    relay,
                    is_already_in_library,
                )
                .await,
            )
        }
        JobState::Failed | JobState::Cancelled | JobState::NeedsDecision | JobState::Dismissed => {
            let failure = first
                .row
                .as_ref()
                .and_then(|r| r.failure.clone())
                .unwrap_or_else(|| {
                    if first.state == JobState::Dismissed {
                        already_running()
                            .with_message("Track is already downloading in another batch")
                    } else {
                        Failure::new(
                            FailureCode::TransferAborted,
                            Stage::Transfer,
                            RetryAdvice::OtherSource,
                            format!("Acquire & Play ended in state {}", first.state.as_str()),
                        )
                    }
                });
            Some(AcquireAndPlayStage::Failed { failure })
        }
        _ => None,
    }
}

/// Reads the indexed Navidrome song facts from the job's ledger slot (flushing the ledger
/// writer first so `SlotResult::Imported { listed: Some(_) }` is never missed by a race),
/// records a `fast_acquisitions` entry for freshly imported tracks (strictly without
/// scheduling any archival upgrade yet), and builds `AcquireAndPlayStage::Ready`.
///
/// When the slot was skipped as `AlreadyInLibrary`, looks up the existing Navidrome song
/// ID and format instead of returning `None` or recording a bogus `fast_acquisitions` row.
async fn finalize_ready(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    user_ctx: &UserCtx,
    folder: &Folder,
    relay: &StageRelayContext,
    already_in_library_hint: bool,
) -> AcquireAndPlayStage {
    let mut navidrome_song_id = None;
    let mut listed_codec = None;
    let mut listed_duration_s = None;
    let mut listed_recording_mbid = None;
    let mut imported_as_is = false;
    let mut already_in_library = already_in_library_hint;

    for attempt in 0..FINALIZE_FLUSH_RETRIES {
        let _ = ledger.applied().await;
        if let Ok(tracks) = store::tracks_of(db, &relay.job_id).await {
            if tracks
                .iter()
                .any(|t| matches!(t.result, Some(SlotResult::AlreadyInLibrary { .. })))
            {
                already_in_library = true;
                break;
            }
            if let Some((song_id, format, duration_s, recording_mbid, untagged)) =
                tracks.into_iter().find_map(|t| match t.result {
                    Some(SlotResult::Imported {
                        untagged,
                        listed: Some(songs),
                        ..
                    }) => songs.into_iter().next().map(|song| {
                        (
                            Some(song.song_id),
                            song.format,
                            song.duration_s,
                            song.recording_mbid,
                            untagged,
                        )
                    }),
                    _ => None,
                })
            {
                navidrome_song_id = song_id;
                listed_codec = format;
                listed_duration_s = duration_s;
                listed_recording_mbid = recording_mbid;
                imported_as_is = untagged;
                break;
            }
        }
        if already_in_library || attempt + 1 >= FINALIZE_FLUSH_RETRIES {
            break;
        }
        tokio::time::sleep(FINALIZE_FLUSH_RETRY_DELAY).await;
    }

    if already_in_library {
        let reference = Reference {
            artist: relay.artist.clone(),
            title: relay.title.clone(),
            album: relay.album.clone(),
            recording_mbid: relay.recording_mbid.clone(),
            duration_s: relay.duration_s,
        };
        let (existing_id, existing_codec) = match navidrome.navidrome_of(&user_ctx.id()).await {
            Ok(navi) => match library::find(&navi, &reference).await {
                Ok(found) => found
                    .matches
                    .iter()
                    .find(|m| matches!(m.how.kind, MatchKind::Mbid | MatchKind::Exact))
                    .or_else(|| found.matches.first())
                    .map(|m| (Some(m.song.song_id.clone()), m.song.format.clone()))
                    .unwrap_or((None, None)),
                Err(_) => (None, None),
            },
            Err(_) => (None, None),
        };
        let is_lossy = existing_codec.as_deref().is_some_and(is_lossy_format);
        return AcquireAndPlayStage::Ready {
            navidrome_song_id: existing_id,
            codec: existing_codec,
            is_lossy,
            already_owned: true,
            backend_id: None,
        };
    }

    let codec = listed_codec.or_else(|| {
        let q = relay.picked_quality.trim();
        (!q.is_empty()).then(|| q.to_string())
    });

    let is_lossy = relay.backend_lossy_only || codec.as_deref().is_some_and(is_lossy_format);

    let resolved_mbid = if imported_as_is {
        None
    } else {
        relay
            .recording_mbid
            .as_deref()
            .or(listed_recording_mbid.as_deref())
    };
    let resolved_duration_s = listed_duration_s.or(relay.duration_s);

    if let Err(f) = record_fast_acquisition_in(
        db,
        NewFastAcquisition {
            user_id: &user_ctx.user_id,
            folder_id: &folder.id,
            navidrome_song_id: navidrome_song_id.as_deref(),
            discovery_track_id: None,
            artist: &relay.artist,
            title: &relay.title,
            album: relay.album.as_deref(),
            recording_mbid: resolved_mbid,
            backend_id: &relay.backend_id,
            codec: codec.as_deref().unwrap_or("unknown"),
            is_lossy,
            duration_s: resolved_duration_s,
            imported_as_is,
        },
    )
    .await
    {
        warn!(
            "Failed to record fast acquisition for '{} - {}': {}",
            relay.artist,
            relay.title,
            crate::core::logged(&f)
        );
    }

    AcquireAndPlayStage::Ready {
        navidrome_song_id,
        codec,
        is_lossy,
        already_owned: false,
        backend_id: Some(relay.backend_id.clone()),
    }
}

async fn emit_stage(
    user: &crate::auth::UserId,
    request_id: &str,
    artist: &str,
    title: &str,
    recording_mbid: Option<&str>,
    stage: AcquireAndPlayStage,
) {
    let event = DownloadEvent::AcquireAndPlay(AcquireAndPlayEvent {
        request_id: request_id.to_string(),
        artist: artist.to_string(),
        title: title.to_string(),
        recording_mbid: recording_mbid.map(str::to_string),
        stage,
    });
    let _ = send_user_event(user, event).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Role, UserId};
    use crate::core::library::SongFacts;
    use crate::db::test_db;
    use crate::models::archival_upgrade::{ArchivalUpgradeRow, FastAcquisitionRow};
    use crate::server_fns::download::test_support::{
        group, item_on, search_result, ScriptedOutcome, StubBackend, StubBackends,
    };
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{native_song, NavidromeStub};
    use async_trait::async_trait;
    use shared::backends::Switch;
    use shared::download::{JobSlot, SearchState};
    use soulbeet::navidrome::NavidromeClient;

    struct StubNavidrome(Option<Arc<NavidromeClient>>);

    #[async_trait]
    impl NavidromeOf for StubNavidrome {
        async fn navidrome_of(
            &self,
            _user: &UserId,
        ) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
            self.0.clone().ok_or(NavidromeLookupError::NotLinked)
        }
    }

    async fn seed_user(db: &SqlitePool, user_id: &str) -> (UserCtx, Folder, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let folder_id = format!("{user_id}-folder");
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(user_id)
            .bind(user_id)
            .execute(db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'Music', ?)")
            .bind(&folder_id)
            .bind(user_id)
            .bind(dir.path().to_str().unwrap())
            .execute(db)
            .await
            .unwrap();
        let ctx = UserCtx {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            role: Role::User,
        };
        let folder = Folder {
            id: folder_id,
            user_id: user_id.to_string(),
            name: "Music".to_string(),
            path: dir.path().to_str().unwrap().to_string(),
            shared: false,
        };
        (ctx, folder, dir)
    }

    #[tokio::test]
    async fn already_owned_track_returns_immediately_without_downloading() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "owned-user").await;
        let ledger = Ledger::start(db.clone());

        let song = native_song(
            "nd-song-1",
            "Kowloon",
            "Wake Up",
            200.0,
            Some("rec-mbid-1"),
        );
        let stub_nd = NavidromeStub::start();
        stub_nd.answer("/api/song", &[&format!("[{song}]")]);
        let navi = stub_nd.client();

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        let stub_backend = StubBackend::new(vec![]);
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub_backend.clone());

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(Some(navi))),
            &backends,
            &[&shared::backends::SLSKD],
            Some(Failure::new(
                FailureCode::ImporterBroken,
                Stage::Import,
                RetryAdvice::NeedsUser,
                "beets is broken",
            )),
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: Some("Come Over".into()),
                recording_mbid: Some("rec-mbid-1".into()),
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            resp.status,
            AcquireAndPlayInitialStatus::AlreadyOwned {
                navidrome_song_id: "nd-song-1".into(),
                codec: Some("flac".into()),
                is_lossy: false,
            }
        );
        // Never touched any download backend.
        assert!(stub_backend.specs().is_empty());

        let event = rx.try_recv().unwrap();
        match event {
            DownloadEvent::AcquireAndPlay(ev) => {
                assert_eq!(ev.request_id, resp.request_id);
                assert_eq!(
                    ev.stage,
                    AcquireAndPlayStage::Ready {
                        navidrome_song_id: Some("nd-song-1".into()),
                        codec: Some("flac".into()),
                        is_lossy: false,
                        already_owned: true,
                        backend_id: None,
                    }
                );
            }
            other => panic!("expected AcquireAndPlay Ready event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ambiguous_match_emits_needs_manual_choice_fallback() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "ambig-user").await;
        let ledger = Ledger::start(db.clone());

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        let stub = StubBackend::new(vec![]);
        let low_item = item_on(DEFAULT_DOWNLOAD_BACKEND, "peer_a", "Wake Up (Live)", "live.mp3");
        let low_group = group("peer_a", vec![low_item], 0.42);
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![low_group],
        ))]);
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub);

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            resp.status,
            AcquireAndPlayInitialStatus::Started { .. }
        ));

        let mut stages = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(DownloadEvent::AcquireAndPlay(ap))) if ap.request_id == resp.request_id => {
                    let done = matches!(ap.stage, AcquireAndPlayStage::NeedsManualChoice { .. });
                    stages.push(ap.stage);
                    if done {
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }

        assert!(
            matches!(stages.first(), Some(AcquireAndPlayStage::Acquiring { .. })),
            "expected Acquiring first, got {stages:?}"
        );
        assert!(
            stages.iter().any(|s| matches!(
                s,
                AcquireAndPlayStage::NeedsManualChoice {
                    best_score,
                    threshold,
                    results,
                    ..
                } if *best_score < *threshold && results.len() == 1
            )),
            "expected NeedsManualChoice fallback, got {stages:?}"
        );
    }

    #[tokio::test]
    async fn fast_secondary_backend_is_preferred_before_soulseek_fallback() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "tiered-user").await;
        let ledger = Ledger::start(db.clone());

        static FAST_META: BackendMeta = BackendMeta {
            id: "fast-instant",
            name: "Fast Instant",
            service: "fast-instant",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        let fast_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let mut fast_item = item_on("fast-instant", "cdn", "Wake Up", "wake.opus");
        fast_item.quality = "OPUS 160".into();
        let mut fast_group = group("cdn", vec![fast_item], 0.91);
        fast_group.quality = "OPUS 160".into();
        fast_stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![fast_group],
        ))]);

        let slskd_stub = StubBackend::new(vec![]);
        let backends = StubBackends::default()
            .with(DEFAULT_DOWNLOAD_BACKEND, slskd_stub.clone())
            .with("fast-instant", fast_stub.clone());

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD, &FAST_META],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        match &resp.status {
            AcquireAndPlayInitialStatus::Started { backend_ids, .. } => {
                assert_eq!(backend_ids, &["fast-instant", DEFAULT_DOWNLOAD_BACKEND]);
            }
            other => panic!("expected Started, got {other:?}"),
        }

        // Wait for the picked fast backend's Acquiring event (with source = Some("cdn")).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut picked_fast = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(DownloadEvent::AcquireAndPlay(ap))) if ap.request_id == resp.request_id => {
                    if let AcquireAndPlayStage::Acquiring {
                        backend_id,
                        source: Some(src),
                        ..
                    } = ap.stage
                    {
                        assert_eq!(backend_id, "fast-instant");
                        assert_eq!(src, "cdn");
                        picked_fast = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(picked_fast, "expected fast-instant backend to pick source");
        assert_eq!(fast_stub.specs().len(), 1);
        assert!(
            slskd_stub.specs().is_empty(),
            "Soulseek fallback should not be searched when fast backend returned a confident match"
        );
    }

    #[tokio::test]
    async fn stage_relay_emits_importing_indexing_ready_and_records_lossy_without_upgrade() {
        let db = test_db::migrated().await;
        let (ctx, folder, _dir) = seed_user(&db, "relay-user").await;
        let ledger = Ledger::start(db.clone());

        let job_id = "job-relay-1".to_string();
        let batch_id = "batch-relay-1".to_string();
        let slot = JobSlot {
            job: job_id.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_id.clone(),
                    user: ctx.id(),
                    folder_id: folder.id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Downloading,
                    search_id: None,
                    picked: vec![(0, item_on("fast-net", "cdn", "Wake Up", "wake.opus"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let relay_rx = tx.subscribe();
        let mut client_rx = tx.subscribe();

        let relay_task = tokio::spawn({
            let db = db.clone();
            let ledger = ledger.clone();
            let ctx = ctx.clone();
            let folder = folder.clone();
            let job_id = job_id.clone();
            let batch_id = batch_id.clone();
            async move {
                relay_job_stages_until_terminal(
                    &db,
                    &ledger,
                    &StubNavidrome(None),
                    &ctx,
                    &folder,
                    relay_rx,
                    StageRelayContext {
                        request_id: "req-1".into(),
                        batch_id,
                        job_id,
                        artist: "Kowloon".into(),
                        title: "Wake Up".into(),
                        album: Some("Come Over".into()),
                        recording_mbid: Some("mbid-9".into()),
                        duration_s: None,
                        backend_id: "fast-net".into(),
                        backend_lossy_only: false,
                        picked_quality: "OPUS 160".into(),
                    },
                )
                .await;
            }
        });

        let base_row = || {
            DownloadProgress::queued("t-1".into(), "cdn".into(), "wake.opus".into(), 100)
                .with_batch(batch_id.clone(), "Kowloon - Wake Up".into())
                .with_job(slot.clone())
        };

        // 1. Importing
        tx.send(DownloadEvent::Progress(vec![
            base_row().with_state(DownloadState::Importing),
        ]))
        .unwrap();

        // 2. Indexing
        let mut indexing_row = base_row().with_state(DownloadState::Imported);
        indexing_row.index = Some(IndexState::Waiting);
        tx.send(DownloadEvent::Progress(vec![indexing_row]))
            .unwrap();

        // 3. Record indexed song in ledger, then emit Listed
        let song_facts = SongFacts {
            song_id: "nd-ready-77".into(),
            title: "Wake Up".into(),
            artist: Some("Kowloon".into()),
            album: Some("Come Over".into()),
            album_id: None,
            year: None,
            track: None,
            duration_s: Some(200),
            format: Some("opus".into()),
            bitrate_kbps: Some(160),
            size_bytes: Some(4_000_000),
            path: Some("Kowloon/Wake Up.opus".into()),
            recording_mbid: Some("mbid-9".into()),
            rating: 0,
            starred: false,
            starred_at: None,
            play_count: 0,
            folder: None,
        };
        let mut listed_row = base_row().with_state(DownloadState::Imported);
        listed_row.index = Some(IndexState::Listed);
        ledger.record(JobEvent::Settled {
            state: JobState::Done,
            rows: vec![listed_row.clone()],
            results: vec![(
                slot.clone(),
                SlotResult::Imported {
                    applied: vec![],
                    untagged: false,
                    listed: Some(vec![song_facts]),
                    replaced: vec![],
                    not_replaced: vec![],
                },
            )],
        });
        ledger.settled().await;
        tx.send(DownloadEvent::Progress(vec![listed_row])).unwrap();

        relay_task.await.unwrap();

        let mut observed = Vec::new();
        while let Ok(ev) = client_rx.try_recv() {
            if let DownloadEvent::AcquireAndPlay(ap) = ev {
                observed.push(ap.stage);
            }
        }
        assert_eq!(
            observed,
            vec![
                AcquireAndPlayStage::Importing,
                AcquireAndPlayStage::Indexing,
                AcquireAndPlayStage::Ready {
                    navidrome_song_id: Some("nd-ready-77".into()),
                    codec: Some("opus".into()),
                    is_lossy: true,
                    already_owned: false,
                    backend_id: Some("fast-net".into()),
                },
            ]
        );

        // Lossy fast acquisition was recorded...
        let fast = FastAcquisitionRow::find_matching_in(
            &db,
            &ctx.user_id,
            Some("nd-ready-77"),
            None,
            "Kowloon",
            "Wake Up",
        )
        .await
        .unwrap()
        .expect("fast acquisition row recorded");
        assert_eq!(fast.codec, "opus");
        assert_eq!(fast.is_lossy, 1);

        // ...but NO archival FLAC upgrade was scheduled yet (strictly On-Keep only!).
        assert!(ArchivalUpgradeRow::list_for_user_in(&db, &ctx.user_id, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn partition_backends_does_not_put_as_found_backends_ahead_of_soulseek() {
        static INTERNET_ARCHIVE: BackendMeta = BackendMeta {
            id: "internet_archive",
            name: "Internet Archive",
            service: "internet_archive",
            delivery: Delivery::AsFound,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };
        static FAST_LOSSY: BackendMeta = BackendMeta {
            id: "fast-lossy",
            name: "Fast Lossy",
            service: "fast-lossy",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let slskd: Searched = (
            DEFAULT_DOWNLOAD_BACKEND.into(),
            StubBackend::new(vec![]),
        );
        let archive: Searched = ("internet_archive".into(), StubBackend::new(vec![]));
        let fast: Searched = ("fast-lossy".into(), StubBackend::new(vec![]));

        // With only AsFound backends (Soulseek + Internet Archive), prefer_fast=true must NOT
        // isolate Internet Archive in fast_tier ahead of Soulseek.
        let (primary, fallback) = partition_backends(
            vec![slskd.clone(), archive.clone()],
            &[&shared::backends::SLSKD, &INTERNET_ARCHIVE],
            true,
        );
        assert_eq!(
            primary.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![DEFAULT_DOWNLOAD_BACKEND, "internet_archive"]
        );
        assert!(fallback.is_empty());

        // With a LossyOnly backend present, ONLY LossyOnly enters fast_tier when prefer_fast=true.
        let (primary, fallback) = partition_backends(
            vec![slskd, archive, fast],
            &[&shared::backends::SLSKD, &INTERNET_ARCHIVE, &FAST_LOSSY],
            true,
        );
        assert_eq!(
            primary.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["fast-lossy"]
        );
        assert_eq!(
            fallback.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![DEFAULT_DOWNLOAD_BACKEND, "internet_archive"]
        );
    }

    #[tokio::test]
    async fn unowned_track_is_blocked_when_importer_is_degraded() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "broken-imp-user").await;
        let ledger = Ledger::start(db.clone());
        let stub = StubBackend::new(vec![]);
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub);

        let err = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD],
            Some(Failure::new(
                FailureCode::ImporterBroken,
                Stage::Import,
                RetryAdvice::NeedsUser,
                "beets broken",
            )),
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::ImporterBroken);
    }

    #[tokio::test]
    async fn dismissed_ledger_slot_terminates_cleanly_without_hanging() {
        let db = test_db::migrated().await;
        let (ctx, folder, _dir) = seed_user(&db, "dismissed-user").await;
        let ledger = Ledger::start(db.clone());

        let job_id = "job-dismissed-1".to_string();
        let slot = JobSlot {
            job: job_id.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_id.clone(),
                    user: ctx.id(),
                    folder_id: folder.id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Downloading,
                    search_id: None,
                    picked: vec![(0, item_on("slskd", "peer", "Wake Up", "wake.flac"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Dismiss(vec![slot]));
        ledger.settled().await;

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let relay_rx = tx.subscribe();
        let mut client_rx = tx.subscribe();

        tokio::time::timeout(
            Duration::from_secs(5),
            relay_job_stages_until_terminal(
                &db,
                &ledger,
                &StubNavidrome(None),
                &ctx,
                &folder,
                relay_rx,
                StageRelayContext {
                    request_id: "req-dismissed".into(),
                    batch_id: "batch-dismissed".into(),
                    job_id,
                    artist: "Kowloon".into(),
                    title: "Wake Up".into(),
                    album: None,
                    recording_mbid: None,
                    duration_s: None,
                    backend_id: DEFAULT_DOWNLOAD_BACKEND.into(),
                    backend_lossy_only: false,
                    picked_quality: "FLAC".into(),
                },
            ),
        )
        .await
        .expect("dismissed slot must terminate promptly without hanging");

        let mut stages = Vec::new();
        while let Ok(ev) = client_rx.try_recv() {
            if let DownloadEvent::AcquireAndPlay(ap) = ev {
                if ap.request_id == "req-dismissed" {
                    stages.push(ap.stage);
                }
            }
        }
        assert!(matches!(
            stages.last(),
            Some(AcquireAndPlayStage::Failed { failure }) if failure.code == FailureCode::AlreadyRunning
        ));
    }

    #[tokio::test]
    async fn already_in_library_looks_up_existing_navidrome_song_without_recording_fast_acquisition()
    {
        let db = test_db::migrated().await;
        let (ctx, folder, _dir) = seed_user(&db, "already-lib-user").await;
        let ledger = Ledger::start(db.clone());

        let song = native_song(
            "nd-existing-55",
            "Kowloon",
            "Wake Up",
            200.0,
            Some("rec-mbid-55"),
        );
        let stub_nd = NavidromeStub::start();
        stub_nd.answer("/api/song", &[&format!("[{song}]")]);
        let navi = stub_nd.client();

        let job_id = "job-already-lib".to_string();
        let batch_id = "batch-already-lib".to_string();
        let slot = JobSlot {
            job: job_id.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_id.clone(),
                    user: ctx.id(),
                    folder_id: folder.id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Importing,
                    search_id: None,
                    picked: vec![(0, item_on("fast-net", "cdn", "Wake Up", "wake.opus"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let mut skipped_row =
            DownloadProgress::queued("t-55".into(), "cdn".into(), "wake.opus".into(), 100)
                .with_batch(batch_id.clone(), "Kowloon - Wake Up".into())
                .with_job(slot.clone())
                .with_state(DownloadState::ImportSkipped);
        skipped_row.skip = Some(shared::download::SkipKind::AlreadyInLibrary);
        ledger.record(JobEvent::Settled {
            state: JobState::Done,
            rows: vec![skipped_row.clone()],
            results: vec![(slot, SlotResult::AlreadyInLibrary { existing: vec![] })],
        });
        ledger.settled().await;

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let relay_rx = tx.subscribe();
        let mut client_rx = tx.subscribe();

        let relay_task = tokio::spawn({
            let db = db.clone();
            let ledger = ledger.clone();
            let ctx = ctx.clone();
            let folder = folder.clone();
            let navi = Arc::clone(&navi);
            async move {
                relay_job_stages_until_terminal(
                    &db,
                    &ledger,
                    &StubNavidrome(Some(navi)),
                    &ctx,
                    &folder,
                    relay_rx,
                    StageRelayContext {
                        request_id: "req-already-lib".into(),
                        batch_id,
                        job_id,
                        artist: "Kowloon".into(),
                        title: "Wake Up".into(),
                        album: Some("Come Over".into()),
                        recording_mbid: Some("rec-mbid-55".into()),
                        duration_s: Some(200),
                        backend_id: "fast-net".into(),
                        backend_lossy_only: true,
                        picked_quality: "OPUS 160".into(),
                    },
                )
                .await;
            }
        });

        tx.send(DownloadEvent::Progress(vec![skipped_row])).unwrap();
        relay_task.await.unwrap();

        let mut stages = Vec::new();
        while let Ok(ev) = client_rx.try_recv() {
            if let DownloadEvent::AcquireAndPlay(ap) = ev {
                if ap.request_id == "req-already-lib" {
                    stages.push(ap.stage);
                }
            }
        }
        assert_eq!(
            stages,
            vec![AcquireAndPlayStage::Ready {
                navidrome_song_id: Some("nd-existing-55".into()),
                codec: Some("flac".into()),
                is_lossy: false,
                already_owned: true,
                backend_id: None,
            }]
        );

        // Must NOT record a bogus fast_acquisitions entry for AlreadyInLibrary!
        assert!(FastAcquisitionRow::find_matching_in(
            &db,
            &ctx.user_id,
            Some("nd-existing-55"),
            None,
            "Kowloon",
            "Wake Up",
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn fast_tier_enqueue_failure_falls_back_to_soulseek_tier() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "fallback-enq-user").await;
        let ledger = Ledger::start(db.clone());

        static FAST_META: BackendMeta = BackendMeta {
            id: "fast-flaky",
            name: "Fast Flaky",
            service: "fast-flaky",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        // Fast tier finds a match, but enqueue fails!
        let fast_stub = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        let mut fast_item = item_on("fast-flaky", "cdn", "Wake Up", "wake.opus");
        fast_item.quality = "OPUS 160".into();
        let mut fast_group = group("cdn", vec![fast_item], 0.95);
        fast_group.quality = "OPUS 160".into();
        fast_stub.script_polls(vec![Ok(search_result(
            "search-fast",
            SearchState::Completed,
            vec![fast_group],
        ))]);

        // Fallback Soulseek tier succeeds on search and enqueue!
        let slskd_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let slskd_item = item_on(DEFAULT_DOWNLOAD_BACKEND, "peer_flac", "Wake Up", "wake.flac");
        let slskd_group = group("peer_flac", vec![slskd_item], 0.93);
        slskd_stub.script_polls(vec![Ok(search_result(
            "search-slskd",
            SearchState::Completed,
            vec![slskd_group],
        ))]);

        let backends = StubBackends::default()
            .with(DEFAULT_DOWNLOAD_BACKEND, slskd_stub.clone())
            .with("fast-flaky", fast_stub.clone());

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD, &FAST_META],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut fell_back_to_slskd = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(DownloadEvent::AcquireAndPlay(ap))) if ap.request_id == resp.request_id => {
                    if let AcquireAndPlayStage::Acquiring {
                        backend_id,
                        source: Some(src),
                        ..
                    } = ap.stage
                    {
                        if backend_id == DEFAULT_DOWNLOAD_BACKEND && src == "peer_flac" {
                            fell_back_to_slskd = true;
                            break;
                        }
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(
            fell_back_to_slskd,
            "expected fallback to Soulseek after fast-tier enqueue failed"
        );
        assert_eq!(fast_stub.specs().len(), 1);
        assert_eq!(slskd_stub.specs().len(), 1);
    }

    #[tokio::test]
    async fn lossy_fallback_never_skips_lossy_only_backends_even_when_prefer_fast_is_true() {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "never-lossy-user").await;
        let ledger = Ledger::start(db.clone());

        UserSettings::upsert_in(
            &db,
            &ctx.user_id,
            crate::models::user_settings::UpdateUserSettings {
                lossy_fallback: Some(LossyFallback::Never),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        static FAST_META: BackendMeta = BackendMeta {
            id: "fast-never",
            name: "Fast Never",
            service: "fast-never",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        let fast_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let slskd_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let slskd_item = item_on(DEFAULT_DOWNLOAD_BACKEND, "peer_flac", "Wake Up", "wake.flac");
        let slskd_group = group("peer_flac", vec![slskd_item], 0.94);
        slskd_stub.script_polls(vec![Ok(search_result(
            "search-slskd",
            SearchState::Completed,
            vec![slskd_group],
        ))]);

        let backends = StubBackends::default()
            .with(DEFAULT_DOWNLOAD_BACKEND, slskd_stub.clone())
            .with("fast-never", fast_stub.clone());

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD, &FAST_META],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        match &resp.status {
            AcquireAndPlayInitialStatus::Started { backend_ids, .. } => {
                assert_eq!(backend_ids, &[DEFAULT_DOWNLOAD_BACKEND]);
            }
            other => panic!("expected Started, got {other:?}"),
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut picked_slskd = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(DownloadEvent::AcquireAndPlay(ap))) if ap.request_id == resp.request_id => {
                    if let AcquireAndPlayStage::Acquiring {
                        backend_id,
                        source: Some(src),
                        ..
                    } = ap.stage
                    {
                        assert_eq!(backend_id, DEFAULT_DOWNLOAD_BACKEND);
                        assert_eq!(src, "peer_flac");
                        picked_slskd = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(picked_slskd, "expected Soulseek to be picked when lossy_fallback is Never");
        assert!(
            fast_stub.specs().is_empty(),
            "LossyOnly backend must never be searched when user set lossy_fallback = Never"
        );
        assert_eq!(slskd_stub.specs().len(), 1);
    }

    #[tokio::test]
    async fn lossy_only_enqueue_uses_skip_duplicate_rule() {
        let db = test_db::migrated().await;
        let (ctx, _folder, dir) = seed_user(&db, "lossy-dup-skip-user").await;
        let ledger = Ledger::start(db.clone());

        static FAST_META: BackendMeta = BackendMeta {
            id: "fast-skip-rule",
            name: "Fast Skip Rule",
            service: "fast-skip-rule",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let fast_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = fast_stub.hold_downloads();
        let mut fast_item = item_on("fast-skip-rule", "cdn-skip", "Wake Up", "wake-skip.opus");
        fast_item.quality = "OPUS 160".into();
        let mut fast_group = group("cdn-skip", vec![fast_item], 0.95);
        fast_group.quality = "OPUS 160".into();
        fast_stub.script_polls(vec![Ok(search_result(
            "search-fast-skip",
            SearchState::Completed,
            vec![fast_group],
        ))]);

        let slskd_stub = StubBackend::new(vec![]);
        let backends = StubBackends::default()
            .with(DEFAULT_DOWNLOAD_BACKEND, slskd_stub)
            .with("fast-skip-rule", fast_stub.clone());

        let _resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD, &FAST_META],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: true,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(10), reached.notified())
            .await
            .expect("fast backend download should be invoked");

        let fast_dyn: Arc<dyn soulbeet::DownloadBackend> = fast_stub.clone();
        let key = crate::server_fns::download::test_support::key(
            &fast_dyn,
            "cdn-skip",
            "wake-skip.opus",
        );
        let probe = crate::transfer_registry::BatchRef::new(
            crate::transfer_registry::TransferOwner {
                user_id: ctx.id(),
                batch_id: "probe-skip".into(),
            },
            "probe".into(),
            dir.path().to_path_buf(),
            ImportMode::Singleton,
            "fast-skip-rule".into(),
        );
        let claim = crate::transfer_registry::TransferRegistry::global().claim(&[key], &probe);
        release.notify_waiters();

        assert_eq!(claim.mine.len(), 1);
        assert_eq!(claim.mine[0].1.duplicates, DuplicateRule::Skip);
    }

    #[tokio::test]
    async fn lossy_fallback_ask_with_prefer_fast_false_prompts_manual_choice_instead_of_auto_picking_lossy()
    {
        let db = test_db::migrated().await;
        let (ctx, _folder, _dir) = seed_user(&db, "ask-no-fast-user").await;
        let ledger = Ledger::start(db.clone());

        // Default user_settings.lossy_fallback is Ask.
        let settings = UserSettings::get_in(&db, &ctx.user_id).await.unwrap();
        assert_eq!(settings.lossy_fallback, LossyFallback::Ask);

        static FAST_META: BackendMeta = BackendMeta {
            id: "fast-ask",
            name: "Fast Ask",
            service: "fast-ask",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            about: &[],
            link: None,
            attribution: None,
            blocked: None,
        };

        let tx = get_or_create_user_channel(&ctx.id()).await;
        let mut rx = tx.subscribe();

        // Soulseek finds nothing; lossy-only fallback finds a high-scoring match.
        let slskd_stub = StubBackend::new(vec![]);
        slskd_stub.script_polls(vec![Ok(search_result(
            "search-slskd-empty",
            SearchState::Completed,
            vec![],
        ))]);

        let fast_stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let mut fast_item = item_on("fast-ask", "cdn-ask", "Wake Up", "wake.opus");
        fast_item.quality = "OPUS 160".into();
        let mut fast_group = group("cdn-ask", vec![fast_item], 0.95);
        fast_group.quality = "OPUS 160".into();
        fast_stub.script_polls(vec![Ok(search_result(
            "search-fast-ask",
            SearchState::Completed,
            vec![fast_group],
        ))]);

        let backends = StubBackends::default()
            .with(DEFAULT_DOWNLOAD_BACKEND, slskd_stub.clone())
            .with("fast-ask", fast_stub.clone());

        let resp = acquire_and_play_in(
            &db,
            &ledger,
            Arc::new(StubNavidrome(None)),
            &backends,
            &[&shared::backends::SLSKD, &FAST_META],
            None,
            &ctx,
            AcquireAndPlayRequest {
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                release_mbid: None,
                duration_s: None,
                folder_id: None,
                prefer_fast: false,
            },
        )
        .await
        .unwrap();

        match &resp.status {
            AcquireAndPlayInitialStatus::Started { backend_ids, .. } => {
                assert_eq!(backend_ids, &[DEFAULT_DOWNLOAD_BACKEND, "fast-ask"]);
            }
            other => panic!("expected Started, got {other:?}"),
        }

        let mut saw_manual_choice = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(DownloadEvent::AcquireAndPlay(ap))) if ap.request_id == resp.request_id => {
                    if let AcquireAndPlayStage::NeedsManualChoice {
                        results,
                        best_score,
                        threshold,
                        ..
                    } = ap.stage
                    {
                        assert!(best_score >= threshold);
                        assert_eq!(results.len(), 1);
                        saw_manual_choice = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw_manual_choice,
            "expected NeedsManualChoice instead of silently auto-picking lossy when Ask + prefer_fast=false"
        );
        assert!(
            fast_stub.requests().is_empty(),
            "LossyOnly backend must not be auto-enqueued when Ask + prefer_fast=false"
        );
    }
}
