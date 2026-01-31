use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    musicbrainz::{AlbumWithTracks, SearchResult},
};

#[cfg(feature = "server")]
use crate::{globals::SERVICES, server_fns::server_error, AuthSession};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumQuery {
    pub id: String,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollQuery {
    pub search_id: String,
    #[serde(default)]
    pub backend: Option<String>,
}

#[post("/api/metadata/search/album", _: AuthSession)]
pub async fn search_album(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    let provider = SERVICES
        .metadata(input.provider.as_deref())
        .ok_or_else(|| server_error("metadata provider not found"))?;

    provider
        .search_albums(input.artist.as_deref(), &input.query, 25)
        .await
        .map_err(server_error)
}

#[post("/api/metadata/search/track", _: AuthSession)]
pub async fn search_track(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    let provider = SERVICES
        .metadata(input.provider.as_deref())
        .ok_or_else(|| server_error("metadata provider not found"))?;

    provider
        .search_tracks(input.artist.as_deref(), &input.query, 25)
        .await
        .map_err(server_error)
}

#[post("/api/metadata/album", _: AuthSession)]
pub async fn find_album(input: AlbumQuery) -> Result<AlbumWithTracks, ServerFnError> {
    let provider = SERVICES
        .metadata(input.provider.as_deref())
        .ok_or_else(|| server_error("metadata provider not found"))?;

    provider.get_album(&input.id).await.map_err(server_error)
}

#[post("/api/download/search/start", _: AuthSession)]
pub async fn start_download_search(data: DownloadQuery) -> Result<String, ServerFnError> {
    let backend = SERVICES
        .download(data.backend.as_deref())
        .ok_or_else(|| server_error("download backend not found"))?;

    backend
        .start_search(data.album.as_ref(), &data.tracks)
        .await
        .map_err(server_error)
}

#[post("/api/download/search/poll", _: AuthSession)]
pub async fn poll_download_search(input: PollQuery) -> Result<DownloadSearchResult, ServerFnError> {
    let backend = SERVICES
        .download(input.backend.as_deref())
        .ok_or_else(|| server_error("download backend not found"))?;

    backend
        .poll_search(&input.search_id)
        .await
        .map_err(server_error)
}
