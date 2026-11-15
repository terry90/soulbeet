use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    metadata::{AlbumWithTracks, Provider, SearchResults},
};

#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::services::{download_backend, metadata_provider};
#[cfg(feature = "server")]
use crate::{
    server_fns::{refusal_error, server_error},
    AuthSession,
};
#[cfg(feature = "server")]
use soulbeet::MetadataError;
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
    let user_settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let provider = metadata_provider(
        input.provider.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    );

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
    let user_settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let provider = metadata_provider(
        input.provider.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    );

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
    let user_settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let provider_str = album_lookup_provider(input.provider, &input.id).map(|p| p.to_string());
    let provider = metadata_provider(
        provider_str.as_deref(),
        user_settings.lastfm_api_key.as_deref(),
    );

    provider.get_album(&input.id).await.map_err(server_error)
}

/// The provider that looks up the album `id` a search by `searched` gave.
/// A Last.fm search gives an album Last.fm has an MBID for that MBID as its
/// id: a MusicBrainz release id, which MusicBrainz looks up whichever search
/// gave it. Only Last.fm's own ids go to Last.fm.
#[cfg(feature = "server")]
fn album_lookup_provider(searched: Option<Provider>, id: &str) -> Option<Provider> {
    match searched {
        Some(Provider::LastFm) if !soulbeet::lastfm::is_album_id(id) => Some(Provider::MusicBrainz),
        searched => searched,
    }
}

/// Album queries arrive from the UI with an empty track list; source matching
/// scores candidate files against expected track titles, so resolve the
/// album's tracklist through the metadata provider before searching. Fails
/// only for an album query.
#[cfg(feature = "server")]
pub(crate) async fn hydrate_album_tracks(
    query: &mut DownloadQuery,
    lastfm_api_key: Option<&str>,
) -> Result<(), TracklistError> {
    if !query.tracks.is_empty() {
        return Ok(());
    }
    let Some(album) = query.album.as_ref() else {
        return Ok(());
    };

    let provider = album_lookup_provider(query.metadata_provider, &album.id).map(|p| p.to_string());
    let album_with_tracks = metadata_provider(provider.as_deref(), lastfm_api_key)
        .get_album(&album.id)
        .await
        .map_err(|error| TracklistError {
            album_title: album.title.clone(),
            error,
        })?;
    query.tracks = album_with_tracks.tracks;
    Ok(())
}

/// The tracklist of an album query's album could not be looked up.
#[cfg(feature = "server")]
#[derive(Debug)]
pub(crate) struct TracklistError {
    pub(crate) album_title: String,
    pub(crate) error: MetadataError,
}

#[cfg(feature = "server")]
impl std::fmt::Display for TracklistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not resolve tracklist for '{}': {}",
            self.album_title, self.error
        )
    }
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
    let user_settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let mut data = data;
    hydrate_album_tracks(&mut data, user_settings.lastfm_api_key.as_deref())
        .await
        .map_err(server_error)?;

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

    #[test]
    fn an_album_id_is_looked_up_by_the_provider_it_belongs_to() {
        let mbid = "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f";
        let lastfm = "lastfm:marrow choir:winter songs";
        assert_eq!(
            album_lookup_provider(Some(Provider::LastFm), lastfm),
            Some(Provider::LastFm)
        );
        assert_eq!(
            album_lookup_provider(Some(Provider::LastFm), mbid),
            Some(Provider::MusicBrainz)
        );
        assert_eq!(
            album_lookup_provider(Some(Provider::MusicBrainz), mbid),
            Some(Provider::MusicBrainz)
        );
        assert_eq!(album_lookup_provider(None, mbid), None);
    }
}
