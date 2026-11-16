//! Ratings and stars of the caller's own songs and albums, each checked to exist for the
//! caller first, and the rating sync: what it does with each rated song, and its report.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use crate::core::delete_guard::{paths, pending_track_of, song_not_found};
use crate::core::delete_guard::{ForgetVia, Guard, NotRemoved, Origin, Verdict};
use crate::core::discovery::{is_not_pending, promote, DiscoveryEffects, NotDone, Replace};
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
}

/// Why a rating or star was not made.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NotSet {
    /// Refused or failed before the write request was sent: nothing changed.
    Refused(Failure),
    /// Refused before the write request was sent for what a rating sync could do with the song at
    /// that rating (rating_refusal); `discovery_track_id` is the pending discovery track the song
    /// is. Nothing changed.
    WouldDelete {
        discovery_track_id: Option<String>,
        failure: Failure,
    },
    /// The write request was sent and failed, so the change may have been made.
    WriteFailed(Failure),
}

impl NotSet {
    pub(crate) fn failure(&self) -> &Failure {
        match self {
            NotSet::Refused(f) | NotSet::WriteFailed(f) => f,
            NotSet::WouldDelete { failure, .. } => failure,
        }
    }

    pub(crate) fn after_write(&self) -> bool {
        matches!(self, NotSet::WriteFailed(_))
    }
}

/// A 1-star song rating from a connected app: once auto-delete is on, whenever the user turns it
/// on, a rating sync deletes the file of each song rated 1 star as Soulful's own deletion, past the
/// app's switch, its daily room and its name in the history, and without a preview.
fn one_star_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "A connected app may not rate a song 1 star: while auto-delete is on, now or once it is turned on, the rating sync deletes the files of songs rated 1 star without a preview. Rate it 1 star yourself in Navidrome, or delete it with preview_delete_tracks and delete_tracks.",
    )
    .with_user_action(UserAction::new(
        UserActionCode::RateInNavidrome,
        "Rate the song 1 star yourself in Navidrome",
        "Navidrome",
    ))
}

/// A rating from a connected app of a pending discovery track: a rating sync promotes it at the
/// promote threshold, through the guard, which can replace an earlier library copy of the song, or
/// removes it at 1 star, whatever threshold the user sets before the sync runs. Either is Soulful's
/// own deletion, past the app's switch, its daily room and its name in the history.
/// promote_discovery_track replaces as the app.
fn pending_rating_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "A connected app may not rate a pending discovery track: the rating sync moves it into your library at the promote threshold, replacing and deleting any earlier copy of the song there, or deletes it at 1 star. Promote it with promote_discovery_track, which replaces an earlier copy only while this app may delete music, within its daily limit, remove it with remove_discovery_track, or rate the song yourself in Navidrome.",
    )
    .with_user_action(UserAction::new(
        UserActionCode::RateInNavidrome,
        "Rate the song yourself in Navidrome",
        "Navidrome",
    ))
}

/// Sets `user`'s rating of `target` to `rating` (0 clears it) for a connected app, after
/// reading the target as Navidrome lists it for `navi`'s account (a song or album it lists for
/// no one, or in a library this account cannot see, is not found). A song rating rating_refusal
/// refuses is WouldDelete or Refused, before anything is written.
pub(crate) async fn rate(
    guard: &Guard<'_>,
    navi: &NavidromeClient,
    user: &UserId,
    target: &Target,
    rating: u8,
) -> Result<Annotated, NotSet> {
    let (annotated, song) = read(navi, target).await.map_err(NotSet::Refused)?;
    if let Some(song) = song.filter(|_| rating > 0) {
        rating_refusal(guard, user, &song, rating).await?;
    }
    navi.set_rating(target.id(), rating)
        .await
        .map_err(|e| NotSet::WriteFailed(write_failure(target, &e)))?;
    Ok(annotated)
}

/// Stars or unstars `target` after reading it as `rate` does.
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
            };
            Ok((annotated, None))
        }
    }
}

/// A rating as Navidrome sends it, None when unrated: getAlbum sends 0 then.
fn rated(rating: Option<u8>) -> Option<u8> {
    rating.filter(|r| *r > 0)
}

