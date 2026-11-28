//! The Internet Archive download backend: archive.org's search and item metadata for finding
//! files, and an engine in this process that downloads them.
//!
//! Only original audio files of items that are not stream-only or restricted are offered, never
//! archive.org's own conversions, and each file is checked against its listed md5 before it
//! lands. The engine is one per process and outlives the handles the registry builds around it,
//! so a handle rebuilt while transfers run lists them still.

mod client;
mod error;
mod matching_ia;
mod query;
mod search;
mod transfers;

pub use client::base_url_from_env;
pub use error::ArchiveError;
pub use transfers::{EngineSettings, Limits};

use crate::backend_error::BackendResult;
use crate::partial::PartialDir;
use client::Api;
use search::Searches;
use shared::archive::BACKEND_ID;
use shared::download::{
    DownloadProgress, DownloadableItem, QueuedDownload, SearchResult, SearchSpec, SourcePresence,
};
use shared::failure::{Failure, Stage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;
use transfers::Transfers;

/// How long a health check or a presence is answered from what archive.org last said.
const CACHED: Duration = Duration::from_secs(60);

/// What archive.org last said of an item, and when. A caller holds it while it asks, so the
/// callers at once share one request.
type Presence = Arc<tokio::sync::Mutex<Option<(Instant, SourcePresence)>>>;

/// What the backend runs in this process: its transfers, its searches, and what it last heard
/// from archive.org.
pub struct Engine {
    api: Api,
    transfers: Arc<Transfers>,
    searches: Searches,
    presence: Mutex<HashMap<String, Presence>>,
    /// Held while a health check asks archive.org, so the checks at once share one request.
    probe: tokio::sync::Mutex<()>,
}

impl Engine {
    /// The engine of `settings`, its unfinished transfers resumed (`Transfers::start`).
    pub async fn start(
        settings: EngineSettings,
        limits: Limits,
    ) -> Result<Arc<Engine>, ArchiveError> {
        let api = Api::new(&settings.base_url);
        let transfers = Transfers::start(settings, limits, api.heard().clone()).await?;
        Ok(Arc::new(Engine {
            api,
            transfers,
            searches: Searches::default(),
            presence: Mutex::new(HashMap::new()),
            probe: tokio::sync::Mutex::new(()),
        }))
    }

    /// Stops every search and unfinished transfer, deletes their partial files and forgets their
    /// book entries; ended transfers stay. Every handle built before is refused `download` and
    /// `start_search` from now on.
    pub async fn stop(&self) {
        self.searches.stop_all();
        self.transfers.stop().await;
    }
}

/// The one engine of a process, made on first use.
pub struct EngineCell(tokio::sync::Mutex<Option<Arc<Engine>>>);

/// The engine the registry's handles share.
pub static ENGINE: EngineCell = EngineCell::new();

impl Default for EngineCell {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineCell {
    pub const fn new() -> Self {
        EngineCell(tokio::sync::Mutex::const_new(None))
    }

    /// The engine, started from `settings` when there is none yet.
    pub async fn get_or_start(
        &self,
        settings: EngineSettings,
    ) -> Result<Arc<Engine>, ArchiveError> {
        let mut engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            return Ok(engine.clone());
        }
        let started = Engine::start(settings, Limits::ARCHIVE).await?;
        *engine = Some(started.clone());
        Ok(started)
    }

    /// Stops the engine (`Engine::stop`): whether one was started in this process.
    pub async fn stop(&self) -> bool {
        match &*self.0.lock().await {
            Some(engine) => {
                engine.stop().await;
                true
            }
            None => false,
        }
    }

    /// With no engine started in this process, forgets the unfinished transfers a previous run
    /// recorded and deletes their partial files, so a later start does not resume them. An
    /// engine started meanwhile is stopped instead.
    pub async fn forget_unfinished(&self, settings: EngineSettings) -> Result<(), ArchiveError> {
        let engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            engine.stop().await;
            return Ok(());
        }
        let dir = PartialDir::open(&settings.download_path, &settings.instance, BACKEND_ID)
            .map_err(|error| ArchiveError::Storage {
                download_path: settings.download_path.clone(),
                error,
            })?;
        for entry in settings.book.all(BACKEND_ID).await? {
            if transfers::finished(&entry) {
                continue;
            }
            if let Err(e) = std::fs::remove_file(dir.part(&entry.key)) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        "Could not delete the partial file of archive transfer {}: {e}",
                        entry.key
                    );
                }
            }
            settings.book.forget(BACKEND_ID, &entry.key).await?;
        }
        Ok(())
    }
}

