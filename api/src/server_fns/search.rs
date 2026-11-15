use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    metadata::{AlbumWithTracks, Provider, SearchResults},
};

#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::core::metadata::{self, SearchKind};
#[cfg(feature = "server")]
use crate::services::download_backend;
#[cfg(feature = "server")]
use crate::{
    server_fns::{failure_error, refusal_error, server_error},
    AuthSession,
};
#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{LazyLock, Mutex, MutexGuard};
#[cfg(feature = "server")]
use std::time::{Duration, Instant};

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

/// How long a started search stays readable by the user who started it.
#[cfg(feature = "server")]
pub(crate) const SEARCH_OWNER_TTL: Duration = Duration::from_secs(3600);

/// Who started each download search, by backend and search id, so only
/// they can read its results.
#[cfg(feature = "server")]
#[derive(Default)]
pub(crate) struct SearchOwners {
    started: Mutex<HashMap<(String, String), (UserId, Instant)>>,
}

#[cfg(feature = "server")]
impl SearchOwners {
    fn started(&self) -> MutexGuard<'_, HashMap<(String, String), (UserId, Instant)>> {
        self.started.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expired(at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(at) > SEARCH_OWNER_TTL
    }

    /// Records `user` as the starter of the search, and forgets searches
    /// older than `SEARCH_OWNER_TTL`.
    fn record(&self, backend: &str, search_id: &str, user: &UserId, now: Instant) {
        let mut started = self.started();
        started.retain(|_, (_, at)| !Self::expired(*at, now));
        started.insert(
            (backend.to_string(), search_id.to_string()),
            (user.clone(), now),
        );
    }

    /// Whether `user` started the search and it has not expired.
    fn owns(&self, backend: &str, search_id: &str, user: &UserId, now: Instant) -> bool {
        self.started()
            .get(&(backend.to_string(), search_id.to_string()))
            .is_some_and(|(owner, at)| owner == user && !Self::expired(*at, now))
    }
}

#[cfg(feature = "server")]
static SEARCH_OWNERS: LazyLock<SearchOwners> = LazyLock::new(SearchOwners::default);

#[post("/api/download/search/start", auth: AuthSession)]
pub async fn start_download_search(data: DownloadQuery) -> Result<String, ServerFnError> {
    let key = metadata::lastfm_key(&auth.0).await.map_err(failure_error)?;
    let mut data = data;
    metadata::hydrate_album_tracks(&mut data, key.as_deref())
        .await
        .map_err(failure_error)?;

    let backend = download_backend(data.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    let search_id = backend
        .start_search(data.album.as_ref(), &data.tracks)
        .await
        .map_err(server_error)?;
    SEARCH_OWNERS.record(backend.id(), &search_id, &auth.0.id(), Instant::now());
    Ok(search_id)
}

#[post("/api/download/search/poll", auth: AuthSession)]
pub async fn poll_download_search(input: PollQuery) -> Result<DownloadSearchResult, ServerFnError> {
    let backend = download_backend(input.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    if !SEARCH_OWNERS.owns(backend.id(), &input.search_id, &auth.0.id(), Instant::now()) {
        return Err(refusal_error("Search not found"));
    }

    backend
        .poll_search(&input.search_id)
        .await
        .map_err(server_error)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn a_search_answers_only_its_owner() {
        let owners = SearchOwners::default();
        let t0 = Instant::now();
        let (a, b) = (UserId::new("user-a"), UserId::new("user-b"));
        owners.record("slskd", "s1", &a, t0);

        assert!(owners.owns("slskd", "s1", &a, t0));
        assert!(!owners.owns("slskd", "s1", &b, t0));
        assert!(!owners.owns("slskd", "s2", &a, t0));
        assert!(!owners.owns(
            "slskd",
            "s1",
            &a,
            t0 + SEARCH_OWNER_TTL + Duration::from_secs(1)
        ));
    }
}
