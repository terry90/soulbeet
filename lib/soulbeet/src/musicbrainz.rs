use musicbrainz_rs::{
    entity::{
        artist_credit::ArtistCredit,
        recording::{Recording, RecordingSearchQuery},
        release::{Release, ReleaseSearchQuery},
        release_group::ReleaseGroupPrimaryType,
    },
    Fetch, MusicBrainzClient, Search,
};
use shared::metadata::{Album, AlbumWithTracks, AlbumGroup, SearchResult, Track, compare_musicbrainz_dates};
use std::{collections::HashSet, future::Future, sync::OnceLock, time::Duration};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use tokio::time::sleep;
use tracing::{info, warn};

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
        MusicBrainzClient::new(&format!(
            "Soulbeet/{version} ( https://github.com/terry90/soulbeet )"
        ))
        .expect("Failed to create MusicBrainz client - invalid user agent format")
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

/// Check if an error is retryable (transient network/server issues)
fn is_retryable_error(error: &musicbrainz_rs::Error) -> bool {
    let error_str = format!("{:?}", error);
    let error_lower = error_str.to_lowercase();

    // Retry on timeout, connection, and 5xx server errors
    if error_lower.contains("timeout")
        || error_lower.contains("connection")
        || error_lower.contains("timed out")
        || error_lower.contains("503")
        || error_lower.contains("502")
        || error_lower.contains("500")
        || error_lower.contains("429") // Rate limited - should retry after backoff
        || error_lower.contains("service unavailable")
    {
        return true;
    }

    // Don't retry client errors (4xx except 429)
    if error_lower.contains("400")
        || error_lower.contains("401")
        || error_lower.contains("403")
        || error_lower.contains("404")
        || error_lower.contains("bad request")
        || error_lower.contains("not found")
        || error_lower.contains("unauthorized")
    {
        return false;
    }

    // Default to retrying for unknown errors (network issues, etc.)
    true
}

