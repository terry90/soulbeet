//! On-Keep-only background archival FLAC upgrades.
//!
//! Tracks acquired through a fast lossy path are recorded in `fast_acquisitions`
//! without scheduling any archival replacement. Only when a user explicitly keeps,
//! stars, or promotes the track does Soulbeet enqueue a low-priority background
//! upgrade job that hunts for a lossless (FLAC) copy on the archival backend
//! (Soulseek by default) and imports it into the library with guarded replacement,
//! preserving Navidrome track identity, ratings, and playlist memberships.

use std::sync::Arc;
use std::time::Duration;

use shared::download::{
    is_lossless_format, is_lossy_format, ArchivalUpgradeJob, ArchivalUpgradeOutcome,
    ArchivalUpgradeStatus, DownloadQuery, DownloadableGroup, DownloadableItem, EnqueueStatus,
    SearchPriority, TriggerArchivalUpgradeRequest,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::Track;
use shared::navidrome::DiscoveryTrack;
use soulbeet::{audio_duration_secs, ImportMode, VerifyPolicy};
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::auth::{Role, UserCtx};
use crate::core::acquire::{
    enqueue_and_watch_with_durations, numbered, prepare_target, Acquisition, NewBatch,
    AUTO_SELECT_SCORE_THRESHOLD,
};
use crate::core::database_failure;
use crate::core::folders;
use crate::core::sources;
use crate::ledger::{
    job_state, store, DuplicateRule, JobKind, JobState, Ledger, NewJob, NewSearch, SlotResult,
};
use crate::models::archival_upgrade::{
    ArchivalUpgradeRow, FastAcquisitionRow, NewArchivalUpgrade, NewFastAcquisition,
};
use crate::models::folder::Folder;
use crate::models::user::User;
use crate::server_fns::download::enqueue::not_enqueued_failure;
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::{Backends, DEFAULT_DOWNLOAD_BACKEND};

/// Maximum background search/enqueue attempts for one archival upgrade job.
pub const MAX_UPGRADE_ATTEMPTS: i64 = 3;

/// Background polling interval for low-priority archival FLAC upgrades.
pub const ARCHIVAL_WORKER_INTERVAL: Duration = Duration::from_secs(120);

/// Records a fast-acquired track so a future Keep/Star/Promote knows its codec
/// and backend. Does NOT schedule an archival upgrade on its own (On-Keep only).
pub async fn record_fast_acquisition_in(
    db: &SqlitePool,
    item: NewFastAcquisition<'_>,
) -> Result<FastAcquisitionRow, Failure> {
    FastAcquisitionRow::insert_in(db, item)
        .await
        .map_err(|e| database_failure(Stage::Import, e))
}

/// Evaluates whether a newly promoted Discovery track was fast-acquired lossy and,
/// if so, schedules a low-priority background archival FLAC upgrade.
pub async fn maybe_enqueue_for_promoted_discovery_in(
    db: &SqlitePool,
    user_id: &str,
    target_folder_id: &str,
    track: &DiscoveryTrack,
) -> Result<ArchivalUpgradeOutcome, Failure> {
    let ctx = UserCtx {
        user_id: user_id.to_string(),
        username: String::new(),
        role: Role::User,
    };
    let path = std::path::PathBuf::from(&track.path);
    let duration_s = tokio::task::spawn_blocking(move || audio_duration_secs(&path))
        .await
        .ok()
        .flatten();
    let req = TriggerArchivalUpgradeRequest {
        navidrome_song_id: track.song_id.clone(),
        discovery_track_id: Some(track.id.clone()),
        artist: track.artist.clone(),
        title: track.title.clone(),
        album: Some(track.album.clone()).filter(|a| !a.trim().is_empty()),
        recording_mbid: None,
        folder_id: Some(target_folder_id.to_string()),
        codec: None,
        duration_s,
        trigger_reason: "promote".to_string(),
    };
    enqueue_on_keep_in(db, &ctx, &req).await
}

async fn has_pending_discovery_track(
    db: &SqlitePool,
    user_id: &str,
    discovery_track_id: Option<&str>,
    navidrome_song_id: Option<&str>,
) -> Result<bool, String> {
    if let Some(dt_id) = discovery_track_id.map(str::trim).filter(|s| !s.is_empty()) {
        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM discovery_tracks t \
             JOIN folders f ON f.id = t.folder_id \
             WHERE f.user_id = ? AND t.id = ? AND t.status = 'Pending' \
             LIMIT 1",
        )
        .bind(user_id)
        .bind(dt_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        if pending.is_some() {
            return Ok(true);
        }
    }
    if let Some(song_id) = navidrome_song_id.map(str::trim).filter(|s| !s.is_empty()) {
        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM discovery_tracks t \
             JOIN folders f ON f.id = t.folder_id \
             WHERE f.user_id = ? AND t.song_id = ? AND t.status = 'Pending' \
             LIMIT 1",
        )
        .bind(user_id)
        .bind(song_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        if pending.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Enforces the **On-Keep-Only** archival FLAC upgrade policy.
///
/// Only explicit keep actions (`"keep"`, `"star"`, `"promote"`, `"manual"`) on a
/// lossy fast-acquired track enqueue a background archival FLAC upgrade job.
pub async fn enqueue_on_keep_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    req: &TriggerArchivalUpgradeRequest,
) -> Result<ArchivalUpgradeOutcome, Failure> {
    let reason = req.trigger_reason.trim().to_ascii_lowercase();
    if !matches!(reason.as_str(), "keep" | "star" | "promote" | "manual") {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!(
                "Archival FLAC upgrades only trigger on keep, star, promote, or manual request (got '{}')",
                req.trigger_reason
            ),
        ));
    }

    let artist = req.artist.trim();
    let title = req.title.trim();
    if artist.is_empty() || title.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Both artist and title are required to trigger an archival upgrade",
        ));
    }

    let requested_folder_id = req
        .folder_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(folder_id) = requested_folder_id {
        folders::owned_in(db, ctx, folder_id).await?;
    }

    // Never allow upgrading a discovery track that is still Pending (unpromoted).
    if has_pending_discovery_track(
        db,
        &ctx.user_id,
        req.discovery_track_id.as_deref(),
        req.navidrome_song_id.as_deref(),
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?
    {
        return Ok(ArchivalUpgradeOutcome::SkippedNotKept);
    }

    // Look up whether this track came from a fast acquisition.
    let fast = FastAcquisitionRow::find_matching_in(
        db,
        &ctx.user_id,
        req.navidrome_song_id.as_deref(),
        req.discovery_track_id.as_deref(),
        artist,
        title,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?;

    let (codec, backend_id, folder_id, recording_mbid, album, original_duration_s) = match fast {
        Some(record) => {
            // Re-check pending discovery status using IDs stored on the fast acquisition record too.
            if has_pending_discovery_track(
                db,
                &ctx.user_id,
                record.discovery_track_id.as_deref(),
                record.navidrome_song_id.as_deref(),
            )
            .await
            .map_err(|e| database_failure(Stage::Input, e))?
            {
                return Ok(ArchivalUpgradeOutcome::SkippedNotKept);
            }
            if is_lossless_format(&record.codec) {
                return Ok(ArchivalUpgradeOutcome::AlreadyLossless {
                    codec: record.codec,
                });
            }
            if record.is_lossy == 0 || !is_lossy_format(&record.codec) {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            }
            if record.imported_as_is != 0 {
                return Ok(ArchivalUpgradeOutcome::SkippedUnmatched);
            }
            let folder_id = requested_folder_id
                .map(str::to_string)
                .unwrap_or_else(|| record.folder_id.clone());
            let fast_duration_s = record
                .duration_s
                .and_then(|s| u32::try_from(s).ok())
                .filter(|&s| s > 0);
            (
                record.codec,
                record.backend_id,
                folder_id,
                req.recording_mbid.clone().or(record.recording_mbid),
                req.album.clone().or(record.album),
                req.duration_s.filter(|&s| s > 0).or(fast_duration_s),
            )
        }
        None => {
            // If no fast_acquisitions record exists, only allow an explicit lossy codec
            // on a manual/keep request when the caller also identifies an owned target folder.
            let Some(codec) = req.codec.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            };
            if is_lossless_format(codec) {
                return Ok(ArchivalUpgradeOutcome::AlreadyLossless {
                    codec: codec.to_string(),
                });
            }
            if !is_lossy_format(codec) {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            }
            let Some(folder_id) = requested_folder_id.map(str::to_string) else {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            };
            (
                codec.to_string(),
                "fast_path".to_string(),
                folder_id,
                req.recording_mbid.clone(),
                req.album.clone(),
                req.duration_s.filter(|&s| s > 0),
            )
        }
    };

    // Never auto-upgrade tracks without a MusicBrainz recording ID so bootlegs,
    // live cuts, or custom edits cannot be silently overwritten by a studio FLAC.
    let recording_mbid = recording_mbid
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let Some(recording_mbid) = recording_mbid else {
        return Ok(ArchivalUpgradeOutcome::SkippedUnmatched);
    };

    // Always verify folder ownership before inserting to prevent FK errors or cross-user targeting.
    folders::owned_in(db, ctx, &folder_id).await?;

    if let Some(existing) = ArchivalUpgradeRow::find_active_or_completed_in(
        db,
        &ctx.user_id,
        req.navidrome_song_id.as_deref(),
        artist,
        title,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?
    {
        return Ok(ArchivalUpgradeOutcome::AlreadyScheduled {
            upgrade_id: existing.id,
        });
    }

    let inserted = match ArchivalUpgradeRow::insert_in(
        db,
        NewArchivalUpgrade {
            user_id: &ctx.user_id,
            folder_id: &folder_id,
            navidrome_song_id: req.navidrome_song_id.as_deref(),
            artist,
            title,
            album: album.as_deref(),
            recording_mbid: Some(&recording_mbid),
            original_codec: &codec,
            original_backend: &backend_id,
            original_duration_s,
            trigger_reason: &reason,
        },
    )
    .await
    {
        Ok(row) => row,
        Err(err) => {
            if let Ok(Some(existing)) = ArchivalUpgradeRow::find_active_or_completed_in(
                db,
                &ctx.user_id,
                req.navidrome_song_id.as_deref(),
                artist,
                title,
            )
            .await
            {
                return Ok(ArchivalUpgradeOutcome::AlreadyScheduled {
                    upgrade_id: existing.id,
                });
            }
            return Err(database_failure(Stage::Input, err));
        }
    };

    info!(
        "Queued archival FLAC upgrade {} for '{} - {}' (was {} from {}, reason: {})",
        inserted.id, inserted.artist, inserted.title, inserted.original_codec, inserted.original_backend, reason
    );

    Ok(ArchivalUpgradeOutcome::Enqueued {
        job: Box::new(inserted.into()),
    })
}

/// Lists archival FLAC upgrade jobs for `ctx`, newest first.
pub async fn list_for_user_in(
    db: &SqlitePool,
    ctx: &UserCtx,
) -> Result<Vec<ArchivalUpgradeJob>, Failure> {
    let rows = ArchivalUpgradeRow::list_for_user_in(db, &ctx.user_id, 50)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(rows.into_iter().map(Into::into).collect())
}

fn item_is_lossless(item: &DownloadableItem) -> bool {
    match item
        .facts
        .extension
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        Some(ext) => is_lossless_format(ext) && !is_lossy_format(&item.quality),
        None => is_lossless_format(&item.quality),
    }
}

