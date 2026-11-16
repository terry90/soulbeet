//! Ratings and stars of the caller's own songs and albums, each checked to exist for the
//! caller first, and the rating sync: what it does with each rated song, and its report.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::{DeletionReason, DiscoveryTrack, SyncResult};
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::{NavidromeClient, StarTarget};
use soulbeet::NavidromeError;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::auth::{UserCtx, UserId};
use crate::core::delete_guard::staging::{remove_staged_in, StagedOutcome};
use crate::core::delete_guard::{paths, song_not_found};
use crate::core::delete_guard::{ForgetVia, Guard, NotRemoved, Origin, Verdict};
use crate::core::discovery::{promote, track_not_pending, DiscoveryEffects, Replace};
use crate::core::folders::folder_not_found;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::library::{album_not_found, navidrome_failure, navidrome_for};
use crate::core::{database_failure, logged};
use crate::import_queue::{ImportQueue, IMPORT_QUEUE};
use crate::models::deletion_block::{DeletionBlockRow, KeptTrack};
use crate::models::deletion_review::DeletionReviewRow;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::folder::Folder;
use crate::models::user_settings::UserSettings;
use crate::server_fns::discovery::LiveDiscovery;

/// A song or album, by its Navidrome id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    Song(String),
    Album(String),
}

impl Target {
    pub(crate) fn id(&self) -> &str {
        match self {
            Target::Song(id) | Target::Album(id) => id,
        }
    }

    /// `id` is unknown, or in a library this account cannot see.
    fn not_found(&self) -> Failure {
        match self {
            Target::Song(_) => song_not_found(),
            Target::Album(id) => album_not_found(id),
        }
    }
}

/// What the next rating sync does with a song at its new rating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SyncEffect {
    PromotesDiscoveryTrack,
    RemovesDiscoveryTrack,
    DeletesFile,
}

impl SyncEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SyncEffect::PromotesDiscoveryTrack => "promotes_discovery_track",
            SyncEffect::RemovesDiscoveryTrack => "removes_discovery_track",
            SyncEffect::DeletesFile => "deletes_file",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            SyncEffect::PromotesDiscoveryTrack => {
                "This is a pending discovery track: the next rating sync moves it into your library"
            }
            SyncEffect::RemovesDiscoveryTrack => {
                "This is a pending discovery track: the next rating sync deletes it"
            }
            SyncEffect::DeletesFile => {
                "Auto-delete is on: the next rating sync deletes this song's file unless the delete checks keep it"
            }
        }
    }
}

/// A rated or starred song or album as Navidrome listed it for the caller before the change.
/// `title`, `artist` and `album` are its tags, Navidrome's text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Annotated {
    pub target: Target,
    pub title: String,
    pub artist: Option<String>,
    /// The song's album; None for an album.
    pub album: Option<String>,
    pub previous_rating: Option<u8>,
    pub previously_starred: bool,
    /// What the next rating sync does with a rated song; None for an album and a star.
    pub effect: Option<SyncEffect>,
}

/// Why a rating or star was not made.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NotSet {
    /// Refused or failed before the write request was sent: nothing changed.
    Refused(Failure),
    /// The write request was sent and failed, so the change may have been made.
    WriteFailed(Failure),
}

impl NotSet {
    pub(crate) fn failure(&self) -> &Failure {
        match self {
            NotSet::Refused(f) | NotSet::WriteFailed(f) => f,
        }
    }

    pub(crate) fn after_write(&self) -> bool {
        matches!(self, NotSet::WriteFailed(_))
    }
}

/// A 1-star song rating from a connected app while the user's auto-delete is on: the next rating
/// sync would delete the file, also of a pending discovery track that is promoted first.
fn one_star_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "A 1-star rating from a connected app would have Soulful delete this song's file at the next rating sync, because auto-delete is on. Rate it 1 star yourself in Navidrome, or turn auto-delete off in Soulful.",
    )
    .with_user_action(UserAction::new(
        UserActionCode::RateInNavidrome,
        "Rate the song 1 star yourself in Navidrome",
        "Navidrome",
    ))
}

/// Sets `user`'s rating of `target` to `rating` (0 clears it) for a connected app, after
/// reading the target as Navidrome lists it for `navi`'s account (a song or album it lists for
/// no one, or in a library this account cannot see, is not found). For a song, `effect` says
/// what the next rating sync does with it; a 1-star song rating is refused while the user's
/// auto-delete is on, or when that setting cannot be read, before anything is written.
pub(crate) async fn rate(
    db: &SqlitePool,
    navi: &NavidromeClient,
    user: &UserId,
    target: &Target,
    rating: u8,
) -> Result<Annotated, NotSet> {
    let (mut annotated, song) = read(navi, target).await.map_err(NotSet::Refused)?;
    if let Some(song) = song {
        annotated.effect = sync_effect(db, user, song, rating)
            .await
            .map_err(NotSet::Refused)?;
    }
    navi.set_rating(target.id(), rating)
        .await
        .map_err(|e| NotSet::WriteFailed(write_failure(target, &e)))?;
    Ok(annotated)
}

/// Stars or unstars `target` after reading it as `rate` does; `effect` is None.
pub(crate) async fn star(
    navi: &NavidromeClient,
    target: &Target,
    starred: bool,
) -> Result<Annotated, NotSet> {
    let (annotated, _) = read(navi, target).await.map_err(NotSet::Refused)?;
    let star_target = match target {
        Target::Song(id) => StarTarget::Song(id),
        Target::Album(id) => StarTarget::Album(id),
    };
    navi.set_star(star_target, starred)
        .await
        .map_err(|e| NotSet::WriteFailed(write_failure(target, &e)))?;
    Ok(annotated)
}

