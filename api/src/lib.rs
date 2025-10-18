//! This crate contains all shared fullstack server functions.

use chrono::Duration;
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;
use shared::musicbrainz::{AlbumWithTracks, SearchResult};

#[cfg(feature = "server")]
use shared::musicbrainz::Track;
#[cfg(feature = "server")]
use soulful::musicbrainz;
#[cfg(feature = "server")]
use soulful::slskd::{AlbumResult, SoulseekClientBuilder};

#[cfg(feature = "server")]
async fn slskd_search(
    artist: String,
    album: String,
    tracks: Option<Vec<Track>>,
) -> Result<Vec<AlbumResult>, ServerFnError> {
    use dioxus::prelude::server_fn::error::NoCustomError;

    let client = SoulseekClientBuilder::new()
        .api_key("BOVeIS961OlDWlUeEjF6DsIZKzf857ijKBGFWWw4N9Scj1xwoq2C3VbjMBU=")
        .base_url("http://192.168.1.105:5030/")
        .download_path("/tmp/downloads")
        .build()
        .unwrap();

    let health = client.check_connection().await;
    let mut search = client
        .search(artist, album, tracks, Duration::seconds(30))
        .await
        .map_err(|e| ServerFnError::<NoCustomError>::ServerError(e.to_string()))?;
    search.sort_by(|a, b| b.score.total_cmp(&a.score));

    for album in search.iter().take(10) {
        println!("Album: {}", album.album_title);
        println!("Score: {}", album.score);
        println!("Quality: {}", album.dominant_quality);

        for track in album.tracks.iter() {
            println!("  Filename: {:?}", track.base.filename);
            println!("  Title: {:?}", track.title);
            println!("  Artist: {:?}", track.artist);
            println!("  Album: {:?}", track.album);
            println!("  Format: {:?}", track.base.quality());
        }
    }

    Ok(search)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
}

#[server]
pub async fn search_album(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    let results = musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Album,
        10,
    )
    .await?;

    Ok(results)
}

#[server]
pub async fn search_track(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    let results = musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Track,
        10,
    )
    .await?;

    Ok(results)
}

#[server]
pub async fn find_album(id: String) -> Result<AlbumWithTracks, ServerFnError> {
    let results = musicbrainz::find_album(&id).await?;

    Ok(results)
}

#[server]
pub async fn download(data: DownloadQuery) -> Result<(), ServerFnError> {
    info!("{data:?}");

    match data {
        DownloadQuery::Album { album } => {
            info!("{album:?}");

            slskd_search(album.artist, album.title, None).await?;
        }
        DownloadQuery::Track { album, tracks } => {
            info!("{tracks:?}");

            slskd_search(album.artist, album.title, Some(tracks)).await?;
        }
    }

    Ok(())
}
