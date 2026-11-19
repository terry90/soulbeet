//! Navidrome as the guard sees it, and a temp library to check files in.

use super::{unlink, Checked, Guard, Listeners, Origin, PreviewedFile, Verdict};
use crate::auth::UserId;
use crate::connection::test_util::{CLAUDE_CALLBACK, RESOURCE};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::services::NavidromeLookupError;
use shared::download::DownloadQuery;
use shared::metadata::Track;
use shared::navidrome::DiscoveryTrack;
use soulbeet::navidrome::models::{NativeSong, SubsonicAlbum, SubsonicSong};
use soulbeet::navidrome::NativePlaylist;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Pair = (String, String);

/// An error a read fails with.
type Fails = fn() -> NavidromeLookupError;

/// What ping answers: the version it reports, or the error it fails with.
type PingAnswer = Result<Option<String>, fn() -> NavidromeLookupError>;

/// A playlist: owner, name (its id too), song ids, public, smart.
type Playlist = (String, String, Vec<String>, bool, bool);

/// The rules of a smart playlist, as Navidrome writes a "Recently added" one.
const SMART_RULES: &str = r#"{"all":[{"inTheLast":{"dateAdded":30}}]}"#;

/// The server version the fake reports unless told otherwise: the maintainer's.
const VERSION: &str = "0.60.3 (34c6f12a)";

#[derive(Default)]
struct State {
    /// Each song's record (rating and average unset) and album id.
    records: BTreeMap<String, (NativeSong, String)>,
    /// (user, song id)
    sees: HashSet<Pair>,
    ratings: HashMap<Pair, u8>,
    stars: HashSet<Pair>,
    /// (user, album id)
    album_ratings: HashMap<Pair, u8>,
    album_stars: HashSet<Pair>,
    hidden: HashMap<String, Vec<u8>>,
    /// The average stored per song, as the last setRating left it.
    averages: HashMap<String, f64>,
    unreadable: HashMap<String, fn() -> NavidromeLookupError>,
    /// song_playlists as a user fails with the error; their other reads answer.
    playlists_unreadable: HashMap<String, Fails>,
    playlists: Vec<Playlist>,
    minimal: HashSet<String>,
    legacy: HashSet<String>,
    lost_albums: HashSet<String>,
    unsearchable: HashSet<String>,
    without_averages: bool,
    /// None: VERSION.
    version: Option<PingAnswer>,
    reads: HashMap<String, usize>,
    ratings_set: Vec<(String, String, u8)>,
    /// Navidrome's users, by name.
    navidrome_users: Vec<String>,
    /// Who may list them.
    admins: HashSet<String>,
    /// Listing them fails with this error.
    users_unlistable: Option<fn() -> NavidromeLookupError>,
    /// How many times they were listed, or listing was asked for.
    user_lists: usize,
}

impl State {
    /// Navidrome's average: every positive rating of the song, seen or not
    /// (persistence/sql_annotations.go:150-158), rounded to 2 decimals.
    fn recompute(&mut self, song: &str) {
        let ratings: Vec<f64> = self
            .ratings
            .iter()
            .filter(|((_, id), r)| id == song && **r > 0)
            .map(|(_, r)| f64::from(*r))
            .chain(
                self.hidden
                    .get(song)
                    .into_iter()
                    .flatten()
                    .map(|r| f64::from(*r)),
            )
            .collect();
        let mean = match ratings.len() {
            0 => 0.0,
            n => ratings.iter().sum::<f64>() / n as f64,
        };
        self.averages
            .insert(song.to_string(), (mean * 100.0).round() / 100.0);
    }

    /// Counts a read as `user` and fails it when `user` is unreadable.
    fn read(&mut self, user: &UserId) -> Result<String, NavidromeLookupError> {
        let user = user.to_string();
        *self.reads.entry(user.clone()).or_default() += 1;
        match self.unreadable.get(&user) {
            Some(error) => Err(error()),
            None => Ok(user),
        }
    }

    fn native(&self, user: &str, id: &str) -> Option<NativeSong> {
        let (record, _) = self.records.get(id)?;
        self.sees
            .contains(&(user.to_string(), id.to_string()))
            .then(|| NativeSong {
                rating: self.rating(user, id),
                // omitempty: an average of 0 is not sent.
                average_rating: self
                    .averages
                    .get(id)
                    .copied()
                    .filter(|a| *a > 0.0 && !self.without_averages),
                ..record.clone()
            })
    }

