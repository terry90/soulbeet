//! The GD Studio download backend: NetEase's catalogue through GD Studio's public API, and an
//! engine in this process that downloads its files from NetEase's servers.
//!
//! Every call to the API waits for the install's budget, below the 50 calls per 5 minutes GD
//! Studio publishes, shared by every user and kept across restarts. A file is fetched only
//! from NetEase's own file servers, and is checked against the size GD Studio gave and the MD5
//! it is named by before it is tagged and lands. The engine is one per process and outlives
//! the handles the registry builds around it, so a handle rebuilt while transfers run lists
//! them still.

mod api;
mod audio_header;
mod budget;
mod error;
mod matching;
mod search;
mod transfer;

pub use api::test_host_from_env;
pub use error::GdError;

use crate::backend_error::BackendResult;
use api::Api;
use budget::Budget;
use search::Searches;
use shared::download::{
    DownloadProgress, DownloadableItem, QueuedDownload, SearchResult, SearchSpec, SourcePresence,
};
use shared::failure::{Failure, Stage};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;
use transfer::Transfers;

/// How hard the backend works GD Studio and NetEase, and how long it waits on them.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Calls to the API in `window`, for the whole install.
    pub calls: usize,
    pub window: Duration,
    /// The least time between two calls.
    pub spacing: Duration,
    /// How long a 429 or 503 holds every call to the API.
    pub hold: Duration,
    /// Failed calls in a row before the API is left alone for `rest_s`.
    pub failures: u64,
    pub rest_s: i64,
    /// How long a search runs at most.
    pub search_deadline: Duration,
    /// How long a file URL is used after GD Studio named it.
    pub reuse: Duration,
    /// Files fetched at once, for the whole install.
    pub workers: usize,
    /// Cut answers before a transfer fails.
    pub attempts: u32,
    /// The wait after a cut answer.
    pub retry_wait: Duration,
    pub connect_timeout: Duration,
    /// How long an answer may send nothing.
    pub read_timeout: Duration,
    /// How long an answer of the API stands for its health.
    pub heard_for: Duration,
    /// How long a failed call stands for it: a recovered API reads as up again soon.
    pub failure_for: Duration,
}

impl Limits {
    pub const GDSTUDIO: Limits = Limits {
        // 5 under the published 50, for the clocks' drift.
        calls: 45,
        window: Duration::from_secs(300),
        spacing: Duration::from_secs(1),
        hold: Duration::from_secs(60),
        failures: 5,
        rest_s: 60,
        // Under the 180 s the app waits for a search.
        search_deadline: Duration::from_secs(150),
        // NetEase's servers refuse a request on a URL 25 minutes after it was named.
        reuse: Duration::from_secs(20 * 60),
        workers: 2,
        attempts: 3,
        retry_wait: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(60),
        heard_for: Duration::from_secs(10 * 60),
        failure_for: Duration::from_secs(60),
    };
}

/// What the engine needs of the server it runs in.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub download_path: PathBuf,
    /// This install's id: its partial files are its own.
    pub instance: String,
    /// `GDSTUDIO_HOST`, a test double's `host:port`; None for GD Studio itself.
    pub test_host: Option<String>,
}

/// The file next to an install's partial folders that keeps the calls sent to GD Studio in the
/// last window, so a restart does not start the budget empty. Not a partial file: nothing
/// clears it.
const CALLS_FILE: &str = ".gdstudio-calls";

/// What the backend runs in this process: its transfers, its searches, and the API with its
/// budget.
pub struct Engine {
    api: Arc<Api>,
    transfers: Arc<Transfers>,
    searches: Searches,
    limits: Limits,
    /// Held while a health check asks the API, so the checks at once share one call.
    probe: tokio::sync::Mutex<()>,
}

impl Engine {
    /// The engine of `settings`, its partial folder opened and emptied first, its budget
    /// holding the calls an earlier run sent less than a window ago.
    pub async fn start(settings: EngineSettings, limits: Limits) -> Result<Arc<Engine>, GdError> {
        let kept = (settings.download_path.join(crate::partial::PARTIAL_FOLDER))
            .join(&settings.instance)
            .join(CALLS_FILE);
        let budget = Budget::kept_in(limits.calls, limits.window, kept);
        let api = Arc::new(Api::new(settings.test_host, &limits, budget)?);
        let transfers = Transfers::start(
            &settings.download_path,
            &settings.instance,
            api.clone(),
            limits,
        )
        .await?;
        Ok(Arc::new(Engine {
            api,
            transfers,
            searches: Searches::default(),
            limits,
            probe: tokio::sync::Mutex::new(()),
        }))
    }

