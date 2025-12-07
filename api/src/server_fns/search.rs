use chrono::Duration;
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::DownloadQuery,
    musicbrainz::{AlbumWithTracks, SearchResult},
    slskd::AlbumResult,
};

#[cfg(feature = "server")]
use shared::musicbrainz::Track;
#[cfg(feature = "server")]
use soulbeet::musicbrainz;

use super::server_error;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::SLSKD_CLIENT;

#[cfg(feature = "server")]
async fn slskd_search(
    artist: String,
    album: String,
    tracks: Vec<Track>,
) -> Result<Vec<AlbumResult>, ServerFnError> {
    let mut search = match SLSKD_CLIENT
        .search(artist, album, tracks, Duration::seconds(45))
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(server_error(e)),
    };

    search.sort_by(|a, b| b.score.total_cmp(&a.score));

    for album in search.iter().take(10) {
        info!("Album: {}", album.album_title);
        info!("Score: {}", album.score);
        info!("Quality: {}", album.dominant_quality);

        for track in album.tracks.iter() {
            info!("  Filename: {:?}", track.base.filename);
            info!("  Title: {:?}", track.title);
            info!("  Artist: {:?}", track.artist);
            info!("  Album: {:?}", track.album);
            info!("  Format: {:?}", track.base.quality());
        }
    }

    Ok(search)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
}

#[post("/api/musicbrainz/search/album", _: AuthSession)]
pub async fn search_album(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Album,
        25,
    )
    .await
    .map_err(server_error)
}

#[post("/api/musicbrainz/search/track", _: AuthSession)]
pub async fn search_track(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Track,
        25,
    )
    .await
    .map_err(server_error)
}

#[get("/api/musicbrainz/album/:id", _: AuthSession)]
pub async fn find_album(id: String) -> Result<AlbumWithTracks, ServerFnError> {
    musicbrainz::find_album(&id).await.map_err(server_error)
}

#[post("/api/slskd/search", _: AuthSession)]
pub async fn search_downloads(data: DownloadQuery) -> Result<Vec<AlbumResult>, ServerFnError> {
    slskd_search(data.album.artist, data.album.title, data.tracks).await
}