/// Retries an async operation with exponential backoff and request timeout.
/// Only retries transient errors (network issues, timeouts, 5xx responses).
/// Does NOT retry client errors (4xx) or permanent failures.
async fn with_retry<T, F, Fut>(operation_name: &str, mut operation: F) -> Result<T, musicbrainz_rs::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, musicbrainz_rs::Error>>,
{
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        // Apply timeout to each request
        let result = tokio::time::timeout(
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
            operation()
        ).await;

        match result {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) => {
                // Check if this error is retryable
                if !is_retryable_error(&e) {
                    warn!(
                        "{} failed with non-retryable error: {:?}",
                        operation_name, e
                    );
                    return Err(e);
                }

                last_error = Some(e);
                if attempt < MAX_RETRIES - 1 {
                    let delay = std::cmp::min(
                        BASE_DELAY_MS * 2u64.pow(attempt),
                        MAX_BACKOFF_MS
                    );
                    warn!(
                        "{} failed (attempt {}/{}), retrying in {}ms: {:?}",
                        operation_name,
                        attempt + 1,
                        MAX_RETRIES,
                        delay,
                        last_error
                    );
                    sleep(Duration::from_millis(delay)).await;
                }
            }
            Err(_timeout) => {
                // Request timed out
                warn!(
                    "{} timed out after {}s (attempt {}/{})",
                    operation_name,
                    REQUEST_TIMEOUT_SECS,
                    attempt + 1,
                    MAX_RETRIES
                );
                // Create a timeout error - we'll retry
                if attempt < MAX_RETRIES - 1 {
                    let delay = std::cmp::min(
                        BASE_DELAY_MS * 2u64.pow(attempt),
                        MAX_BACKOFF_MS
                    );
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    // Return the last error or panic (should never happen since we always set last_error on timeout)
    // If we somehow have no error, create a synthetic one
    match last_error {
        Some(e) => Err(e),
        None => {
            warn!(
                "{} failed after {} retries with no recorded error (likely all timeouts)",
                operation_name, MAX_RETRIES
            );
            // Re-run the operation one more time to get an error to return
            // This is a fallback - shouldn't normally happen
            operation().await
        }
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
) -> Result<Vec<SearchResult>, musicbrainz_rs::Error> {
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
                b_rating.partial_cmp(&a_rating).unwrap_or(std::cmp::Ordering::Equal)
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
                let mut album_query = ReleaseSearchQuery::query_builder();
                if let Some(ref artist) = artist {
                    album_query.artist(artist).and();
                }
                let search_query = album_query.release(query).and().status("Official").build();
                let limit = 100; // lower numbers (like 25) might not yield everything
                async move {
                    Release::search(search_query)
                        .limit(limit)
                        .with_release_groups()
                        .execute_with_client(client)
                        .await
                }
            })
            .await?;

            // release_group_id -> (Title -> Album (edition))
            let mut results_container: HashMap<String, HashMap<String, Album>> = HashMap::new();
            // release_group_id -> release group title
            let mut release_groups: HashMap<String, String> = HashMap::new();
            // track when release groups first appeared in response
            // used to sort the final result vec
            let mut rg_order: HashMap<String, usize> = HashMap::new();

            for release in search_results.entities {
                if release.release_group.as_ref().unwrap().primary_type != Some(ReleaseGroupPrimaryType::Album)
                    && release.release_group.as_ref().unwrap().primary_type != Some(ReleaseGroupPrimaryType::Ep)
                {
                    continue;
                }

                // For releases with more than 1 medium or with a single medium
                // that is not "CD", "Digital Media" or "12\" Vinyl",
                // it's likely a compilation, so skip it
                if release.media.as_ref().is_some_and(|m| {
                    m.len() > 1 ||
                        m.len() == 1
                            && m[0].format.as_deref() != Some("CD")
                            && m[0].format.as_deref() != Some("Digital Media")
                            && m[0].format.as_deref() != Some("12\" Vinyl")
                }) {
                    continue;
                }

                let rg_id = release.release_group.as_ref().unwrap().id.clone();

                let title = match release.disambiguation.as_deref() {
                    None | Some("") => release.title.clone(),
                    Some(disambiguation) => format!("{} ({})", release.title.clone(), disambiguation),
                };

                if !rg_order.contains_key(&rg_id) {
                    rg_order.insert(rg_id.clone(), rg_order.len());
                }


                let edition = Album {
                    id: release.id.clone(),
                    title: title.clone(),
                    artist: format_artist_credit(&release.artist_credit),
                    release_date: release.date.as_ref().map(|d| d.0.clone()),
                    mbid: Some(release.id.clone()),
                    cover_url: Some(format!("https://coverartarchive.org/release/{}/front-250", release.id)),
                };

                // Use the release group ID to track unique releases and avoid duplicates based on title.
                // However, for releases with the same title and release group ID,
                // we keep the oldest one (based on release date) to ensure we get the canonical release
                let release_group = results_container.entry(rg_id.clone()).or_insert_with(|| {
                    release_groups.insert(rg_id.clone(), release.release_group.as_ref().unwrap().title.clone());
                    HashMap::new()
                });

                match release_group.entry(title.clone()) {
                    Entry::Vacant(entry) => {
                        entry.insert(edition);
                    }
                    Entry::Occupied(mut entry) => {
                        let existing = entry.get();
                        let new_date: Option<&str> = release.date.as_ref().map(|d| d.0.as_ref());
                        if compare_musicbrainz_dates(&existing.release_date, &new_date) == std::cmp::Ordering::Greater {
                            entry.insert(edition);
                        }
                    }
                }
            }

            // Flatten the results container into a single vector of SearchResult::AlbumGroup
            for (rg_id, editions) in results_container.into_iter() {
                let oldest_edition = editions.values().min_by(|a, b|
                    compare_musicbrainz_dates(&a.release_date, &b.release_date)
                ).unwrap();
                let album_group = AlbumGroup {
                    id: rg_id.clone(),
                    title: release_groups.get(&rg_id).unwrap_or(&oldest_edition.title).clone(),
                    artist: oldest_edition.artist.clone(),
                    release_date: oldest_edition.release_date.clone(),
                    mbid: Some(rg_id.clone()),
                    cover_url: Some(format!("https://coverartarchive.org/release-group/{}/front-250", rg_id)),
                    editions: editions.into_values().collect(),
                };
                results.push(SearchResult::AlbumGroup(album_group));
            }

            results.sort_by(|a, b| {
                let SearchResult::AlbumGroup(a_album) = a else { return std::cmp::Ordering::Equal; };
                let SearchResult::AlbumGroup(b_album) = b else { return std::cmp::Ordering::Equal; };
                let a_rg_rank = rg_order.get(&a_album.id).unwrap_or(&usize::MAX);
                let b_rg_rank = rg_order.get(&b_album.id).unwrap_or(&usize::MAX);

                // First sort by release group rank (i.e. when they appeared in the musicbrainz response)
                a_rg_rank.cmp(&b_rg_rank)
                    // then compare by date
                    .then(compare_musicbrainz_dates(&a_album.release_date, &b_album.release_date))
            });
        }
    }

    Ok(results)
}

/// Fetches a release (album) by its ID and returns it with its full tracklist.
pub async fn find_album(release_id: &str) -> Result<AlbumWithTracks, musicbrainz_rs::Error> {
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
    ) -> crate::error::Result<Vec<SearchResult>> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Album, limit as u8)
            .await
            .map_err(|e| crate::error::SoulseekError::Api {
                status: 500,
                message: e.to_string(),
            })
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> crate::error::Result<Vec<SearchResult>> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Track, limit as u8)
            .await
            .map_err(|e| crate::error::SoulseekError::Api {
                status: 500,
                message: e.to_string(),
            })
    }

    async fn get_album(&self, id: &str) -> crate::error::Result<AlbumWithTracks> {
        find_album(id).await.map_err(|e| crate::error::SoulseekError::Api {
            status: 500,
            message: e.to_string(),
        })
    }
}
