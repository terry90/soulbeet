//! The Direct Connect download backend: an AirDC++ Web Client the admin runs next to Soulful,
//! reached over its Web API as slskd is. Searches go to the hubs the sidecar is connected to,
//! each file is a file bundle in the sidecar's queue, and a file counts as finished once AirDC++
//! has checked it against its TTH.
//!
//! The engine is one per process and outlives the handles the registry builds around it: its
//! searches, the clocks of stalled files and the transfer book survive a settings save, while
//! each handle carries the sidecar connection it was built with.

mod error;
#[cfg(test)]
mod fake;
mod health;
mod http;
mod models;
mod progress;
mod queue;
mod search;
#[cfg(test)]
mod tests;

pub use error::{AirdcppError, Missing};
pub use http::{api_base, forget_refusals, Connection, Sidecar};

use crate::backend_error::BackendResult;
use crate::TransferBook;
use models::SidecarSettings;
use progress::{Listing, Watch};
use search::{Searches, Shared};
use shared::airdcpp::SidecarReport;
use shared::download::{
    DownloadProgress, DownloadableItem, QueuedDownload, SearchResult, SearchSpec, SourcePresence,
};
use shared::failure::{Failure, Stage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

/// How long one health answer is given to every caller.
const HEALTH_KEPT: Duration = Duration::from_secs(10);

/// How long the backend waits on the sidecar and its peers, and how it spaces what it asks.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long results are read after the search went out.
    pub collect: Duration,
    /// From the moment the search went out, when it stops loading file lists.
    pub deadline: Duration,
    /// How long a file list may take to load, and how often it is looked at meanwhile.
    pub list_wait: Duration,
    pub list_poll: Duration,
    /// How long a file may get no byte while AirDC++ only fails to connect to its peers.
    pub give_up: Duration,
    /// After this long without a byte, AirDC++ is asked once to look for other peers.
    pub alternates_after: Duration,
    /// The space between two queue requests.
    pub enqueue_spacing: Duration,
    /// The least space between two hub searches: AirDC++'s own at normal priority.
    pub min_hub_interval: Duration,
    /// How long one bundle and transfer listing is given to every caller.
    pub listing_kept: Duration,
}

impl Timing {
    pub const AIRDCPP: Timing = Timing {
        collect: Duration::from_secs(20),
        deadline: Duration::from_secs(120),
        list_wait: Duration::from_secs(20),
        list_poll: Duration::from_secs(1),
        // Every file that came in the live tests had its first byte within 144 s.
        give_up: Duration::from_secs(300),
        alternates_after: Duration::from_secs(60),
        enqueue_spacing: Duration::from_millis(200),
        min_hub_interval: Duration::from_secs(10),
        listing_kept: Duration::from_secs(1),
    };
}

/// What the engine needs of the server it runs in.
#[derive(Clone)]
pub struct EngineSettings {
    pub download_path: PathBuf,
    pub book: Arc<dyn TransferBook>,
}

#[derive(Default)]
struct State {
    /// Bumped by each stop: a handle built before it is refused.
    generation: u64,
    /// The bundles seen waiting, by bundle id.
    watches: HashMap<u64, Watch>,
    /// Ended and lost entries were forgotten once in this process.
    pruned: bool,
}

/// What the backend runs in this process.
pub struct Engine {
    timing: Timing,
    download_path: PathBuf,
    book: Arc<dyn TransferBook>,
    state: Mutex<State>,
    shared: Arc<Shared>,
    searches: Searches,
    /// The last bundle and transfer lists, shared by every caller for a second.
    listing: tokio::sync::Mutex<Option<Arc<Listing>>>,
    /// Held to read by each `download` call and to write by a stop, so a stop sees every file
    /// a call put in the book, and no file is queued after it.
    queueing: tokio::sync::RwLock<()>,
}

impl Engine {
    /// The engine of `settings`. It asks the sidecar nothing until a handle uses it.
    pub fn new(settings: EngineSettings, timing: Timing) -> Arc<Engine> {
        Arc::new(Engine {
            timing,
            download_path: settings.download_path,
            book: settings.book,
            state: Mutex::new(State::default()),
            shared: Arc::new(Shared::new(timing)),
            searches: Searches::default(),
            listing: tokio::sync::Mutex::new(None),
            queueing: tokio::sync::RwLock::new(()),
        })
    }

    fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    fn current(&self, generation: u64) -> Result<(), AirdcppError> {
        match self.generation() == generation {
            true => Ok(()),
            false => Err(AirdcppError::TurnedOff),
        }
    }

    /// Stops every search, refuses the handles built before, and removes from the sidecar every
    /// unfinished bundle the book holds, forgetting its entry. Finished bundles and their
    /// entries stay. Entries whose bundle cannot be read now stay too, for the next stop.
    pub async fn stop(&self, sidecar: Option<&Sidecar>) {
        self.state.lock().unwrap().generation += 1;
        self.searches.stop_all();
        // A download call in flight sees the new generation at its next file and returns.
        let _queueing = self.queueing.write().await;
        self.remove_unfinished(sidecar).await;
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

    /// The engine, made from `settings` when there is none yet.
    pub async fn get_or_start(&self, settings: EngineSettings) -> Arc<Engine> {
        let mut engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            return engine.clone();
        }
        let made = Engine::new(settings, Timing::AIRDCPP);
        *engine = Some(made.clone());
        made
    }