    fn rating(&self, user: &str, id: &str) -> u8 {
        self.ratings
            .get(&(user.to_string(), id.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

/// Navidrome as the guard sees it: each user's view of the songs, their ratings and stars,
/// an average stored per song that only set_rating recomputes (sql_annotations.go:141-158).
#[derive(Default)]
pub(crate) struct FakeNavidrome {
    state: Mutex<State>,
}

impl FakeNavidrome {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// The record's fields other than rating and average.
    pub fn file(&self, song: NativeSong, album_id: &str) {
        self.with(|s| {
            let id = song.id.clone();
            s.records.insert(id.clone(), (song, album_id.to_string()));
            s.recompute(&id);
        })
    }

    pub fn sees(&self, user: &str, song_id: &str) {
        self.with(|s| s.sees.insert((user.into(), song_id.into())));
    }

    /// `user`'s reads answer None for `song_id`, as getSong answers code 70.
    pub fn unsee(&self, user: &str, song_id: &str) {
        self.with(|s| s.sees.remove(&(user.into(), song_id.into())));
    }

    pub fn rates(&self, user: &str, song_id: &str, rating: u8) {
        self.with(|s| {
            s.ratings.insert((user.into(), song_id.into()), rating);
            s.recompute(song_id);
        })
    }

    pub fn stars(&self, user: &str, song_id: &str) {
        self.with(|s| s.stars.insert((user.into(), song_id.into())));
    }

    pub fn rates_album(&self, user: &str, album_id: &str, rating: u8) {
        self.with(|s| {
            s.album_ratings
                .insert((user.into(), album_id.into()), rating)
        });
    }

    pub fn stars_album(&self, user: &str, album_id: &str) {
        self.with(|s| s.album_stars.insert((user.into(), album_id.into())));
    }

    /// A rater Soulful has no account for; counts in the average only.
    pub fn hidden_rater(&self, song_id: &str, rating: u8) {
        self.with(|s| {
            s.hidden.entry(song_id.into()).or_default().push(rating);
            s.recompute(song_id);
        })
    }

    /// The stored average, as left by an earlier setRating (stale until the next one).
    pub fn stale_average(&self, song_id: &str, average: f64) {
        self.with(|s| s.averages.insert(song_id.into(), average));
    }

    /// Every read as `user` fails with this error.
    pub fn unreadable(&self, user: &str, error: fn() -> NavidromeLookupError) {
        self.with(|s| s.unreadable.insert(user.into(), error));
    }

    /// Reading the playlists that hold a song as `user` fails with `error`; their other reads
    /// answer.
    pub fn playlists_unreadable(&self, user: &str, error: Fails) {
        self.with(|s| s.playlists_unreadable.insert(user.into(), error));
    }

    /// `owner`'s playlist `name` (also its id) holding `songs`, public or not, smart (with
    /// SMART_RULES) or static.
    pub fn playlist(&self, owner: &str, name: &str, songs: &[&str], public: bool, smart: bool) {
        self.with(|s| {
            s.playlists.push((
                owner.into(),
                name.into(),
                songs.iter().map(|id| id.to_string()).collect(),
                public,
                smart,
            ))
        });
    }

    /// Undoes `unreadable`.
    pub fn readable(&self, user: &str) {
        self.with(|s| s.unreadable.remove(user));
    }

    /// Answers `user` as a minimal client: songs without albumId.
    pub fn minimal_client(&self, user: &str) {
        self.with(|s| s.minimal.insert(user.into()));
    }

    /// Answers `user` as a legacy client: albums without userRating.
    pub fn legacy_client(&self, user: &str) {
        self.with(|s| s.legacy.insert(user.into()));
    }

    /// getAlbum answers code 70 for `album_id`, for everyone.
    pub fn lose_album(&self, album_id: &str) {
        self.with(|s| s.lost_albums.insert(album_id.into()));
    }

    /// No search finds `song_id`.
    pub fn unsearchable(&self, song_id: &str) {
        self.with(|s| s.unsearchable.insert(song_id.into()));
    }

    /// Navidrome never sends an average.
    pub fn without_averages(&self) {
        self.with(|s| s.without_averages = true);
    }

    /// ping reports `version` (None: no serverVersion).
    pub fn reports_version(&self, version: Option<&str>) {
        self.with(|s| s.version = Some(Ok(version.map(String::from))));
    }

    /// ping fails with this error.
    pub fn version_unreadable(&self, error: fn() -> NavidromeLookupError) {
        self.with(|s| s.version = Some(Err(error)));
    }

    /// Navidrome's users are `names`.
    pub fn users_are(&self, names: &[&str]) {
        self.with(|s| s.navidrome_users = names.iter().map(|n| n.to_string()).collect());
    }

    /// `user` is a Navidrome admin, who may list Navidrome's users.
    pub fn admin(&self, user: &str) {
        self.with(|s| s.admins.insert(user.into()));
    }

    /// Listing Navidrome's users fails with this error, for an admin too.
    pub fn users_unlistable(&self, error: fn() -> NavidromeLookupError) {
        self.with(|s| s.users_unlistable = Some(error));
    }

    /// How many times Navidrome's users were asked for.
    pub fn user_lists(&self) -> usize {
        self.with(|s| s.user_lists)
    }

    pub fn reads_by(&self, user: &str) -> usize {
        self.with(|s| s.reads.get(user).copied().unwrap_or(0))
    }

    /// Every setRating, as (user, song id, rating).
    pub fn ratings_set(&self) -> Vec<(String, String, u8)> {
        self.with(|s| s.ratings_set.clone())
    }
}

#[async_trait::async_trait]
impl Listeners for FakeNavidrome {
    async fn native_song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<NativeSong>, NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            Ok(s.native(&user, id))
        })
    }

    async fn song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicSong>, NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            let Some(song) = s.native(&user, id) else {
                return Ok(None);
            };
            let album_id = s.records[id].1.clone();
            let pair = (user.clone(), id.to_string());
            Ok(Some(SubsonicSong {
                id: song.id,
                title: song.title,
                artist: Some(song.artist),
                album: Some(song.album),
                album_id: (!s.minimal.contains(&user)).then_some(album_id),
                genre: None,
                duration: None,
                user_rating: (song.rating > 0).then_some(song.rating),
                play_count: None,
                starred: s
                    .stars
                    .contains(&pair)
                    .then(|| "2026-11-14T20:11:05Z".to_string()),
                music_brainz_id: song.mbz_recording_id,
                suffix: None,
                bit_rate: None,
                size: Some(song.size),
                year: None,
                track: None,
            }))
        })
    }

    async fn album(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicAlbum>, NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            let known = s.records.values().any(|(_, album)| album == id);
            if !known || s.lost_albums.contains(id) {
                return Ok(None);
            }
            let pair = (user.clone(), id.to_string());
            Ok(Some(SubsonicAlbum {
                id: id.to_string(),
                name: Some("Winter Songs".into()),
                title: None,
                artist: Some("Marrow Choir".into()),
                song_count: None,
                duration: None,
                genre: None,
                user_rating: (!s.legacy.contains(&user))
                    .then(|| s.album_ratings.get(&pair).copied().unwrap_or(0)),
                starred: s
                    .album_stars
                    .contains(&pair)
                    .then(|| "2026-11-14T20:11:05Z".to_string()),
                year: None,
                play_count: None,
                song: Vec::new(),
            }))
        })
    }

    async fn set_rating(
        &self,
        user: &UserId,
        id: &str,
        rating: u8,
    ) -> Result<(), NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            s.ratings_set.push((user.clone(), id.to_string(), rating));
            s.ratings.insert((user, id.to_string()), rating);
            s.recompute(id);
            Ok(())
        })
    }

    /// The playlists holding `id` that are public or `user`'s, every one for an admin, as
    /// GetPlaylists picks them (persistence/playlist_repository.go:84-93,238-256). Whether `user`
    /// sees the song does not matter. Two things differ from Navidrome: ownership compares names
    /// without ASCII case where Navidrome compares owner_id (the same answer, since Navidrome
    /// refuses names that differ only in case, persistence/user_repository.go:342), and the list
    /// is sorted by byte order where Navidrome sorts naturally by name, so no test relies on the
    /// order.
    async fn song_playlists(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Vec<NativePlaylist>, NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            if let Some(error) = s.playlists_unreadable.get(&user) {
                return Err(error());
            }
            let admin = s.admins.contains(&user);
            let mut lists: Vec<NativePlaylist> = s
                .playlists
                .iter()
                .filter(|(owner, _, songs, public, _)| {
                    songs.iter().any(|song| song == id)
                        && (admin || *public || owner.eq_ignore_ascii_case(&user))
                })
                .map(|(owner, name, songs, _, smart)| NativePlaylist {
                    id: name.clone(),
                    name: name.clone(),
                    owner_name: owner.clone(),
                    sync: false,
                    song_count: songs.len() as u32,
                    rules: smart.then(|| serde_json::from_str(SMART_RULES).unwrap()),
                })
                .collect();
            lists.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(lists)
        })
    }

    /// Every record `user` sees that a search can find: a text search can return other files.
    async fn songs_for_item(
        &self,
        user: &UserId,
        _: &ImportedItem,
    ) -> Result<Option<Vec<NativeSong>>, NavidromeLookupError> {
        self.with(|s| {
            let user = s.read(user)?;
            Ok(Some(
                s.records
                    .keys()
                    .filter(|id| !s.unsearchable.contains(*id))
                    .filter_map(|id| s.native(&user, id))
                    .collect(),
            ))
        })
    }

    async fn server_version(&self, user: &UserId) -> Result<Option<String>, NavidromeLookupError> {
        self.with(|s| {
            s.read(user)?;
            match &s.version {
                None => Ok(Some(VERSION.to_string())),
                Some(Ok(version)) => Ok(version.clone()),
                Some(Err(error)) => Err(error()),
            }
        })
    }

    async fn navidrome_users(
        &self,
        user: &UserId,
    ) -> Result<Option<Vec<String>>, NavidromeLookupError> {
        self.with(|s| {
            s.user_lists += 1;
            let user = s.read(user)?;
            if let Some(error) = s.users_unlistable {
                return Err(error());
            }
            Ok(s.admins.contains(&user).then(|| s.navidrome_users.clone()))
        })
    }
}

