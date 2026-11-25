//! Discovery for one user: its runs, its pending tracks and their expiry, and keeping or
//! removing a pending track. The web UI's server functions and the MCP tools call the same code.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::{
    DiscoveryProgress, DiscoveryStatus, DiscoveryTrack, GenerationStatus, LastRun, ProfilePhase,
    ProfileProgress,
};
use shared::recommendation::DiscoveryProfile;
use soulbeet::navidrome::NavidromeClient;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::delete_guard::paths;
use crate::core::delete_guard::staging::{
    claim_removal_in, remove_claimed_in, remove_staged_in, StagedOutcome,
};
use crate::core::delete_guard::{forget_detached, Origin};
use crate::core::folders::folder_not_found;
use crate::core::index::library_songs;
use crate::core::library::{known_roots_in, navidrome_failure, place_in, NavidromePlace};
use crate::import_queue::ImportQueue;
use crate::models::discovery_history::DiscoveryHistoryRow;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::engine_report::EngineReportRow;
use crate::models::folder::Folder;
use crate::models::user_settings::UserSettings;

/// How a promote treats an earlier copy of the song in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Replace {
    /// beets may replace it, through the delete guard with this origin.
    Guarded(Origin),
    /// beets skips a song the library has: the promote ends `import_already_in_library`.
    Never,
}

/// What discovery does outside Soulful's database, so tests run it on stand-ins.
#[async_trait]
pub(crate) trait DiscoveryEffects: Send + Sync {
    /// Records `progress` as `user`'s run by claim_run's rule.
    async fn claim(&self, user: &UserId, progress: DiscoveryProgress) -> Result<(), Failure>;
    /// `user`'s last recorded run.
    async fn current(&self, user: &UserId) -> Option<DiscoveryProgress>;
    /// Runs the generation for `user` in its own task and records its end.
    fn generate(&self, user: &UserId);
    /// Imports `src` into the library folder `target` for `actor` and waits for it.
    async fn import(
        &self,
        src: &Path,
        target: &Path,
        actor: &UserId,
        replace: Replace,
    ) -> Result<(), Failure>;
}

pub(crate) fn generation_running() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery generation already in progress",
    )
}

pub(crate) fn discovery_disabled() -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery is not enabled",
    )
}

pub(crate) fn choose_discovery_folder() -> UserAction {
    UserAction::new(
        UserActionCode::AddFolder,
        "Choose a discovery folder",
        "Settings > Library > Discovery",
    )
}

/// The user's settings name no discovery folder.
pub(crate) fn discovery_folder_required() -> Failure {
    Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "No download folder configured for discovery",
    )
    .with_user_action(choose_discovery_folder())
}

fn discovery_track_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery track not found",
    )
}

/// Another call holds the track or ended it (D16).
pub(crate) fn track_not_pending() -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Track is no longer pending (already promoted, removed, or being promoted)",
    )
}

/// Whether `f`, from a promote or a staging removal, is `track_not_pending`: the only
/// expired_handle either gives.
pub(crate) fn is_not_pending(f: &Failure) -> bool {
    f.code == FailureCode::ExpiredHandle
}

pub(crate) fn promote_file_gone() -> Failure {
    Failure::new(
        FailureCode::ImportFilesGone,
        Stage::Import,
        RetryAdvice::NotRetryable,
        "The downloaded file is gone",
    )
}

/// A promote task that panicked, coded like a panicked import job.
fn promote_stopped(e: &tokio::task::JoinError) -> Failure {
    Failure::new(
        FailureCode::ImportBeetsFailed,
        Stage::Import,
        RetryAdvice::SameSourceLater,
        format!("Promote stopped: {e}"),
    )
}

/// Records `progress` as `user_id`'s run unless one is still running, in which case nothing
/// changes. The caller holds the lock of `runs`, so two starts cannot both pass.
pub(crate) fn claim_run(
    runs: &mut HashMap<String, DiscoveryProgress>,
    user_id: &str,
    progress: DiscoveryProgress,
) -> Result<(), Failure> {
    if runs.get(user_id).is_some_and(|p| !p.is_terminal()) {
        return Err(generation_running());
    }
    runs.insert(user_id.to_string(), progress);
    Ok(())
}

/// The profiles a settings value names, the default profile when it names none.
pub(crate) fn parse_profiles(s: &str) -> Vec<DiscoveryProfile> {
    let mut profiles = Vec::new();
    for part in s.split(',') {
        if let Ok(p) = part.trim().parse() {
            profiles.push(p);
        }
    }
    if profiles.is_empty() {
        profiles.push(DiscoveryProfile::default());
    }
    profiles
}

/// `p` while it is shown: a run that ended is not shown once more than 5 whole minutes have
/// passed since it ended.
pub(crate) fn current_progress(
    p: &DiscoveryProgress,
    now: DateTime<Utc>,
) -> Option<DiscoveryProgress> {
    if p.is_terminal() {
        if let Some(completed_at) = &p.completed_at {
            if let Ok(ts) = DateTime::parse_from_rfc3339(completed_at) {
                if now.signed_duration_since(ts).num_minutes() > 5 {
                    return None;
                }
            }
        }
    }
    Some(p.clone())
}

