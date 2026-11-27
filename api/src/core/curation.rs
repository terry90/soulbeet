//! Unified track curation (Keep, Star, Rate, Drop) and Multi-Listener Delete Guard previews
//! shared by Web, Desktop, and Android clients.

use std::sync::Arc;

use async_trait::async_trait;
use shared::client::{
    CurationAction, CurationResult, DeleteGuardPreview, DropTrackRequest, KeepTrackRequest,
    ListenerProtection, ListenerProtectionKind, RateTrackRequest, StarTrackRequest,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::{DeletionReason, DiscoveryStatus, DiscoveryTrack};
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::StarTarget;
use tracing::warn;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::{pending_track_of, song_not_found, ForgetVia, Guard, Origin, Verdict};
use crate::core::discovery::{
    is_not_pending, promote_to, remove_in, track_not_pending, DiscoveryEffects, Replace,
};
use crate::core::index::ServerNavidrome;
use crate::core::library::{navidrome_failure, navidrome_for};
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::user_settings::UserSettings;

/// Navidrome annotation operations used by curation, abstracted for unit tests.
#[async_trait]
pub(crate) trait CurationNavidrome: Send + Sync {
    async fn get_song(&self, ctx: &UserCtx, song_id: &str) -> Result<Option<SubsonicSong>, Failure>;
    async fn set_star(&self, ctx: &UserCtx, song_id: &str, starred: bool) -> Result<(), Failure>;
    async fn set_rating(&self, ctx: &UserCtx, song_id: &str, rating: u8) -> Result<(), Failure>;
}

/// Live Navidrome implementation backed by each user's configured `NavidromeClient`.
pub(crate) struct LiveCurationNavidrome;

#[async_trait]
impl CurationNavidrome for LiveCurationNavidrome {
    async fn get_song(&self, ctx: &UserCtx, song_id: &str) -> Result<Option<SubsonicSong>, Failure> {
        let navi = navidrome_for(&ServerNavidrome, ctx, Stage::LibraryCheck).await?;
        navi.get_song(song_id)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))
    }

    async fn set_star(&self, ctx: &UserCtx, song_id: &str, starred: bool) -> Result<(), Failure> {
        let navi = navidrome_for(&ServerNavidrome, ctx, Stage::LibraryCheck).await?;
        navi.set_star(StarTarget::Song(song_id), starred)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))
    }

    async fn set_rating(&self, ctx: &UserCtx, song_id: &str, rating: u8) -> Result<(), Failure> {
        let navi = navidrome_for(&ServerNavidrome, ctx, Stage::LibraryCheck).await?;
        navi.set_rating(song_id, rating)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))
    }
}

fn invalid_rating() -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Rating must be between 0 and 5",
    )
}

fn protection_from_failure(failure: &Failure) -> ListenerProtection {
    let kind = match failure.code {
        FailureCode::DeleteBlockedByOtherUser => ListenerProtectionKind::OtherListenerMark,
        FailureCode::DeleteVetoUnknown => ListenerProtectionKind::UnreadableListener,
        FailureCode::DeleteOutsideLibrary => ListenerProtectionKind::OutsideLibrary,
        FailureCode::DeletePathUnresolved => ListenerProtectionKind::PathUnresolved,
        FailureCode::DeleteInUse => ListenerProtectionKind::InUse,
        _ => ListenerProtectionKind::Other,
    };
    ListenerProtection {
        kind,
        summary: failure.message.clone(),
    }
}

fn preview_from_blocked(song_id: &str, failure: &Failure) -> DeleteGuardPreview {
    let is_listener_block = matches!(
        failure.code,
        FailureCode::DeleteBlockedByOtherUser | FailureCode::DeleteVetoUnknown
    );
    DeleteGuardPreview {
        song_id: song_id.to_string(),
        is_pending_discovery: false,
        discovery_track_id: None,
        can_delete: false,
        sole_owner: false,
        other_listeners: if is_listener_block {
            vec![protection_from_failure(failure)]
        } else {
            Vec::new()
        },
        own_playlists: Vec::new(),
        block_reason: Some(failure.message.clone()),
    }
}

fn is_delete_failure(code: FailureCode) -> bool {
    matches!(
        code,
        FailureCode::DeletePathUnresolved
            | FailureCode::DeleteOutsideLibrary
            | FailureCode::DeleteNotATrack
            | FailureCode::DeleteBlockedByOtherUser
            | FailureCode::DeleteVetoUnknown
            | FailureCode::DeleteInUse
            | FailureCode::DeleteChangedSincePreview
            | FailureCode::DeleteFailed
            | FailureCode::DeleteDailyLimit
    )
}

fn effective_song_id(req_song_id: &str, track: &DiscoveryTrack) -> String {
    if !req_song_id.trim().is_empty() {
        req_song_id.to_string()
    } else {
        track.song_id.clone().unwrap_or_default()
    }
}

enum DiscoveryLookup {
    Pending(DiscoveryTrack),
    Already(DiscoveryTrack),
    NotDiscovery,
}

