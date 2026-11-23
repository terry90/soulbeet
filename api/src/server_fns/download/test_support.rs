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
use soulbeet::{BackendError, BackendFault, BackendResult, DownloadBackend, ImportMode, PeerAsk};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Notify};

use crate::services::{BackendLookupError, Backends, DEFAULT_DOWNLOAD_BACKEND};
use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner};

pub fn item(source: &str, title: &str, id: &str) -> DownloadableItem {
    DownloadableItem {
        id: id.to_string(),
        backend: DEFAULT_DOWNLOAD_BACKEND.to_string(),
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

/// `item(source, title, id)` on the backend registered as `backend`.
pub fn item_on(backend: &str, source: &str, title: &str, id: &str) -> DownloadableItem {
    DownloadableItem {
        backend: backend.to_string(),
        ..item(source, title, id)
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

/// A batch of its own user on the default backend, so tests sharing the
/// process-wide registry never see each other's claims. Its folder is
/// `<label> [12345678]`, and it imports into `/music`.
pub fn batch(label: &str) -> BatchRef {
    batch_on(DEFAULT_DOWNLOAD_BACKEND, label)
}

/// `batch(label)` on the backend registered as `backend`.
pub fn batch_on(backend: &str, label: &str) -> BatchRef {
    let unique = uuid::Uuid::new_v4();
    BatchRef::new(
        TransferOwner {
            user_id: UserId::new(&format!("tester-{unique}")),
            batch_id: format!("12345678-{unique}"),
        },
        label.to_string(),
        PathBuf::from("/music"),
        ImportMode::Singleton,
        backend.to_string(),
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
        running.backend.clone(),
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
/// nothing found. Records every search spec and stop, destination, request,
/// listing and cancellation, and lists whatever transfers the test sets. Each stub
/// has its own backend id, so the registry keys of concurrent tests never
/// collide.
pub struct StubBackend {
    id: &'static str,
    script: Mutex<Vec<ScriptedOutcome>>,
    search_start: Mutex<Option<BackendError>>,
    polls: Mutex<VecDeque<BackendResult<SearchResult>>>,
    cancelled: Mutex<Vec<Cancel>>,
    destinations: Mutex<Vec<String>>,
    requests: Mutex<Vec<String>>,
    listing: Mutex<Vec<DownloadProgress>>,
    listed: Mutex<Vec<Vec<String>>>,
    hold: Mutex<Option<Hold>>,
    specs: Mutex<Vec<SearchSpec>>,
    search_hold: Mutex<Option<Hold>>,
    stopped_searches: Mutex<Vec<String>>,
    queue_position: Mutex<Option<u32>>,
    presence: Mutex<SourcePresence>,
    /// slskd's status and body for every `queue_position` and `source_status` call, while set.
    queue_position_error: Mutex<Option<(u16, String)>>,
    presence_error: Mutex<Option<(u16, String)>>,
    presence_asked: Mutex<Vec<String>>,
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
            listed: Mutex::new(Vec::new()),
            hold: Mutex::new(None),
            specs: Mutex::new(Vec::new()),
            search_hold: Mutex::new(None),
            stopped_searches: Mutex::new(Vec::new()),
            queue_position: Mutex::new(None),
            presence: Mutex::new(SourcePresence::Online),
            queue_position_error: Mutex::new(None),
            presence_error: Mutex::new(None),
            presence_asked: Mutex::new(Vec::new()),
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
        *self.search_start.lock().unwrap() = Some(e.into());
    }

    /// What `poll_search` answers, in order.
    pub fn script_polls(&self, polls: Vec<SoulResult<SearchResult>>) {
        *self.polls.lock().unwrap() = polls
            .into_iter()
            .map(|poll| poll.map_err(BackendError::from))
            .collect();
    }

    /// What `queue_position` answers for every transfer; None until set.
    pub fn set_queue_position(&self, position: Option<u32>) {
        *self.queue_position.lock().unwrap() = position;
    }

    /// What `source_status` answers for every source; Online until set.
    pub fn set_presence(&self, presence: SourcePresence) {
        *self.presence.lock().unwrap() = presence;
    }

    /// Makes every `queue_position` call fail as slskd answers `status` with `body`.
    pub fn fail_queue_position(&self, status: u16, body: &str) {
        *self.queue_position_error.lock().unwrap() = Some((status, body.to_string()));
    }

    /// Makes every `source_status` call fail as slskd answers `status` with `body`.
    pub fn fail_presence(&self, status: u16, body: &str) {
        *self.presence_error.lock().unwrap() = Some((status, body.to_string()));
    }

    /// The source of every `source_status` call, in order.
    pub fn presence_asked(&self) -> Vec<String> {
        self.presence_asked.lock().unwrap().clone()
    }

    /// What `get_downloads_for` returns, filtered by peer.
    pub fn set_listing(&self, listing: Vec<DownloadProgress>) {
        *self.listing.lock().unwrap() = listing;
    }

    /// The peers of every `get_downloads_for` call, in order.
    pub fn listings(&self) -> Vec<Vec<String>> {
        self.listed.lock().unwrap().clone()
    }
}

/// Looks each id up strictly: the stub registered under it, or the lookup
/// error `fail` set for it, and any other id is unknown. Records every id it
/// is asked for. While held, every lookup waits for `release`.
#[derive(Default)]
pub struct StubBackends {
    stubs: HashMap<String, Arc<StubBackend>>,
    failures: Mutex<HashMap<String, BackendLookupError>>,
    asked: Mutex<Vec<String>>,
    held: watch::Sender<bool>,
    calls_held: AtomicUsize,
}

impl StubBackends {
    /// These lookups with `stub` registered under `id`.
    pub fn with(mut self, id: &str, stub: Arc<StubBackend>) -> Self {
        self.stubs.insert(id.to_string(), stub);
        self
    }

    /// Makes every lookup of `id` answer `e`.
    pub fn fail(&self, id: &str, e: BackendLookupError) {
        self.failures.lock().unwrap().insert(id.to_string(), e);
    }

    /// Holds every lookup from now on until `release`.
    pub fn hold(&self) {
        self.held.send_replace(true);
    }

    pub fn release(&self) {
        self.held.send_replace(false);
    }

    /// The lookups waiting for `release`.
    pub fn calls_held(&self) -> usize {
        self.calls_held.load(Ordering::SeqCst)
    }

    /// Every id looked up, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Backends for StubBackends {
    async fn get(&self, id: &str) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
        self.asked.lock().unwrap().push(id.to_string());
        let mut held = self.held.subscribe();
        if *held.borrow_and_update() {
            self.calls_held.fetch_add(1, Ordering::SeqCst);
            let _ = held.wait_for(|held| !held).await;
            self.calls_held.fetch_sub(1, Ordering::SeqCst);
        }
        if let Some(e) = self.failures.lock().unwrap().get(id) {
            return Err(e.clone());
        }
        match self.stubs.get(id) {
            Some(stub) => Ok(stub.clone()),
            None => Err(BackendLookupError::Unknown(id.to_string())),
        }
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

/// A fault of a backend other than slskd. `unreadable` is what its
/// `answer_unreadable` says.
#[derive(Debug)]
pub struct StubFault {
    pub unreadable: bool,
}

impl std::fmt::Display for StubFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.unreadable {
            "stub answer unreadable"
        } else {
            "stub unreachable"
        })
    }
}

impl BackendFault for StubFault {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        let (code, retry) = if self.unreadable {
            (
                FailureCode::BackendResponseUnreadable,
                RetryAdvice::NotRetryable,
            )
        } else {
            (
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            )
        };
        Failure::new(code, stage, retry, message)
    }

    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        self.failure(stage, format!("stub could not answer {ask:?}"))
    }

    fn answer_unreadable(&self) -> bool {
        self.unreadable
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
    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
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
    async fn poll_search(&self, search_id: &str) -> BackendResult<SearchResult> {
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
    ) -> BackendResult<Vec<QueuedDownload>> {
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
            )
            .into()),
        }
    }
    async fn get_downloads_for(&self, sources: &[String]) -> BackendResult<Vec<DownloadProgress>> {
        self.listed.lock().unwrap().push(sources.to_vec());
        Ok(self
            .listing
            .lock()
            .unwrap()
            .iter()
            .filter(|t| sources.contains(&t.source))
            .cloned()
            .collect())
    }
    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> BackendResult<Option<DownloadProgress>> {
        Ok(self
            .listing
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.source == source && t.id == id)
            .cloned())
    }
    async fn cancel_download(&self, username: &str, id: &str, remove: bool) -> BackendResult<()> {
        self.cancelled.lock().unwrap().push(Cancel {
            peer: username.to_string(),
            id: id.to_string(),
            remove,
        });
        Ok(())
    }
    async fn queue_position(&self, _source: &str, _id: &str) -> BackendResult<Option<u32>> {
        if let Some((status, message)) = self.queue_position_error.lock().unwrap().clone() {
            return Err(SoulseekError::Api { status, message }.into());
        }
        Ok(*self.queue_position.lock().unwrap())
    }
    async fn source_status(&self, source: &str) -> BackendResult<SourcePresence> {
        self.presence_asked.lock().unwrap().push(source.to_string());
        if let Some((status, message)) = self.presence_error.lock().unwrap().clone() {
            return Err(SoulseekError::Api { status, message }.into());
        }
        Ok(*self.presence.lock().unwrap())
    }
    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
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