/// The moment the automation expires `track`: its `created_at` (RFC 3339, or SQLite's
/// "%Y-%m-%d %H:%M:%S" in UTC) plus its profile's lifetime in days, 7 when `lifetime_days` has
/// none. None when `created_at` does not parse.
pub(crate) fn expires_at(
    track: &DiscoveryTrack,
    lifetime_days: &HashMap<String, u32>,
) -> Option<DateTime<Utc>> {
    let created = DateTime::parse_from_rfc3339(&track.created_at)
        .map(|t| t.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&track.created_at, "%Y-%m-%d %H:%M:%S")
                .map(|naive| naive.and_utc())
        })
        .ok()?;
    let days = lifetime_of(lifetime_days, &track.profile);
    Some(created + Duration::days(days.into()))
}

/// `profile`'s lifetime in days in `lifetime_days`, 7 when it has none.
pub(crate) fn lifetime_of(lifetime_days: &HashMap<String, u32>, profile: &str) -> u32 {
    lifetime_days.get(profile).copied().unwrap_or(7)
}

/// Removes, as expired (remove_staged_in), each pending track of `folder` in `expirable` whose
/// expires_at is at or before `now`. `expirable` is what the cycle's rating sync found not rated
/// to keep (SyncReport::expirable): any other track waits for a sync that tells. A track that
/// cannot be removed is logged and stays. The number expired.
pub(crate) async fn expire_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    folder: &Folder,
    lifetime_days: &HashMap<String, u32>,
    now: DateTime<Utc>,
    expirable: &HashSet<String>,
) -> Result<u32, String> {
    let pending = DiscoveryTrackRow::get_pending_by_folder_in(db, &folder.id).await?;
    let mut expired = 0;
    for track in &pending {
        if !expirable.contains(&track.id) {
            continue;
        }
        let Some(expires) = expires_at(track, lifetime_days) else {
            continue;
        };
        if now < expires {
            continue;
        }
        match remove_staged_in(db, queue, track, folder, StagedOutcome::Expired).await {
            Ok(()) => expired += 1,
            Err(f) if is_not_pending(&f) => debug!(
                "{} of user {} is no longer pending, so it does not expire",
                track.title, folder.user_id
            ),
            Err(f) => warn!(
                "Could not expire {} for user {}: {}",
                track.title,
                folder.user_id,
                crate::core::logged(&f)
            ),
        }
    }
    Ok(expired)
}

/// Removes (remove_staged_in) the staged file of each pending track in every folder of the
/// account `user_id`, before that account is deleted: its folders and tracks go with it, and no
/// expiry would find those files again. A track that cannot be removed is logged and keeps its
/// file, as in expire_in.
pub(crate) async fn remove_staged_of_user_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    user_id: &str,
) -> Result<(), Failure> {
    let folders = (Folder::get_all_by_user_in(db, user_id).await)
        .map_err(|e| database_failure(Stage::Delete, e))?;
    for folder in &folders {
        let pending = (DiscoveryTrackRow::get_pending_by_folder_in(db, &folder.id).await)
            .map_err(|e| database_failure(Stage::Delete, e))?;
        for track in &pending {
            match remove_staged_in(db, queue, track, folder, StagedOutcome::Removed).await {
                Ok(()) => {}
                Err(f) if is_not_pending(&f) => debug!(
                    "{} of user {user_id} is no longer pending, so its file stays",
                    track.title
                ),
                Err(f) => warn!(
                    "Could not remove {} of user {user_id}, whose account is being deleted: {}",
                    track.title,
                    crate::core::logged(&f)
                ),
            }
        }
    }
    Ok(())
}

