//! Read-only insights into a user's listening history and recommendation engine previews.
//! Neither operation downloads files or writes to `discovery_candidates` or `engine_reports`.

use std::collections::HashMap;
use std::path::Path;

use chrono::DateTime;
use serde::Serialize;
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use shared::recommendation::{
    Candidate, DiscoveryProfile, Listen, PlayedArtist, ProfileConfig, SourceError, SourceErrors,
    TimePeriod,
};
use soulbeet::engine::{build_profile, recommend};
use soulbeet::history::listenbrainz::ListenBrainzHistory;
use soulbeet::navidrome::models::NativeSong;
use soulbeet::ListeningHistory;
use sqlx::SqlitePool;

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::index::NavidromeOf;
use crate::core::library::{
    match_key, navidrome_failure, navidrome_for, FolderRef, FolderResolver,
};
use crate::core::recommend::{self, DiscoveryServices, RunSources};
use crate::core::time;
use crate::models::user_settings::UserSettings;

/// Default number of items returned per list when the caller omits `limit`.
pub(crate) const DEFAULT_LIMIT: usize = 20;
/// Maximum number of items returned per list.
pub(crate) const MAX_LIMIT: usize = 50;
/// Maximum number of seed artists included in a recommendation preview.
const SEED_ARTISTS_MAX: usize = 10;

/// Cut external text to `TEXT_MAX_CHARS` of JSON.
fn cut(value: String) -> String {
    cut_json(value, TEXT_MAX_CHARS)
}

/// Optional external text, cut as `cut` does; empty becomes `None`.
fn opt_text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| cut(value.to_string()))
}

/// Cut every error message in `errors` to `TEXT_MAX_CHARS`.
fn cut_errors(errors: Vec<SourceError>) -> Vec<SourceError> {
    errors
        .into_iter()
        .map(|e| SourceError {
            source: e.source,
            message: cut(e.message),
            failures: e.failures,
        })
        .collect()
}

/// A played song in `recent_plays` or `top_tracks`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayedSongEntry {
    pub song_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub play_count: u64,
    pub play_date: Option<String>,
    pub rating: u8,
    pub starred: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<FolderRef>,
}

impl PlayedSongEntry {
    fn from_native(song: &NativeSong, resolver: &FolderResolver) -> Self {
        Self {
            song_id: song.id.clone(),
            title: cut(song.title.clone()),
            artist: opt_text(&song.artist),
            album: opt_text(&song.album),
            play_count: song.play_count,
            play_date: song.play_date.map(time::at),
            rating: song.rating,
            starred: song.starred,
            folder: resolver.resolve_native(song),
        }
    }
}

/// An artist aggregated across the user's Navidrome library.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtistHistoryEntry {
    pub artist: String,
    pub play_count: u64,
    pub track_count: u32,
    pub starred_tracks: u32,
    pub avg_rating: Option<f64>,
}

/// A genre aggregated by play count across played songs in Navidrome.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GenreHistoryEntry {
    pub genre: String,
    pub play_count: u64,
    pub track_count: u32,
}

/// High-level totals for the user's Navidrome library and listening configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListeningSummary {
    pub total_tracks: u32,
    pub total_plays: u64,
    pub played_tracks: u32,
    pub starred_tracks: u32,
    pub rated_tracks: u32,
    pub listenbrainz_username: Option<String>,
}

/// A recent scrobble from ListenBrainz.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListenBrainzListen {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub listened_at: Option<String>,
}

impl From<Listen> for ListenBrainzListen {
    fn from(listen: Listen) -> Self {
        Self {
            artist: cut(listen.artist),
            track: cut(listen.track),
            album: listen.album.as_deref().and_then(opt_text),
            listened_at: DateTime::from_timestamp(listen.timestamp, 0).map(time::at),
        }
    }
}

/// A top artist from ListenBrainz.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListenBrainzArtist {
    pub artist: String,
    pub artist_mbid: Option<String>,
    pub play_count: u64,
}

impl From<PlayedArtist> for ListenBrainzArtist {
    fn from(played: PlayedArtist) -> Self {
        Self {
            artist: cut(played.artist.name),
            artist_mbid: played.artist.mbid,
            play_count: played.plays,
        }
    }
}

