//! Shared fixtures for the download module's unit tests: builders for the
//! search types the failover path consumes, and a `DownloadBackend` whose
//! every enqueue and search answer is scripted. Lives outside the test
//! modules so `failover`, `monitor` and `auto_download` drive the same stub
//! instead of keeping copies of it in step.

use crate::auth::UserId;
use async_trait::async_trait;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableGroup, DownloadableItem, EnqueueStatus, FileFacts,
    QueuedDownload, SearchResult, SearchSpec, SearchState, SearchStats, SourcePresence,
};
use shared::failure::{Cause, Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::metadata::Track;
use soulbeet::error::{Result as SoulResult, SoulseekError};
use soulbeet::{DownloadBackend, ImportMode};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner};

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
        match_score: 0.0,
        below_floor: false,
        facts: FileFacts::default(),
    }
}

/// A wanted track with only its artist and title.
pub fn track(artist: &str, title: &str) -> Track {
    Track {
        id: String::new(),
        title: title.to_string(),
        artist: artist.to_string(),
        album_id: None,
        album_title: None,
        release_date: None,
        duration: None,
        mbid: None,
        release_mbid: None,
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

/// A batch of its own user, so tests sharing the process-wide registry never
/// see each other's claims. Its folder is `<label> [12345678]`, and it
/// imports into `/music`.
pub fn batch(label: &str) -> BatchRef {
    let unique = uuid::Uuid::new_v4();
    BatchRef::new(
        TransferOwner {
            user_id: UserId::new(&format!("tester-{unique}")),
            batch_id: format!("12345678-{unique}"),
        },
        label.to_string(),
        PathBuf::from("/music"),
        ImportMode::Singleton,
    )
}

/// Another batch of `running`'s user, importing into `target`.
pub fn same_user_into(running: &BatchRef, target: PathBuf) -> BatchRef {
    BatchRef::new(
        TransferOwner {
            user_id: running.owner.user_id.clone(),
            batch_id: uuid::Uuid::new_v4().to_string(),
        },
        "Come Over".to_string(),
        target,
        running.import_mode,
    )
}

/// The registry key of `filename` from `peer` on this backend.
pub fn key(backend: &Arc<dyn DownloadBackend>, peer: &str, filename: &str) -> TransferKey {
    TransferKey {
        backend: backend.id(),
        peer: peer.to_string(),
        filename: filename.to_string(),
    }
}

/// The stub's own answer for a file it is already fetching.
pub const STUB_IN_FLIGHT: &str = "stub is already fetching this file";

/// What a scripted `download` call does: accept the item (transfer id
/// `t-{item.id}`), accept another file of the item's peer instead (transfer
/// id `t-{file}`), refuse it because the backend is already fetching it
/// (cause `STUB_IN_FLIGHT`), accept the call but mark the item rejected
/// (peer offline), accept the call but answer nothing for the item, or fail
/// the call itself (transport error).
pub enum ScriptedOutcome {
    Enqueued,
    EnqueuedAs(&'static str),
    AlreadyInFlight,
    Rejected,
    Unanswered,
    TransportError,
}

/// Answers each enqueue from a scripted list, then rejects everything after
/// the script runs out. Starts every search unless told to fail it, and
/// answers its polls from a scripted list, then as still running with
/// nothing found. Records every search spec and stop, destination, request
/// and cancellation, and lists whatever transfers the test sets. Each stub
/// has its own backend id, so the registry keys of concurrent tests never
/// collide.
pub struct StubBackend {
    id: &'static str,
    script: Mutex<Vec<ScriptedOutcome>>,
    search_start: Mutex<Option<SoulseekError>>,
    polls: Mutex<VecDeque<SoulResult<SearchResult>>>,
    cancelled: Mutex<Vec<Cancel>>,
    destinations: Mutex<Vec<String>>,
    requests: Mutex<Vec<String>>,
    listing: Mutex<Vec<DownloadProgress>>,
    hold: Mutex<Option<Hold>>,
    specs: Mutex<Vec<SearchSpec>>,
    search_hold: Mutex<Option<Hold>>,
    stopped_searches: Mutex<Vec<String>>,
}

/// Holds every `download` (or `start_search`) call until released, after announcing it.
#[derive(Clone)]
struct Hold {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

impl StubBackend {
    pub fn new(script: Vec<ScriptedOutcome>) -> Arc<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = format!("stub-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        Arc::new(Self {
            id: Box::leak(id.into_boxed_str()),
            script: Mutex::new(script),
            search_start: Mutex::new(None),
            polls: Mutex::new(VecDeque::new()),
            cancelled: Mutex::new(Vec::new()),
            destinations: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            listing: Mutex::new(Vec::new()),
            hold: Mutex::new(None),
            specs: Mutex::new(Vec::new()),
            search_hold: Mutex::new(None),
            stopped_searches: Mutex::new(Vec::new()),
        })
    }

    /// Every `cancel_download` call, in order.
    pub fn cancelled(&self) -> Vec<Cancel> {
        self.cancelled.lock().unwrap().clone()
    }

    /// The destination of every `download` call, in order.
    pub fn destinations(&self) -> Vec<String> {
        self.destinations.lock().unwrap().clone()
    }

    /// The request of every `download` call, in order, from when it arrives.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Makes each `download` call wait: the first notifier fires when a call
    /// arrives, and the call answers once the second one is notified.
    pub fn hold_downloads(&self) -> (Arc<Notify>, Arc<Notify>) {
        let hold = Hold {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *self.hold.lock().unwrap() = Some(hold.clone());
        (hold.reached, hold.release)
    }

    /// Makes each `start_search` call wait, as `hold_downloads` does for `download`. A
    /// `stop_search` releases a held search too, as slskd ends a search it is told to stop.
    pub fn hold_searches(&self) -> (Arc<Notify>, Arc<Notify>) {
        let hold = Hold {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *self.search_hold.lock().unwrap() = Some(hold.clone());
        (hold.reached, hold.release)
    }

    /// The spec of every `start_search` call, in order, from when it arrives.
    pub fn specs(&self) -> Vec<SearchSpec> {
        self.specs.lock().unwrap().clone()
    }

    /// The search id of every `stop_search` call, in order.
    pub fn stopped_searches(&self) -> Vec<String> {
        self.stopped_searches.lock().unwrap().clone()
    }

    /// Makes `start_search` fail with `e`.
    pub fn fail_search_start(&self, e: SoulseekError) {
        *self.search_start.lock().unwrap() = Some(e);
    }

    /// What `poll_search` answers, in order.
    pub fn script_polls(&self, polls: Vec<SoulResult<SearchResult>>) {
        *self.polls.lock().unwrap() = polls.into();
    }

    /// What `get_downloads_for` (filtered by peer) and `get_downloads` return.
    pub fn set_listing(&self, listing: Vec<DownloadProgress>) {
        *self.listing.lock().unwrap() = listing;
    }
}

/// One `cancel_download` call. `remove` also deletes slskd's record of the
/// transfer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cancel {
    pub peer: String,
    pub id: String,
    pub remove: bool,
}

impl Cancel {
    /// A cancel that leaves the record for a later cleanup.
    pub fn only(peer: &str, id: &str) -> Self {
        Self {
            peer: peer.to_string(),
            id: id.to_string(),
            remove: false,
        }
    }

    /// A cancel that also removes the record.
    pub fn removing(peer: &str, id: &str) -> Self {
        Self {
            remove: true,
            ..Self::only(peer, id)
        }
    }
}

/// A search poll's answer.
pub fn search_result(
    search_id: &str,
    state: SearchState,
    groups: Vec<DownloadableGroup>,
) -> SearchResult {
    SearchResult {
        search_id: search_id.to_string(),
        groups,
        has_more: state == SearchState::InProgress,
        state,
        stats: SearchStats::default(),
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
        self.id
    }
    fn name(&self) -> &'static str {
        "Stub"
    }
    async fn start_search(&self, spec: &SearchSpec) -> SoulResult<String> {
        self.specs.lock().unwrap().push(spec.clone());
        let hold = self.search_hold.lock().unwrap().clone();
        if let Some(hold) = hold {
            hold.reached.notify_one();
            hold.release.notified().await;
        }
        match self.search_start.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok("search".to_string()),
        }
    }
    async fn poll_search(&self, search_id: &str) -> SoulResult<SearchResult> {
        let next = self.polls.lock().unwrap().pop_front();
        next.unwrap_or_else(|| {
            Ok(search_result(
                search_id,
                SearchState::InProgress,
                Vec::new(),
            ))
        })
    }
    fn new_request(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
    ) -> SoulResult<Vec<QueuedDownload>> {
        self.requests.lock().unwrap().push(request.to_string());
        let hold = self.hold.lock().unwrap().clone();
        if let Some(hold) = hold {
            hold.reached.notify_one();
            hold.release.notified().await;
        }
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
            ScriptedOutcome::EnqueuedAs(file) => Ok(vec![QueuedDownload {
                id: format!("t-{file}"),
                source: item.source.clone(),
                item: file.to_string(),
                size: item.size.unwrap_or(0),
                status: EnqueueStatus::Enqueued,
            }]),
            ScriptedOutcome::AlreadyInFlight => answer(
                item.id.clone(),
                EnqueueStatus::AlreadyInFlight(Cause::new(Service::Slskd, None, STUB_IN_FLIGHT)),
            ),
            ScriptedOutcome::Rejected => answer(
                item.id.clone(),
                EnqueueStatus::Failed(Failure::new(
                    FailureCode::PeerOffline,
                    Stage::Enqueue,
                    RetryAdvice::OtherSource,
                    "peer offline",
                )),
            ),
            ScriptedOutcome::Unanswered => Ok(Vec::new()),
            // An unreadable answer stands in for any failure of the call;
            // its meaning doesn't matter here, only that `download` itself
            // errors rather than returning an Ok with a rejected item.
            ScriptedOutcome::TransportError => Err(SoulseekError::InvalidResponse(
                "batch enqueue response: expected value at line 1 column 1 (body 20 bytes)"
                    .to_string(),
            )),
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
    async fn get_download(&self, source: &str, id: &str) -> SoulResult<Option<DownloadProgress>> {
        Ok(self
            .listing
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.source == source && t.id == id)
            .cloned())
    }
    async fn cancel_download(&self, username: &str, id: &str, remove: bool) -> SoulResult<()> {
        self.cancelled.lock().unwrap().push(Cancel {
            peer: username.to_string(),
            id: id.to_string(),
            remove,
        });
        Ok(())
    }
    async fn queue_position(&self, _source: &str, _id: &str) -> SoulResult<Option<u32>> {
        Ok(None)
    }
    async fn source_status(&self, _source: &str) -> SoulResult<SourcePresence> {
        Ok(SourcePresence::Online)
    }
    async fn stop_search(&self, search_id: &str) -> SoulResult<()> {
        self.stopped_searches
            .lock()
            .unwrap()
            .push(search_id.to_string());
        if let Some(hold) = self.search_hold.lock().unwrap().as_ref() {
            hold.release.notify_one();
        }
        Ok(())
    }
    async fn health_check(&self) -> Result<(), Failure> {
        Ok(())
    }
}
