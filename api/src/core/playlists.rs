//! The caller's Navidrome playlists and other users' public ones, as the
//! tools report them (D10), and the writes to the caller's own: each checks
//! the owner first and reads its change back from Navidrome.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use serde::Serialize;
use shared::failure::{
    cut_json, Failure, FailureCode, RetryAdvice, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS,
};
use soulbeet::navidrome::models::{PlaylistTrack, SubsonicPlaylist};
use soulbeet::navidrome::{NavidromeClient, PlaylistMeta};
use soulbeet::NavidromeError;
use tokio::sync::OwnedMutexGuard;

use crate::auth::UserCtx;
use crate::core::library::{navidrome_failure, SongFacts};

/// Most song ids one playlist write takes.
pub(crate) const SONG_IDS_MAX: usize = 1000;

/// Highest entry position a track write takes.
pub(crate) const POSITION_MAX: u32 = 100_000;

/// The lock of each playlist and of each user's playlist names that a call holds or waits for.
/// An entry whose lock nobody holds is dead and pruned at the next `lock`.
static LOCKS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// The lock of `key`, held from a write's first read to its read back, so a read, the list
/// computed from it and its write never interleave with another of Soulful's writes. A call
/// taking both takes `names_key` before `playlist_key`.
pub(crate) async fn lock(key: &str) -> OwnedMutexGuard<()> {
    let mutex = {
        let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        locks.retain(|_, held| held.strong_count() > 0);
        match locks.get(key).and_then(Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_string(), Arc::downgrade(&mutex));
                mutex
            }
        }
    };
    mutex.lock_owned().await
}

/// The keys of the locks someone holds or waits for.
#[cfg(test)]
fn lock_entries() -> Vec<String> {
    let locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    (locks.iter())
        .filter(|(_, held)| held.strong_count() > 0)
        .map(|(key, _)| key.clone())
        .collect()
}

/// The lock key of playlist `id`.
pub(crate) fn playlist_key(id: &str) -> String {
    format!("playlist:{id}")
}

/// The lock key of the caller's playlist names: a name check and the write it allows.
pub(crate) fn names_key(ctx: &UserCtx) -> String {
    format!("names:{}", ctx.user_id)
}

/// A playlist as the tools report it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistFacts {
    pub playlist_id: String,
    pub name: String,
    pub comment: Option<String>,
    /// The owner's Navidrome user name.
    pub owner: String,
    /// Whether the caller owns it; false for another user's public playlist.
    pub owned: bool,
    pub public: bool,
    pub song_count: u32,
    pub duration_s: u32,
    pub created: String,
    pub changed: String,
}

/// A playlist entry: its 0-based place among all of the playlist's tracks,
/// the index updatePlaylist's songIndexToRemove takes
/// (core/playlists/playlists.go:188-197), whether its file is gone, and the
/// song. Tracks Navidrome does not list to this account leave gaps.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistEntry {
    pub position: usize,
    pub missing: bool,
    #[serde(flatten)]
    pub song: SongFacts,
}

/// `p` as the tools report it. The caller owns it when its owner is the
/// caller's user name ignoring ASCII case: Navidrome finds a user by
/// `user_name COLLATE NOCASE` (persistence/user_repository.go:203) and
/// Soulful signs in to Navidrome with the Soulful user name (D10).
pub(crate) fn facts(p: &SubsonicPlaylist, username: &str) -> PlaylistFacts {
    PlaylistFacts {
        playlist_id: p.id.clone(),
        name: cut_json(p.name.clone(), TEXT_MAX_CHARS),
        comment: (!p.comment.is_empty()).then(|| cut_json(p.comment.clone(), RAW_MAX_CHARS)),
        owner: cut_json(p.owner.clone(), TEXT_MAX_CHARS),
        owned: owns(p, username),
        public: p.public,
        song_count: p.song_count,
        duration_s: p.duration,
        created: p.created.clone(),
        changed: p.changed.clone(),
    }
}

/// Whether the caller, `username`, owns `p`; see `facts`.
fn owns(p: &SubsonicPlaylist, username: &str) -> bool {
    p.owner.eq_ignore_ascii_case(username)
}

/// The caller's own playlists and other users' public ones, in Navidrome's
/// order, by name (server/subsonic/playlists.go:18-30). Navidrome lists an
/// admin every playlist (persistence/playlist_repository.go:84-93); another
/// user's private one is left out.
pub async fn list(navi: &NavidromeClient, ctx: &UserCtx) -> Result<Vec<PlaylistFacts>, Failure> {
    let playlists = navi
        .get_playlists()
        .await
        .map_err(|e| navidrome_failure(Stage::Playlist, &e))?;
    Ok(playlists
        .iter()
        .map(|p| facts(p, &ctx.username))
        .filter(|p| p.owned || p.public)
        .collect())
}

/// The playlist and every track of it in Navidrome's order, missing files
/// included. playlist_not_found(id) when Navidrome has none, and for another
/// user's private playlist, which is never read further: the answer is the
/// same as for an id that does not exist.
pub async fn get(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<(PlaylistFacts, Vec<PlaylistEntry>), Failure> {
    let (playlist, tracks) = read(navi, ctx, id).await?;
    Ok((
        facts(&playlist, &ctx.username),
        tracks.iter().map(entry).collect(),
    ))
}

/// `get`'s reads as Navidrome answers them: the playlist, whose name and comment are not cut,
/// and its native track rows.
async fn read(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<(SubsonicPlaylist, Vec<PlaylistTrack>), Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::Playlist, &e);
    let playlist = navi
        .get_playlist(id)
        .await
        .map_err(failure)?
        .filter(|p| p.public || owns(p, &ctx.username))
        .ok_or_else(|| playlist_not_found(id))?;
    // None here is Navidrome's own 404 for a playlist it no longer lists
    // to this account: it was deleted, or made private, since getPlaylist.
    let tracks = navi
        .playlist_tracks(id)
        .await
        .map_err(failure)?
        .ok_or_else(|| playlist_not_found(id))?;
    Ok((playlist, tracks))
}

/// A native track row as an entry.
fn entry(track: &PlaylistTrack) -> PlaylistEntry {
    PlaylistEntry {
        position: position_of(track),
        missing: track.song.missing,
        song: SongFacts::from(&track.song),
    }
}

/// A native track row's 0-based position: its 1-based place less one.
fn position_of(track: &PlaylistTrack) -> usize {
    track.position.get() as usize - 1
}

/// The song ids of native track rows, in order.
fn ids_of(tracks: &[PlaylistTrack]) -> Vec<String> {
    tracks.iter().map(|t| t.song.id.clone()).collect()
}

/// No playlist with this id, or another user's private one. It carries no
/// cause, so the two read alike.
pub(crate) fn playlist_not_found(id: &str) -> Failure {
    Failure::new(
        FailureCode::PlaylistNotFound,
        Stage::Playlist,
        RetryAdvice::NotRetryable,
        format!("Playlist {id} not found"),
    )
}

/// A playlist as Navidrome lists it after a write: its facts and its tracks' song ids in
/// Navidrome's order, missing files included.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Written {
    pub playlist: PlaylistFacts,
    pub song_ids: Vec<String>,
    /// The entries a rewrite of the track list dropped because Navidrome does not list them to
    /// this account (tracks of other libraries); 0 for any other write.
    pub hidden_dropped: u32,
}

fn written(p: &SubsonicPlaylist, tracks: &[PlaylistTrack], ctx: &UserCtx) -> Written {
    Written {
        playlist: facts(p, &ctx.username),
        song_ids: ids_of(tracks),
        hidden_dropped: 0,
    }
}

