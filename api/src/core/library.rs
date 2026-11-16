//! What the caller's Navidrome library holds, as the tools report it, and
//! whether imports into a folder show up there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use shared::failure::{cut_chars, Failure, Stage, TEXT_MAX_CHARS};
use shared::navidrome::LibraryStats;
use soulbeet::navidrome::models::{NativeSong, SubsonicAlbum, SubsonicArtist, SubsonicSong};
use soulbeet::navidrome::NavidromeClient;
use soulbeet::NavidromeError;
use sqlx::SqlitePool;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::index::NavidromeOf;
use crate::models::navidrome_library::NavidromeLibraryRow;

/// A song as the tools report it, from a native record or a Subsonic child
/// (D7). The rating, star and play count are the caller's own; Navidrome
/// sends 0 for "none" (model/annotation.go:6-12), and so does this.
/// `path` is relative to the song's library, and only a native record has
/// it: a Subsonic child's is made up, or absolute with ReportRealPath
/// (server/subsonic/helpers.go:227-231).
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// A text field taken from Navidrome, cut to TEXT_MAX_CHARS; empty is unknown.
fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| cut_chars(value.to_string(), TEXT_MAX_CHARS))
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
            title: cut_chars(song.title.clone(), TEXT_MAX_CHARS),
            artist: text(&song.artist),
            album: text(&song.album),
            album_id: given(Some(&song.album_id)),
            year: known(song.year),
            track: known(song.track_number),
            duration_s: (song.duration > 0.0).then(|| song.duration.round() as u32),
            format: text(&song.suffix),
            bitrate_kbps: known(song.bit_rate),
            size_bytes: Some(song.size),
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
            title: cut_chars(song.title.clone(), TEXT_MAX_CHARS),
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
            name: cut_chars(album.display_name().to_string(), TEXT_MAX_CHARS),
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
            name: cut_chars(artist.name.clone(), TEXT_MAX_CHARS),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::models::navidrome_library::NavidromeLibraryRow;
    use crate::test_navidrome::{child, native_song, subsonic_failed, subsonic_ok, NavidromeStub};
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
        assert_eq!(f.title.chars().count(), 500);
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
}
