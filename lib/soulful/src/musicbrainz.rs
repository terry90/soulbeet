use musicbrainz_rs::{
    entity::{
        artist_credit::ArtistCredit,
        recording::{Recording, RecordingSearchQuery},
        release::{Release, ReleaseStatus},
        release_group::{ReleaseGroup, ReleaseGroupPrimaryType, ReleaseGroupSearchQuery},
    },
    Fetch, MusicBrainzClient, Search,
};
use shared::musicbrainz::{Album, AlbumWithTracks, SearchResult, Track};
use std::{collections::HashSet, sync::OnceLock};

// This ensures the client is initialized only once with a proper user agent.
fn musicbrainz_client() -> &'static MusicBrainzClient {
    static CLIENT: OnceLock<MusicBrainzClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        MusicBrainzClient::new(&format!(
            "Soulful/{version} ( https://github.com/terry90/soulful )"
        ))
        .unwrap()
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

/// An enumeration to specify the type of search.
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

    match search_type {
        SearchType::Track => {
            let mut recording_query = RecordingSearchQuery::query_builder();
            if let Some(ref artist) = artist {
                recording_query.artist_name(artist).and();
            }
            let search_query = recording_query.recording(query).build();

            let search_results = Recording::search(search_query)
                .limit(limit)
                .with_releases()
                .execute_with_client(client)
                .await?;

            let mut unique_tracks = HashSet::new();

            for recording in search_results.entities {
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
                        id: recording.id,
                        title: recording.title.clone(),
                        artist: artist_name.clone(),
                        album_id: first_release.map(|release| release.id.clone()),
                        album_title: first_release.map(|r| r.title.clone()),
                        release_date: first_release.and_then(|r| r.date.clone().map(|d| d.0)),
                        duration: format_duration(&recording.length),
                    };
                    unique_tracks.insert(key);
                    results.push(SearchResult::Track(track));
                }
            }
        }
        SearchType::Album => {
            let mut album_query = ReleaseGroupSearchQuery::query_builder();
            if let Some(ref artist) = artist {
                album_query.artist(artist).and();
            }
            let search_query = album_query.release_group(query).build();

            let search_results = ReleaseGroup::search(search_query)
                .limit(limit)
                .with_releases()
                .execute_with_client(client)
                .await?;

            for release_group in search_results.entities {
                if release_group.primary_type != Some(ReleaseGroupPrimaryType::Album) {
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
                    }));
                }
            }
        }
    }

    Ok(results)
}

/// Fetches a release (album) by its ID and returns it with its full tracklist.
pub async fn find_album(release_id: &str) -> Result<AlbumWithTracks, musicbrainz_rs::Error> {
    let client = musicbrainz_client();

    // Fetch the release with recordings (tracks) and artist credits for the tracks.
    let release = Release::fetch()
        .id(release_id)
        .with_recordings()
        .with_artist_credits()
        .execute_with_client(client)
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
                        });
                    }
                }
            }
        }
    }

    // First, create the standalone Album object.
    let album = Album {
        id: release.id,
        title: release.title,
        artist: format_artist_credit(&release.artist_credit),
        release_date: release.date.map(|d| d.0),
    };

    // Then, package it into the new struct along with the tracks.
    let album_with_tracks = AlbumWithTracks { album, tracks };

    Ok(album_with_tracks)
}