/// What a refused or unfinished playlist write is, for the tool to give facts and next.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlaylistRefusal {
    /// Navidrome or Soulful failed before any write request was sent, playlist_not_found(id)
    /// among them: the failure says which.
    Failed(Failure),
    /// A write request was sent and it, or the read back after it, failed, so the change may
    /// have been made; playlist_not_found(id) when Navidrome no longer had the playlist to write.
    WriteFailed(Failure),
    NotOwner {
        playlist_id: String,
    },
    NameTaken {
        name: String,
        existing: Vec<String>,
    },
    /// Song ids Navidrome does not list for the caller, and those it lists with a missing file.
    SongsNotPresent {
        unknown: Vec<String>,
        missing_file: Vec<String>,
    },
    /// The read back differs from the list written: the songs missing from it and those not
    /// asked for, each as often as it differs, and whether the songs both have keep their order.
    TracksNotWritten {
        playlist_id: String,
        name: String,
        missing: Vec<String>,
        unexpected: Vec<String>,
        order_kept: bool,
    },
    NotWritten {
        playlist_id: String,
        name: String,
        what: NotWrittenPart,
    },
    /// Playlist `playlist_id` was created but a later step of the same call failed: writing its
    /// comment or public flag (`described` false), or the read back after them.
    Unfinished {
        playlist_id: String,
        failure: Failure,
        described: bool,
    },
    /// A track edit of a playlist whose tracks Navidrome sets itself (D4). `would_be` is the list
    /// the edit would have left, its missing files left out, for a static copy.
    NotEditable {
        playlist_id: String,
        name: String,
        why: NotEditable,
        would_be: Vec<String>,
    },
    /// A removal's `position` holds `found` (None: no entry there), not the song `expected` (D3).
    Changed {
        playlist_id: String,
        name: String,
        position: usize,
        expected: String,
        found: Option<String>,
    },
    /// An insert before `position`, which no entry holds and which is not right after the last.
    NoEntryAt {
        playlist_id: String,
        position: usize,
    },
}

/// Why Navidrome sets a playlist's tracks itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NotEditable {
    /// A smart playlist: its rules pick its tracks.
    Smart,
    /// Synced from a playlist file: Navidrome replaces its tracks when the file changes.
    Synced,
}

/// The part of a playlist a write changed that Navidrome does not show changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NotWrittenPart {
    Name,
    Comment,
    Public,
    Delete,
}

impl PlaylistRefusal {
    /// The refusal as a failure. Its message names no playlist or song: their names are
    /// Navidrome's text, which goes only into facts.
    pub(crate) fn failure(&self) -> Failure {
        let refused = |code, stage, message: String| {
            Failure::new(code, stage, RetryAdvice::NotRetryable, message)
        };
        match self {
            PlaylistRefusal::Failed(f)
            | PlaylistRefusal::WriteFailed(f)
            | PlaylistRefusal::Unfinished { failure: f, .. } => f.clone(),
            PlaylistRefusal::NotOwner { .. } => refused(
                FailureCode::PlaylistNotOwner,
                Stage::Playlist,
                "Only the owner of this playlist can change it".into(),
            ),
            PlaylistRefusal::NameTaken { .. } => refused(
                FailureCode::PlaylistNameTaken,
                Stage::Playlist,
                "You already have a playlist with this name".into(),
            ),
            PlaylistRefusal::SongsNotPresent {
                unknown,
                missing_file,
            } => refused(
                FailureCode::UnknownHandle,
                Stage::Input,
                format!(
                    "Navidrome does not list {} of these songs for you and has {} with a missing file",
                    unknown.len(),
                    missing_file.len()
                ),
            ),
            PlaylistRefusal::TracksNotWritten {
                missing,
                unexpected,
                order_kept,
                ..
            } => refused(
                FailureCode::PlaylistTracksNotWritten,
                Stage::Playlist,
                format!(
                    "Navidrome does not list this playlist as written: {} song(s) missing, {} not asked for, order {}",
                    missing.len(),
                    unexpected.len(),
                    if *order_kept { "kept" } else { "changed" }
                ),
            ),
            PlaylistRefusal::NotWritten { what, .. } => {
                let message = match what {
                    NotWrittenPart::Name => "Navidrome answered the change of this playlist's name but does not show it",
                    NotWrittenPart::Comment => "Navidrome answered the change of this playlist's comment but does not show it",
                    NotWrittenPart::Public => "Navidrome answered the change of this playlist's public flag but does not show it",
                    NotWrittenPart::Delete => "Navidrome answered the delete of this playlist but still lists it",
                };
                refused(
                    FailureCode::PlaylistNotWritten,
                    Stage::Playlist,
                    message.into(),
                )
            }
            PlaylistRefusal::NotEditable { why, .. } => refused(
                FailureCode::PlaylistNotEditable,
                Stage::Playlist,
                match why {
                    NotEditable::Smart => "This is a smart playlist: Navidrome fills its tracks from its rules",
                    NotEditable::Synced => "This playlist is synced from a playlist file: Navidrome replaces its tracks from that file when the file changes",
                }
                .into(),
            ),
            PlaylistRefusal::Changed { position, .. } => refused(
                FailureCode::PlaylistChanged,
                Stage::Playlist,
                format!("Position {position} of this playlist no longer holds the song given: the playlist changed since it was read"),
            ),
            PlaylistRefusal::NoEntryAt { position, .. } => refused(
                FailureCode::InvalidInput,
                Stage::Input,
                format!("This playlist has no entry at position {position}: use a position get_playlist lists, or leave position out to add at the end"),
            ),
        }
    }

    /// Whether a write request was sent before the refusal, so the playlist may have changed.
    pub(crate) fn after_write(&self) -> bool {
        matches!(
            self,
            PlaylistRefusal::WriteFailed(_)
                | PlaylistRefusal::TracksNotWritten { .. }
                | PlaylistRefusal::NotWritten { .. }
                | PlaylistRefusal::Unfinished { .. }
        )
    }
}

/// A playlist to create: `name` is trimmed before use.
pub(crate) struct NewPlaylist {
    pub name: String,
    pub comment: Option<String>,
    pub public: Option<bool>,
    pub song_ids: Vec<String>,
    pub allow_duplicate_name: bool,
}

/// The parts of a playlist to change, each only when Some; an empty comment clears it.
pub(crate) struct MetaChange {
    pub name: Option<String>,
    pub comment: Option<String>,
    pub public: Option<bool>,
    pub allow_duplicate_name: bool,
}

fn failed(e: NavidromeError) -> PlaylistRefusal {
    PlaylistRefusal::Failed(navidrome_failure(Stage::Playlist, &e))
}

/// A write's answer for playlist `id`: Ok(false) is Navidrome no longer having it, as when it
/// went between the check and the write.
fn wrote(answer: Result<bool, NavidromeError>, id: &str) -> Result<(), Failure> {
    match answer {
        Ok(true) => Ok(()),
        Ok(false) => Err(playlist_not_found(id)),
        Err(e) => Err(navidrome_failure(Stage::Playlist, &e)),
    }
}

