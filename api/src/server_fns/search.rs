use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    metadata::{AlbumWithTracks, Provider, SearchResults},
};

#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::services::{download_backend, metadata_provider};
#[cfg(feature = "server")]
use crate::{server_fns::server_error, AuthSession};

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
    #[serde(default)]
    pub backend: Option<String>,
}

#[post("/api/metadata/search/album", auth: AuthSession)]
pub async fn search_album(input: SearchQuery) -> Result<SearchResults, ServerFnError> {
    let user_settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
    let provider = metadata_provider(
        input.provider.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    )
    .await
    .map_err(server_error)?;

    let provider_enum: Provider = provider.id().parse().unwrap_or_default();
    let results = provider
        .search_albums(input.artist.as_deref(), &input.query, 25)
        .await
        .map_err(server_error)?;

    Ok(SearchResults {
        provider: provider_enum,
        results,
    })
}

#[post("/api/metadata/search/track", auth: AuthSession)]
pub async fn search_track(input: SearchQuery) -> Result<SearchResults, ServerFnError> {
    let user_settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
    let provider = metadata_provider(
        input.provider.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    )
    .await
    .map_err(server_error)?;

    let provider_enum: Provider = provider.id().parse().unwrap_or_default();
    let results = provider
        .search_tracks(input.artist.as_deref(), &input.query, 25)
        .await
        .map_err(server_error)?;

    Ok(SearchResults {
        provider: provider_enum,
        results,
    })
}

#[post("/api/metadata/album", auth: AuthSession)]
pub async fn find_album(input: AlbumQuery) -> Result<AlbumWithTracks, ServerFnError> {
    let user_settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
    let provider_str = input.provider.map(|p| p.to_string());
    let provider = metadata_provider(
        provider_str.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    )
    .await
    .map_err(server_error)?;

    provider.get_album(&input.id).await.map_err(server_error)
}

/// Album queries arrive from the UI with an empty track list; source matching
/// scores candidate files against expected track titles, so resolve the
/// album's tracklist through the metadata provider before searching.
#[cfg(feature = "server")]
pub(crate) async fn hydrate_album_tracks(query: &mut DownloadQuery) -> Result<(), String> {
    if !query.tracks.is_empty() {
        return Ok(());
    }
    let Some(album) = query.album.clone() else {
        return Ok(());
    };

    let provider = metadata_provider(None, None)
        .await
        .map_err(|e| format!("metadata provider unavailable: {e}"))?;
    let album_with_tracks = provider
        .get_album(&album.id)
        .await
        .map_err(|e| format!("could not resolve tracklist for '{}': {e}", album.title))?;
    query.tracks = album_with_tracks.tracks;
    Ok(())
}

#[post("/api/download/search/start", auth: AuthSession)]
pub async fn start_download_search(data: DownloadQuery) -> Result<String, ServerFnError> {
    let mut data = data;
    hydrate_album_tracks(&mut data).await.map_err(server_error)?;

    let user_settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;

    let backend = download_backend(data.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    backend
        .start_search(
            data.album.as_ref(),
            &data.tracks,
            user_settings.require_flac_only,
        )
        .await
        .map_err(server_error)
}

#[post("/api/download/search/poll", _: AuthSession)]
pub async fn poll_download_search(input: PollQuery) -> Result<DownloadSearchResult, ServerFnError> {
    let backend = download_backend(input.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    backend
        .poll_search(&input.search_id)
        .await
        .map_err(server_error)
}
