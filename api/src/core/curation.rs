//! Unified track curation (Keep, Star, Rate, Drop) and Multi-Listener Delete Guard previews
//! shared by Web, Desktop, and Android clients.

use std::sync::Arc;

use async_trait::async_trait;
use shared::client::{
    CurationAction, CurationResult, DeleteGuardPreview, DropTrackRequest, KeepTrackRequest,
    ListenerProtection, ListenerProtectionKind, RateTrackRequest, StarTrackRequest,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::{DeletionReason, DiscoveryTrack};
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::StarTarget;
use tracing::warn;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::{pending_track_of, song_not_found, ForgetVia, Guard, Origin, Verdict};
use crate::core::discovery::{promote_to, remove_in, DiscoveryEffects, Replace};
use crate::core::index::ServerNavidrome;
use crate::core::library::{navidrome_failure, navidrome_for};
use crate::import_queue::ImportQueue;
use crate::models::discovery_history::DiscoveryHistoryRow;
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

/// Resolves whether `song_id` (or explicit `discovery_track_id`) corresponds to a Pending
/// discovery track owned by `ctx`.
async fn find_pending_discovery(
    guard: &Guard<'_>,
    ctx: &UserCtx,
    song_id: &str,
    explicit_discovery_id: Option<&str>,
) -> Result<Option<DiscoveryTrack>, Failure> {
    let pending = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;

    if let Some(track_id) = explicit_discovery_id.filter(|s| !s.is_empty()) {
        if let Some(track) = pending.into_iter().find(|t| t.id == track_id) {
            return Ok(Some(track));
        }
        return Ok(None);
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
    match pending_track_of(guard.db, guard.music_path, &user, &native, &pending).await {
        Ok(found) => Ok(found.cloned()),
        Err(_) => Ok(None),
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
        Err(failure) if failure.code == FailureCode::UnknownHandle => Err(failure),
        Err(failure) => Ok(preview_from_blocked(song_id, &failure)),
    }
}

/// Keeps/promotes a track. Pending Discovery tracks are moved into the permanent library and
/// rated at least `discovery_promote_threshold`. Permanent tracks are affirmed at the threshold.
pub(crate) async fn keep_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &KeepTrackRequest,
) -> Result<CurationResult, Failure> {
    let settings = UserSettings::get_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let threshold = settings.discovery_promote_threshold.clamp(1, 5);
    let user = ctx.id();

    if let Some(track) = find_pending_discovery(
        guard,
        ctx,
        &req.song_id,
        req.discovery_track_id.as_deref(),
    )
    .await?
    {
        let (promoted, _) = promote_to(
            guard.db,
            effects,
            queue,
            &user,
            &track.id,
            req.target_folder_id.as_deref(),
            Replace::Guarded(Origin::Ui),
        )
        .await
        .map_err(|e| e.into_failure())?;

        let mut applied_rating = None;
        if !req.song_id.is_empty() {
            match navi.get_song(ctx, &req.song_id).await {
                Ok(Some(song)) => {
                    let current = song.user_rating.unwrap_or(0);
                    let target = current.max(threshold);
                    if target > current {
                        if let Err(e) = navi.set_rating(ctx, &req.song_id, target).await {
                            warn!(
                                "Promoted '{}' but could not raise Navidrome rating: {}",
                                promoted.title, e.message
                            );
                        }
                    }
                    applied_rating = Some(target);
                }
                Ok(None) | Err(_) => {}
            }
        }

        // PR 1C will plug the On-Keep archival FLAC upgrade worker in here when a track was
        // fast-acquired in lossy quality.
        let archival_upgrade_queued = false;

        return Ok(CurationResult {
            song_id: req.song_id.clone(),
            action: CurationAction::Kept,
            promoted_discovery: true,
            starred: None,
            rating: applied_rating,
            deleted_from_disk: false,
            archival_upgrade_queued,
            guard_preview: None,
            message: format!("Saved '{}' to your library", promoted.title),
        });
    }

    let song = navi
        .get_song(ctx, &req.song_id)
        .await?
        .ok_or_else(song_not_found)?;
    let current = song.user_rating.unwrap_or(0);
    let target = current.max(threshold);
    if target > current {
        navi.set_rating(ctx, &req.song_id, target).await?;
    }

    Ok(CurationResult {
        song_id: req.song_id.clone(),
        action: CurationAction::Kept,
        promoted_discovery: false,
        starred: Some(song.starred.is_some()),
        rating: Some(target),
        deleted_from_disk: false,
        archival_upgrade_queued: false,
        guard_preview: None,
        message: format!("Kept '{}' in your library", song.title),
    })
}

/// Stars or unstars a track. Starring a Pending Discovery track automatically promotes/keeps it
/// into the permanent library (locked design decision).
pub(crate) async fn star_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &StarTrackRequest,
) -> Result<CurationResult, Failure> {
    let song = navi
        .get_song(ctx, &req.song_id)
        .await?
        .ok_or_else(song_not_found)?;
    navi.set_star(ctx, &req.song_id, req.starred).await?;

    let mut promoted_discovery = false;
    let mut applied_rating = song.user_rating.filter(|r| *r > 0);
    let archival_upgrade_queued = false;

    if req.starred {
        if let Some(track) = find_pending_discovery(
            guard,
            ctx,
            &req.song_id,
            req.discovery_track_id.as_deref(),
        )
        .await?
        {
            let settings = UserSettings::get_in(guard.db, &ctx.user_id)
                .await
                .map_err(|e| database_failure(Stage::Input, e))?;
            let threshold = settings.discovery_promote_threshold.clamp(1, 5);
            let user = ctx.id();

            promote_to(
                guard.db,
                effects,
                queue,
                &user,
                &track.id,
                None,
                Replace::Guarded(Origin::Ui),
            )
            .await
            .map_err(|e| e.into_failure())?;
            promoted_discovery = true;

            let current = song.user_rating.unwrap_or(0);
            let target = current.max(threshold);
            if target > current {
                if let Err(e) = navi.set_rating(ctx, &req.song_id, target).await {
                    warn!(
                        "Starred and promoted '{}' but could not raise rating: {}",
                        track.title, e.message
                    );
                } else {
                    applied_rating = Some(target);
                }
            }
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
        song_id: req.song_id.clone(),
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

/// Sets a 0–5★ rating on a track. On a Pending Discovery track, rating >= promote_threshold
/// immediately promotes it into the permanent library and rating == 1 drops it from staging.
pub(crate) async fn rate_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &RateTrackRequest,
) -> Result<CurationResult, Failure> {
    if req.rating > 5 {
        return Err(invalid_rating());
    }
    let song = navi
        .get_song(ctx, &req.song_id)
        .await?
        .ok_or_else(song_not_found)?;
    navi.set_rating(ctx, &req.song_id, req.rating).await?;

    let settings = UserSettings::get_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let threshold = settings.discovery_promote_threshold.clamp(1, 5);
    let user = ctx.id();

    if req.rating > 0 {
        if let Some(track) = find_pending_discovery(
            guard,
            ctx,
            &req.song_id,
            req.discovery_track_id.as_deref(),
        )
        .await?
        {
            if req.rating >= threshold {
                promote_to(
                    guard.db,
                    effects,
                    queue,
                    &user,
                    &track.id,
                    None,
                    Replace::Guarded(Origin::Ui),
                )
                .await
                .map_err(|e| e.into_failure())?;
                return Ok(CurationResult {
                    song_id: req.song_id.clone(),
                    action: CurationAction::Rated,
                    promoted_discovery: true,
                    starred: Some(song.starred.is_some()),
                    rating: Some(req.rating),
                    deleted_from_disk: false,
                    archival_upgrade_queued: false,
                    guard_preview: None,
                    message: format!(
                        "Rated '{}' {}★ and saved to your library",
                        song.title, req.rating
                    ),
                });
            }
            if req.rating == 1 {
                remove_in(guard.db, queue, &user, &track.id)
                    .await
                    .map_err(|e| e.into_failure())?;
                return Ok(CurationResult {
                    song_id: req.song_id.clone(),
                    action: CurationAction::DroppedDiscovery,
                    promoted_discovery: false,
                    starred: Some(false),
                    rating: Some(1),
                    deleted_from_disk: true,
                    archival_upgrade_queued: false,
                    guard_preview: None,
                    message: format!("Dropped '{}' from Discovery", song.title),
                });
            }
        }
    }

    let guard_preview = if req.rating == 1 && settings.auto_delete_enabled {
        guard_preview_in(guard, ctx, &req.song_id).await.ok()
    } else {
        None
    };

    Ok(CurationResult {
        song_id: req.song_id.clone(),
        action: CurationAction::Rated,
        promoted_discovery: false,
        starred: Some(song.starred.is_some()),
        rating: Some(req.rating),
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
/// or handles removal of a Permanent Library track through Soulbeet's Multi-Listener Delete Guard:
/// - Sole owner + `confirm_delete == true`: deletes file via `Guard::carry_out`.
/// - Sole owner + `confirm_delete == false`: returns `ConfirmationRequired` with preview.
/// - Protected by another listener: never deletes the file; unstars & sets 1★ for the caller.
pub(crate) async fn drop_track_in(
    guard: &Guard<'_>,
    navi: &dyn CurationNavidrome,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: &DropTrackRequest,
) -> Result<CurationResult, Failure> {
    let user = ctx.id();

    // Direct drop by explicit discovery track id if pending.
    if let Some(track) = find_pending_discovery(
        guard,
        ctx,
        &req.song_id,
        req.discovery_track_id.as_deref(),
    )
    .await?
    {
        return drop_pending_discovery(guard, navi, queue, ctx, &req.song_id, track).await;
    }

    match guard.check_song(&user, &req.song_id).await {
        Ok(Verdict::Discovery { track, .. }) => {
            drop_pending_discovery(guard, navi, queue, ctx, &req.song_id, track).await
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
                starred: Some(false),
                rating: Some(1),
                deleted_from_disk: true,
                archival_upgrade_queued: false,
                guard_preview: Some(preview),
                message: format!("Deleted '{}' from library", title),
            })
        }
        Err(failure)
            if matches!(
                failure.code,
                FailureCode::DeleteBlockedByOtherUser | FailureCode::DeleteVetoUnknown
            ) =>
        {
            let preview = preview_from_blocked(&req.song_id, &failure);
            // Protect the shared file: unstar and rate 1★ for this listener only so it drops
            // out of their personal mixes without affecting anyone else.
            if let Err(e) = navi.set_star(ctx, &req.song_id, false).await {
                warn!(
                    "Could not unstar protected song {} during drop: {}",
                    req.song_id, e.message
                );
            }
            if let Err(e) = navi.set_rating(ctx, &req.song_id, 1).await {
                warn!(
                    "Could not set 1★ on protected song {} during drop: {}",
                    req.song_id, e.message
                );
            }

            Ok(CurationResult {
                song_id: req.song_id.clone(),
                action: CurationAction::ProtectedAndUnstarred,
                promoted_discovery: false,
                starred: Some(false),
                rating: Some(1),
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
    remove_in(guard.db, queue, &user, &track.id)
        .await
        .map_err(|e| e.into_failure())?;

    if let Err(e) = DiscoveryHistoryRow::update_outcome_in(
        guard.db,
        &ctx.user_id,
        &track.artist,
        &track.title,
        "removed",
    )
    .await
    {
        warn!(
            "Failed to record rejected discovery outcome for '{}': {e}",
            track.title
        );
    }

    if !song_id.is_empty() {
        let _ = navi.set_star(ctx, song_id, false).await;
        let _ = navi.set_rating(ctx, song_id, 1).await;
    }

    Ok(CurationResult {
        song_id: song_id.to_string(),
        action: CurationAction::DroppedDiscovery,
        promoted_discovery: false,
        starred: Some(false),
        rating: Some(1),
        deleted_from_disk: true,
        archival_upgrade_queued: false,
        guard_preview: None,
        message: format!("Dropped '{}' from Discovery", track.title),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    use shared::navidrome::{DiscoveryProgress, DiscoveryStatus};

    use super::*;
    use crate::auth::UserId;
    use crate::core::delete_guard::test_support::{FakeNavidrome, Library};
    use crate::core::delete_guard::Listeners;
    use crate::core::test_ctx;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};

    struct FakeCurationNavi {
        fake: Arc<FakeNavidrome>,
        stars_written: Mutex<Vec<(String, String, bool)>>,
        ratings_written: Mutex<Vec<(String, String, u8)>>,
    }

    impl FakeCurationNavi {
        fn new(fake: Arc<FakeNavidrome>) -> Self {
            Self {
                fake,
                stars_written: Mutex::new(Vec::new()),
                ratings_written: Mutex::new(Vec::new()),
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
        assert_eq!(res.rating, Some(3));
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
        assert!(!staged.exists());
        let row = DiscoveryTrackRow::get_by_id_in(&lib.db, "dt-drop")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DiscoveryStatus::Removed);

        // Make sure unused import warning doesn't happen on HashMap if omitted later.
        let _ = HashMap::<String, String>::new();
    }
}