/// A temp library (every file modified at mtime()): users alice and bob linked to Navidrome (navidrome_token 'cipher'), carol not;
/// Navidrome's users are alice and bob, and neither is an admin;
/// folders alice-music at `{root}/music/alice`, bob-music at `{root}/music/bob`, stored as the
/// tempdir gave the paths; `{root}/music/alice/A/x.flac` (5 bytes) is song "s1" (album "al1";
/// artist and album artist "Marrow Choir", album "Winter Songs", title "O Holy Night", no
/// recording MBID) in library 1 at `{root}/music`, seen by alice and bob, rated 1 by alice.
pub(crate) struct Library {
    /// Holds the temp directory until the library is dropped.
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub db: SqlitePool,
    pub fake: Arc<FakeNavidrome>,
}

impl Library {
    pub async fn new() -> Library {
        Library::new_on(crate::db::test_db::migrated().await).await
    }

    /// As new(), on `db` (a test_db::migrated_file for tests of writers running at once).
    pub async fn new_on(db: SqlitePool) -> Library {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for (user, token) in [
            ("alice", Some("cipher")),
            ("bob", Some("cipher")),
            ("carol", None),
        ] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, navidrome_token) VALUES (?, ?, 'x', ?)",
            )
            .bind(user)
            .bind(user)
            .bind(token)
            .execute(&db)
            .await
            .unwrap();
        }
        let lib = Library {
            _dir: dir,
            root,
            db,
            fake: Arc::new(FakeNavidrome::default()),
        };
        lib.folder("alice", "alice-music", "music/alice", false)
            .await;
        lib.folder("bob", "bob-music", "music/bob", false).await;
        std::fs::create_dir_all(lib.root.join("music/bob")).unwrap();
        lib.fake.file(
            NativeSong {
                id: "s1".into(),
                path: "alice/A/x.flac".into(),
                title: "O Holy Night".into(),
                size: 5,
                library_id: 1,
                library_path: lib.root.join("music").to_string_lossy().into_owned(),
                missing: false,
                updated_at: mtime(),
                artist: "Marrow Choir".into(),
                album: "Winter Songs".into(),
                album_artist: "Marrow Choir".into(),
                rating: 0,
                average_rating: None,
                mbz_recording_id: None,
                album_id: "al1".into(),
                track_number: 0,
                year: 0,
                duration: 0.0,
                suffix: "flac".into(),
                bit_rate: 0,
                starred: false,
                starred_at: None,
                rated_at: None,
                play_count: 0,
                play_date: None,
                mbz_artist_id: None,
                mbz_album_artist_id: None,
            },
            "al1",
        );
        lib.write("alice/A/x.flac", b"audio");
        lib.fake.sees("alice", "s1");
        lib.fake.sees("bob", "s1");
        lib.fake.rates("alice", "s1", 1);
        lib.fake.users_are(&["alice", "bob"]);
        lib
    }

    pub fn guard(&self) -> Guard<'_> {
        Guard::new(&self.db, self.fake.as_ref(), None)
    }

    pub fn alice(&self) -> UserId {
        UserId::new("alice")
    }

    /// Inserts the registered client `dcr_c` (redirect URI CLAUDE_CALLBACK, so its app host is
    /// claude.ai) when absent, and grant `grant_id` of `user` on it with `allow_delete`; returns
    /// the grant as an Origin::Mcp.
    pub async fn app(&self, grant_id: &str, user: &str, allow_delete: bool) -> Origin {
        sqlx::query(
            "INSERT INTO oauth_clients (client_id, kind, client_name, redirect_uris, created_at) \
             VALUES ('dcr_c', 'dcr', 'Claude', ?, 0) ON CONFLICT (client_id) DO NOTHING",
        )
        .bind(format!("[\"{CLAUDE_CALLBACK}\"]"))
        .execute(&self.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_grants (id, user_id, client_id, redirect_uri, resource, scope, allow_delete, created_at) \
             VALUES (?, ?, 'dcr_c', ?, ?, 'soulful offline_access', ?, 0)",
        )
        .bind(grant_id)
        .bind(user)
        .bind(CLAUDE_CALLBACK)
        .bind(RESOURCE)
        .bind(allow_delete)
        .execute(&self.db)
        .await
        .unwrap();
        Origin::Mcp {
            grant_id: grant_id.into(),
        }
    }

    /// Inserts `n` audit rows of alice with origin mcp and no path, created `ago` seconds before
    /// now.
    pub async fn app_deletions(&self, n: u32, ago: i64) {
        for _ in 0..n {
            sqlx::query(
                "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, origin, reason, client, created_at) \
                 VALUES (?, 'alice', 's-earlier', 'Earlier', 'Marrow Choir', 'Winter Songs', 'mcp', 'cleanup', 'claude.ai', \
                         datetime('now', ?))",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(format!("-{ago} seconds"))
            .execute(&self.db)
            .await
            .unwrap();
        }
    }

    /// Writes `bytes` at `{root}/music/{rel}`, modified at mtime(), as the records say.
    pub fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join("music").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        set_mtime(&path, mtime());
    }

    /// Adds song `id` (album "al1") at `{root}/music/{rel}` with `bytes`, seen by alice and bob, unrated.
    pub fn song(&self, id: &str, rel: &str, bytes: &[u8]) {
        self.write(rel, bytes);
        self.fake.file(
            NativeSong {
                id: id.into(),
                path: rel.into(),
                size: bytes.len() as u64,
                ..self.record("s1")
            },
            "al1",
        );
        self.fake.sees("alice", id);
        self.fake.sees("bob", id);
    }

    /// The native record `song` gave the fake, to build a variant of it.
    pub fn record(&self, id: &str) -> NativeSong {
        self.fake.with(|s| s.records[id].0.clone())
    }

    /// Inserts folder `id` of `user` at `{root}/{rel}`.
    pub async fn folder(&self, user: &str, id: &str, rel: &str, shared: bool) {
        sqlx::query("INSERT INTO folders (id, user_id, name, path, shared) VALUES (?, ?, ?, ?, ?)")
            .bind(id)
            .bind(user)
            .bind(id)
            .bind(format!("{}/{rel}", self.root.display()))
            .bind(shared)
            .execute(&self.db)
            .await
            .unwrap();
    }

    /// Inserts a step 5 `jobs` row of alice (import_mode 'singleton') with `wanted` as its JSON;
    /// `set_job_state` changes its state.
    pub async fn job(&self, id: &str, folder_id: &str, state: &str, wanted: DownloadQuery) {
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, state) \
             VALUES (?, 'alice', ?, 1763200000, 1763200000, ?, 'singleton', ?)",
        )
        .bind(id)
        .bind(folder_id)
        .bind(serde_json::to_string(&wanted).unwrap())
        .bind(state)
        .execute(&self.db)
        .await
        .unwrap();
    }

    pub async fn set_job_state(&self, id: &str, state: &str) {
        sqlx::query("UPDATE jobs SET state = ? WHERE id = ?")
            .bind(state)
            .bind(id)
            .execute(&self.db)
            .await
            .unwrap();
    }

    /// Inserts a Pending discovery track titled `id` by "Marrow Choir", and the discovery_history
    /// row of the folder's owner that generation records with it; returns the track as
    /// DiscoveryTrackRow::get_by_id would.
    pub async fn pending_track(
        &self,
        id: &str,
        folder_id: &str,
        profile: &str,
        path: &Path,
        song_id: Option<&str>,
    ) -> DiscoveryTrack {
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile) \
             VALUES (?, ?, ?, 'Marrow Choir', 'Winter Songs', ?, ?, ?)",
        )
        .bind(id)
        .bind(song_id)
        .bind(id)
        .bind(path.to_str().unwrap())
        .bind(folder_id)
        .bind(profile)
        .execute(&self.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO discovery_history (id, user_id, artist, track, profile) \
             SELECT ?, user_id, 'Marrow Choir', ?, ? FROM folders WHERE id = ?",
        )
        .bind(format!("history-{id}"))
        .bind(id)
        .bind(profile)
        .bind(folder_id)
        .execute(&self.db)
        .await
        .unwrap();
        DiscoveryTrackRow::get_by_id_in(&self.db, id)
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn track_status(&self, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM discovery_tracks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.db)
            .await
            .unwrap()
    }

    /// The outcome of the history row `pending_track` wrote for track `id`.
    pub async fn history_outcome(&self, id: &str) -> String {
        sqlx::query_scalar("SELECT outcome FROM discovery_history WHERE id = ?")
            .bind(format!("history-{id}"))
            .fetch_one(&self.db)
            .await
            .unwrap()
    }
}

