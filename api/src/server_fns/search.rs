use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::{
    download::{DownloadQuery, SearchResult as DownloadSearchResult},
    metadata::{AlbumWithTracks, Discography, SearchResult},
};

#[cfg(feature = "server")]
use crate::core::metadata::{self, SearchKind};
#[cfg(feature = "server")]
use crate::core::sources;
#[cfg(feature = "server")]
use crate::{server_fns::failure_error, AuthSession};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
#[cfg(feature = "server")]
use soulbeet::musicbrainz::ArtistAlbums;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
}

/// An artist-only search: the artist's name, and the MBID of the one of that
/// name to list when it is not the first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscographyQuery {
    pub artist: String,
    pub mbid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumQuery {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollQuery {
    pub search_id: String,
}

#[post("/api/metadata/search/album", _: AuthSession)]
pub async fn search_album(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    metadata::search(
        SearchKind::Albums,
        input.artist.as_deref(),
        &input.query,
        metadata::SEARCH_LIMIT,
    )
    .await
    .map_err(failure_error)
}

#[post("/api/metadata/search/track", _: AuthSession)]
pub async fn search_track(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    metadata::search(
        SearchKind::Tracks,
        input.artist.as_deref(),
        &input.query,
        metadata::SEARCH_LIMIT,
    )
    .await
    .map_err(failure_error)
}

/// The page's answer to an artist-only search: None when no artist carries
/// the name. A picked artist MusicBrainz no longer finds under the name is
/// refused, so the page says so instead of listing another.
#[cfg(feature = "server")]
fn discography_answer(found: ArtistAlbums) -> Result<Option<Discography>, Failure> {
    match found {
        ArtistAlbums::NoArtist => Ok(None),
        ArtistAlbums::Found(discography) => Ok(Some(discography)),
        ArtistAlbums::NotAmong(_) => Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "MusicBrainz no longer finds the artist picked under this name. Search again to pick one.",
        )),
    }
}

#[post("/api/metadata/discography", _: AuthSession)]
pub async fn artist_discography(
    input: DiscographyQuery,
) -> Result<Option<Discography>, ServerFnError> {
    metadata::discography(
        &input.artist,
        input.mbid.as_deref(),
        metadata::DISCOGRAPHY_LIMIT,
    )
    .await
    .and_then(discography_answer)
    .map_err(failure_error)
}

#[post("/api/metadata/album", _: AuthSession)]
pub async fn find_album(input: AlbumQuery) -> Result<AlbumWithTracks, ServerFnError> {
    metadata::album(&input.id).await.map_err(failure_error)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tab opened before the upgrade still sends `provider`.
    #[test]
    fn a_request_from_a_tab_opened_before_the_upgrade_still_reads() {
        let q: SearchQuery = serde_json::from_value(
            serde_json::json!({"artist": null, "query": "Winter Songs", "provider": "lastfm"}),
        )
        .unwrap();
        assert_eq!(
            q,
            SearchQuery {
                artist: None,
                query: "Winter Songs".into()
            }
        );
        let a: AlbumQuery = serde_json::from_value(
            serde_json::json!({"id": "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f", "provider": "lastfm"}),
        )
        .unwrap();
        assert_eq!(
            a,
            AlbumQuery {
                id: "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f".into()
            }
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn a_picked_artist_no_longer_found_is_refused() {
        use shared::metadata::ArtistSummary;
        assert_eq!(discography_answer(ArtistAlbums::NoArtist), Ok(None));
        let nirvana = ArtistSummary {
            mbid: "5b11f4ce-a62d-471e-81fc-a69a8278c7da".into(),
            name: "Nirvana".into(),
            disambiguation: None,
        };
        let found = Discography {
            artist: nirvana.clone(),
            others: vec![],
            albums: vec![],
            albums_complete: true,
            singles_complete: true,
        };
        assert_eq!(
            discography_answer(ArtistAlbums::Found(found.clone())),
            Ok(Some(found))
        );
        let refused = discography_answer(ArtistAlbums::NotAmong(vec![nirvana])).unwrap_err();
        assert_eq!(
            (refused.code, refused.message.as_str()),
            (
                FailureCode::InvalidInput,
                "MusicBrainz no longer finds the artist picked under this name. Search again to pick one."
            )
        );
    }
}