/// `target` as Navidrome lists it for `navi`'s account, and the song when it is one.
async fn read(
    navi: &NavidromeClient,
    target: &Target,
) -> Result<(Annotated, Option<SubsonicSong>), Failure> {
    let failed = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    match target {
        Target::Song(id) => {
            let song = (navi.get_song(id).await.map_err(failed)?).ok_or_else(song_not_found)?;
            let annotated = Annotated {
                target: target.clone(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                album: song.album.clone(),
                previous_rating: rated(song.user_rating),
                previously_starred: song.starred.is_some(),
                effect: None,
            };
            Ok((annotated, Some(song)))
        }
        Target::Album(id) => {
            let album =
                (navi.get_album(id).await.map_err(failed)?).ok_or_else(|| album_not_found(id))?;
            let annotated = Annotated {
                target: target.clone(),
                title: album.name.or(album.title).unwrap_or_default(),
                artist: album.artist,
                album: None,
                previous_rating: rated(album.user_rating),
                previously_starred: album.starred.is_some(),
                effect: None,
            };
            Ok((annotated, None))
        }
    }
}

/// A rating as Navidrome sends it, None when unrated: getAlbum sends 0 then.
fn rated(rating: Option<u8>) -> Option<u8> {
    rating.filter(|r| *r > 0)
}

/// What the next rating sync does with `song` rated `rating`, as the sync decides it: the
/// pending discovery track of the user's discovery folder it is promoted or removed, else
/// nothing. A 1-star rating while auto-delete is on is refused.
async fn sync_effect(
    db: &SqlitePool,
    user: &UserId,
    song: SubsonicSong,
    rating: u8,
) -> Result<Option<SyncEffect>, Failure> {
    let unreadable = |e: String| database_failure(Stage::Input, e);
    let settings = UserSettings::get_in(db, &user.to_string())
        .await
        .map_err(unreadable)?;
    if rating == 1 && settings.auto_delete_enabled {
        return Err(one_star_refused());
    }
    let pending = match &settings.discovery_folder_id {
        Some(folder) => DiscoveryTrackRow::get_pending_by_folder_in(db, folder)
            .await
            .map_err(unreadable)?,
        None => vec![],
    };
    // The song as the sync will list it: Navidrome lists a cleared rating as none.
    let rated = SubsonicSong {
        user_rating: (rating > 0).then_some(rating),
        ..song
    };
    Ok(
        match pending_action(&rated, &pending, settings.discovery_promote_threshold) {
            Some(PendingAction::Promote(_)) => Some(SyncEffect::PromotesDiscoveryTrack),
            Some(PendingAction::Remove(_)) => Some(SyncEffect::RemovesDiscoveryTrack),
            None => None,
        },
    )
}

/// A failed write of `target`: Navidrome's "data not found" (Subsonic 70) is the target gone.
fn write_failure(target: &Target, e: &NavidromeError) -> Failure {
    if e.is_not_found() {
        target.not_found()
    } else {
        navidrome_failure(Stage::LibraryCheck, e)
    }
}

/// The pending discovery track `song` is: the track's song id when it has one, else the file
/// name of the song's Subsonic path, ignoring ASCII case.
pub(crate) fn pending_track_for<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
) -> Option<&'a DiscoveryTrack> {
    let file_name = |path: &str| Path::new(path).file_name().map(|f| f.to_ascii_lowercase());
    pending.iter().find(|track| match &track.song_id {
        Some(song_id) => *song_id == song.id,
        None => song
            .path
            .as_deref()
            .and_then(file_name)
            .is_some_and(|name| Some(name) == file_name(&track.path)),
    })
}

/// What the rating sync does with a pending discovery track the user rated.
pub(crate) enum PendingAction<'a> {
    Promote(&'a DiscoveryTrack),
    Remove(&'a DiscoveryTrack),
}

/// Promote the pending track of `song` at `promote_threshold` or above, else remove it at
/// rating 1.
pub(crate) fn pending_action<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
    promote_threshold: u8,
) -> Option<PendingAction<'a>> {
    let rating = song.user_rating?;
    let track = pending_track_for(song, pending)?;
    if rating >= promote_threshold {
        Some(PendingAction::Promote(track))
    } else if rating == 1 {
        Some(PendingAction::Remove(track))
    } else {
        None
    }
}

/// What the rating sync did, or tried to do, with a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    /// Deleted the library file of a 1-star song (auto-delete).
    Delete,
    /// Moved a pending discovery track into the library.
    Promote,
    /// Deleted a pending discovery track's staged file.
    RemoveFromDiscovery,
}

/// A track the rating sync acted on: its song as search3 listed it, the pending discovery track
/// when it is one, and the failure that stopped the action, None when it was done.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SyncedTrack {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub discovery_track_id: Option<String>,
    pub action: SyncAction,
    pub failure: Option<Failure>,
}

impl SyncedTrack {
    fn of(song: &SubsonicSong, action: SyncAction, track: Option<&DiscoveryTrack>) -> Self {
        SyncedTrack {
            song_id: song.id.clone(),
            title: song.title.clone(),
            artist: song.artist.clone().unwrap_or_default(),
            discovery_track_id: track.map(|t| t.id.clone()),
            action,
            failure: None,
        }
    }

    fn failed(self, failure: Option<Failure>) -> Self {
        SyncedTrack { failure, ..self }
    }
}

/// Whether this sync may run auto-delete (D15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoDeletePermit {
    Allowed,
    NotForThisApp,
}

/// What auto-delete did in a sync: off in the settings, on, or on but not allowed for the app
/// that asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoDeleteRan {
    Off,
    On,
    NotAllowed,
}

