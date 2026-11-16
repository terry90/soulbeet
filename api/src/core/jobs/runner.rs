//! What MCP acquisition acts through: the database, the ledger, the one line of MCP searches
//! and the effects outside them.

use crate::auth::UserCtx;
use crate::core::sources::Searches;
use crate::core::{backend_unavailable, metadata};
use crate::ledger::{Ledger, LEDGER};
use crate::services::download_backend;
use async_trait::async_trait;
use shared::download::DownloadQuery;
use shared::failure::{Failure, Stage};
use soulbeet::DownloadBackend;
use sqlx::SqlitePool;
use std::sync::{Arc, LazyLock};

/// What acquisition does outside the ledger, so tests run it on stubs.
#[async_trait]
pub(crate) trait Effects: Send + Sync {
    /// The default download backend.
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure>;
    /// Fills an album query's empty tracklist from its metadata provider.
    async fn hydrate(&self, ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure>;
}

/// Everything a tool and the job runner act through.
pub(crate) struct Acquire {
    pub db: SqlitePool,
    pub ledger: Ledger,
    pub effects: Arc<dyn Effects>,
    /// One MCP search at a time.
    pub searches: Arc<Searches>,
}

/// The server's: its database, LEDGER, the live effects and one line of searches.
pub(crate) static ACQUIRE: LazyLock<Arc<Acquire>> = LazyLock::new(|| {
    Arc::new(Acquire {
        db: (*crate::db::DB).clone(),
        ledger: LEDGER.clone(),
        effects: Arc::new(LiveEffects),
        searches: Arc::default(),
    })
});

/// The services the server is configured with.
struct LiveEffects;

#[async_trait]
impl Effects for LiveEffects {
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
        download_backend(None)
            .await
            .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))
    }

    async fn hydrate(&self, ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure> {
        let key = metadata::lastfm_key(ctx).await?;
        metadata::hydrate_album_tracks(query, key.as_deref()).await
    }
}

#[cfg(test)]
pub(crate) use test_effects::TestEffects;

#[cfg(test)]
mod test_effects {
    use super::*;
    use crate::server_fns::download::test_support::StubBackend;
    use shared::metadata::{AlbumWithTracks, Provider, SearchResult, Track};
    use soulbeet::{MetadataError, MetadataProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::watch;

    impl Acquire {
        /// An `Acquire` on `db` with its own ledger writer and line of searches, acting through
        /// the `TestEffects` it returns.
        pub(crate) fn for_test(db: SqlitePool) -> (Arc<Acquire>, Arc<TestEffects>) {
            let effects = Arc::new(TestEffects {
                stub: StubBackend::new(vec![]),
                tracks: Mutex::default(),
                hydrate_error: Mutex::default(),
                backend_held: watch::Sender::new(false),
                backend_calls_held: AtomicUsize::new(0),
            });
            let acquire = Acquire {
                ledger: Ledger::start(db.clone()),
                db,
                effects: effects.clone(),
                searches: Arc::default(),
            };
            (Arc::new(acquire), effects)
        }
    }

    /// Answers the `StubBackend` it holds, and hydrates through step 3's `hydrate_in` on a
    /// provider whose album has the tracks `set_tracks` gave (none until then), or whose
    /// `get_album` answers the error `fail_hydrate` gave.
    pub(crate) struct TestEffects {
        stub: Arc<StubBackend>,
        tracks: Mutex<Vec<Track>>,
        hydrate_error: Mutex<Option<MetadataError>>,
        /// While true, `backend` calls wait for it to turn false.
        backend_held: watch::Sender<bool>,
        backend_calls_held: AtomicUsize,
    }

    impl TestEffects {
        pub(crate) fn stub(&self) -> &StubBackend {
            &self.stub
        }

        pub(crate) fn set_tracks(&self, tracks: Vec<Track>) {
            *self.tracks.lock().unwrap() = tracks;
        }

        pub(crate) fn fail_hydrate(&self, e: MetadataError) {
            *self.hydrate_error.lock().unwrap() = Some(e);
        }

        /// Holds every `backend` call from now on until `release_backend`.
        pub(crate) fn hold_backend(&self) {
            self.backend_held.send_replace(true);
        }

        pub(crate) fn release_backend(&self) {
            self.backend_held.send_replace(false);
        }

        /// The `backend` calls waiting for `release_backend`.
        pub(crate) fn backend_calls_held(&self) -> usize {
            self.backend_calls_held.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Effects for TestEffects {
        async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
            let mut held = self.backend_held.subscribe();
            if *held.borrow_and_update() {
                self.backend_calls_held.fetch_add(1, Ordering::SeqCst);
                let _ = held.wait_for(|held| !held).await;
                self.backend_calls_held.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(self.stub.clone())
        }

        async fn hydrate(&self, _ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure> {
            let album = match self.hydrate_error.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(self.tracks.lock().unwrap().clone()),
            };
            metadata::hydrate_in(&Tracklist(album), query).await
        }
    }

    /// A provider whose every album has these tracks, or which answers this error.
    struct Tracklist(Result<Vec<Track>, MetadataError>);

    #[async_trait]
    impl MetadataProvider for Tracklist {
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
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(vec![])
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(vec![])
        }
        async fn get_album(&self, id: &str) -> Result<AlbumWithTracks, MetadataError> {
            let tracks = self.0.clone()?;
            Ok(AlbumWithTracks {
                album: shared::metadata::Album {
                    id: id.to_string(),
                    title: String::new(),
                    artist: String::new(),
                    release_date: None,
                    mbid: Some(id.to_string()),
                    cover_url: None,
                    release_types: None,
                },
                tracks,
            })
        }
    }
}