/// Picks the best lossless group scoring at least `AUTO_SELECT_SCORE_THRESHOLD`.
/// Requires `g.items` non-empty and EVERY item's extension/quality to explicitly satisfy
/// `is_lossless_format(...)`, rejecting empty strings, unknown codecs, and mixed groups.
pub fn pick_lossless_group(
    groups: Vec<DownloadableGroup>,
) -> Option<(DownloadableGroup, Vec<DownloadableGroup>)> {
    let mut lossless: Vec<DownloadableGroup> = groups
        .into_iter()
        .filter(|g| {
            !g.items.is_empty()
                && g.score >= AUTO_SELECT_SCORE_THRESHOLD
                && g.items.iter().all(item_is_lossless)
        })
        .collect();
    lossless.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if lossless.is_empty() {
        return None;
    }
    let best = lossless.remove(0);
    Some((best, lossless))
}

/// Reconciles all `status = 'downloading'` archival upgrade jobs against their ledger jobs.
///
/// - When the ledger job reaches `JobState::Done` → marks `Completed` and updates matching
///   `fast_acquisitions` rows to lossless so future keep checks know the track is upgraded.
/// - When the ledger job reaches `Failed`, `Cancelled`, `NeedsDecision`, or `Dismissed` →
///   increments `attempts` and marks `Queued` if `attempts < MAX_UPGRADE_ATTEMPTS`, else `Failed`.
pub async fn reconcile_downloading_upgrades_in(db: &SqlitePool) -> Result<usize, Failure> {
    let downloading = ArchivalUpgradeRow::list_downloading_in(db)
        .await
        .map_err(|e| database_failure(Stage::Index, e))?;
    let mut reconciled = 0;

    for row in downloading {
        let Some(ref ledger_job_id) = row.job_id else {
            let reason = "Archival upgrade is downloading without a ledger job_id";
            let next_attempts = row.attempts + 1;
            let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
                ArchivalUpgradeStatus::Failed
            } else {
                ArchivalUpgradeStatus::Queued
            };
            ArchivalUpgradeRow::mark_status_in(db, &row.id, next_status, None, 1, Some(reason))
                .await
                .map_err(|e| database_failure(Stage::Index, e))?;
            reconciled += 1;
            continue;
        };

        let tracks = store::tracks_of(db, ledger_job_id)
            .await
            .map_err(|e| database_failure(Stage::Index, e))?;

        let (state, fallback_reason) = if !tracks.is_empty() {
            let slot_states: Vec<JobState> = tracks.iter().map(|t| t.state).collect();
            let reason = tracks
                .iter()
                .find_map(|t| t.row.as_ref().and_then(|r| r.failure.as_ref()))
                .map(|f| f.message.clone());
            (job_state(&slot_states), reason)
        } else {
            let job_row: Option<(String, Option<String>)> =
                sqlx::query_as("SELECT state, failure FROM jobs WHERE id = ?")
                    .bind(ledger_job_id)
                    .fetch_optional(db)
                    .await
                    .map_err(|e| database_failure(Stage::Index, e))?;
            match job_row {
                Some((state_str, failure_json)) => {
                    let parsed = JobState::parse(&state_str).unwrap_or(JobState::Failed);
                    let reason = failure_json
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<Failure>(j).ok())
                        .map(|f| f.message);
                    (parsed, reason)
                }
                None => (
                    JobState::Failed,
                    Some(format!("Ledger job '{ledger_job_id}' no longer exists")),
                ),
            }
        };

        match state {
            JobState::Done => {
                ArchivalUpgradeRow::mark_status_in(
                    db,
                    &row.id,
                    ArchivalUpgradeStatus::Completed,
                    None,
                    0,
                    None,
                )
                .await
                .map_err(|e| database_failure(Stage::Index, e))?;

                let (new_song_id, listed_codec) = tracks
                    .into_iter()
                    .find_map(|t| match t.result {
                        Some(SlotResult::Imported {
                            listed: Some(songs),
                            ..
                        }) => songs
                            .into_iter()
                            .next()
                            .map(|s| (Some(s.song_id), s.format)),
                        _ => None,
                    })
                    .unwrap_or((None, None));
                let upgraded_codec = listed_codec
                    .as_deref()
                    .filter(|c| is_lossless_format(c))
                    .unwrap_or("flac");
                if let Err(e) = FastAcquisitionRow::mark_upgraded_in(
                    db,
                    &row.user_id,
                    row.navidrome_song_id.as_deref(),
                    &row.artist,
                    &row.title,
                    upgraded_codec,
                    new_song_id.as_deref(),
                )
                .await
                {
                    warn!(
                        "Failed to update fast_acquisitions after archival upgrade {}: {e}",
                        row.id
                    );
                }
                info!(
                    "Archival FLAC upgrade {} completed for '{} - {}'",
                    row.id, row.artist, row.title
                );
                reconciled += 1;
            }
            JobState::Failed
            | JobState::Cancelled
            | JobState::NeedsDecision
            | JobState::Dismissed => {
                let reason = fallback_reason.unwrap_or_else(|| {
                    format!("Ledger job '{ledger_job_id}' ended in state {}", state.as_str())
                });
                let next_attempts = row.attempts + 1;
                let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
                    ArchivalUpgradeStatus::Failed
                } else {
                    ArchivalUpgradeStatus::Queued
                };
                ArchivalUpgradeRow::mark_status_in(
                    db,
                    &row.id,
                    next_status,
                    None,
                    1,
                    Some(&reason),
                )
                .await
                .map_err(|e| database_failure(Stage::Index, e))?;
                warn!(
                    "Archival FLAC upgrade {} ({} - {}) {} after ledger state {}: {}",
                    row.id,
                    row.artist,
                    row.title,
                    next_status.as_str(),
                    state.as_str(),
                    reason
                );
                reconciled += 1;
            }
            _ => {}
        }
    }

    Ok(reconciled)
}

