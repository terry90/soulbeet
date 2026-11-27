//! Metadata search, album lookup and an album query's tracklist.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use shared::download::DownloadQuery;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{
    Album, AlbumWithTracks, ArtistCatalog, CatalogAlbumDetail, CatalogProvider, SearchResult,
    UnifiedCatalogSearch,
};
use soulbeet::deezer::DeezerClient;
use soulbeet::knowledge::{KnowledgeError, KnowledgeHosts};
use soulbeet::musicbrainz::{
    self, ArtistAlbums, ArtistMatch, MusicBrainzProvider, RecordingReleases,
};
use soulbeet::polite::{Pace, RemoteError};
use soulbeet::{MetadataError, MetadataProvider};
use tracing::{info, warn};

use crate::models::knowledge_cache::SqliteKnowledgeCache;

/// Which provider search runs: search_albums or search_tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Albums,
    Tracks,
}

/// Results the web UI's searches ask for.
pub(crate) const SEARCH_LIMIT: usize = 25;

/// Default result count per section for unified catalog search.
pub const UNIFIED_SEARCH_LIMIT: usize = 20;

/// Maximum number of Unicode scalar values accepted for a metadata search query.
pub const MAX_SEARCH_QUERY_CHARS: usize = 256;

fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn bound_query(query: &str) -> &str {
    truncate_chars(query.trim(), MAX_SEARCH_QUERY_CHARS).trim()
}

fn validate_mbid(id: &str) -> Option<String> {
    uuid::Uuid::parse_str(id)
        .ok()
        .map(|u| u.hyphenated().to_string())
}

static DEEZER_CLIENT: LazyLock<Result<DeezerClient, RemoteError>> = LazyLock::new(|| {
    let hosts = KnowledgeHosts::from_env();
    let cache = Arc::new(SqliteKnowledgeCache::new((*crate::db::DB).clone()));
    DeezerClient::new(&hosts.deezer, Pace::DEEZER, cache)
});

fn parse_deezer_album_id(id: &str) -> Option<u64> {
    id.strip_prefix("deezer:album:")
        .and_then(|s| s.parse::<u64>().ok())
}

fn parse_deezer_artist_id(id: &str) -> Option<u64> {
    id.strip_prefix("deezer:artist:")
        .and_then(|s| s.parse::<u64>().ok())
}

fn not_found_failure(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::MetadataNotFound,
        Stage::Metadata,
        RetryAdvice::OtherQuery,
        message,
    )
}

fn remote_failure(e: &RemoteError) -> Failure {
    match e {
        RemoteError::RateLimited { .. } => Failure::new(
            FailureCode::MetadataRateLimited,
            Stage::Metadata,
            RetryAdvice::SameSourceLater,
            e.to_string(),
        ),
        _ => Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Metadata,
            RetryAdvice::SameSourceLater,
            e.to_string(),
        ),
    }
}

fn deezer_failure(e: &KnowledgeError) -> Failure {
    match e {
        KnowledgeError::Remote(r) => remote_failure(r),
        KnowledgeError::MusicBrainz(m) => provider_failure(m),
        other => Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Metadata,
            RetryAdvice::SameSourceLater,
            other.to_string(),
        ),
    }
}

fn lastfm_id_failure(id: &str) -> Failure {
    not_found_failure(format!(
        "'{id}' came from a Last.fm search, which Soulbeet no longer uses: search for it again"
    ))
}

/// Hybrid unified catalog search across tracks, albums, and artists.
/// Uses Deezer by default and automatically falls back to MusicBrainz if Deezer
/// returns empty results or fails. Passing `Some(CatalogProvider::Musicbrainz)`
/// queries MusicBrainz directly.
pub async fn search_unified(
    query: &str,
    provider: Option<CatalogProvider>,
    limit: usize,
) -> Result<UnifiedCatalogSearch, Failure> {
    search_unified_with(
        query,
        provider,
        limit,
        |q, lim| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.search_unified(&q, lim).await
        },
        |q, lim| async move { musicbrainz::search_unified(&q, lim).await },
    )
    .await
}

async fn search_unified_with<FD, FutD, FM, FutM>(
    query: &str,
    provider: Option<CatalogProvider>,
    limit: usize,
    deezer_search: FD,
    mb_search: FM,
) -> Result<UnifiedCatalogSearch, Failure>
where
    FD: FnOnce(String, usize) -> FutD,
    FutD: Future<Output = Result<UnifiedCatalogSearch, KnowledgeError>>,
    FM: FnOnce(String, usize) -> FutM,
    FutM: Future<Output = Result<UnifiedCatalogSearch, MetadataError>>,
{
    let trimmed = bound_query(query);
    if trimmed.is_empty() {
        return Ok(UnifiedCatalogSearch {
            query: String::new(),
            provider: provider.unwrap_or(CatalogProvider::Deezer),
            fallback_used: false,
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
        });
    }

    let limit = limit.clamp(1, 50);

    if provider == Some(CatalogProvider::Musicbrainz) {
        return mb_search(trimmed.to_string(), limit)
            .await
            .map_err(|e| provider_failure(&e));
    }

    match deezer_search(trimmed.to_string(), limit).await {
        Ok(res)
            if !res.tracks.is_empty() || !res.albums.is_empty() || !res.artists.is_empty() =>
        {
            Ok(res)
        }
        Ok(_) => {
            info!(
                query = trimmed,
                "Deezer unified search returned no results; falling back to MusicBrainz"
            );
            let mut fallback = mb_search(trimmed.to_string(), limit)
                .await
                .map_err(|e| provider_failure(&e))?;
            fallback.fallback_used = true;
            Ok(fallback)
        }
        Err(deezer_err) => {
            warn!(
                query = trimmed,
                error = %deezer_err,
                "Deezer unified search failed; falling back to MusicBrainz"
            );
            let mut fallback = mb_search(trimmed.to_string(), limit)
                .await
                .map_err(|e| provider_failure(&e))?;
            fallback.fallback_used = true;
            Ok(fallback)
        }
    }
}