/// One rating sync: the songs it read, whether auto-delete ran, and every track it acted on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SyncReport {
    pub scanned: u32,
    pub auto_delete: AutoDeleteRan,
    pub tracks: Vec<SyncedTrack>,
    /// The entries of auto_delete_songs that carry a failure (D12), as replace_in stored them.
    pub kept: u32,
}

impl SyncReport {
    /// The counts the web UI and the automation log show: the actions done, and the kept tracks.
    pub(crate) fn counts(&self) -> SyncResult {
        let done = |action: SyncAction| {
            (self.tracks.iter())
                .filter(|t| t.action == action && t.failure.is_none())
                .count() as u32
        };
        SyncResult {
            deleted_tracks: done(SyncAction::Delete),
            promoted_tracks: done(SyncAction::Promote),
            removed_tracks: done(SyncAction::RemoveFromDiscovery),
            total_songs_scanned: self.scanned,
            kept_tracks: self.kept,
        }
    }
}

/// One rating sync per user at a time (D11).
#[derive(Default)]
pub(crate) struct SyncRuns(Mutex<HashSet<String>>);

/// A user's running sync; dropping it lets the next one start.
pub(crate) struct SyncClaim<'a> {
    runs: &'a SyncRuns,
    user: String,
}

impl SyncRuns {
    /// Claims `user`'s sync, None while another one runs.
    pub(crate) fn claim(&self, user: &UserId) -> Option<SyncClaim<'_>> {
        let user = user.to_string();
        let claimed = (self.0.lock().unwrap_or_else(PoisonError::into_inner)).insert(user.clone());
        // Built only once claimed: a claim dropped here would remove the running sync's user.
        claimed.then(|| SyncClaim { runs: self, user })
    }
}

impl Drop for SyncClaim<'_> {
    fn drop(&mut self) {
        let mut running = self.runs.0.lock().unwrap_or_else(PoisonError::into_inner);
        running.remove(&self.user);
    }
}

/// The server's running syncs.
pub(crate) static SYNC_RUNS: LazyLock<SyncRuns> = LazyLock::new(SyncRuns::default);

pub(crate) fn sync_running() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Input,
        RetryAdvice::SameSourceLater,
        "A rating sync is already running for this account",
    )
}

/// What a rating sync reads and acts through.
pub(crate) struct SyncDeps<'a> {
    pub db: &'a SqlitePool,
    pub navidrome: &'a dyn NavidromeOf,
    pub guard: &'a Guard<'a>,
    pub queue: &'a ImportQueue,
    pub discovery: &'a Arc<dyn DiscoveryEffects>,
    pub runs: &'a SyncRuns,
}

/// `ctx`'s rating sync, once `deps.runs` gave it the user's claim: lists the songs with the
/// caller's ratings, removes the pending discovery tracks rated 1 (remove_disliked_pending_in),
/// promotes those rated at the threshold or above, then runs auto-delete when the settings have
/// it on and `permit` allows it, storing the tracks it kept. An app that may not delete promotes
/// with `Replace::Never`, so no library copy is replaced either. Every failure comes before any
/// action; a failure of an action is its track's entry. Entries in that order.
pub(crate) async fn sync_in(
    deps: &SyncDeps<'_>,
    ctx: &UserCtx,
    permit: AutoDeletePermit,
) -> Result<SyncReport, Failure> {
    let user = ctx.id();
    let _claim = deps.runs.claim(&user).ok_or_else(sync_running)?;
    let client = navidrome_for(deps.navidrome, ctx, Stage::LibraryCheck).await?;
    let songs = (client.get_all_songs_with_ratings().await)
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;
    let unreadable = |e: String| database_failure(Stage::Input, e);
    let settings = (UserSettings::get_in(deps.db, &ctx.user_id).await).map_err(unreadable)?;
    let threshold = settings.discovery_promote_threshold;
    let pending = match &settings.discovery_folder_id {
        Some(fid) => {
            (DiscoveryTrackRow::get_pending_by_folder_in(deps.db, fid).await).map_err(unreadable)?
        }
        None => vec![],
    };

    let mut tracks = remove_disliked_pending_in(deps.db, &songs, &pending, threshold).await;
    let replace = match permit {
        AutoDeletePermit::Allowed => Replace::Guarded(Origin::Auto),
        AutoDeletePermit::NotForThisApp => Replace::Never,
    };
    for song in &songs {
        if let Some(PendingAction::Promote(track)) = pending_action(song, &pending, threshold) {
            let result = promote(deps.db, deps.discovery, &user, &track.id, replace).await;
            if let Err(e) = &result {
                warn!("Failed to promote track {}: {}", track.title, e.message);
            }
            let entry = SyncedTrack::of(song, SyncAction::Promote, Some(track));
            tracks.push(entry.failed(result.err()));
        }
    }

    let auto_delete = match (settings.auto_delete_enabled, permit) {
        (false, _) => AutoDeleteRan::Off,
        (true, AutoDeletePermit::Allowed) => AutoDeleteRan::On,
        (true, AutoDeletePermit::NotForThisApp) => AutoDeleteRan::NotAllowed,
    };
    let mut kept = 0;
    match auto_delete {
        AutoDeleteRan::Off => info!("Auto-delete is disabled for this user"),
        AutoDeleteRan::NotAllowed => info!("Auto-delete is skipped: this app may not delete"),
        AutoDeleteRan::On => {
            let run = auto_delete_songs(deps.guard, &user, &songs, &pending, deps.queue).await;
            let kept_tracks = kept_of(&run);
            if let Err(e) = DeletionBlockRow::replace_in(deps.db, &ctx.user_id, &kept_tracks).await
            {
                warn!("Could not store the tracks auto-delete kept: {e}");
            }
            kept = kept_tracks.len() as u32;
            tracks.extend(run);
        }
    }
    let report = SyncReport {
        scanned: songs.len() as u32,
        auto_delete,
        tracks,
        kept,
    };
    let c = report.counts();
    info!(
        "Ratings sync complete: {} songs scanned, {} deleted, {} promoted, {} removed, {} kept",
        c.total_songs_scanned, c.deleted_tracks, c.promoted_tracks, c.removed_tracks, c.kept_tracks
    );
    Ok(report)
}

