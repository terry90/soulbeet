//! What the caller's Navidrome library holds, as the tools report it, the
//! songs that match a track the caller names, and whether imports into a
//! folder show up there.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use shared::navidrome::LibraryStats;
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::{NativeSong, SubsonicAlbum, SubsonicArtist, SubsonicSong};
use soulbeet::navidrome::{LibraryKind, NavidromeClient};
use soulbeet::NavidromeError;
use sqlx::SqlitePool;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::index::{NavidromeOf, LOOKUP_PAGE};
use crate::models::navidrome_library::NavidromeLibraryRow;

/// A song as the tools report it, from a native record or a Subsonic child
/// (D7). The rating, star and play count are the caller's own; Navidrome
/// sends 0 for "none" (model/annotation.go:6-12), and so does this.
/// `path` is relative to the song's library, and only a native record has
/// it: a Subsonic child's is made up, or absolute with ReportRealPath
/// (server/subsonic/helpers.go:227-231).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SongFacts {
    pub song_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub duration_s: Option<u32>,
    pub format: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub size_bytes: Option<u64>,
    pub path: Option<String>,
    pub recording_mbid: Option<String>,
    pub rating: u8,
    pub starred: bool,
    pub starred_at: Option<String>,
    pub play_count: u64,
}

/// A text taken from Navidrome, cut to TEXT_MAX_CHARS of JSON (`cut_json`).
fn cut(value: String) -> String {
    cut_json(value, TEXT_MAX_CHARS)
}

/// A text field taken from Navidrome, cut as `cut` does; empty is unknown.
fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| cut(value.to_string()))
}

/// An id or time taken from Navidrome, never cut; empty is unknown.
fn given(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_string)
}

/// A number Navidrome sends as 0 when it does not know it (model/mediafile.go:49-63).
fn known(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

impl From<&NativeSong> for SongFacts {
    fn from(song: &NativeSong) -> SongFacts {
        SongFacts {
            song_id: song.id.clone(),
            title: cut(song.title.clone()),
            artist: text(&song.artist),
            album: text(&song.album),
            album_id: given(Some(&song.album_id)),
            year: known(song.year),
            track: known(song.track_number),
            duration_s: (song.duration > 0.0).then(|| song.duration.round() as u32),
            format: text(&song.suffix),
            bitrate_kbps: known(song.bit_rate),
            size_bytes: (song.size != 0).then_some(song.size),
            path: text(&song.path),
            recording_mbid: given(song.mbz_recording_id.as_deref()),
            rating: song.rating,
            starred: song.starred,
            starred_at: given(song.starred_at.as_deref()),
            play_count: song.play_count,
        }
    }
}

impl From<&SubsonicSong> for SongFacts {
    fn from(song: &SubsonicSong) -> SongFacts {
        SongFacts {
            song_id: song.id.clone(),
            title: cut(song.title.clone()),
            artist: song.artist.as_deref().and_then(text),
            album: song.album.as_deref().and_then(text),
            album_id: given(song.album_id.as_deref()),
            year: song.year.and_then(known),
            track: song.track.and_then(known),
            duration_s: song.duration.and_then(known),
            format: song.suffix.as_deref().and_then(text),
            bitrate_kbps: song.bit_rate.and_then(known),
            size_bytes: song.size.filter(|size| *size != 0),
            path: None,
            recording_mbid: given(song.music_brainz_id.as_deref()),
            rating: song.user_rating.unwrap_or(0),
            starred: song.starred.is_some(),
            starred_at: given(song.starred.as_deref()),
            play_count: song.play_count.map_or(0, u64::from),
        }
    }
}

/// An album as the tools report it; the rating, star and play count are the
/// caller's own.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlbumFacts {
    pub album_id: String,
    pub name: String,
    pub artist: Option<String>,
    pub year: Option<u32>,
    pub song_count: Option<u32>,
    pub duration_s: Option<u32>,
    pub genre: Option<String>,
    pub rating: u8,
    pub starred: bool,
    pub starred_at: Option<String>,
    pub play_count: u64,
}

impl From<&SubsonicAlbum> for AlbumFacts {
    fn from(album: &SubsonicAlbum) -> AlbumFacts {
        AlbumFacts {
            album_id: album.id.clone(),
            name: cut(album.display_name().to_string()),
            artist: album.artist.as_deref().and_then(text),
            year: album.year.and_then(known),
            song_count: album.song_count,
            duration_s: album.duration,
            genre: album.genre.as_deref().and_then(text),
            rating: album.user_rating.unwrap_or(0),
            starred: album.starred.is_some(),
            starred_at: given(album.starred.as_deref()),
            play_count: album.play_count.unwrap_or(0),
        }
    }
}

/// An artist as the tools report it; the rating and star are the caller's own.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtistFacts {
    pub artist_id: String,
    pub name: String,
    pub album_count: Option<u32>,
    pub rating: u8,
    pub starred: bool,
    pub starred_at: Option<String>,
    pub mbid: Option<String>,
}

impl From<&SubsonicArtist> for ArtistFacts {
    fn from(artist: &SubsonicArtist) -> ArtistFacts {
        ArtistFacts {
            artist_id: artist.id.clone(),
            name: cut(artist.name.clone()),
            album_count: artist.album_count,
            rating: artist.user_rating.unwrap_or(0),
            starred: artist.starred.is_some(),
            starred_at: given(artist.starred.as_deref()),
            mbid: given(artist.music_brainz_id.as_deref()),
        }
    }
}

/// `e` as the failure of `stage`, shown as its own text.
pub(crate) fn navidrome_failure(stage: Stage, e: &NavidromeError) -> Failure {
    e.failure(stage, e.to_string())
}

