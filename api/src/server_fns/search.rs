use chrono::Duration;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::DownloadQuery,
    musicbrainz::{AlbumWithTracks, SearchResult},
    slskd::SearchResponse,
};

#[cfg(feature = "server")]
use soulbeet::musicbrainz;

use super::server_error;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::SLSKD_CLIENT;

static SLSKD_MAX_SEARCH_DURATION: i64 = 120; // seconds

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

#[post("/api/slskd/search/start", _: AuthSession)]
pub async fn start_download_search(data: DownloadQuery) -> Result<String, ServerFnError> {
    let artist = data.album.artist;
    let album = data.album.title;
    let tracks = data.tracks;

    SLSKD_CLIENT
        .start_search(
            artist,
            album,
            tracks,
            Duration::seconds(SLSKD_MAX_SEARCH_DURATION),
        )
        .await
        .map_err(server_error)
}

#[post("/api/slskd/search/poll", _: AuthSession)]
pub async fn poll_download_search(search_id: String) -> Result<SearchResponse, ServerFnError> {
    let (results, has_more, state) = SLSKD_CLIENT
        .poll_search(search_id.clone())
        .await
        .map_err(server_error)?;

    Ok(SearchResponse {
        search_id,
        total_results: results.len(),
        results,
        has_more,
        state,
    })
}
