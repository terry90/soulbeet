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
    is_lossy_format, ArchivalUpgradeJob, ArchivalUpgradeOutcome, ArchivalUpgradeStatus,
    DownloadQuery, DownloadableGroup, TriggerArchivalUpgradeRequest,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::Track;
use shared::navidrome::DiscoveryTrack;
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::auth::UserCtx;
use crate::core::acquire::{
    enqueue_and_watch, numbered, prepare_target, Acquisition, NewBatch, AUTO_SELECT_SCORE_THRESHOLD,
};
use crate::core::database_failure;
use crate::core::folders;
use crate::core::sources;
use crate::ledger::{DuplicateRule, JobKind, JobState, Ledger, NewJob, NewSearch};
use crate::models::archival_upgrade::{
    ArchivalUpgradeRow, FastAcquisitionRow, NewArchivalUpgrade, NewFastAcquisition,
};
use crate::models::folder::Folder;
use crate::models::user::User;
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
    let req = TriggerArchivalUpgradeRequest {
        navidrome_song_id: track.song_id.clone(),
        discovery_track_id: Some(track.id.clone()),
        artist: track.artist.clone(),
        title: track.title.clone(),
        album: Some(track.album.clone()).filter(|a| !a.trim().is_empty()),
        recording_mbid: None,
        folder_id: Some(target_folder_id.to_string()),
        codec: None,
        trigger_reason: "promote".to_string(),
    };
    enqueue_on_keep_in(db, user_id, &req).await
}

