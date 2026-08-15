//! Shared fixtures for the download module's unit tests: builders for the
//! search types the failover path consumes, and a `DownloadBackend` whose
//! every enqueue answer is scripted. Lives outside the test modules so
//! `failover` and `monitor` drive the same stub instead of keeping two
//! copies of it in step.

use async_trait::async_trait;
use shared::download::{
    DownloadProgress, DownloadableGroup, DownloadableItem, QueuedDownload, SearchResult,
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

/// What a scripted `download` call does: accept the item, accept the call but
/// mark the item rejected (peer offline, already queued elsewhere), or fail
/// the call itself (transport error).
pub enum ScriptedOutcome {
    Enqueued,
    Rejected,
    TransportError,
}

/// Answers each enqueue from a scripted list, then rejects everything after
/// the script runs out.
pub struct StubBackend {
    script: Mutex<Vec<ScriptedOutcome>>,
}

impl StubBackend {
    pub fn new(script: Vec<ScriptedOutcome>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
        })
    }
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
    async fn download(&self, items: Vec<DownloadableItem>) -> SoulResult<Vec<QueuedDownload>> {
        let item = items.into_iter().next().expect("one item per enqueue");
        let outcome = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                ScriptedOutcome::Rejected
            } else {
                script.remove(0)
            }
        };
        match outcome {
            ScriptedOutcome::Enqueued => Ok(vec![QueuedDownload {
                id: item.id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                size: item.size.unwrap_or(0),
                error: None,
            }]),
            ScriptedOutcome::Rejected => Ok(vec![QueuedDownload {
                id: item.id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                size: item.size.unwrap_or(0),
                error: Some("peer offline".to_string()),
            }]),
            // LockError stands in for any transport-level failure; its
            // meaning doesn't matter here, only that `download` itself
            // errors rather than returning an Ok with a rejected item.
            ScriptedOutcome::TransportError => Err(SoulseekError::LockError),
        }
    }
    async fn get_downloads(&self) -> SoulResult<Vec<DownloadProgress>> {
        Ok(Vec::new())
    }
    async fn cancel_download(&self, _u: &str, _id: &str, _remove: bool) -> SoulResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> bool {
        true
    }
}