/// The handle the registry keeps: the process's engine, and the generation it was built in.
pub struct ArchiveBackend {
    engine: Arc<Engine>,
    generation: u64,
}

impl ArchiveBackend {
    pub fn new(engine: Arc<Engine>) -> Self {
        let generation = engine.transfers.generation();
        ArchiveBackend { engine, generation }
    }
}

#[async_trait::async_trait]
impl crate::DownloadBackend for ArchiveBackend {
    fn id(&self) -> &'static str {
        shared::backends::ARCHIVE.id
    }

    fn name(&self) -> &'static str {
        shared::backends::ARCHIVE.name
    }

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
        let engine = &self.engine;
        Ok(engine
            .transfers
            .while_current(self.generation, || engine.searches.start(&engine.api, spec))?)
    }

    async fn poll_search(&self, search_id: &str) -> BackendResult<SearchResult> {
        Ok(self.engine.searches.poll(search_id)?)
    }

    fn new_request(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        owner: &str,
    ) -> BackendResult<Vec<QueuedDownload>> {
        Ok(self
            .engine
            .transfers
            .download(self.generation, items, destination, request, owner)
            .await?)
    }

    async fn get_downloads_for(&self, sources: &[String]) -> BackendResult<Vec<DownloadProgress>> {
        Ok(self.engine.transfers.listed(sources))
    }

    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> BackendResult<Option<DownloadProgress>> {
        Ok(self.engine.transfers.one(source, id))
    }

    async fn cancel_download(
        &self,
        source: &str,
        download_id: &str,
        remove: bool,
    ) -> BackendResult<()> {
        Ok(self
            .engine
            .transfers
            .cancel(source, download_id, remove)
            .await?)
    }

    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>> {
        Ok(self.engine.transfers.queue_position(source, id))
    }

    async fn source_status(&self, source: &str) -> BackendResult<SourcePresence> {
        let slot = {
            let mut presence = self.engine.presence.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Kept while a caller holds it or its answer is recent.
            presence.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot
                        .try_lock()
                        .is_ok_and(|p| p.is_some_and(|(at, _)| at.elapsed() < CACHED))
            });
            presence.entry(source.to_string()).or_default().clone()
        };
        let mut known = slot.lock().await;
        if let Some((at, presence)) = *known {
            if at.elapsed() < CACHED {
                return Ok(presence);
            }
        }
        let presence = self.engine.api.presence(source).await?;
        *known = Some((Instant::now(), presence));
        Ok(presence)
    }

    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
        self.engine.searches.stop(search_id);
        Ok(())
    }

    /// What archive.org answered the backend's last request, when that was less than a minute
    /// ago; else one request of its own.
    async fn health_check(&self) -> Result<(), Failure> {
        let _probe = self.engine.probe.lock().await;
        let heard = match self.engine.api.heard().within(CACHED) {
            Some(heard) => heard,
            None => {
                let heard = self.engine.api.health().await;
                if let Err(e) = &heard {
                    warn!("Internet Archive health check failed: {e}");
                }
                heard
            }
        };
        heard.map_err(|e| {
            let e = ArchiveError::from(e);
            e.failure(Stage::SourceSearch, e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};
    use crate::DownloadBackend;
    use transfers::tests::{body, item, Rig};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn the_backend_goes_by_its_meta_and_a_rebuilt_handle_shares_the_engine() {
        let rig = Rig::new();
        let cell = EngineCell::new();
        let engine = cell
            .get_or_start(rig.settings("http://127.0.0.1:1"))
            .await
            .unwrap();
        let backend = ArchiveBackend::new(engine.clone());
        let meta = shared::backends::meta("archive").unwrap();
        assert_eq!((backend.id(), backend.name()), (meta.id, meta.name));
        assert_eq!(backend.id(), shared::archive::BACKEND_ID);
        let again = cell
            .get_or_start(rig.settings("http://127.0.0.1:1"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&engine, &again));
    }

    #[tokio::test]
    async fn a_health_check_costs_archive_org_one_request_a_minute() {
        let (base, seen) = serve(vec![
            ok(r#"{"response":{"numFound":3,"start":0,"docs":[]}}"#),
            ok(r#"{"response":{"numFound":3,"start":0,"docs":[]}}"#),
        ]);
        let rig = Rig::new();
        let engine = Engine::start(rig.settings(&base), Limits::ARCHIVE)
            .await
            .unwrap();
        let backend = ArchiveBackend::new(engine.clone());
        backend.health_check().await.unwrap();
        backend.health_check().await.unwrap();
        // A handle rebuilt meanwhile asks no more.
        ArchiveBackend::new(engine).health_check().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_health_check_answers_from_the_backends_last_request_to_archive_org() {
        let (base, seen) = serve(vec![ok(
            r#"{"response":{"numFound":0,"start":0,"docs":[]}}"#,
        )]);
        let rig = Rig::new();
        let engine = Engine::start(rig.settings(&base), Limits::ARCHIVE)
            .await
            .unwrap();
        let backend = ArchiveBackend::new(engine.clone());
        engine.api.searcher().unwrap().items("x").await.unwrap();
        backend.health_check().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "no request of its own");

        engine
            .api
            .heard()
            .note::<()>(&Err(crate::polite::RemoteError::RateLimited {
                service: "archive.org",
            }));
        let failure = backend.health_check().await.unwrap_err();
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::SearchRateLimited
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn callers_at_once_share_one_presence_request() {
        let (base, seen) = serve(vec![ok(
            r#"{"result": {"identifier": "dmp003", "collection": ["netlabels"]}}"#,
        )]);
        let rig = Rig::new();
        let engine = Engine::start(rig.settings(&base), Limits::ARCHIVE)
            .await
            .unwrap();
        let (a, b) = (
            ArchiveBackend::new(engine.clone()),
            ArchiveBackend::new(engine),
        );
        let (first, second) = tokio::join!(a.source_status("dmp003"), b.source_status("dmp003"));
        assert_eq!(first.unwrap(), SourcePresence::Online);
        assert_eq!(second.unwrap(), SourcePresence::Online);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_items_presence_is_asked_once_a_minute() {
        let (base, seen) = serve(vec![
            ok(r#"{"result": {"identifier": "dmp003", "collection": ["netlabels"]}}"#),
            ok("{}"),
        ]);
        let rig = Rig::new();
        let engine = Engine::start(rig.settings(&base), Limits::ARCHIVE)
            .await
            .unwrap();
        let backend = ArchiveBackend::new(engine);
        for _ in 0..2 {
            assert_eq!(
                backend.source_status("dmp003").await.unwrap(),
                SourcePresence::Online
            );
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stopped_engine_refuses_the_handles_built_before_and_serves_the_next_one() {
        let rig = Rig::new();
        let engine = Engine::start(rig.settings("http://127.0.0.1:1"), Limits::ARCHIVE)
            .await
            .unwrap();
        let before = ArchiveBackend::new(engine.clone());
        engine.stop().await;

        let spec = SearchSpec::of(
            None,
            vec![shared::metadata::Track {
                id: "t".into(),
                title: "Moon Age".into(),
                artist: "Duster".into(),
                album_id: None,
                album_title: None,
                release_date: None,
                duration: None,
                mbid: None,
                release_mbid: None,
            }],
        );
        let refused = before.start_search(&spec).await.unwrap_err();
        assert_eq!(
            refused.failure(Stage::SourceSearch, "m").code,
            shared::failure::FailureCode::BackendNotConfigured
        );
        let file = body(10, 1);
        assert!(before
            .download(vec![item("duster", "01.flac", &file)], "D [1]", "r", "u1")
            .await
            .is_err());

        let after = ArchiveBackend::new(engine);
        let id = after.start_search(&spec).await.unwrap();
        after.stop_search(&id).await.unwrap();
    }

    #[tokio::test]
    async fn stopping_with_no_engine_forgets_what_a_previous_run_left_unfinished() {
        let rig = Rig::new();
        let entry = |key: &str, state: &str| crate::BookEntry {
            backend: BACKEND_ID.into(),
            key: key.into(),
            transfer_id: Some(key.into()),
            file_id: None,
            source: "duster".into(),
            item: format!("duster/{key}.flac"),
            request: "r".into(),
            target: "D [1]".into(),
            data: serde_json::json!({"size": 1, "md5": "m", "etag": null, "state": state,
                "failure": null, "ended_at": 1}),
            created_at: 1,
        };
        {
            let mut entries = rig.book.0.lock().unwrap();
            entries.push(entry("unfinished", "queued"));
            entries.push(entry("landing", "landing"));
            entries.push(entry("done", "completed"));
        }
        std::fs::create_dir_all(rig.part_dir()).unwrap();
        std::fs::write(rig.part_dir().join("unfinished"), b"partial").unwrap();

        let cell = EngineCell::new();
        assert!(!cell.stop().await, "no engine was started");
        cell.forget_unfinished(rig.settings("http://127.0.0.1:1"))
            .await
            .unwrap();

        assert!(rig.parts().is_empty());
        assert_eq!(
            rig.book.states(),
            [("done".to_string(), "completed".to_string())]
        );
    }
}