/// Why a connected app may not rate `song` `rating` (1 to 5), as WouldDelete: what a rating sync
/// could do with it later, since the settings can change before the sync runs. A pending discovery
/// track of any folder of the user is promoted at the promote threshold or removed at 1 star (the
/// sync reads only the discovery folder's tracks, but the folder can change), and a library song
/// rated 1 star is deleted while auto-delete is on. The track is told as the guard tells it
/// (pending_track_of), the discovery folder's tracks first so the one named is the one the next
/// sync acts on. Refused too, as Refused: when the settings or tracks cannot be read, and while
/// the user has pending tracks, when the song's file cannot be told apart from theirs.
async fn rating_refusal(
    guard: &Guard<'_>,
    user: &UserId,
    song: &SubsonicSong,
    rating: u8,
) -> Result<(), NotSet> {
    let unreadable = |e: String| NotSet::Refused(database_failure(Stage::Input, e));
    let settings = UserSettings::get_in(guard.db, &user.to_string())
        .await
        .map_err(unreadable)?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(guard.db, &user.to_string())
        .await
        .map_err(unreadable)?;
    pending.sort_by_key(|t| Some(&t.folder_id) != settings.discovery_folder_id.as_ref());
    let by_id = pending
        .iter()
        .find(|t| t.song_id.as_ref() == Some(&song.id));
    let track = match by_id {
        Some(track) => Some(track),
        None if pending.is_empty() => None,
        None => {
            let native = (guard.read_song(user, &song.id).await)
                .map_err(NotSet::Refused)?
                .ok_or_else(|| NotSet::Refused(song_not_found()))?;
            pending_track_of(guard.db, guard.music_path, user, &native, &pending)
                .await
                .map_err(NotSet::Refused)?
        }
    };
    match (track, rating) {
        (Some(track), _) => Err(NotSet::WouldDelete {
            discovery_track_id: Some(track.id.clone()),
            failure: pending_rating_refused(),
        }),
        (None, 1) => Err(NotSet::WouldDelete {
            discovery_track_id: None,
            failure: one_star_refused(),
        }),
        (None, _) => Ok(()),
    }
}

/// A failed write of `target`: Navidrome's "data not found" (Subsonic 70) is the target gone.
fn write_failure(target: &Target, e: &NavidromeError) -> Failure {
    if e.is_not_found() {
        target.not_found()
    } else {
        navidrome_failure(Stage::LibraryCheck, e)
    }
}

/// The pending track of `pending` each song of `songs` is, for the songs whose rating the sync
/// acts on (at `promote_threshold` or above, or 1), keyed by song id: told as the guard tells it
/// (pending_track_of), by the track's song id, else by `actor`'s native record of the song naming
/// the track's staged file. A song whose size no staged file has cannot be one, so its record is
/// not read. A song whose record or file cannot be read is none: the sync leaves it.
pub(crate) async fn pending_songs<'p>(
    guard: &Guard<'_>,
    actor: &UserId,
    songs: &[SubsonicSong],
    pending: &'p [DiscoveryTrack],
    promote_threshold: u8,
) -> HashMap<String, &'p DiscoveryTrack> {
    let mut staged_sizes = HashSet::new();
    for track in pending {
        if let Ok(meta) = tokio::fs::metadata(&track.path).await {
            staged_sizes.insert(meta.len());
        }
    }
    let mut found = HashMap::new();
    for song in songs {
        let acted_on = song
            .user_rating
            .is_some_and(|r| r >= promote_threshold || r == 1);
        if !acted_on {
            continue;
        }
        if let Some(track) = pending
            .iter()
            .find(|t| t.song_id.as_ref() == Some(&song.id))
        {
            found.insert(song.id.clone(), track);
            continue;
        }
        if song.size.is_some_and(|size| !staged_sizes.contains(&size)) {
            continue;
        }
        let native = match guard.read_song(actor, &song.id).await {
            Ok(Some(native)) => native,
            Ok(None) => continue,
            Err(f) => {
                info!(
                    "Could not tell whether {} is a pending discovery track: {}",
                    song.title,
                    logged(&f)
                );
                continue;
            }
        };
        match pending_track_of(guard.db, guard.music_path, actor, &native, pending).await {
            Ok(Some(track)) => {
                found.insert(song.id.clone(), track);
            }
            Ok(None) => {}
            Err(f) => info!(
                "Could not tell whether {} is a pending discovery track: {}",
                song.title,
                logged(&f)
            ),
        }
    }
    found
}

/// What the rating sync does with a pending discovery track the user rated.
pub(crate) enum PendingAction<'a> {
    Promote(&'a DiscoveryTrack),
    Remove(&'a DiscoveryTrack),
}

/// Promote the pending track of `song` (pending_songs) at `promote_threshold` or above, else
/// remove it at rating 1.
pub(crate) fn pending_action<'a>(
    song: &SubsonicSong,
    pending: &HashMap<String, &'a DiscoveryTrack>,
    promote_threshold: u8,
) -> Option<PendingAction<'a>> {
    let rating = song.user_rating?;
    let track = *pending.get(&song.id)?;
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

    fn with_failure(self, failure: Option<Failure>) -> Self {
        SyncedTrack { failure, ..self }
    }

    /// Another promote or removal took the track first: the sync skips it.
    fn not_pending(&self) -> bool {
        self.failure.as_ref().is_some_and(is_not_pending)
    }
}