/// Searches MusicBrainz, asking it for `limit` results.
pub async fn search(
    kind: SearchKind,
    artist: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, Failure> {
    search_in(&MusicBrainzProvider::new(), kind, artist, query, limit).await
}

/// The search on `provider`.
async fn search_in(
    provider: &dyn MetadataProvider,
    kind: SearchKind,
    artist: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, Failure> {
    let query = bound_query(query);
    let artist = artist.map(bound_query).filter(|s| !s.is_empty());
    match kind {
        SearchKind::Albums => provider.search_albums(artist, query, limit).await,
        SearchKind::Tracks => provider.search_tracks(artist, query, limit).await,
    }
    .map_err(|e| provider_failure(&e))
}

/// The release `id` (Deezer `deezer:album:{num}` or MusicBrainz release MBID) with its tracks.
pub async fn album(id: &str) -> Result<AlbumWithTracks, Failure> {
    album_with(
        &MusicBrainzProvider::new(),
        |num| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.get_album_with_tracks(num).await
        },
        id,
    )
    .await
}

async fn album_with<FD, FutD>(
    provider: &dyn MetadataProvider,
    deezer_album: FD,
    id: &str,
) -> Result<AlbumWithTracks, Failure>
where
    FD: FnOnce(u64) -> FutD,
    FutD: Future<Output = Result<Option<AlbumWithTracks>, KnowledgeError>>,
{
    if id.starts_with("deezer:album:") {
        let Some(num) = parse_deezer_album_id(id) else {
            return Err(not_found_failure(format!("Invalid Deezer album id '{id}'")));
        };
        return match deezer_album(num).await {
            Ok(Some(album)) => Ok(album),
            Ok(None) => Err(not_found_failure(format!("Deezer album {num} not found"))),
            Err(e) => Err(deezer_failure(&e)),
        };
    }
    if id.starts_with("lastfm:") {
        return Err(lastfm_id_failure(id));
    }
    let Some(mbid) = validate_mbid(id) else {
        return Err(not_found_failure(format!("Invalid album id '{id}'")));
    };
    album_in(provider, &mbid).await
}

async fn album_in(provider: &dyn MetadataProvider, id: &str) -> Result<AlbumWithTracks, Failure> {
    let Some(mbid) = validate_mbid(id) else {
        return Err(not_found_failure(format!("Invalid album id '{id}'")));
    };
    provider
        .get_album(&mbid)
        .await
        .map_err(|e| provider_failure(&e))
}

/// Full catalog album detail (metadata + ordered tracklist) for a Deezer or MusicBrainz album id.
pub async fn album_detail(id: &str) -> Result<CatalogAlbumDetail, Failure> {
    album_detail_with(
        |mbid| async move { musicbrainz::find_catalog_album_detail(&mbid).await },
        |num| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.get_catalog_album_detail(num).await
        },
        id,
    )
    .await
}

async fn album_detail_with<FM, FutM, FD, FutD>(
    mb_album_detail: FM,
    deezer_album: FD,
    id: &str,
) -> Result<CatalogAlbumDetail, Failure>
where
    FM: FnOnce(String) -> FutM,
    FutM: Future<Output = Result<CatalogAlbumDetail, MetadataError>>,
    FD: FnOnce(u64) -> FutD,
    FutD: Future<Output = Result<Option<CatalogAlbumDetail>, KnowledgeError>>,
{
    if id.starts_with("deezer:album:") {
        let Some(num) = parse_deezer_album_id(id) else {
            return Err(not_found_failure(format!("Invalid Deezer album id '{id}'")));
        };
        return match deezer_album(num).await {
            Ok(Some(detail)) => Ok(detail),
            Ok(None) => Err(not_found_failure(format!("Deezer album {num} not found"))),
            Err(e) => Err(deezer_failure(&e)),
        };
    }
    if id.starts_with("lastfm:") {
        return Err(lastfm_id_failure(id));
    }
    let Some(mbid) = validate_mbid(id) else {
        return Err(not_found_failure(format!("Invalid album id '{id}'")));
    };
    mb_album_detail(mbid).await.map_err(|e| provider_failure(&e))
}

