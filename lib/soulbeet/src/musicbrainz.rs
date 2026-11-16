use musicbrainz_rs::{
    entity::{
        artist::{Artist, ArtistSearchQuery, ArtistType},
        artist_credit::ArtistCredit,
        recording::{Recording, RecordingSearchQuery},
        release::{Release, ReleaseStatus},
        release_group::{
            ReleaseGroup, ReleaseGroupPrimaryType, ReleaseGroupSearchQuery,
            ReleaseGroupSecondaryType,
        },
        BrowseResult,
    },
    Browse, Fetch, MusicBrainzClient, Search,
};
use serde::Serialize;
use shared::failure::Service;
use shared::metadata::{Album, AlbumWithTracks, Provider, SearchResult, Track};
use std::{collections::HashSet, future::Future, sync::OnceLock, time::Duration};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::MetadataError;

/// Timeout for individual MusicBrainz requests (15 seconds)
const REQUEST_TIMEOUT_SECS: u64 = 15;

/// Maximum retries for transient errors
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_DELAY_MS: u64 = 500;

/// Maximum backoff delay cap (milliseconds)
const MAX_BACKOFF_MS: u64 = 5000;

// This ensures the client is initialized only once with a proper user agent.
fn musicbrainz_client() -> &'static MusicBrainzClient {
    static CLIENT: OnceLock<MusicBrainzClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let mut client = MusicBrainzClient::new(&format!(
            "Soulbeet/{version} ( https://github.com/terry90/soulbeet )"
        ))
        .expect("Failed to create MusicBrainz client - invalid user agent format");
        // MUSICBRAINZ_HOST points the client at a MusicBrainz mirror
        // (host[:port], no scheme). Mirrors the `musicbrainz.host` option
        // beets exposes for the same purpose.
        if let Ok(host) = std::env::var("MUSICBRAINZ_HOST") {
            let host = host.trim();
            if !host.is_empty() {
                client.musicbrainz_domain = host.to_string();
            }
        }
        client
    })
}

/// Formats the artist credits into a single, comma-separated string.
fn format_artist_credit(credits: &Option<Vec<ArtistCredit>>) -> String {
    credits
        .as_ref()
        .map(|credits| {
            credits
                .iter()
                .map(|credit| credit.name.clone())
                .collect::<Vec<String>>()
                .join(", ")
        })
        .unwrap_or_else(|| "Unknown Artist".to_string())
}

/// Formats a duration from milliseconds to a MM:SS string.
fn format_duration(duration_ms: &Option<u32>) -> Option<String> {
    duration_ms.map(|ms| {
        let total_seconds = ms / 1000;
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes:02}:{seconds:02}")
    })
}

impl From<musicbrainz_rs::Error> for MetadataError {
    fn from(e: musicbrainz_rs::Error) -> Self {
        use musicbrainz_rs::Error;
        let service = Service::Musicbrainz;
        match e {
            Error::NotFound(_) => MetadataError::NotFound {
                service,
                status: None,
                raw: e.to_string(),
            },
            // MusicBrainz's own `{error, help}` body, other than "Not Found"
            // (musicbrainz_rs `src/entity/api.rs:8-24`).
            Error::MusicbrainzError(query, body) => MetadataError::Rejected {
                service,
                status: None,
                raw: format!(
                    "{query}: {}",
                    serde_json::to_string(&body).expect("two strings serialize")
                ),
            },
            // musicbrainz_rs retries MusicBrainz's 503 rate-limit answer
            // (`src/config.rs:9`, `src/fetching.rs:33-59`) until it gives up.
            Error::MaxRetriesExceeded => MetadataError::RateLimited {
                service,
                status: None,
                raw: e.to_string(),
            },
            Error::ReqwestError(e) => MetadataError::Unavailable {
                service,
                status: e.status().map(|s| s.as_u16()),
                raw: e.to_string(),
            },
            Error::SerdeJsonError(e) => MetadataError::Unreadable {
                service,
                status: None,
                raw: e.to_string(),
            },
            Error::MissingRetryToken | Error::InvalidUserAgent(_) => unreachable!(
                "musicbrainz_rs 44c25c8 never constructs MissingRetryToken, and gives \
                 InvalidUserAgent only from MusicBrainzClient::new, which Soulful calls \
                 once under expect: {e}"
            ),
        }
    }
}