/// Fetches a discovery track by `(id, user_id)` regardless of status (verifying ownership
/// through its folder).
async fn find_user_discovery_by_id(
    db: &sqlx::SqlitePool,
    user_id: &str,
    track_id: &str,
) -> Result<Option<DiscoveryTrack>, Failure> {
    let row = sqlx::query_as::<_, DiscoveryTrackRow>(
        "SELECT t.* FROM discovery_tracks t \
         JOIN folders f ON f.id = t.folder_id \
         WHERE t.id = ? AND f.user_id = ?",
    )
    .bind(track_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .map_err(|e| database_failure(Stage::LibraryCheck, e))?;
    Ok(row.map(Into::into))
}

/// Persists the pure quality rating written directly inside Soulbeet on `discovery_tracks.rating`
/// so rating sync does not treat it as an external-client promote/drop trigger.
async fn record_discovery_rating(
    db: &sqlx::SqlitePool,
    user_id: &str,
    track_id: &str,
    rating: Option<i64>,
) -> Result<(), Failure> {
    sqlx::query(
        "UPDATE discovery_tracks SET rating = ? \
         WHERE id = ? AND folder_id IN (SELECT id FROM folders WHERE user_id = ?)",
    )
    .bind(rating)
    .bind(track_id)
    .bind(user_id)
    .execute(db)
    .await
    .map_err(|e| database_failure(Stage::LibraryCheck, e))?;
    Ok(())
}

/// Resolves whether `song_id` (or explicit `discovery_track_id`) corresponds to a Pending or
/// already-transitioned discovery track owned by `ctx`.
async fn lookup_discovery(
    guard: &Guard<'_>,
    ctx: &UserCtx,
    song_id: &str,
    explicit_discovery_id: Option<&str>,
) -> Result<DiscoveryLookup, Failure> {
    let pending = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;

    if let Some(track_id) = explicit_discovery_id.filter(|s| !s.is_empty()) {
        if let Some(track) = pending.into_iter().find(|t| t.id == track_id) {
            return Ok(DiscoveryLookup::Pending(track));
        }
        if let Some(existing) = find_user_discovery_by_id(guard.db, &ctx.user_id, track_id).await? {
            return Ok(DiscoveryLookup::Already(existing));
        }
        return Err(track_not_pending());
    }

    if pending.is_empty() || song_id.is_empty() {
        return Ok(DiscoveryLookup::NotDiscovery);
    }

    if let Some(track) = pending
        .iter()
        .find(|t| t.song_id.as_deref() == Some(song_id))
    {
        return Ok(DiscoveryLookup::Pending(track.clone()));
    }

    let user = ctx.id();
    let Some(native) = guard.read_song(&user, song_id).await? else {
        return Ok(DiscoveryLookup::NotDiscovery);
    };
    Ok(
        match pending_track_of(guard.db, guard.music_path, &user, &native, &pending).await? {
            Some(track) => DiscoveryLookup::Pending(track.clone()),
            None => DiscoveryLookup::NotDiscovery,
        },
    )
}

/// Finds a Pending discovery track matching `song_id` (or `explicit_discovery_id`) for recording
/// a pure quality rating. Returns `Ok(None)` if the track is not pending.
async fn find_pending_for_rating(
    guard: &Guard<'_>,
    ctx: &UserCtx,
    song_id: &str,
    explicit_discovery_id: Option<&str>,
) -> Result<Option<DiscoveryTrack>, Failure> {
    let pending = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;

    if let Some(track_id) = explicit_discovery_id.filter(|s| !s.is_empty()) {
        return Ok(pending.into_iter().find(|t| t.id == track_id));
    }

    if pending.is_empty() || song_id.is_empty() {
        return Ok(None);
    }

    if let Some(track) = pending
        .iter()
        .find(|t| t.song_id.as_deref() == Some(song_id))
    {
        return Ok(Some(track.clone()));
    }

    let user = ctx.id();
    let Some(native) = guard.read_song(&user, song_id).await? else {
        return Ok(None);
    };
    Ok(
        pending_track_of(guard.db, guard.music_path, &user, &native, &pending)
            .await?
            .cloned(),
    )
}

/// Builds the `CurationResult` for a promoted (or already-promoted) discovery track without
/// modifying its Navidrome star rating.
async fn kept_discovery_result(
    navi: &dyn CurationNavidrome,
    ctx: &UserCtx,
    req_song_id: &str,
    track: &DiscoveryTrack,
) -> CurationResult {
    let song_id = effective_song_id(req_song_id, track);
    let mut current_rating = track.rating.filter(|r| *r > 0);
    let mut fetched_starred = None;
    if !song_id.is_empty() {
        if let Ok(Some(song)) = navi.get_song(ctx, &song_id).await {
            fetched_starred = Some(song.starred.is_some());
            current_rating = song.user_rating.filter(|r| *r > 0);
        }
    }

    // PR 1C will plug the On-Keep archival FLAC upgrade worker in here when a track was
    // fast-acquired in lossy quality.
    let archival_upgrade_queued = false;

    CurationResult {
        song_id,
        action: CurationAction::Kept,
        promoted_discovery: true,
        starred: fetched_starred,
        rating: current_rating,
        deleted_from_disk: false,
        archival_upgrade_queued,
        guard_preview: None,
        message: format!("Saved '{}' to your library", track.title),
    }
}

fn dropped_discovery_result(
    song_id: &str,
    track: &DiscoveryTrack,
    starred: Option<bool>,
) -> CurationResult {
    CurationResult {
        song_id: song_id.to_string(),
        action: CurationAction::DroppedDiscovery,
        promoted_discovery: false,
        starred,
        rating: None,
        deleted_from_disk: true,
        archival_upgrade_queued: false,
        guard_preview: None,
        message: format!("Dropped '{}' from Discovery", track.title),
    }
}

/// Evaluates Soulbeet's Multi-Listener Delete Guard for `song_id` without mutating anything.
/// Powers the Desktop Track Inspector and pre-delete confirmation sheet.
pub(crate) async fn guard_preview_in(
    guard: &Guard<'_>,
    ctx: &UserCtx,
    song_id: &str,
) -> Result<DeleteGuardPreview, Failure> {
    let user = ctx.id();
    match guard.check_song(&user, song_id).await {
        Ok(Verdict::Discovery { track, .. }) => Ok(DeleteGuardPreview {
            song_id: song_id.to_string(),
            is_pending_discovery: true,
            discovery_track_id: Some(track.id),
            can_delete: true,
            sole_owner: true,
            other_listeners: Vec::new(),
            own_playlists: Vec::new(),
            block_reason: None,
        }),
        Ok(Verdict::Library(checked)) => Ok(DeleteGuardPreview {
            song_id: song_id.to_string(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: true,
            sole_owner: true,
            other_listeners: Vec::new(),
            own_playlists: checked.playlists.into_iter().map(|p| p.name).collect(),
            block_reason: None,
        }),
        Err(failure) if is_delete_failure(failure.code) => {
            Ok(preview_from_blocked(song_id, &failure))
        }
        Err(failure) => Err(failure),
    }
}

/// Keeps/promotes a track without altering its Navidrome star rating.
/// Pending Discovery tracks are moved into the permanent library; duplicate calls on an
/// already-promoted (or currently promoting) discovery track succeed idempotently.
pub(crate) async fn keep_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &KeepTrackRequest,
) -> Result<CurationResult, Failure> {
    let user = ctx.id();

    match lookup_discovery(guard, ctx, &req.song_id, req.discovery_track_id.as_deref()).await? {
        DiscoveryLookup::Pending(track) => {
            match promote_to(
                guard.db,
                effects,
                queue,
                &user,
                &track.id,
                req.target_folder_id.as_deref(),
                Replace::Guarded(Origin::Ui),
            )
            .await
            {
                Ok((promoted, _)) => {
                    return Ok(kept_discovery_result(navi, ctx, &req.song_id, &promoted).await);
                }
                Err(not_done) => {
                    let failure = not_done.into_failure();
                    if is_not_pending(&failure) {
                        if let Some(existing) =
                            find_user_discovery_by_id(guard.db, &ctx.user_id, &track.id).await?
                        {
                            if matches!(
                                existing.status,
                                DiscoveryStatus::Promoted | DiscoveryStatus::Promoting
                            ) {
                                return Ok(
                                    kept_discovery_result(navi, ctx, &req.song_id, &existing).await,
                                );
                            }
                        }
                    }
                    return Err(failure);
                }
            }
        }
        DiscoveryLookup::Already(track) => {
            if matches!(
                track.status,
                DiscoveryStatus::Promoted | DiscoveryStatus::Promoting
            ) {
                return Ok(kept_discovery_result(navi, ctx, &req.song_id, &track).await);
            }
            return Err(track_not_pending());
        }
        DiscoveryLookup::NotDiscovery => {}
    }

    let song = navi
        .get_song(ctx, &req.song_id)
        .await?
        .ok_or_else(song_not_found)?;

    Ok(CurationResult {
        song_id: req.song_id.clone(),
        action: CurationAction::Kept,
        promoted_discovery: false,
        starred: Some(song.starred.is_some()),
        rating: song.user_rating.filter(|r| *r > 0),
        deleted_from_disk: false,
        archival_upgrade_queued: false,
        guard_preview: None,
        message: format!("Kept '{}' in your library", song.title),
    })
}

/// Stars or unstars a track. Starring a Pending Discovery track automatically promotes/keeps it
/// into the permanent library without stamping an artificial star rating. Duplicate star+keep
/// calls on an already-promoted (or currently promoting) discovery track succeed idempotently.
pub(crate) async fn star_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &StarTrackRequest,
) -> Result<CurationResult, Failure> {
    let discovery_lookup = if req.starred {
        let lookup =
            lookup_discovery(guard, ctx, &req.song_id, req.discovery_track_id.as_deref()).await?;
        if let DiscoveryLookup::Already(ref track) = lookup {
            if !matches!(
                track.status,
                DiscoveryStatus::Promoted | DiscoveryStatus::Promoting
            ) {
                return Err(track_not_pending());
            }
        }
        Some(lookup)
    } else {
        None
    };

    let song_id = match &discovery_lookup {
        Some(DiscoveryLookup::Pending(track) | DiscoveryLookup::Already(track)) => {
            effective_song_id(&req.song_id, track)
        }
        _ => req.song_id.clone(),
    };

    let song = if song_id.is_empty() {
        None
    } else {
        navi.get_song(ctx, &song_id).await?
    };
    let song = match song {
        Some(song) => song,
        None => {
            if let Some(DiscoveryLookup::Already(track)) = discovery_lookup {
                return Ok(CurationResult {
                    song_id,
                    action: CurationAction::Starred,
                    promoted_discovery: true,
                    starred: None,
                    rating: track.rating.filter(|r| *r > 0),
                    deleted_from_disk: false,
                    archival_upgrade_queued: false,
                    guard_preview: None,
                    message: format!("Starred and saved '{}' to your library", track.title),
                });
            }
            return Err(song_not_found());
        }
    };
    navi.set_star(ctx, &song_id, req.starred).await?;

    let mut promoted_discovery = false;
    let applied_rating = song.user_rating.filter(|r| *r > 0);
    let archival_upgrade_queued = false;

    if let Some(lookup) = discovery_lookup {
        match lookup {
            DiscoveryLookup::Pending(track) => {
                let user = ctx.id();
                match promote_to(
                    guard.db,
                    effects,
                    queue,
                    &user,
                    &track.id,
                    None,
                    Replace::Guarded(Origin::Ui),
                )
                .await
                {
                    Ok(_) => promoted_discovery = true,
                    Err(not_done) => {
                        let failure = not_done.into_failure();
                        if is_not_pending(&failure) {
                            if let Some(existing) =
                                find_user_discovery_by_id(guard.db, &ctx.user_id, &track.id).await?
                            {
                                if matches!(
                                    existing.status,
                                    DiscoveryStatus::Promoted | DiscoveryStatus::Promoting
                                ) {
                                    promoted_discovery = true;
                                } else {
                                    return Err(failure);
                                }
                            } else {
                                return Err(failure);
                            }
                        } else {
                            return Err(failure);
                        }
                    }
                }
            }
            DiscoveryLookup::Already(_) => {
                promoted_discovery = true;
            }
            DiscoveryLookup::NotDiscovery => {}
        }
    }

    let (action, message) = if req.starred && promoted_discovery {
        (
            CurationAction::Starred,
            format!("Starred and saved '{}' to your library", song.title),
        )
    } else if req.starred {
        (CurationAction::Starred, format!("Starred '{}'", song.title))
    } else {
        (
            CurationAction::Unstarred,
            format!("Removed star from '{}'", song.title),
        )
    };

    Ok(CurationResult {
        song_id,
        action,
        promoted_discovery,
        starred: Some(req.starred),
        rating: applied_rating,
        deleted_from_disk: false,
        archival_upgrade_queued,
        guard_preview: None,
        message,
    })
}