/// The caller's Navidrome client, or why there is none as the failure of
/// `stage`. The server's `navidrome` is navidrome_client_for_user(ctx.user_id),
/// which signs in with the caller's own Navidrome account: Navidrome then
/// answers with what that account sees (an admin: every library), and the
/// ratings, stars and play counts are the caller's own.
pub(crate) async fn navidrome_for(
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    stage: Stage,
) -> Result<Arc<NavidromeClient>, Failure> {
    navidrome
        .navidrome_of(&ctx.id())
        .await
        .map_err(|e| e.failure(stage, e.to_string()))
}

/// The library's counts: every song search3 lists and every album
/// getAlbumList2 lists, the caller's ratings among the songs, and the 20
/// most common genres. Navidrome's search3 answers ok without the songs on a
/// database error (server/subsonic/searching.go:42-56), so such an error
/// reads as too few tracks; getAlbumList2 fails on one (album_lists.go:75-78).
pub async fn stats(navi: &NavidromeClient) -> Result<LibraryStats, Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    let songs = navi.get_all_songs_with_ratings().await.map_err(failure)?;
    let albums = navi.get_all_albums().await.map_err(failure)?;

    let total_tracks = songs.len() as u32;
    let mut rated_tracks = 0u32;
    let mut rating_sum = 0.0f64;
    let mut rating_distribution = [0u32; 5];
    let mut genres: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut artists: std::collections::HashSet<String> = std::collections::HashSet::new();

    for song in &songs {
        if let Some(artist) = &song.artist {
            artists.insert(artist.to_lowercase());
        }
        if let Some(genre) = &song.genre {
            if !genre.is_empty() {
                *genres.entry(genre.clone()).or_default() += 1;
            }
        }
        if let Some(rating) = song.user_rating {
            if (1..=5).contains(&rating) {
                rated_tracks += 1;
                rating_sum += rating as f64;
                rating_distribution[(rating - 1) as usize] += 1;
            }
        }
    }

    let average_rating = if rated_tracks > 0 {
        rating_sum / rated_tracks as f64
    } else {
        0.0
    };

    let mut genre_vec: Vec<(String, u32)> = genres.into_iter().collect();
    genre_vec.sort_by_key(|a| std::cmp::Reverse(a.1));
    genre_vec.truncate(20);

    Ok(LibraryStats {
        total_tracks,
        rated_tracks,
        unrated_tracks: total_tracks - rated_tracks,
        average_rating,
        rating_distribution,
        total_albums: albums.len() as u32,
        total_artists: artists.len() as u32,
        genres: genre_vec,
    })
}

/// Most seconds a song's length may differ from the reference's for an exact match.
pub(crate) const DURATION_TOLERANCE_S: u32 = 5;
/// Least word similarity, of the title and of the artist, for a fuzzy match.
pub(crate) const FUZZY_FLOOR: f64 = 0.5;
/// Matches kept per reference.
pub(crate) const MATCHES_KEPT: usize = 5;
/// Most pages of LOOKUP_PAGE songs the text lookup reads.
pub(crate) const TEXT_LOOKUP_PAGES: u32 = 5;
/// References looked up at once.
pub(crate) const LOOKUPS_AT_ONCE: usize = 4;

/// A track to look for in the library, as the caller describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub recording_mbid: Option<String>,
    pub duration_s: Option<u32>,
}

/// How a song matched a reference (D8).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Its recording MBID is the reference's.
    Mbid,
    /// Same normalized artist and title, and the length within
    /// DURATION_TOLERANCE_S when the reference gives one.
    Exact,
    /// Similar artist and title; the caller decides whether it is the track.
    Fuzzy,
}

/// A song that matched a reference, and how.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    #[serde(flatten)]
    pub song: SongFacts,
    #[serde(rename = "match")]
    pub how: MatchHow,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchHow {
    pub kind: MatchKind,
    /// The mean word similarity of title and artist, for a fuzzy match.
    pub score: Option<f64>,
    /// What the song does not share with the reference: `artist`, `title`,
    /// `album` (when given) and `duration` (when given and more than
    /// DURATION_TOLERANCE_S off).
    pub differs: Vec<&'static str>,
    /// The song's length less the reference's, when both are known.
    pub duration_delta_s: Option<i64>,
    /// What the reference gives that the song does not say, so it was not
    /// compared. Per field:
    /// - `duration`: given, and Navidrome does not know the song's length.
    ///   Such a song is no exact match: `exact` needs the length within
    ///   DURATION_TOLERANCE_S when the reference gives one.
    /// - `album`: given, and the song has none. The album never decides the
    ///   kind, whether it agrees, differs or is unchecked: `mbid` looks at
    ///   the MBID alone, `exact` at artist, title and length, and `fuzzy`'s
    ///   score at title and artist. It is reported, in `differs` or here,
    ///   for the caller to weigh.
    pub unchecked: Vec<&'static str>,
}

/// What the library holds for a reference.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Found {
    /// The best MATCHES_KEPT matches, best first.
    pub matches: Vec<Match>,
    /// Matches left out of `matches`.
    pub more_matches: usize,
    /// The MBID lookup read a full page, or the text lookup read
    /// TEXT_LOOKUP_PAGES full pages, so Navidrome may hold more candidates
    /// than were compared.
    pub candidates_truncated: bool,
}