/// The entries of an auto-delete run that carry a failure, each logged, and a count per code.
fn kept_of(run: &[SyncedTrack]) -> Vec<KeptTrack> {
    let kept: Vec<KeptTrack> = (run.iter())
        .filter_map(|t| {
            Some(KeptTrack {
                song_id: t.song_id.clone(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                failure: t.failure.clone()?,
            })
        })
        .collect();
    let mut by_code = BTreeMap::<&str, u32>::new();
    for k in &kept {
        let code = k.failure.code.as_str();
        *by_code.entry(code).or_default() += 1;
        let (artist, title, text) = (&k.artist, &k.title, logged(&k.failure));
        if k.failure.cause.is_some() {
            warn!("Auto-delete kept {artist} - {title}: {code}: {text}");
        } else {
            info!("Auto-delete kept {artist} - {title}: {code}: {text}");
        }
    }
    for (code, n) in by_code {
        info!("Auto-delete kept {n}: {code}");
    }
    kept
}

/// sync_in on the server's database, Navidrome, guard, import queue, discovery and syncs.
pub(crate) async fn sync(ctx: &UserCtx, permit: AutoDeletePermit) -> Result<SyncReport, Failure> {
    let (guard, discovery): (_, Arc<dyn DiscoveryEffects>) =
        (Guard::live(), Arc::new(LiveDiscovery));
    let deps = SyncDeps {
        db: &crate::db::DB,
        navidrome: &ServerNavidrome,
        guard: &guard,
        queue: &IMPORT_QUEUE,
        discovery: &discovery,
        runs: &SYNC_RUNS,
    };
    sync_in(&deps, ctx, permit).await
}

/// Discovery's own cleanup (D12): removes the pending track of each song of `songs` that
/// pending_action removes, through remove_staged_in with its folder, with no Navidrome call.
/// An entry for each removal, done or failed; a track no longer pending is skipped.
pub(crate) async fn remove_disliked_pending_in(
    db: &SqlitePool,
    songs: &[SubsonicSong],
    pending: &[DiscoveryTrack],
    promote_threshold: u8,
) -> Vec<SyncedTrack> {
    let mut removed = Vec::new();
    for song in songs {
        let Some(PendingAction::Remove(track)) = pending_action(song, pending, promote_threshold)
        else {
            continue;
        };
        let result = match Folder::get_by_id_in(db, &track.folder_id).await {
            Ok(Some(folder)) => remove_staged_in(db, track, &folder, StagedOutcome::Removed).await,
            Ok(None) => Err(folder_not_found()),
            Err(e) => Err(database_failure(Stage::Delete, e)),
        };
        let entry = SyncedTrack::of(song, SyncAction::RemoveFromDiscovery, Some(track));
        match result {
            Ok(()) => removed.push(entry),
            Err(f) if f == track_not_pending() => {
                info!("Skipped removing track {}: {}", track.title, f.message)
            }
            Err(f) => {
                warn!("Failed to remove track {}: {}", track.title, logged(&f));
                removed.push(entry.failed(Some(f)));
            }
        }
    }
    removed
}

/// Deletes through the guard every song of `songs` that `actor` rated 1 and pending_track_for
/// does not match in `pending`: a library file is carried out, a pending discovery track the
/// guard finds is removed as a staged track. Only a song the fresh record still shows at 1
/// star is removed; one rated otherwise since `songs` was listed is skipped, also when the
/// guard refuses it. A song whose file an earlier deletion of `actor` removed, which Navidrome
/// lists until it notices, is skipped. Every other song is an entry, with the failure that kept
/// it when one did.
pub(crate) async fn auto_delete_songs(
    guard: &Guard<'_>,
    actor: &UserId,
    songs: &[SubsonicSong],
    pending: &[DiscoveryTrack],
    queue: &ImportQueue,
) -> Vec<SyncedTrack> {
    let mut run = Vec::new();
    let deleted = match DeletionReviewRow::deleted_files_in(guard.db, &actor.to_string()).await {
        Ok(deleted) => deleted,
        Err(e) => {
            warn!("Auto-delete could not read which files it deleted before: {e}");
            vec![]
        }
    };
    for song in songs {
        if song.user_rating != Some(1) || pending_track_for(song, pending).is_some() {
            continue;
        }
        let artist = song.artist.as_deref().unwrap_or_default();
        let skipped = |why: &str| info!("Auto-delete skipped {artist} - {}: {why}", song.title);
        if deleted_before(guard, actor, &deleted, &song.id).await {
            skipped("deleted earlier, and Navidrome has not noticed yet");
            continue;
        }
        let delete = SyncedTrack::of(song, SyncAction::Delete, None);
        let entry = match guard.check_song(actor, &song.id).await {
            Ok(Verdict::Library(checked)) if checked.song.rating == 1 => {
                let forget = ForgetVia::Queue(queue.clone());
                let reason = DeletionReason::Disliked;
                // Its only warning, an unrecorded deletion, is logged where it happens.
                let done = guard
                    .carry_out(actor, checked, reason, Origin::Auto, forget)
                    .await;
                Some(delete.failed(done.err().map(NotRemoved::into_failure)))
            }
            Ok(Verdict::Library(_)) => {
                skipped("no longer rated 1");
                None
            }
            Ok(Verdict::Discovery { track, rating: 1 }) => {
                let remove = SyncedTrack::of(song, SyncAction::RemoveFromDiscovery, Some(&track));
                let result = match Folder::get_by_id_in(guard.db, &track.folder_id).await {
                    Ok(Some(folder)) => {
                        remove_staged_in(guard.db, &track, &folder, StagedOutcome::Removed).await
                    }
                    Ok(None) => Err(folder_not_found()),
                    Err(e) => Err(database_failure(Stage::Delete, e)),
                };
                match result {
                    Err(f) if f == track_not_pending() => {
                        skipped("no longer pending in discovery");
                        None
                    }
                    result => Some(remove.failed(result.err())),
                }
            }
            Ok(Verdict::Discovery { .. }) => {
                skipped("no longer rated 1");
                None
            }
            Err(f) => match still_rated_one(guard, actor, &song.id).await {
                true => Some(delete.failed(Some(f))),
                false => {
                    skipped("no longer rated 1");
                    None
                }
            },
        };
        run.extend(entry);
    }
    run
}

/// Whether a deletion in `deleted` (song id, canonical path) removed the file song `song_id`
/// has now: `actor`'s native record of it names that path on this machine, found as
/// local_file_in finds it (paths::names_file), and nothing is there. Navidrome keeps a song's id
/// and ratings when its scanner matches a missing file to a new one
/// (scanner/phase_2_missing_tracks.go:275-308, moveMatched), so a song whose file is now
/// elsewhere is checked as any other. A record that cannot be read is not skipped either: the
/// guard reads it again and says why it keeps the file.
async fn deleted_before(
    guard: &Guard<'_>,
    actor: &UserId,
    deleted: &[(String, String)],
    song_id: &str,
) -> bool {
    let recorded: Vec<&Path> = deleted
        .iter()
        .filter(|(id, _)| id == song_id)
        .map(|(_, path)| Path::new(path))
        .collect();
    if recorded.is_empty() {
        return false;
    }
    let Ok(Some(song)) = guard.listeners.native_song(actor, song_id).await else {
        return false;
    };
    for path in recorded {
        let named = paths::names_file(guard.db, &song, guard.music_path, path)
            .await
            .unwrap_or(false);
        if named
            && tokio::fs::symlink_metadata(path)
                .await
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return true;
        }
    }
    false
}