/// Sets a pure 0-5 star quality rating on a track everywhere, including Pending Discovery tracks.
/// Never auto-promotes or auto-deletes pending discovery tracks; records Soulbeet's rating in
/// `discovery_tracks.rating` so external-client rating sync skips it.
pub(crate) async fn rate_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    _effects: &Arc<dyn DiscoveryEffects>,
    _queue: &ImportQueue,
    ctx: &UserCtx,
    req: &RateTrackRequest,
) -> Result<CurationResult, Failure> {
    if req.rating > 5 {
        return Err(invalid_rating());
    }
    let pending_track = find_pending_for_rating(
        guard,
        ctx,
        &req.song_id,
        req.discovery_track_id.as_deref(),
    )
    .await?;

    let effective_id = match &pending_track {
        Some(track) => effective_song_id(&req.song_id, track),
        None => req.song_id.clone(),
    };

    let song = navi
        .get_song(ctx, &effective_id)
        .await?
        .ok_or_else(song_not_found)?;

    let prior_rating = pending_track
        .as_ref()
        .and_then(|track| track.rating.map(i64::from));
    let is_pending_discovery = if let Some(ref track) = pending_track {
        let stored_rating = (req.rating > 0).then_some(req.rating as i64);
        record_discovery_rating(guard.db, &ctx.user_id, &track.id, stored_rating).await?;
        true
    } else {
        false
    };

    if let Err(err) = navi.set_rating(ctx, &effective_id, req.rating).await {
        if let Some(ref track) = pending_track {
            if let Err(rollback_err) =
                record_discovery_rating(guard.db, &ctx.user_id, &track.id, prior_rating).await
            {
                warn!(
                    "Could not restore discovery rating for track '{}' after set_rating failure: {}",
                    track.id, rollback_err.message
                );
            }
        }
        return Err(err);
    }

    let guard_preview = if !is_pending_discovery && req.rating == 1 {
        let settings = UserSettings::get_in(guard.db, &ctx.user_id)
            .await
            .map_err(|e| database_failure(Stage::Input, e))?;
        if settings.auto_delete_enabled {
            guard_preview_in(guard, ctx, &effective_id).await.ok()
        } else {
            None
        }
    } else {
        None
    };

    Ok(CurationResult {
        song_id: effective_id,
        action: CurationAction::Rated,
        promoted_discovery: false,
        starred: Some(song.starred.is_some()),
        rating: (req.rating > 0).then_some(req.rating),
        deleted_from_disk: false,
        archival_upgrade_queued: false,
        guard_preview,
        message: if req.rating == 0 {
            format!("Cleared rating on '{}'", song.title)
        } else {
            format!("Rated '{}' {}★", song.title, req.rating)
        },
    })
}