/// D8: `s` as normalize_for_matching gives it, or, for a text with no letter
/// or digit (the band "!!!"), lowercased with its white space collapsed, so
/// such a name equals only itself and not every name that normalizes to "".
pub(crate) fn match_key(s: &str) -> String {
    let key = normalize_for_matching(s);
    if !key.is_empty() {
        return key;
    }
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Dice similarity of the words of `a` and `b`'s match keys, each word
/// counted as often as it occurs: 1.0 for two equal keys.
fn word_dice(a: &str, b: &str) -> f64 {
    let (a, b) = (match_key(a), match_key(b));
    if a == b {
        return 1.0;
    }
    let a: Vec<&str> = a.split(' ').filter(|w| !w.is_empty()).collect();
    let mut rest: Vec<&str> = b.split(' ').filter(|w| !w.is_empty()).collect();
    let total = a.len() + rest.len();
    let mut shared = 0;
    for word in a {
        if let Some(at) = rest.iter().position(|w| *w == word) {
            rest.swap_remove(at);
            shared += 1;
        }
    }
    (2 * shared) as f64 / total as f64
}

/// D8. None when `song` is not a match. An MBID is compared ignoring ASCII
/// case, as UUIDs are. A song whose length Navidrome does not know has no
/// duration_delta_s, so a reference with a duration matches it at best
/// fuzzily, with `duration` in `unchecked`, not in `differs`; an album the
/// song does not name is `unchecked` too, and the album, given or not,
/// never changes the kind (`MatchHow::unchecked`).
pub(crate) fn classify(r: &Reference, song: &NativeSong) -> Option<Match> {
    let facts = SongFacts::from(song);
    let duration_delta_s = r
        .duration_s
        .zip(facts.duration_s)
        .map(|(wanted, length)| i64::from(length) - i64::from(wanted));
    let length_off =
        duration_delta_s.is_some_and(|d| d.unsigned_abs() > u64::from(DURATION_TOLERANCE_S));
    let same = |a: &str, b: &str| match_key(a) == match_key(b);
    let (artist_same, title_same) = (same(&r.artist, &song.artist), same(&r.title, &song.title));
    let mut differs = Vec::new();
    if !artist_same {
        differs.push("artist");
    }
    if !title_same {
        differs.push("title");
    }
    let mut unchecked = Vec::new();
    if r.album.is_some() && song.album.is_empty() {
        unchecked.push("album");
    } else if r
        .album
        .as_deref()
        .is_some_and(|album| !same(album, &song.album))
    {
        differs.push("album");
    }
    if length_off {
        differs.push("duration");
    }
    if r.duration_s.is_some() && duration_delta_s.is_none() {
        unchecked.push("duration");
    }
    let mbid = r
        .recording_mbid
        .as_deref()
        .zip(song.mbz_recording_id.as_deref())
        .is_some_and(|(wanted, tagged)| wanted.eq_ignore_ascii_case(tagged));
    let length_within = r.duration_s.is_none() || (duration_delta_s.is_some() && !length_off);
    let (kind, score) = if mbid {
        (MatchKind::Mbid, None)
    } else if artist_same && title_same && length_within {
        (MatchKind::Exact, None)
    } else {
        let (title, artist) = (
            word_dice(&r.title, &song.title),
            word_dice(&r.artist, &song.artist),
        );
        if title < FUZZY_FLOOR || artist < FUZZY_FLOOR {
            return None;
        }
        (
            MatchKind::Fuzzy,
            Some(((title + artist) / 2.0 * 100.0).round() / 100.0),
        )
    };
    Some(Match {
        song: facts,
        how: MatchHow {
            kind,
            score,
            differs,
            duration_delta_s,
            unchecked,
        },
    })
}

/// D8: the MBID lookup (one page) when the reference has a recording MBID,
/// then, unless it found an mbid match, the text lookup of
/// "{artist} {title}" in pages of LOOKUP_PAGE up to a short page, at most
/// TEXT_LOOKUP_PAGES. Each song is classified once, by id. The matches come
/// mbid first, then exact, then fuzzy by score, ties by song id, cut to
/// MATCHES_KEPT. Failures are LibraryCheck failures.
pub async fn find(navi: &NavidromeClient, r: &Reference) -> Result<Found, Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    let mut take = |songs: Vec<NativeSong>, matches: &mut Vec<Match>| {
        for song in songs {
            if seen.insert(song.id.clone()) {
                matches.extend(classify(r, &song));
            }
        }
    };
    let mut candidates_truncated = false;
    if let Some(mbid) = &r.recording_mbid {
        let songs = navi
            .find_songs(mbid, 0, LOOKUP_PAGE)
            .await
            .map_err(failure)?;
        candidates_truncated = songs.len() == LOOKUP_PAGE as usize;
        take(songs, &mut matches);
    }
    if !matches.iter().any(|m| m.how.kind == MatchKind::Mbid) {
        let text = format!("{} {}", r.artist, r.title);
        for page in 0..TEXT_LOOKUP_PAGES {
            let start = page * LOOKUP_PAGE;
            let songs = navi
                .find_songs(&text, start, start + LOOKUP_PAGE)
                .await
                .map_err(failure)?;
            let short = songs.len() < LOOKUP_PAGE as usize;
            take(songs, &mut matches);
            if short {
                break;
            }
            candidates_truncated |= page + 1 == TEXT_LOOKUP_PAGES;
        }
    }
    matches.sort_by(|a, b| {
        (a.how.kind as u8)
            .cmp(&(b.how.kind as u8))
            .then(
                b.how
                    .score
                    .unwrap_or(0.0)
                    .total_cmp(&a.how.score.unwrap_or(0.0)),
            )
            .then_with(|| a.song.song_id.cmp(&b.song.song_id))
    });
    let more_matches = matches.len().saturating_sub(MATCHES_KEPT);
    matches.truncate(MATCHES_KEPT);
    Ok(Found {
        matches,
        more_matches,
        candidates_truncated,
    })
}

/// One kind of library item, as the tools report it.
#[derive(Debug, Clone, PartialEq)]
pub enum LibraryItems {
    Songs(Vec<SongFacts>),
    Albums(Vec<AlbumFacts>),
    Artists(Vec<ArtistFacts>),
}