/// Retries an async operation with exponential backoff and request timeout.
/// Only retries transient errors (`MetadataError::is_transient`): no answer
/// within REQUEST_TIMEOUT_SECS, or an HTTP exchange that failed.
async fn with_retry<T, F, Fut>(operation_name: &str, mut operation: F) -> Result<T, MetadataError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, musicbrainz_rs::Error>>,
{
    let mut attempt = 0;
    loop {
        // Respect MusicBrainz rate limit (1 req/sec)
        crate::http::mb_rate_limit().await;

        let result =
            tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), operation()).await;
        let e = match result {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) => MetadataError::from(e),
            Err(_timeout) => MetadataError::Unavailable {
                service: Service::Musicbrainz,
                status: None,
                raw: format!("no answer within {REQUEST_TIMEOUT_SECS} s"),
            },
        };
        attempt += 1;
        if !e.is_transient() || attempt == MAX_RETRIES {
            warn!(
                "{} failed (attempt {}/{}): {}",
                operation_name, attempt, MAX_RETRIES, e
            );
            return Err(e);
        }
        let delay = std::cmp::min(BASE_DELAY_MS * 2u64.pow(attempt - 1), MAX_BACKOFF_MS);
        warn!(
            "{} failed (attempt {}/{}), retrying in {}ms: {}",
            operation_name, attempt, MAX_RETRIES, delay, e
        );
        sleep(Duration::from_millis(delay)).await;
    }
}

/// An enumeration to specify the type of search.
#[derive(Debug)]
pub enum SearchType {
    Track,
    Album,
}