/// Enforces the **On-Keep-Only** archival FLAC upgrade policy.
///
/// Only explicit keep actions (`"keep"`, `"star"`, `"promote"`, `"manual"`) on a
/// lossy fast-acquired track enqueue a background archival FLAC upgrade job.
pub async fn enqueue_on_keep_in(
    db: &SqlitePool,
    user_id: &str,
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

    // Look up whether this track came from a fast acquisition.
    let fast = FastAcquisitionRow::find_matching_in(
        db,
        user_id,
        req.navidrome_song_id.as_deref(),
        req.discovery_track_id.as_deref(),
        &req.artist,
        &req.title,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?;

    let (codec, backend_id, folder_id, recording_mbid, album) = match fast {
        Some(record) => {
            if record.is_lossy == 0 || !is_lossy_format(&record.codec) {
                return Ok(ArchivalUpgradeOutcome::AlreadyLossless {
                    codec: record.codec,
                });
            }
            let folder_id = req
                .folder_id
                .clone()
                .unwrap_or_else(|| record.folder_id.clone());
            (
                record.codec,
                record.backend_id,
                folder_id,
                req.recording_mbid.clone().or(record.recording_mbid),
                req.album.clone().or(record.album),
            )
        }
        None => {
            // If no fast_acquisitions record exists, only allow an explicit lossy codec
            // on a manual/keep request when the caller also identifies a target folder.
            let Some(codec) = req.codec.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            };
            if !is_lossy_format(codec) {
                return Ok(ArchivalUpgradeOutcome::AlreadyLossless {
                    codec: codec.to_string(),
                });
            }
            let Some(folder_id) = req.folder_id.clone() else {
                return Ok(ArchivalUpgradeOutcome::NotFastAcquired);
            };
            (
                codec.to_string(),
                "fast_path".to_string(),
                folder_id,
                req.recording_mbid.clone(),
                req.album.clone(),
            )
        }
    };

    if let Some(existing) = ArchivalUpgradeRow::find_active_or_completed_in(
        db,
        user_id,
        req.navidrome_song_id.as_deref(),
        &req.artist,
        &req.title,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?
    {
        return Ok(ArchivalUpgradeOutcome::AlreadyScheduled {
            upgrade_id: existing.id,
        });
    }

    let inserted = ArchivalUpgradeRow::insert_in(
        db,
        NewArchivalUpgrade {
            user_id,
            folder_id: &folder_id,
            navidrome_song_id: req.navidrome_song_id.as_deref(),
            artist: &req.artist,
            title: &req.title,
            album: album.as_deref(),
            recording_mbid: recording_mbid.as_deref(),
            original_codec: &codec,
            original_backend: &backend_id,
            trigger_reason: &reason,
        },
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?;

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

/// Picks the best lossless group scoring at least `AUTO_SELECT_SCORE_THRESHOLD`.
pub fn pick_lossless_group(groups: Vec<DownloadableGroup>) -> Option<(DownloadableGroup, Vec<DownloadableGroup>)> {
    let mut lossless: Vec<DownloadableGroup> = groups
        .into_iter()
        .filter(|g| {
            let group_lossless = !is_lossy_format(&g.quality)
                || g.items.iter().all(|item| {
                    let ext = item.facts.extension.as_deref().unwrap_or(&item.quality);
                    !is_lossy_format(ext)
                });
            group_lossless && g.score >= AUTO_SELECT_SCORE_THRESHOLD && !g.items.is_empty()
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
    let Some(upgrade) = ArchivalUpgradeRow::next_queued_in(db, MAX_UPGRADE_ATTEMPTS)
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?
    else {
        return Ok(WorkerTickOutcome::Idle);
    };

    let backend = match backends.get(archival_backend_id).await {
        Ok(b) => b,
        Err(e) => {
            let reason = format!("Archival backend '{archival_backend_id}' unavailable: {e}");
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

    let _ = ArchivalUpgradeRow::mark_status_in(
        db,
        &upgrade.id,
        ArchivalUpgradeStatus::Searching,
        None,
        0,
        None,
    )
    .await;

    let track = Track {
        id: upgrade.recording_mbid.clone().unwrap_or_default(),
        title: upgrade.title.clone(),
        artist: upgrade.artist.clone(),
        album_id: None,
        album_title: upgrade.album.clone(),
        release_date: None,
        duration: None,
        mbid: upgrade.recording_mbid.clone(),
        release_mbid: None,
    };
    let query = DownloadQuery::new(vec![track.clone()]).backend(archival_backend_id);
    let searched = (archival_backend_id.to_string(), backend.clone());

    let groups = match sources::search_ranked(&searched, &[], None, &[track]).await {
        Ok(g) => g,
        Err(failure) => {
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
    };

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

    ledger
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
        .map_err(|e| database_failure(Stage::Enqueue, e))?;

    let fresh = NewBatch {
        batch_id,
        label: format!("Archival FLAC: {} - {}", upgrade.artist, upgrade.title),
        backend: archival_backend_id.to_string(),
        target,
        import_mode: ImportMode::Singleton,
        duplicates: DuplicateRule::Guarded,
    };
    let acquisition = Acquisition {
        job: job_id.clone(),
        items: picked_items,
        alternates,
        max_sources: MAX_SOURCES_PER_TRACK,
    };

    match enqueue_and_watch(ledger, &ctx, Arc::clone(&backend), fresh, acquisition).await {
        Ok(_) => {
            let _ = ArchivalUpgradeRow::mark_status_in(
                db,
                &upgrade.id,
                ArchivalUpgradeStatus::Downloading,
                Some(&job_id),
                1,
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
        Err(failure) => {
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
    use crate::db::test_db;
    use crate::server_fns::download::test_support::{
        group, item_on, search_result, ScriptedOutcome, StubBackend, StubBackends,
    };
    use shared::download::SearchState;
    use shared::navidrome::DiscoveryStatus;
    use soulbeet::DownloadBackend;

    async fn seed_user_and_folder(db: &SqlitePool) -> (String, String, tempfile::TempDir) {
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
        (user_id, folder_id, dir)
    }

    #[tokio::test]
    async fn recording_a_lossy_fast_acquisition_never_schedules_an_upgrade_until_kept() {
        let db = test_db::migrated().await;
        let (user_id, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &user_id,
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
            },
        )
        .await
        .unwrap();

        // Still zero archival upgrade jobs before explicit Keep/Promote!
        assert!(ArchivalUpgradeRow::list_for_user_in(&db, &user_id, 10)
            .await
            .unwrap()
            .is_empty());

        // Non-keep trigger reasons are rejected.
        let rejected = enqueue_on_keep_in(
            &db,
            &user_id,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-opus-1".into()),
                discovery_track_id: Some("disc-1".into()),
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
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
            trigger_reason: "keep".into(),
        };
        let first = enqueue_on_keep_in(&db, &user_id, &keep_req).await.unwrap();
        let upgrade_id = match first {
            ArchivalUpgradeOutcome::Enqueued { job } => {
                assert_eq!(job.original_codec, "opus");
                assert_eq!(job.original_backend, "fast-archive");
                assert_eq!(job.status, ArchivalUpgradeStatus::Queued);
                job.id
            }
            other => panic!("expected Enqueued, got {other:?}"),
        };

        // Re-keeping or starring the same track is deduplicated.
        let second = enqueue_on_keep_in(&db, &user_id, &keep_req).await.unwrap();
        assert_eq!(
            second,
            ArchivalUpgradeOutcome::AlreadyScheduled { upgrade_id }
        );
    }

    #[tokio::test]
    async fn lossless_tracks_and_non_fast_tracks_skip_archival_upgrade() {
        let db = test_db::migrated().await;
        let (user_id, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &user_id,
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
            },
        )
        .await
        .unwrap();

        let flac_outcome = enqueue_on_keep_in(
            &db,
            &user_id,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-flac-1".into()),
                discovery_track_id: None,
                artist: "Daft Punk".into(),
                title: "Veridis Quo".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id.clone()),
                codec: None,
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
            &user_id,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-unknown".into()),
                discovery_track_id: None,
                artist: "Air".into(),
                title: "La Femme d'Argent".into(),
                album: None,
                recording_mbid: None,
                folder_id: Some(folder_id),
                codec: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(unknown_outcome, ArchivalUpgradeOutcome::NotFastAcquired);
    }

    #[tokio::test]
    async fn promoted_discovery_hook_enqueues_upgrade_only_for_lossy_fast_track() {
        let db = test_db::migrated().await;
        let (user_id, folder_id, _dir) = seed_user_and_folder(&db).await;

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("nd-disc-42"),
                discovery_track_id: Some("dt-42"),
                artist: "L'Impératrice",
                title: "Agitations tropicales",
                album: Some("Odyssée"),
                recording_mbid: None,
                backend_id: "fast-net",
                codec: "mp3",
                is_lossy: true,
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
        };

        let outcome =
            maybe_enqueue_for_promoted_discovery_in(&db, &user_id, &folder_id, &track)
                .await
                .unwrap();
        assert!(matches!(outcome, ArchivalUpgradeOutcome::Enqueued { .. }));
    }

    #[tokio::test]
    async fn worker_ignores_lossy_candidates_and_enqueues_flac_replacement() {
        let db = test_db::migrated().await;
        let (user_id, folder_id, _dir) = seed_user_and_folder(&db).await;
        let ledger = Ledger::start(db.clone());

        record_fast_acquisition_in(
            &db,
            NewFastAcquisition {
                user_id: &user_id,
                folder_id: &folder_id,
                navidrome_song_id: Some("song-99"),
                discovery_track_id: None,
                artist: "Kowloon",
                title: "Wake Up",
                album: Some("Come Over"),
                recording_mbid: None,
                backend_id: "fast-net",
                codec: "opus",
                is_lossy: true,
            },
        )
        .await
        .unwrap();

        let enqueued = enqueue_on_keep_in(
            &db,
            &user_id,
            &TriggerArchivalUpgradeRequest {
                navidrome_song_id: Some("song-99".into()),
                discovery_track_id: None,
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
                album: Some("Come Over".into()),
                recording_mbid: None,
                folder_id: Some(folder_id),
                codec: None,
                trigger_reason: "keep".into(),
            },
        )
        .await
        .unwrap();
        let upgrade_id = match enqueued {
            ArchivalUpgradeOutcome::Enqueued { job } => job.id,
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
}