    /// Stops every search and unfinished transfer, forgets those transfers and deletes their
    /// partial files; ended transfers stay. Every handle built before is refused `download`,
    /// `start_search` and `health_check` from now on.
    pub async fn stop(&self) {
        // Older handles are refused first, so no search starts after the searches are stopped.
        let halted = self.transfers.halt();
        self.searches.stop_all();
        self.transfers.clear(halted).await;
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
    pub async fn get_or_start(&self, settings: EngineSettings) -> Result<Arc<Engine>, GdError> {
        let mut engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            return Ok(engine.clone());
        }
        let started = Engine::start(settings, Limits::GDSTUDIO).await?;
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

    /// With no engine started in this process, deletes the partial files a previous run left.
    /// An engine started meanwhile is stopped instead.
    pub async fn clear_left(&self, settings: EngineSettings) -> Result<(), GdError> {
        let engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            engine.stop().await;
            return Ok(());
        }
        let dir = transfer::open_dir(&settings.download_path, &settings.instance)?;
        tokio::task::spawn_blocking(move || dir.clear_except(&Default::default()))
            .await
            .map_err(std::io::Error::other)
            .and_then(|cleared| cleared)
            .map_err(|error| GdError::Storage {
                download_path: settings.download_path.clone(),
                error: Arc::new(error),
            })
    }
}

/// The handle the registry keeps: the process's engine, and the generation it was built in.
pub struct GdStudioBackend {
    engine: Arc<Engine>,
    generation: u64,
}

impl GdStudioBackend {
    pub fn new(engine: Arc<Engine>) -> Self {
        let generation = engine.transfers.generation();
        GdStudioBackend { engine, generation }
    }
}