/// Playlist `id` as `ctx` may write it: one `get` reads, then NotOwner unless the caller owns
/// it. Another user's private playlist reads as not found, as `get` has it.
pub(crate) async fn owned(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<Written, PlaylistRefusal> {
    let (playlist, tracks) = owned_read(navi, ctx, id).await?;
    Ok(written(&playlist, &tracks, ctx))
}

/// `owned` as Navidrome answers it: the playlist and its native track rows.
async fn owned_read(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<(SubsonicPlaylist, Vec<PlaylistTrack>), PlaylistRefusal> {
    let (playlist, tracks) = read(navi, ctx, id).await.map_err(PlaylistRefusal::Failed)?;
    if !owns(&playlist, &ctx.username) {
        return Err(PlaylistRefusal::NotOwner {
            playlist_id: id.to_string(),
        });
    }
    Ok((playlist, tracks))
}

/// SongsNotPresent unless Navidrome lists every id for the caller, none with a missing file.
/// Navidrome writes an id it cannot read without an error, so this runs before every track
/// write. Each id is named once.
pub(crate) async fn present(
    navi: &NavidromeClient,
    song_ids: &[String],
) -> Result<(), PlaylistRefusal> {
    let songs = navi.get_native_songs(song_ids).await.map_err(failed)?;
    let listed: HashMap<&str, bool> = songs.iter().map(|s| (s.id.as_str(), s.missing)).collect();
    let (mut unknown, mut missing_file) = (vec![], vec![]);
    let mut seen = HashSet::new();
    for id in song_ids.iter().filter(|id| seen.insert(id.as_str())) {
        match listed.get(id.as_str()) {
            None => unknown.push(id.clone()),
            Some(true) => missing_file.push(id.clone()),
            Some(false) => {}
        }
    }
    if unknown.is_empty() && missing_file.is_empty() {
        Ok(())
    } else {
        Err(PlaylistRefusal::SongsNotPresent {
            unknown,
            missing_file,
        })
    }
}

/// NameTaken when one of the caller's own playlists, other than `except`, has exactly `name`,
/// unless `allow`: Navidrome allows duplicates. Another user's playlist never counts.
async fn name_free(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    name: &str,
    except: Option<&str>,
    allow: bool,
) -> Result<(), PlaylistRefusal> {
    if allow {
        return Ok(());
    }
    let playlists = navi.get_playlists().await.map_err(failed)?;
    let existing: Vec<String> = (playlists.iter())
        .filter(|p| owns(p, &ctx.username) && p.name == name && Some(p.id.as_str()) != except)
        .map(|p| p.id.clone())
        .collect();
    if existing.is_empty() {
        Ok(())
    } else {
        Err(PlaylistRefusal::NameTaken {
            name: name.to_string(),
            existing,
        })
    }
}

/// TracksNotWritten unless `w` lists exactly `asked`, in order.
fn tracks_written(w: &Written, asked: &[String]) -> Result<(), PlaylistRefusal> {
    if w.song_ids == asked {
        return Ok(());
    }
    let missing = surplus(asked, &w.song_ids);
    let unexpected = surplus(&w.song_ids, asked);
    let order_kept = shared_order(&w.song_ids, asked) == shared_order(asked, &w.song_ids);
    Err(PlaylistRefusal::TracksNotWritten {
        playlist_id: w.playlist.playlist_id.clone(),
        name: w.playlist.name.clone(),
        missing,
        unexpected,
        order_kept,
    })
}

/// Each id of `list` as often as it is in `list` more than in `other`, in `list`'s order.
fn surplus(list: &[String], other: &[String]) -> Vec<String> {
    let mut kept = counts(other);
    (list.iter())
        .filter(|id| !take(&mut kept, id))
        .cloned()
        .collect()
}

/// `list` with each id kept at its first places, as often as `other` has it.
fn shared_order<'a>(list: &'a [String], other: &[String]) -> Vec<&'a String> {
    let mut kept = counts(other);
    list.iter().filter(|id| take(&mut kept, id)).collect()
}

