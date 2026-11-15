use musicbrainz_rs::{
    entity::{
        artist_credit::ArtistCredit,
        recording::{Recording, RecordingSearchQuery},
        release::{Release, ReleaseStatus},
        release_group::{ReleaseGroup, ReleaseGroupPrimaryType, ReleaseGroupSearchQuery},
    },
    Fetch, MusicBrainzClient, Search,
};
use shared::failure::Service;
use shared::metadata::{Album, AlbumWithTracks, SearchResult, Track};
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
                raw: e.to_string(),
            },
            Error::ReqwestError(e) => MetadataError::Unavailable {
                service,
                status: e.status().map(|s| s.as_u16()),
                raw: e.to_string(),
            },
            Error::SerdeJsonError(e) => MetadataError::Unreadable {
                service,
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

    // Fetch the release with recordings (tracks) and artist credits for the tracks.
    let release = with_retry("MusicBrainz album fetch", || async {
        Release::fetch()
            .id(release_id)
            .with_recordings()
            .with_artist_credits()
            .execute_with_client(client)
            .await
    })
    .await?;

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
    };

    let album_with_tracks = AlbumWithTracks { album, tracks };

    Ok(album_with_tracks)
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
    fn id(&self) -> &'static str {
        "musicbrainz"
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
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = reqwest::get(format!("http://127.0.0.1:{port}"))
            .await
            .unwrap_err();
        let kinds: Vec<&str> = [
            musicbrainz_rs::Error::NotFound("q".into()),
            musicbrainz_rs::Error::MusicbrainzError("q".into(), mb_error),
            musicbrainz_rs::Error::MaxRetriesExceeded,
            musicbrainz_rs::Error::SerdeJsonError(parse),
            musicbrainz_rs::Error::ReqwestError(refused),
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
}