#[async_trait::async_trait]
impl crate::DownloadBackend for GdStudioBackend {
    fn id(&self) -> &'static str {
        shared::backends::GDSTUDIO.id
    }

    fn name(&self) -> &'static str {
        shared::backends::GDSTUDIO.name
    }

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
        let engine = &self.engine;
        Ok(engine.transfers.while_current(self.generation, || {
            engine.searches.start(&engine.api, &engine.limits, spec)
        })?)
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
            .download(self.generation, items, destination, request, owner)?)
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
        self.engine
            .transfers
            .cancel(source, download_id, remove)
            .await;
        Ok(())
    }

    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>> {
        Ok(self.engine.transfers.queue_position(source, id))
    }

    /// From what the API last answered: no call is made.
    async fn source_status(&self, _source: &str) -> BackendResult<SourcePresence> {
        let api = &self.engine.api;
        let failing = api.resting().await
            || matches!(
                api.heard().within(self.engine.limits.failure_for),
                Some(Err(_))
            );
        Ok(match failing {
            true => SourcePresence::Offline,
            false => SourcePresence::Online,
        })
    }

    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
        self.engine.searches.stop(search_id);
        Ok(())
    }

    /// The partial folder can be written, and what the API answered the backend's last call:
    /// an answer less than 10 minutes old, a failure less than a minute old; else one call of
    /// its own, from the budget. A handle built before a stop is refused and calls nothing.
    async fn health_check(&self) -> Result<(), Failure> {
        let engine = &self.engine;
        let failed = |e: GdError| e.failure(Stage::SourceSearch, e.to_string());
        let _probe = engine.probe.lock().await;
        (engine.transfers)
            .while_current(self.generation, || Ok(()))
            .map_err(failed)?;
        // Under the probe's lock: the checks at once share its file name.
        engine.transfers.probe_storage().map_err(failed)?;
        if engine.api.resting().await {
            return Err(failed(GdError::Resting {
                failures: engine.limits.failures,
                rest_s: engine.limits.rest_s,
            }));
        }
        let heard = engine.api.heard();
        let heard = match (
            heard.within(engine.limits.heard_for),
            heard.within(engine.limits.failure_for),
        ) {
            (Some(Ok(())), _) => Ok(()),
            (Some(Err(e)), Some(_)) => Err(GdError::Remote(e)),
            _ => {
                let checked = engine.api.health().await;
                if let Err(e) = &checked {
                    warn!("GD Studio health check failed: {e}");
                }
                checked
            }
        };
        heard.map_err(failed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gdstudio::api::tests::{fixture, ok};
    use crate::test_http::{serve, Reply};
    use crate::DownloadBackend;

    pub(crate) fn fast() -> Limits {
        Limits {
            calls: 1000,
            spacing: Duration::ZERO,
            hold: Duration::ZERO,
            retry_wait: Duration::from_millis(10),
            read_timeout: Duration::from_secs(30),
            ..Limits::GDSTUDIO
        }
    }

    /// A downloads folder and an instance of the engine's own.
    pub(crate) struct Rig {
        pub root: tempfile::TempDir,
        pub instance: String,
    }

    impl Rig {
        pub(crate) fn new() -> Rig {
            Rig {
                root: tempfile::tempdir().unwrap(),
                instance: uuid::Uuid::new_v4().to_string(),
            }
        }

        /// The settings of an engine whose API is the test double at `base`.
        pub(crate) fn settings(&self, base: &str) -> EngineSettings {
            EngineSettings {
                download_path: self.root.path().to_path_buf(),
                instance: self.instance.clone(),
                test_host: Some(base.trim_start_matches("http://").to_string()),
            }
        }

        pub(crate) fn part_dir(&self) -> PathBuf {
            self.root
                .path()
                .join(crate::partial::PARTIAL_FOLDER)
                .join(&self.instance)
                .join(shared::gdstudio::BACKEND_ID)
        }

        pub(crate) fn parts(&self) -> Vec<String> {
            std::fs::read_dir(self.part_dir())
                .map(|dir| {
                    dir.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
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
        let backend = GdStudioBackend::new(engine.clone());
        let meta = shared::backends::meta("gdstudio").unwrap();
        assert_eq!((backend.id(), backend.name()), (meta.id, meta.name));
        assert_eq!(backend.id(), shared::gdstudio::BACKEND_ID);
        let again = cell
            .get_or_start(rig.settings("http://127.0.0.1:1"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&engine, &again));
    }

    #[tokio::test]
    async fn a_new_engine_clears_what_an_earlier_run_left_and_a_cold_stop_does_too() {
        let rig = Rig::new();
        std::fs::create_dir_all(rig.part_dir()).unwrap();
        std::fs::write(rig.part_dir().join("left-by-a-crash"), b"partial").unwrap();
        Engine::start(rig.settings("http://127.0.0.1:1"), fast())
            .await
            .unwrap();
        assert!(rig.parts().is_empty(), "{:?}", rig.parts());

        let rig = Rig::new();
        std::fs::create_dir_all(rig.part_dir()).unwrap();
        std::fs::write(rig.part_dir().join("left-by-a-crash"), b"partial").unwrap();
        let cell = EngineCell::new();
        assert!(!cell.stop().await, "no engine was started");
        cell.clear_left(rig.settings("http://127.0.0.1:1"))
            .await
            .unwrap();
        assert!(rig.parts().is_empty());
    }

    #[tokio::test]
    async fn a_health_check_answers_from_the_last_call_and_otherwise_takes_a_slot() {
        let (base, seen) = serve(vec![ok("[]"), ok(&fixture("s1_album_search.json"))]);
        let rig = Rig::new();
        let limits = Limits { calls: 1, ..fast() };
        let engine = Engine::start(rig.settings(&base), limits).await.unwrap();
        let backend = GdStudioBackend::new(engine.clone());
        backend.health_check().await.unwrap();
        backend.health_check().await.unwrap();
        GdStudioBackend::new(engine.clone())
            .health_check()
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "one call for three checks");
        // The check took the one slot of the window: a search waits for it.
        let spec = SearchSpec::of(
            None,
            vec![crate::gdstudio::matching::tests::mb_track("a", "b", None)],
        );
        let id = backend.start_search(&spec).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        backend.stop_search(&id).await.unwrap();
        assert_eq!(
            backend.source_status("NetEase").await.unwrap(),
            SourcePresence::Online
        );
    }

    #[tokio::test]
    async fn the_calls_sent_before_a_restart_still_count() {
        let (base, seen) = serve(vec![ok("[]"), ok("[]")]);
        let rig = Rig::new();
        let limits = Limits { calls: 1, ..fast() };
        let first = Engine::start(rig.settings(&base), limits).await.unwrap();
        GdStudioBackend::new(first).health_check().await.unwrap();
        let kept = (rig.root.path().join(crate::partial::PARTIAL_FOLDER))
            .join(&rig.instance)
            .join(CALLS_FILE);
        assert!(kept.exists());

        let restarted = Engine::start(rig.settings(&base), limits).await.unwrap();
        let waited = tokio::time::timeout(
            Duration::from_millis(300),
            GdStudioBackend::new(restarted).health_check(),
        )
        .await;
        assert!(waited.is_err(), "the window still holds the earlier call");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_api_reads_offline_and_its_failure_names_gd_studio() {
        let (base, _) = serve(vec![Reply {
            status: 401,
            body: r#"{"detail":"Invalid request."}"#.into(),
        }]);
        let rig = Rig::new();
        let engine = Engine::start(rig.settings(&base), fast()).await.unwrap();
        let backend = GdStudioBackend::new(engine);
        let failure = backend.health_check().await.unwrap_err();
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::BackendResponseUnreadable
        );
        assert_eq!(
            failure.cause.map(|c| (c.service, c.status)),
            Some((shared::failure::Service::GdStudio, Some(401)))
        );
        // Answered from what was heard: no second call.
        assert!(backend.health_check().await.is_err());
        assert_eq!(
            backend.source_status("NetEase").await.unwrap(),
            SourcePresence::Offline
        );
    }

    #[tokio::test]
    async fn a_failure_stands_for_a_while_and_is_asked_about_again_after_it() {
        let (base, seen) = serve(vec![
            Reply {
                status: 503,
                body: "<html>down</html>".into(),
            },
            ok("[]"),
        ]);
        let rig = Rig::new();
        let limits = Limits {
            failure_for: Duration::from_millis(100),
            ..fast()
        };
        let backend =
            GdStudioBackend::new(Engine::start(rig.settings(&base), limits).await.unwrap());
        let down = backend.health_check().await.unwrap_err();
        assert_eq!(down.code, shared::failure::FailureCode::BackendUnreachable);
        assert!(backend.health_check().await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        backend.health_check().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_downloads_folder_that_cannot_be_written_fails_the_health_check() {
        let rig = Rig::new();
        let (base, _) = serve(vec![]);
        let engine = Engine::start(rig.settings(&base), fast()).await.unwrap();
        std::fs::remove_dir_all(rig.part_dir()).unwrap();
        // A file where the partial folder was.
        std::fs::write(rig.part_dir(), b"in the way").unwrap();
        let failure = GdStudioBackend::new(engine)
            .health_check()
            .await
            .unwrap_err();
        assert_eq!(
            failure.user_action.map(|a| a.code),
            Some(shared::failure::UserActionCode::FixDownloadPath)
        );
    }

    #[tokio::test]
    async fn a_stopped_engine_refuses_the_handles_built_before_and_serves_the_next_one() {
        let rig = Rig::new();
        let (base, _) = serve(vec![]);
        let engine = Engine::start(rig.settings(&base), fast()).await.unwrap();
        let before = GdStudioBackend::new(engine.clone());
        engine.stop().await;
        let spec = SearchSpec::of(
            None,
            vec![crate::gdstudio::matching::tests::mb_track(
                "Duster", "Moon Age", None,
            )],
        );
        let refused = before.start_search(&spec).await.unwrap_err();
        assert_eq!(
            refused.failure(Stage::SourceSearch, "m").code,
            shared::failure::FailureCode::BackendNotConfigured
        );
        assert!(before.download(vec![], "D [1]", "r", "u1").await.is_err());
        let unasked = before.health_check().await.unwrap_err();
        assert_eq!(
            unasked.code,
            shared::failure::FailureCode::BackendNotConfigured
        );
        assert!(engine.api.heard().within(Duration::from_secs(60)).is_none());
        let after = GdStudioBackend::new(engine);
        let id = after.start_search(&spec).await.unwrap();
        after.stop_search(&id).await.unwrap();
    }
}