/// Optional ListenBrainz listening history when a username is configured and included.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListenBrainzSummary {
    pub username: String,
    pub recent_listens: Vec<ListenBrainzListen>,
    pub top_artists: Vec<ListenBrainzArtist>,
}

/// Complete read-only summary of a user's listening history and taste profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListeningHistoryReport {
    pub summary: ListeningSummary,
    pub recent_plays: Vec<PlayedSongEntry>,
    pub top_tracks: Vec<PlayedSongEntry>,
    pub top_artists: Vec<ArtistHistoryEntry>,
    pub top_genres: Vec<GenreHistoryEntry>,
    pub listenbrainz: Option<ListenBrainzSummary>,
    pub source_errors: Vec<SourceError>,
}

/// Main artist display name for a Navidrome song (preferring the first artist participant from
/// Navidrome 0.55+ so collaborations aggregate cleanly under their lead artist, falling back to
/// the song's display artist).
fn primary_artist_name(song: &NativeSong) -> &str {
    song.participants
        .artist
        .first()
        .map_or(song.artist.as_str(), |p| p.name.as_str())
}

/// Read-only summary of the caller's listening history from Navidrome and (optionally)
/// ListenBrainz. Never writes to the database and never downloads anything.
pub(crate) async fn listening_history_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    services: &DiscoveryServices,
    ctx: &UserCtx,
    music_path: Option<&Path>,
    limit: usize,
    include_listenbrainz: bool,
) -> Result<ListeningHistoryReport, Failure> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let plan = recommend::plan_for(&settings);

    let navi = navidrome_for(navidrome, ctx, Stage::LibraryCheck).await?;

    let (page_res, subsonic_res, resolver) = tokio::join!(
        navi.all_songs(),
        navi.search_all_songs(),
        FolderResolver::load_in(db, &ctx.user_id, music_path),
    );
    let page = page_res.map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;
    if (page.songs.len() as u64) < page.total {
        return Err(Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::LibraryCheck,
            RetryAdvice::SameSourceLater,
            format!(
                "Navidrome counts {} songs, and its pages held {}. Try again later.",
                page.total,
                page.songs.len()
            ),
        ));
    }

    let mut source_errors = SourceErrors::default();
    let genre_by_id: HashMap<String, String> = match subsonic_res {
        Ok(songs) => songs
            .into_iter()
            .filter_map(|s| {
                let genre = s.genre.as_deref().map(str::trim).filter(|g| !g.is_empty())?;
                Some((s.id, genre.to_string()))
            })
            .collect(),
        Err(e) => {
            source_errors.record("Navidrome genres", &e);
            HashMap::new()
        }
    };

    let songs = &page.songs;
    let mut total_plays = 0u64;
    let mut played_tracks = 0u32;
    let mut starred_tracks = 0u32;
    let mut rated_tracks = 0u32;

    struct ArtistAcc {
        display: String,
        play_count: u64,
        track_count: u32,
        starred_tracks: u32,
        rating_sum: u64,
        rated_count: u32,
    }
    struct GenreAcc {
        display: String,
        play_count: u64,
        track_count: u32,
    }

    let mut artists_map: HashMap<String, ArtistAcc> = HashMap::new();
    let mut genres_map: HashMap<String, GenreAcc> = HashMap::new();

    for song in songs {
        total_plays += song.play_count;
        if song.play_count > 0 {
            played_tracks += 1;
        }
        if song.starred {
            starred_tracks += 1;
        }
        let rated = (1..=5).contains(&song.rating);
        if rated {
            rated_tracks += 1;
        }

        let artist_name = primary_artist_name(song).trim();
        if !artist_name.is_empty() {
            let key = match_key(artist_name);
            let acc = artists_map.entry(key).or_insert_with(|| ArtistAcc {
                display: artist_name.to_string(),
                play_count: 0,
                track_count: 0,
                starred_tracks: 0,
                rating_sum: 0,
                rated_count: 0,
            });
            acc.play_count += song.play_count;
            acc.track_count += 1;
            if song.starred {
                acc.starred_tracks += 1;
            }
            if rated {
                acc.rating_sum += u64::from(song.rating);
                acc.rated_count += 1;
            }
        }

        if song.play_count > 0 {
            if let Some(genre) = genre_by_id.get(&song.id) {
                let key = genre.to_lowercase();
                let acc = genres_map.entry(key).or_insert_with(|| GenreAcc {
                    display: genre.clone(),
                    play_count: 0,
                    track_count: 0,
                });
                acc.play_count += song.play_count;
                acc.track_count += 1;
            }
        }
    }

    let mut recent: Vec<&NativeSong> = songs.iter().filter(|s| s.play_date.is_some()).collect();
    recent.sort_by(|a, b| {
        b.play_date
            .cmp(&a.play_date)
            .then_with(|| b.play_count.cmp(&a.play_count))
            .then_with(|| a.id.cmp(&b.id))
    });
    let recent_plays: Vec<PlayedSongEntry> = recent
        .into_iter()
        .take(limit)
        .map(|s| PlayedSongEntry::from_native(s, &resolver))
        .collect();

    let mut most_played: Vec<&NativeSong> = songs.iter().filter(|s| s.play_count > 0).collect();
    most_played.sort_by(|a, b| {
        b.play_count
            .cmp(&a.play_count)
            .then_with(|| b.play_date.cmp(&a.play_date))
            .then_with(|| a.id.cmp(&b.id))
    });
    let top_tracks: Vec<PlayedSongEntry> = most_played
        .into_iter()
        .take(limit)
        .map(|s| PlayedSongEntry::from_native(s, &resolver))
        .collect();

    let mut top_artists: Vec<ArtistHistoryEntry> = artists_map
        .into_values()
        .filter(|a| a.play_count > 0)
        .map(|a| {
            let avg_rating = (a.rated_count > 0)
                .then(|| ((a.rating_sum as f64 / f64::from(a.rated_count)) * 100.0).round() / 100.0);
            ArtistHistoryEntry {
                artist: cut(a.display),
                play_count: a.play_count,
                track_count: a.track_count,
                starred_tracks: a.starred_tracks,
                avg_rating,
            }
        })
        .collect();
    top_artists.sort_by(|a, b| {
        b.play_count
            .cmp(&a.play_count)
            .then_with(|| b.starred_tracks.cmp(&a.starred_tracks))
            .then_with(|| a.artist.cmp(&b.artist))
    });
    top_artists.truncate(limit);

    let mut top_genres: Vec<GenreHistoryEntry> = genres_map
        .into_values()
        .map(|g| GenreHistoryEntry {
            genre: cut(g.display),
            play_count: g.play_count,
            track_count: g.track_count,
        })
        .collect();
    top_genres.sort_by(|a, b| {
        b.play_count
            .cmp(&a.play_count)
            .then_with(|| b.track_count.cmp(&a.track_count))
            .then_with(|| a.genre.cmp(&b.genre))
    });
    top_genres.truncate(limit);

    let mut listenbrainz = None;
    if include_listenbrainz {
        if let Some(lb_user) = &plan.listenbrainz_user {
            match recommend::listenbrainz_for(services, plan.listenbrainz_token.clone()).await {
                Ok((lb_client, lb_setup_errors)) => {
                    source_errors.extend(lb_setup_errors);
                    let lb_history = ListenBrainzHistory::new(lb_client, lb_user.clone());
                    let (listens_res, artists_res) = tokio::join!(
                        lb_history.listens(limit),
                        lb_history.top_artists(TimePeriod::AllTime, limit),
                    );
                    let listens_ok = listens_res.is_ok();
                    let artists_ok = artists_res.is_ok();
                    let recent_listens = match listens_res {
                        Ok(list) => list.into_iter().map(ListenBrainzListen::from).collect(),
                        Err(e) => {
                            source_errors.record(e.service(), &e);
                            Vec::new()
                        }
                    };
                    let lb_top_artists = match artists_res {
                        Ok(list) => list.into_iter().map(ListenBrainzArtist::from).collect(),
                        Err(e) => {
                            source_errors.record(e.service(), &e);
                            Vec::new()
                        }
                    };
                    if listens_ok || artists_ok {
                        listenbrainz = Some(ListenBrainzSummary {
                            username: cut(lb_user.clone()),
                            recent_listens,
                            top_artists: lb_top_artists,
                        });
                    }
                }
                Err(e) => source_errors.record(e.service(), &e),
            }
        }
    }

    Ok(ListeningHistoryReport {
        summary: ListeningSummary {
            total_tracks: songs.len() as u32,
            total_plays,
            played_tracks,
            starred_tracks,
            rated_tracks,
            listenbrainz_username: plan.listenbrainz_user.map(cut),
        },
        recent_plays,
        top_tracks,
        top_artists,
        top_genres,
        listenbrainz,
        source_errors: cut_errors(source_errors.into_vec()),
    })
}