/// Outcome of one background archival worker tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerTickOutcome {
    Idle,
    Enqueued { upgrade_id: String, job_id: String },
    Deferred { upgrade_id: String, reason: String },
    Failed { upgrade_id: String, reason: String },
}

/// Processes the oldest queued archival FLAC upgrade job using `backends` (looking up
/// `archival_backend_id`, which defaults to Soulseek / `DEFAULT_DOWNLOAD_BACKEND`).
pub async fn process_next_upgrade_in(
    db: &SqlitePool,
    ledger: &Ledger,
    backends: &dyn Backends,
    archival_backend_id: &str,
) -> Result<WorkerTickOutcome, Failure> {
    reconcile_downloading_upgrades_in(db).await?;

    let Some(upgrade) = ArchivalUpgradeRow::next_queued_in(db, MAX_UPGRADE_ATTEMPTS)
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?
    else {
        return Ok(WorkerTickOutcome::Idle);
    };

    let backend = match backends.get(archival_backend_id).await {
        Ok(b) => b,
        Err(e) => {
            // Do not burn retry attempts on transient backend lookup/connectivity downtime;
            // keep the upgrade queued and defer until the archival backend comes back.
            let reason = format!("Archival backend '{archival_backend_id}' unavailable: {e}");
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                ArchivalUpgradeStatus::Queued,
                None,
                0,
                Some(&reason),
            )
            .await;
            return Ok(WorkerTickOutcome::Deferred {
                upgrade_id: upgrade.id,
                reason,
            });
        }
    };

    let Some(user) = User::find_by_id_in(db, &upgrade.user_id)
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?
    else {
        let _ = ArchivalUpgradeRow::mark_status_in(
            db,
            &upgrade.id,
            ArchivalUpgradeStatus::Failed,
            None,
            1,
            Some("User no longer exists"),
        )
        .await;
        return Ok(WorkerTickOutcome::Failed {
            upgrade_id: upgrade.id,
            reason: "User no longer exists".to_string(),
        });
    };

    let ctx = UserCtx {
        user_id: user.id.clone(),
        username: user.username.clone(),
        role: user.role,
    };

    let folder: Folder = match folders::owned_in(db, &ctx, &upgrade.folder_id).await {
        Ok(f) => f,
        Err(f) => {
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                ArchivalUpgradeStatus::Failed,
                None,
                1,
                Some(&f.message),
            )
            .await;
            return Ok(WorkerTickOutcome::Failed {
                upgrade_id: upgrade.id,
                reason: f.message,
            });
        }
    };

    // Defense in depth: never search/overwrite a queued upgrade row that lacks a recording MBID.
    let verified_mbid = upgrade
        .recording_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(verified_mbid) = verified_mbid else {
        let reason =
            "Skipped archival upgrade: track lacks a verified MusicBrainz recording ID".to_string();
        let _ = ArchivalUpgradeRow::mark_status_in(
            db,
            &upgrade.id,
            ArchivalUpgradeStatus::Failed,
            None,
            MAX_UPGRADE_ATTEMPTS,
            Some(&reason),
        )
        .await;
        return Ok(WorkerTickOutcome::Failed {
            upgrade_id: upgrade.id,
            reason,
        });
    };

    let _ = ArchivalUpgradeRow::mark_status_in(
        db,
        &upgrade.id,
        ArchivalUpgradeStatus::Searching,
        None,
        0,
        None,
    )
    .await;

    let formatted_duration = upgrade
        .original_duration_s
        .and_then(|s| u32::try_from(s).ok())
        .filter(|&s| s > 0)
        .map(|s| format!("{}:{:02}", s / 60, s % 60));

    let track = Track {
        id: verified_mbid.clone(),
        title: upgrade.title.clone(),
        artist: upgrade.artist.clone(),
        album_id: None,
        album_title: upgrade.album.clone(),
        release_date: None,
        duration: formatted_duration,
        mbid: Some(verified_mbid),
        release_mbid: None,
    };
    let query = DownloadQuery::new(vec![track.clone()]).backend(archival_backend_id);
    let searched = (archival_backend_id.to_string(), backend.clone());

    let found = sources::search_tier_with_priority(
        std::slice::from_ref(&searched),
        None,
        &[track],
        SearchPriority::Background,
    )
    .await;
    if found.groups.is_empty() {
        let failure = found.failure();
        let next_attempts = upgrade.attempts + 1;
        let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
            ArchivalUpgradeStatus::Failed
        } else {
            ArchivalUpgradeStatus::Queued
        };
        let _ = ArchivalUpgradeRow::mark_status_in(
            db,
            &upgrade.id,
            next_status,
            None,
            1,
            Some(&failure.message),
        )
        .await;
        return Ok(if next_status == ArchivalUpgradeStatus::Failed {
            WorkerTickOutcome::Failed {
                upgrade_id: upgrade.id,
                reason: failure.message,
            }
        } else {
            WorkerTickOutcome::Deferred {
                upgrade_id: upgrade.id,
                reason: failure.message,
            }
        });
    }
    let groups = found.groups;

    let Some((picked, alternates)) = pick_lossless_group(groups) else {
        let reason = "No lossless (FLAC) candidate met the quality threshold".to_string();
        let next_attempts = upgrade.attempts + 1;
        let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
            ArchivalUpgradeStatus::Failed
        } else {
            ArchivalUpgradeStatus::Queued
        };
        let _ = ArchivalUpgradeRow::mark_status_in(
            db,
            &upgrade.id,
            next_status,
            None,
            1,
            Some(&reason),
        )
        .await;
        return Ok(if next_status == ArchivalUpgradeStatus::Failed {
            WorkerTickOutcome::Failed {
                upgrade_id: upgrade.id,
                reason,
            }
        } else {
            WorkerTickOutcome::Deferred {
                upgrade_id: upgrade.id,
                reason,
            }
        });
    };

    let target = match prepare_target(&folder).await {
        Ok(t) => t,
        Err(failure) => {
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                ArchivalUpgradeStatus::Failed,
                None,
                1,
                Some(&failure.message),
            )
            .await;
            return Ok(WorkerTickOutcome::Failed {
                upgrade_id: upgrade.id,
                reason: failure.message,
            });
        }
    };

    let job_id = uuid::Uuid::new_v4().to_string();
    let batch_id = uuid::Uuid::new_v4().to_string();
    let search_id = uuid::Uuid::new_v4().to_string();
    let picked_items = numbered(picked.items);

    if let Err(e) = ledger
        .create(
            NewJob {
                id: job_id.clone(),
                user: ctx.id(),
                folder_id: folder.id.clone(),
                wanted: query.clone(),
                import_mode: ImportMode::Singleton,
                state: JobState::Queued,
                search_id: Some(search_id.clone()),
                picked: picked_items.clone(),
                kind: JobKind::Track,
                options: None,
            },
            Some(NewSearch {
                id: search_id,
                user: ctx.id(),
                backend: Some(archival_backend_id.to_string()),
                query,
                spec: None,
            }),
        )
        .await
    {
        let failure = database_failure(Stage::Enqueue, e);
        let next_attempts = upgrade.attempts + 1;
        let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
            ArchivalUpgradeStatus::Failed
        } else {
            ArchivalUpgradeStatus::Queued
        };
        let _ = ArchivalUpgradeRow::mark_status_in(
            db,
            &upgrade.id,
            next_status,
            None,
            1,
            Some(&failure.message),
        )
        .await;
        return Err(failure);
    }

    let original_duration_s = upgrade
        .original_duration_s
        .and_then(|s| u32::try_from(s).ok())
        .filter(|&s| s > 0);
    let expected_durations: std::collections::HashMap<u32, u32> = original_duration_s
        .into_iter()
        .flat_map(|dur| picked_items.iter().map(move |(slot, _)| (*slot, dur)))
        .collect();

    let fresh = NewBatch {
        batch_id,
        label: format!("Archival FLAC: {} - {}", upgrade.artist, upgrade.title),
        backend: archival_backend_id.to_string(),
        target,
        import_mode: ImportMode::Singleton,
        duplicates: DuplicateRule::Guarded,
        verify_policy: VerifyPolicy::ArchivalLossless,
    };
    let acquisition = Acquisition {
        job: job_id.clone(),
        items: picked_items,
        alternates,
        max_sources: MAX_SOURCES_PER_TRACK,
    };

    match enqueue_and_watch_with_durations(
        ledger,
        &ctx,
        Arc::clone(&backend),
        fresh,
        acquisition,
        expected_durations,
    )
    .await
    {
        Ok(results) if results.iter().any(|d| d.status == EnqueueStatus::Enqueued) => {
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                ArchivalUpgradeStatus::Downloading,
                Some(&job_id),
                0,
                None,
            )
            .await;
            info!(
                "Started archival FLAC upgrade {} (job {}) for '{} - {}'",
                upgrade.id, job_id, upgrade.artist, upgrade.title
            );
            Ok(WorkerTickOutcome::Enqueued {
                upgrade_id: upgrade.id,
                job_id,
            })
        }
        Ok(results) => {
            let failure = not_enqueued_failure(&results);
            ledger.record(crate::ledger::JobEvent::Ended {
                job: job_id.clone(),
                state: JobState::Failed,
                failure: Some(failure.clone()),
            });
            let next_attempts = upgrade.attempts + 1;
            let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
                ArchivalUpgradeStatus::Failed
            } else {
                ArchivalUpgradeStatus::Queued
            };
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                next_status,
                Some(&job_id),
                1,
                Some(&failure.message),
            )
            .await;
            Ok(if next_status == ArchivalUpgradeStatus::Failed {
                WorkerTickOutcome::Failed {
                    upgrade_id: upgrade.id,
                    reason: failure.message,
                }
            } else {
                WorkerTickOutcome::Deferred {
                    upgrade_id: upgrade.id,
                    reason: failure.message,
                }
            })
        }
        Err(failure) => {
            ledger.record(crate::ledger::JobEvent::Ended {
                job: job_id.clone(),
                state: JobState::Failed,
                failure: Some(failure.clone()),
            });
            let next_attempts = upgrade.attempts + 1;
            let next_status = if next_attempts >= MAX_UPGRADE_ATTEMPTS {
                ArchivalUpgradeStatus::Failed
            } else {
                ArchivalUpgradeStatus::Queued
            };
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                next_status,
                Some(&job_id),
                1,
                Some(&failure.message),
            )
            .await;
            Ok(if next_status == ArchivalUpgradeStatus::Failed {
                WorkerTickOutcome::Failed {
                    upgrade_id: upgrade.id,
                    reason: failure.message,
                }
            } else {
                WorkerTickOutcome::Deferred {
                    upgrade_id: upgrade.id,
                    reason: failure.message,
                }
            })
        }
    }
}