/// Whether this sync may run auto-delete, and as whom (D15, step 11 D16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutoDeletePermit {
    /// The web UI or the automation: Soulful deletes on its own (Origin::Auto).
    Allowed,
    /// A connected app whose switch is on: its deletions are Origin::Mcp { grant_id }, each
    /// checked against the switch and the day's room, and named in the audit.
    ForApp {
        grant_id: String,
    },
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
    /// Pending discovery tracks skipped because another promote or removal took them first.
    pub not_pending: u32,
    /// The entries of auto_delete_songs that carry a failure (D12).
    pub kept: u32,
    /// Why Settings does not list the kept tracks: replace_in failed, so it still lists the
    /// previous sync's. None when they were stored, or auto-delete did not run.
    pub kept_not_stored: Option<Failure>,
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
/// caller's ratings, tells which of them are pending tracks of the discovery folder
/// (pending_songs), removes those rated 1 (remove_disliked_pending_in), promotes those rated at
/// the threshold or above, then runs auto-delete on the other songs rated 1 when the settings have
/// it on and `permit` allows it, storing the tracks it kept. Promotes replace and auto-delete
/// deletes as `permit` says: Origin::Auto for Allowed, the app's Origin::Mcp for ForApp (each
/// file checked against its switch and the day's room); an app that may not delete promotes with
/// `Replace::Never`, so no library copy is replaced either. Every failure comes before any
/// action; a failure of an action is its track's entry, and a failure to store the kept tracks
/// is `kept_not_stored`. Entries in that order. A pending track another promote or removal took
/// first is no entry: it is counted in `not_pending`.
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
    let tracks_of_folder = match &settings.discovery_folder_id {
        Some(fid) => {
            (DiscoveryTrackRow::get_pending_by_folder_in(deps.db, fid).await).map_err(unreadable)?
        }
        None => vec![],
    };
    let pending = pending_songs(deps.guard, &user, &songs, &tracks_of_folder, threshold).await;

    let mut tracks = remove_disliked_pending_in(deps.db, &songs, &pending, threshold).await;
    let origin = match &permit {
        AutoDeletePermit::Allowed => Some(Origin::Auto),
        AutoDeletePermit::ForApp { grant_id } => Some(Origin::Mcp {
            grant_id: grant_id.clone(),
        }),
        AutoDeletePermit::NotForThisApp => None,
    };
    let replace = origin.clone().map_or(Replace::Never, Replace::Guarded);
    for song in &songs {
        if let Some(PendingAction::Promote(track)) = pending_action(song, &pending, threshold) {
            let promoted = promote(deps.db, deps.discovery, &user, &track.id, replace.clone());
            let result = promoted.await.map_err(NotDone::into_failure);
            match &result {
                Err(e) if is_not_pending(e) => {
                    info!("Skipped promoting track {}: {}", track.title, e.message)
                }
                Err(e) => warn!("Failed to promote track {}: {}", track.title, e.message),
                Ok(_) => {}
            }
            let entry = SyncedTrack::of(song, SyncAction::Promote, Some(track));
            tracks.push(entry.with_failure(result.err()));
        }
    }
    let listed = tracks.len();
    tracks.retain(|t| !t.not_pending());
    let mut not_pending = (listed - tracks.len()) as u32;

    let (mut kept, mut kept_not_stored) = (0, None);
    let auto_delete = match (settings.auto_delete_enabled, &origin) {
        (false, _) => {
            info!("Auto-delete is disabled for this user");
            AutoDeleteRan::Off
        }
        (true, None) => {
            info!("Auto-delete is skipped: this app may not delete");
            AutoDeleteRan::NotAllowed
        }
        (true, Some(origin)) => {
            let mut run =
                auto_delete_songs(deps.guard, &user, &songs, &pending, deps.queue, origin).await;
            let listed = run.len();
            run.retain(|t| !t.not_pending());
            not_pending += (listed - run.len()) as u32;
            let kept_tracks = kept_of(&run);
            if let Err(e) = DeletionBlockRow::replace_in(deps.db, &ctx.user_id, &kept_tracks).await
            {
                let name = &ctx.username;
                warn!("Could not store the tracks auto-delete kept for {name}: {e}");
                kept_not_stored = Some(database_failure(Stage::Delete, e));
            }
            kept = kept_tracks.len() as u32;
            tracks.extend(run);
            AutoDeleteRan::On
        }
    };
    let report = SyncReport {
        scanned: songs.len() as u32,
        auto_delete,
        tracks,
        not_pending,
        kept,
        kept_not_stored,
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
/// An entry for each removal, done or failed; one of a track no longer pending carries
/// track_not_pending, which sync_in counts as skipped.
pub(crate) async fn remove_disliked_pending_in(
    db: &SqlitePool,
    songs: &[SubsonicSong],
    pending: &HashMap<String, &DiscoveryTrack>,
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
        match &result {
            Err(f) if is_not_pending(f) => {
                info!("Skipped removing track {}: {}", track.title, f.message)
            }
            Err(f) => warn!("Failed to remove track {}: {}", track.title, logged(f)),
            Ok(()) => {}
        }
        let entry = SyncedTrack::of(song, SyncAction::RemoveFromDiscovery, Some(track));
        removed.push(entry.with_failure(result.err()));
    }
    removed
}

