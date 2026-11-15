use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    metadata::{AlbumWithTracks, Provider, SearchResults},
};

#[cfg(feature = "server")]
use crate::core::metadata::{self, SearchKind};
#[cfg(feature = "server")]
use crate::core::sources;
#[cfg(feature = "server")]
use crate::{server_fns::failure_error, AuthSession};

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
    pub provider: Option<Provider>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollQuery {
    pub search_id: String,
}

#[post("/api/metadata/search/album", auth: AuthSession)]
pub async fn search_album(input: SearchQuery) -> Result<SearchResults, ServerFnError> {
    metadata::search(
        &auth.0,
        SearchKind::Albums,
        input.artist.as_deref(),
        &input.query,
        input.provider.as_deref(),
    )
    .await
    .map_err(failure_error)
}

#[post("/api/metadata/search/track", auth: AuthSession)]
pub async fn search_track(input: SearchQuery) -> Result<SearchResults, ServerFnError> {
    metadata::search(
        &auth.0,
        SearchKind::Tracks,
        input.artist.as_deref(),
        &input.query,
        input.provider.as_deref(),
    )
    .await
    .map_err(failure_error)
}

#[post("/api/metadata/album", auth: AuthSession)]
pub async fn find_album(input: AlbumQuery) -> Result<AlbumWithTracks, ServerFnError> {
    metadata::album(&auth.0, &input.id, input.provider)
        .await
        .map_err(failure_error)
}

#[post("/api/download/search/start", auth: AuthSession)]
pub async fn start_download_search(data: DownloadQuery) -> Result<String, ServerFnError> {
    sources::start(&auth.0, data).await.map_err(failure_error)
}

#[post("/api/download/search/poll", auth: AuthSession)]
pub async fn poll_download_search(input: PollQuery) -> Result<DownloadSearchResult, ServerFnError> {
    sources::poll(&auth.0, &input.search_id)
        .await
        .map_err(failure_error)
}