/// Runs one low-priority tick against the live database, ledger, and backend registry.
pub async fn run_archival_worker_tick() {
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        debug!(
            "Archival FLAC worker: skipping tick while importer is degraded ({})",
            failure.message
        );
        return;
    }
    match process_next_upgrade_in(
        &crate::db::DB,
        &crate::ledger::LEDGER,
        &crate::services::Registry,
        DEFAULT_DOWNLOAD_BACKEND,
    )
    .await
    {
        Ok(WorkerTickOutcome::Idle) => debug!("Archival FLAC worker: queue empty"),
        Ok(outcome) => info!("Archival FLAC worker tick: {outcome:?}"),
        Err(failure) => warn!("Archival FLAC worker error: {}", crate::core::logged(&failure)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::library::SongFacts;
    use crate::db::test_db;
    use crate::ledger::JobEvent;
    use crate::server_fns::download::test_support::{
        group, item_on, search_result, ScriptedOutcome, StubBackend, StubBackends,
    };
    use shared::download::{DownloadProgress, DownloadState, IndexState, JobSlot, SearchState};
    use shared::navidrome::DiscoveryStatus;
    use soulbeet::DownloadBackend;

    async fn seed_user_and_folder(db: &SqlitePool) -> (UserCtx, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let user_id = "archival-u1".to_string();
        let folder_id = "archival-f1".to_string();
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, 'alice', 'x')")
            .bind(&user_id)
            .execute(db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'Music', ?)")
            .bind(&folder_id)
            .bind(&user_id)
            .bind(dir.path().to_str().unwrap())
            .execute(db)
            .await
            .unwrap();
        let ctx = UserCtx {
            user_id,
            username: "alice".into(),
            role: Role::User,
        };
        (ctx, folder_id, dir)
    }

    #[tokio::test]
    async fn recording_a_lossy_fast_acquisition_never_schedules_an_upgrade_until_kept() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-opus-1"),
                discovery_track_id: Some("disc-1"),
                artist: "Kowloon",
                title: "Wake Up",
                album: Some("Come Over"),
                recording_mbid: Some("mbid-1"),
                backend_id: "fast-archive",
                codec: "opus",
                is_lossy: true,
                duration_s: Some(235),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        // Still zero archival upgrade jobs before explicit Keep/Promote!
        assert!(ArchivalUpgradeRow::list_for_user_in(&db, &ctx.user_id, 10)
            .await
            .unwrap()
            .is_empty());

        // Non-keep trigger reasons are rejected.
        let rejected = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-opus-1".into()),
                discovery_track_id: Some("disc-1".into()),
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
                duration_s: None,
                trigger_reason: "preview".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.code, FailureCode::InvalidInput);

        // Explicit Keep schedules the archival FLAC upgrade once.
        let keep_req = TriggerArchivalUpgradeRequest {
            navidrome_song_id: Some("song-opus-1".into()),
            discovery_track_id: Some("disc-1".into()),
            artist: "Kowloon".into(),
            title: "Wake Up".into(),
            album: Some("Come Over".into()),
            recording_mbid: Some("mbid-1".into()),
            folder_id: Some(folder_id.clone()),
            codec: None,
            duration_s: None,
            trigger_reason: "keep".into(),
        };
        let first = enqueue_on_keep_in(&db, &ctx, &keep_req).await.unwrap();
        let upgrade_id = match first {
            ArchivalUpgradeOutcome::Enqueued { job } => {
                assert_eq!(job.original_codec, "opus");
                assert_eq!(job.original_backend, "fast-archive");
                assert_eq!(job.original_duration_s, Some(235));
                assert_eq!(job.status, ArchivalUpgradeStatus::Queued);
                job.id
            }
            other => panic!("expected Enqueued, got {other:?}"),
        };

        // Re-keeping or starring the same track is deduplicated.
        let second = enqueue_on_keep_in(&db, &ctx, &keep_req).await.unwrap();
        assert_eq!(
            second,
            ArchivalUpgradeOutcome::AlreadyScheduled { upgrade_id }
        );
    }

    #[tokio::test]
    async fn lossless_tracks_and_non_fast_tracks_skip_archival_upgrade() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-flac-1"),
                discovery_track_id: None,
                artist: "Daft Punk",
                title: "Veridis Quo",
                album: Some("Discovery"),
                recording_mbid: None,
                backend_id: "slskd",
                codec: "flac",
                is_lossy: false,
                duration_s: None,
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        let flac_outcome = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-flac-1".into()),
                discovery_track_id: None,
                artist: "Daft Punk".into(),
                title: "Veridis Quo".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
                duration_s: None,
                trigger_reason: "star".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            flac_outcome,
            ArchivalUpgradeOutcome::AlreadyLossless {
                codec: "flac".into()
            }
        );

        let unknown_outcome = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-unknown".into()),
                discovery_track_id: None,
                artist: "Air".into(),
                title: "La Femme d'Argent".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: Some("unknown".into()),
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(unknown_outcome, ArchivalUpgradeOutcome::NotFastAcquired);
    }

    #[tokio::test]
    async fn enqueue_on_keep_rejects_pending_discovery_track_and_unauthorized_folder() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;

        // Seed another user & folder to test authorization.
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('other-u', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('other-f', 'other-u', 'Bob', '/tmp/bob')")
            .execute(&db)
            .await
            .unwrap();

        // Empty artist or title is rejected.
        let empty_err = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: None,
                discovery_track_id: None,
                artist: "   ".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: Some("mp3".into()),
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(empty_err.code, FailureCode::InvalidInput);

        // Targeting another user's folder is rejected with UnknownHandle (folder not found).
        let unauth_err = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: None,
                discovery_track_id: None,
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some("other-f".into()),
                codec: Some("mp3".into()),
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(unauth_err.code, FailureCode::UnknownHandle);

        // Insert a Pending discovery track and matching fast_acquisition.
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-pending', 'nd-pending', 'Wake Up', 'Kowloon', 'Come Over', '/music/disc.mp3', ?, 'balanced', 'Pending')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("nd-pending"),
                discovery_track_id: Some("dt-pending"),
                artist: "Kowloon",
                title: "Wake Up",
                album: Some("Come Over"),
                recording_mbid: Some("mbid-pending"),
                backend_id: "fast-net",
                codec: "opus",
                is_lossy: true,
                duration_s: None,
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        // Even with trigger_reason == "keep", an unpromoted Pending discovery track is skipped.
        let by_dt = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: None,
                discovery_track_id: Some("dt-pending".into()),
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(by_dt, ArchivalUpgradeOutcome::SkippedNotKept);

        // Even omitting discovery_track_id and only sending navidrome_song_id still catches Pending.
        let by_song = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("nd-pending".into()),
                discovery_track_id: None,
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id),
                codec: None,
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(by_song, ArchivalUpgradeOutcome::SkippedNotKept);
    }

    #[tokio::test]
    async fn promoted_discovery_hook_enqueues_upgrade_only_for_lossy_fast_track() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("nd-disc-42"),
                discovery_track_id: Some("dt-42"),
                artist: "L'Impératrice",
                title: "Agitations tropicales",
                album: Some("Odyssée"),
                recording_mbid: Some("mbid-agitations-42"),
                backend_id: "fast-net",
                codec: "mp3",
                is_lossy: true,
                duration_s: Some(248),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        let track = DiscoveryTrack {
            id: "dt-42".into(),
            song_id: Some("nd-disc-42".into()),
            folder_id: folder_id.clone(),
            artist: "L'Impératrice".into(),
            title: "Agitations tropicales".into(),
            album: "Odyssée".into(),
            path: "/music/Discovery/Balanced/track.mp3".into(),
            profile: "balanced".into(),
            rating: None,
            status: DiscoveryStatus::Promoted,
            created_at: "2026-11-25 20:00:00".into(),
            exclude_from_core_taste: false,
            reason: None,
            culture_bucket: None,
            pack_slug: None,
        };

        let outcome =
            maybe_enqueue_for_promoted_discovery_in(&db, &ctx.user_id, &folder_id, &track)
                .await
                .unwrap();
        assert!(matches!(outcome, ArchivalUpgradeOutcome::Enqueued { .. }));
    }

    #[test]
    fn pick_lossless_group_rejects_empty_unknown_and_mixed_groups() {
        let mut empty_quality_item = item_on("slskd", "p1", "Wake Up", "wake");
        empty_quality_item.quality = String::new();
        let mut empty_group = group("p1", vec![empty_quality_item], 0.99);
        empty_group.quality = String::new();

        let mut tape_item = item_on("slskd", "p2", "Wake Up", "wake.tape");
        tape_item.quality = "tape".into();
        tape_item.facts.extension = Some("tape".into());
        let mut tape_group = group("p2", vec![tape_item], 0.98);
        tape_group.quality = "tape".into();

        // Mixed group: 1 FLAC + 1 MP3 even though group quality says FLAC
        let flac_item = item_on("slskd", "p3", "Wake Up", "01.flac");
        let mut mp3_item = item_on("slskd", "p3", "Bonus", "02.mp3");
        mp3_item.quality = "320kbps MP3".into();
        mp3_item.facts.extension = Some("mp3".into());
        let mixed_group = group("p3", vec![flac_item, mp3_item], 0.97);

        let no_items_group = group("p4", vec![], 0.99);

        assert!(pick_lossless_group(vec![
            empty_group,
            tape_group,
            mixed_group,
            no_items_group
        ])
        .is_none());

        let mut pure_flac = item_on("slskd", "p5", "Wake Up", "wake.flac");
        pure_flac.facts.extension = Some("flac".into());
        let pure_group = group("p5", vec![pure_flac], 0.91);
        let (picked, _) = pick_lossless_group(vec![pure_group]).expect("pure flac picked");
        assert_eq!(picked.source, "p5");
    }

    #[tokio::test]
    async fn reconcile_downloading_upgrades_transitions_to_completed_queued_and_failed() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;
        let ledger = Ledger::start(db.clone());

        // 1. Track A: ledger job reaches Done -> upgrade Completed & fast_acquisitions updated to lossless.
        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-a-old"),
                discovery_track_id: None,
                artist: "Artist A",
                title: "Track A",
                album: None,
                recording_mbid: Some("mbid-a"),
                backend_id: "fast-net",
                codec: "opus",
                is_lossy: true,
                duration_s: Some(200),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();
        let up_a = ArchivalUpgradeRow::insert_in(
            &db,
            NewArchivalUpgrade {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-a-old"),
                artist: "Artist A",
                title: "Track A",
                album: None,
                recording_mbid: Some("mbid-a"),
                original_codec: "opus",
                original_backend: "fast-net",
                original_duration_s: Some(200),
                trigger_reason: "keep",
            },
        )
        .await
        .unwrap();

        let job_a = "ledger-job-a".to_string();
        let slot_a = JobSlot {
            job: job_a.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_a.clone(),
                    user: ctx.id(),
                    folder_id: folder_id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Downloading,
                    search_id: None,
                    picked: vec![(0, item_on("slskd", "peer", "Track A", "a.flac"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let mut done_row = DownloadProgress::queued("t-a".into(), "peer".into(), "a.flac".into(), 10)
            .with_state(DownloadState::Imported)
            .with_job(slot_a.clone());
        done_row.index = Some(IndexState::Listed);
        ledger.record(JobEvent::Settled {
            state: JobState::Done,
            rows: vec![done_row],
            results: vec![(
                slot_a,
                SlotResult::Imported {
                    applied: vec![],
                    untagged: false,
                    listed: Some(vec![SongFacts {
                        song_id: "song-a-flac".into(),
                        title: "Track A".into(),
                        artist: Some("Artist A".into()),
                        album: None,
                        album_id: None,
                        year: None,
                        track: None,
                        duration_s: Some(200),
                        format: Some("flac".into()),
                        bitrate_kbps: Some(900),
                        size_bytes: Some(20_000_000),
                        path: Some("Artist A/Track A.flac".into()),
                        recording_mbid: None,
                        rating: 0,
                        starred: false,
                        starred_at: None,
                        play_count: 0,
                        folder: None,
                    }]),
                    replaced: vec![],
                    not_replaced: vec![],
                },
            )],
        });
        ledger.settled().await;

        ArchivalUpgradeRow::mark_status_in(
            &db,
            &up_a.id,
            ArchivalUpgradeStatus::Downloading,
            Some(&job_a),
            0,
            None,
        )
        .await
        .unwrap();

        // 2. Track B: ledger job Failed with attempts=0 -> returns to Queued with attempts=1.
        let up_b = ArchivalUpgradeRow::insert_in(
            &db,
            NewArchivalUpgrade {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: None,
                artist: "Artist B",
                title: "Track B",
                album: None,
                recording_mbid: Some("mbid-b"),
                original_codec: "mp3",
                original_backend: "fast-net",
                original_duration_s: None,
                trigger_reason: "keep",
            },
        )
        .await
        .unwrap();
        let job_b = "ledger-job-b".to_string();
        let slot_b = JobSlot {
            job: job_b.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_b.clone(),
                    user: ctx.id(),
                    folder_id: folder_id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Downloading,
                    search_id: None,
                    picked: vec![(0, item_on("slskd", "peer", "Track B", "b.flac"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let fail_row = DownloadProgress::failed(
            "t-b".into(),
            "peer".into(),
            "b.flac".into(),
            Failure::new(
                FailureCode::TransferTimedOut,
                Stage::Transfer,
                RetryAdvice::OtherSource,
                "Peer stalled",
            ),
        )
        .with_job(slot_b);
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![fail_row],
        });
        ledger.settled().await;
        ArchivalUpgradeRow::mark_status_in(
            &db,
            &up_b.id,
            ArchivalUpgradeStatus::Downloading,
            Some(&job_b),
            0,
            None,
        )
        .await
        .unwrap();

        // 3. Track C: ledger job Dismissed at attempts=2 -> hits MAX_UPGRADE_ATTEMPTS -> Failed.
        let up_c = ArchivalUpgradeRow::insert_in(
            &db,
            NewArchivalUpgrade {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: None,
                artist: "Artist C",
                title: "Track C",
                album: None,
                recording_mbid: Some("mbid-c"),
                original_codec: "mp3",
                original_backend: "fast-net",
                original_duration_s: None,
                trigger_reason: "keep",
            },
        )
        .await
        .unwrap();
        let job_c = "ledger-job-c".to_string();
        let slot_c = JobSlot {
            job: job_c.clone(),
            slot: 0,
        };
        ledger
            .create(
                NewJob {
                    id: job_c.clone(),
                    user: ctx.id(),
                    folder_id: folder_id.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Downloading,
                    search_id: None,
                    picked: vec![(0, item_on("slskd", "peer", "Track C", "c.flac"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Dismiss(vec![slot_c]));
        ledger.settled().await;
        ArchivalUpgradeRow::mark_status_in(
            &db,
            &up_c.id,
            ArchivalUpgradeStatus::Downloading,
            Some(&job_c),
            MAX_UPGRADE_ATTEMPTS - 1,
            None,
        )
        .await
        .unwrap();

        let count = reconcile_downloading_upgrades_in(&db).await.unwrap();
        assert_eq!(count, 3);

        let a_after = ArchivalUpgradeRow::get_by_id_in(&db, &up_a.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ArchivalUpgradeStatus::parse(&a_after.status),
            ArchivalUpgradeStatus::Completed
        );
        let fast_a = FastAcquisitionRow::find_matching_in(
            &db,
            &ctx.user_id,
            Some("song-a-flac"),
            None,
            "Artist A",
            "Track A",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fast_a.codec, "flac");
        assert_eq!(fast_a.is_lossy, 0);

        let b_after = ArchivalUpgradeRow::get_by_id_in(&db, &up_b.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ArchivalUpgradeStatus::parse(&b_after.status),
            ArchivalUpgradeStatus::Queued
        );
        assert_eq!(b_after.attempts, 1);
        assert_eq!(b_after.last_error.as_deref(), Some("Peer stalled"));

        let c_after = ArchivalUpgradeRow::get_by_id_in(&db, &up_c.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ArchivalUpgradeStatus::parse(&c_after.status),
            ArchivalUpgradeStatus::Failed
        );
        assert_eq!(c_after.attempts, MAX_UPGRADE_ATTEMPTS);
    }

    #[tokio::test]
    async fn worker_ignores_lossy_candidates_and_enqueues_flac_replacement() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;
        let ledger = Ledger::start(db.clone());

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-99"),
                discovery_track_id: None,
                artist: "Kowloon",
                title: "Wake Up",
                album: Some("Come Over"),
                recording_mbid: Some("mbid-kowloon-99"),
                backend_id: "fast-net",
                codec: "opus",
                is_lossy: true,
                duration_s: Some(263),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        let enqueued = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-99".into()),
                discovery_track_id: None,
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: Some("Come Over".into()),
                recording_mbid: None,
                folder_id: Some(folder_id),
                codec: None,
                duration_s: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        let upgrade_id = match enqueued {
            ArchivalUpgradeOutcome::Enqueued { job } => {
                assert_eq!(job.original_duration_s, Some(263));
                job.id
            }
            other => panic!("unexpected {other:?}"),
        };

        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend_id = stub.id();
        let mut lossy_item = item_on(backend_id, "peer_mp3", "Wake Up", "mp3/wake.mp3");
        lossy_item.quality = "320kbps MP3".into();
        let mut lossy_group = group("peer_mp3", vec![lossy_item], 0.99);
        lossy_group.quality = "320kbps MP3".into();

        let flac_item = item_on(backend_id, "peer_flac", "Wake Up", "flac/wake.flac");
        let flac_group = group("peer_flac", vec![flac_item], 0.92);

        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![lossy_group, flac_group],
        ))]);
        let backends = StubBackends::default().with(backend_id, stub.clone());

        let tick = process_next_upgrade_in(&db, &ledger, &backends, backend_id)
            .await
            .unwrap();
        match tick {
            WorkerTickOutcome::Enqueued {
                upgrade_id: got_id, ..
            } => assert_eq!(got_id, upgrade_id),
            other => panic!("expected Enqueued, got {other:?}"),
        }

        // Verify the archival search ran with Background priority and populated Track.duration!
        let specs = stub.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].priority, SearchPriority::Background);
        assert_eq!(specs[0].tracks.len(), 1);
        assert_eq!(specs[0].tracks[0].duration.as_deref(), Some("4:23"));
        assert_eq!(specs[0].tracks[0].duration_secs(), Some(263));

        let stored = ArchivalUpgradeRow::get_by_id_in(&db, &upgrade_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ArchivalUpgradeStatus::parse(&stored.status),
            ArchivalUpgradeStatus::Downloading
        );
        assert!(stored.job_id.is_some());
    }

    #[tokio::test]
    async fn tracks_without_recording_mbid_or_imported_as_is_are_skipped_for_archival_upgrade() {
        let db = test_db::migrated().await;
        let (ctx, folder_id, _dir) = seed_user_and_folder(&db).await;

        // 1. Track without recording_mbid (e.g., bootleg / unreleased live cut) -> skipped.
        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-bootleg"),
                discovery_track_id: None,
                artist: "Radiohead",
                title: "Creep (Live Acoustic Bootleg)",
                album: None,
                recording_mbid: None,
                backend_id: "fast-net",
                codec: "opus",
                is_lossy: true,
                duration_s: Some(310),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        let no_mbid = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-bootleg".into()),
                discovery_track_id: None,
                artist: "Radiohead".into(),
                title: "Creep (Live Acoustic Bootleg)".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
                duration_s: Some(310),
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(no_mbid, ArchivalUpgradeOutcome::SkippedUnmatched);

        // 2. Track imported AsIs (even if request passes an mbid) -> skipped.
        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &ctx.user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-asis"),
                discovery_track_id: None,
                artist: "Daft Punk",
                title: "One More Time (DJ Edit)",
                album: None,
                recording_mbid: Some("mbid-studio"),
                backend_id: "fast-net",
                codec: "mp3",
                is_lossy: true,
                duration_s: Some(420),
                imported_as_is: true,
            },
        )
        .await
        .unwrap();

        let as_is = enqueue_on_keep_in(
            &db,
            &ctx,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-asis".into()),
                discovery_track_id: None,
                artist: "Daft Punk".into(),
                title: "One More Time (DJ Edit)".into(),
                album: None,
                recording_mbid: Some("mbid-studio".into()),
                folder_id: Some(folder_id),
                codec: None,
                duration_s: Some(420),
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(as_is, ArchivalUpgradeOutcome::SkippedUnmatched);

        assert!(ArchivalUpgradeRow::list_for_user_in(&db, &ctx.user_id, 10)
            .await
            .unwrap()
            .is_empty());
    }
}