impl LibraryItems {
    /// Each item as its JSON object, in order.
    pub fn into_values(self) -> Vec<Value> {
        fn values<T: Serialize>(items: Vec<T>) -> Vec<Value> {
            items.iter().map(|item| json!(item)).collect()
        }
        match self {
            LibraryItems::Songs(songs) => values(songs),
            LibraryItems::Albums(albums) => values(albums),
            LibraryItems::Artists(artists) => values(artists),
        }
    }

    /// The `kind` lists of an answer that holds all three.
    fn of(
        kind: LibraryKind,
        songs: &[SubsonicSong],
        albums: &[SubsonicAlbum],
        artists: &[SubsonicArtist],
    ) -> LibraryItems {
        match kind {
            LibraryKind::Songs => LibraryItems::Songs(songs.iter().map(SongFacts::from).collect()),
            LibraryKind::Albums => {
                LibraryItems::Albums(albums.iter().map(AlbumFacts::from).collect())
            }
            LibraryKind::Artists => {
                LibraryItems::Artists(artists.iter().map(ArtistFacts::from).collect())
            }
        }
    }
}

/// search3 of `kind` from `offset`, `count` items, most relevant first.
/// Navidrome answers a database error as an empty or short list
/// (server/subsonic/searching.go:47-62), so only a failed request fails.
pub async fn search(
    navi: &NavidromeClient,
    query: &str,
    kind: LibraryKind,
    offset: u32,
    count: u32,
) -> Result<LibraryItems, Failure> {
    let found = navi
        .search3(query, kind, offset, count)
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;
    Ok(LibraryItems::of(
        kind,
        &found.song,
        &found.album,
        &found.artist,
    ))
}

/// The album with its songs in album order; album_not_found(id) when
/// Navidrome has none this account can see (D12).
pub async fn album(
    navi: &NavidromeClient,
    album_id: &str,
) -> Result<(AlbumFacts, Vec<SongFacts>), Failure> {
    let album = navi
        .get_album(album_id)
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?
        .ok_or_else(|| album_not_found(album_id))?;
    let songs = album.song.iter().map(SongFacts::from).collect();
    Ok((AlbumFacts::from(&album), songs))
}

/// What the caller starred of `kind`, most recently starred first
/// (server/filter/filters.go:221-227).
pub async fn starred(navi: &NavidromeClient, kind: LibraryKind) -> Result<LibraryItems, Failure> {
    let starred = navi
        .get_starred()
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;
    Ok(LibraryItems::of(
        kind,
        &starred.song,
        &starred.album,
        &starred.artist,
    ))
}

/// An album id Navidrome does not know or that is in a library this account
/// cannot see: Navidrome answers both alike (D12).
pub(crate) fn album_not_found(id: &str) -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        format!("Navidrome has no album {id} that this account can see"),
    )
}

/// Whether imports into a folder show up in Navidrome, as far as Soulful
/// knows. There is no "not listed": a library Soulful has not located on
/// this machine yet may hold the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Listing {
    /// A known library root holds the folder, so Navidrome scans what is
    /// imported into it.
    Listed,
    /// No known library root holds the folder.
    Unknown,
}

/// Where Navidrome's libraries are on this machine, as far as Soulful knows:
/// `music_path` (NAVIDROME_MUSIC_PATH, the default library's root) and every
/// root learned from an import or a deletion check (navidrome_libraries),
/// in library order. Navidrome's own path of a library stands for no root
/// here: a learned root replaces it, and without one it is known only from
/// the songs Navidrome lists.
pub(crate) async fn library_roots_in(
    db: &SqlitePool,
    music_path: Option<&Path>,
) -> Result<Vec<PathBuf>, Failure> {
    let learned = NavidromeLibraryRow::all_in(db)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(music_path
        .map(Path::to_path_buf)
        .into_iter()
        .chain(learned.into_iter().map(|row| PathBuf::from(row.local_path)))
        .collect())
}

/// Listed when one of `roots` is `folder` or one of its parents, by whole
/// path components; Unknown otherwise.
pub(crate) fn listing(folder: &Path, roots: &[PathBuf]) -> Listing {
    match roots.iter().any(|root| folder.starts_with(root)) {
        true => Listing::Listed,
        false => Listing::Unknown,
    }
}

/// Songs `duplicates` reads from Navidrome in one request.
pub(crate) const DUPLICATE_PAGE: u32 = 1000;

/// Songs of the library that share an artist and title (compared as `match_key` does).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SongGroup {
    /// The first song's artist and title, as it has them.
    pub artist: String,
    pub title: String,
    /// Whether the songs are one recording: true when each has a recording MBID and they are
    /// all equal, false when each has one and they differ, None when a song has none.
    pub same_recording: Option<bool>,
    /// The songs, by id.
    pub songs: Vec<SongFacts>,
}