/// Whether `actor`'s record of `song_id` still shows a rating of 1; true when it cannot be
/// read, so the refusal stands.
async fn still_rated_one(guard: &Guard<'_>, actor: &UserId, song_id: &str) -> bool {
    match guard.listeners.native_song(actor, song_id).await {
        Ok(Some(song)) => song.rating == 1,
        Ok(None) | Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::discovery::promote_file_gone;
    use crate::core::discovery::test_support::TestDiscovery;
    use crate::core::test_ctx;
    use crate::db::test_db;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::models::user_settings::UpdateUserSettings;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        album_body, search3_body, song_body, subsonic_failed, subsonic_song, NavidromeFor,
        NavidromeStub, HOLY,
    };
    use shared::navidrome::{DiscoveryStatus, SyncResult};
    use soulbeet::navidrome::models::NativeSong;

    const WINTER: &str = "7hQ2nLr5VxZ0cYbM8wTkAf";

    async fn set(db: &SqlitePool, update: UpdateUserSettings) {
        UserSettings::upsert_in(db, "alice", update).await.unwrap();
    }

    fn auto_delete(on: bool) -> UpdateUserSettings {
        UpdateUserSettings {
            auto_delete_enabled: Some(on),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_one_star_rating_is_refused_while_auto_delete_is_on() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(4), false)],
        );
        let (alice, holy) = (lib.alice(), Target::Song(HOLY.0.into()));
        let a = rate(&lib.db, &navi.client(), &alice, &holy, 1)
            .await
            .unwrap();
        assert_eq!(
            a,
            Annotated {
                target: holy.clone(),
                title: HOLY.1.into(),
                artist: Some("Marrow Choir".into()),
                album: Some("Winter Songs".into()),
                previous_rating: Some(4),
                previously_starred: false,
                effect: None,
            }
        );
        // A pending discovery track of the discovery folder with that song id: the sync removes
        // it as discovery's own cleanup.
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some(HOLY.0))
            .await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await;
        let a = rate(&lib.db, &navi.client(), &alice, &holy, 1).await;
        assert_eq!(a.unwrap().effect, Some(SyncEffect::RemovesDiscoveryTrack));
        let sent = format!("id={}&rating=1", HOLY.0);
        assert_eq!(navi.posted("/rest/setRating"), [sent.clone(), sent.clone()]);

        // On: refused, pending track or not (it could be promoted first, then deleted).
        set(&lib.db, auto_delete(true)).await;
        assert_eq!(
            rate(&lib.db, &navi.client(), &alice, &holy, 1).await,
            Err(NotSet::Refused(one_star_refused()))
        );
        // A setting that cannot be read refuses too.
        lib.db.close().await;
        let e = rate(&lib.db, &navi.client(), &alice, &holy, 1)
            .await
            .unwrap_err();
        assert_eq!(
            (e.after_write(), e.failure().code),
            (false, FailureCode::SettingsUnreadable)
        );
        assert_eq!(navi.posted("/rest/setRating"), [sent.clone(), sent]);
    }

    #[tokio::test]
    async fn a_pending_track_rated_at_the_threshold_is_promoted() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, true)]);
        // No song id yet: the sync matches it by the file name of the song's path.
        let staged = lib
            .root
            .join("music/alice/Discovery/Balanced/01-03 - O Holy Night.flac");
        lib.pending_track("t1", "alice-music", "Balanced", &staged, None)
            .await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                auto_delete_enabled: Some(true),
                ..Default::default()
            },
        )
        .await;
        let holy = Target::Song(HOLY.0.into());
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &holy, 3)
            .await
            .unwrap();
        assert_eq!(
            (a.previous_rating, a.previously_starred, a.effect),
            (None, true, Some(SyncEffect::PromotesDiscoveryTrack))
        );
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &holy, 2)
            .await
            .unwrap();
        assert_eq!(a.effect, None);
    }

    #[tokio::test]
    async fn an_album_rated_one_star_is_written_while_auto_delete_is_on() {
        // Auto-delete reads song ratings only.
        let lib = Library::new().await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getAlbum", &[&album_body(WINTER, "Winter Songs", 0)]);
        let winter = Target::Album(WINTER.into());
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &winter, 1)
            .await
            .unwrap();
        assert_eq!(
            a,
            Annotated {
                target: winter,
                title: "Winter Songs".into(),
                artist: Some("Marrow Choir".into()),
                album: None,
                previous_rating: None,
                previously_starred: true,
                effect: None,
            }
        );
        assert_eq!(
            navi.posted("/rest/setRating"),
            [format!("id={WINTER}&rating=1")]
        );
    }

    #[tokio::test]
    async fn a_star_reports_what_was_starred_before() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(2), true)],
        );
        navi.answer("/rest/getAlbum", &[&album_body(WINTER, "Winter Songs", 5)]);
        let a = star(&navi.client(), &Target::Song(HOLY.0.into()), true)
            .await
            .unwrap();
        assert_eq!(
            (a.previous_rating, a.previously_starred, a.effect),
            (Some(2), true, None)
        );
        let a = star(&navi.client(), &Target::Album(WINTER.into()), false)
            .await
            .unwrap();
        assert_eq!(
            (a.title.as_str(), a.previous_rating),
            ("Winter Songs", Some(5))
        );
        assert_eq!(navi.posted("/rest/star"), [format!("id={}", HOLY.0)]);
        assert_eq!(navi.posted("/rest/unstar"), [format!("albumId={WINTER}")]);
    }

    #[tokio::test]
    async fn an_unknown_song_or_album_is_never_rated_or_starred() {
        let navi = NavidromeStub::start();
        // server/subsonic/browsing.go:251-253 and :194-197 (0.64.2); also what a song in a library
        // this account cannot see answers.
        navi.answer("/rest/getSong", &[&subsonic_failed(70, "Song not found")]);
        navi.answer("/rest/getAlbum", &[&subsonic_failed(70, "Album not found")]);
        let db = test_db::migrated().await;
        let alice = UserId::new("alice");
        assert_eq!(
            rate(&db, &navi.client(), &alice, &Target::Song("gone".into()), 5).await,
            Err(NotSet::Refused(song_not_found()))
        );
        assert_eq!(
            star(&navi.client(), &Target::Album("gone".into()), true).await,
            Err(NotSet::Refused(album_not_found("gone")))
        );
        assert!(!navi
            .asked()
            .iter()
            .any(|p| p == "/rest/setRating" || p == "/rest/star"));
    }

    #[tokio::test]
    async fn a_song_that_goes_before_the_rating_is_not_found() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, false)]);
        // server/subsonic/media_annotation.go:42-46 (model.GetEntityByID), api.go:304-305
        navi.answer("/rest/setRating", &[&subsonic_failed(70, "data not found")]);
        let db = test_db::migrated().await;
        let holy = Target::Song(HOLY.0.into());
        assert_eq!(
            rate(&db, &navi.client(), &UserId::new("alice"), &holy, 2).await,
            Err(NotSet::WriteFailed(song_not_found()))
        );
    }

    #[test]
    fn a_track_with_a_song_id_matches_only_that_song() {
        let track = DiscoveryTrack {
            id: "t1".into(),
            song_id: Some("s5".into()),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            path: "/music/alice/Discovery/Explore/03.flac".into(),
            folder_id: "alice-music".into(),
            profile: "Explore".into(),
            rating: None,
            status: DiscoveryStatus::Pending,
            created_at: "2026-11-15 20:00:00".into(),
        };
        let same_name = SubsonicSong {
            path: Some("Marrow Choir/Explore/03.flac".into()),
            ..subsonic_song("s6", "O Holy Night", Some(1))
        };
        assert!(pending_track_for(&same_name, std::slice::from_ref(&track)).is_none());
        assert!(pending_track_for(
            &subsonic_song("s5", "O Holy Night", Some(1)),
            std::slice::from_ref(&track)
        )
        .is_some());
    }

    /// Each entry's song, action and failure code.
    fn acts(run: &[SyncedTrack]) -> Vec<(&str, SyncAction, Option<FailureCode>)> {
        (run.iter())
            .map(|t| {
                (
                    t.song_id.as_str(),
                    t.action,
                    t.failure.as_ref().map(|f| f.code),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn one_star_songs_are_deleted_through_the_guard_and_the_kept_ones_say_why() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.rates("bob", "s2", 4);
        let songs = vec![
            subsonic_song("s1", "O Holy Night", Some(1)),
            subsonic_song("s2", "Silent Night", Some(1)),
            subsonic_song("s3", "Away in a Manger", Some(5)),
        ];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue()).await;
        assert_eq!(
            acts(&run),
            vec![
                ("s1", SyncAction::Delete, None),
                (
                    "s2",
                    SyncAction::Delete,
                    Some(FailureCode::DeleteBlockedByOtherUser)
                )
            ]
        );
        assert!(
            !lib.root.join("music/alice/A/x.flac").exists()
                && lib.root.join("music/alice/A/y.flac").exists()
        );
        set(&lib.db, auto_delete(true)).await;
        DeletionBlockRow::replace_in(&lib.db, "alice", &kept_of(&run))
            .await
            .unwrap();
        let listed = DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap();
        assert_eq!(
            (listed[0].title.as_str(), listed[0].message.as_str()),
            (
                "Silent Night",
                "Another listener rated or starred this track"
            )
        );
        DeletionBlockRow::replace_in(&lib.db, "alice", &[])
            .await
            .unwrap();
        assert!(DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_song_no_longer_rated_one_star_is_neither_deleted_nor_kept() {
        // search3 listed s1 at 1 star; alice has rated it 4 since.
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        let songs = [subsonic_song("s1", "O Holy Night", Some(1))];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue()).await;
        assert_eq!(run, vec![]);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_song_rated_again_since_the_listing_is_not_kept_when_the_guard_refuses_it() {
        let lib = Library::new().await;
        lib.fake.stars("bob", "s1");
        lib.fake.rates("alice", "s1", 4);
        let (songs, queue) = ([subsonic_song("s1", "O Holy Night", Some(1))], queue());
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(run, vec![]);
        // Still at 1 star, the same refusal keeps it.
        lib.fake.rates("alice", "s1", 1);
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(
            acts(&run),
            vec![(
                "s1",
                SyncAction::Delete,
                Some(FailureCode::DeleteBlockedByOtherUser)
            )]
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_song_deleted_earlier_that_navidrome_still_lists_is_neither_checked_nor_kept() {
        let lib = Library::new().await;
        let (songs, queue) = ([subsonic_song("s1", "O Holy Night", Some(1))], queue());
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
        // The next sync lists it again: Navidrome has not scanned since. Only its record is
        // read: the guard never checks it, so no rating is sent.
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(run, vec![]);
        assert_eq!(lib.fake.ratings_set().len(), 1);
        // A file at that path again (re-imported) is checked as usual.
        lib.write("alice/A/x.flac", b"audio");
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
    }

    #[tokio::test]
    async fn a_song_navidrome_moved_to_another_file_since_its_deletion_is_checked_again() {
        let lib = Library::new().await;
        let (songs, queue) = ([subsonic_song("s1", "O Holy Night", Some(1))], queue());
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
        // Re-imported at another path: Navidrome's scanner matched the missing file to the new
        // one and gave it the song's id and alice's rating of 1 (moveMatched).
        lib.write("alice/B/x.flac", b"audio");
        lib.fake.file(
            NativeSong {
                path: "alice/B/x.flac".into(),
                ..lib.record("s1")
            },
            "al1",
        );
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
        assert!(!lib.root.join("music/alice/B/x.flac").exists());
    }

    #[tokio::test]
    async fn a_pending_track_the_guard_finds_on_its_own_is_removed_as_a_staged_track() {
        // Not among the discovery folder's tracks the sync loaded (`pending` is empty), as for a
        // track left in an earlier discovery folder of the user.
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.song("s5", "alice/Discovery/Explore/03.flac", b"audio");
        lib.fake.rates("alice", "s5", 1);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let songs = [subsonic_song("s5", "03", Some(1))];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue()).await;
        assert_eq!(
            (acts(&run), run[0].discovery_track_id.as_deref()),
            (
                vec![("s5", SyncAction::RemoveFromDiscovery, None)],
                Some("t1")
            )
        );
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "removed".into())
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_one_star_discovery_track_is_removed_without_navidrome_placing_its_file() {
        // Navidrome has no record of s9 for anyone, and the track has no song id yet: today's file
        // name match on the Subsonic path still finds it, and the guard is never asked.
        let lib = Library::new().await;
        let staged = lib
            .root
            .join("music/alice/Discovery/Explore/Marrow Choir/03 - O Holy Night.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let song = SubsonicSong {
            path: Some("Marrow Choir/Explore/03 - O HOLY NIGHT.flac".into()),
            ..subsonic_song("s9", "O Holy Night", Some(1))
        };
        let pending = vec![track];
        assert!(
            matches!(pending_action(&song, &pending, 1), Some(PendingAction::Promote(t)) if t.id == "t1")
        );
        let songs = std::slice::from_ref(&song);
        assert_eq!(
            remove_disliked_pending_in(&lib.db, songs, &pending, 1).await,
            vec![]
        );
        assert!(staged.exists());
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), songs, &pending, &queue()).await;
        assert_eq!(run, vec![]);
        let removed = remove_disliked_pending_in(&lib.db, songs, &pending, 3).await;
        assert_eq!(
            (acts(&removed), removed[0].discovery_track_id.as_deref()),
            (
                vec![("s9", SyncAction::RemoveFromDiscovery, None)],
                Some("t1")
            )
        );
        assert!(!staged.exists());
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "removed".into())
        );
        assert_eq!(
            (lib.fake.reads_by("alice"), lib.fake.reads_by("bob")),
            (0, 0)
        );
    }

    fn deps<'a>(
        lib: &'a Library,
        navi: &'a dyn NavidromeOf,
        guard: &'a Guard<'a>,
        queue: &'a ImportQueue,
        discovery: &'a Arc<dyn DiscoveryEffects>,
        runs: &'a SyncRuns,
    ) -> SyncDeps<'a> {
        SyncDeps {
            db: &lib.db,
            navidrome: navi,
            guard,
            queue,
            discovery,
            runs,
        }
    }

    fn effects() -> Arc<dyn DiscoveryEffects> {
        Arc::new(TestDiscovery::default())
    }

    fn queue() -> ImportQueue {
        ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())))
    }

    #[tokio::test]
    async fn the_report_names_each_track_and_the_failure_the_run_gave_it() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.rates("bob", "s2", 4);
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[
                ("s1", "O Holy Night", 1),
                ("s2", "Silent Night", 1),
                ("s3", "Away in a Manger", 5),
            ])],
        );
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap();
        assert_eq!((report.scanned, report.auto_delete), (3, AutoDeleteRan::On));
        assert_eq!(
            report
                .tracks
                .iter()
                .map(|t| (
                    t.song_id.as_str(),
                    t.action,
                    t.failure.as_ref().map(|f| f.code)
                ))
                .collect::<Vec<_>>(),
            vec![
                ("s1", SyncAction::Delete, None),
                (
                    "s2",
                    SyncAction::Delete,
                    Some(FailureCode::DeleteBlockedByOtherUser)
                )
            ]
        );
        assert_eq!(
            report.counts(),
            SyncResult {
                deleted_tracks: 1,
                promoted_tracks: 0,
                removed_tracks: 0,
                total_songs_scanned: 3,
                kept_tracks: 1
            }
        );
        let listed = DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap();
        assert_eq!(
            (listed[0].title.as_str(), listed[0].message.as_str()),
            (
                "Silent Night",
                "Another listener rated or starred this track"
            )
        );
    }

    #[tokio::test]
    async fn a_discovery_removal_auto_delete_could_not_make_is_kept_too() {
        // The guard finds a pending track of alice by its song id whose stored path is outside
        // its profile folder, so the staging remover refuses it.
        let lib = Library::new().await;
        lib.song("s7", "alice/A/q.flac", b"audio");
        lib.fake.rates("alice", "s7", 1);
        let stored = lib.root.join("music/alice/A/q.flac");
        lib.pending_track("t7", "alice-music", "Explore", &stored, Some("s7"))
            .await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[("s7", "Silent Night", 1)])],
        );
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap();
        let t = &report.tracks[0];
        assert_eq!(
            (
                t.action,
                t.discovery_track_id.as_deref(),
                t.failure.as_ref().map(|f| f.code)
            ),
            (
                SyncAction::RemoveFromDiscovery,
                Some("t7"),
                Some(FailureCode::DeleteOutsideLibrary)
            )
        );
        assert_eq!(
            (
                report.kept,
                DeletionBlockRow::for_user_in(&lib.db, "alice")
                    .await
                    .unwrap()
                    .len()
            ),
            (1, 1)
        );
        assert!(stored.exists());
    }

    #[tokio::test]
    async fn an_app_that_may_not_delete_syncs_without_auto_delete() {
        let lib = Library::new().await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[("s1", "O Holy Night", 1)])],
        );
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::NotForThisApp)
            .await
            .unwrap();
        assert_eq!(
            (report.auto_delete, report.tracks.len()),
            (AutoDeleteRan::NotAllowed, 0)
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![], "the guard never ran");
    }

    #[tokio::test]
    async fn a_promote_for_an_app_that_may_not_delete_never_replaces_a_library_copy() {
        let lib = Library::new().await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await;
        let staged = |name: &str| {
            let staged = lib.root.join("music/alice/Discovery/Explore").join(name);
            std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
            std::fs::write(&staged, b"audio").unwrap();
            staged
        };
        let (first, second) = (staged("03.flac"), staged("04.flac"));
        lib.pending_track("t1", "alice-music", "Explore", &first, Some("s5"))
            .await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[
                ("s5", "O Holy Night", 5),
                ("s6", "Silent Night", 5),
            ])],
        );
        let test = Arc::new(TestDiscovery::default());
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            test.clone() as Arc<dyn DiscoveryEffects>,
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::NotForThisApp)
            .await
            .unwrap();
        assert_eq!(
            (report.tracks[0].action, report.tracks[0].failure.as_ref()),
            (SyncAction::Promote, None)
        );
        lib.pending_track("t2", "alice-music", "Explore", &second, Some("s6"))
            .await;
        sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap();
        let music = lib.root.join("music/alice");
        assert_eq!(
            test.imports(),
            vec![
                (first, music.clone(), Replace::Never),
                (second, music, Replace::Guarded(Origin::Auto))
            ]
        );
    }

    #[tokio::test]
    async fn a_promote_that_fails_is_an_entry_with_its_failure() {
        let lib = Library::new().await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await;
        // The staged file was never written, so the promote finds it gone.
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some("s5"))
            .await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[("s5", "O Holy Night", 5)])],
        );
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap();
        assert_eq!(
            (
                report.tracks[0].action,
                report.tracks[0].discovery_track_id.as_deref()
            ),
            (SyncAction::Promote, Some("t1"))
        );
        assert_eq!(report.tracks[0].failure, Some(promote_file_gone()));
        assert_eq!(lib.track_status("t1").await, "Pending");
    }

    #[tokio::test]
    async fn a_second_sync_for_the_same_user_is_refused_while_one_runs() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/search3", &[&search3_body(&[])]);
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let held = runs.claim(&lib.alice()).unwrap();
        assert_eq!(
            sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
                .await
                .unwrap_err(),
            sync_running()
        );
        assert!(
            navi.asked().is_empty(),
            "nothing is read while another sync runs"
        );
        drop(held);
        assert!(
            sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
                .await
                .is_ok()
        );
        let f = sync_running();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::AlreadyRunning,
                Stage::Input,
                RetryAdvice::SameSourceLater,
                "A rating sync is already running for this account"
            )
        );
    }

    #[tokio::test]
    async fn a_user_without_navidrome_cannot_sync() {
        let lib = Library::new().await;
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Err(|| NavidromeLookupError::NotLinked)),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let f = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::NavidromeNotLinked,
                Stage::LibraryCheck,
                "Navidrome not configured for user"
            )
        );
    }
}