/// Checks that `ctx` can start a run (discovery on, a discovery folder chosen), claims it on
/// `effects` with each selected profile waiting, or skipped when it already holds its track
/// count of pending tracks, and starts the generation. The progress claimed.
pub(crate) async fn start_in(
    db: &SqlitePool,
    effects: &dyn DiscoveryEffects,
    ctx: &UserCtx,
) -> Result<DiscoveryProgress, Failure> {
    let settings = crate::core::settings::get_in(db, ctx).await?;
    if !settings.discovery_enabled {
        return Err(discovery_disabled());
    }
    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();
    let folder_id = settings
        .discovery_folder()
        .ok_or_else(discovery_folder_required)?;
    let mut profiles = Vec::new();
    for profile in &selected_profiles {
        let profile = profile.to_string();
        let target = track_counts.get(&profile).copied().unwrap_or(10) as usize;
        let existing =
            DiscoveryTrackRow::get_pending_by_folder_and_profile_in(db, folder_id, &profile)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
        let phase = match target.saturating_sub(existing) {
            0 => ProfilePhase::Skipped,
            _ => ProfilePhase::Waiting,
        };
        profiles.push(ProfileProgress {
            profile,
            phase,
            current: 0,
            total: 0,
        });
    }
    let progress = DiscoveryProgress {
        status: GenerationStatus::Running,
        profiles,
        started_at: Some(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    // Claimed before the generation starts, so its first progress update finds the run.
    effects.claim(&ctx.id(), progress.clone()).await?;
    effects.generate(&ctx.id());
    Ok(progress)
}

/// What get_discovery shows of a user's discovery.
pub(crate) struct Overview {
    pub settings: UserSettings,
    /// The discovery folder, when the settings name one of the user's own folders.
    pub folder: Option<Folder>,
    /// Its pending tracks, soonest to expire first; those whose expiry cannot be read last.
    pub pending: Vec<DiscoveryTrack>,
    /// The current run, or one that ended in the last 5 minutes.
    pub run: Option<DiscoveryProgress>,
    /// The latest recommendation run, as the Discovery page shows it. The automatic cycle's
    /// runs have no `run`, so this is the only place their outcome shows.
    pub last_run: Option<LastRun>,
}

/// `ctx`'s discovery: settings, folder, pending tracks, current run and last recommendation run.
pub(crate) async fn overview_in(
    db: &SqlitePool,
    effects: &dyn DiscoveryEffects,
    ctx: &UserCtx,
) -> Result<Overview, Failure> {
    let settings = crate::core::settings::get_in(db, ctx).await?;
    let folder = match settings.discovery_folder() {
        Some(id) => Folder::get_by_id_in(db, id)
            .await
            .map_err(|e| database_failure(Stage::Input, e))?
            .filter(|f| f.user_id == ctx.user_id),
        None => None,
    };
    let mut pending = match &folder {
        Some(folder) => DiscoveryTrackRow::get_pending_by_folder_in(db, &folder.id)
            .await
            .map_err(|e| database_failure(Stage::Input, e))?,
        None => vec![],
    };
    let lifetimes = settings.parse_lifetime_days();
    pending.sort_by_key(|t| {
        let expires = expires_at(t, &lifetimes);
        (expires.is_none(), expires)
    });
    let run = (effects.current(&ctx.id()).await).and_then(|p| current_progress(&p, Utc::now()));
    let reports = EngineReportRow::latest_per_profile_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(Overview {
        settings,
        folder,
        pending,
        run,
        last_run: crate::core::recommend::last_run_of(reports),
    })
}

/// The track's folder when `user_id` owns it: `folder_not_found` for none, and for another
/// user's `discovery_track_not_found`, as for a track that does not exist: no id of another
/// user's tells that it exists.
fn track_folder_of(folder: Option<Folder>, user_id: &str) -> Result<Folder, Failure> {
    let folder = folder.ok_or_else(folder_not_found)?;
    if folder.user_id == user_id {
        Ok(folder)
    } else {
        Err(discovery_track_not_found())
    }
}

/// The track (`discovery_track_not_found` when missing) and its folder (`track_folder_of`).
/// Database errors are `database_failure(stage, e)`.
async fn owned_discovery_track_in(
    db: &SqlitePool,
    track_id: &str,
    user_id: &str,
    stage: Stage,
) -> Result<(DiscoveryTrack, Folder), Failure> {
    let track = DiscoveryTrackRow::get_by_id_in(db, track_id)
        .await
        .map_err(|e| database_failure(stage, e))?
        .ok_or_else(discovery_track_not_found)?;
    let folder = Folder::get_by_id_in(db, &track.folder_id)
        .await
        .map_err(|e| database_failure(stage, e))?;
    let folder = track_folder_of(folder, user_id)?;
    Ok((track, folder))
}

/// `owned_discovery_track_in` for a promote, which then marks the track Promoting:
/// `track_not_pending` when it is not Pending. The owner is checked first, so another user's
/// track is never left promoting.
async fn claim_for_promote_in(
    db: &SqlitePool,
    track_id: &str,
    user_id: &str,
) -> Result<(DiscoveryTrack, Folder), Failure> {
    let owned = owned_discovery_track_in(db, track_id, user_id, Stage::Import).await?;
    if !DiscoveryTrackRow::try_set_promoting_in(db, track_id)
        .await
        .map_err(|e| database_failure(Stage::Import, e))?
    {
        return Err(track_not_pending());
    }
    Ok(owned)
}

/// Why a promote or a removal of a discovery track was not done.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NotDone {
    /// Refused before the track was claimed: nothing changed.
    Unclaimed(Failure),
    /// Failed after the track was claimed.
    Claimed(Failure),
}

impl NotDone {
    pub(crate) fn into_failure(self) -> Failure {
        match self {
            NotDone::Unclaimed(f) | NotDone::Claimed(f) => f,
        }
    }
}

/// Moves `user`'s pending track `track_id` into its folder's library through `effects.import`
/// with `replace`, claiming it first (D16), and drops its item from its profile folder's library
/// through `queue`. Runs in its own task, so the status and history updates always follow the
/// import even when the caller goes away while the import waits in the library's queue. The
/// track as promoted.
pub(crate) async fn promote(
    db: &SqlitePool,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    user: &UserId,
    track_id: &str,
    replace: Replace,
) -> Result<DiscoveryTrack, NotDone> {
    promote_to(db, effects, queue, user, track_id, None, replace)
        .await
        .map(|(track, _)| track)
}

/// Like `promote`, optionally targeting another folder `target_folder_id` owned by `user`
/// instead of the track's discovery folder (for example promoting directly to a shared folder).
/// Returns the promoted track and the folder it was imported into.
pub(crate) async fn promote_to(
    db: &SqlitePool,
    effects: &Arc<dyn DiscoveryEffects>,
    queue: &ImportQueue,
    user: &UserId,
    track_id: &str,
    target_folder_id: Option<&str>,
    replace: Replace,
) -> Result<(DiscoveryTrack, Folder), NotDone> {
    let (db, effects, queue, user, track_id, target_folder_id) = (
        db.clone(),
        effects.clone(),
        queue.clone(),
        user.clone(),
        track_id.to_string(),
        target_folder_id.map(str::to_string),
    );
    tokio::spawn(async move {
        promote_detached(
            &db,
            &*effects,
            &queue,
            &user,
            &track_id,
            target_folder_id.as_deref(),
            replace,
        )
        .await
    })
    .await
    .unwrap_or_else(|e| Err(NotDone::Claimed(promote_stopped(&e))))
}

async fn promote_detached(
    db: &SqlitePool,
    effects: &dyn DiscoveryEffects,
    queue: &ImportQueue,
    user: &UserId,
    track_id: &str,
    target_folder_id: Option<&str>,
    replace: Replace,
) -> Result<(DiscoveryTrack, Folder), NotDone> {
    // Validate an explicit target folder before claiming the track, so an invalid folder leaves
    // the track untouched.
    let explicit_target = match target_folder_id {
        Some(id) => {
            let folder = Folder::get_by_id_in(db, id)
                .await
                .map_err(|e| NotDone::Unclaimed(database_failure(Stage::Import, e)))?
                .filter(|f| f.user_id == user.to_string())
                .ok_or_else(|| NotDone::Unclaimed(folder_not_found()))?;
            Some(folder)
        }
        None => None,
    };
    let (track, source_folder) = (claim_for_promote_in(db, track_id, &user.to_string()).await)
        .map_err(NotDone::Unclaimed)?;
    let target_folder = explicit_target.unwrap_or_else(|| source_folder.clone());
    promote_claimed(
        db,
        effects,
        queue,
        user,
        track,
        source_folder,
        target_folder.clone(),
        replace,
    )
    .await
    .map(|track| (track, target_folder))
    .map_err(NotDone::Claimed)
}

/// Derives the staging root directory for `track`: `Discovery/Seasonal/<slug>` when
/// `track.pack_slug` is set, otherwise `Discovery/<profile>`.
pub(crate) fn staging_root(folder: &Folder, track: &DiscoveryTrack) -> PathBuf {
    match track.pack_slug.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(slug) => Path::new(&folder.discovery_path())
            .join("Seasonal")
            .join(slug),
        None => PathBuf::from(folder.discovery_profile_path(&track.profile)),
    }
}