/// Performs a refined search for music, prioritizing canonical releases.
pub async fn search(
    artist: &Option<String>,
    query: &str,
    search_type: SearchType,
    limit: u8,
) -> Result<Vec<SearchResult>, MetadataError> {
    let client = musicbrainz_client();
    let mut results = Vec::new();

    info!(
        "Starting {:?} search for query: '{}', artist: '{:?}'",
        search_type, query, artist
    );

    match search_type {
        SearchType::Track => {
            let search_results = with_retry("MusicBrainz track search", || {
                let mut recording_query = RecordingSearchQuery::query_builder();
                if let Some(ref artist) = artist {
                    recording_query.artist_name(artist).and();
                }
                let search_query = recording_query.recording(query).build();
                async move {
                    Recording::search(search_query)
                        .limit(limit)
                        .with_releases()
                        .with_ratings()
                        .execute_with_client(client)
                        .await
                }
            })
            .await?;

            // Sort by rating (descending) - higher rated recordings first
            let mut recordings: Vec<_> = search_results.entities;
            recordings.sort_by(|a, b| {
                let a_rating = a.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                let b_rating = b.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                b_rating
                    .partial_cmp(&a_rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut unique_tracks = HashSet::new();

            for recording in recordings {
                let artist_name = format_artist_credit(&recording.artist_credit);
                let album_title = recording
                    .releases
                    .as_ref()
                    .and_then(|r| r.first())
                    .map(|r| r.title.clone())
                    .unwrap_or_default();

                // Use a combination of title, artist, and album to define a unique track
                let key = (
                    recording.title.to_lowercase(),
                    artist_name.to_lowercase(),
                    album_title.to_lowercase(),
                );

                if !unique_tracks.contains(&key) {
                    let first_release = recording.releases.as_ref().and_then(|r| r.first());
                    let track = Track {
                        id: recording.id.clone(),
                        title: recording.title.clone(),
                        artist: artist_name.clone(),
                        album_id: first_release.map(|release| release.id.clone()),
                        album_title: first_release.map(|r| r.title.clone()),
                        release_date: first_release.and_then(|r| r.date.clone().map(|d| d.0)),
                        duration: format_duration(&recording.length),
                        mbid: Some(recording.id.clone()),
                        release_mbid: first_release.map(|r| r.id.clone()),
                    };
                    unique_tracks.insert(key);
                    results.push(SearchResult::Track(track));
                }
            }
        }
        SearchType::Album => {
            let search_results = with_retry("MusicBrainz album search", || {
                let mut album_query = ReleaseGroupSearchQuery::query_builder();
                if let Some(ref artist) = artist {
                    album_query.artist(artist).and();
                }
                let search_query = album_query.release_group(query).build();
                async move {
                    ReleaseGroup::search(search_query)
                        .limit(limit)
                        .with_releases()
                        .with_ratings()
                        .execute_with_client(client)
                        .await
                }
            })
            .await?;

            // Sort by rating (descending) - higher rated albums first
            let mut release_groups: Vec<_> = search_results.entities;
            release_groups.sort_by(|a, b| {
                let a_rating = a.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                let b_rating = b.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                b_rating
                    .partial_cmp(&a_rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            for release_group in release_groups {
                if release_group.primary_type != Some(ReleaseGroupPrimaryType::Album)
                    && release_group.primary_type != Some(ReleaseGroupPrimaryType::Ep)
                {
                    continue;
                }

                if let Some(best_release) = release_group.releases.as_ref().and_then(|releases| {
                    releases
                        .iter()
                        .filter(|r| r.status == Some(ReleaseStatus::Official))
                        .min_by_key(|release| release.date.as_ref().map(|d| &d.0))
                }) {
                    // If no official release was found, take the first one available
                    let final_release = best_release.clone();

                    results.push(SearchResult::Album(Album {
                        id: final_release.id.clone(),
                        title: release_group.title.clone(),
                        artist: format_artist_credit(&release_group.artist_credit),
                        release_date: final_release.date.as_ref().map(|d| d.0.clone()),
                        mbid: Some(final_release.id.clone()),
                        cover_url: None,
                        release_types: release_types(&release_group),
                    }));
                }
            }
        }
    }

    Ok(results)
}

/// Fetches a release (album) by its ID and returns it with its full tracklist.
pub async fn find_album(release_id: &str) -> Result<AlbumWithTracks, MetadataError> {
    let client = musicbrainz_client();

    // Fetch the release with recordings (tracks), artist credits for the
    // tracks and its release group for the release types.
    let release = with_retry("MusicBrainz album fetch", || async {
        Release::fetch()
            .id(release_id)
            .with_recordings()
            .with_artist_credits()
            .with_release_groups()
            .execute_with_client(client)
            .await
    })
    .await?;

    Ok(album_from_release(release))
}

/// `find_album`'s answer from the release it fetched.
fn album_from_release(release: Release) -> AlbumWithTracks {
    let mut tracks = Vec::new();

    // A release contains media (like CD 1, CD 2), and each medium has tracks.
    if let Some(media) = &release.media {
        for medium in media {
            if let Some(release_tracks) = &medium.tracks {
                for track in release_tracks {
                    if let Some(recording) = &track.recording {
                        tracks.push(Track {
                            id: recording.id.clone(),
                            title: recording.title.clone(),
                            artist: format_artist_credit(&recording.artist_credit),
                            album_id: Some(release.id.clone()),
                            album_title: Some(release.title.clone()),
                            release_date: release.date.as_ref().map(|d| d.0.clone()),
                            duration: format_duration(&recording.length),
                            mbid: Some(recording.id.clone()),
                            release_mbid: Some(release.id.clone()),
                        });
                    }
                }
            }
        }
    }

    // First, create the standalone Album object.
    let album = Album {
        id: release.id.clone(),
        title: release.title,
        artist: format_artist_credit(&release.artist_credit),
        release_date: release.date.map(|d| d.0),
        mbid: Some(release.id),
        cover_url: None,
        release_types: release.release_group.as_ref().and_then(release_types),
    };

    AlbumWithTracks { album, tracks }
}

/// An artist a MusicBrainz artist search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtistMatch {
    pub mbid: String,
    pub name: String,
    pub sort_name: String,
    pub disambiguation: Option<String>,
    /// MusicBrainz's name for the artist type; None when it gives none or
    /// one musicbrainz_rs does not know.
    pub artist_type: Option<String>,
    pub country: Option<String>,
    pub begin: Option<String>,
    pub end: Option<String>,
}

/// The recording a recording's releases were looked up for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingInfo {
    pub mbid: String,
    pub title: String,
    pub artist: String,
    pub duration_s: Option<u32>,
    pub disambiguation: Option<String>,
    pub first_release_date: Option<String>,
}

/// The release group a release belongs to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReleaseGroupInfo {
    pub mbid: String,
    pub title: String,
    /// As `release_types` gives them.
    pub release_types: Option<Vec<String>>,
    pub first_release_date: Option<String>,
}

/// A release a recording is on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingRelease {
    pub mbid: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    /// MusicBrainz's name for the release status; None when it gives none or
    /// one musicbrainz_rs does not know.
    pub status: Option<String>,
    pub release_group: Option<ReleaseGroupInfo>,
}

/// A recording and one page of the releases it is on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingReleases {
    pub recording: RecordingInfo,
    /// Every release the recording is on, not only this page's.
    pub release_count: u32,
    pub offset: u32,
    pub releases: Vec<RecordingRelease>,
}

/// Searches MusicBrainz for artists by name.
pub async fn search_artists(query: &str, limit: u8) -> Result<Vec<ArtistMatch>, MetadataError> {
    let client = musicbrainz_client();
    let found = with_retry("MusicBrainz artist search", || {
        let search_query = ArtistSearchQuery::query_builder().artist(query).build();
        async move {
            Artist::search(search_query)
                .limit(limit)
                .execute_with_client(client)
                .await
        }
    })
    .await?;
    Ok(found.entities.into_iter().map(artist_match).collect())
}

/// The recording `mbid` and the page of releases it is on that starts at
/// `offset`. MusicBrainz answers at most 100 releases per page
/// (musicbrainz.org/doc/MusicBrainz_API "Browse").
pub async fn recording_releases(
    mbid: &str,
    offset: u16,
    limit: u8,
) -> Result<RecordingReleases, MetadataError> {
    let client = musicbrainz_client();
    let recording = with_retry("MusicBrainz recording fetch", || async {
        Recording::fetch()
            .id(mbid)
            .with_artists()
            .execute_with_client(client)
            .await
    })
    .await?;
    let page = with_retry("MusicBrainz recording releases", || async {
        Release::browse()
            .by_recording(mbid)
            .with_release_groups()
            .limit(limit)
            .offset(offset)
            .execute_with_client(client)
            .await
    })
    .await?;
    recording_releases_from(recording, page)
}

fn artist_match(a: Artist) -> ArtistMatch {
    let life_span = a.life_span.unwrap_or_default();
    ArtistMatch {
        mbid: a.id,
        name: a.name,
        sort_name: a.sort_name,
        disambiguation: given(Some(a.disambiguation)),
        artist_type: a
            .artist_type
            .filter(|t| *t != ArtistType::UnrecognizedArtistType)
            .map(|t| serde_name(&t)),
        country: given(a.country),
        begin: given(life_span.begin.map(|d| d.0)),
        end: given(life_span.end.map(|d| d.0)),
    }
}

fn recording_releases_from(
    recording: Recording,
    page: BrowseResult<Release>,
) -> Result<RecordingReleases, MetadataError> {
    let (Ok(release_count), Ok(offset)) = (u32::try_from(page.count), u32::try_from(page.offset))
    else {
        return Err(MetadataError::Unreadable {
            service: Service::Musicbrainz,
            status: None,
            raw: "MusicBrainz answered a negative release count or offset".into(),
        });
    };
    Ok(RecordingReleases {
        recording: RecordingInfo {
            artist: format_artist_credit(&recording.artist_credit),
            mbid: recording.id,
            title: recording.title,
            duration_s: recording.length.map(|ms| ms / 1000).filter(|&s| s > 0),
            disambiguation: given(recording.disambiguation),
            first_release_date: given(recording.first_release_date.map(|d| d.0)),
        },
        release_count,
        offset,
        releases: page
            .entities
            .into_iter()
            .map(|r| RecordingRelease {
                mbid: r.id,
                title: r.title,
                date: given(r.date.map(|d| d.0)),
                country: given(r.country),
                status: r
                    .status
                    .filter(|s| *s != ReleaseStatus::UnrecognizedReleaseStatus)
                    .map(|s| serde_name(&s)),
                release_group: r.release_group.map(|g| ReleaseGroupInfo {
                    release_types: release_types(&g),
                    mbid: g.id,
                    title: g.title,
                    first_release_date: given(g.first_release_date.map(|d| d.0)),
                }),
            })
            .collect(),
    })
}

/// The release group's primary type (when it has one), then its secondary
/// types, by MusicBrainz's names for them. None when musicbrainz_rs does not
/// know one of them: it reads such a type as an `Unrecognized*` variant,
/// which does not carry MusicBrainz's name.
fn release_types(group: &ReleaseGroup) -> Option<Vec<String>> {
    if group.primary_type == Some(ReleaseGroupPrimaryType::UnrecognizedReleaseGroupPrimaryType)
        || group
            .secondary_types
            .contains(&ReleaseGroupSecondaryType::UnrecognizedReleaseGroupSecondaryType)
    {
        return None;
    }
    let primary = group.primary_type.iter().map(serde_name);
    Some(
        primary
            .chain(group.secondary_types.iter().map(serde_name))
            .collect(),
    )
}

/// The name serde gives a musicbrainz_rs type variant, which is MusicBrainz's
/// name for every variant other than the `Unrecognized*` ones.
fn serde_name(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        other => unreachable!("musicbrainz_rs type variants serialize to their name: {other:?}"),
    }
}