/// The modification time the test files are given, and Navidrome's records of them carry.
pub(crate) fn mtime() -> chrono::DateTime<chrono::Utc> {
    "2026-11-15T10:01:30.123456789Z".parse().unwrap()
}

/// Sets the modification time of the file at `path` to `at`.
pub(crate) fn set_mtime(path: &Path, at: chrono::DateTime<chrono::Utc>) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at.into())
        .unwrap();
}

/// The library file a check allowed; panics on anything else.
pub(crate) fn library_verdict(v: Result<Verdict, shared::failure::Failure>) -> Checked {
    match v {
        Ok(Verdict::Library(c)) => c,
        Ok(Verdict::Discovery { track, .. }) => panic!("discovery {track:?}"),
        Err(f) => panic!("{f:?}"),
    }
}

/// `song_id`'s file at `path` as a preview stores it, its identity read from the file there now
/// (which must exist).
pub(crate) fn previewed_file(song_id: &str, path: &Path) -> PreviewedFile {
    PreviewedFile {
        song_id: song_id.into(),
        path: path.to_path_buf(),
        file: unlink::FileId::of(&std::fs::symlink_metadata(path).unwrap()),
    }
}

/// A job's `wanted` for one track (Track.id "track-1", no album fields).
pub(crate) fn wanted(artist: &str, title: &str, mbid: Option<&str>) -> DownloadQuery {
    DownloadQuery::new(vec![Track {
        id: "track-1".into(),
        title: title.into(),
        artist: artist.into(),
        album_id: None,
        album_title: None,
        release_date: None,
        duration: None,
        mbid: mbid.map(String::from),
        release_mbid: None,
    }])
}