/// Drops a Pending Discovery track (deleting its staged file + recording negative taste signal)
/// without stamping an artificial 1-star rating, or handles removal of a Permanent Library track
/// through Soulbeet's Multi-Listener Delete Guard:
/// - Sole owner + `confirm_delete == true`: deletes file via `Guard::carry_out`.
/// - Sole owner or protected + `confirm_delete == false`: returns `ConfirmationRequired` preview
///   without mutating disk or Navidrome.
/// - Protected by another listener + `confirm_delete == true`: never deletes the file; unstars &
///   sets 1★ for the caller.
/// - `DeleteVetoUnknown` and all other guard failures are always propagated as `Err`.
pub(crate) async fn drop_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &DropTrackRequest,
) -> Result<CurationResult, Failure> {
    let user = ctx.id();

    // Direct drop by explicit discovery track id (idempotent if already removed/removing).
    match lookup_discovery(guard, ctx, &req.song_id, req.discovery_track_id.as_deref()).await? {
        DiscoveryLookup::Pending(track) => {
            let song_id = effective_song_id(&req.song_id, &track);
            return drop_pending_discovery(guard, navi, queue, ctx, &song_id, track).await;
        }
        DiscoveryLookup::Already(track) => {
            if matches!(
                track.status,
                DiscoveryStatus::Removed | DiscoveryStatus::Removing
            ) {
                let song_id = effective_song_id(&req.song_id, &track);
                return Ok(dropped_discovery_result(&song_id, &track, None));
            }
            return Err(track_not_pending());
        }
        DiscoveryLookup::NotDiscovery => {}
    }

    match guard.check_song(&user, &req.song_id).await {
        Ok(Verdict::Discovery { track, .. }) => {
            let song_id = effective_song_id(&req.song_id, &track);
            drop_pending_discovery(guard, navi, queue, ctx, &song_id, track).await
        }
        Ok(Verdict::Library(checked)) => {
            let title = checked.song.title.clone();
            let preview = DeleteGuardPreview {
                song_id: req.song_id.clone(),
                is_pending_discovery: false,
                discovery_track_id: None,
                can_delete: true,
                sole_owner: true,
                other_listeners: Vec::new(),
                own_playlists: checked.playlists.iter().map(|p| p.name.clone()).collect(),
                block_reason: None,
            };

            if !req.confirm_delete {
                return Ok(CurationResult {
                    song_id: req.song_id.clone(),
                    action: CurationAction::ConfirmationRequired,
                    promoted_discovery: false,
                    starred: None,
                    rating: None,
                    deleted_from_disk: false,
                    archival_upgrade_queued: false,
                    guard_preview: Some(preview),
                    message: format!("Confirm removing '{}' from disk", title),
                });
            }

            let reason = req.reason.unwrap_or(DeletionReason::Cleanup);
            guard
                .carry_out(
                    &user,
                    checked,
                    reason,
                    &Origin::Ui,
                    ForgetVia::Queue(queue.clone()),
                )
                .await
                .map_err(|e| e.into_failure())?;

            Ok(CurationResult {
                song_id: req.song_id.clone(),
                action: CurationAction::DeletedFromLibrary,
                promoted_discovery: false,
                starred: None,
                rating: None,
                deleted_from_disk: true,
                archival_upgrade_queued: false,
                guard_preview: Some(preview),
                message: format!("Deleted '{}' from library", title),
            })
        }
        Err(failure) if failure.code == FailureCode::DeleteBlockedByOtherUser => {
            let preview = preview_from_blocked(&req.song_id, &failure);
            if !req.confirm_delete {
                return Ok(CurationResult {
                    song_id: req.song_id.clone(),
                    action: CurationAction::ConfirmationRequired,
                    promoted_discovery: false,
                    starred: None,
                    rating: None,
                    deleted_from_disk: false,
                    archival_upgrade_queued: false,
                    guard_preview: Some(preview),
                    message:
                        "File is shared with another listener; confirm to unstar and hide it from your mixes"
                            .to_string(),
                });
            }

            // Protect the shared file: unstar and rate 1★ for this listener only so it drops
            // out of their personal mixes without affecting anyone else.
            let starred = match navi.set_star(ctx, &req.song_id, false).await {
                Ok(()) => Some(false),
                Err(e) => {
                    warn!(
                        "Could not unstar protected song {} during drop: {}",
                        req.song_id, e.message
                    );
                    None
                }
            };
            let rating = match navi.set_rating(ctx, &req.song_id, 1).await {
                Ok(()) => Some(1),
                Err(e) => {
                    warn!(
                        "Could not set 1★ on protected song {} during drop: {}",
                        req.song_id, e.message
                    );
                    None
                }
            };

            Ok(CurationResult {
                song_id: req.song_id.clone(),
                action: CurationAction::ProtectedAndUnstarred,
                promoted_discovery: false,
                starred,
                rating,
                deleted_from_disk: false,
                archival_upgrade_queued: false,
                guard_preview: Some(preview),
                message:
                    "File kept because another listener uses it; unstarred and hidden from your mixes"
                        .to_string(),
            })
        }
        Err(failure) => Err(failure),
    }
}

