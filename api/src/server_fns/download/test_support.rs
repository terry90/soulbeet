//! Shared fixtures for the download module's unit tests: builders for the
//! search types the failover path consumes, and a `DownloadBackend` whose
//! every enqueue answer is scripted. Lives outside the test modules so
//! `failover` and `monitor` drive the same stub instead of keeping two
//! copies of it in step.

use async_trait::async_trait;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableGroup, DownloadableItem, EnqueueStatus,
    QueuedDownload, SearchResult,
};
use shared::metadata::{Album, Track};
use soulbeet::error::{Result as SoulResult, SoulseekError};
use soulbeet::DownloadBackend;
use std::sync::{Arc, Mutex};

pub fn item(source: &str, title: &str, id: &str) -> DownloadableItem {
    DownloadableItem {
        id: id.to_string(),
        source: source.to_string(),
        title: title.to_string(),
        artist: "Kowloon".to_string(),
        album: "Come Over".to_string(),
        size: Some(27_213_250),
        duration: Some(200),
        quality: "FLAC".to_string(),
        quality_score: 1.0,
        backend_data: None,
    }
}

pub fn group(source: &str, items: Vec<DownloadableItem>, score: f64) -> DownloadableGroup {
    DownloadableGroup {
        source: source.to_string(),
        group_id: format!("{source}/group"),
        title: "Come Over".to_string(),
        artist: Some("Kowloon".to_string()),
        item_count: items.len(),
        total_size: 0,
        items,
        quality: "FLAC".to_string(),
        score,
    }
}

/// What a scripted `download` call does: accept the item (transfer id
/// `t-{item.id}`), refuse it because the backend is already fetching it,
/// accept the call but mark the item rejected (peer offline), or fail the
/// call itself (transport error).
pub enum ScriptedOutcome {
    Enqueued,
    AlreadyInFlight,
    Rejected,
    TransportError,
}

/// Answers each enqueue from a scripted list, then rejects everything after
/// the script runs out. Records every destination and cancellation, and
/// lists whatever transfers the test sets.
pub struct StubBackend {
    script: Mutex<Vec<ScriptedOutcome>>,
    cancelled: Mutex<Vec<(String, String)>>,
    destinations: Mutex<Vec<String>>,
    listing: Mutex<Vec<DownloadProgress>>,
}

impl StubBackend {
    pub fn new(script: Vec<ScriptedOutcome>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            cancelled: Mutex::new(Vec::new()),
            destinations: Mutex::new(Vec::new()),
            listing: Mutex::new(Vec::new()),
        })
    }

    /// The `(peer, transfer id)` pairs passed to `cancel_download`, in order.
    pub fn cancelled(&self) -> Vec<(String, String)> {
        self.cancelled.lock().unwrap().clone()
    }

    /// The destination of every `download` call, in order.
    pub fn destinations(&self) -> Vec<String> {
        self.destinations.lock().unwrap().clone()
    }

    /// What `get_downloads_for` (filtered by peer) and `get_downloads` return.
    pub fn set_listing(&self, listing: Vec<DownloadProgress>) {
        *self.listing.lock().unwrap() = listing;
    }
}

/// A transfer as the backend lists it.
pub fn transfer(source: &str, filename: &str, id: &str, state: DownloadState) -> DownloadProgress {
    DownloadProgress::queued(id.to_string(), source.to_string(), filename.to_string(), 0)
        .with_state(state)
}

#[async_trait]
impl DownloadBackend for StubBackend {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn name(&self) -> &'static str {
        "Stub"
    }
    async fn start_search(&self, _album: Option<&Album>, _tracks: &[Track]) -> SoulResult<String> {
        Ok("search".to_string())
    }
    async fn poll_search(&self, _search_id: &str) -> SoulResult<SearchResult> {
        unreachable!("failover never searches")
    }
    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
    ) -> SoulResult<Vec<QueuedDownload>> {
        self.destinations
            .lock()
            .unwrap()
            .push(destination.to_string());
        let item = items.into_iter().next().expect("one item per enqueue");
        let outcome = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                ScriptedOutcome::Rejected
            } else {
                script.remove(0)
            }
        };
        let answer = |id: String, status: EnqueueStatus| {
            Ok(vec![QueuedDownload {
                id,
                source: item.source.clone(),
                item: item.id.clone(),
                size: item.size.unwrap_or(0),
                status,
            }])
        };
        match outcome {
            ScriptedOutcome::Enqueued => answer(format!("t-{}", item.id), EnqueueStatus::Enqueued),
            ScriptedOutcome::AlreadyInFlight => {
                answer(item.id.clone(), EnqueueStatus::AlreadyInFlight)
            }
            ScriptedOutcome::Rejected => answer(
                item.id.clone(),
                EnqueueStatus::Failed("peer offline".to_string()),
            ),
            // LockError stands in for any transport-level failure; its
            // meaning doesn't matter here, only that `download` itself
            // errors rather than returning an Ok with a rejected item.
            ScriptedOutcome::TransportError => Err(SoulseekError::LockError),
        }
    }
    async fn get_downloads_for(&self, sources: &[String]) -> SoulResult<Vec<DownloadProgress>> {
        Ok(self
            .listing
            .lock()
            .unwrap()
            .iter()
            .filter(|t| sources.contains(&t.source))
            .cloned()
            .collect())
    }
    async fn get_downloads(&self) -> SoulResult<Vec<DownloadProgress>> {
        Ok(self.listing.lock().unwrap().clone())
    }
    async fn cancel_download(&self, username: &str, id: &str, _remove: bool) -> SoulResult<()> {
        self.cancelled
            .lock()
            .unwrap()
            .push((username.to_string(), id.to_string()));
        Ok(())
    }
    async fn health_check(&self) -> bool {
        true
    }
}