/// Full catalog artist detail (summary, top tracks, discography) for a Deezer artist id
/// or a MusicBrainz artist MBID/name.
pub async fn artist_catalog(
    id: &str,
    name: Option<&str>,
) -> Result<Option<ArtistCatalog>, Failure> {
    artist_catalog_with(
        id,
        name,
        |num| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.get_artist_catalog(num).await
        },
        |artist_name| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.artist_id(&artist_name).await
        },
        |mbid| async move { musicbrainz::artist_catalog_by_mbid(&mbid, DISCOGRAPHY_LIMIT).await },
        |artist_name, mbid| async move {
            musicbrainz::artist_catalog(&artist_name, mbid.as_deref(), DISCOGRAPHY_LIMIT).await
        },
    )
    .await
}

async fn artist_catalog_with<FD, FutD, FId, FutId, FMId, FutMId, FM, FutM>(
    id: &str,
    name: Option<&str>,
    deezer_catalog: FD,
    deezer_artist_id: FId,
    mb_by_mbid: FMId,
    mb_by_name: FM,
) -> Result<Option<ArtistCatalog>, Failure>
where
    FD: Fn(u64) -> FutD,
    FutD: Future<Output = Result<Option<ArtistCatalog>, KnowledgeError>>,
    FId: FnOnce(String) -> FutId,
    FutId: Future<Output = Result<Option<u64>, KnowledgeError>>,
    FMId: FnOnce(String) -> FutMId,
    FutMId: Future<Output = Result<Option<ArtistCatalog>, MetadataError>>,
    FM: FnOnce(String, Option<String>) -> FutM,
    FutM: Future<Output = Result<Option<ArtistCatalog>, MetadataError>>,
{
    let trimmed_id = id.trim();
    if trimmed_id.starts_with("deezer:artist:") {
        let Some(num) = parse_deezer_artist_id(trimmed_id) else {
            return Err(not_found_failure(format!(
                "Invalid Deezer artist id '{id}'"
            )));
        };
        return deezer_catalog(num).await.map_err(|e| deezer_failure(&e));
    }

    let trimmed_name = name.map(bound_query).filter(|s| !s.is_empty());

    // Valid MusicBrainz artist MBID: fetch directly when no name was provided (H6),
    // or resolve via name + chosen MBID when both are provided.
    if let Some(canonical_mbid) = validate_mbid(trimmed_id) {
        return match trimmed_name {
            None => mb_by_mbid(canonical_mbid)
                .await
                .map_err(|e| provider_failure(&e)),
            Some(artist_name) => mb_by_name(artist_name.to_string(), Some(canonical_mbid))
                .await
                .map_err(|e| provider_failure(&e)),
        };
    }

    // No valid MBID (e.g. empty id + local artist name, or bare artist name):
    // try Deezer exact artist lookup first, falling back to MusicBrainz.
    let lookup_name = trimmed_name
        .or_else(|| {
            let bounded = bound_query(trimmed_id);
            (!bounded.is_empty()).then_some(bounded)
        });
    let Some(artist_name) = lookup_name else {
        return Ok(None);
    };

    match deezer_artist_id(artist_name.to_string()).await {
        Ok(Some(dz_id)) => match deezer_catalog(dz_id).await {
            Ok(Some(cat)) => return Ok(Some(cat)),
            Ok(None) => {
                info!(
                    artist = artist_name,
                    deezer_id = dz_id,
                    "Deezer artist catalog returned no data; falling back to MusicBrainz"
                );
            }
            Err(e) => {
                warn!(
                    artist = artist_name,
                    deezer_id = dz_id,
                    error = %e,
                    "Deezer artist catalog failed; falling back to MusicBrainz"
                );
            }
        },
        Ok(None) => {
            info!(
                artist = artist_name,
                "Deezer artist lookup found no exact match; falling back to MusicBrainz"
            );
        }
        Err(e) => {
            warn!(
                artist = artist_name,
                error = %e,
                "Deezer artist lookup failed; falling back to MusicBrainz"
            );
        }
    }

    mb_by_name(artist_name.to_string(), None)
        .await
        .map_err(|e| provider_failure(&e))
}

pub(crate) fn provider_failure(e: &MetadataError) -> Failure {
    e.failure(Stage::Metadata, e.to_string())
}

/// Artists MusicBrainz finds by name or alias.
pub async fn artists(query: &str, limit: u8) -> Result<Vec<ArtistMatch>, Failure> {
    musicbrainz::search_artists(bound_query(query), limit)
        .await
        .map_err(|e| provider_failure(&e))
}

/// Rows the web UI's artist-only search shows.
pub(crate) const DISCOGRAPHY_LIMIT: usize = musicbrainz::DISCOGRAPHY_MAX;

/// The first `limit` rows of the artist named `name`, its albums and EPs
/// then its singles, or of the one
/// of that name whose MBID is `chosen`; `ArtistAlbums` says when MusicBrainz
/// has no artist of that name, or none of them is `chosen`.
pub async fn discography(
    name: &str,
    chosen: Option<&str>,
    limit: usize,
) -> Result<ArtistAlbums, Failure> {
    musicbrainz::discography(name, chosen, limit)
        .await
        .map_err(|e| provider_failure(&e))
}

/// The recording `mbid` on MusicBrainz and a page of the releases it is on.
pub async fn recording_releases(
    mbid: &str,
    offset: u16,
    limit: u8,
) -> Result<RecordingReleases, Failure> {
    musicbrainz::recording_releases(mbid, offset, limit)
        .await
        .map_err(|e| provider_failure(&e))
}