/// A seed artist from the user's listening profile that drove the recommendations.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeedArtist {
    pub name: String,
    pub mbid: Option<String>,
    pub plays: u64,
}

/// A ranked recommendation candidate returned by `recommendations_preview_in`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreviewCandidate {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub recording_mbid: Option<String>,
    pub artist_mbid: Option<String>,
    pub score: f64,
    pub source: String,
    pub signals: Vec<String>,
    pub primary_genre: Option<String>,
    pub release_year: Option<u16>,
    pub artist_listener_count: Option<u64>,
}

impl From<Candidate> for PreviewCandidate {
    fn from(c: Candidate) -> Self {
        Self {
            artist: cut(c.artist),
            title: cut(c.track),
            album: c.album.as_deref().and_then(opt_text),
            recording_mbid: c.mbid,
            artist_mbid: c.artist_mbid,
            score: (c.score * 1000.0).round() / 1000.0,
            source: cut(c.source),
            signals: c.signals.into_iter().map(cut).collect(),
            primary_genre: c.primary_genre.as_deref().and_then(opt_text),
            release_year: c.release_year,
            artist_listener_count: c.artist_listener_count,
        }
    }
}

/// Read-only preview of Soulbeet's recommendation engine for one discovery profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecommendationPreview {
    pub profile: String,
    pub sources: Vec<String>,
    pub source_errors: Vec<SourceError>,
    pub seed_artists: Vec<SeedArtist>,
    pub candidates: Vec<PreviewCandidate>,
}

