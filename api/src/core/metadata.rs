//! Metadata search, album lookup and an album query's tracklist.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::models::user_settings::UserSettings;
use crate::services::{metadata_provider, providers, usable_lastfm_key};
use shared::download::DownloadQuery;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{Album, AlbumWithTracks, Provider, SearchResults};
use soulbeet::musicbrainz::{self, ArtistMatch, RecordingReleases};
use soulbeet::{MetadataError, MetadataProvider};
use std::sync::Arc;

/// Which provider search runs: search_albums or search_tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Albums,
    Tracks,
}

/// Results the web UI's searches ask for.
pub(crate) const SEARCH_LIMIT: usize = 25;

/// The user's Last.fm key, which picks and authenticates Last.fm.
pub(crate) async fn lastfm_key(ctx: &UserCtx) -> Result<Option<String>, Failure> {
    UserSettings::get(&ctx.user_id)
        .await
        .map(|settings| settings.lastfm_api_key)
        .map_err(settings_unreadable)
}

/// The user's settings could not be read, which the metadata lookups need.
fn settings_unreadable(e: impl std::fmt::Display) -> Failure {
    database_failure(Stage::Metadata, e).with_message("Could not read your settings")
}

/// Searches `provider`, or MusicBrainz when it is unset or cannot be used,
/// asking it for `limit` results.
pub async fn search(
    ctx: &UserCtx,
    kind: SearchKind,
    artist: Option<&str>,
    query: &str,
    provider: Option<&str>,
    limit: usize,
) -> Result<SearchResults, Failure> {
    let key = lastfm_key(ctx).await?;
    let provider = metadata_provider(provider, key.as_deref());
    search_in(&*provider, kind, artist, query, limit).await
}