/// Deletes through the guard every song of `songs` that `actor` rated 1 and that is not in
/// `pending` (pending_songs): a library file is carried out as `origin`, a pending discovery track
/// the guard finds is removed as a staged track. Only a song the fresh record still shows at 1
/// star is removed; one rated otherwise since `songs` was listed is skipped, also when the
/// guard refuses it. A song whose file an earlier deletion of `actor` removed, which Navidrome
/// lists until it notices, is skipped. Every other song is an entry, with the failure that kept
/// it when one did; a discovery track no longer pending carries track_not_pending, which
/// sync_in counts as skipped. Once the day's room of an app (Origin::Mcp) is used up, each
/// further song is kept with that refusal and Navidrome is not read for it.
pub(crate) async fn auto_delete_songs(
    guard: &Guard<'_>,
    actor: &UserId,
    songs: &[SubsonicSong],
    pending: &HashMap<String, &DiscoveryTrack>,
    queue: &ImportQueue,
    origin: &Origin,
) -> Vec<SyncedTrack> {
    let mut run = Vec::new();
    let deleted = match DeletionReviewRow::deleted_files_in(guard.db, &actor.to_string()).await {
        Ok(deleted) => deleted,
        Err(e) => {
            warn!("Auto-delete could not read which files it deleted before: {e}");
            vec![]
        }
    };
    let mut room_used_up: Option<Failure> = None;
    for song in songs {
        if song.user_rating != Some(1) || pending.contains_key(&song.id) {
            continue;
        }
        let artist = song.artist.as_deref().unwrap_or_default();
        let skipped = |why: &str| info!("Auto-delete skipped {artist} - {}: {why}", song.title);
        if deleted_before(guard, actor, &deleted, &song.id).await {
            skipped("deleted earlier, and Navidrome has not noticed yet");
            continue;
        }
        let delete = SyncedTrack::of(song, SyncAction::Delete, None);
        if let Some(limit) = &room_used_up {
            run.push(delete.with_failure(Some(limit.clone())));
            continue;
        }
        let entry = match guard.check_song(actor, &song.id).await {
            Ok(Verdict::Library(checked)) if checked.song.rating == 1 => {
                let forget = ForgetVia::Queue(queue.clone());
                let reason = DeletionReason::Disliked;
                // Its only warning, an unrecorded deletion, is logged where it happens.
                let done = guard
                    .carry_out(actor, checked, reason, origin, forget)
                    .await;
                let failure = done.err().map(NotRemoved::into_failure);
                if failure.as_ref().map(|f| f.code) == Some(FailureCode::DeleteDailyLimit) {
                    room_used_up = failure.clone();
                }
                Some(delete.with_failure(failure))
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
                if result.as_ref().is_err_and(is_not_pending) {
                    skipped("no longer pending in discovery");
                }
                Some(remove.with_failure(result.err()))
            }
            Ok(Verdict::Discovery { .. }) => {
                skipped("no longer rated 1");
                None
            }
            Err(f) => match still_rated_one(guard, actor, &song.id).await {
                true => Some(delete.with_failure(Some(f))),
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
    use crate::core::delete_guard::blocked_by_other_user;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::discovery::promote_file_gone;
    use crate::core::discovery::test_support::TestDiscovery;
    use crate::core::test_ctx;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::models::user_settings::UpdateUserSettings;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        album_body, search3_body, song_body, subsonic_failed, subsonic_song, NavidromeFor,
        NavidromeStub, HOLY,
    };
    use shared::navidrome::SyncResult;
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
    async fn a_one_star_song_rating_is_refused_whatever_auto_delete_says_now() {
        // Auto-delete off: the user may turn it on before the next sync, which would then delete
        // the file as Soulful's own deletion.
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(4), false)],
        );
        let (guard, alice, holy) = (lib.guard(), lib.alice(), Target::Song(HOLY.0.into()));
        let refused = Err(NotSet::WouldDelete {
            discovery_track_id: None,
            failure: one_star_refused(),
        });
        for on in [false, true] {
            set(&lib.db, auto_delete(on)).await;
            assert_eq!(
                rate(&guard, &navi.client(), &alice, &holy, 1).await,
                refused,
                "auto-delete {on}"
            );
        }
        assert!(navi.posted("/rest/setRating").is_empty());
        let a = rate(&guard, &navi.client(), &alice, &holy, 2)
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
            }
        );
        // A setting that cannot be read refuses too; clearing a rating reads none.
        lib.db.close().await;
        let e = rate(&guard, &navi.client(), &alice, &holy, 3)
            .await
            .unwrap_err();
        assert_eq!(
            (e.after_write(), e.failure().code),
            (false, FailureCode::SettingsUnreadable)
        );
        rate(&guard, &navi.client(), &alice, &holy, 0)
            .await
            .unwrap();
        assert_eq!(
            navi.posted("/rest/setRating"),
            [
                format!("id={}&rating=2", HOLY.0),
                format!("id={}&rating=0", HOLY.0)
            ]
        );
        let f = one_star_refused();
        assert_eq!(
            (f.code, f.stage, f.retry, f.user_action.map(|a| a.code)),
            (
                FailureCode::PermissionNotGranted,
                Stage::Input,
                RetryAdvice::NeedsUser,
                Some(UserActionCode::RateInNavidrome)
            )
        );
    }

    #[tokio::test]
    async fn any_rating_of_a_pending_track_told_by_its_file_is_refused() {
        // No song id yet: the guard tells the track by the file the song's native record names.
        // Every rating is refused, whatever the threshold now: the user may lower it before the
        // sync, which would then promote the track as Soulful's own deletion.
        let lib = Library::new().await;
        lib.song(HOLY.0, "alice/Discovery/Balanced/03.flac", b"audio");
        let staged = lib.root.join("music/alice/Discovery/Balanced/03.flac");
        lib.pending_track("t1", "alice-music", "Balanced", &staged, None)
            .await;
        // A library song whose file has the same name is not that track.
        lib.song("s6", "alice/A/03.flac", b"audio");
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                discovery_promote_threshold: Some(4),
                ..Default::default()
            },
        )
        .await;
        let navi = NavidromeStub::start();
        navi.answer_when(
            "/rest/getSong",
            &format!("id={}", HOLY.0),
            200,
            &song_body(HOLY.0, HOLY.1, None, true),
        );
        navi.answer(
            "/rest/getSong",
            &[&song_body("s6", "Silent Night", None, false)],
        );
        let (guard, holy) = (lib.guard(), Target::Song(HOLY.0.into()));
        for rating in [1, 3, 5] {
            assert_eq!(
                rate(&guard, &navi.client(), &lib.alice(), &holy, rating).await,
                Err(NotSet::WouldDelete {
                    discovery_track_id: Some("t1".into()),
                    failure: pending_rating_refused()
                }),
                "rating {rating}"
            );
        }
        assert!(navi.posted("/rest/setRating").is_empty());
        let a = rate(&guard, &navi.client(), &lib.alice(), &holy, 0)
            .await
            .unwrap();
        assert_eq!((a.previous_rating, a.previously_starred), (None, true));
        let silent = Target::Song("s6".into());
        rate(&guard, &navi.client(), &lib.alice(), &silent, 3)
            .await
            .unwrap();
        assert_eq!(
            navi.posted("/rest/setRating"),
            [
                format!("id={}&rating=0", HOLY.0),
                "id=s6&rating=3".to_string()
            ]
        );
        // A song whose file cannot be found here cannot be told apart from the track.
        std::fs::remove_file(lib.root.join("music/alice/A/03.flac")).unwrap();
        let e = rate(&guard, &navi.client(), &lib.alice(), &silent, 3)
            .await
            .unwrap_err();
        assert_eq!(
            (e.after_write(), e.failure().code),
            (false, FailureCode::DeletePathUnresolved)
        );
        assert_eq!(navi.posted("/rest/setRating").len(), 2);
        assert_eq!(lib.fake.ratings_set(), vec![], "the guard wrote no rating");
    }

    #[tokio::test]
    async fn a_rating_is_refused_for_a_pending_track_of_any_folder_of_the_user() {
        // t1 is pending in alice-music while the discovery folder is alice-other: the sync does
        // not read it now, but would act on it once the folder is alice-music again.
        let lib = Library::new().await;
        lib.folder("alice", "alice-other", "music/alice-other", false)
            .await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some(HOLY.0))
            .await;
        // Another user's pending track of the same song is not alice's.
        let bobs = lib.root.join("music/bob/Discovery/Explore/03.flac");
        lib.pending_track("t2", "bob-music", "Explore", &bobs, Some(HOLY.0))
            .await;
        let discovery_folder = |id: &str| UpdateUserSettings {
            discovery_folder_id: Some(id.into()),
            ..Default::default()
        };
        set(&lib.db, discovery_folder("alice-other")).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, false)]);
        let (guard, holy) = (lib.guard(), Target::Song(HOLY.0.into()));
        let refused = |track: &str| {
            Err(NotSet::WouldDelete {
                discovery_track_id: Some(track.into()),
                failure: pending_rating_refused(),
            })
        };
        assert_eq!(
            rate(&guard, &navi.client(), &lib.alice(), &holy, 5).await,
            refused("t1")
        );
        // A track of the same song in the discovery folder is the one named: the next sync acts
        // on it.
        let other = lib.root.join("music/alice-other/Discovery/Explore/03.flac");
        lib.pending_track("t3", "alice-other", "Explore", &other, Some(HOLY.0))
            .await;
        assert_eq!(
            rate(&guard, &navi.client(), &lib.alice(), &holy, 5).await,
            refused("t3")
        );
        set(&lib.db, discovery_folder("alice-music")).await;
        assert_eq!(
            rate(&guard, &navi.client(), &lib.alice(), &holy, 5).await,
            refused("t1")
        );
        assert_eq!(navi.posted("/rest/setRating").len(), 0);
        // With neither pending any more, bob's track is no reason to refuse.
        sqlx::query("UPDATE discovery_tracks SET status = 'Removed' WHERE id IN ('t1', 't3')")
            .execute(&lib.db)
            .await
            .unwrap();
        rate(&guard, &navi.client(), &lib.alice(), &holy, 5)
            .await
            .unwrap();
        assert_eq!(navi.posted("/rest/setRating").len(), 1);
    }

    #[tokio::test]
    async fn an_album_rated_one_star_is_written_while_auto_delete_is_on() {
        // Auto-delete reads song ratings only.
        let lib = Library::new().await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getAlbum", &[&album_body(WINTER, "Winter Songs", 0)]);
        let winter = Target::Album(WINTER.into());
        let a = rate(&lib.guard(), &navi.client(), &lib.alice(), &winter, 1)
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
        assert_eq!((a.previous_rating, a.previously_starred), (Some(2), true));
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
        let lib = Library::new().await;
        let alice = UserId::new("alice");
        assert_eq!(
            rate(
                &lib.guard(),
                &navi.client(),
                &alice,
                &Target::Song("gone".into()),
                5
            )
            .await,
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
        let lib = Library::new().await;
        let holy = Target::Song(HOLY.0.into());
        assert_eq!(
            rate(
                &lib.guard(),
                &navi.client(),
                &UserId::new("alice"),
                &holy,
                2
            )
            .await,
            Err(NotSet::WriteFailed(song_not_found()))
        );
    }

    #[tokio::test]
    async fn pending_songs_tells_tracks_by_their_file_as_the_guard_does() {
        let lib = Library::new().await;
        lib.song("s5", "alice/Discovery/Explore/03.flac", b"audio");
        lib.song("s6", "alice/A/03.flac", b"audio");
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let rated = |id: &str, rating: u8, size: u64| SubsonicSong {
            // The Subsonic path names no file of this machine.
            path: Some("Marrow Choir/Explore/03.flac".into()),
            size: Some(size),
            ..subsonic_song(id, "O Holy Night", Some(rating))
        };
        let songs = [
            rated("s5", 5, 5),
            rated("s6", 5, 5),
            // No staged file has its size: its record is not read.
            rated("s7", 5, 9),
            // Below the threshold and above 1: the sync does nothing with it.
            rated("s8", 3, 5),
        ];
        let pending = std::slice::from_ref(&track);
        let found = pending_songs(&lib.guard(), &lib.alice(), &songs, pending, 4).await;
        assert_eq!(
            found
                .iter()
                .map(|(s, t)| (s.as_str(), t.id.as_str()))
                .collect::<Vec<_>>(),
            vec![("s5", "t1")]
        );
        assert_eq!(lib.fake.reads_by("alice"), 2);
        // A track with a song id is that song's, read from nothing.
        let by_id = DiscoveryTrack {
            song_id: Some("s6".into()),
            ..track.clone()
        };
        let tracks = [by_id];
        let found = pending_songs(&lib.guard(), &lib.alice(), &songs[1..2], &tracks, 4).await;
        assert_eq!(found.keys().collect::<Vec<_>>(), ["s6"]);
        assert_eq!(lib.fake.reads_by("alice"), 2);
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
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue(),
            &Origin::Auto,
        )
        .await;
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
                "Another listener rated, starred or kept this track in a playlist"
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
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue(),
            &Origin::Auto,
        )
        .await;
        assert_eq!(run, vec![]);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_song_rated_again_since_the_listing_is_not_kept_when_the_guard_refuses_it() {
        let lib = Library::new().await;
        lib.fake.stars("bob", "s1");
        lib.fake.rates("alice", "s1", 4);
        let (songs, queue) = ([subsonic_song("s1", "O Holy Night", Some(1))], queue());
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
        assert_eq!(run, vec![]);
        // Still at 1 star, the same refusal keeps it.
        lib.fake.rates("alice", "s1", 1);
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
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
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
        // The next sync lists it again: Navidrome has not scanned since. Only its record is
        // read: the guard never checks it, so no rating is sent.
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
        assert_eq!(run, vec![]);
        assert_eq!(lib.fake.ratings_set().len(), 1);
        // A file at that path again (re-imported) is checked as usual.
        lib.write("alice/A/x.flac", b"audio");
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
        assert_eq!(acts(&run), vec![("s1", SyncAction::Delete, None)]);
    }

    #[tokio::test]
    async fn a_song_navidrome_moved_to_another_file_since_its_deletion_is_checked_again() {
        let lib = Library::new().await;
        let (songs, queue) = ([subsonic_song("s1", "O Holy Night", Some(1))], queue());
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
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
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue,
            &Origin::Auto,
        )
        .await;
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
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &songs,
            &HashMap::new(),
            &queue(),
            &Origin::Auto,
        )
        .await;
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
    async fn a_one_star_discovery_track_told_by_its_file_is_removed_as_a_staged_track() {
        // The track has no song id yet: the song's native record names its staged file.
        let lib = Library::new().await;
        let rel = "alice/Discovery/Explore/Marrow Choir/03 - O Holy Night.flac";
        lib.song("s9", rel, b"audio");
        let staged = lib.root.join("music").join(rel);
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let song = SubsonicSong {
            size: Some(5),
            ..subsonic_song("s9", "O Holy Night", Some(1))
        };
        let songs = std::slice::from_ref(&song);
        let tracks = vec![track];
        let pending = pending_songs(&lib.guard(), &lib.alice(), songs, &tracks, 3).await;
        assert!(
            matches!(pending_action(&song, &pending, 1), Some(PendingAction::Promote(t)) if t.id == "t1")
        );
        assert_eq!(
            remove_disliked_pending_in(&lib.db, songs, &pending, 1).await,
            vec![]
        );
        assert!(staged.exists());
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            songs,
            &pending,
            &queue(),
            &Origin::Auto,
        )
        .await;
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
        assert_eq!(lib.fake.ratings_set(), vec![], "the guard never checked it");
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
                "Another listener rated, starred or kept this track in a playlist"
            )
        );
        assert_eq!(report.kept_not_stored, None);
    }

    #[tokio::test]
    async fn kept_tracks_that_cannot_be_stored_are_in_the_report() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.rates("bob", "s2", 4);
        set(&lib.db, auto_delete(true)).await;
        // Settings lists an earlier sync's kept track, and the database refuses to replace it.
        let earlier = KeptTrack {
            song_id: "s9".into(),
            title: "Away in a Manger".into(),
            artist: "Marrow Choir".into(),
            failure: blocked_by_other_user(),
        };
        DeletionBlockRow::replace_in(&lib.db, "alice", &[earlier])
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_kept BEFORE DELETE ON deletion_blocks \
             BEGIN SELECT RAISE(FAIL, 'disk I/O error'); END",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[
                ("s1", "O Holy Night", 1),
                ("s2", "Silent Night", 1),
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
        assert_eq!(
            (report.counts().deleted_tracks, report.kept),
            (1, 1),
            "what the sync did is still reported"
        );
        let f = report
            .kept_not_stored
            .expect("the store failure is reported");
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::Delete,
                "Soulful's database is unavailable"
            )
        );
        assert!(f.cause.unwrap().raw.contains("disk I/O error"));
        let listed = DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap();
        assert_eq!(
            listed.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(),
            vec!["Away in a Manger"]
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
        assert_eq!(lib.fake.reads_by("alice"), 0, "the guard never ran");
    }

    #[tokio::test]
    async fn a_sync_an_app_started_deletes_as_that_app_within_its_daily_limit() {
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.song("s3", "alice/C/z.flac", b"audio");
        lib.fake.rates("alice", "s3", 1);
        lib.app("g1", "alice", true).await;
        lib.app_deletions(99, 3_600).await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[
                &search3_body(&[
                    ("s1", "O Holy Night", 1),
                    ("s2", "Silent Night", 1),
                    ("s3", "Away in a Manger", 1),
                ]),
                &search3_body(&[("s2", "Silent Night", 1), ("s3", "Away in a Manger", 1)]),
            ],
        );
        let (of, guard, runs, discovery, queue) = (
            NavidromeFor(Ok(navi.client())),
            lib.guard(),
            SyncRuns::default(),
            effects(),
            queue(),
        );
        let deps = deps(&lib, &of, &guard, &queue, &discovery, &runs);
        let app = AutoDeletePermit::ForApp {
            grant_id: "g1".into(),
        };
        let report = sync_in(&deps, &test_ctx("alice"), app).await.unwrap();
        assert_eq!(
            report
                .tracks
                .iter()
                .map(|t| (t.song_id.as_str(), t.failure.as_ref().map(|f| f.code)))
                .collect::<Vec<_>>(),
            vec![
                ("s1", None),
                ("s2", Some(FailureCode::DeleteDailyLimit)),
                ("s3", Some(FailureCode::DeleteDailyLimit))
            ]
        );
        assert_eq!(report.auto_delete, AutoDeleteRan::On);
        // Once the room is used up, s3 is kept without Navidrome being read for it: rule 4 wrote
        // the ratings of s1 and s2 only.
        assert_eq!(
            (lib.fake.ratings_set().iter())
                .map(|(_, song, _)| song.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2"]
        );
        assert!(
            !lib.root.join("music/alice/A/x.flac").exists()
                && lib.root.join("music/alice/B/y.flac").exists()
        );
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT origin, client FROM deletion_reviews WHERE song_id = 's1'")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        assert_eq!(row, ("mcp".into(), Some("claude.ai".into())));
        // Settings lists the tracks the limit kept, by artist and title.
        let listed = DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap();
        assert_eq!(
            listed.iter().map(|k| k.title.as_str()).collect::<Vec<_>>(),
            ["Away in a Manger", "Silent Night"]
        );
        // The web UI's sync and the automation stay origin auto and are not counted.
        let report = sync_in(&deps, &test_ctx("alice"), AutoDeletePermit::Allowed)
            .await
            .unwrap();
        assert_eq!(
            report
                .tracks
                .iter()
                .map(|t| (t.song_id.as_str(), t.failure.is_none()))
                .collect::<Vec<_>>(),
            vec![("s2", true), ("s3", true)]
        );
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT origin, client FROM deletion_reviews WHERE song_id = 's2'")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        assert_eq!(row, ("auto".into(), None));
    }

    #[tokio::test]
    async fn a_promote_replaces_a_library_copy_only_as_the_permit_allows() {
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
        let (first, second, third) = (staged("03.flac"), staged("04.flac"), staged("05.flac"));
        lib.pending_track("t1", "alice-music", "Explore", &first, Some("s5"))
            .await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&search3_body(&[
                ("s5", "O Holy Night", 5),
                ("s6", "Silent Night", 5),
                ("s7", "Away in a Manger", 5),
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
        // A sync an app with the switch started replaces as that app.
        lib.pending_track("t3", "alice-music", "Explore", &third, Some("s7"))
            .await;
        let app = AutoDeletePermit::ForApp {
            grant_id: "g1".into(),
        };
        sync_in(&deps, &test_ctx("alice"), app).await.unwrap();
        let music = lib.root.join("music/alice");
        let as_app = Origin::Mcp {
            grant_id: "g1".into(),
        };
        assert_eq!(
            test.imports(),
            vec![
                (first, music.clone(), Replace::Never),
                (second, music.clone(), Replace::Guarded(Origin::Auto)),
                (third, music, Replace::Guarded(as_app))
            ]
        );
    }

    #[tokio::test]
    async fn a_track_another_action_took_first_is_skipped_and_counted() {
        // Two songs whose native records name each pending track's staged file: the first promote
        // and the first removal take the track, the second finds it no longer pending.
        let lib = Library::new().await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await;
        for (track, songs, name) in [
            ("t1", ["s5", "s6"], "03.flac"),
            ("t2", ["s7", "s8"], "04.flac"),
        ] {
            let rel = format!("alice/Discovery/Explore/{name}");
            for song in songs {
                lib.song(song, &rel, b"audio");
            }
            let staged = lib.root.join("music").join(&rel);
            lib.pending_track(track, "alice-music", "Explore", &staged, None)
                .await;
        }
        let mut body: serde_json::Value = serde_json::from_str(&search3_body(&[
            ("s5", "O Holy Night", 5),
            ("s6", "O Holy Night", 5),
            ("s7", "Silent Night", 1),
            ("s8", "Silent Night", 1),
        ]))
        .unwrap();
        let songs = body["subsonic-response"]["searchResult3"]["song"]
            .as_array_mut()
            .unwrap();
        for song in songs.iter_mut() {
            song["size"] = serde_json::json!(5);
        }
        let navi = NavidromeStub::start();
        navi.answer("/rest/search3", &[&body.to_string()]);
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
            acts(&report.tracks),
            vec![
                ("s7", SyncAction::RemoveFromDiscovery, None),
                ("s5", SyncAction::Promote, None)
            ]
        );
        assert_eq!(report.not_pending, 2);
        assert_eq!(
            report.counts(),
            SyncResult {
                deleted_tracks: 0,
                promoted_tracks: 1,
                removed_tracks: 1,
                total_songs_scanned: 4,
                kept_tracks: 0
            }
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