/// Runs Soulbeet's recommendation engine in read-only preview mode for `profile`.
/// Never downloads files and never writes to `discovery_candidates` or `engine_reports`.
pub(crate) async fn recommendations_preview_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    services: &DiscoveryServices,
    user: &UserId,
    profile: DiscoveryProfile,
    limit: usize,
) -> Result<RecommendationPreview, Failure> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let user_id = user.to_string();
    let settings = UserSettings::get_in(db, &user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let RunSources {
        histories,
        generators,
        knowledge,
        errors,
    } = recommend::sources_of(navidrome, services, user, recommend::plan_for(&settings))
        .await
        .map_err(|(_, failure)| failure)?;

    let build = build_profile(&histories, knowledge.as_ref()).await;
    if !build.profile.has_seeds() {
        return Err(recommend::no_seeds(build.errors.into_vec()));
    }

    let config = ProfileConfig::from_profile(profile);
    let (candidates, report) =
        recommend(&build, &generators, knowledge.as_ref(), &config, limit).await;

    let mut all_errors = errors;
    all_errors.extend(report.source_errors.into());

    let seed_artists: Vec<SeedArtist> = if !build.profile.seed_artists.is_empty() {
        build
            .profile
            .seed_artists
            .iter()
            .take(SEED_ARTISTS_MAX)
            .map(|p| SeedArtist {
                name: cut(p.artist.name.clone()),
                mbid: p.artist.mbid.clone(),
                plays: p.plays,
            })
            .collect()
    } else {
        build
            .profile
            .favourite_artists
            .iter()
            .take(SEED_ARTISTS_MAX)
            .map(|a| SeedArtist {
                name: cut(a.name.clone()),
                mbid: a.mbid.clone(),
                plays: 0,
            })
            .collect()
    };

    Ok(RecommendationPreview {
        profile: profile.name().to_string(),
        sources: report.sources,
        source_errors: cut_errors(all_errors.into_vec()),
        seed_artists,
        candidates: candidates
            .into_iter()
            .take(limit)
            .map(PreviewCandidate::from)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use shared::failure::{FailureCode, UserActionCode};
    use soulbeet::knowledge::KnowledgeHosts;
    use soulbeet::polite::Pace;
    use soulbeet::traits::MusicBrainzLookup;

    use super::*;
    use crate::core::test_ctx;
    use crate::db::test_db;
    use crate::models::knowledge_cache::SqliteKnowledgeCache;
    use crate::models::user_settings::UpdateUserSettings;
    use crate::test_navidrome::{subsonic_ok, NavidromeFor, NavidromeStub, BAD_GATEWAY};

    const UNREACHED: &str = "http://127.0.0.1:9";
    const SH: &str = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    const VN: &str = "2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b";
    const REC1: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

    struct StubMusicBrainz;

    #[async_trait::async_trait]
    impl MusicBrainzLookup for StubMusicBrainz {
        async fn search_artists(
            &self,
            _: &str,
            _: u8,
        ) -> Result<Vec<soulbeet::musicbrainz::ArtistMatch>, soulbeet::MetadataError> {
            Ok(Vec::new())
        }

        async fn genre_names(&self) -> Result<Vec<String>, soulbeet::MetadataError> {
            Ok(vec!["dream pop".into(), "shoegaze".into()])
        }
    }

    fn services_at(db: &SqlitePool, listenbrainz: &str, labs: &str) -> DiscoveryServices {
        let pace = Pace {
            min_interval: Duration::ZERO,
            backoff: Duration::ZERO,
            retries: 0,
            timeout: Duration::from_secs(5),
        };
        DiscoveryServices {
            hosts: KnowledgeHosts {
                listenbrainz: listenbrainz.into(),
                labs: labs.into(),
                deezer: UNREACHED.into(),
            },
            cache: Arc::new(SqliteKnowledgeCache::new(db.clone())),
            musicbrainz: Arc::new(StubMusicBrainz),
            metabrainz_pace: pace,
            deezer_pace: pace,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn song_json(
        id: &str,
        artist: &str,
        title: &str,
        album: &str,
        path: &str,
        library_path: &str,
        play_count: u64,
        play_date: Option<&str>,
        rating: u8,
        starred: bool,
    ) -> serde_json::Value {
        let mut s = json!({
            "id": id,
            "path": path,
            "title": title,
            "size": 1024,
            "libraryId": 1,
            "libraryPath": library_path,
            "missing": false,
            "updatedAt": "2026-11-15T10:00:00Z",
            "artist": artist,
            "album": album,
            "albumArtist": artist,
            "playCount": play_count,
            "rating": rating,
            "starred": starred,
        });
        if let Some(d) = play_date {
            s["playDate"] = json!(d);
        }
        s
    }

    #[tokio::test]
    async fn listening_history_aggregates_plays_artists_genres_and_skips_listenbrainz_when_disabled() {
        let db = test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                listenbrainz_username: Some("alice_lb".into()),
                listenbrainz_token: Some("secret-lb-token".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let songs = json!([
            song_json(
                "s1", "Static Harbor", "Northern Line", "Winter Songs",
                "alice/Static Harbor/01.flac", "/music",
                10, Some("2026-11-18T20:00:00Z"), 5, true,
            ),
            song_json(
                "s2", "Static Harbor", "Undertow", "Winter Songs",
                "alice/Static Harbor/02.flac", "/music",
                4, Some("2026-11-20T09:30:00Z"), 3, false,
            ),
            song_json(
                "s3", "Vela Norte", "Foglight", "Harbour",
                "alice/Vela Norte/01.flac", "/music",
                12, Some("2026-11-19T12:00:00Z"), 4, true,
            ),
            song_json(
                "s4", "Marrow Choir", "Silent Night", "Carols",
                "alice/Marrow Choir/01.flac", "/music",
                0, None, 0, true,
            ),
        ]);

        let subsonic_songs = subsonic_ok(
            r#""searchResult3":{"song":[
                {"id":"s1","title":"Northern Line","genre":"Dream Pop"},
                {"id":"s2","title":"Undertow","genre":"dream pop"},
                {"id":"s3","title":"Foglight","genre":"Shoegaze"},
                {"id":"s4","title":"Silent Night","genre":"Choral"}
            ]}"#,
        );

        let navi = NavidromeStub::start();
        navi.answer("/api/song", &[&songs.to_string()]);
        navi.header("/api/song", "X-Total-Count", "4");
        navi.answer("/rest/search3", &[&subsonic_songs]);

        let report = listening_history_in(
            &db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(&db, UNREACHED, UNREACHED),
            &test_ctx("alice"),
            None,
            10,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            report.summary,
            ListeningSummary {
                total_tracks: 4,
                total_plays: 26,
                played_tracks: 3,
                starred_tracks: 3,
                rated_tracks: 3,
                listenbrainz_username: Some("alice_lb".into()),
            }
        );

        // recent_plays: newest play_date first (s2 -> s3 -> s1; s4 has no play_date)
        let recent_ids: Vec<&str> = report
            .recent_plays
            .iter()
            .map(|s| s.song_id.as_str())
            .collect();
        assert_eq!(recent_ids, ["s2", "s3", "s1"]);
        assert_eq!(
            report.recent_plays[0].play_date.as_deref(),
            Some("2026-11-20T09:30:00Z")
        );
        assert_eq!(
            report.recent_plays[0]
                .folder
                .as_ref()
                .map(|f| f.folder_id.as_str()),
            Some(folder_id.as_str())
        );

        // top_tracks: play_count desc (s3:12 -> s1:10 -> s2:4)
        let top_ids: Vec<&str> = report
            .top_tracks
            .iter()
            .map(|s| s.song_id.as_str())
            .collect();
        assert_eq!(top_ids, ["s3", "s1", "s2"]);

        // top_artists: Static Harbor (14 plays, 2 tracks, 1 starred, avg 4.0) before Vela Norte (12 plays, 1 track, 1 starred, avg 4.0)
        assert_eq!(
            report.top_artists,
            vec![
                ArtistHistoryEntry {
                    artist: "Static Harbor".into(),
                    play_count: 14,
                    track_count: 2,
                    starred_tracks: 1,
                    avg_rating: Some(4.0),
                },
                ArtistHistoryEntry {
                    artist: "Vela Norte".into(),
                    play_count: 12,
                    track_count: 1,
                    starred_tracks: 1,
                    avg_rating: Some(4.0),
                },
            ]
        );

        // top_genres: Dream Pop (14 plays, 2 tracks) before Shoegaze (12 plays, 1 track); Choral has 0 plays
        assert_eq!(
            report.top_genres,
            vec![
                GenreHistoryEntry {
                    genre: "Dream Pop".into(),
                    play_count: 14,
                    track_count: 2,
                },
                GenreHistoryEntry {
                    genre: "Shoegaze".into(),
                    play_count: 12,
                    track_count: 1,
                },
            ]
        );

        // include_listenbrainz: false did not contact ListenBrainz and reported no errors
        assert!(report.listenbrainz.is_none());
        assert!(report.source_errors.is_empty());
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("secret-lb-token"));
    }

    #[tokio::test]
    async fn listening_history_keeps_navidrome_data_when_listenbrainz_fails() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                listenbrainz_username: Some("alice_lb".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let songs = json!([song_json(
            "s1",
            "Static Harbor",
            "Northern Line",
            "Winter Songs",
            "alice/01.flac",
            "/music",
            5,
            Some("2026-11-18T20:00:00Z"),
            5,
            true,
        )]);
        let navi = NavidromeStub::start();
        navi.answer("/api/song", &[&songs.to_string()]);
        navi.header("/api/song", "X-Total-Count", "1");
        navi.answer("/rest/search3", &[&subsonic_ok(r#""searchResult3":{}"#)]);

        let lb = NavidromeStub::start();
        lb.answer_when("/1/user/alice_lb/listens", "", 502, BAD_GATEWAY);
        lb.answer_when("/1/stats/user/alice_lb/artists", "", 502, BAD_GATEWAY);

        let report = listening_history_in(
            &db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(&db, &lb.url, UNREACHED),
            &test_ctx("alice"),
            None,
            10,
            true,
        )
        .await
        .unwrap();

        assert_eq!(report.summary.total_plays, 5);
        assert_eq!(report.recent_plays.len(), 1);
        assert!(report.listenbrainz.is_none());
        assert_eq!(report.source_errors.len(), 1);
        assert_eq!(report.source_errors[0].source, "ListenBrainz");
    }

    #[tokio::test]
    async fn recommendations_preview_returns_no_listening_data_on_empty_library() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;

        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        navi.header("/api/song", "X-Total-Count", "0");
        navi.answer("/api/artist", &["[]"]);

        let failure = recommendations_preview_in(
            &db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(&db, UNREACHED, UNREACHED),
            &UserId::new("alice"),
            DiscoveryProfile::Balanced,
            20,
        )
        .await
        .unwrap_err();

        assert_eq!(failure.code, FailureCode::NoListeningData);
        assert_eq!(
            failure.user_action.as_ref().map(|a| a.code),
            Some(UserActionCode::ListenInNavidrome)
        );
    }

    #[tokio::test]
    async fn recommendations_preview_returns_ranked_candidates_without_writing_discovery_tables() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;

        let mut played = song_json(
            "s1",
            "Static Harbor",
            "Northern Line",
            "Winter Songs",
            "alice/01.flac",
            "/music",
            7,
            Some("2026-11-18T20:00:00Z"),
            5,
            true,
        );
        played["participants"] = json!({
            "artist": [{"id": "ar-sh", "name": "Static Harbor"}],
            "albumartist": [{"id": "ar-sh", "name": "Static Harbor"}],
        });

        let navi = NavidromeStub::start();
        navi.answer("/api/song", &[&json!([played]).to_string()]);
        navi.header("/api/song", "X-Total-Count", "1");
        navi.answer(
            "/api/artist",
            &[&json!([{"id": "ar-sh", "name": "Static Harbor", "mbzArtistId": SH}]).to_string()],
        );

        let lb = NavidromeStub::start();
        lb.answer_when(
            "/1/metadata/artist/",
            SH,
            200,
            &json!([{"artist_mbid": SH, "tag": {"artist": [{"tag": "dream pop", "count": 10}]}}])
                .to_string(),
        );
        lb.answer_when(
            "/1/metadata/artist/",
            VN,
            200,
            &json!([{"artist_mbid": VN, "tag": {"artist": [{"tag": "shoegaze", "count": 8}]}}])
                .to_string(),
        );
        lb.answer(
            "/1/popularity/artist",
            &[&json!([
                {"artist_mbid": SH, "total_user_count": 500},
                {"artist_mbid": VN, "total_user_count": 300}
            ])
            .to_string()],
        );
        lb.answer(
            "/1/stats/sitewide/artists",
            &[&json!({"payload": {"artists": [{"artist_mbid": SH, "artist_name": "Static Harbor"}]}})
                .to_string()],
        );
        lb.answer(
            &format!("/1/lb-radio/artist/{VN}"),
            &[&json!({VN: [{"recording_mbid": REC1, "total_listen_count": 420}]}).to_string()],
        );
        lb.answer(
            "/1/metadata/recording/",
            &[&json!({
                REC1: {
                    "artist": {"name": "Vela Norte", "artists": [{"artist_mbid": VN}]},
                    "recording": {"name": "Foglight", "first_release_date": "2025-05-01"}
                }
            })
            .to_string()],
        );

        let labs = NavidromeStub::start();
        labs.answer("/tag-similarity/json", &["[]"]);
        labs.answer_when(
            "/similar-artists/json",
            SH,
            200,
            &json!([{"artist_mbid": VN, "name": "Vela Norte", "score": 1000}]).to_string(),
        );
        labs.answer_when("/similar-artists/json", VN, 200, "[]");

        let preview = recommendations_preview_in(
            &db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(&db, &lb.url, &labs.url),
            &UserId::new("alice"),
            DiscoveryProfile::Balanced,
            10,
        )
        .await
        .unwrap();

        assert_eq!(preview.profile, "Balanced");
        assert!(preview.sources.contains(&"Navidrome".to_string()));
        assert!(preview
            .sources
            .contains(&"ListenBrainz similar artists".to_string()));
        assert!(preview.source_errors.is_empty(), "{:?}", preview.source_errors);
        assert_eq!(
            preview.seed_artists,
            vec![SeedArtist {
                name: "Static Harbor".into(),
                mbid: Some(SH.into()),
                plays: 7,
            }]
        );
        assert_eq!(preview.candidates.len(), 1);
        let c = &preview.candidates[0];
        assert_eq!(
            (
                c.artist.as_str(),
                c.title.as_str(),
                c.recording_mbid.as_deref(),
                c.artist_mbid.as_deref(),
                c.primary_genre.as_deref(),
                c.release_year,
                c.artist_listener_count,
            ),
            (
                "Vela Norte",
                "Foglight",
                Some(REC1),
                Some(VN),
                Some("shoegaze"),
                Some(2025),
                Some(300),
            )
        );
        assert!(c.signals.contains(&"artist_hop1".to_string()));

        // Verify strictly read-only: no writes to discovery_candidates or engine_reports.
        let (candidate_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM discovery_candidates")
            .fetch_one(&db)
            .await
            .unwrap();
        let (report_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM engine_reports")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!((candidate_rows, report_rows), (0, 0));
    }
}