/// D11: every present song of the caller through `list_songs`, DUPLICATE_PAGE at a time until a
/// short page, grouped by (match_key(artist), match_key(title)); the groups of two or more,
/// ordered by that key. A song id seen again on a later page is skipped. A page that fails fails the search, since a part would miss copies.
pub async fn duplicates(navi: &NavidromeClient) -> Result<Vec<SongGroup>, Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    let mut by_key: BTreeMap<(String, String), Vec<SongFacts>> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut start = 0;
    loop {
        let page = navi
            .list_songs(start, start + DUPLICATE_PAGE)
            .await
            .map_err(failure)?;
        let last = page.len() < DUPLICATE_PAGE as usize;
        for song in &page {
            // A song added mid-search shifts the offsets, so a page can repeat a row.
            if !seen.insert(song.id.clone()) {
                continue;
            }
            by_key
                .entry((match_key(&song.artist), match_key(&song.title)))
                .or_default()
                .push(SongFacts::from(song));
        }
        if last {
            break;
        }
        start += DUPLICATE_PAGE;
    }
    Ok(by_key
        .into_values()
        .filter(|songs| songs.len() > 1)
        .map(|mut songs| {
            songs.sort_by(|a, b| a.song_id.cmp(&b.song_id));
            let mbids: Option<Vec<&str>> =
                songs.iter().map(|s| s.recording_mbid.as_deref()).collect();
            let same_recording =
                mbids.map(|ids| ids.iter().all(|id| id.eq_ignore_ascii_case(ids[0])));
            SongGroup {
                artist: songs[0].artist.clone().unwrap_or_default(),
                title: songs[0].title.clone(),
                same_recording,
                songs,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::models::navidrome_library::NavidromeLibraryRow;
    use crate::test_navidrome::{
        child, native_song, song_requests, subsonic_failed, subsonic_ok, NavidromeStub, BAD_GATEWAY,
    };
    use serde_json::json;
    use shared::failure::{Cause, FailureCode, RetryAdvice, Service};
    use soulbeet::navidrome::models::{NativeSong, SubsonicSong};

    #[test]
    fn song_facts_come_from_a_native_record_with_its_path() {
        let song: NativeSong = serde_json::from_str(&native_song(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            "Marrow Choir",
            "O Holy Night",
            263.4,
            Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"),
        ))
        .unwrap();
        assert_eq!(
            SongFacts::from(&song),
            SongFacts {
                song_id: "3sG8kPq0WmZ1xYbN4vTcDe".into(),
                title: "O Holy Night".into(),
                artist: Some("Marrow Choir".into()),
                album: Some("Winter Songs".into()),
                album_id: Some("7hQ2nLr5VxZ0cYbM8wTkAf".into()),
                year: Some(2019),
                track: Some(3),
                duration_s: Some(263),
                format: Some("flac".into()),
                bitrate_kbps: Some(912),
                size_bytes: Some(28734112),
                path: Some("Marrow Choir/Winter Songs/03 - O Holy Night.flac".into()),
                recording_mbid: Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c".into()),
                rating: 4,
                starred: true,
                starred_at: Some("2026-11-02T18:04:51.112Z".into()),
                play_count: 3
            }
        );
    }

    #[test]
    fn song_facts_from_a_subsonic_child_have_no_path() {
        let song: SubsonicSong = serde_json::from_str(&child(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            "Marrow Choir",
            "O Holy Night",
        ))
        .unwrap();
        let f = SongFacts::from(&song);
        assert_eq!(
            (
                f.path,
                f.track,
                f.rating,
                f.starred,
                f.play_count,
                f.recording_mbid.as_deref()
            ),
            (
                None,
                Some(3),
                4,
                true,
                0,
                Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c")
            )
        );
        let bare: SubsonicSong =
            serde_json::from_str(r#"{"id":"x1","title":"Silent Night","musicBrainzId":""}"#)
                .unwrap();
        let f = SongFacts::from(&bare);
        assert_eq!(
            (f.recording_mbid, f.format, f.rating, f.starred),
            (None, None, 0, false)
        );
    }

    #[test]
    fn a_title_from_a_file_tag_is_cut_to_500_characters() {
        let song: NativeSong = serde_json::from_str(&native_song(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            "Marrow Choir",
            &"x".repeat(10_000),
            263.4,
            None,
        ))
        .unwrap();
        let f = SongFacts::from(&song);
        assert!((495..=500).contains(&shared::failure::json_chars(&f.title)));
        assert!(f.title.contains(" characters left out]\n"), "{}", f.title);
        assert_eq!(f.recording_mbid, None);
        assert_eq!(f.song_id, "3sG8kPq0WmZ1xYbN4vTcDe");
    }

    #[tokio::test]
    async fn stats_count_rated_tracks_albums_artists_and_genres() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{},{}]}}"#,
                child("s1", "Marrow Choir", "O Holy Night"),
                child("s2", "Marrow Choir", "Silent Night").replace(r#""userRating":4,"#, "")
            ))],
        );
        navi.answer(
            "/rest/getAlbumList2",
            &[&subsonic_ok(
                r#""albumList2":{"album":[{"id":"7hQ2nLr5VxZ0cYbM8wTkAf","name":"Winter Songs","artist":"Marrow Choir"}]}"#,
            )],
        );
        let s = stats(&navi.client()).await.unwrap();
        assert_eq!(
            (
                s.total_tracks,
                s.rated_tracks,
                s.unrated_tracks,
                s.average_rating,
                s.rating_distribution,
                s.total_albums,
                s.total_artists
            ),
            (2, 1, 1, 4.0, [0, 0, 0, 1, 0], 1, 1)
        );
    }

    #[tokio::test]
    async fn a_refused_library_read_is_a_library_check_failure() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{}]}}"#,
                child("s1", "Marrow Choir", "O Holy Night")
            ))],
        );
        // getAlbumList2 on a database error: newError(ErrorGeneric, "internal error"),
        // server/subsonic/album_lists.go:75-78 (0.64.2)
        navi.answer(
            "/rest/getAlbumList2",
            &[&subsonic_failed(0, "internal error")],
        );
        let f = stats(&navi.client()).await.unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::NavidromeRefused,
                Stage::LibraryCheck,
                RetryAdvice::NotRetryable,
                "Navidrome error 0: internal error"
            )
        );
        assert_eq!(
            f.cause,
            Some(Cause::new(Service::Navidrome, Some(0), "internal error"))
        );
    }

    #[test]
    fn a_folder_inside_a_known_root_is_listed_and_any_other_is_unknown() {
        let roots = [PathBuf::from("/music"), PathBuf::from("/srv/bob")];
        for (folder, want) in [
            ("/music/alice", Listing::Listed),
            ("/music", Listing::Listed),
            ("/srv/bob/", Listing::Listed),
            ("/music-archive/alice", Listing::Unknown),
            ("/srv", Listing::Unknown),
            ("/downloads", Listing::Unknown),
        ] {
            assert_eq!(listing(Path::new(folder), &roots), want, "{folder}");
        }
        assert_eq!(listing(Path::new("/music/alice"), &[]), Listing::Unknown);
    }

    #[tokio::test]
    async fn the_known_roots_are_the_music_path_and_every_learned_root() {
        let db = test_db::migrated().await;
        assert_eq!(
            library_roots_in(&db, None).await.unwrap(),
            Vec::<PathBuf>::new()
        );
        for (library_id, navidrome_path, local_path) in [
            (2, "/libraries/bob", "/srv/bob"),
            (1, "/music", "/mnt/music"),
        ] {
            NavidromeLibraryRow::upsert_in(
                &db,
                &NavidromeLibraryRow {
                    library_id,
                    navidrome_path: navidrome_path.into(),
                    local_path: local_path.into(),
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            library_roots_in(&db, Some(Path::new("/data/music")))
                .await
                .unwrap(),
            vec![
                PathBuf::from("/data/music"),
                PathBuf::from("/mnt/music"),
                PathBuf::from("/srv/bob")
            ]
        );
    }

    const MBID: &str = "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c";

    fn song(title: &str, artist: &str, duration: f64, mbid: Option<&str>) -> NativeSong {
        serde_json::from_str(&native_song(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            artist,
            title,
            duration,
            mbid,
        ))
        .unwrap()
    }

    fn wanted(artist: &str, title: &str) -> Reference {
        Reference {
            artist: artist.into(),
            title: title.into(),
            album: None,
            recording_mbid: None,
            duration_s: None,
        }
    }

    #[test]
    fn a_song_with_the_recording_mbid_is_an_mbid_match_whatever_its_tags() {
        let r = Reference {
            recording_mbid: Some(MBID.into()),
            ..wanted("Bing Crosby", "Silent Night")
        };
        let m = classify(&r, &song("O Holy Night", "Marrow Choir", 263.4, Some(MBID))).unwrap();
        assert_eq!(
            m.how,
            MatchHow {
                kind: MatchKind::Mbid,
                score: None,
                differs: vec!["artist", "title"],
                duration_delta_s: None,
                unchecked: vec![]
            }
        );
    }

    #[test]
    fn equal_normalized_artist_and_title_within_five_seconds_is_exact() {
        let r = Reference {
            duration_s: Some(268),
            ..wanted("marrow choir", "O holy night!")
        };
        let m = classify(&r, &song("O Holy Night", "Marrow Choir", 263.4, None)).unwrap();
        assert_eq!(
            m.how,
            MatchHow {
                kind: MatchKind::Exact,
                score: None,
                differs: vec![],
                duration_delta_s: Some(-5),
                unchecked: vec![]
            }
        );
        let r = Reference {
            album: Some("Christmas Hymns".into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        assert_eq!(
            classify(&r, &song("O Holy Night", "Marrow Choir", 263.4, None))
                .unwrap()
                .how
                .differs,
            vec!["album"]
        );
    }

    #[test]
    fn a_longer_title_or_another_length_is_fuzzy_with_what_differs() {
        let m = classify(
            &wanted("Marrow Choir", "O Holy Night (Cantique de Noël)"),
            &song("O Holy Night", "Marrow Choir", 263.4, None),
        )
        .unwrap();
        // title words {o, holy, night, cantique, de, noël} against {o, holy, night}: 2*3/9 = 0.67; artist 1.0
        assert_eq!(
            m.how,
            MatchHow {
                kind: MatchKind::Fuzzy,
                score: Some(0.83),
                differs: vec!["title"],
                duration_delta_s: None,
                unchecked: vec![]
            }
        );
        let r = Reference {
            duration_s: Some(275),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        assert_eq!(
            classify(&r, &song("O Holy Night", "Marrow Choir", 263.4, None))
                .unwrap()
                .how,
            MatchHow {
                kind: MatchKind::Fuzzy,
                score: Some(1.0),
                differs: vec!["duration"],
                duration_delta_s: Some(-12),
                unchecked: vec![]
            }
        );
    }

    #[test]
    fn the_album_is_reported_but_never_decides_the_kind() {
        let exact = |album: &str| {
            let r = Reference {
                album: Some("Winter Songs".into()),
                ..wanted("Marrow Choir", "O Holy Night")
            };
            let mut s = song("O Holy Night", "Marrow Choir", 263.4, None);
            s.album = album.into();
            let how = classify(&r, &s).unwrap().how;
            (how.kind, how.differs, how.unchecked)
        };
        assert_eq!(exact("Winter Songs"), (MatchKind::Exact, vec![], vec![]));
        assert_eq!(
            exact("Christmas Hymns"),
            (MatchKind::Exact, vec!["album"], vec![])
        );
        assert_eq!(exact(""), (MatchKind::Exact, vec![], vec!["album"]));
    }

    #[test]
    fn a_length_navidrome_does_not_know_is_unchecked_not_agreed() {
        let r = Reference {
            duration_s: Some(263),
            album: Some("Winter Songs".into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        let mut unknown = song("O Holy Night", "Marrow Choir", 0.0, None);
        unknown.album = String::new();
        assert_eq!(
            classify(&r, &unknown).unwrap().how,
            MatchHow {
                kind: MatchKind::Fuzzy,
                score: Some(1.0),
                differs: vec![],
                duration_delta_s: None,
                unchecked: vec!["album", "duration"]
            }
        );
    }

    #[test]
    fn zero_and_empty_navidrome_values_are_unknown() {
        // Navidrome sends 0 or "" for what it does not know (model/mediafile.go:49-63), and an
        // empty MBID for an untagged file.
        let mut record: serde_json::Value = serde_json::from_str(&native_song(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            "Marrow Choir",
            "O Holy Night",
            0.0,
            Some(""),
        ))
        .unwrap();
        for (key, zero) in [
            ("year", json!(0)),
            ("bitRate", json!(0)),
            ("suffix", json!("")),
            ("size", json!(0)),
            ("trackNumber", json!(0)),
            ("albumId", json!("")),
        ] {
            record[key] = zero;
        }
        let f = SongFacts::from(&serde_json::from_value::<NativeSong>(record).unwrap());
        assert_eq!(
            (f.year, f.duration_s, f.bitrate_kbps, f.format, f.size_bytes),
            (None, None, None, None, None)
        );
        assert_eq!((f.track, f.album_id, f.recording_mbid), (None, None, None));
        let child: SubsonicSong = serde_json::from_str(
            r#"{"id":"x1","title":"Silent Night","year":0,"bitRate":0,"suffix":"","duration":0,"size":0,"musicBrainzId":""}"#,
        )
        .unwrap();
        let f = SongFacts::from(&child);
        assert_eq!(
            (
                f.year,
                f.duration_s,
                f.bitrate_kbps,
                f.format,
                f.size_bytes,
                f.recording_mbid
            ),
            (None, None, None, None, None, None)
        );
    }

    #[test]
    fn another_artist_is_not_a_match() {
        assert_eq!(
            classify(
                &wanted("Bing Crosby", "O Holy Night"),
                &song("O Holy Night", "Marrow Choir", 263.4, None)
            ),
            None
        );
    }

    #[test]
    fn a_name_without_letters_or_digits_matches_only_itself() {
        // !!! and their album Myth Takes (2007)
        assert_eq!(
            (match_key("!!!"), match_key(" O  Holy Night! ")),
            ("!!!".to_string(), "o holy night".to_string())
        );
        let m = classify(
            &wanted("!!!", "Myth Takes"),
            &song("Myth Takes", "!!!", 263.4, None),
        )
        .unwrap();
        assert_eq!(m.how.kind, MatchKind::Exact);
        assert_eq!(
            classify(
                &wanted("!!!", "Myth Takes"),
                &song("Myth Takes", "???", 263.4, None)
            )
            .map(|m| m.how.kind),
            None
        );
    }

    #[tokio::test]
    async fn an_mbid_match_needs_no_text_lookup() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song(
                    "3sG8kPq0WmZ1xYbN4vTcDe",
                    "Marrow Choir",
                    "O Holy Night",
                    263.4,
                    Some(MBID)
                )
            )],
        );
        let r = Reference {
            recording_mbid: Some(MBID.into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        let found = find(&navi.client(), &r).await.unwrap();
        assert_eq!(
            (
                found.matches.len(),
                found.matches[0].how.kind,
                found.more_matches,
                found.candidates_truncated
            ),
            (1, MatchKind::Mbid, 0, false)
        );
        assert_eq!(
            song_requests(&navi),
            vec![format!(
                "/api/song?_start=0&_end=100&_sort=id&_order=ASC&title={MBID}&missing=false"
            )]
        );
    }

    #[tokio::test]
    async fn without_an_mbid_match_the_text_lookup_runs_and_sorts_its_matches() {
        let navi = NavidromeStub::start();
        navi.answer_when(
            "/api/song",
            "title=Marrow",
            200,
            &format!(
                "[{},{}]",
                native_song(
                    "f1",
                    "Marrow Choir",
                    "O Holy Night (Cantique de Noël)",
                    263.4,
                    None
                ),
                native_song("e1", "Marrow Choir", "O Holy Night", 263.4, None)
            ),
        );
        navi.answer("/api/song", &["[]"]);
        let r = Reference {
            recording_mbid: Some(MBID.into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        let found = find(&navi.client(), &r).await.unwrap();
        assert_eq!(
            found
                .matches
                .iter()
                .map(|m| (m.song.song_id.as_str(), m.how.kind))
                .collect::<Vec<_>>(),
            vec![("e1", MatchKind::Exact), ("f1", MatchKind::Fuzzy)]
        );
        assert!(
            song_requests(&navi)
                .iter()
                .any(|t| t.contains("title=Marrow+Choir+O+Holy+Night")),
            "{:?}",
            song_requests(&navi)
        );
    }

    #[tokio::test]
    async fn a_song_both_lookups_return_is_kept_once() {
        let navi = NavidromeStub::start();
        // Tagged with another recording, so the MBID lookup finds it (as a release track id
        // would, persistence/mediafile_repository.go:97) but it is no mbid match.
        let other = "5b1e9d3c-2f4a-4c6e-8d7b-9a0c1e2f3b4d";
        navi.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song("e1", "Marrow Choir", "O Holy Night", 263.4, Some(other))
            )],
        );
        let r = Reference {
            recording_mbid: Some(MBID.into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        let found = find(&navi.client(), &r).await.unwrap();
        assert_eq!(
            (
                found.matches.len(),
                found.matches[0].how.kind,
                found.more_matches
            ),
            (1, MatchKind::Exact, 0)
        );
        assert_eq!(song_requests(&navi).len(), 2);
    }

    #[tokio::test]
    async fn a_full_page_of_the_mbid_lookup_is_truncated() {
        let navi = NavidromeStub::start();
        // 100 songs tagged with another recording that a release track id ties to MBID
        // (persistence/mediafile_repository.go:97), then nothing for the text lookup.
        let other = "5b1e9d3c-2f4a-4c6e-8d7b-9a0c1e2f3b4d";
        let full = (0..100)
            .map(|i| {
                native_song(
                    &format!("s{i:03}"),
                    "Marrow Choir",
                    "O Holy Night",
                    263.4,
                    Some(other),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        navi.answer("/api/song", &[&format!("[{full}]"), "[]"]);
        let r = Reference {
            recording_mbid: Some(MBID.into()),
            ..wanted("Marrow Choir", "O Holy Night")
        };
        let found = find(&navi.client(), &r).await.unwrap();
        assert_eq!(
            (
                found.matches.len(),
                found.more_matches,
                found.candidates_truncated
            ),
            (5, 95, true)
        );
        assert_eq!(song_requests(&navi).len(), 2);
    }

    #[tokio::test]
    async fn the_text_lookup_stops_after_five_full_pages_and_says_so() {
        let navi = NavidromeStub::start();
        let pages: Vec<String> = (0..5)
            .map(|p| {
                format!(
                    "[{}]",
                    (0..100)
                        .map(|i| native_song(
                            &format!("s{p}{i:02}"),
                            "Marrow Choir",
                            "O Holy Night",
                            263.4,
                            None
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect();
        navi.answer(
            "/api/song",
            &pages.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let found = find(&navi.client(), &wanted("Marrow Choir", "O Holy Night"))
            .await
            .unwrap();
        assert_eq!(
            (
                found.matches.len(),
                found.more_matches,
                found.candidates_truncated
            ),
            (5, 495, true)
        );
        assert_eq!(song_requests(&navi).len(), 5);
    }

    #[tokio::test]
    async fn a_failed_lookup_is_a_library_check_failure() {
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "", 502, "");
        let f = find(&navi.client(), &wanted("Marrow Choir", "O Holy Night"))
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::NavidromeUnreachable,
                Stage::LibraryCheck,
                "Find songs failed: HTTP 502"
            )
        );
    }

    #[tokio::test]
    async fn songs_held_twice_are_grouped_across_pages_and_say_whether_they_are_one_recording() {
        let navi = NavidromeStub::start();
        let mbid = Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c");
        // A full first page: 998 distinct songs, then two copies of O Holy Night; the second page
        // holds Silent Night twice, one without an MBID.
        let mut first: Vec<String> = (0..998)
            .map(|i| {
                native_song(
                    &format!("f{i:04}"),
                    "Filler",
                    &format!("Track {i}"),
                    200.0,
                    None,
                )
            })
            .collect();
        first.push(native_song(
            "a1",
            "Marrow Choir",
            "O Holy Night",
            263.4,
            mbid,
        ));
        first.push(native_song(
            "a2",
            "marrow choir",
            "O Holy Night!",
            262.0,
            mbid,
        ));
        let second = [
            native_song(
                "b1",
                "Marrow Choir",
                "Silent Night",
                222.0,
                Some("6a09041b-0f79-3278-88d9-0552dd28d37f"),
            ),
            native_song("b2", "Marrow Choir", "Silent Night", 222.0, None),
        ];
        navi.answer(
            "/api/song",
            &[
                &format!("[{}]", first.join(",")),
                &format!("[{}]", second.join(",")),
            ],
        );
        let groups = duplicates(&navi.client()).await.unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|g| (
                    g.title.as_str(),
                    g.same_recording,
                    g.songs
                        .iter()
                        .map(|s| s.song_id.as_str())
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("O Holy Night", Some(true), vec!["a1", "a2"]),
                ("Silent Night", None, vec!["b1", "b2"])
            ]
        );
        assert!(song_requests(&navi)
            .iter()
            .any(|t| t.contains("_start=1000&_end=2000")));
    }

    #[tokio::test]
    async fn a_song_a_later_page_repeats_is_not_its_own_duplicate() {
        let navi = NavidromeStub::start();
        let mut first: Vec<String> = (0..999)
            .map(|i| {
                native_song(
                    &format!("f{i:04}"),
                    "Filler",
                    &format!("Track {i}"),
                    200.0,
                    None,
                )
            })
            .collect();
        let last = native_song("a1", "Marrow Choir", "O Holy Night", 263.4, None);
        first.push(last.clone());
        navi.answer(
            "/api/song",
            &[&format!("[{}]", first.join(",")), &format!("[{last}]")],
        );
        let groups = duplicates(&navi.client()).await.unwrap();
        assert!(groups.is_empty());
    }

    #[tokio::test]
    async fn two_different_recordings_of_one_song_are_not_the_same_recording() {
        let navi = NavidromeStub::start();
        let songs = [
            native_song(
                "a",
                "Marrow Choir",
                "Silent Night",
                222.0,
                Some("6a09041b-0f79-3278-88d9-0552dd28d37f"),
            ),
            native_song(
                "b",
                "Marrow Choir",
                "Silent Night",
                222.0,
                Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"),
            ),
            native_song("c", "Marrow Choir", "Away in a Manger", 180.0, None),
        ];
        navi.answer("/api/song", &[&format!("[{}]", songs.join(","))]);
        let groups = duplicates(&navi.client()).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].same_recording, Some(false));
        assert_eq!(song_requests(&navi).len(), 1, "a short page is the last");
    }

    #[tokio::test]
    async fn a_page_that_fails_fails_the_search_instead_of_answering_a_part() {
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "_start=0", 502, BAD_GATEWAY);
        let f = duplicates(&navi.client()).await.unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::NavidromeUnreachable,
                Stage::LibraryCheck,
                RetryAdvice::SameSourceLater
            )
        );
        assert_eq!(f.cause.map(|c| c.status), Some(Some(502)));
    }
}