/// None for a text MusicBrainz leaves empty.
fn given(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.is_empty())
}

pub struct MusicBrainzProvider;

impl MusicBrainzProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MusicBrainzProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::MetadataProvider for MusicBrainzProvider {
    fn id(&self) -> Provider {
        Provider::MusicBrainz
    }

    fn name(&self) -> &'static str {
        "MusicBrainz"
    }

    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MetadataError> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Album, limit.min(100) as u8).await
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MetadataError> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Track, limit.min(100) as u8).await
    }

    async fn get_album(&self, id: &str) -> Result<AlbumWithTracks, MetadataError> {
        find_album(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn musicbrainz_errors_keep_their_kind() {
        let mb_error: musicbrainz_rs::entity::api::MusicbrainzError =
            serde_json::from_value(serde_json::json!({"error": "Invalid mbid.", "help": "For usage, please see: https://musicbrainz.org/development/mmd"})).unwrap();
        let parse = serde_json::from_str::<u8>("x").unwrap_err();
        // A server that closes every connection unanswered: the port stays
        // bound for the test, so nothing else can take it and answer.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                drop(connection);
            }
        });
        let dropped = reqwest::get(format!("http://{addr}")).await.unwrap_err();
        let kinds: Vec<&str> = [
            musicbrainz_rs::Error::NotFound("q".into()),
            musicbrainz_rs::Error::MusicbrainzError("q".into(), mb_error),
            musicbrainz_rs::Error::MaxRetriesExceeded,
            musicbrainz_rs::Error::SerdeJsonError(parse),
            musicbrainz_rs::Error::ReqwestError(dropped),
        ]
        .into_iter()
        .map(|e| match MetadataError::from(e) {
            MetadataError::NotFound { .. } => "not_found",
            MetadataError::Rejected { .. } => "rejected",
            MetadataError::RateLimited { .. } => "rate_limited",
            MetadataError::Unreadable { .. } => "unreadable",
            MetadataError::Unavailable { .. } => "unavailable",
            MetadataError::NotConfigured { .. } => "not_configured",
        })
        .collect();
        assert_eq!(
            kinds,
            [
                "not_found",
                "rejected",
                "rate_limited",
                "unreadable",
                "unavailable"
            ]
        );
    }

    /// GET https://musicbrainz.org/ws/2/recording/4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8?inc=artists&fmt=json
    const RECORDING: &str = r#"{"title":"Silent Night","length":149000,"video":false,"first-release-date":"1995","id":"4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8","disambiguation":"","artist-credit":[{"artist":{"disambiguation":"","country":"US","sort-name":"Crosby, Bing","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","name":"Bing Crosby","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person"},"name":"Bing Crosby","joinphrase":""}]}"#;
    /// GET https://musicbrainz.org/ws/2/release?recording=4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8&inc=release-groups&limit=2&offset=0&fmt=json
    const RELEASES: &str = r#"{"release-count":1,"release-offset":0,"releases":[{"id":"317d1e85-3f9e-463e-939b-bebf47aa2178","title":"White Christmas","status":"Official","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","date":"1995","country":"GB","disambiguation":"","quality":"normal","release-group":{"primary-type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","title":"Merry Christmas","first-release-date":"1945","secondary-type-ids":["dd2a21e1-0c00-3729-a7a0-de60b84eb5d1"],"id":"a82c4b78-9e9a-390c-af1f-3c8a8a44c555","disambiguation":"later known as “White Christmas”","primary-type":"Album","secondary-types":["Compilation"]}}]}"#;
    /// GET https://musicbrainz.org/ws/2/artist?query=artist:%22Bing%20Crosby%22&limit=2&fmt=json (first artist)
    const ARTISTS: &str = r#"{"created":"2026-11-15T11:44:54.893Z","count":3,"offset":0,"artists":[{"id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","score":100,"name":"Bing Crosby","sort-name":"Crosby, Bing","country":"US","life-span":{"begin":"1903-05-03","end":"1977-10-14","ended":true}}]}"#;

    #[test]
    fn a_recording_comes_with_its_length_and_the_releases_it_is_on() {
        let r = recording_releases_from(
            serde_json::from_str(RECORDING).unwrap(),
            serde_json::from_str(RELEASES).unwrap(),
        )
        .unwrap();
        assert_eq!(
            r,
            RecordingReleases {
                recording: RecordingInfo {
                    mbid: "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into(),
                    title: "Silent Night".into(),
                    artist: "Bing Crosby".into(),
                    duration_s: Some(149),
                    disambiguation: None,
                    first_release_date: Some("1995".into())
                },
                release_count: 1,
                offset: 0,
                releases: vec![RecordingRelease {
                    mbid: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
                    title: "White Christmas".into(),
                    date: Some("1995".into()),
                    country: Some("GB".into()),
                    status: Some("Official".into()),
                    release_group: Some(ReleaseGroupInfo {
                        mbid: "a82c4b78-9e9a-390c-af1f-3c8a8a44c555".into(),
                        title: "Merry Christmas".into(),
                        release_types: Some(vec!["Album".into(), "Compilation".into()]),
                        first_release_date: Some("1945".into())
                    })
                }],
            }
        );
    }

    #[test]
    fn a_negative_release_count_is_unreadable() {
        let mut page: serde_json::Value = serde_json::from_str(RELEASES).unwrap();
        page["release-count"] = (-1).into();
        let e = recording_releases_from(
            serde_json::from_str(RECORDING).unwrap(),
            serde_json::from_value(page).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Unreadable MusicBrainz answer: MusicBrainz answered a negative release count or offset"
        );
    }

    #[test]
    fn a_release_type_the_crate_does_not_know_gives_no_release_types() {
        let mut page: serde_json::Value = serde_json::from_str(RELEASES).unwrap();
        page["releases"][0]["release-group"]["secondary-types"] =
            serde_json::json!(["Field recording"]);
        let page: musicbrainz_rs::entity::BrowseResult<Release> =
            serde_json::from_value(page).unwrap();
        let group = page.entities[0].release_group.as_ref().unwrap();
        assert_eq!(release_types(group), None);
    }

    #[test]
    fn an_artist_match_has_its_type_country_and_dates() {
        let page: musicbrainz_rs::entity::search::SearchResult<Artist> =
            serde_json::from_str(ARTISTS).unwrap();
        assert_eq!(
            artist_match(page.entities.into_iter().next().unwrap()),
            ArtistMatch {
                mbid: "2437980f-513a-44fc-80f1-b90d9d7fcf8f".into(),
                name: "Bing Crosby".into(),
                sort_name: "Crosby, Bing".into(),
                disambiguation: None,
                artist_type: Some("Person".into()),
                country: Some("US".into()),
                begin: Some("1903-05-03".into()),
                end: Some("1977-10-14".into())
            }
        );
    }

    /// GET https://musicbrainz.org/ws/2/release/317d1e85-3f9e-463e-939b-bebf47aa2178?inc=recordings+artist-credits+release-groups&fmt=json,
    /// fetched 2026-11-15, trimmed to the fields above plus the medium holding track 6 (its other 16 tracks left out).
    const RELEASE: &str = r#"{"id":"317d1e85-3f9e-463e-939b-bebf47aa2178","title":"White Christmas","status":"Official","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","date":"1995","country":"GB","disambiguation":"","quality":"normal","artist-credit":[{"joinphrase":"","name":"Bing Crosby","artist":{"type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person","sort-name":"Crosby, Bing","country":"US","disambiguation":"","name":"Bing Crosby"}}],"release-group":{"title":"Merry Christmas","primary-type":"Album","secondary-type-ids":["dd2a21e1-0c00-3729-a7a0-de60b84eb5d1"],"id":"a82c4b78-9e9a-390c-af1f-3c8a8a44c555","primary-type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","disambiguation":"later known as “White Christmas”","secondary-types":["Compilation"],"first-release-date":"1945"},"media":[{"format":"CD","format-id":"9712d52a-4509-3d4b-a1a2-67c88c643e31","position":1,"track-count":17,"track-offset":0,"tracks":[{"artist-credit":[{"joinphrase":"","artist":{"country":"US","disambiguation":"","name":"Bing Crosby","sort-name":"Crosby, Bing","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","type":"Person"},"name":"Bing Crosby"}],"number":"6","length":149000,"recording":{"id":"4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8","first-release-date":"1995","artist-credit":[{"joinphrase":"","name":"Bing Crosby","artist":{"id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","type":"Person","country":"US","disambiguation":"","name":"Bing Crosby","sort-name":"Crosby, Bing"}}],"disambiguation":"","video":false,"length":149000,"title":"Silent Night"},"id":"72b85e69-2569-30a8-a1f2-ad514cd8c9d7","title":"Silent Night","position":6}]}]}"#;

    #[test]
    fn a_looked_up_release_carries_its_release_groups_types() {
        let r = album_from_release(serde_json::from_str(RELEASE).unwrap());
        assert_eq!(
            (r.album.title.as_str(), r.album.release_types.clone()),
            (
                "White Christmas",
                Some(vec!["Album".to_string(), "Compilation".to_string()])
            )
        );
        assert_eq!(
            (r.tracks[0].id.as_str(), r.tracks[0].duration.as_deref()),
            ("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8", Some("02:29"))
        );
        let mut bare: serde_json::Value = serde_json::from_str(RELEASE).unwrap();
        bare.as_object_mut().unwrap().remove("release-group");
        assert_eq!(
            album_from_release(serde_json::from_value(bare).unwrap())
                .album
                .release_types,
            None
        );
    }

    #[test]
    fn a_recording_musicbrainz_does_not_have_is_not_found() {
        // GET https://musicbrainz.org/ws/2/recording/0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c?inc=artists&fmt=json answers
        // 404 {"help":"For usage, please see: https://musicbrainz.org/development/mmd","error":"Not Found"},
        // which musicbrainz_rs turns into NotFound(url) (src/entity/api.rs:14-19, src/api/api_request.rs:51-67)
        let url = "http://musicbrainz.org/ws/2/recording/0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c?fmt=json&inc=artists";
        let e = MetadataError::from(musicbrainz_rs::Error::NotFound(url.into()));
        assert_eq!(
            e.to_string(),
            format!(
                "MusicBrainz found nothing: Musicbrainz returned \"Not found\" for query \"{url}\""
            )
        );
    }
}