    /// Stops the engine (`Engine::stop`), or with no engine made in this process, removes the
    /// unfinished bundles a previous run recorded, so a later start does not list them.
    pub async fn stop(&self, settings: EngineSettings, sidecar: Option<&Sidecar>) {
        let engine = self.0.lock().await.clone();
        let engine = engine.unwrap_or_else(|| Engine::new(settings, Timing::AIRDCPP));
        engine.stop(sidecar).await;
    }
}

/// One sidecar connection and what it last said of itself: a handle built from new settings
/// asks again.
pub(crate) struct Link {
    pub sidecar: Arc<Sidecar>,
    /// The sidecar's settings, read by the health check and the first download.
    pub settings: Mutex<Option<SidecarSettings>>,
    /// The last health answer, shared by the callers of the same 10 s.
    pub health: tokio::sync::Mutex<Option<(Instant, Result<(), Failure>)>>,
    /// When the folder probe last ran and what it found, failed or not.
    pub probed: Mutex<Option<(Instant, Result<(), AirdcppError>)>>,
}

impl Link {
    fn new(sidecar: Sidecar) -> Link {
        Link {
            sidecar: Arc::new(sidecar),
            settings: Mutex::new(None),
            health: tokio::sync::Mutex::new(None),
            probed: Mutex::new(None),
        }
    }

    /// The sidecar's settings, as last read, else read now.
    pub(crate) async fn settings(&self) -> Result<SidecarSettings, AirdcppError> {
        if let Some(settings) = self.settings.lock().unwrap().clone() {
            return Ok(settings);
        }
        let read = health::read_settings(&self.sidecar).await?;
        *self.settings.lock().unwrap() = Some(read.clone());
        Ok(read)
    }
}

/// The handle the registry keeps: the process's engine, the sidecar it was built for, and the
/// generation it was built in.
pub struct AirdcppBackend {
    engine: Arc<Engine>,
    link: Arc<Link>,
    generation: u64,
}

impl AirdcppBackend {
    pub fn new(engine: Arc<Engine>, sidecar: Sidecar) -> Self {
        let generation = engine.generation();
        AirdcppBackend {
            engine,
            link: Arc::new(Link::new(sidecar)),
            generation,
        }
    }

    /// What a check of the sidecar finds, for the settings page.
    pub async fn check(&self) -> SidecarReport {
        health::report(&self.engine, &self.link).await
    }
}

#[async_trait::async_trait]
impl crate::DownloadBackend for AirdcppBackend {
    fn id(&self) -> &'static str {
        shared::backends::AIRDCPP.id
    }

    fn name(&self) -> &'static str {
        shared::backends::AIRDCPP.name
    }

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
        self.engine.current(self.generation)?;
        Ok(self.engine.searches.start(
            self.link.sidecar.clone(),
            self.engine.shared.clone(),
            spec,
        )?)
    }

    async fn poll_search(&self, search_id: &str) -> BackendResult<SearchResult> {
        self.engine.searches.poll(search_id)
    }

    fn new_request(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        _owner: &str,
    ) -> BackendResult<Vec<QueuedDownload>> {
        Ok(self
            .engine
            .download(&self.link, self.generation, items, destination, request)
            .await?)
    }

    async fn get_downloads_for(&self, sources: &[String]) -> BackendResult<Vec<DownloadProgress>> {
        Ok(self.engine.listed(&self.link, sources).await?)
    }

    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> BackendResult<Option<DownloadProgress>> {
        let rows = self
            .engine
            .listed(&self.link, &[source.to_string()])
            .await?;
        Ok(rows.into_iter().find(|r| r.id == id))
    }

    async fn cancel_download(
        &self,
        source: &str,
        download_id: &str,
        remove: bool,
    ) -> BackendResult<()> {
        Ok(self
            .engine
            .cancel(&self.link, source, download_id, remove)
            .await?)
    }

    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>> {
        Ok(self.engine.place(&self.link, source, id).await?)
    }

    async fn source_status(&self, source: &str) -> BackendResult<SourcePresence> {
        Ok(health::presence(&self.link.sidecar, source).await?)
    }

    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
        self.engine.searches.stop(search_id);
        Ok(())
    }

    /// The sidecar's sign-in, permissions, language and hubs, and at most every 10 minutes
    /// whether it saves into Soulbeet's downloads folder; one answer for every caller of the
    /// same 10 s.
    async fn health_check(&self) -> Result<(), Failure> {
        let mut kept = self.link.health.lock().await;
        if let Some((at, answer)) = &*kept {
            if at.elapsed() < HEALTH_KEPT {
                return answer.clone();
            }
        }
        let answer = health::check(&self.engine, &self.link)
            .await
            .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()));
        if let Err(failure) = &answer {
            warn!("Direct Connect health check failed: {}", failure.message);
        }
        *kept = Some((Instant::now(), answer.clone()));
        answer
    }
}