/// Album queries arrive from the UI with an empty track list; source matching
/// scores candidate files against expected track titles, so resolve the
/// album's tracklist through Deezer or MusicBrainz before searching. Fails only
/// for an album query.
pub async fn hydrate_album_tracks(query: &mut DownloadQuery) -> Result<(), Failure> {
    hydrate_with(
        &MusicBrainzProvider::new(),
        |num| async move {
            let dz = DEEZER_CLIENT
                .as_ref()
                .map_err(|e| KnowledgeError::Remote(e.clone()))?;
            dz.get_album_with_tracks(num).await
        },
        query,
    )
    .await
}

/// Fills an album query's empty track list from `provider`.
pub(crate) async fn hydrate_in(
    provider: &dyn MetadataProvider,
    query: &mut DownloadQuery,
) -> Result<(), Failure> {
    let Some(album) = album_to_hydrate(query) else {
        return Ok(());
    };
    if let Some(failure) = lastfm_album_failure(album) {
        return Err(failure);
    }
    let Some(mbid) = validate_mbid(&album.id) else {
        return Err(not_found_failure(format!(
            "could not resolve tracklist for '{}': invalid album id '{}'",
            album.title, album.id
        )));
    };
    let album_with_tracks = provider
        .get_album(&mbid)
        .await
        .map_err(|e| tracklist_failure(&album.title, &e))?;
    query.tracks = album_with_tracks.tracks;
    Ok(())
}

async fn hydrate_with<FD, FutD>(
    provider: &dyn MetadataProvider,
    deezer_album: FD,
    query: &mut DownloadQuery,
) -> Result<(), Failure>
where
    FD: FnOnce(u64) -> FutD,
    FutD: Future<Output = Result<Option<AlbumWithTracks>, KnowledgeError>>,
{
    let Some(album) = album_to_hydrate(query) else {
        return Ok(());
    };
    if let Some(failure) = lastfm_album_failure(album) {
        return Err(failure);
    }
    if album.id.starts_with("deezer:album:") {
        let Some(num) = parse_deezer_album_id(&album.id) else {
            return Err(not_found_failure(format!(
                "could not resolve tracklist for '{}': invalid Deezer album id '{}'",
                album.title, album.id
            )));
        };
        let album_with_tracks = match deezer_album(num).await {
            Ok(Some(album_with_tracks)) => album_with_tracks,
            Ok(None) => {
                return Err(not_found_failure(format!(
                    "could not resolve tracklist for '{}': Deezer album {num} not found",
                    album.title
                )));
            }
            Err(e) => {
                return Err(deezer_failure(&e).with_message(format!(
                    "could not resolve tracklist for '{}': {e}",
                    album.title
                )));
            }
        };
        query.tracks = album_with_tracks.tracks;
        return Ok(());
    }
    hydrate_in(provider, query).await
}

/// The album of an album query whose track list is still empty.
fn album_to_hydrate(query: &DownloadQuery) -> Option<&Album> {
    query.album.as_ref().filter(|_| query.tracks.is_empty())
}

/// A job kept from before Soulbeet dropped Last.fm can hold an album id a Last.fm search gave.
/// No provider can look it up, so the user searches for the album again.
fn lastfm_album_failure(album: &Album) -> Option<Failure> {
    album.id.starts_with("lastfm:").then(|| {
        Failure::new(
            FailureCode::MetadataNotFound,
            Stage::Metadata,
            RetryAdvice::OtherQuery,
            format!(
                "'{}' came from a Last.fm search, which Soulbeet no longer uses: search for it again",
                album.title
            ),
        )
    })
}