/// The promote of `track`, which claim_for_promote_in claimed. Once the import moved the staged
/// file out of its staging folder, the file's item is dropped from that folder's library
/// (forget_detached); a file the import left there, as beets' copy does, keeps its item.
#[allow(clippy::too_many_arguments)]
async fn promote_claimed(
    db: &SqlitePool,
    effects: &dyn DiscoveryEffects,
    queue: &ImportQueue,
    user: &UserId,
    track: DiscoveryTrack,
    source_folder: Folder,
    target_folder: Folder,
    replace: Replace,
) -> Result<DiscoveryTrack, Failure> {
    let track_id = track.id.as_str();
    let back_to_pending = || async {
        if let Err(e) =
            DiscoveryTrackRow::update_status_in(db, track_id, &DiscoveryStatus::Pending).await
        {
            warn!("Could not set '{}' back to pending: {e}", track.title);
        }
    };

    let src = PathBuf::from(&track.path);
    // The path beets has the file at: discovery imported it into the canonical staging folder.
    let Ok(staged) = tokio::fs::canonicalize(&src).await else {
        // Pending again, so the expiry handles it.
        back_to_pending().await;
        warn!("Promote of '{}': {} is gone", track.title, track.path);
        return Err(promote_file_gone());
    };

    let target = PathBuf::from(&target_folder.path);
    if let Err(failure) = effects.import(&src, &target, user, replace).await {
        back_to_pending().await;
        return Err(failure);
    }

    let profile_root = staging_root(&source_folder, &track);
    if let Some(parent) = src.parent() {
        let _ = crate::server_fns::cleanup_empty_ancestors(parent, &profile_root).await;
    }
    if matches!(tokio::fs::try_exists(&staged).await, Ok(false)) {
        forget_detached(queue, staged, profile_root);
    }

    DiscoveryTrackRow::update_status_in(db, track_id, &DiscoveryStatus::Promoted)
        .await
        .map_err(|e| database_failure(Stage::Import, e))?;

    if let Err(f) = crate::core::archival::maybe_enqueue_for_promoted_discovery_in(
        db,
        &source_folder.user_id,
        &target_folder.id,
        &track,
    )
    .await
    {
        warn!(
            "Failed to evaluate archival FLAC upgrade for promoted track '{}': {}",
            track.title,
            crate::core::logged(&f)
        );
    }

    if let Err(e) = DiscoveryHistoryRow::update_outcome_in(
        db,
        &source_folder.user_id,
        &track.artist,
        &track.title,
        "promoted",
    )
    .await
    {
        warn!(
            "Failed to update history for promoted track '{}': {}",
            track.title, e
        );
    }

    info!(
        "Promoted discovery track: {} -> {}",
        track.title, target_folder.path
    );
    Ok(DiscoveryTrack {
        status: DiscoveryStatus::Promoted,
        ..track
    })
}

/// Deletes `user`'s pending track `track_id` from its profile folder and marks it removed,
/// claiming it first (claim_removal_in, D16). The track as removed.
pub(crate) async fn remove_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    user: &UserId,
    track_id: &str,
) -> Result<DiscoveryTrack, NotDone> {
    let (track, folder) =
        (owned_discovery_track_in(db, track_id, &user.to_string(), Stage::Delete).await)
            .map_err(NotDone::Unclaimed)?;
    claim_removal_in(db, &track)
        .await
        .map_err(NotDone::Unclaimed)?;
    (remove_claimed_in(db, queue, &track, &folder, StagedOutcome::Removed).await)
        .map_err(NotDone::Claimed)?;
    Ok(DiscoveryTrack {
        status: DiscoveryStatus::Removed,
        ..track
    })
}