async fn drop_pending_discovery(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    queue: &ImportQueue,
    ctx: &UserCtx,
    song_id: &str,
    track: DiscoveryTrack,
) -> Result<CurationResult, Failure> {
    let user = ctx.id();
    if let Err(not_done) = remove_in(guard.db, queue, &user, &track.id).await {
        let failure = not_done.into_failure();
        if is_not_pending(&failure) {
            if let Some(existing) =
                find_user_discovery_by_id(guard.db, &ctx.user_id, &track.id).await?
            {
                if matches!(
                    existing.status,
                    DiscoveryStatus::Removed | DiscoveryStatus::Removing
                ) {
                    return Ok(dropped_discovery_result(song_id, &existing, None));
                }
            }
        }
        return Err(failure);
    }

    let starred = if !song_id.is_empty() {
        match navi.set_star(ctx, song_id, false).await {
            Ok(()) => Some(false),
            Err(e) => {
                warn!(
                    "Dropped discovery track '{}' but could not unstar in Navidrome: {}",
                    track.title, e.message
                );
                None
            }
        }
    } else {
        None
    };

    Ok(dropped_discovery_result(song_id, &track, starred))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use shared::navidrome::{DiscoveryProgress, DiscoveryStatus};

    use super::*;
    use crate::auth::UserId;
    use crate::core::delete_guard::test_support::{FakeNavidrome, Library};
    use crate::core::delete_guard::Listeners;
    use crate::core::test_ctx;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::services::NavidromeLookupError;

    struct FakeCurationNavi {
        fake: Arc<FakeNavidrome>,
        stars_written: Mutex<Vec<(String, String, bool)>>,
        ratings_written: Mutex<Vec<(String, String, u8)>>,
        fail_set_rating: bool,
        inspect_db: Option<(sqlx::SqlitePool, String)>,
        observed_db_rating_on_set: Mutex<Option<Option<u8>>>,
    }

    impl FakeCurationNavi {
        fn new(fake: Arc<FakeNavidrome>) -> Self {
            Self {
                fake,
                stars_written: Mutex::new(Vec::new()),
                ratings_written: Mutex::new(Vec::new()),
                fail_set_rating: false,
                inspect_db: None,
                observed_db_rating_on_set: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl CurationNavidrome for FakeCurationNavi {
        async fn get_song(
            &self,
            ctx: &UserCtx,
            song_id: &str,
        ) -> Result<Option<SubsonicSong>, Failure> {
            self.fake
                .song(&ctx.id(), song_id)
                .await
                .map_err(|e| e.failure(Stage::LibraryCheck, e.to_string()))
        }

        async fn set_star(
            &self,
            ctx: &UserCtx,
            song_id: &str,
            starred: bool,
        ) -> Result<(), Failure> {
            self.stars_written
                .lock()
                .unwrap()
                .push((ctx.user_id.clone(), song_id.to_string(), starred));
            if starred {
                self.fake.stars(&ctx.user_id, song_id);
            }
            Ok(())
        }

        async fn set_rating(
            &self,
            ctx: &UserCtx,
            song_id: &str,
            rating: u8,
        ) -> Result<(), Failure> {
            if let Some((ref db, ref track_id)) = self.inspect_db {
                let row = DiscoveryTrackRow::get_by_id_in(db, track_id)
                    .await
                    .unwrap()
                    .unwrap();
                *self.observed_db_rating_on_set.lock().unwrap() = Some(row.rating);
            }
            if self.fail_set_rating {
                return Err(Failure::new(
                    FailureCode::NavidromeUnreachable,
                    Stage::LibraryCheck,
                    RetryAdvice::SameSourceLater,
                    "simulated set_rating failure",
                ));
            }
            self.ratings_written
                .lock()
                .unwrap()
                .push((ctx.user_id.clone(), song_id.to_string(), rating));
            self.fake
                .set_rating(&ctx.id(), song_id, rating)
                .await
                .map_err(|e| e.failure(Stage::LibraryCheck, e.to_string()))
        }
    }

    #[derive(Default)]
    struct StubDiscoveryEffects {
        imported: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl DiscoveryEffects for StubDiscoveryEffects {
        async fn claim(&self, _: &UserId, _: DiscoveryProgress) -> Result<(), Failure> {
            Ok(())
        }

        async fn current(&self, _: &UserId) -> Option<DiscoveryProgress> {
            None
        }

        fn generate(&self, _: &UserId) {}

        async fn import(
            &self,
            src: &Path,
            target: &Path,
            _: &UserId,
            _: Replace,
        ) -> Result<(), Failure> {
            self.imported.lock().unwrap().push((
                src.to_string_lossy().into_owned(),
                target.to_string_lossy().into_owned(),
            ));
            // Simulate beets moving the staged file out of Discovery.
            let _ = tokio::fs::remove_file(src).await;
            Ok(())
        }
    }

    fn test_queue() -> ImportQueue {
        ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())))
    }

    async fn insert_pending_discovery(
        lib: &Library,
        track_id: &str,
        song_id: &str,
        rel_path: &str,
    ) -> std::path::PathBuf {
        let full = lib.root.join("music/alice/Discovery/Balanced").join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, b"audio").unwrap();

        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES (?, ?, 'O Holy Night', 'Marrow Choir', 'Winter Songs', ?, 'alice-music', 'Balanced', 'Pending')",
        )
        .bind(track_id)
        .bind(song_id)
        .bind(full.to_string_lossy().as_ref())
        .execute(&lib.db)
        .await
        .unwrap();

        full
    }

    #[tokio::test]
    async fn starring_a_pending_discovery_track_auto_promotes_it() {
        let lib = Library::new().await;
        let staged = insert_pending_discovery(&lib, "dt-1", "s1", "Marrow Choir/01.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let effects_inner = Arc::new(StubDiscoveryEffects::default());
        let effects: Arc<dyn DiscoveryEffects> = effects_inner.clone();
        let queue = test_queue();
        let alice = test_ctx("alice");

        let res = star_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &StarTrackRequest {
                song_id: "s1".into(),
                starred: true,
                discovery_track_id: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(res.action, CurationAction::Starred);
        assert!(res.promoted_discovery);
        assert_eq!(res.starred, Some(true));
        // Starring must NOT artificially stamp 3 stars; it preserves the existing rating (1 in Library::new).
        assert_eq!(res.rating, Some(1));
        assert!(navi.ratings_written.lock().unwrap().is_empty());
        assert!(!res.deleted_from_disk);
        assert_eq!(effects_inner.imported.lock().unwrap().len(), 1);
        assert!(!staged.exists());

        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Promoted);
    }

    #[tokio::test]
    async fn drop_deletes_sole_owner_library_track_after_confirmation() {
        let lib = Library::new().await;
        let file = lib.root.join("music/alice/A/x.flac");
        assert!(file.exists());
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let alice = test_ctx("alice");

        // Preview first (confirm_delete = false) must not touch the file.
        let preview_res = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(preview_res.action, CurationAction::ConfirmationRequired);
        assert!(!preview_res.deleted_from_disk);
        assert!(preview_res.guard_preview.as_ref().unwrap().can_delete);
        assert!(preview_res.guard_preview.as_ref().unwrap().sole_owner);
        assert!(file.exists());

        // Confirmed delete removes the file cleanly.
        let deleted = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: true,
                reason: Some(DeletionReason::Disliked),
            },
        )
        .await
        .unwrap();
        assert_eq!(deleted.action, CurationAction::DeletedFromLibrary);
        assert!(deleted.deleted_from_disk);
        assert_eq!(deleted.starred, None);
        assert_eq!(deleted.rating, None);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn drop_protects_shared_track_and_unstars_for_caller_only() {
        let lib = Library::new().await;
        // Bob stars the shared track so Multi-Listener Delete Guard vetoes disk deletion.
        lib.fake.stars("bob", "s1");

        let file = lib.root.join("music/alice/A/x.flac");
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let alice = test_ctx("alice");

        let preview = guard_preview_in(&guard, &alice, "s1").await.unwrap();
        assert!(!preview.can_delete);
        assert!(!preview.sole_owner);
        assert_eq!(preview.other_listeners.len(), 1);
        assert_eq!(
            preview.other_listeners[0].kind,
            ListenerProtectionKind::OtherListenerMark
        );

        let res = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: true,
                reason: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(res.action, CurationAction::ProtectedAndUnstarred);
        assert!(!res.deleted_from_disk);
        assert_eq!(res.starred, Some(false));
        assert_eq!(res.rating, Some(1));
        assert!(file.exists(), "shared file must remain on disk");
        assert!(
            navi.stars_written
                .lock()
                .unwrap()
                .iter()
                .any(|(u, s, starred)| u == "alice" && s == "s1" && !*starred)
        );
        assert!(
            navi.ratings_written
                .lock()
                .unwrap()
                .iter()
                .any(|(u, s, r)| u == "alice" && s == "s1" && *r == 1)
        );
    }

    #[tokio::test]
    async fn drop_removes_pending_discovery_track_and_records_outcome() {
        let lib = Library::new().await;
        let staged = insert_pending_discovery(&lib, "dt-drop", "s1", "Marrow Choir/drop.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let alice = test_ctx("alice");

        let res = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(res.action, CurationAction::DroppedDiscovery);
        assert!(res.deleted_from_disk);
        assert_eq!(res.rating, None);
        assert!(navi.ratings_written.lock().unwrap().is_empty());
        assert!(!staged.exists());
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-drop")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Removed);
    }

    #[tokio::test]
    async fn drop_with_delete_veto_unknown_propagates_error_without_mutating_navidrome() {
        let lib = Library::new().await;
        // Bob's Navidrome account cannot be read, so Delete Guard returns DeleteVetoUnknown.
        lib.fake.unreadable("bob", || {
            NavidromeLookupError::Client(soulbeet::NavidromeError::Transport("offline".into()))
        });

        let file = lib.root.join("music/alice/A/x.flac");
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let alice = test_ctx("alice");

        let err = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: true,
                reason: None,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, FailureCode::DeleteVetoUnknown);
        assert!(file.exists(), "file must remain on disk on veto unknown");
        assert!(navi.stars_written.lock().unwrap().is_empty());
        assert!(navi.ratings_written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn drop_with_confirm_delete_false_never_mutates_navidrome_on_shared_track() {
        let lib = Library::new().await;
        lib.fake.stars("bob", "s1");

        let file = lib.root.join("music/alice/A/x.flac");
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let alice = test_ctx("alice");

        let res = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: None,
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(res.action, CurationAction::ConfirmationRequired);
        assert!(!res.deleted_from_disk);
        assert_eq!(res.starred, None);
        assert_eq!(res.rating, None);
        assert!(!res.guard_preview.as_ref().unwrap().can_delete);
        assert!(file.exists());
        assert!(navi.stars_written.lock().unwrap().is_empty());
        assert!(navi.ratings_written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn guard_preview_propagates_non_delete_failures_and_explicit_non_pending_errors() {
        let lib = Library::new().await;
        lib.fake.unreadable("alice", || {
            NavidromeLookupError::Client(soulbeet::NavidromeError::Transport("down".into()))
        });
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let alice = test_ctx("alice");

        let err = guard_preview_in(&guard, &alice, "s1").await.unwrap_err();
        assert_eq!(err.code, FailureCode::NavidromeUnreachable);

        // Explicit discovery_track_id that is not pending returns ExpiredHandle (track_not_pending).
        lib.fake.readable("alice");
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let queue = test_queue();
        let err = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("missing-dt".into()),
                confirm_delete: true,
                reason: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::ExpiredHandle);
    }

    #[tokio::test]
    async fn keep_and_rate_report_truthful_navidrome_state() {
        let lib = Library::new().await;
        lib.fake.stars("alice", "s1");
        let _staged = insert_pending_discovery(&lib, "dt-keep", "s1", "Marrow Choir/k.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let mut navi = FakeCurationNavi::new(lib.fake.clone());
        navi.fail_set_rating = true;
        let effects: Arc<dyn DiscoveryEffects> = Arc::new(StubDiscoveryEffects::default());
        let queue = test_queue();
        let alice = test_ctx("alice");

        // Passing empty song_id with explicit discovery_track_id resolves fallback song_id = "s1".
        // Failing set_rating must NOT falsely report applied rating = threshold (3); it preserves
        // the existing rating (1 in Library::new).
        let kept = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: String::new(),
                discovery_track_id: Some("dt-keep".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(kept.song_id, "s1");
        assert!(kept.promoted_discovery);
        assert_eq!(kept.starred, Some(true));
        assert_eq!(kept.rating, Some(1));

        // Clearing rating (0) normalizes rating to None.
        navi.fail_set_rating = false;
        let cleared = rate_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &RateTrackRequest {
                song_id: "s1".into(),
                rating: 0,
                discovery_track_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(cleared.rating, None);
        assert_eq!(cleared.starred, Some(true));
    }

    #[tokio::test]
    async fn keep_promotes_discovery_and_affirms_library_without_stamping_stars() {
        let lib = Library::new().await;
        // s2 is an unrated pending discovery track; s3 is an unrated permanent library track.
        lib.song("s2", "alice/Discovery/Balanced/unrated.flac", b"audio");
        lib.song("s3", "alice/A/lib.flac", b"audio");
        let staged =
            insert_pending_discovery(&lib, "dt-unrated", "s2", "Marrow Choir/unrated.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let effects_inner = Arc::new(StubDiscoveryEffects::default());
        let effects: Arc<dyn DiscoveryEffects> = effects_inner.clone();
        let queue = test_queue();
        let alice = test_ctx("alice");

        let kept_disc = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s2".into(),
                discovery_track_id: Some("dt-unrated".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(kept_disc.action, CurationAction::Kept);
        assert!(kept_disc.promoted_discovery);
        assert_eq!(kept_disc.rating, None);
        assert!(!staged.exists());
        assert!(navi.ratings_written.lock().unwrap().is_empty());

        // Permanent library track: keep does not stamp threshold stars either.
        let kept_lib = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s3".into(),
                discovery_track_id: None,
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(kept_lib.action, CurationAction::Kept);
        assert!(!kept_lib.promoted_discovery);
        assert_eq!(kept_lib.rating, None);
        assert!(navi.ratings_written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rate_on_pending_discovery_records_rating_without_promoting_or_deleting() {
        let lib = Library::new().await;
        let staged =
            insert_pending_discovery(&lib, "dt-rate", "s1", "Marrow Choir/rate.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let effects_inner = Arc::new(StubDiscoveryEffects::default());
        let effects: Arc<dyn DiscoveryEffects> = effects_inner.clone();
        let queue = test_queue();
        let alice = test_ctx("alice");

        // Rating 5 stars (>= threshold) records rating = 5 and does NOT promote.
        let r5 = rate_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &RateTrackRequest {
                song_id: "s1".into(),
                rating: 5,
                discovery_track_id: Some("dt-rate".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(r5.action, CurationAction::Rated);
        assert!(!r5.promoted_discovery);
        assert!(!r5.deleted_from_disk);
        assert_eq!(r5.rating, Some(5));
        assert!(staged.exists());
        assert!(effects_inner.imported.lock().unwrap().is_empty());
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-rate")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Pending);
        assert_eq!(row.rating, Some(5));

        // Rating 1 star records rating = 1 and does NOT delete.
        let r1 = rate_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &RateTrackRequest {
                song_id: "s1".into(),
                rating: 1,
                discovery_track_id: Some("dt-rate".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(r1.action, CurationAction::Rated);
        assert!(!r1.deleted_from_disk);
        assert_eq!(r1.rating, Some(1));
        assert_eq!(r1.guard_preview, None);
        assert!(staged.exists());
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-rate")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Pending);
        assert_eq!(row.rating, Some(1));

        // Clearing rating (0) resets discovery_tracks.rating to NULL.
        let r0 = rate_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &RateTrackRequest {
                song_id: "s1".into(),
                rating: 0,
                discovery_track_id: Some("dt-rate".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(r0.action, CurationAction::Rated);
        assert_eq!(r0.rating, None);
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-rate")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Pending);
        assert_eq!(row.rating, None);
    }

    #[tokio::test]
    async fn keep_star_and_drop_are_idempotent_on_duplicate_requests() {
        let lib = Library::new().await;
        let _staged = insert_pending_discovery(&lib, "dt-dup", "s1", "Marrow Choir/dup.flac").await;
        let _staged_drop =
            insert_pending_discovery(&lib, "dt-dup-drop", "s1", "Marrow Choir/dup2.flac").await;
        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let effects_inner = Arc::new(StubDiscoveryEffects::default());
        let effects: Arc<dyn DiscoveryEffects> = effects_inner.clone();
        let queue = test_queue();
        let alice = test_ctx("alice");

        // First keep promotes the track.
        let first = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(first.action, CurationAction::Kept);
        assert!(first.promoted_discovery);
        assert_eq!(effects_inner.imported.lock().unwrap().len(), 1);

        // Second keep on already-Promoted track succeeds idempotently without re-importing.
        let second = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(second.action, CurationAction::Kept);
        assert!(second.promoted_discovery);
        assert_eq!(effects_inner.imported.lock().unwrap().len(), 1);

        // Star + Keep on already-Promoted track also succeeds idempotently.
        let starred = star_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &StarTrackRequest {
                song_id: "s1".into(),
                starred: true,
                discovery_track_id: Some("dt-dup".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(starred.action, CurationAction::Starred);
        assert!(starred.promoted_discovery);
        assert_eq!(effects_inner.imported.lock().unwrap().len(), 1);

        // While status is Promoting (in-flight), keep also succeeds idempotently.
        sqlx::query("UPDATE discovery_tracks SET status = 'Promoting' WHERE id = 'dt-dup'")
            .execute(&lib.db)
            .await
            .unwrap();
        let in_flight = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(in_flight.action, CurationAction::Kept);
        assert!(in_flight.promoted_discovery);

        // Explicit drop on a Promoted/Promoting track is still rejected with ExpiredHandle.
        let err = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup".into()),
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::ExpiredHandle);

        // First drop removes dt-dup-drop; duplicate drop succeeds idempotently.
        let d1 = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup-drop".into()),
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(d1.action, CurationAction::DroppedDiscovery);
        let d2 = drop_track_in(
            &guard,
            &navi,
            &queue,
            &alice,
            &DropTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup-drop".into()),
                confirm_delete: false,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(d2.action, CurationAction::DroppedDiscovery);

        // Keep on a Removed track is still rejected with ExpiredHandle.
        let err = keep_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &KeepTrackRequest {
                song_id: "s1".into(),
                discovery_track_id: Some("dt-dup-drop".into()),
                target_folder_id: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::ExpiredHandle);
    }

    #[tokio::test]
    async fn rate_track_writes_db_before_navidrome_and_rolls_back_on_error() {
        let lib = Library::new().await;
        let _staged =
            insert_pending_discovery(&lib, "dt-order", "s1", "Marrow Choir/order.flac").await;
        sqlx::query("UPDATE discovery_tracks SET rating = 4 WHERE id = 'dt-order'")
            .execute(&lib.db)
            .await
            .unwrap();

        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let mut navi = FakeCurationNavi::new(lib.fake.clone());
        navi.fail_set_rating = true;
        navi.inspect_db = Some((lib.db.clone(), "dt-order".into()));
        let effects: Arc<dyn DiscoveryEffects> = Arc::new(StubDiscoveryEffects::default());
        let queue = test_queue();
        let alice = test_ctx("alice");

        let err = rate_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &RateTrackRequest {
                song_id: "s1".into(),
                rating: 5,
                discovery_track_id: Some("dt-order".into()),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::NavidromeUnreachable);
        // SQLite must have already held the new rating (5) when set_rating was called.
        assert_eq!(
            *navi.observed_db_rating_on_set.lock().unwrap(),
            Some(Some(5))
        );
        // After set_rating failed, SQLite must have restored the prior rating (4).
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-order")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.rating, Some(4));
    }

    #[tokio::test]
    async fn star_track_is_idempotent_when_promoted_track_song_id_was_rescanned() {
        let lib = Library::new().await;
        let _staged = insert_pending_discovery(
            &lib,
            "dt-moved",
            "old-gone-song-id",
            "Marrow Choir/moved.flac",
        )
        .await;
        sqlx::query("UPDATE discovery_tracks SET status = 'Promoted', rating = 4 WHERE id = 'dt-moved'")
            .execute(&lib.db)
            .await
            .unwrap();

        let music_root = lib.root.join("music");
        let guard = Guard::new(&lib.db, &*lib.fake, Some(&music_root));
        let navi = FakeCurationNavi::new(lib.fake.clone());
        let effects: Arc<dyn DiscoveryEffects> = Arc::new(StubDiscoveryEffects::default());
        let queue = test_queue();
        let alice = test_ctx("alice");

        // old-gone-song-id does not exist in FakeNavidrome (simulates Navidrome rescan after move).
        let res = star_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &StarTrackRequest {
                song_id: "old-gone-song-id".into(),
                starred: true,
                discovery_track_id: Some("dt-moved".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(res.action, CurationAction::Starred);
        assert!(res.promoted_discovery);
        assert_eq!(res.rating, Some(4));
        assert!(navi.stars_written.lock().unwrap().is_empty());

        // Same for Promoting (in-flight) when old song_id is already gone.
        sqlx::query("UPDATE discovery_tracks SET status = 'Promoting' WHERE id = 'dt-moved'")
            .execute(&lib.db)
            .await
            .unwrap();
        let res_promoting = star_track_in(
            &guard,
            &navi,
            &effects,
            &queue,
            &alice,
            &StarTrackRequest {
                song_id: "old-gone-song-id".into(),
                starred: true,
                discovery_track_id: Some("dt-moved".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(res_promoting.action, CurationAction::Starred);
        assert!(res_promoting.promoted_discovery);
    }
}