fn counts(list: &[String]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for id in list {
        *counts.entry(id.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Takes one `id` from `counts`, false when none is left.
fn take(counts: &mut HashMap<&str, usize>, id: &str) -> bool {
    match counts.get_mut(id) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// Creates a static playlist owned by the caller, under `names_key`: the name check and the
/// song check, createPlaylist, then updatePlaylist for the comment and public flag when given,
/// then one read back compared with the songs, comment and flag asked for. Once the playlist
/// exists, a failure is Unfinished.
pub(crate) async fn create(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    new: &NewPlaylist,
) -> Result<Written, PlaylistRefusal> {
    let _names = lock(&names_key(ctx)).await;
    let name = new.name.trim();
    name_free(navi, ctx, name, None, new.allow_duplicate_name).await?;
    present(navi, &new.song_ids).await?;
    let created = navi
        .create_playlist(name, &new.song_ids)
        .await
        .map_err(|e| PlaylistRefusal::WriteFailed(navidrome_failure(Stage::Playlist, &e)))?;
    let id = created.id.as_str();
    let unfinished = |failure: Failure, described: bool| PlaylistRefusal::Unfinished {
        playlist_id: created.id.clone(),
        failure,
        described,
    };
    if new.comment.is_some() || new.public.is_some() {
        let meta = PlaylistMeta {
            name: None,
            comment: new.comment.as_deref(),
            public: new.public,
        };
        wrote(navi.update_playlist_meta(id, &meta).await, id).map_err(|f| unfinished(f, false))?;
    }
    let (playlist, tracks) = read(navi, ctx, id).await.map_err(|f| unfinished(f, true))?;
    let w = written(&playlist, &tracks, ctx);
    tracks_written(&w, &new.song_ids)?;
    shows(&w, &playlist, None, new.comment.as_deref(), new.public)?;
    Ok(w)
}

/// NotWritten for the first of the asked `name`, `comment` and `public` that `p`, the read
/// back of `w`, does not show. Navidrome leaves an empty comment out, read as "".
fn shows(
    w: &Written,
    p: &SubsonicPlaylist,
    name: Option<&str>,
    comment: Option<&str>,
    public: Option<bool>,
) -> Result<(), PlaylistRefusal> {
    let what = if name.is_some_and(|name| p.name != name) {
        NotWrittenPart::Name
    } else if comment.is_some_and(|comment| p.comment != comment) {
        NotWrittenPart::Comment
    } else if public.is_some_and(|public| p.public != public) {
        NotWrittenPart::Public
    } else {
        return Ok(());
    };
    Err(PlaylistRefusal::NotWritten {
        playlist_id: w.playlist.playlist_id.clone(),
        name: w.playlist.name.clone(),
        what,
    })
}

/// Changes the name, comment or public flag of the caller's playlist `id`, under `names_key`
/// when renaming and `playlist_key`: the owner check, the name check, updatePlaylist, then the
/// read back, each changed part compared.
pub(crate) async fn update(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
    change: &MetaChange,
) -> Result<Written, PlaylistRefusal> {
    let name = change.name.as_deref().map(str::trim);
    let _names = match name {
        Some(_) => Some(lock(&names_key(ctx)).await),
        None => None,
    };
    let _playlist = lock(&playlist_key(id)).await;
    owned(navi, ctx, id).await?;
    if let Some(name) = name {
        name_free(navi, ctx, name, Some(id), change.allow_duplicate_name).await?;
    }
    let meta = PlaylistMeta {
        name,
        comment: change.comment.as_deref(),
        public: change.public,
    };
    wrote(navi.update_playlist_meta(id, &meta).await, id).map_err(PlaylistRefusal::WriteFailed)?;
    let (playlist, tracks) = read(navi, ctx, id)
        .await
        .map_err(PlaylistRefusal::WriteFailed)?;
    let w = written(&playlist, &tracks, ctx);
    shows(&w, &playlist, name, meta.comment, meta.public)?;
    Ok(w)
}

/// Deletes the caller's playlist `id` under `playlist_key`: the owner check, deletePlaylist,
/// then getPlaylist must find none. Returns the playlist as it was before.
pub(crate) async fn delete(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<PlaylistFacts, PlaylistRefusal> {
    let _playlist = lock(&playlist_key(id)).await;
    let before = owned(navi, ctx, id).await?.playlist;
    wrote(navi.delete_playlist(id).await, id).map_err(PlaylistRefusal::WriteFailed)?;
    match navi.get_playlist(id).await {
        Ok(None) => Ok(before),
        Ok(Some(_)) => Err(PlaylistRefusal::NotWritten {
            playlist_id: id.to_string(),
            name: before.name,
            what: NotWrittenPart::Delete,
        }),
        Err(e) => Err(PlaylistRefusal::WriteFailed(navidrome_failure(
            Stage::Playlist,
            &e,
        ))),
    }
}

/// How a track write reaches Navidrome. Append adds after the last row and keeps every row;
/// Remove deletes the rows at these positions and keeps the others, hidden ones included; Replace
/// rewrites the list, so the rows this account cannot see are dropped. Replace never takes an
/// empty list, which Navidrome ignores.
pub(crate) enum TrackWrite {
    Append(Vec<String>),
    Replace(Vec<String>),
    Remove(Vec<usize>),
}

/// The caller holds `playlist_key(id)`. Reads the caller's playlist as `owned` does; `plan`, on
/// its facts and native rows, gives the write and the song ids the playlist must list after it, or
/// a refusal. Then the native record: a smart or synced playlist is NotEditable (D4), offering
/// that list without its missing files; `present` on the songs the list gains; the write (none
/// for a Remove of nothing); and the read back, which must list exactly that list. Nothing is
/// written before `present`, so every refusal up to there leaves the playlist as it was.
async fn write_tracks(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
    plan: impl FnOnce(
        &PlaylistFacts,
        &[PlaylistTrack],
    ) -> Result<(TrackWrite, Vec<String>), PlaylistRefusal>,
) -> Result<Written, PlaylistRefusal> {
    let (playlist, tracks) = owned_read(navi, ctx, id).await?;
    let before = facts(&playlist, &ctx.username);
    let (write, after) = plan(&before, &tracks)?;
    let native = (navi.get_native_playlist(id).await.map_err(failed)?)
        .ok_or_else(|| PlaylistRefusal::Failed(playlist_not_found(id)))?;
    let why = if native.is_smart() {
        Some(NotEditable::Smart)
    } else if native.sync {
        Some(NotEditable::Synced)
    } else {
        None
    };
    if let Some(why) = why {
        let missing: HashSet<&str> = (tracks.iter())
            .filter(|t| t.song.missing)
            .map(|t| t.song.id.as_str())
            .collect();
        return Err(PlaylistRefusal::NotEditable {
            playlist_id: before.playlist_id,
            name: before.name,
            why,
            would_be: (after.iter())
                .filter(|id| !missing.contains(id.as_str()))
                .cloned()
                .collect(),
        });
    }
    present(navi, &surplus(&after, &ids_of(&tracks))).await?;
    let (answer, hidden_dropped) = match &write {
        TrackWrite::Append(ids) => (Some(navi.append_playlist_tracks(id, ids).await), 0),
        TrackWrite::Replace(ids) => (
            Some(navi.replace_playlist_tracks(id, ids).await),
            native.song_count.saturating_sub(tracks.len() as u32),
        ),
        TrackWrite::Remove(positions) if positions.is_empty() => (None, 0),
        TrackWrite::Remove(positions) => {
            (Some(navi.remove_playlist_tracks(id, positions).await), 0)
        }
    };
    let sent = answer.is_some();
    if let Some(answer) = answer {
        wrote(answer, id).map_err(PlaylistRefusal::WriteFailed)?;
    }
    let (playlist, tracks) = read(navi, ctx, id).await.map_err(|f| match sent {
        true => PlaylistRefusal::WriteFailed(f),
        false => PlaylistRefusal::Failed(f),
    })?;
    let w = Written {
        hidden_dropped,
        ..written(&playlist, &tracks, ctx)
    };
    tracks_written(&w, &after)?;
    Ok(w)
}

/// Under `playlist_key`: the caller's playlist `id` lists `song_ids`, in order (Replace, which
/// drops the entries Navidrome hides from the caller), or, for an empty list, none of the entries
/// it listed (Remove; the hidden ones stay, as Navidrome ignores a Replace with no songs).
pub(crate) async fn set_tracks(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
    song_ids: &[String],
) -> Result<Written, PlaylistRefusal> {
    let _playlist = lock(&playlist_key(id)).await;
    write_tracks(navi, ctx, id, |_, tracks| {
        Ok(if song_ids.is_empty() {
            let every = tracks.iter().map(position_of).collect();
            (TrackWrite::Remove(every), vec![])
        } else {
            (TrackWrite::Replace(song_ids.to_vec()), song_ids.to_vec())
        })
    })
    .await
}

/// Under `playlist_key`: `song_ids` appended to the caller's playlist `id` when `before` is None
/// or the position right after the last entry (0 for a playlist listing none), or inserted before
/// the entry at position `before` (Replace). Any other `before` is NoEntryAt: positions skip the
/// tracks Navidrome hides from the caller, so only a listed one names a place.
pub(crate) async fn add_tracks(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
    song_ids: &[String],
    before: Option<usize>,
) -> Result<Written, PlaylistRefusal> {
    let _playlist = lock(&playlist_key(id)).await;
    write_tracks(navi, ctx, id, |playlist, tracks| {
        let mut after = ids_of(tracks);
        let end = tracks.last().map_or(0, |t| position_of(t) + 1);
        let Some(before) = before.filter(|&before| before != end) else {
            after.extend_from_slice(song_ids);
            return Ok((TrackWrite::Append(song_ids.to_vec()), after));
        };
        let at = (tracks.iter())
            .position(|t| position_of(t) == before)
            .ok_or_else(|| PlaylistRefusal::NoEntryAt {
                playlist_id: playlist.playlist_id.clone(),
                position: before,
            })?;
        after.splice(at..at, song_ids.iter().cloned());
        Ok((TrackWrite::Replace(after.clone()), after))
    })
    .await
}

/// Under `playlist_key`: removes from the caller's playlist `id` the entries `entries` name, each
/// by its 0-based position and the song that must be there; Changed for the first that does not
/// hold it, nothing removed (D3). The positions are distinct. Entries Navidrome hides from the
/// caller stay.
pub(crate) async fn remove_tracks(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
    entries: &[(usize, String)],
) -> Result<Written, PlaylistRefusal> {
    let _playlist = lock(&playlist_key(id)).await;
    write_tracks(navi, ctx, id, |playlist, tracks| {
        let at: HashMap<usize, &String> = (tracks.iter())
            .map(|t| (position_of(t), &t.song.id))
            .collect();
        for (position, expected) in entries {
            let found = at.get(position).copied();
            if found != Some(expected) {
                return Err(PlaylistRefusal::Changed {
                    playlist_id: playlist.playlist_id.clone(),
                    name: playlist.name.clone(),
                    position: *position,
                    expected: expected.clone(),
                    found: found.cloned(),
                });
            }
        }
        let removed: HashSet<usize> = entries.iter().map(|(position, _)| *position).collect();
        let after = (tracks.iter())
            .filter(|t| !removed.contains(&position_of(t)))
            .map(|t| t.song.id.clone())
            .collect();
        let positions = entries.iter().map(|(position, _)| *position).collect();
        Ok((TrackWrite::Remove(positions), after))
    })
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::auth::Role;
    use crate::core::test_ctx;
    use crate::test_navidrome::{
        child, native_playlist, native_songs, playlist_body, playlist_row, playlist_rows,
        playlists_body, subsonic_failed, subsonic_ok, NavidromeStub, ADVENT, AWAY, DISCOVERY_RULES,
        HOLY, SILENT,
    };

    /// A playlist as getPlaylists and getPlaylist send it, `public` left out
    /// when false (responses.go:311-312).
    fn pl(id: &str, name: &str, owner: &str, public: bool) -> String {
        let mut p = json!({"id": id, "name": name, "songCount": 0, "duration": 0, "public": true,
            "owner": owner, "created": "2026-11-10T18:00:00Z", "changed": "2026-11-10T18:00:00Z"});
        if !public {
            p.as_object_mut().unwrap().remove("public");
        }
        p.to_string()
    }

    fn playlists(items: &[&str]) -> String {
        subsonic_ok(&format!(
            r#""playlists":{{"playlist":[{}]}}"#,
            items.join(",")
        ))
    }

    fn playlist_answer(p: &str, entries: &[String]) -> String {
        let mut p: Value = serde_json::from_str(p).unwrap();
        p["entry"] = serde_json::from_str(&format!("[{}]", entries.join(","))).unwrap();
        subsonic_ok(&format!(r#""playlist":{p}"#))
    }

    fn alice(role: Role) -> UserCtx {
        UserCtx {
            user_id: "u1".into(),
            username: "alice".into(),
            role,
        }
    }

    #[tokio::test]
    async fn an_admin_sees_no_private_playlist_of_another_user() {
        // Navidrome gives an admin every playlist, persistence/playlist_repository.go:84-93
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists(&[
                &pl("p1", "Christmas", "alice", false),
                &pl("p2", "Bob's mix", "bob", false),
                &pl("p3", "Advent", "bob", true),
            ])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(&pl("p2", "Bob's mix", "bob", false), &[])],
        );
        // server/subsonic/playlists.go:46
        navi.answer_when(
            "/rest/getPlaylist",
            "id=gone",
            200,
            &subsonic_failed(70, "playlist not found"),
        );
        let admin = alice(Role::Admin);
        let listed = list(&navi.client(), &admin).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|p| (p.playlist_id.as_str(), p.owned))
                .collect::<Vec<_>>(),
            vec![("p1", true), ("p3", false)]
        );
        let private = get(&navi.client(), &admin, "p2").await.unwrap_err();
        let unknown = get(&navi.client(), &admin, "gone").await.unwrap_err();
        assert_eq!(
            (private, unknown),
            (playlist_not_found("p2"), playlist_not_found("gone"))
        );
        assert!(
            navi.asked()
                .iter()
                .all(|p| !p.starts_with("/api/playlist/")),
            "{:?}",
            navi.asked()
        );
    }

    #[test]
    fn ownership_ignores_the_case_of_the_owner_name() {
        let p: SubsonicPlaylist =
            serde_json::from_str(&pl("p1", "Christmas", "Alice", false)).unwrap();
        assert!(facts(&p, "alice").owned);
        assert!(!facts(&p, "alicia").owned);
    }

    #[tokio::test]
    async fn a_playlist_navidrome_cannot_find_is_not_found() {
        let navi = NavidromeStub::start();
        // server/subsonic/playlists.go:46
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_failed(70, "playlist not found")],
        );
        let f = get(&navi.client(), &test_ctx("u1"), "gone")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::PlaylistNotFound,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                "Playlist gone not found"
            )
        );
        assert_eq!(f.cause, None);
    }

    #[tokio::test]
    async fn entries_keep_navidromes_order_as_positions() {
        let navi = NavidromeStub::start();
        // getPlaylist leaves out the missing s7 (core/playlists/playlists.go:97,
        // persistence/playlist_repository.go:153-166); the native list keeps it, and leaves
        // out place 3, a track of a library alice cannot see (tracksQuery, :378-400).
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[
                    child("s9", "Marrow Choir", "Silent Night"),
                    child("s1", "Marrow Choir", "O Holy Night"),
                ],
            )],
        );
        navi.answer(
            "/api/playlist/p1/tracks",
            &[&format!(
                "[{},{},{}]",
                playlist_row(1, "s9", "Marrow Choir", "Silent Night", false),
                playlist_row(2, "s1", "Marrow Choir", "O Holy Night", false),
                playlist_row(4, "s7", "Marrow Choir", "Coventry Carol", true)
            )],
        );
        let (p, entries) = get(&navi.client(), &alice(Role::User), "p1").await.unwrap();
        assert_eq!((p.owned, p.comment, p.public), (true, None, false));
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.position, e.song.song_id.as_str(), e.missing))
                .collect::<Vec<_>>(),
            vec![(0, "s9", false), (1, "s1", false), (3, "s7", true)]
        );
        assert_eq!(
            entries[0].song.path.as_deref(),
            Some("Marrow Choir/Winter Songs/03 - Silent Night.flac")
        );
    }

    #[tokio::test]
    async fn a_proxy_404_after_the_playlist_was_found_is_its_failure() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[],
            )],
        );
        // nginx's 404 page (src/http/ngx_http_special_response.c) from a proxy that forwards
        // /rest but not /api.
        let page = "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n";
        navi.answer_when("/api/playlist/p1/tracks", "", 404, page);
        let f = get(&navi.client(), &alice(Role::User), "p1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::NavidromeUnreachable,
                Stage::Playlist,
                "Get playlist tracks failed: HTTP 404"
            )
        );
        assert_eq!(
            f.cause,
            Some(shared::failure::Cause::new(
                shared::failure::Service::Navidrome,
                Some(404),
                page
            ))
        );
    }

    #[tokio::test]
    async fn a_playlist_gone_between_the_two_reads_is_not_found() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[],
            )],
        );
        // http.Error(w, "not found", 404), server/nativeapi/playlists.go:42-44
        navi.answer_when("/api/playlist/p1/tracks", "", 404, "not found\n");
        let f = get(&navi.client(), &alice(Role::User), "p1")
            .await
            .unwrap_err();
        assert_eq!(f, playlist_not_found("p1"));
    }

    /// The paths `navi` was asked, its sign-in left out.
    fn asked(navi: &NavidromeStub) -> Vec<String> {
        (navi.asked().into_iter())
            .filter(|p| p != "/auth/login")
            .collect()
    }

    fn tracks_of(id: &str) -> String {
        format!("/api/playlist/{id}/tracks")
    }

    fn new_playlist(name: &str, songs: &[&str]) -> NewPlaylist {
        NewPlaylist {
            name: name.into(),
            comment: None,
            public: None,
            song_ids: songs.iter().map(|s| s.to_string()).collect(),
            allow_duplicate_name: false,
        }
    }

    fn ids(songs: &[&str]) -> Vec<String> {
        songs.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn a_playlist_is_created_described_and_read_back() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/api/song",
            &[&native_songs(&[(HOLY.0, false), (SILENT.0, false)])],
        );
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY, SILENT],
            )],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                true,
                "Carols",
                &[HOLY, SILENT],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[
                (1, HOLY.0, HOLY.1, false),
                (2, SILENT.0, SILENT.1, false),
            ])],
        );
        let new = NewPlaylist {
            comment: Some("Carols".into()),
            public: Some(true),
            ..new_playlist("Advent", &[HOLY.0, SILENT.0])
        };
        let w = create(&navi.client(), &test_ctx("alice"), &new)
            .await
            .unwrap();
        assert_eq!(
            (
                w.playlist.comment.as_deref(),
                w.playlist.public,
                w.song_ids.clone()
            ),
            (Some("Carols"), true, ids(&[HOLY.0, SILENT.0]))
        );
        assert_eq!(
            asked(&navi),
            vec![
                "/rest/getPlaylists".to_string(),
                "/api/song".into(),
                "/rest/createPlaylist".into(),
                "/rest/updatePlaylist".into(),
                "/rest/getPlaylist".into(),
                tracks_of(ADVENT)
            ]
        );
    }

    #[tokio::test]
    async fn a_name_the_caller_already_has_is_refused_unless_duplicates_are_asked_for() {
        let navi = NavidromeStub::start();
        // Bob's "Advent" does not count; alice's counts whatever the case of her name in Navidrome.
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[
                ("p-bob", "Advent", "bob", true),
                ("p-old", "Advent", "Alice", false),
                ("p-other", "advent", "alice", false),
            ])],
        );
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(&tracks_of(ADVENT), &["[]"]);
        let refused = create(
            &navi.client(),
            &test_ctx("alice"),
            &new_playlist(" Advent ", &[]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            refused,
            PlaylistRefusal::NameTaken {
                name: "Advent".into(),
                existing: vec!["p-old".into()]
            }
        );
        assert_eq!(
            refused.failure().message,
            "You already have a playlist with this name"
        );
        assert!(!refused.after_write());
        assert!(!asked(&navi).contains(&"/rest/createPlaylist".to_string()));
        let again = NewPlaylist {
            allow_duplicate_name: true,
            ..new_playlist("Advent", &[])
        };
        let w = create(&navi.client(), &test_ctx("alice"), &again)
            .await
            .unwrap();
        assert_eq!(
            (w.playlist.playlist_id.as_str(), w.song_ids),
            (ADVENT, vec![])
        );
    }

    #[tokio::test]
    async fn songs_navidrome_does_not_list_or_lists_as_missing_stop_the_write() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/api/song",
            &[&native_songs(&[(HOLY.0, false), (SILENT.0, true)])],
        );
        let songs = new_playlist("Advent", &[HOLY.0, SILENT.0, AWAY.0, AWAY.0]);
        let r = create(&navi.client(), &test_ctx("alice"), &songs)
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::SongsNotPresent {
                unknown: vec![AWAY.0.into()],
                missing_file: vec![SILENT.0.into()]
            }
        );
        assert_eq!(
            (
                r.failure().code,
                r.failure().stage,
                r.failure().message.as_str()
            ),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                "Navidrome does not list 1 of these songs for you and has 1 with a missing file"
            )
        );
        assert!(!asked(&navi).contains(&"/rest/createPlaylist".to_string()));
    }

    #[tokio::test]
    async fn another_users_playlist_is_never_written() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[
                &playlist_body(ADVENT, "Advent", "bob", true, "", &[HOLY]),
                &playlist_body(ADVENT, "Advent", "bob", false, "", &[HOLY]),
            ],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let rename = MetaChange {
            name: Some("Mine now".into()),
            comment: None,
            public: None,
            allow_duplicate_name: false,
        };
        let ctx = test_ctx("alice");
        let r = update(&navi.client(), &ctx, ADVENT, &rename)
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::NotOwner {
                playlist_id: ADVENT.into()
            }
        );
        assert_eq!(
            (r.failure().code, r.failure().message.as_str()),
            (
                FailureCode::PlaylistNotOwner,
                "Only the owner of this playlist can change it"
            )
        );
        // bob's private playlist, which only an admin account sees, reads like a missing one (step 8 D10).
        assert_eq!(
            delete(&navi.client(), &ctx, ADVENT).await.unwrap_err(),
            PlaylistRefusal::Failed(playlist_not_found(ADVENT))
        );
        let asked = asked(&navi);
        assert!(
            !asked.iter().any(|p| p == "/rest/updatePlaylist"
                || p == "/rest/deletePlaylist"
                || p == "/rest/getPlaylists"),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn a_change_navidrome_does_not_show_is_not_written() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[(ADVENT, "Advent", "alice", false)])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let ctx = test_ctx("alice");
        let change = |name: Option<&str>, comment: Option<&str>, public: Option<bool>| MetaChange {
            name: name.map(str::to_string),
            comment: comment.map(str::to_string),
            public,
            allow_duplicate_name: false,
        };
        for (change, message) in [
            (
                change(Some("Advent 2026"), None, None),
                "Navidrome answered the change of this playlist's name but does not show it",
            ),
            (
                change(None, Some("Carols"), None),
                "Navidrome answered the change of this playlist's comment but does not show it",
            ),
            (
                change(None, None, Some(true)),
                "Navidrome answered the change of this playlist's public flag but does not show it",
            ),
        ] {
            let r = update(&navi.client(), &ctx, ADVENT, &change)
                .await
                .unwrap_err();
            assert_eq!(
                (r.failure().code, r.failure().message.as_str()),
                (FailureCode::PlaylistNotWritten, message)
            );
            assert!(r.after_write());
        }
        // Its own name is no other playlist's: renaming to it is a change Navidrome shows.
        let w = update(
            &navi.client(),
            &ctx,
            ADVENT,
            &change(Some("Advent"), Some(""), Some(false)),
        )
        .await
        .unwrap();
        assert_eq!(w.song_ids, ids(&[HOLY.0]));
    }

    #[tokio::test]
    async fn a_delete_is_read_back_and_navidromes_refusal_keeps_its_text() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[
                &playlist_body(ADVENT, "Advent", "alice", false, "", &[HOLY]),
                &subsonic_failed(70, "playlist not found"), // server/subsonic/playlists.go:46
            ],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        assert_eq!(
            delete(&navi.client(), &test_ctx("alice"), ADVENT)
                .await
                .unwrap()
                .name,
            "Advent"
        );
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        // newError(responses.ErrorAuthorizationFail), server/subsonic/playlists.go:84-86
        navi.answer(
            "/rest/deletePlaylist",
            &[&subsonic_failed(
                50,
                "User is not authorized for the given operation",
            )],
        );
        let r = delete(&navi.client(), &test_ctx("alice"), ADVENT)
            .await
            .unwrap_err();
        assert!(r.after_write());
        let PlaylistRefusal::WriteFailed(f) = r else {
            panic!("{r:?}")
        };
        assert_eq!(
            (f.code, f.stage),
            (FailureCode::NavidromeRefused, Stage::Playlist)
        );
        assert_eq!(
            f.cause.map(|c| (c.status, c.raw)),
            Some((
                Some(50),
                "User is not authorized for the given operation".into()
            ))
        );
        // Still listed after the delete answered ok.
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let r = delete(&navi.client(), &test_ctx("alice"), ADVENT)
            .await
            .unwrap_err();
        assert_eq!(
            r.failure().message,
            "Navidrome answered the delete of this playlist but still lists it"
        );
    }

    #[tokio::test]
    async fn a_comment_that_fails_after_the_create_says_the_playlist_exists() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        // mapToSubsonicError's "Internal Server Error: {err}", server/subsonic/api.go:290-291
        navi.answer(
            "/rest/updatePlaylist",
            &[&subsonic_failed(
                0,
                "Internal Server Error: database is locked",
            )],
        );
        let new = NewPlaylist {
            comment: Some("Carols".into()),
            ..new_playlist("Advent", &[])
        };
        let r = create(&navi.client(), &test_ctx("alice"), &new)
            .await
            .unwrap_err();
        assert!(r.after_write());
        let PlaylistRefusal::Unfinished {
            playlist_id,
            failure,
            described,
        } = r
        else {
            panic!("{r:?}")
        };
        assert_eq!(
            (playlist_id.as_str(), failure.code, described),
            (ADVENT, FailureCode::NavidromeRefused, false)
        );
        assert!(!asked(&navi).contains(&"/rest/getPlaylist".to_string()));
    }

    #[tokio::test]
    async fn songs_navidrome_did_not_write_are_named() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/api/song",
            &[&native_songs(&[
                (HOLY.0, false),
                (SILENT.0, false),
                (AWAY.0, false),
            ])],
        );
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        // AWAY dropped, HOLY written twice: both kept songs keep their order.
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[
                (1, HOLY.0, HOLY.1, false),
                (2, SILENT.0, SILENT.1, false),
                (3, HOLY.0, HOLY.1, false),
            ])],
        );
        let new = new_playlist("Advent", &[HOLY.0, SILENT.0, AWAY.0]);
        let r = create(&navi.client(), &test_ctx("alice"), &new)
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::TracksNotWritten {
                playlist_id: ADVENT.into(),
                name: "Advent".into(),
                missing: ids(&[AWAY.0]),
                unexpected: ids(&[HOLY.0]),
                order_kept: true
            }
        );
        assert_eq!(
            r.failure().message,
            "Navidrome does not list this playlist as written: 1 song(s) missing, 1 not asked for, order kept"
        );
        let swapped = Written {
            playlist: facts(
                &serde_json::from_str(&pl(ADVENT, "Advent", "alice", false)).unwrap(),
                "alice",
            ),
            song_ids: ids(&[SILENT.0, HOLY.0]),
            hidden_dropped: 0,
        };
        let Err(PlaylistRefusal::TracksNotWritten {
            order_kept,
            missing,
            ..
        }) = tracks_written(&swapped, &ids(&[HOLY.0, SILENT.0]))
        else {
            panic!()
        };
        assert_eq!((order_kept, missing), (false, vec![]));
    }

    /// The paths `navi` was asked, its sign-in and native reads left out.
    fn rest(navi: &NavidromeStub) -> Vec<String> {
        (navi.asked().into_iter())
            .filter(|p| p.starts_with("/rest/"))
            .collect()
    }

    fn native_of(id: &str) -> String {
        format!("/api/playlist/{id}")
    }

    #[tokio::test]
    async fn setting_the_tracks_replaces_them_and_an_empty_list_removes_every_entry() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        // The second set reads what the first wrote, removes the three entries and reads it back
        // empty; the third finds no entry and writes nothing.
        let three = playlist_rows(&[
            (1, SILENT.0, SILENT.1, false),
            (2, HOLY.0, HOLY.1, false),
            (3, AWAY.0, AWAY.1, false),
        ]);
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false), (2, SILENT.0, SILENT.1, false)]),
                &three,
                &three,
                "[]",
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[
                &native_playlist(ADVENT, "Advent", 2, false, None),
                &native_playlist(ADVENT, "Advent", 3, false, None),
                &native_playlist(ADVENT, "Advent", 0, false, None),
            ],
        );
        // Only the song the playlist does not hold yet is looked up.
        navi.answer("/api/song", &[&native_songs(&[(AWAY.0, false)])]);
        let ctx = test_ctx("alice");
        let w = set_tracks(
            &navi.client(),
            &ctx,
            ADVENT,
            &ids(&[SILENT.0, HOLY.0, AWAY.0]),
        )
        .await
        .unwrap();
        assert_eq!(
            (w.song_ids, w.hidden_dropped),
            (ids(&[SILENT.0, HOLY.0, AWAY.0]), 0)
        );
        let w = set_tracks(&navi.client(), &ctx, ADVENT, &[]).await.unwrap();
        assert!(w.song_ids.is_empty());
        let w = set_tracks(&navi.client(), &ctx, ADVENT, &[]).await.unwrap();
        assert!(w.song_ids.is_empty());
        let (read, native) = ("/rest/getPlaylist".to_string(), native_of(ADVENT));
        assert_eq!(
            asked(&navi),
            vec![
                read.clone(),
                tracks_of(ADVENT),
                native.clone(),
                "/api/song".into(),
                "/rest/createPlaylist".into(),
                read.clone(),
                tracks_of(ADVENT),
                read.clone(),
                tracks_of(ADVENT),
                native.clone(),
                "/rest/updatePlaylist".into(),
                read.clone(),
                tracks_of(ADVENT),
                read.clone(),
                tracks_of(ADVENT),
                native,
                read,
                tracks_of(ADVENT),
            ]
        );
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!(
                "playlistId={ADVENT}&songIndexToRemove=0&songIndexToRemove=1&songIndexToRemove=2"
            )]
        );
    }

    #[tokio::test]
    async fn smart_and_synced_playlists_refuse_track_edits_but_take_a_new_name() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[
                &playlist_body(ADVENT, "Fresh Picks", "alice", false, "", &[]),
                &playlist_body(ADVENT, "Fresh Picks", "alice", false, "", &[]),
                &playlist_body(ADVENT, "Fresh Picks", "alice", false, "", &[]),
                &playlist_body(ADVENT, "Fresh Picks 2", "alice", false, "", &[]),
            ],
        );
        // AWAY's file is gone: a static copy could not take it.
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[
                (1, HOLY.0, HOLY.1, false),
                (2, AWAY.0, AWAY.1, true),
            ])],
        );
        navi.answer(
            &native_of(ADVENT),
            &[
                &native_playlist(ADVENT, "Fresh Picks", 2, false, Some(DISCOVERY_RULES)),
                &native_playlist(ADVENT, "Fresh Picks", 2, true, None),
            ],
        );
        let ctx = test_ctx("alice");
        let r = add_tracks(&navi.client(), &ctx, ADVENT, &ids(&[SILENT.0]), None)
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::NotEditable {
                playlist_id: ADVENT.into(),
                name: "Fresh Picks".into(),
                why: NotEditable::Smart,
                would_be: ids(&[HOLY.0, SILENT.0])
            }
        );
        let f = r.failure();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::PlaylistNotEditable,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                "This is a smart playlist: Navidrome fills its tracks from its rules"
            )
        );
        assert!(!r.after_write());
        let r = set_tracks(&navi.client(), &ctx, ADVENT, &ids(&[SILENT.0]))
            .await
            .unwrap_err();
        assert!(
            matches!(r, PlaylistRefusal::NotEditable { why: NotEditable::Synced, ref would_be, .. } if *would_be == ids(&[SILENT.0])),
            "{r:?}"
        );
        assert_eq!(
            r.failure().message,
            "This playlist is synced from a playlist file: Navidrome replaces its tracks from that file when the file changes"
        );
        let rename = MetaChange {
            name: Some("Fresh Picks 2".into()),
            comment: None,
            public: None,
            allow_duplicate_name: true,
        };
        assert_eq!(
            update(&navi.client(), &ctx, ADVENT, &rename)
                .await
                .unwrap()
                .playlist
                .name,
            "Fresh Picks 2"
        );
        let asked = asked(&navi);
        assert!(
            !asked
                .iter()
                .any(|p| p == "/rest/createPlaylist" || p == "/api/song"),
            "{asked:?}"
        );
        assert_eq!(
            asked
                .iter()
                .filter(|p| *p == "/rest/updatePlaylist")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn songs_are_inserted_before_a_position_or_appended() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        // Place 2 is a track of a library alice cannot see: positions 0 and 2, then 3.
        let appended = playlist_rows(&[
            (1, HOLY.0, HOLY.1, false),
            (3, SILENT.0, SILENT.1, false),
            (4, AWAY.0, AWAY.1, false),
        ]);
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false), (3, SILENT.0, SILENT.1, false)]),
                &appended,
                &appended,
                &appended,
                &appended,
                &playlist_rows(&[
                    (1, HOLY.0, HOLY.1, false),
                    (2, AWAY.0, AWAY.1, false),
                    (3, SILENT.0, SILENT.1, false),
                    (4, AWAY.0, AWAY.1, false),
                ]),
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[
                &native_playlist(ADVENT, "Advent", 3, false, None),
                &native_playlist(ADVENT, "Advent", 4, false, None),
            ],
        );
        navi.answer("/api/song", &[&native_songs(&[(AWAY.0, false)])]);
        let ctx = test_ctx("alice");
        let add = |before: Option<usize>| {
            let (navi, ctx) = (navi.client(), ctx.clone());
            async move { add_tracks(&navi, &ctx, ADVENT, &ids(&[AWAY.0]), before).await }
        };
        // Right after the last entry, as after no position: appended, the hidden track kept.
        let w = add(Some(3)).await.unwrap();
        assert_eq!(
            (w.song_ids, w.hidden_dropped),
            (ids(&[HOLY.0, SILENT.0, AWAY.0]), 0)
        );
        for gap in [1, 9] {
            let r = add(Some(gap)).await.unwrap_err();
            assert_eq!(
                r,
                PlaylistRefusal::NoEntryAt {
                    playlist_id: ADVENT.into(),
                    position: gap
                }
            );
            let f = r.failure();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message),
                (
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!("This playlist has no entry at position {gap}: use a position get_playlist lists, or leave position out to add at the end")
                )
            );
        }
        // Before SILENT, at position 2: the list is rewritten and the hidden track dropped.
        let w = add(Some(2)).await.unwrap();
        assert_eq!(
            (w.song_ids, w.hidden_dropped),
            (ids(&[HOLY.0, AWAY.0, SILENT.0, AWAY.0]), 1)
        );
        assert_eq!(
            rest(&navi),
            vec![
                "/rest/getPlaylist",
                "/rest/updatePlaylist",
                "/rest/getPlaylist",
                "/rest/getPlaylist",
                "/rest/getPlaylist",
                "/rest/getPlaylist",
                "/rest/createPlaylist",
                "/rest/getPlaylist"
            ]
        );
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!("playlistId={ADVENT}&songIdToAdd={}", AWAY.0)]
        );
        assert_eq!(
            navi.posted("/rest/createPlaylist"),
            vec![format!(
                "playlistId={ADVENT}&songId={}&songId={}&songId={}&songId={}",
                HOLY.0, AWAY.0, SILENT.0, AWAY.0
            )]
        );
    }

    #[tokio::test]
    async fn a_position_that_moved_is_refused_and_nothing_is_removed() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[
                (1, SILENT.0, SILENT.1, false),
                (2, HOLY.0, HOLY.1, false),
            ])],
        );
        let ctx = test_ctx("alice");
        let r = remove_tracks(&navi.client(), &ctx, ADVENT, &[(0, HOLY.0.into())])
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::Changed {
                playlist_id: ADVENT.into(),
                name: "Advent".into(),
                position: 0,
                expected: HOLY.0.into(),
                found: Some(SILENT.0.into())
            }
        );
        let f = r.failure();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::PlaylistChanged,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                "Position 0 of this playlist no longer holds the song given: the playlist changed since it was read"
            )
        );
        assert!(!r.after_write());
        // The first entry is right, the second past the end.
        let r = remove_tracks(
            &navi.client(),
            &ctx,
            ADVENT,
            &[(0, SILENT.0.into()), (2, HOLY.0.into())],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                r,
                PlaylistRefusal::Changed {
                    position: 2,
                    found: None,
                    ..
                }
            ),
            "{r:?}"
        );
        let asked = asked(&navi);
        assert!(
            !asked
                .iter()
                .any(|p| p == "/rest/updatePlaylist" || *p == native_of(ADVENT)),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn a_rewrite_says_how_many_hidden_entries_navidrome_dropped() {
        // The native songCount counts every row of the playlist, tracks of libraries the account
        // cannot see included (refreshCounters, persistence/playlist_repository.go:333-371); the
        // native rows are the ones it sees. createPlaylist with playlistId deletes every row
        // (playlist_repository.go:272-331).
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false), (3, SILENT.0, SILENT.1, false)]),
                &playlist_rows(&[(1, SILENT.0, SILENT.1, false), (2, HOLY.0, HOLY.1, false)]),
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[&native_playlist(ADVENT, "Advent", 3, false, None)],
        );
        let w = set_tracks(
            &navi.client(),
            &test_ctx("alice"),
            ADVENT,
            &ids(&[SILENT.0, HOLY.0]),
        )
        .await
        .unwrap();
        assert_eq!(
            (w.song_ids, w.hidden_dropped),
            (ids(&[SILENT.0, HOLY.0]), 1)
        );
        // A removal deletes only the rows it names (persistence/playlist_track_repository.go:262-270).
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false), (3, SILENT.0, SILENT.1, false)]),
                &playlist_rows(&[(2, SILENT.0, SILENT.1, false)]),
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[&native_playlist(ADVENT, "Advent", 3, false, None)],
        );
        let w = remove_tracks(
            &navi.client(),
            &test_ctx("alice"),
            ADVENT,
            &[(0, HOLY.0.into())],
        )
        .await
        .unwrap();
        assert_eq!((w.song_ids, w.hidden_dropped), (ids(&[SILENT.0]), 0));
        // The position as given: songIndexToRemove 0 deletes row 1 (core/playlists/playlists.go:186-197).
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!("playlistId={ADVENT}&songIndexToRemove=0")]
        );
    }

    #[tokio::test]
    async fn a_track_write_navidrome_did_not_keep_is_reported_with_the_differences() {
        // A client changed the playlist between the write and the read back.
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false)]),
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false), (2, SILENT.0, SILENT.1, false)]),
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[&native_playlist(ADVENT, "Advent", 1, false, None)],
        );
        navi.answer(
            "/api/song",
            &[&native_songs(&[(SILENT.0, false), (AWAY.0, false)])],
        );
        let r = set_tracks(
            &navi.client(),
            &test_ctx("alice"),
            ADVENT,
            &ids(&[SILENT.0, AWAY.0]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::TracksNotWritten {
                playlist_id: ADVENT.into(),
                name: "Advent".into(),
                missing: ids(&[AWAY.0]),
                unexpected: ids(&[HOLY.0]),
                order_kept: true
            }
        );
        assert_eq!(
            r.failure().message,
            "Navidrome does not list this playlist as written: 1 song(s) missing, 1 not asked for, order kept"
        );
        assert!(r.after_write());
    }

    #[tokio::test]
    async fn two_adds_to_one_playlist_run_one_after_the_other() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        let two = playlist_rows(&[(1, HOLY.0, HOLY.1, false), (2, SILENT.0, SILENT.1, false)]);
        navi.answer(
            &tracks_of(ADVENT),
            &[
                &playlist_rows(&[(1, HOLY.0, HOLY.1, false)]),
                &two,
                &two,
                &playlist_rows(&[
                    (1, HOLY.0, HOLY.1, false),
                    (2, SILENT.0, SILENT.1, false),
                    (3, AWAY.0, AWAY.1, false),
                ]),
            ],
        );
        navi.answer(
            &native_of(ADVENT),
            &[&native_playlist(ADVENT, "Advent", 1, false, None)],
        );
        navi.answer(
            "/api/song",
            &[
                &native_songs(&[(SILENT.0, false)]),
                &native_songs(&[(AWAY.0, false)]),
            ],
        );
        let (client, ctx) = (navi.client(), test_ctx("alice"));
        let (silent, away) = (ids(&[SILENT.0]), ids(&[AWAY.0]));
        let (a, b) = tokio::join!(
            add_tracks(&client, &ctx, ADVENT, &silent, None),
            add_tracks(&client, &ctx, ADVENT, &away, None)
        );
        assert_eq!(
            (a.unwrap().song_ids.len(), b.unwrap().song_ids.len()),
            (2, 3)
        );
        let per_add = vec![
            "/rest/getPlaylist".to_string(),
            tracks_of(ADVENT),
            native_of(ADVENT),
            "/api/song".into(),
            "/rest/updatePlaylist".into(),
            "/rest/getPlaylist".into(),
            tracks_of(ADVENT),
        ];
        assert_eq!(asked(&navi), [per_add.clone(), per_add].concat());
    }

    #[tokio::test]
    async fn a_lock_entry_goes_once_nobody_holds_it() {
        let guard = lock(&playlist_key("p-lock-test")).await;
        assert!(lock_entries().contains(&"playlist:p-lock-test".to_string()));
        drop(guard);
        assert!(!lock_entries().contains(&"playlist:p-lock-test".to_string()));
    }

    #[tokio::test]
    async fn a_second_holder_of_a_lock_waits_for_the_first() {
        let first = lock(&playlist_key("p-wait-test")).await;
        let second = tokio::spawn(async { drop(lock(&playlist_key("p-wait-test")).await) });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        second.await.unwrap();
    }
}