/// Where `folder`'s Discovery folder is in Navidrome (place_in over the known roots). When no
/// known root holds it, the folder's pending tracks teach one as Navidrome lists them, the way
/// an import's index wait learns a root (index::listed), until a root holds the folder. None
/// when none does: Navidrome's paths of the folder's songs are then unknown.
pub(crate) async fn discovery_place_in(
    db: &SqlitePool,
    navi: &NavidromeClient,
    music_path: Option<&Path>,
    folder: &Folder,
) -> Result<Option<NavidromePlace>, Failure> {
    let discovery = PathBuf::from(folder.discovery_path());
    if let Some(place) = place_in(&discovery, &known_roots_in(db, music_path).await?) {
        return Ok(Some(place));
    }
    let library = Path::new(&folder.path);
    let tracks = DiscoveryTrackRow::get_pending_by_folder_in(db, &folder.id)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;
    for track in tracks {
        let local = PathBuf::from(&track.path);
        let Ok(path) = local.strip_prefix(library) else {
            continue;
        };
        let item = ImportedItem {
            path: path.to_path_buf(),
            recording_mbid: None,
            artist: track.artist,
            title: track.title,
            album: track.album,
            beets_id: None,
            release_mbid: None,
            match_per_mille: None,
        };
        let found = library_songs(navi, library, &item)
            .await
            .map_err(|e| navidrome_failure(Stage::Playlist, &e))?;
        match paths::learn_in(db, &found, &local).await {
            Ok(learned) if learned.is_empty() => continue,
            Ok(_) => {}
            Err(e) => {
                warn!(
                    "Could not learn where Navidrome's libraries are on this machine from {}: {e}",
                    local.display()
                );
                continue;
            }
        }
        if let Some(place) = place_in(&discovery, &known_roots_in(db, music_path).await?) {
            return Ok(Some(place));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use shared::failure::Failure;
    use shared::navidrome::DiscoveryProgress;
    use tokio::sync::Notify;

    use super::{claim_run, DiscoveryEffects, Replace};
    use crate::auth::UserId;

    /// Runs kept in a map under claim_run, generations and imports recorded, imports answering
    /// Ok unless `fail_imports` gave a failure, held when `hold_imports` asked for it.
    #[derive(Default)]
    pub(crate) struct TestDiscovery {
        runs: Mutex<HashMap<String, DiscoveryProgress>>,
        generated: Mutex<Vec<String>>,
        imports: Mutex<Vec<(PathBuf, PathBuf, Replace)>>,
        hold: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
        import_failure: Mutex<Option<Failure>>,
    }

    impl TestDiscovery {
        /// The users a generation was started for, in order.
        pub(crate) fn generated(&self) -> Vec<String> {
            self.generated.lock().unwrap().clone()
        }

        /// Each import's source, target and rule, in order.
        pub(crate) fn imports(&self) -> Vec<(PathBuf, PathBuf, Replace)> {
            self.imports.lock().unwrap().clone()
        }

        /// From now on an import notifies the first when it is reached, then waits for the
        /// second.
        pub(crate) fn hold_imports(&self) -> (Arc<Notify>, Arc<Notify>) {
            let pair = (Arc::new(Notify::new()), Arc::new(Notify::new()));
            *self.hold.lock().unwrap() = Some(pair.clone());
            pair
        }

        /// From now on every import answers `failure`.
        pub(crate) fn fail_imports(&self, failure: Failure) {
            *self.import_failure.lock().unwrap() = Some(failure);
        }
    }

    #[async_trait]
    impl DiscoveryEffects for TestDiscovery {
        async fn claim(&self, user: &UserId, progress: DiscoveryProgress) -> Result<(), Failure> {
            claim_run(&mut self.runs.lock().unwrap(), &user.to_string(), progress)
        }

        async fn current(&self, user: &UserId) -> Option<DiscoveryProgress> {
            self.runs.lock().unwrap().get(&user.to_string()).cloned()
        }

        fn generate(&self, user: &UserId) {
            self.generated.lock().unwrap().push(user.to_string());
        }

        async fn import(
            &self,
            src: &Path,
            target: &Path,
            _actor: &UserId,
            replace: Replace,
        ) -> Result<(), Failure> {
            let entry = (src.to_path_buf(), target.to_path_buf(), replace);
            self.imports.lock().unwrap().push(entry);
            let hold = self.hold.lock().unwrap().clone();
            if let Some((reached, release)) = hold {
                reached.notify_one();
                release.notified().await;
            }
            match self.import_failure.lock().unwrap().clone() {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestDiscovery;
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::test_ctx;
    use crate::import_queue::test_support::stub_queue;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};
    use crate::test_navidrome::NavidromeStub;

    #[test]
    fn a_track_expires_its_profiles_lifetime_after_it_was_made() {
        let track = |created_at: &str, profile: &str| DiscoveryTrack {
            id: "t1".into(),
            song_id: None,
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            path: "/music/alice/Discovery/Balanced/03.flac".into(),
            folder_id: "alice-music".into(),
            profile: profile.into(),
            rating: None,
            status: DiscoveryStatus::Pending,
            created_at: created_at.into(),
            exclude_from_core_taste: false,
            reason: None,
            culture_bucket: None,
            pack_slug: None,
        };
        let lifetimes = HashMap::from([("Balanced".to_string(), 14u32)]);
        assert_eq!(
            expires_at(&track("2026-11-15 20:00:00", "Balanced"), &lifetimes)
                .map(|t| t.to_rfc3339()),
            Some("2026-11-29T20:00:00+00:00".into())
        );
        assert_eq!(
            expires_at(&track("2026-11-15T20:00:00+01:00", "Explore"), &lifetimes)
                .map(|t| t.to_rfc3339()),
            Some("2026-11-22T19:00:00+00:00".into())
        );
        assert_eq!(
            expires_at(&track("yesterday", "Balanced"), &lifetimes),
            None
        );
    }

    #[test]
    fn a_finished_run_is_shown_until_its_sixth_minute() {
        let done = DiscoveryProgress {
            status: GenerationStatus::Complete,
            completed_at: Some("2026-11-15T20:00:00+00:00".into()),
            ..Default::default()
        };
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert!(current_progress(&done, at("2026-11-15T20:05:59Z")).is_some());
        assert!(current_progress(&done, at("2026-11-15T20:06:00Z")).is_none());
        let running = DiscoveryProgress {
            status: GenerationStatus::Running,
            ..Default::default()
        };
        assert!(current_progress(&running, at("2026-11-16T20:00:00Z")).is_some());
    }

    #[tokio::test]
    async fn another_users_track_is_refused_before_it_is_claimed() {
        let db = crate::db::test_db::migrated().await;
        let folder = crate::core::folder_of(&db, "alice").await;
        sqlx::query(
            "INSERT INTO discovery_tracks (id, title, artist, album, path, folder_id)
             VALUES ('t1', 'Wake Up', 'Kowloon', 'Come Over', '/music/alice/x.flac', ?)",
        )
        .bind(&folder)
        .execute(&db)
        .await
        .unwrap();
        let claim = |user: &'static str| {
            let db = db.clone();
            async move {
                claim_for_promote_in(&db, "t1", user)
                    .await
                    .map(|(track, folder)| (track.id, folder.user_id))
            }
        };

        assert_eq!(claim("bob").await, Err(discovery_track_not_found()));
        // Still Pending, so its owner can promote it.
        assert_eq!(
            claim("alice").await,
            Ok(("t1".to_string(), "alice".to_string()))
        );
        assert_eq!(claim("alice").await, Err(track_not_pending()));
    }

    #[test]
    fn promote_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, stage, retry, message) in [
            (
                discovery_track_not_found(),
                UnknownHandle,
                Stage::Input,
                NotRetryable,
                "Discovery track not found",
            ),
            (
                track_not_pending(),
                ExpiredHandle,
                Stage::Input,
                NotRetryable,
                "Track is no longer pending (already promoted, removed, or being promoted)",
            ),
            (
                promote_file_gone(),
                ImportFilesGone,
                Stage::Import,
                NotRetryable,
                "The downloaded file is gone",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, stage, retry, message)
            );
        }
    }

    #[test]
    fn only_the_folder_owner_may_change_a_discovery_track() {
        let folder = |user_id: &str| Folder {
            id: "f1".into(),
            user_id: user_id.into(),
            name: "Music".into(),
            path: "/music".into(),
            shared: false,
        };
        assert_eq!(
            track_folder_of(Some(folder("user-b")), "user-a"),
            Err(discovery_track_not_found())
        );
        assert_eq!(track_folder_of(None, "user-a"), Err(folder_not_found()));
        assert_eq!(
            track_folder_of(Some(folder("user-a")), "user-a"),
            Ok(folder("user-a"))
        );
    }

    #[tokio::test]
    async fn a_promote_that_panics_is_an_import_that_stopped() {
        let e = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let f = promote_stopped(&e);
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater)
        );
        assert_eq!(f.message, format!("Promote stopped: {e}"));
    }

    #[test]
    fn a_second_start_is_refused_while_the_first_runs() {
        let running = || DiscoveryProgress {
            status: GenerationStatus::Running,
            ..Default::default()
        };
        let mut runs = HashMap::new();
        assert_eq!(claim_run(&mut runs, "user-a", running()), Ok(()));
        assert_eq!(
            claim_run(&mut runs, "user-a", running()),
            Err(generation_running())
        );
        assert_eq!(claim_run(&mut runs, "user-b", running()), Ok(()));
        runs.get_mut("user-a").unwrap().status = GenerationStatus::Complete;
        assert_eq!(claim_run(&mut runs, "user-a", running()), Ok(()));
    }

    #[tokio::test]
    async fn a_run_starts_only_when_discovery_is_on_with_a_folder_and_once_at_a_time() {
        let lib = Library::new().await;
        let (effects, ctx) = (TestDiscovery::default(), test_ctx("alice"));
        assert_eq!(
            start_in(&lib.db, &effects, &ctx).await.unwrap_err().message,
            "Discovery is not enabled"
        );
        UserSettings::upsert_in(
            &lib.db,
            "alice",
            UpdateUserSettings {
                discovery_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            start_in(&lib.db, &effects, &ctx).await.unwrap_err().message,
            "No download folder configured for discovery"
        );
        // How Settings clears the choice.
        let cleared = UpdateUserSettings {
            discovery_folder_id: Some(String::new()),
            ..Default::default()
        };
        UserSettings::upsert_in(&lib.db, "alice", cleared)
            .await
            .unwrap();
        assert_eq!(
            start_in(&lib.db, &effects, &ctx).await,
            Err(discovery_folder_required())
        );
        assert!(effects.generated().is_empty());
        UserSettings::upsert_in(
            &lib.db,
            "alice",
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let progress = start_in(&lib.db, &effects, &ctx).await.unwrap();
        assert_eq!(
            progress
                .profiles
                .iter()
                .map(|p| (p.profile.as_str(), p.phase.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("Conservative", ProfilePhase::Waiting),
                ("Balanced", ProfilePhase::Waiting),
                ("Adventurous", ProfilePhase::Waiting)
            ]
        );
        assert_eq!(effects.generated(), vec!["alice".to_string()]);
        assert_eq!(
            start_in(&lib.db, &effects, &ctx).await.unwrap_err().message,
            "Discovery generation already in progress"
        );
    }

    /// The native `GET /api/song` answer of one song of library 1 (at /navidrome/music, which
    /// is no folder here) at `path`, 5 bytes, modified at mtime() as the test files are.
    fn listed_song(path: &str) -> String {
        format!(
            r#"[{{"id":"f2c4d1a8b0e94c37a6d5e8b1c2a3f405","libraryId":1,"libraryPath":"/navidrome/music","path":"{path}",
        "title":"t1","album":"Winter Songs","artist":"Marrow Choir","albumArtist":"Marrow Choir","size":5,"suffix":"flac","missing":false,
        "updatedAt":"2026-11-15T10:01:30.123456789Z"}}]"#
        )
    }

    async fn folder(lib: &Library, id: &str) -> Folder {
        Folder::get_by_id_in(&lib.db, id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn each_users_discovery_folder_is_placed_in_the_library_a_listed_track_teaches() {
        let lib = Library::new().await;
        lib.write("alice/Discovery/Balanced/Marrow Choir/03.flac", b"audio");
        let staged = lib
            .root
            .join("music/alice/Discovery/Balanced/Marrow Choir/03.flac");
        lib.pending_track("t1", "alice-music", "Balanced", &staged, None)
            .await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/api/song",
            &[&listed_song(
                "alice/Discovery/Balanced/Marrow Choir/03.flac",
            )],
        );
        let alice = folder(&lib, "alice-music").await;
        let alice = discovery_place_in(&lib.db, &navi.client(), None, &alice)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            alice,
            NavidromePlace {
                library_id: 1,
                path: "alice/Discovery".into()
            }
        );
        assert_eq!(
            alice.child("Balanced").song_prefix(),
            "alice/Discovery/Balanced/"
        );
        let asked = navi.asked().len();

        // bob's folder is in the root alice's track taught, so Navidrome is not asked again.
        let bob = folder(&lib, "bob-music").await;
        let bob = discovery_place_in(&lib.db, &navi.client(), None, &bob)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bob.child("Balanced").song_prefix(),
            "bob/Discovery/Balanced/"
        );
        assert_eq!(navi.asked().len(), asked);
    }

    #[tokio::test]
    async fn a_discovery_folder_navidrome_lists_no_track_of_has_no_place() {
        let lib = Library::new().await;
        lib.write("alice/Discovery/Balanced/Marrow Choir/03.flac", b"audio");
        let staged = lib
            .root
            .join("music/alice/Discovery/Balanced/Marrow Choir/03.flac");
        lib.pending_track("t1", "alice-music", "Balanced", &staged, None)
            .await;
        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        let alice = folder(&lib, "alice-music").await;
        assert_eq!(
            discovery_place_in(&lib.db, &navi.client(), None, &alice)
                .await
                .unwrap(),
            None
        );
        // NAVIDROME_MUSIC_PATH places it without asking Navidrome.
        let asked = navi.asked().len();
        let music = lib.root.join("music");
        assert_eq!(
            discovery_place_in(&lib.db, &navi.client(), Some(music.as_path()), &alice)
                .await
                .unwrap(),
            Some(NavidromePlace {
                library_id: 1,
                path: "alice/Discovery".into()
            })
        );
        assert_eq!(navi.asked().len(), asked);
    }

    fn staged_file(lib: &Library) -> PathBuf {
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        staged
    }

    #[tokio::test]
    async fn another_users_track_is_neither_promoted_nor_removed() {
        let lib = Library::new().await;
        let staged = staged_file(&lib);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let effects: Arc<dyn DiscoveryEffects> = Arc::new(TestDiscovery::default());
        let (queue, stub) = stub_queue();
        let bob = UserId::new("bob");
        let ui = Replace::Guarded(Origin::Ui);
        assert_eq!(
            promote(&lib.db, &effects, &queue, &bob, "t1", ui)
                .await
                .unwrap_err(),
            NotDone::Unclaimed(discovery_track_not_found())
        );
        assert_eq!(
            remove_in(&lib.db, &queue, &bob, "t1").await.unwrap_err(),
            NotDone::Unclaimed(discovery_track_not_found())
        );
        assert!(staged.exists());
        assert_eq!(lib.track_status("t1").await, "Pending");
        assert!(stub.forgets().is_empty());
        let profile = std::fs::canonicalize(staged.parent().unwrap()).unwrap();
        let canonical = std::fs::canonicalize(&staged).unwrap();
        assert_eq!(
            remove_in(&lib.db, &queue, &lib.alice(), "t1")
                .await
                .unwrap()
                .id,
            "t1"
        );
        assert!(!staged.exists());
        assert_eq!(stub.forgets_once(1).await, vec![(profile, canonical)]);
    }

    #[tokio::test]
    async fn two_promotes_of_one_track_claim_it_once() {
        let lib = Library::new().await;
        let staged = staged_file(&lib);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let test = Arc::new(TestDiscovery::default());
        let (reached, release) = test.hold_imports();
        let effects: Arc<dyn DiscoveryEffects> = test.clone();
        let (queue, stub) = stub_queue();
        let ui = Replace::Guarded(Origin::Ui);
        let first = tokio::spawn({
            let (db, effects, alice) = (lib.db.clone(), effects.clone(), lib.alice());
            let (queue, ui) = (queue.clone(), ui.clone());
            async move { promote(&db, &effects, &queue, &alice, "t1", ui).await }
        });
        // The first holds the claim and waits in its import.
        reached.notified().await;
        assert_eq!(
            promote(&lib.db, &effects, &queue, &lib.alice(), "t1", ui)
                .await
                .unwrap_err(),
            NotDone::Unclaimed(track_not_pending())
        );
        assert_eq!(
            remove_in(&lib.db, &queue, &lib.alice(), "t1")
                .await
                .unwrap_err(),
            NotDone::Unclaimed(track_not_pending())
        );
        assert!(staged.exists());
        release.notify_one();
        assert_eq!(first.await.unwrap().unwrap().id, "t1");
        assert_eq!(
            (lib.track_status("t1").await, test.imports().len()),
            ("Promoted".into(), 1)
        );
        // The import left the staged file where it was, as beets' copy does, so its item stays.
        assert!(staged.exists() && stub.forgets().is_empty());
    }

    #[tokio::test]
    async fn a_promote_that_moved_the_staged_file_drops_its_item_from_the_profiles_library() {
        let lib = Library::new().await;
        let staged = staged_file(&lib);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let profile = std::fs::canonicalize(staged.parent().unwrap()).unwrap();
        let canonical = std::fs::canonicalize(&staged).unwrap();
        let test = Arc::new(TestDiscovery::default());
        let (reached, release) = test.hold_imports();
        let effects: Arc<dyn DiscoveryEffects> = test.clone();
        let (queue, stub) = stub_queue();
        let promoting = tokio::spawn({
            let (db, alice) = (lib.db.clone(), lib.alice());
            let (effects, queue) = (effects.clone(), queue.clone());
            let ui = Replace::Guarded(Origin::Ui);
            async move { promote(&db, &effects, &queue, &alice, "t1", ui).await }
        });
        // beets moves the file into the library, as `move: yes` does.
        reached.notified().await;
        std::fs::remove_file(&staged).unwrap();
        release.notify_one();
        assert_eq!(promoting.await.unwrap().unwrap().id, "t1");
        assert_eq!(lib.track_status("t1").await, "Promoted");
        assert_eq!(stub.forgets_once(1).await, vec![(profile, canonical)]);
    }

    #[tokio::test]
    async fn only_a_pending_track_is_removed() {
        let lib = Library::new().await;
        let staged = staged_file(&lib);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let (queue, stub) = stub_queue();
        for status in ["Promoting", "Promoted", "Removing", "Removed"] {
            sqlx::query("UPDATE discovery_tracks SET status = ? WHERE id = 't1'")
                .bind(status)
                .execute(&lib.db)
                .await
                .unwrap();
            assert_eq!(
                remove_in(&lib.db, &queue, &lib.alice(), "t1")
                    .await
                    .unwrap_err(),
                NotDone::Unclaimed(track_not_pending()),
                "{status}"
            );
            assert_eq!(lib.track_status("t1").await, status);
            assert!(staged.exists());
            assert_ne!(lib.history_outcome("t1").await, "removed");
        }
        assert!(stub.forgets().is_empty());
    }

    #[tokio::test]
    async fn only_a_track_the_sync_found_expirable_expires_and_only_past_its_lifetime() {
        let lib = Library::new().await;
        let mut staged = vec![];
        for (id, name) in [("t1", "03.flac"), ("t2", "04.flac")] {
            let path = lib.root.join("music/alice/Discovery/Explore").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"audio").unwrap();
            lib.pending_track(id, "alice-music", "Explore", &path, None)
                .await;
            staged.push(path);
        }
        let folder = Folder::get_by_id_in(&lib.db, "alice-music")
            .await
            .unwrap()
            .unwrap();
        // t2 is one the sync found rated to keep, or could not tell.
        let expirable = HashSet::from(["t1".to_string()]);
        let (queue, stub) = stub_queue();
        let profile = std::fs::canonicalize(staged[0].parent().unwrap()).unwrap();
        let canonical = std::fs::canonicalize(&staged[0]).unwrap();
        let lifetimes = HashMap::new();
        assert_eq!(
            expire_in(&lib.db, &queue, &folder, &lifetimes, Utc::now(), &expirable).await,
            Ok(0)
        );
        assert!(staged[0].exists());
        let later = Utc::now() + Duration::days(8);
        assert_eq!(
            expire_in(&lib.db, &queue, &folder, &lifetimes, later, &expirable).await,
            Ok(1)
        );
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "expired".into())
        );
        assert!(!staged[0].exists());
        assert_eq!(lib.track_status("t2").await, "Pending");
        assert!(staged[1].exists());
        assert_eq!(stub.forgets_once(1).await, vec![(profile, canonical)]);
    }

    #[tokio::test]
    async fn a_promote_whose_file_is_gone_leaves_the_track_pending() {
        let lib = Library::new().await;
        lib.pending_track(
            "t1",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/Discovery/Explore/gone.flac"),
            None,
        )
        .await;
        let effects: Arc<dyn DiscoveryEffects> = Arc::new(TestDiscovery::default());
        assert_eq!(
            promote(
                &lib.db,
                &effects,
                &stub_queue().0,
                &lib.alice(),
                "t1",
                Replace::Guarded(Origin::Ui)
            )
            .await
            .unwrap_err(),
            NotDone::Claimed(promote_file_gone())
        );
        assert_eq!(lib.track_status("t1").await, "Pending");
    }
}
