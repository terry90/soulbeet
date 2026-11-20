//! Metadata search, album lookup and an album query's tracklist.

use shared::download::DownloadQuery;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{Album, AlbumWithTracks, SearchResult};
use soulbeet::musicbrainz::{self, ArtistMatch, MusicBrainzProvider, RecordingReleases};
use soulbeet::{MetadataError, MetadataProvider};

/// Which provider search runs: search_albums or search_tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Albums,
    Tracks,
}

/// Results the web UI's searches ask for.
pub(crate) const SEARCH_LIMIT: usize = 25;

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
    match kind {
        SearchKind::Albums => provider.search_albums(artist, query, limit).await,
        SearchKind::Tracks => provider.search_tracks(artist, query, limit).await,
    }
    .map_err(|e| provider_failure(&e))
}

/// The MusicBrainz release `id` with its tracks.
pub async fn album(id: &str) -> Result<AlbumWithTracks, Failure> {
    album_in(&MusicBrainzProvider::new(), id).await
}

async fn album_in(provider: &dyn MetadataProvider, id: &str) -> Result<AlbumWithTracks, Failure> {
    provider
        .get_album(id)
        .await
        .map_err(|e| provider_failure(&e))
}

pub(crate) fn provider_failure(e: &MetadataError) -> Failure {
    e.failure(Stage::Metadata, e.to_string())
}

/// Artists MusicBrainz finds by name.
pub async fn artists(query: &str, limit: u8) -> Result<Vec<ArtistMatch>, Failure> {
    musicbrainz::search_artists(query, limit)
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
/// album's tracklist through MusicBrainz before searching. Fails only for an
/// album query.
pub async fn hydrate_album_tracks(query: &mut DownloadQuery) -> Result<(), Failure> {
    hydrate_in(&MusicBrainzProvider::new(), query).await
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
    let album_with_tracks = provider
        .get_album(&album.id)
        .await
        .map_err(|e| tracklist_failure(&album.title, &e))?;
    query.tracks = album_with_tracks.tracks;
    Ok(())
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
    use shared::metadata::Track;

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
}