/// The search on `provider`, the results named by its id.
async fn search_in(
    provider: &dyn MetadataProvider,
    kind: SearchKind,
    artist: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<SearchResults, Failure> {
    let results = match kind {
        SearchKind::Albums => provider.search_albums(artist, query, limit).await,
        SearchKind::Tracks => provider.search_tracks(artist, query, limit).await,
    }
    .map_err(|e| provider_failure(&e))?;
    Ok(SearchResults {
        provider: provider.id(),
        results,
    })
}

/// The album `id` a search by `provider` gave, with its tracks.
pub async fn album(
    ctx: &UserCtx,
    id: &str,
    provider: Option<Provider>,
) -> Result<AlbumWithTracks, Failure> {
    let key = lastfm_key(ctx).await?;
    let lookup = album_lookup_provider(provider, id).map(|p| p.to_string());
    let provider = metadata_provider(lookup.as_deref(), key.as_deref());
    album_in(&*provider, id).await
}

async fn album_in(provider: &dyn MetadataProvider, id: &str) -> Result<AlbumWithTracks, Failure> {
    provider
        .get_album(id)
        .await
        .map_err(|e| provider_failure(&e))
}

/// The provider that looks up the album `id` a search by `searched` gave.
/// A Last.fm search gives an album Last.fm has an MBID for that MBID as its
/// id: a MusicBrainz release id, which MusicBrainz looks up whichever search
/// gave it. Only Last.fm's own ids go to Last.fm, in either form: the web
/// UI and a resumed job can hold an album id written before ids named their
/// kind (`LastFmId::album_in_any_form`).
fn album_lookup_provider(searched: Option<Provider>, id: &str) -> Option<Provider> {
    match searched {
        Some(Provider::LastFm) if !soulbeet::lastfm::is_album_id_in_any_form(id) => {
            Some(Provider::MusicBrainz)
        }
        searched => searched,
    }
}

pub(crate) fn provider_failure(e: &MetadataError) -> Failure {
    e.failure(Stage::Metadata, e.to_string())
}

/// The release `id` (an MBID, or a `lastfm:` id) with its tracks and the
/// provider that answered (D6).
pub async fn release(ctx: &UserCtx, id: &str) -> Result<(AlbumWithTracks, Provider), Failure> {
    let key = lastfm_key(ctx).await?;
    let provider = release_provider(id, key.as_deref())?;
    release_in(&*provider, id).await
}

/// Last.fm for a `lastfm:` id, which needs a saved Last.fm key, else
/// MusicBrainz. `metadata_provider` would give MusicBrainz for a `lastfm:` id
/// when no key is saved, and MusicBrainz cannot look such an id up.
fn release_provider(
    id: &str,
    lastfm_key: Option<&str>,
) -> Result<Arc<dyn MetadataProvider>, Failure> {
    let lastfm = soulbeet::lastfm::is_album_id(id);
    if lastfm && usable_lastfm_key(lastfm_key).is_none() {
        return Err(Failure::new(
            FailureCode::ProviderNotConfigured,
            Stage::Metadata,
            RetryAdvice::NeedsUser,
            "A lastfm: release id needs a Last.fm API key saved in Soulful's settings",
        )
        .with_user_action(soulbeet::lastfm::configure_lastfm()));
    }
    Ok(metadata_provider(
        lastfm.then_some(providers::LASTFM),
        lastfm_key,
    ))
}

async fn release_in(
    provider: &dyn MetadataProvider,
    id: &str,
) -> Result<(AlbumWithTracks, Provider), Failure> {
    album_in(provider, id).await.map(|a| (a, provider.id()))
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
/// album's tracklist through the metadata provider before searching. Fails
/// only for an album query.
pub async fn hydrate_album_tracks(
    query: &mut DownloadQuery,
    lastfm_api_key: Option<&str>,
) -> Result<(), Failure> {
    let Some(album) = album_to_hydrate(query) else {
        return Ok(());
    };
    let lookup = album_lookup_provider(query.metadata_provider, &album.id).map(|p| p.to_string());
    let provider = metadata_provider(lookup.as_deref(), lastfm_api_key);
    hydrate_in(&*provider, query).await
}

/// Fills an album query's empty track list from `provider`.
pub(crate) async fn hydrate_in(
    provider: &dyn MetadataProvider,
    query: &mut DownloadQuery,
) -> Result<(), Failure> {
    let Some(album) = album_to_hydrate(query) else {
        return Ok(());
    };
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
    use shared::metadata::SearchResult as MetadataSearchResult;
    use soulbeet::lastfm::LastFmId;

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
        fn id(&self) -> Provider {
            Provider::MusicBrainz
        }
        fn name(&self) -> &'static str {
            "MusicBrainz"
        }
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
            Err(unanswered())
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
            Err(unanswered())
        }
        async fn get_album(&self, _: &str) -> Result<AlbumWithTracks, MetadataError> {
            Err(unanswered())
        }
    }

    /// Finds nothing, as the provider it names.
    struct Empty(Provider);

    #[async_trait::async_trait]
    impl MetadataProvider for Empty {
        fn id(&self) -> Provider {
            self.0
        }
        fn name(&self) -> &'static str {
            "Empty"
        }
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
            Ok(Vec::new())
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
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
        fn id(&self) -> Provider {
            Provider::MusicBrainz
        }
        fn name(&self) -> &'static str {
            "MusicBrainz"
        }
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            limit: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
            *self.0.lock().unwrap() = Some(limit);
            Ok(vec![])
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            limit: usize,
        ) -> Result<Vec<MetadataSearchResult>, MetadataError> {
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
    async fn a_release_the_provider_cannot_answer_fails_with_its_failure() {
        let f = release_in(&Unanswered, "317d1e85-3f9e-463e-939b-bebf47aa2178")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::MetadataUnavailable,
                Stage::Metadata,
                "MusicBrainz is unavailable: no answer within 15 s"
            )
        );
    }

    #[test]
    fn a_release_id_is_looked_up_by_the_provider_it_belongs_to() {
        let mbid = "317d1e85-3f9e-463e-939b-bebf47aa2178";
        let lastfm = &LastFmId::album("Bing Crosby", "White Christmas").id();
        let provider = |id, key| release_provider(id, key).map(|p| p.id());
        assert_eq!(provider(mbid, None).unwrap(), Provider::MusicBrainz);
        assert_eq!(provider(mbid, Some("a1b2")).unwrap(), Provider::MusicBrainz);
        assert_eq!(provider(lastfm, Some("a1b2")).unwrap(), Provider::LastFm);
        for key in [None, Some("")] {
            let f = provider(lastfm, key).unwrap_err();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (
                    FailureCode::ProviderNotConfigured,
                    Stage::Metadata,
                    RetryAdvice::NeedsUser,
                    "A lastfm: release id needs a Last.fm API key saved in Soulful's settings"
                )
            );
            assert_eq!(f.user_action, Some(soulbeet::lastfm::configure_lastfm()));
        }
    }

    #[tokio::test]
    async fn search_results_name_the_provider_that_answered() {
        for provider in [Provider::LastFm, Provider::MusicBrainz] {
            let found = search_in(
                &Empty(provider),
                SearchKind::Albums,
                None,
                "Winter Songs",
                SEARCH_LIMIT,
            )
            .await
            .unwrap();
            assert_eq!(found.provider, provider);
        }
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

    #[test]
    fn the_last_fm_key_read_keeps_its_text() {
        let f = settings_unreadable("error returned from database: (code: 5) database is locked");
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::Metadata,
                RetryAdvice::SameSourceLater,
                "Could not read your settings"
            )
        );
        assert_eq!(f.cause.unwrap().service, Service::Database);
    }

    #[test]
    fn an_album_id_is_looked_up_by_the_provider_it_belongs_to() {
        let mbid = "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f";
        let lastfm = &LastFmId::album("Marrow Choir", "Winter Songs").id();
        assert_eq!(
            album_lookup_provider(Some(Provider::LastFm), lastfm),
            Some(Provider::LastFm)
        );
        // An album id the web UI or a job kept from before ids named their kind.
        assert_eq!(
            album_lookup_provider(Some(Provider::LastFm), "lastfm:marrow choir:winter songs"),
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