/// The tracklist of the album could not be looked up.
fn tracklist_failure(album_title: &str, e: &MetadataError) -> Failure {
    e.failure(
        Stage::Metadata,
        format!("could not resolve tracklist for '{album_title}': {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::Service;
    use shared::metadata::{CatalogTrack, Track};

    /// Fails every call the way step 3's with_retry does when MusicBrainz gives no answer.
    struct Unanswered;

    fn unanswered() -> MetadataError {
        MetadataError::Unavailable {
            service: Service::Musicbrainz,
            status: None,
            raw: "no answer within 15 s".into(),
        }
    }

    #[async_trait::async_trait]
    impl MetadataProvider for Unanswered {
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Err(unanswered())
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Err(unanswered())
        }
        async fn get_album(&self, _: &str) -> Result<AlbumWithTracks, MetadataError> {
            Err(unanswered())
        }
    }

    /// Finds nothing.
    struct Empty;

    #[async_trait::async_trait]
    impl MetadataProvider for Empty {
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(Vec::new())
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(Vec::new())
        }
        async fn get_album(&self, _: &str) -> Result<AlbumWithTracks, MetadataError> {
            Err(unanswered())
        }
    }

    /// Records the limit a search asks for, and finds nothing.
    struct AskedFor(std::sync::Mutex<Option<usize>>);

    #[async_trait::async_trait]
    impl MetadataProvider for AskedFor {
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            limit: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            *self.0.lock().unwrap() = Some(limit);
            Ok(vec![])
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            limit: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            *self.0.lock().unwrap() = Some(limit);
            Ok(vec![])
        }
        async fn get_album(&self, _: &str) -> Result<AlbumWithTracks, MetadataError> {
            Err(unanswered())
        }
    }

    #[tokio::test]
    async fn a_search_asks_the_provider_for_the_limit_given() {
        for kind in [SearchKind::Albums, SearchKind::Tracks] {
            let p = AskedFor(Default::default());
            search_in(&p, kind, None, "Silent Night", 10).await.unwrap();
            assert_eq!(*p.0.lock().unwrap(), Some(10), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_search_answers_what_the_provider_found() {
        let found = search_in(
            &Empty,
            SearchKind::Albums,
            None,
            "Winter Songs",
            SEARCH_LIMIT,
        )
        .await
        .unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn a_lookup_the_provider_cannot_answer_fails_with_its_failure() {
        let expected = (
            FailureCode::MetadataUnavailable,
            Stage::Metadata,
            RetryAdvice::SameSourceLater,
            "MusicBrainz is unavailable: no answer within 15 s",
        );
        for kind in [SearchKind::Albums, SearchKind::Tracks] {
            let f = search_in(
                &Unanswered,
                kind,
                Some("Marrow Choir"),
                "Winter Songs",
                SEARCH_LIMIT,
            )
            .await
            .unwrap_err();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                expected,
                "{kind:?}"
            );
        }
        let f = album_in(&Unanswered, "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f")
            .await
            .unwrap_err();
        assert_eq!((f.code, f.stage, f.retry, f.message.as_str()), expected);
    }

    #[tokio::test]
    async fn an_album_from_a_lastfm_search_fails_without_asking_the_provider() {
        // Both id forms a 0.9.0 job can hold (LastFmId::album and the form from before ids named their kind).
        for id in [
            "lastfm:album:marrow choir:winter songs",
            "lastfm:marrow choir:winter songs",
        ] {
            let album = Album {
                id: id.into(),
                title: "Winter Songs".into(),
                artist: "Marrow Choir".into(),
                release_date: None,
                mbid: None,
                cover_url: None,
                release_types: None,
                first_release_date: None,
            };
            let mut query = DownloadQuery::new(vec![]).album(album.clone());
            // Unanswered fails with metadata_unavailable: any other code shows it was not asked.
            let f = hydrate_in(&Unanswered, &mut query).await.unwrap_err();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str(), f.cause, f.user_action),
                (
                    FailureCode::MetadataNotFound,
                    Stage::Metadata,
                    RetryAdvice::OtherQuery,
                    "'Winter Songs' came from a Last.fm search, which Soulbeet no longer uses: search for it again",
                    None,
                    None
                )
            );
            // With its tracklist already kept, the job needs no lookup and goes on.
            let track = Track {
                id: "t1".into(),
                title: "O Holy Night".into(),
                artist: "Marrow Choir".into(),
                album_id: None,
                album_title: None,
                release_date: None,
                duration: None,
                mbid: None,
                release_mbid: None,
            };
            let mut kept = DownloadQuery::new(vec![track]).album(album);
            hydrate_in(&Unanswered, &mut kept).await.unwrap();
        }
    }

    #[tokio::test]
    async fn an_album_whose_tracklist_cannot_be_read_fails_with_its_title() {
        let album = Album {
            id: "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f".into(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
            first_release_date: None,
        };
        let mut query = DownloadQuery::new(vec![]).album(album);
        let f = hydrate_in(&Unanswered, &mut query).await.unwrap_err();
        assert_eq!(
            (f.code, f.stage),
            (FailureCode::MetadataUnavailable, Stage::Metadata)
        );
        assert_eq!(
            f.message,
            "could not resolve tracklist for 'Winter Songs': MusicBrainz is unavailable: no answer within 15 s"
        );
        assert!(query.tracks.is_empty());
    }

    #[test]
    fn a_tracklist_that_cannot_be_read_fails_at_the_metadata_stage() {
        let e = MetadataError::Unavailable {
            service: Service::Musicbrainz,
            status: Some(503),
            raw: "no answer within 15 s".into(),
        };
        let f = tracklist_failure("Winter Songs", &e);
        assert_eq!(
            (f.code, f.stage),
            (FailureCode::MetadataUnavailable, Stage::Metadata)
        );
        assert_eq!(
            f.message,
            "could not resolve tracklist for 'Winter Songs': MusicBrainz is unavailable: no answer within 15 s"
        );
    }

    fn sample_catalog_track(provider: CatalogProvider) -> CatalogTrack {
        CatalogTrack {
            id: match provider {
                CatalogProvider::Deezer => "deezer:track:1".into(),
                CatalogProvider::Musicbrainz => "mb-rec-1".into(),
            },
            title: "Digital Love".into(),
            artist: "Daft Punk".into(),
            artist_id: None,
            album_id: None,
            album_title: Some("Discovery".into()),
            release_date: Some("2001-03-12".into()),
            duration: Some("04:58".into()),
            duration_secs: Some(298),
            mbid: None,
            release_mbid: None,
            cover_url: None,
            variant_label: None,
            provider,
        }
    }

    fn sample_unified(
        query: &str,
        provider: CatalogProvider,
        with_hit: bool,
    ) -> UnifiedCatalogSearch {
        UnifiedCatalogSearch {
            query: query.into(),
            provider,
            fallback_used: false,
            tracks: if with_hit {
                vec![sample_catalog_track(provider)]
            } else {
                vec![]
            },
            albums: vec![],
            artists: vec![],
        }
    }

    #[tokio::test]
    async fn unified_search_returns_deezer_hits_without_fallback() {
        let mb_called = std::sync::atomic::AtomicBool::new(false);
        let res = search_unified_with(
            "  daft punk  ",
            None,
            100,
            |q, lim| async move {
                assert_eq!(q, "daft punk");
                assert_eq!(lim, 50);
                Ok(sample_unified(&q, CatalogProvider::Deezer, true))
            },
            |_q, _lim| {
                mb_called.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(sample_unified("daft punk", CatalogProvider::Musicbrainz, true)) }
            },
        )
        .await
        .unwrap();

        assert_eq!(res.provider, CatalogProvider::Deezer);
        assert!(!res.fallback_used);
        assert_eq!(res.tracks.len(), 1);
        assert!(!mb_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unified_search_falls_back_to_musicbrainz_when_deezer_returns_empty() {
        let res = search_unified_with(
            "obscure bootleg",
            Some(CatalogProvider::Deezer),
            0,
            |q, lim| async move {
                assert_eq!(lim, 1);
                Ok(sample_unified(&q, CatalogProvider::Deezer, false))
            },
            |q, lim| async move {
                assert_eq!(lim, 1);
                Ok(sample_unified(&q, CatalogProvider::Musicbrainz, true))
            },
        )
        .await
        .unwrap();

        assert_eq!(res.provider, CatalogProvider::Musicbrainz);
        assert!(res.fallback_used);
        assert_eq!(res.tracks.len(), 1);
    }

    #[tokio::test]
    async fn unified_search_falls_back_to_musicbrainz_when_deezer_errors() {
        let res = search_unified_with(
            "daft punk",
            None,
            UNIFIED_SEARCH_LIMIT,
            |_q, _lim| async {
                Err(KnowledgeError::Remote(RemoteError::Unavailable {
                    service: "Deezer",
                    detail: "connection refused".into(),
                }))
            },
            |q, lim| async move {
                assert_eq!(lim, UNIFIED_SEARCH_LIMIT);
                Ok(sample_unified(&q, CatalogProvider::Musicbrainz, true))
            },
        )
        .await
        .unwrap();

        assert_eq!(res.provider, CatalogProvider::Musicbrainz);
        assert!(res.fallback_used);
        assert_eq!(res.tracks.len(), 1);
    }

    #[tokio::test]
    async fn unified_search_explicit_musicbrainz_skips_deezer() {
        let deezer_called = std::sync::atomic::AtomicBool::new(false);
        let res = search_unified_with(
            "radiohead",
            Some(CatalogProvider::Musicbrainz),
            10,
            |_q, _lim| {
                deezer_called.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(sample_unified("radiohead", CatalogProvider::Deezer, true)) }
            },
            |q, lim| async move {
                assert_eq!(q, "radiohead");
                assert_eq!(lim, 10);
                Ok(sample_unified(&q, CatalogProvider::Musicbrainz, true))
            },
        )
        .await
        .unwrap();

        assert_eq!(res.provider, CatalogProvider::Musicbrainz);
        assert!(!res.fallback_used);
        assert!(!deezer_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unified_search_blank_query_returns_empty_without_calling_providers() {
        let res = search_unified_with(
            "   ",
            None,
            UNIFIED_SEARCH_LIMIT,
            |_q, _lim| async { panic!("deezer should not be called") },
            |_q, _lim| async { panic!("musicbrainz should not be called") },
        )
        .await
        .unwrap();

        assert!(res.query.is_empty());
        assert_eq!(res.provider, CatalogProvider::Deezer);
        assert!(!res.fallback_used);
        assert!(res.tracks.is_empty() && res.albums.is_empty() && res.artists.is_empty());
    }

    #[tokio::test]
    async fn deezer_album_id_dispatches_to_deezer_in_album_and_hydrate_while_lastfm_rejects() {
        let dz_album = Album {
            id: "deezer:album:1408080".into(),
            title: "Discovery".into(),
            artist: "Daft Punk".into(),
            release_date: Some("2001-03-12".into()),
            mbid: None,
            cover_url: None,
            release_types: Some(vec!["Album".into()]),
            first_release_date: Some("2001-03-12".into()),
        };
        let dz_track = Track {
            id: "deezer:track:3135553".into(),
            title: "One More Time".into(),
            artist: "Daft Punk".into(),
            album_id: Some("deezer:album:1408080".into()),
            album_title: Some("Discovery".into()),
            release_date: Some("2001-03-12".into()),
            duration: Some("05:20".into()),
            mbid: None,
            release_mbid: None,
        };
        let expected = AlbumWithTracks {
            album: dz_album.clone(),
            tracks: vec![dz_track.clone()],
        };

        // album() dispatches deezer:album:1408080 to Deezer without touching MusicBrainz (Unanswered).
        let got = album_with(
            &Unanswered,
            |num| {
                let expected = expected.clone();
                async move {
                    assert_eq!(num, 1408080);
                    Ok(Some(expected))
                }
            },
            "deezer:album:1408080",
        )
        .await
        .unwrap();
        assert_eq!(got, expected);

        // Invalid numeric suffix fails with MetadataNotFound.
        let invalid = album_with(
            &Unanswered,
            |_num| async { panic!("should not call deezer") },
            "deezer:album:not-a-number",
        )
        .await
        .unwrap_err();
        assert_eq!(invalid.code, FailureCode::MetadataNotFound);

        // Deezer Ok(None) fails with MetadataNotFound.
        let missing = album_with(
            &Unanswered,
            |_num| async { Ok(None) },
            "deezer:album:1408080",
        )
        .await
        .unwrap_err();
        assert_eq!(missing.code, FailureCode::MetadataNotFound);

        // Deezer Err fails with MetadataUnavailable.
        let unavailable = album_with(
            &Unanswered,
            |_num| async {
                Err(KnowledgeError::Remote(RemoteError::Unavailable {
                    service: "Deezer",
                    detail: "down".into(),
                }))
            },
            "deezer:album:1408080",
        )
        .await
        .unwrap_err();
        assert_eq!(unavailable.code, FailureCode::MetadataUnavailable);

        // lastfm: rejects without hitting either provider.
        let lastfm_err = album_with(
            &Unanswered,
            |_num| async { panic!("should not call deezer") },
            "lastfm:album:daft punk:discovery",
        )
        .await
        .unwrap_err();
        assert_eq!(lastfm_err.code, FailureCode::MetadataNotFound);

        // hydrate_album_tracks dispatches deezer:album:1408080 to Deezer.
        let mut query = DownloadQuery::new(vec![]).album(dz_album);
        hydrate_with(
            &Unanswered,
            |num| {
                let expected = expected.clone();
                async move {
                    assert_eq!(num, 1408080);
                    Ok(Some(expected))
                }
            },
            &mut query,
        )
        .await
        .unwrap();
        assert_eq!(query.tracks, vec![dz_track]);

        // hydrate_album_tracks still rejects lastfm: albums.
        let lastfm_album = Album {
            id: "lastfm:album:daft punk:discovery".into(),
            title: "Discovery".into(),
            artist: "Daft Punk".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
            first_release_date: None,
        };
        let mut lastfm_query = DownloadQuery::new(vec![]).album(lastfm_album);
        let err = hydrate_with(
            &Unanswered,
            |_num| async { panic!("should not call deezer") },
            &mut lastfm_query,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::MetadataNotFound);
    }

    #[tokio::test]
    async fn invalid_non_uuid_album_ids_are_rejected_before_touching_musicbrainz() {
        for bad_id in ["deezer:track:123", "../artist/55", "not-a-uuid", ""] {
            let err = album_with(
                &Unanswered,
                |_num| async { panic!("deezer should not be called") },
                bad_id,
            )
            .await
            .unwrap_err();
            assert_eq!(
                (err.code, err.stage, err.retry),
                (
                    FailureCode::MetadataNotFound,
                    Stage::Metadata,
                    RetryAdvice::OtherQuery
                ),
                "album_with should reject {bad_id:?}"
            );

            let detail_err = album_detail_with(
                |_mbid| async { panic!("musicbrainz should not be called") },
                |_num| async { panic!("deezer should not be called") },
                bad_id,
            )
            .await
            .unwrap_err();
            assert_eq!(
                (detail_err.code, detail_err.stage, detail_err.retry),
                (
                    FailureCode::MetadataNotFound,
                    Stage::Metadata,
                    RetryAdvice::OtherQuery
                ),
                "album_detail_with should reject {bad_id:?}"
            );

            let bad_album = Album {
                id: bad_id.into(),
                title: "Bad Album".into(),
                artist: "Artist".into(),
                release_date: None,
                mbid: None,
                cover_url: None,
                release_types: None,
                first_release_date: None,
            };
            let mut query = DownloadQuery::new(vec![]).album(bad_album);
            let hydrate_err = hydrate_with(
                &Unanswered,
                |_num| async { panic!("deezer should not be called") },
                &mut query,
            )
            .await
            .unwrap_err();
            assert_eq!(
                (hydrate_err.code, hydrate_err.stage, hydrate_err.retry),
                (
                    FailureCode::MetadataNotFound,
                    Stage::Metadata,
                    RetryAdvice::OtherQuery
                ),
                "hydrate_with should reject {bad_id:?}"
            );
        }
    }

    #[tokio::test]
    async fn unified_search_truncates_oversized_queries_at_char_boundary() {
        let huge = "é".repeat(5_000);
        let res = search_unified_with(
            &huge,
            None,
            10,
            |q, _lim| async move {
                assert_eq!(q.chars().count(), MAX_SEARCH_QUERY_CHARS);
                Ok(sample_unified(&q, CatalogProvider::Deezer, true))
            },
            |_q, _lim| async { panic!("musicbrainz should not be called") },
        )
        .await
        .unwrap();
        assert_eq!(res.query.chars().count(), MAX_SEARCH_QUERY_CHARS);
    }

    fn sample_artist_catalog(id: &str, name: &str, provider: CatalogProvider) -> ArtistCatalog {
        ArtistCatalog {
            artist: shared::metadata::CatalogArtist {
                id: id.into(),
                name: name.into(),
                disambiguation: None,
                picture_url: None,
                fan_count: None,
                mbid: (provider == CatalogProvider::Musicbrainz).then(|| id.into()),
                provider,
            },
            provider,
            others: vec![],
            top_tracks: vec![],
            albums: vec![],
            albums_complete: true,
            singles_complete: true,
        }
    }

    #[tokio::test]
    async fn artist_catalog_handles_mbid_only_local_name_deezer_first_and_fallback() {
        let valid_mbid = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";

        // H6: MBID-only dispatches to artist_catalog_by_mbid.
        let by_mbid = artist_catalog_with(
            valid_mbid,
            None,
            |_num| async { panic!("deezer should not be called") },
            |_name| async { panic!("deezer artist_id should not be called") },
            |mbid| async move {
                assert_eq!(mbid, valid_mbid);
                Ok(Some(sample_artist_catalog(
                    &mbid,
                    "Nirvana",
                    CatalogProvider::Musicbrainz,
                )))
            },
            |_name, _mbid| async { panic!("mb_by_name should not be called") },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(by_mbid.artist.name, "Nirvana");
        assert_eq!(by_mbid.provider, CatalogProvider::Musicbrainz);

        // MBID + name dispatches to MusicBrainz artist_catalog with both.
        let by_mbid_and_name = artist_catalog_with(
            valid_mbid,
            Some("Nirvana"),
            |_num| async { panic!("deezer should not be called") },
            |_name| async { panic!("deezer artist_id should not be called") },
            |_mbid| async { panic!("mb_by_mbid should not be called") },
            |name, mbid| async move {
                assert_eq!(name, "Nirvana");
                assert_eq!(mbid.as_deref(), Some(valid_mbid));
                Ok(Some(sample_artist_catalog(
                    valid_mbid,
                    "Nirvana",
                    CatalogProvider::Musicbrainz,
                )))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(by_mbid_and_name.artist.id, valid_mbid);

        // M7: Empty id + artist name tries Deezer artist_id first and returns Deezer catalog.
        let local_via_deezer = artist_catalog_with(
            "",
            Some("Daft Punk"),
            |num| async move {
                assert_eq!(num, 27);
                Ok(Some(sample_artist_catalog(
                    "deezer:artist:27",
                    "Daft Punk",
                    CatalogProvider::Deezer,
                )))
            },
            |name| async move {
                assert_eq!(name, "Daft Punk");
                Ok(Some(27))
            },
            |_mbid| async { panic!("mb_by_mbid should not be called") },
            |_name, _mbid| async { panic!("mb_by_name should not be called") },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(local_via_deezer.provider, CatalogProvider::Deezer);
        assert_eq!(local_via_deezer.artist.id, "deezer:artist:27");

        // M7 fallback: Empty id + artist name falls back to MusicBrainz when Deezer has no match.
        let local_fallback_mb = artist_catalog_with(
            "",
            Some("Obscure Local Band"),
            |_num| async { panic!("deezer_catalog should not be called") },
            |_name| async { Ok(None) },
            |_mbid| async { panic!("mb_by_mbid should not be called") },
            |name, mbid| async move {
                assert_eq!(name, "Obscure Local Band");
                assert_eq!(mbid, None);
                Ok(Some(sample_artist_catalog(
                    valid_mbid,
                    "Obscure Local Band",
                    CatalogProvider::Musicbrainz,
                )))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(local_fallback_mb.provider, CatalogProvider::Musicbrainz);

        // M7 fallback on Deezer error: falls back to MusicBrainz.
        let local_fallback_on_err = artist_catalog_with(
            "",
            Some("Daft Punk"),
            |_num| async { panic!("deezer_catalog should not be called") },
            |_name| async {
                Err(KnowledgeError::Remote(RemoteError::Unavailable {
                    service: "Deezer",
                    detail: "down".into(),
                }))
            },
            |_mbid| async { panic!("mb_by_mbid should not be called") },
            |name, mbid| async move {
                assert_eq!(name, "Daft Punk");
                assert_eq!(mbid, None);
                Ok(Some(sample_artist_catalog(
                    valid_mbid,
                    "Daft Punk",
                    CatalogProvider::Musicbrainz,
                )))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(local_fallback_on_err.provider, CatalogProvider::Musicbrainz);

        // Non-UUID id without name is treated as artist name (Deezer first).
        let bare_name = artist_catalog_with(
            "Daft Punk",
            None,
            |num| async move {
                assert_eq!(num, 27);
                Ok(Some(sample_artist_catalog(
                    "deezer:artist:27",
                    "Daft Punk",
                    CatalogProvider::Deezer,
                )))
            },
            |name| async move {
                assert_eq!(name, "Daft Punk");
                Ok(Some(27))
            },
            |_mbid| async { panic!("mb_by_mbid should not be called") },
            |_name, _mbid| async { panic!("mb_by_name should not be called") },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bare_name.provider, CatalogProvider::Deezer);
    }
}
