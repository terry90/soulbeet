use super::{processing, wildcard};
use crate::matching::path::WantedTrack;
use crate::{
    backend_error::BackendResult,
    error::{MissingSetting, Result, SoulseekError},
    http::{resolve_docker_url, CircuitBreaker},
    slskd::models::{
        BatchEnqueueOptions, BatchEnqueueRequest, BatchEnqueueResponse, DownloadRequestFile,
        SearchResponse,
    },
};
use chrono::{DateTime, Duration, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
    download::{EnqueueStatus, SearchSpec, SearchStats, SourcePresence},
    failure::{
        Cause, Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
    },
    metadata::{Album, Track},
    slskd::{
        AlbumResult, DownloadResponse, FileEntry, FlattenedFiles, SearchPriority, SearchState,
        TrackResult,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration as StdDuration,
};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use url::Url;

const MAX_SEARCH_RESULTS: usize = 50;

const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Configuration for download batching to avoid overwhelming the slskd API.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Maximum number of files to send in a single request per user.
    pub batch_size: usize,
    /// Delay between batches in milliseconds.
    pub batch_delay_ms: u64,
    /// Maximum number of retries for failed batches.
    pub max_retries: usize,
    /// Base delay for exponential backoff in milliseconds.
    pub retry_base_delay_ms: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            batch_size: 3,
            batch_delay_ms: 1000,
            max_retries: 3,
            retry_base_delay_ms: 2000,
        }
    }
}

#[derive(Debug, Clone)]
struct SearchContext {
    artist: String,
    album: Option<String>,
    tracks: Vec<WantedTrack>,
    /// The text slskd was sent first.
    query_text: String,
    /// The texts sent in turn while no peer answers (`wildcard::retry_texts`).
    retry_texts: Vec<String>,
    /// How many of `retry_texts` were sent.
    retried: usize,
    /// The slskd search running now: the search's own id until a retry text is sent.
    slskd_id: String,
    keep_below_floor: bool,
    /// When the first text was sent: the deadline counts from it for every retry.
    start_time: DateTime<Utc>,
    timeout: Duration,
    /// `stop_search` was called: no retry text is sent any more.
    stop_requested: bool,
    /// slskd was told to stop the slskd search running now, by `stop_search` or at the
    /// deadline.
    cancelled: bool,
    /// Priority class preserved across unanswered wildcard retries.
    priority: SearchPriority,
}

/// What one poll of a search found: the processed albums and what the search
/// read, both empty while the search is in progress.
#[derive(Debug)]
pub struct SearchPoll {
    pub albums: Vec<AlbumResult>,
    pub state: SearchState,
    pub stats: SearchStats,
}

/// The part of slskd's search record a poll reads (`GET searches/{id}`,
/// `SearchesController.cs:140`; `Search/Types/Search.cs:52,55,72`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchStatus {
    is_complete: bool,
    response_count: u32,
    /// slskd's `SearchStates` flags, written as `JsonStringEnumConverter` writes a flags enum
    /// (`Program.cs:980`): `Completed, TimedOut`.
    state: String,
}

impl SearchStatus {
    /// Whether the search ended with `flag` among its states: `Errored` when Soulseek.NET threw
    /// during the search, `Cancelled` when its token was cancelled (`SearchService.cs:331-345`).
    fn ended_as(&self, flag: &str) -> bool {
        self.state.split(", ").any(|state| state == flag)
    }
}

/// What a poll does after reading a search's status.
enum PollStep {
    /// The search ended: read its responses.
    Read,
    /// The search runs: look again.
    Wait,
    /// The deadline passed: tell slskd to stop the search.
    Cancel,
    /// The search kept running after it was cancelled.
    GiveUp,
}

/// How long a cancelled search may take to end.
const CANCEL_GRACE: Duration = Duration::seconds(15);

/// slskd marks a search complete before it writes the responses
/// (`Search/SearchService.cs:284-285`, then `:359-361`), so an empty answer
/// to a search that counted responses is read again after this delay.
const RESPONSES_REREAD_DELAY: StdDuration = StdDuration::from_millis(500);
/// How many times such an answer is read again before the responses count
/// as lost.
const RESPONSES_REREADS: u32 = 6;

/// The step for a search `elapsed` into its `deadline`.
fn poll_step(complete: bool, elapsed: Duration, deadline: Duration, cancelled: bool) -> PollStep {
    if complete {
        PollStep::Read
    } else if elapsed < deadline {
        PollStep::Wait
    } else if !cancelled {
        PollStep::Cancel
    } else if elapsed < deadline + CANCEL_GRACE {
        PollStep::Wait
    } else {
        PollStep::GiveUp
    }
}

/// Default burst allowance before steady-state GCRA pacing kicks in.
const DEFAULT_BURST_SIZE: usize = 4;
/// Minimum spacing between consecutive searches during a burst.
const DEFAULT_BURST_SPACING: StdDuration = StdDuration::from_secs(1);
/// Window slots reserved exclusively for `SearchPriority::Interactive` callers
/// (when the window holds more than 5 slots).
const DEFAULT_INTERACTIVE_RESERVE: usize = 5;

#[derive(Debug, Default)]
struct PacerState {
    timestamps: VecDeque<TokioInstant>,
    tat: Option<TokioInstant>,
    last_sent: Option<TokioInstant>,
    total_recorded: usize,
}

/// Paces slskd search requests against the Soulseek 35 / 220s rate limit while:
/// - serializing `POST /api/v0/searches` calls so we never race slskd's internal
///   `SemaphoreSlim(1, 1)` into self-inflicted 429s,
/// - recording quota ONLY after a POST succeeds/is accepted,
/// - allowing a small initial burst (4 searches at 1s spacing) and smoothly pacing
///   steady-state traffic at `window / max_searches` (~6.3s), and
/// - reserving headroom (5 of 35 slots) and priority turn order for interactive callers.
#[derive(Debug)]
pub(crate) struct SearchPacer {
    max_searches: usize,
    window: StdDuration,
    interactive_reserve: usize,
    burst_size: usize,
    burst_spacing: StdDuration,
    steady_interval: StdDuration,
    post_gate: Mutex<()>,
    state: StdMutex<PacerState>,
    interactive_waiting: AtomicUsize,
    turn_notify: Notify,
}

struct InteractiveWaiter<'a>(&'a SearchPacer);

impl Drop for InteractiveWaiter<'_> {
    fn drop(&mut self) {
        self.0.interactive_waiting.fetch_sub(1, Ordering::AcqRel);
        self.0.turn_notify.notify_waiters();
    }
}

/// Exclusive permit to execute a single `POST /api/v0/searches` call.
/// Dropping the permit without calling `record_accepted()` consumes zero quota.
pub(crate) struct SearchPermit<'a> {
    pacer: &'a SearchPacer,
    _post_guard: MutexGuard<'a, ()>,
    _interactive_guard: Option<InteractiveWaiter<'a>>,
}

impl SearchPermit<'_> {
    /// Records that slskd accepted the search POST at the current instant.
    pub(crate) fn record_accepted(&self) {
        let mut state = self.pacer.state.lock().unwrap_or_else(|e| e.into_inner());
        self.pacer.record_accepted_at(&mut state, TokioInstant::now());
    }
}

impl Drop for SearchPermit<'_> {
    fn drop(&mut self) {
        self.pacer.turn_notify.notify_waiters();
    }
}

impl SearchPacer {
    pub(crate) fn new(max_searches: usize, window: StdDuration) -> Self {
        let max_searches = max_searches.max(1);
        let steady_interval = window / (max_searches as u32);
        let interactive_reserve = if max_searches > DEFAULT_INTERACTIVE_RESERVE {
            DEFAULT_INTERACTIVE_RESERVE
        } else {
            0
        };
        let burst_size = DEFAULT_BURST_SIZE.min(max_searches);
        let burst_spacing = DEFAULT_BURST_SPACING.min(steady_interval);
        Self {
            max_searches,
            window,
            interactive_reserve,
            burst_size,
            burst_spacing,
            steady_interval,
            post_gate: Mutex::new(()),
            state: StdMutex::new(PacerState::default()),
            interactive_waiting: AtomicUsize::new(0),
            turn_notify: Notify::new(),
        }
    }

    /// Acquires an exclusive search permit for `priority`, waiting for both
    /// priority turn order and rate-limit / burst pacing as needed.
    pub(crate) async fn acquire(&self, priority: SearchPriority) -> SearchPermit<'_> {
        let interactive_guard = (priority == SearchPriority::Interactive).then(|| {
            self.interactive_waiting.fetch_add(1, Ordering::AcqRel);
            InteractiveWaiter(self)
        });

        loop {
            if priority == SearchPriority::Background
                && self.interactive_waiting.load(Ordering::Acquire) > 0
            {
                let notified = self.turn_notify.notified();
                if self.interactive_waiting.load(Ordering::Acquire) > 0 {
                    notified.await;
                    continue;
                }
            }

            let post_guard = self.post_gate.lock().await;

            if priority == SearchPriority::Background
                && self.interactive_waiting.load(Ordering::Acquire) > 0
            {
                drop(post_guard);
                tokio::task::yield_now().await;
                continue;
            }

            let (wait, window_len, limit) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let wait = self.compute_wait(&mut state, priority, TokioInstant::now());
                (wait, state.timestamps.len(), self.effective_limit(priority))
            };

            if wait.is_zero() {
                return SearchPermit {
                    pacer: self,
                    _post_guard: post_guard,
                    _interactive_guard: interactive_guard,
                };
            }

            info!(
                "Search pacer ({:?}, {}/{} in window): waiting {:.2}s",
                priority,
                window_len,
                limit,
                wait.as_secs_f64()
            );
            // Release the POST gate while sleeping so interactive callers can
            // proceed immediately without waiting behind a background sleep.
            drop(post_guard);
            tokio::time::sleep(wait).await;
        }
    }

    fn effective_limit(&self, priority: SearchPriority) -> usize {
        match priority {
            SearchPriority::Interactive => self.max_searches,
            SearchPriority::Background => self
                .max_searches
                .saturating_sub(self.interactive_reserve)
                .max(1),
        }
    }

    fn compute_wait(
        &self,
        state: &mut PacerState,
        priority: SearchPriority,
        now: TokioInstant,
    ) -> StdDuration {
        while let Some(&oldest) = state.timestamps.front() {
            if now.saturating_duration_since(oldest) >= self.window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }

        let mut wait = StdDuration::ZERO;

        // 1. Hard sliding window and interactive headroom reserve.
        let limit = self.effective_limit(priority);
        if state.timestamps.len() >= limit {
            let release_idx = state.timestamps.len() - limit;
            let release_at = state.timestamps[release_idx] + self.window;
            wait = wait.max(release_at.saturating_duration_since(now));
        }

        // 2. Minimum inter-search spacing during bursts.
        if let Some(last) = state.last_sent {
            let min_next = last + self.burst_spacing;
            wait = wait.max(min_next.saturating_duration_since(now));
        }

        // 3. GCRA steady-state pacing once the burst budget is spent.
        if let Some(tat) = state.tat {
            let debt = tat.saturating_duration_since(now);
            let burst_allowance = self
                .steady_interval
                .saturating_mul(self.burst_size.saturating_sub(1) as u32);
            if debt > burst_allowance {
                wait = wait.max(debt - burst_allowance);
            }
        }

        wait
    }

    fn record_accepted_at(&self, state: &mut PacerState, now: TokioInstant) {
        state.timestamps.push_back(now);
        state.last_sent = Some(now);
        let base = state.tat.map_or(now, |t| t.max(now));
        state.tat = Some(base + self.steady_interval);
        state.total_recorded += 1;
    }

    #[cfg(test)]
    pub(crate) fn total_recorded(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total_recorded
    }
}

#[derive(Debug)]
pub struct SoulseekClient {
    base_url: Url,
    api_key: Option<String>,
    client: Client,
    search_pacer: Arc<SearchPacer>,
    active_searches: Arc<Mutex<HashMap<String, SearchContext>>>,
    download_config: DownloadConfig,
    circuit_breaker: Arc<CircuitBreaker>,
    /// slskd's download retry limit, read on the first listing.
    retry_limit: tokio::sync::OnceCell<u32>,
}

#[derive(Default)]
pub struct SoulseekClientBuilder {
    base_url: Option<String>,
    api_key: Option<String>,
    max_searches_per_window: Option<usize>,
    rate_limit_window_seconds: Option<i64>,
    download_config: Option<DownloadConfig>,
}

impl SoulseekClientBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn base_url(mut self, url: &str) -> Self {
        let resolved_url = resolve_docker_url(url);
        self.base_url = Some(resolved_url);
        self
    }

    pub fn api_key(mut self, key: &str) -> Self {
        self.api_key = Some(key.to_string());
        self
    }

    pub fn rate_limit(mut self, max_searches: usize, window_seconds: i64) -> Self {
        self.max_searches_per_window = Some(max_searches);
        self.rate_limit_window_seconds = Some(window_seconds);
        self
    }

    pub fn download_config(mut self, config: DownloadConfig) -> Self {
        self.download_config = Some(config);
        self
    }

    /// A builder for the slskd at `url` with `api_key`, as Soulful stores
    /// them: a setting that is missing or empty is `NotConfigured`.
    pub fn from_settings(url: Option<String>, api_key: Option<String>) -> Result<Self> {
        let url = url.ok_or(SoulseekError::NotConfigured(MissingSetting::Url))?;
        let api_key = api_key.ok_or(SoulseekError::NotConfigured(MissingSetting::ApiKey))?;
        if url.is_empty() || api_key.is_empty() {
            return Err(SoulseekError::NotConfigured(MissingSetting::Empty));
        }
        Ok(Self::new().base_url(&url).api_key(&api_key))
    }

    pub fn build(self) -> Result<SoulseekClient> {
        let base_url_str = self
            .base_url
            .ok_or(SoulseekError::NotConfigured(MissingSetting::Url))?;
        let base_url = Url::parse(base_url_str.trim_end_matches('/'))?;

        // Build HTTP client with proper timeouts
        let client = Client::builder()
            .connect_timeout(StdDuration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
            .timeout(StdDuration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .pool_idle_timeout(StdDuration::from_secs(90))
            .build()
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("Failed to build HTTP client: {}", e),
            })?;

        let max_searches = self.max_searches_per_window.unwrap_or(35);
        let window_secs = self.rate_limit_window_seconds.unwrap_or(220).max(1) as u64;
        Ok(SoulseekClient {
            base_url,
            api_key: self.api_key,
            client,
            search_pacer: Arc::new(SearchPacer::new(
                max_searches,
                StdDuration::from_secs(window_secs),
            )),
            active_searches: Arc::new(Mutex::new(HashMap::new())),
            download_config: self.download_config.unwrap_or_default(),
            circuit_breaker: Arc::new(CircuitBreaker::default()),
            retry_limit: tokio::sync::OnceCell::new(),
        })
    }
}

impl SoulseekClient {
    async fn make_request<T: DeserializeOwned, B: Serialize + Clone>(
        &self,
        method: Method,
        endpoint: &str,
        body: Option<B>,
    ) -> Result<T> {
        const MAX_429_RETRIES: u32 = 3;
        const DEFAULT_RETRY_AFTER_SECS: u64 = 5;

        for attempt in 0..=MAX_429_RETRIES {
            // Check circuit breaker before making request
            if self.circuit_breaker.is_open().await {
                warn!(
                    "Circuit breaker is open ({} consecutive failures), rejecting request to {}",
                    self.circuit_breaker.failure_count().await,
                    endpoint
                );
                return Err(SoulseekError::Api {
                    status: 503,
                    message: "Circuit breaker is open - slskd appears to be unavailable"
                        .to_string(),
                });
            }

            let url = self.base_url.join(&format!("api/v0/{endpoint}"))?;
            debug!("Request: {} {} (attempt {})", method, url, attempt + 1);
            let mut request = self.client.request(method.clone(), url);
            if let Some(key) = &self.api_key {
                request = request.header("X-API-Key", key);
            }
            if let Some(ref b) = body {
                request = request.json(b);
            }

            let response = match request.send().await {
                Ok(resp) => {
                    self.circuit_breaker.record_success().await;
                    resp
                }
                Err(e) => {
                    self.circuit_breaker.record_failure().await;
                    // A failure never holds a request URL (design 10): the URL names the
                    // slskd host and the peer asked about.
                    let e = e.without_url();
                    if e.is_timeout() {
                        warn!("Request to {} timed out", endpoint);
                        return Err(SoulseekError::Api {
                            status: 408,
                            message: format!("Request timed out: {}", e),
                        });
                    }
                    if e.is_connect() {
                        warn!("Failed to connect to slskd at {}", endpoint);
                        return Err(SoulseekError::Api {
                            status: 503,
                            message: format!("Connection failed: {}", e),
                        });
                    }
                    return Err(e.into());
                }
            };

            // Handle 429 rate limiting: wait and retry
            if response.status().as_u16() == 429 {
                if attempt < MAX_429_RETRIES {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(DEFAULT_RETRY_AFTER_SECS);
                    warn!(
                        "Rate limited (429) on {}, waiting {}s before retry {}/{}",
                        endpoint,
                        retry_after,
                        attempt + 1,
                        MAX_429_RETRIES
                    );
                    tokio::time::sleep(StdDuration::from_secs(retry_after)).await;
                    continue;
                }
                // Exhausted 429 retries
                let text = response.text().await.unwrap_or_default();
                return Err(SoulseekError::Api {
                    status: 429,
                    message: format!("Rate limited after {} retries: {}", MAX_429_RETRIES, text),
                });
            }

            return Self::handle_response(response).await;
        }

        // Unreachable, but required by the compiler
        Err(SoulseekError::Api {
            status: 429,
            message: "Rate limited: max retries exceeded".to_string(),
        })
    }

    async fn handle_response<T: DeserializeOwned>(response: Response) -> Result<T> {
        let status = response.status();
        if status.is_success() {
            // A 2xx body that does not parse is schema drift, not an API
            // error: InvalidResponse lets callers distinguish it from
            // transport failures (the download monitor bounds it).
            let text = response.text().await?;
            if text.trim().is_empty() {
                serde_json::from_str("null")
                    .map_err(|e| SoulseekError::InvalidResponse(format!("JSON parse error: {e}")))
            } else {
                serde_json::from_str(&text)
                    .map_err(|e| SoulseekError::InvalidResponse(format!("JSON parse error: {e}")))
            }
        } else {
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "Could not read error body".to_string());
            Err(SoulseekError::Api {
                status: status.as_u16(),
                message: text,
            })
        }
    }

    /// The search for `spec`: `spec.text` trimmed when given, else the
    /// `search_parts` of its album and tracks joined by a space, which a search needs either way
    /// to score what it finds. While no peer answers it, the search is sent
    /// again under the same id with the texts `wildcard::retry_texts` gives
    /// for the caller's text, or for the artist and title.
    pub async fn start_search(&self, spec: SearchSpec, timeout: Duration) -> Result<String> {
        let SearchSpec {
            album,
            tracks,
            text,
            keep_below_floor,
            priority,
        } = spec;
        let parts = search_parts(album.as_ref(), &tracks)?;
        let (query, retry_texts) = match text.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => (text.to_string(), wildcard::retry_texts(&[text])),
            _ => (parts.join(" "), wildcard::retry_texts(&parts)),
        };

        let wanted: Vec<WantedTrack> = tracks
            .iter()
            .map(|t| WantedTrack {
                title: t.title.clone(),
                duration_s: t.duration_secs(),
            })
            .collect();

        info!(
            "Starting {:?} search for: '{}' with timeout {}ms",
            priority,
            query,
            timeout.num_milliseconds()
        );
        let permit = self.search_pacer.acquire(priority).await;
        let search_id = self.post_search(permit, &query).await?;

        self.active_searches.lock().await.insert(
            search_id.clone(),
            SearchContext {
                album: album.as_ref().map(|a| a.title.clone()),
                artist: album
                    .as_ref()
                    .map(|a| a.artist.clone())
                    .unwrap_or_else(|| tracks[0].artist.clone()),
                tracks: wanted,
                query_text: query,
                retry_texts,
                retried: 0,
                slskd_id: search_id.clone(),
                keep_below_floor,
                start_time: Utc::now(),
                timeout,
                stop_requested: false,
                cancelled: false,
                priority,
            },
        );

        info!("Search initiated with ID: {search_id}");
        Ok(search_id)
    }

    /// Starts one slskd search for `text` while holding `permit` so concurrent POSTs
    /// never race slskd's internal SemaphoreSlim(1, 1). Records quota ONLY after
    /// `POST /api/v0/searches` succeeds.
    async fn post_search(&self, permit: SearchPermit<'_>, text: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct SearchId {
            id: String,
        }
        let started: SearchId = self
            .make_request(Method::POST, "searches", Some(&SearchRequest::new(text)))
            .await
            .map_err(|e| match e {
                SoulseekError::Api {
                    status: 400,
                    message,
                } => search_refused(message),
                e => e,
            })?;
        permit.record_accepted();
        Ok(started.id)
    }

    /// One poll of `search_id`, holding for up to 10 seconds while the search
    /// runs. slskd 0.26 writes the responses only when the search ends
    /// (`SearchService.cs:359`; `Search/Extensions.cs:135-149` drops them on
    /// every earlier update), so a poll returns albums only once it ends.
    /// A search that ended or failed is deleted from slskd.
    pub async fn poll_search(&self, search_id: String) -> Result<SearchPoll> {
        let Some(context) = self.active_searches.lock().await.get(&search_id).cloned() else {
            return Err(SoulseekError::UnknownSearch { search_id });
        };

        let polled = self.poll_turns(&search_id, context).await;
        if matches!(&polled, Ok(poll) if poll.state == SearchState::InProgress) {
            return polled;
        }
        let ended = self.active_searches.lock().await.remove(&search_id);
        if let Some(ended) = ended {
            let _ = self.delete_search(&ended.slskd_id).await;
        }
        match &polled {
            Ok(poll) => info!(
                "Search {} completed with {} files from slskd",
                search_id, poll.stats.file_count
            ),
            Err(e) => warn!("Search {} failed with error, cleaning up: {}", search_id, e),
        }
        polled
    }

    /// Reads the status of `search_id` and acts on it until the search ends,
    /// fails or the long poll is over.
    async fn poll_turns(&self, search_id: &str, mut context: SearchContext) -> Result<SearchPoll> {
        let poll_start = Utc::now();
        let long_poll_timeout = Duration::seconds(10);

        loop {
            let endpoint = format!("searches/{}", context.slskd_id);
            let status: SearchStatus = self
                .make_request(Method::GET, &endpoint, None::<()>)
                .await
                .map_err(poll_refused)?;
            let elapsed = Utc::now() - context.start_time;
            match poll_step(
                status.is_complete,
                elapsed,
                context.timeout,
                context.cancelled,
            ) {
                PollStep::Read => {
                    let mut poll = self.read_responses(&context, status.response_count).await?;
                    if poll.stats.response_count > 0 {
                        return Ok(poll);
                    }
                    // No peer answered because the search failed, not because none was asked.
                    if status.ended_as("Errored") {
                        return Err(SoulseekError::SearchErrored {
                            search_id: search_id.to_string(),
                        });
                    }
                    if status.ended_as("Cancelled") || self.stopped(search_id).await {
                        return Ok(poll);
                    }
                    // A retry runs under the same id: this poll goes on reading it.
                    if self.retry_unanswered(search_id, &mut context).await {
                        continue;
                    }
                    if !self.stopped(search_id).await {
                        poll.stats.unanswered_note = Some(unanswered_note(context.retried > 0));
                    }
                    return Ok(poll);
                }
                PollStep::Wait => {
                    if (Utc::now() - poll_start) > long_poll_timeout {
                        return Ok(SearchPoll {
                            albums: Vec::new(),
                            state: SearchState::InProgress,
                            stats: SearchStats::default(),
                        });
                    }
                    tokio::time::sleep(StdDuration::from_secs(1)).await;
                }
                PollStep::Cancel => {
                    info!("Search {} reached its deadline, cancelling it", search_id);
                    // slskd answers 200 also for a search that ended
                    // meanwhile (`SearchesController.cs:209-227`).
                    self.make_request::<(), ()>(Method::PUT, &endpoint, None)
                        .await
                        .map_err(poll_refused)?;
                    context.cancelled = true;
                    if let Some(ctx) = self.active_searches.lock().await.get_mut(search_id) {
                        ctx.cancelled = true;
                    }
                }
                PollStep::GiveUp => {
                    return Err(SoulseekError::SearchNotEnding {
                        search_id: search_id.to_string(),
                    })
                }
            }
        }
    }

    /// The responses of the slskd search `context` runs, which slskd completed counting
    /// `response_count`.
    async fn read_responses(
        &self,
        context: &SearchContext,
        response_count: u32,
    ) -> Result<SearchPoll> {
        let endpoint = format!("searches/{}/responses", context.slskd_id);
        let mut rereads = 0;
        let responses = loop {
            let responses: Vec<SearchResponse> = self
                .make_request(Method::GET, &endpoint, None::<()>)
                .await
                .map_err(poll_refused)?;
            if !responses.is_empty() || response_count == 0 {
                break responses;
            }
            if rereads == RESPONSES_REREADS {
                return Err(SoulseekError::SearchResponsesLost { response_count });
            }
            rereads += 1;
            tokio::time::sleep(RESPONSES_REREAD_DELAY).await;
        };

        let processing::Processed {
            mut albums,
            not_audio,
            below_floor,
            rejection_counts,
            top_rejected,
        } = processing::process_search_responses(
            &responses,
            &context.artist,
            context.album.as_deref(),
            &context.tracks,
            context.keep_below_floor,
        );
        albums.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        albums.truncate(MAX_SEARCH_RESULTS);

        Ok(SearchPoll {
            albums,
            state: SearchState::Completed,
            stats: SearchStats {
                query_text: context.query_text.clone(),
                response_count: responses.len() as u32,
                file_count: responses.iter().map(|r| r.files.len() as u32).sum(),
                locked_file_count: responses.iter().map(|r| r.locked_file_count).sum(),
                not_audio,
                below_floor,
                retried_texts: context.retry_texts[..context.retried].to_vec(),
                unanswered_note: None,
                rejection_counts,
                top_rejected,
            },
        })
    }

    /// Whether search `search_id` was stopped, by `stop_search` or at its deadline.
    async fn stopped(&self, search_id: &str) -> bool {
        (self.active_searches.lock().await)
            .get(search_id)
            .is_some_and(|context| context.stop_requested || context.cancelled)
    }

    /// Sends the next retry text of search `search_id`, whose slskd search `context` names drew
    /// no response: whether it was sent. None is sent once the texts ran out, once the search
    /// was stopped, or when less than the inactivity timeout is left before the deadline, too
    /// little for a search no peer answers to end on its own; a text slskd refuses is logged.
    /// The new slskd search takes the old one's place in `context` and in the search's entry,
    /// and the old one is deleted.
    async fn retry_unanswered(&self, search_id: &str, context: &mut SearchContext) -> bool {
        let Some(text) = context.retry_texts.get(context.retried).cloned() else {
            return false;
        };
        let time_left = || {
            context.timeout - (Utc::now() - context.start_time)
                >= Duration::milliseconds(SEARCH_INACTIVITY_MS.into())
        };
        if !time_left() {
            return false;
        }
        // Checked again after the wait for the rate limit, which can outlast the deadline.
        let permit = self.search_pacer.acquire(context.priority).await;
        if !time_left() || self.stopped(search_id).await {
            return false;
        }
        info!("Search {search_id} drew no response, sending it again as '{text}'");
        let next = match self.post_search(permit, &text).await {
            Ok(next) => next,
            Err(e) => {
                warn!("Search {search_id} could not be sent again as '{text}': {e}");
                return false;
            }
        };
        let ended = std::mem::replace(&mut context.slskd_id, next.clone());
        context.retried += 1;
        // Recorded before the old search is deleted: a poll dropped meanwhile leaves the entry
        // naming the search that runs, and its next poll reads that one.
        let stop_requested = match self.active_searches.lock().await.get_mut(search_id) {
            Some(entry) => {
                entry.slskd_id = next.clone();
                entry.retried = context.retried;
                entry.stop_requested
            }
            None => false,
        };
        let _ = self.delete_search(&ended).await;
        if stop_requested {
            // `stop_search` asked slskd to stop the search that ended.
            let endpoint = format!("searches/{next}");
            match self
                .make_request::<(), ()>(Method::PUT, &endpoint, None)
                .await
            {
                Ok(()) => {
                    context.cancelled = true;
                    if let Some(entry) = self.active_searches.lock().await.get_mut(search_id) {
                        entry.cancelled = true;
                    }
                }
                Err(e) => warn!("slskd did not stop the retry of stopped search {search_id}: {e}"),
            }
        }
        true
    }

    /// Enqueue `req` into `destination`, a folder under slskd's download
    /// directory that every file of the call shares, in slskd batches of the
    /// request `request` (`shared::slskd::request_batch_id`).
    pub async fn download(
        &self,
        req: Vec<TrackResult>,
        destination: &str,
        request: &str,
    ) -> Result<Vec<DownloadResponse>> {
        info!(
            "Attempting to download: {} files into {:?}",
            req.len(),
            destination
        );
        let config = &self.download_config;
        let wanted = req
            .into_iter()
            .map(|r| {
                (
                    r.base.username,
                    DownloadRequestFile {
                        filename: r.base.filename,
                        size: r.base.size,
                    },
                )
            })
            .collect();

        let mut results = Vec::new();
        for batch in plan_batches(wanted, destination, config.batch_size, request) {
            if batch.index > 0 {
                debug!("Waiting {}ms before next batch", config.batch_delay_ms);
                tokio::time::sleep(tokio::time::Duration::from_millis(config.batch_delay_ms)).await;
            }
            results.extend(retry_batch(config, &batch, |b| self.send_download_batch(b)).await);
        }
        Ok(results)
    }

    async fn send_download_batch(&self, batch: &PendingBatch) -> Result<Vec<DownloadResponse>> {
        let url = self.base_url.join("api/v0/transfers/downloads/batches")?;

        info!(
            "Sending batch {} ({}) to '{}': {} files",
            batch.index,
            batch.id,
            batch.username,
            batch.files.len()
        );
        debug!("Batch payload: {:?}", batch.files);

        let response = self
            .client
            .post(url)
            .header("X-API-Key", self.api_key.as_deref().unwrap_or(""))
            .json(&batch.request())
            .send()
            .await?;

        let status = response.status();
        let resp_text = response.text().await?;

        info!(
            "Batch {} response: status={}, body_len={}",
            batch.index,
            status,
            resp_text.len()
        );

        if !status.is_success() {
            // slskd answers 404 with this text for offline users, and
            // deliberately for blacklisted ones too. Matched on any status so
            // classification survives a future status-code change.
            if resp_text.to_lowercase().contains("appears to be offline") {
                warn!("User '{}' is offline, not retrying", batch.username);
                return Err(SoulseekError::UserOffline {
                    username: batch.username.clone(),
                    status: status.as_u16(),
                    body: resp_text,
                });
            }

            // The batches route only exists since slskd 0.26.
            if status.as_u16() == 404 {
                return Err(SoulseekError::SlskdTooOld { body: resp_text });
            }

            // An earlier attempt created this batch and its answer was lost.
            if status.as_u16() == 409 {
                warn!(
                    "Batch {} already exists in slskd, reading its transfers back",
                    batch.id
                );
                let listing = self
                    .get_downloads_for_users(std::slice::from_ref(&batch.username))
                    .await?;
                return Ok(replayed_batch_responses(batch, &listing));
            }

            return Err(SoulseekError::Api {
                status: status.as_u16(),
                message: resp_text,
            });
        }

        parse_batch_enqueue_response(&batch.username, &batch.files, &resp_text)
    }

    /// How many attempts slskd makes at a download. Concurrent first callers
    /// wait for one read; a failed read is tried again on the next call.
    async fn retry_limit(&self) -> Result<u32> {
        self.retry_limit
            .get_or_try_init(|| async {
                let options: serde_json::Value = self
                    .make_request(Method::GET, "options", None::<()>)
                    .await?;
                retry_limit_from_options(&options)
            })
            .await
            .copied()
    }

    /// Report entries that failed to parse rather than dropping them: a
    /// silent skip is indistinguishable from "no downloads in progress",
    /// which is how the 0.26 schema drift went unnoticed (#73). Only a
    /// wholesale failure is fatal, since one bad entry among good ones is
    /// a single broken transfer, not schema drift.
    fn usable_files(files: Vec<FileEntry>, errors: Vec<String>) -> Result<Vec<FileEntry>> {
        if !errors.is_empty() {
            if files.is_empty() {
                return Err(SoulseekError::InvalidResponse(format!(
                    "all {} transfer entries failed to parse; first error: {}",
                    errors.len(),
                    errors[0]
                )));
            }
            warn!(
                "Skipped {} unparseable slskd transfer entries; first error: {}",
                errors.len(),
                errors[0]
            );
        }
        Ok(files)
    }

    /// Transfers for the given peers only.
    ///
    /// slskd retains every completed transfer with `Removed = 0` until the
    /// user clears it, so `GET transfers/downloads` grows without bound: one
    /// instance answered with 40,092 transfers in 24.7 MB of JSON, which a
    /// 2-second poll could not read before timing out, and the circuit
    /// breaker then stayed open (#78). Asking per peer bounds the response
    /// by that peer's history rather than the whole instance's.
    pub async fn get_downloads_for_users(&self, usernames: &[String]) -> Result<Vec<FileEntry>> {
        let mut requested: Vec<&String> = Vec::new();
        for username in usernames {
            if !requested.contains(&username) {
                requested.push(username);
            }
        }

        let mut files: Vec<FileEntry> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for username in requested {
            let endpoint = match user_downloads_endpoint(username) {
                Ok(endpoint) => endpoint,
                // Its transfers cannot be listed, so they read as missing.
                Err(e) => {
                    warn!("{e}");
                    continue;
                }
            };
            let flattened: FlattenedFiles =
                match self.make_request(Method::GET, &endpoint, None::<()>).await {
                    Ok(flattened) => flattened,
                    // slskd answers 404 for a peer it holds no transfers
                    // for. That is an empty result, not a failure.
                    Err(SoulseekError::Api { status: 404, .. }) => continue,
                    Err(e) => return Err(e),
                };
            files.extend(flattened.files);
            errors.extend(flattened.errors);
        }

        Self::usable_files(files, errors)
    }

    /// `GET transfers/downloads/{username}/{id}` (`TransfersController.cs:549-573`). None for 404 and
    /// for a removed record. slskd finds the transfer by id alone; the username only builds the path.
    pub async fn get_download(&self, username: &str, id: &str) -> Result<Option<FileEntry>> {
        match self
            .make_request::<FileEntry, ()>(Method::GET, &download_endpoint(username, id)?, None)
            .await
        {
            Ok(entry) if entry.removed => Ok(None),
            Ok(entry) => Ok(Some(entry)),
            Err(SoulseekError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn cancel_download(
        &self,
        username: &str,
        download_id: &str,
        remove: bool,
    ) -> Result<()> {
        info!("Cancelling download: {}", download_id);
        self.make_request(
            Method::DELETE,
            &transfer_endpoint(username, download_id, remove)?,
            None::<()>,
        )
        .await
    }

    pub async fn delete_search(&self, search_id: &str) -> Result<()> {
        let endpoint = format!("searches/{search_id}");
        debug!("Deleting search {}", search_id);
        match self
            .make_request::<(), ()>(Method::DELETE, &endpoint, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(SoulseekError::Api { status: 404, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `GET transfers/downloads/{username}/{id}/position` (`TransfersController.cs:584-617`):
    /// slskd asks the peer now and answers the bare place. None for 404: no such transfer, or
    /// slskd's blacklist holds the user (`:595-598`, `:610-613`). A failed ask is a 500 whose
    /// body is the exception's text written as a JSON string (`:56`, `:614-617`); the error
    /// keeps that text. A transfer between retries or ended answers such a 500, `... is not
    /// active` (Soulseek.NET `SoulseekClient.cs:1643-1646`).
    ///
    /// Soulseek.NET reads the peer's place as a signed int (`PlaceInQueueResponse.cs:36,50`) and
    /// slskd answers it as it is: a place outside u32, which no peer means, is None, as the
    /// downloads list reads it. A peer slskd found offline is `UserOffline`
    /// (`SoulseekClient.cs:3790`).
    pub async fn get_place_in_queue(&self, username: &str, id: &str) -> Result<Option<u32>> {
        let endpoint = format!("{}/position", download_endpoint(username, id)?);
        match self
            .make_request::<i64, ()>(Method::GET, &endpoint, None)
            .await
        {
            Ok(place) => Ok(u32::try_from(place).ok()),
            Err(SoulseekError::Api { status: 404, .. }) => Ok(None),
            Err(SoulseekError::Api {
                status: 500,
                message,
            }) => {
                let body = json_text(message);
                Err(
                    match body.to_lowercase().contains("appears to be offline") {
                        true => SoulseekError::UserOffline {
                            username: username.to_string(),
                            status: 500,
                            body,
                        },
                        false => SoulseekError::Api {
                            status: 500,
                            message: body,
                        },
                    },
                )
            }
            Err(e) => Err(e),
        }
    }

    /// `GET users/{username}/status` (`Users/API/Controllers/UsersController.cs:253-278`),
    /// `presence` as `UserPresence`'s name (`Program.cs:980`). An offline peer is a 200 saying
    /// `Offline`; 404 means slskd's blacklist holds the user (`:264-267`), so it is `Blocked`.
    pub async fn get_user_presence(&self, username: &str) -> Result<SourcePresence> {
        let endpoint = format!("users/{}/status", path_segment(username)?);
        match self
            .make_request::<UserStatus, ()>(Method::GET, &endpoint, None)
            .await
        {
            Ok(status) => Ok(match status.presence {
                WirePresence::Online => SourcePresence::Online,
                WirePresence::Away => SourcePresence::Away,
                WirePresence::Offline => SourcePresence::Offline,
            }),
            Err(SoulseekError::Api { status: 404, .. }) => Ok(SourcePresence::Blocked),
            // The global handler writes the exception's text as a JSON string (Program.cs:1051-1053).
            Err(SoulseekError::Api {
                status: 500,
                message,
            }) => Err(SoulseekError::Api {
                status: 500,
                message: json_text(message),
            }),
            Err(e) => Err(e),
        }
    }

    /// `PUT searches/{id}` (`Search/API/Controllers/SearchesController.cs:209-228`): slskd
    /// cancels the search's token (`SearchService.cs:414-423`), and the search ends
    /// `Completed, Cancelled` with the responses it had (`SearchService.cs:337-341,359`). 404
    /// (no such search, `:222-225`) is Ok: nothing runs.
    /// slskd is asked to stop the slskd search a search this client polls runs now. The search
    /// sends no retry text from the call on, marked before slskd is asked so that a retry
    /// starting meanwhile sees the mark and stops itself. Once slskd took the stop, the search
    /// is marked cancelled too, so its deadline does not cancel it again and its next polls give
    /// it the grace a cancelled search has to end.
    pub async fn stop_search(&self, search_id: &str) -> Result<()> {
        let running = match self.active_searches.lock().await.get_mut(search_id) {
            Some(context) => {
                context.stop_requested = true;
                context.slskd_id.clone()
            }
            None => search_id.to_string(),
        };
        let endpoint = format!("searches/{}", path_segment(&running)?);
        match self
            .make_request::<(), ()>(Method::PUT, &endpoint, None)
            .await
        {
            Ok(()) | Err(SoulseekError::Api { status: 404, .. }) => {}
            Err(e) => return Err(e),
        }
        if let Some(context) = self.active_searches.lock().await.get_mut(search_id) {
            context.cancelled = true;
        }
        Ok(())
    }

    /// Checks that slskd answers and is connected and logged in to the
    /// Soulseek network; else the failure that says which, with slskd's own
    /// answer as its cause: a rejected API key is `backend_not_configured`,
    /// no answer `backend_unreachable`, and so is a slskd that is not
    /// connected or not logged in.
    pub async fn check_connection(&self) -> std::result::Result<(), Failure> {
        // First verify slskd is reachable (session endpoint)
        if let Err(e) = self
            .make_request::<serde_json::Value, ()>(Method::GET, "session", None)
            .await
        {
            let message = match &e {
                SoulseekError::Api {
                    status: status @ (401 | 403),
                    ..
                } => format!(
                    "slskd rejected the API key (HTTP {status}). Check the API key value, its role, \
                     and any CIDR restriction in slskd's web.authentication.api_keys config."
                ),
                other => format!("Cannot reach slskd (session endpoint failed: {other})"),
            };
            return Err(e.failure(Stage::SourceSearch, message));
        }

        // Then verify Soulseek network connection via application state
        match self
            .make_request::<serde_json::Value, ()>(Method::GET, "application", None)
            .await
        {
            Ok(state) => server_state_failure(&state["server"]).map_or(Ok(()), Err),
            Err(e) => {
                warn!("Failed to check slskd application state: {}", e);
                // If we can reach the session endpoint but not the application endpoint,
                // slskd is reachable but something is off. Return Ok since the basic
                // connectivity is there -- avoids false negatives on older slskd versions.
                Ok(())
            }
        }
    }
}

/// Why `server`, the `server` member of slskd's `GET application`
/// (`isConnected`, `isLoggedIn`), keeps slskd from searching; None when it
/// is connected and logged in, or when the member cannot be read (older
/// slskd versions are given the benefit of the doubt).
fn server_state_failure(server: &serde_json::Value) -> Option<Failure> {
    let flag = |name: &str| server.get(name).and_then(serde_json::Value::as_bool);
    let (connected, logged_in) = (flag("isConnected")?, flag("isLoggedIn")?);
    let failure = |retry, message: &str| {
        Failure::new(
            FailureCode::BackendUnreachable,
            Stage::SourceSearch,
            retry,
            message,
        )
        .with_cause(Service::Slskd, Some(200), server.to_string())
    };
    if !connected {
        return Some(failure(
            RetryAdvice::SameSourceLater,
            "slskd is running but not connected to the Soulseek network. \
             Check slskd's connection settings or restart slskd.",
        ));
    }
    if !logged_in {
        return Some(
            failure(
                RetryAdvice::NeedsUser,
                "slskd is connected to Soulseek but not logged in. \
                 Check slskd's Soulseek username and password.",
            )
            .with_user_action(UserAction::new(
                UserActionCode::ConfigureBackend,
                "Check the Soulseek username and password slskd signs in with",
                "slskd",
            )),
        );
    }
    None
}

/// Everything but unreserved characters, `.` included. Soulseek usernames
/// are free-form, so a peer called `a/b`, `a\b` or `x?y` would otherwise
/// re-point the request at a different route.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'~');

/// slskd's exact answer for a file it already has an unfinished transfer
/// of (`DownloadService.EnqueueAsync`).
const ALREADY_IN_PROGRESS: &str = "Skipped: Already in progress";

/// slskd's 400 to `POST searches`, read from its body: Soulseek.NET's
/// `DuplicateTokenException` says `An active search with token {token} is
/// already in progress`, and every other 400 refuses the request itself, an
/// `ArgumentException` about the query or a field that failed validation
/// (`SearchesController.cs`).
fn search_refused(body: String) -> SoulseekError {
    if body.starts_with("An active search with token ") && body.ends_with(" is already in progress")
    {
        SoulseekError::SearchAlreadyRunning { body }
    } else {
        SoulseekError::SearchQueryRejected { body }
    }
}

/// The text of an error body a `[Produces("application/json")]` controller wrote as a JSON
/// string; the body as sent when it is not one.
fn json_text(body: String) -> String {
    serde_json::from_str(&body).unwrap_or(body)
}

/// slskd's `Users/Types/Status.cs` as `GET users/{username}/status` writes it.
#[derive(Deserialize)]
struct UserStatus {
    presence: WirePresence,
}

/// Soulseek.NET's `UserPresence` by name (`src/UserPresence.cs:29-45`).
#[derive(Deserialize)]
enum WirePresence {
    Online,
    Away,
    Offline,
}

/// An error polling a search: reading its status or responses, or cancelling
/// it. slskd answers 404, with an empty body, only when it has no search with
/// that id (`SearchesController.GetById`, `GetResponsesById`, `Cancel`);
/// every other error is kept.
fn poll_refused(e: SoulseekError) -> SoulseekError {
    match e {
        SoulseekError::Api {
            status: 404,
            message,
        } => SoulseekError::SearchGone { body: message },
        e => e,
    }
}

/// `value` as one path segment. URL parsing resolves `.` and `..` as dot
/// segments even when percent-encoded, so those cannot be sent at all.
fn path_segment(value: &str) -> Result<String> {
    if matches!(value, "" | "." | "..") {
        return Err(SoulseekError::UnaddressablePeer {
            username: value.to_string(),
        });
    }
    Ok(utf8_percent_encode(value, PATH_SEGMENT).to_string())
}

/// slskd's `transfers.download.retry.attempts` from its `GET options`
/// answer (`Core/Options.cs:264,913,980,997`, camelCase per
/// `Program.cs:977-982`), at least 1 (`[Range(1, int.MaxValue)]`, `:996`).
fn retry_limit_from_options(options: &serde_json::Value) -> Result<u32> {
    options["transfers"]["download"]["retry"]["attempts"]
        .as_u64()
        .and_then(|attempts| u32::try_from(attempts).ok())
        .filter(|&attempts| attempts > 0)
        .ok_or_else(|| {
            SoulseekError::InvalidResponse(
                "slskd options without transfers.download.retry.attempts".into(),
            )
        })
}

fn user_downloads_endpoint(username: &str) -> Result<String> {
    Ok(format!("transfers/downloads/{}", path_segment(username)?))
}

fn download_endpoint(username: &str, id: &str) -> Result<String> {
    Ok(format!(
        "{}/{}",
        user_downloads_endpoint(username)?,
        path_segment(id)?
    ))
}

fn transfer_endpoint(username: &str, id: &str, remove: bool) -> Result<String> {
    Ok(format!(
        "{}?remove={remove}",
        download_endpoint(username, id)?
    ))
}

/// One POST to slskd's batch endpoint. The id is fixed when the batch is
/// planned, so every retry of it sends the same one.
struct PendingBatch {
    id: String,
    username: String,
    destination: String,
    files: Vec<DownloadRequestFile>,
    /// Position among this user's batches; the first is sent without delay.
    index: usize,
}

impl PendingBatch {
    fn request(&self) -> BatchEnqueueRequest<'_> {
        BatchEnqueueRequest {
            id: &self.id,
            username: &self.username,
            files: &self.files,
            options: BatchEnqueueOptions {
                destination: &self.destination,
            },
        }
    }

    /// The same answer for every file of the batch.
    fn responses(&self, status: EnqueueStatus) -> Vec<DownloadResponse> {
        self.files
            .iter()
            .map(|f| DownloadResponse {
                username: self.username.clone(),
                filename: f.filename.clone(),
                size: f.size as u64,
                transfer_id: None,
                status: status.clone(),
            })
            .collect()
    }
}

/// Split one call into slskd batches: one user per batch, at most
/// `batch_size` files each, all into `destination`. Users keep the order of
/// their first file, and a file asked for twice is sent once. The batches are
/// numbered in order as batches of `request`.
fn plan_batches(
    wanted: Vec<(String, DownloadRequestFile)>,
    destination: &str,
    batch_size: usize,
    request: &str,
) -> Vec<PendingBatch> {
    let mut per_user: Vec<(String, Vec<DownloadRequestFile>)> = Vec::new();
    for (username, file) in wanted {
        let at = match per_user.iter().position(|(u, _)| *u == username) {
            Some(at) => at,
            None => {
                per_user.push((username, Vec::new()));
                per_user.len() - 1
            }
        };
        let files = &mut per_user[at].1;
        if !files.iter().any(|f| f.filename == file.filename) {
            files.push(file);
        }
    }

    per_user
        .into_iter()
        .flat_map(|(username, files)| {
            files
                .chunks(batch_size.max(1))
                .enumerate()
                .map(|(index, chunk)| (username.clone(), index, chunk.to_vec()))
                .collect::<Vec<_>>()
        })
        .zip(0..)
        .map(|((username, index, files), n)| PendingBatch {
            id: shared::slskd::request_batch_id(request, n),
            username,
            destination: destination.to_string(),
            files,
            index,
        })
        .collect()
}

/// Send `batch` through `send`, retrying retryable errors with exponential
/// backoff. Every attempt sends the same batch, so the same client id.
async fn retry_batch<'b, F, Fut>(
    config: &DownloadConfig,
    batch: &'b PendingBatch,
    mut send: F,
) -> Vec<DownloadResponse>
where
    F: FnMut(&'b PendingBatch) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<DownloadResponse>>>,
{
    // Cap exponential backoff at 30 seconds to prevent excessive waits
    const MAX_BACKOFF_MS: u64 = 30_000;

    let mut attempt = 0;
    let last_error = loop {
        if attempt > 0 {
            let delay = std::cmp::min(
                config.retry_base_delay_ms * (1 << (attempt - 1)),
                MAX_BACKOFF_MS,
            );
            warn!(
                "Retrying batch {} for '{}' (attempt {}/{}), waiting {}ms",
                batch.index, batch.username, attempt, config.max_retries, delay
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }

        let e = match send(batch).await {
            Ok(responses) => return responses,
            Err(e) => e,
        };
        warn!(
            "Batch {} for '{}' attempt {} failed: {}",
            batch.index, batch.username, attempt, e
        );
        // Stop retrying for non-retryable errors (e.g. user offline)
        if !e.is_retryable() {
            warn!(
                "Batch {} for '{}': error is non-retryable, stopping",
                batch.index, batch.username
            );
            let failure = e.failure(Stage::Enqueue, e.to_string());
            return batch.responses(EnqueueStatus::Failed(failure));
        }
        if attempt == config.max_retries {
            break e;
        }
        attempt += 1;
    };

    let error_msg = format!(
        "Failed after {} retries: {}",
        config.max_retries, last_error
    );

    warn!(
        "Batch {} for '{}' failed after all retries: {}",
        batch.index, batch.username, error_msg
    );

    batch.responses(EnqueueStatus::Failed(
        last_error.failure(Stage::Enqueue, error_msg),
    ))
}

/// Answers for a batch slskd already holds (409 on a retried POST): its own
/// transfers, found by batch id and exact filename, are enqueued. A file the
/// first attempt did not enqueue cannot be told apart from one that failed,
/// so it is reported failed.
fn replayed_batch_responses(batch: &PendingBatch, listing: &[FileEntry]) -> Vec<DownloadResponse> {
    batch
        .files
        .iter()
        .map(|file| {
            let transfer = listing.iter().find(|t| {
                t.filename == file.filename
                    && t.batch_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&batch.id))
            });
            DownloadResponse {
                username: batch.username.clone(),
                filename: file.filename.clone(),
                size: file.size as u64,
                transfer_id: transfer.map(|t| t.id.clone()),
                status: match transfer {
                    Some(_) => EnqueueStatus::Enqueued,
                    None => EnqueueStatus::Failed(Failure::new(
                        FailureCode::EnqueueFailed,
                        Stage::Enqueue,
                        RetryAdvice::SameSourceLater,
                        "Not found in slskd after a retried enqueue",
                    )),
                },
            }
        })
        .collect()
}

/// Map slskd's batch enqueue response (201 all enqueued, 207 partial,
/// 200 all failed) onto per-file results for this batch.
fn parse_batch_enqueue_response(
    username: &str,
    batch: &[DownloadRequestFile],
    resp_text: &str,
) -> Result<Vec<DownloadResponse>> {
    let parsed: BatchEnqueueResponse = serde_json::from_str(resp_text).map_err(|e| {
        SoulseekError::InvalidResponse(format!(
            "batch enqueue response: {e} (body {} bytes)",
            resp_text.len()
        ))
    })?;

    Ok(batch
        .iter()
        .map(|file| {
            let transfer = parsed
                .batch
                .transfers
                .iter()
                .find(|t| t.filename == file.filename);
            let failure = parsed.failures.iter().find(|f| f.filename == file.filename);

            let status = match (transfer, failure) {
                (Some(_), _) => EnqueueStatus::Enqueued,
                (None, Some(f)) if f.message == ALREADY_IN_PROGRESS => {
                    EnqueueStatus::AlreadyInFlight(Cause::new(
                        Service::Slskd,
                        None,
                        f.message.clone(),
                    ))
                }
                (None, Some(f)) => EnqueueStatus::Failed(
                    Failure::new(
                        FailureCode::EnqueueFailed,
                        Stage::Enqueue,
                        RetryAdvice::OtherSource,
                        f.message.clone(),
                    )
                    .with_cause(Service::Slskd, None, f.message.clone()),
                ),
                (None, None) => EnqueueStatus::Failed(Failure::new(
                    FailureCode::EnqueueFailed,
                    Stage::Enqueue,
                    RetryAdvice::OtherSource,
                    "Not reported in slskd enqueue response",
                )),
            };

            DownloadResponse {
                username: username.to_string(),
                filename: file.filename.clone(),
                size: file.size as u64,
                transfer_id: transfer.map(|t| t.id.clone()),
                status,
            }
        })
        .collect())
}

#[async_trait::async_trait]
impl crate::DownloadBackend for SoulseekClient {
    fn id(&self) -> &'static str {
        shared::backends::SLSKD.id
    }

    fn name(&self) -> &'static str {
        shared::backends::SLSKD.name
    }

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
        let timeout = Duration::seconds(120);
        Ok(self.start_search(spec.clone(), timeout).await?)
    }

    async fn poll_search(&self, search_id: &str) -> BackendResult<shared::download::SearchResult> {
        let poll = self.poll_search(search_id.to_string()).await?;
        Ok(shared::download::SearchResult {
            search_id: search_id.to_string(),
            groups: poll.albums.into_iter().map(Into::into).collect(),
            has_more: poll.state == SearchState::InProgress,
            state: poll.state.into(),
            stats: poll.stats,
        })
    }

    fn new_request(&self) -> String {
        shared::slskd::request_batch_id(&uuid::Uuid::new_v4().to_string(), 0)
    }

    async fn download(
        &self,
        items: Vec<shared::download::DownloadableItem>,
        destination: &str,
        request: &str,
        _owner: &str,
    ) -> BackendResult<Vec<shared::download::QueuedDownload>> {
        let tracks: Vec<TrackResult> = items
            .into_iter()
            .filter_map(|item| item.to_slskd_track())
            .collect();

        let responses = self.download(tracks, destination, request).await?;
        Ok(responses.into_iter().map(Into::into).collect())
    }

    async fn get_downloads_for(
        &self,
        sources: &[String],
    ) -> BackendResult<Vec<shared::download::DownloadProgress>> {
        let limit = self.retry_limit().await?;
        let entries = self.get_downloads_for_users(sources).await?;
        Ok(entries
            .into_iter()
            .map(|e| e.into_progress(limit))
            .collect())
    }

    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> BackendResult<Option<shared::download::DownloadProgress>> {
        let limit = self.retry_limit().await?;
        Ok(self
            .get_download(source, id)
            .await?
            .map(|e| e.into_progress(limit)))
    }

    async fn cancel_download(
        &self,
        source: &str,
        download_id: &str,
        remove: bool,
    ) -> BackendResult<()> {
        Ok(self.cancel_download(source, download_id, remove).await?)
    }

    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>> {
        Ok(self.get_place_in_queue(source, id).await?)
    }

    async fn source_status(&self, source: &str) -> BackendResult<SourcePresence> {
        Ok(self.get_user_presence(source).await?)
    }

    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
        Ok(self.stop_search(search_id).await?)
    }

    async fn health_check(&self) -> std::result::Result<(), Failure> {
        self.check_connection().await.inspect_err(|failure| {
            warn!("Health check failed: {}", failure.message);
        })
    }
}

/// slskd's search inactivity timeout, in milliseconds although slskd's DTO
/// comment says seconds: `SearchRequest.cs:114` passes it unconverted to
/// Soulseek.NET's `SearchOptions.SearchTimeout`, which is a millisecond count
/// (`src/Options/SearchOptions.cs:36,53`). The search ends this long after
/// the last response.
const SEARCH_INACTIVITY_MS: u32 = 15_000;

/// The artist and the title a search names: one track names itself, an album
/// names the album, and a search without an album needs exactly one track.
fn search_parts<'a>(album: Option<&'a Album>, tracks: &'a [Track]) -> Result<[&'a str; 2]> {
    match (album, tracks) {
        (Some(album), [track]) => Ok([album.artist.trim(), track.title.trim()]),
        (Some(album), _) => Ok([album.artist.trim(), album.title.trim()]),
        (None, [track]) => Ok([track.artist.trim(), track.title.trim()]),
        (None, _) => Err(SoulseekError::NoSearchTarget {
            tracks: tracks.len(),
        }),
    }
}

/// What a search no peer answered says of itself, `retried` when retry texts were sent too.
fn unanswered_note(retried: bool) -> String {
    let also = match retried {
        true => ", also with wildcards in place of some words",
        false => "",
    };
    format!(
        "No peer answered{also}. The Soulseek server silently drops searches that contain a \
         phrase on its exclusion list (often a well known artist name or title), so it may be \
         blocking this one."
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequest<'a> {
    search_text: &'a str,
    search_timeout: u32,
    filter_responses: bool,
    minimum_peer_upload_speed: u32,
}

impl<'a> SearchRequest<'a> {
    fn new(search_text: &'a str) -> Self {
        Self {
            search_text,
            search_timeout: SEARCH_INACTIVITY_MS,
            filter_responses: true,
            minimum_peer_upload_speed: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply, Seen};
    use shared::download::EnqueueStatus;
    use std::sync::Mutex as StdMutex;

    fn track(artist: &str, title: &str) -> Track {
        Track {
            id: String::new(),
            title: title.into(),
            artist: artist.into(),
            album_id: None,
            album_title: None,
            release_date: None,
            duration: None,
            mbid: None,
            release_mbid: None,
        }
    }

    fn album(artist: &str, title: &str) -> Album {
        Album {
            id: String::new(),
            title: title.into(),
            artist: artist.into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
            first_release_date: None,
        }
    }

    /// slskd's `GET application` with its server in `state` (a
    /// `SoulseekClientStates` flags value, `Core/API/DTO/ServerState.cs`).
    fn application(state: &str, connected: bool, logged_in: bool) -> String {
        format!(
            r#"{{"server":{{"address":"vps.slsknet.org:2271","ipEndPoint":null,"state":"{state}","username":"marrow","isConnected":{connected},"isLoggedIn":{logged_in},"isTransitioning":false}}}}"#
        )
    }

    /// A client of the slskd at `url`.
    fn client_at(url: &str) -> SoulseekClient {
        SoulseekClientBuilder::new()
            .base_url(url)
            .api_key("k")
            .build()
            .unwrap()
    }

    #[test]
    fn the_client_goes_by_its_backend_meta() {
        use crate::DownloadBackend;
        let client = client_at("http://127.0.0.1:1");
        let meta = shared::backends::meta("slskd").unwrap();
        assert_eq!((client.id(), client.name()), (meta.id, meta.name));
        assert_eq!(client.id(), shared::slskd::BACKEND_ID);
    }

    #[tokio::test]
    async fn a_batch_that_got_no_answer_does_not_name_its_url() {
        // Port 1 on loopback refuses the connection at once.
        let url = "http://127.0.0.1:1";
        let file = DownloadRequestFile {
            filename: "Kowloon\\Come Over\\07 - Wake Up.flac".into(),
            size: 27_213_250,
        };
        let batches = plan_batches(vec![("carol_shares".into(), file)], "Come Over", 50, "r1");
        let e = (client_at(url).send_download_batch(&batches[0]).await).unwrap_err();
        let text = e.to_string();
        assert!(
            !text.contains("127.0.0.1") && !text.contains(" for url "),
            "{text}"
        );
        // reqwest's text for a refused connection, without " for url (...)".
        assert_eq!(text, "Request error: error sending request");
    }

    async fn checked(replies: Vec<Reply>) -> std::result::Result<(), Failure> {
        let (url, _) = serve(replies);
        SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap()
            .check_connection()
            .await
    }

    #[tokio::test]
    async fn a_health_check_says_why_slskd_cannot_take_work() {
        assert_eq!(
            checked(vec![
                ok(""),
                ok(&application("Connected, LoggedIn", true, true))
            ])
            .await,
            Ok(())
        );

        let rejected = checked(vec![Reply {
            status: 401,
            body: String::new(),
        }])
        .await
        .unwrap_err();
        assert_eq!(
            (
                rejected.code,
                rejected.retry,
                rejected.cause.as_ref().map(|c| c.status)
            ),
            (
                FailureCode::BackendNotConfigured,
                RetryAdvice::NeedsUser,
                Some(Some(401))
            )
        );
        assert!(rejected
            .message
            .starts_with("slskd rejected the API key (HTTP 401)."));
        assert!(rejected.user_action.is_some());

        let down = checked(vec![Reply {
            status: 503,
            body: "Service Unavailable".into(),
        }])
        .await
        .unwrap_err();
        assert_eq!(down.code, FailureCode::BackendUnreachable);
        assert!(down
            .message
            .starts_with("Cannot reach slskd (session endpoint failed: "));
        assert_eq!(down.cause.unwrap().raw, "Service Unavailable");

        let offline = application("Disconnected", false, false);
        let not_connected = checked(vec![ok(""), ok(&offline)]).await.unwrap_err();
        assert_eq!(
            (not_connected.code, not_connected.retry),
            (
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater
            )
        );
        assert!(not_connected
            .message
            .starts_with("slskd is running but not connected"));
        let raw = not_connected.cause.unwrap().raw;
        assert!(raw.contains(r#""isConnected":false"#), "{raw}");

        let not_logged_in = checked(vec![ok(""), ok(&application("Connected", true, false))])
            .await
            .unwrap_err();
        assert_eq!(
            (not_logged_in.code, not_logged_in.retry),
            (FailureCode::BackendUnreachable, RetryAdvice::NeedsUser)
        );
        assert!(not_logged_in
            .message
            .starts_with("slskd is connected to Soulseek but not logged in."));
        assert_eq!(
            not_logged_in.user_action.map(|a| a.code),
            Some(UserActionCode::ConfigureBackend)
        );
    }

    #[test]
    fn the_search_text_names_one_track_or_the_album() {
        let night = track("Marrow Choir", " O Holy Night ");
        let silent = track("Marrow Choir", "Silent Night");
        let winter = album("Marrow Choir ", "Winter Songs");
        assert_eq!(
            search_parts(None, std::slice::from_ref(&night)).unwrap(),
            ["Marrow Choir", "O Holy Night"]
        );
        assert_eq!(
            search_parts(Some(&winter), std::slice::from_ref(&night)).unwrap(),
            ["Marrow Choir", "O Holy Night"]
        );
        assert_eq!(
            search_parts(Some(&winter), &[night, silent]).unwrap(),
            ["Marrow Choir", "Winter Songs"]
        );
        assert_eq!(
            search_parts(Some(&winter), &[]).unwrap(),
            ["Marrow Choir", "Winter Songs"]
        );
    }

    #[test]
    fn a_search_without_an_album_takes_exactly_one_track() {
        let two = vec![
            track("Marrow Choir", "O Holy Night"),
            track("Marrow Choir", "Silent Night"),
        ];
        for tracks in [Vec::new(), two] {
            let e = search_parts(None, &tracks).unwrap_err();
            assert!(
                matches!(e, SoulseekError::NoSearchTarget { tracks: n } if n == tracks.len()),
                "{e:?}"
            );
            assert!(!e.is_retryable());
            let f = e.failure(Stage::SourceSearch, e.to_string());
            assert_eq!(
                (f.code, f.retry),
                (FailureCode::InvalidInput, RetryAdvice::NotRetryable)
            );
        }
        assert_eq!(
            SoulseekError::NoSearchTarget { tracks: 2 }.to_string(),
            "A search without an album takes exactly one track, not 2"
        );
    }

    #[test]
    fn the_search_request_sends_slskds_inactivity_timeout_in_milliseconds() {
        assert_eq!(
            serde_json::to_value(SearchRequest::new("Marrow Choir O Holy Night")).unwrap(),
            serde_json::json!({"searchText": "Marrow Choir O Holy Night", "searchTimeout": 15000,
                               "filterResponses": true, "minimumPeerUploadSpeed": 10})
        );
    }

    /// `status` is Failed with this code and message.
    fn failed_with(status: &EnqueueStatus, code: FailureCode, message: &str) -> bool {
        matches!(status, EnqueueStatus::Failed(f) if f.code == code && f.message == message)
    }

    fn batch(files: &[&str]) -> Vec<DownloadRequestFile> {
        files
            .iter()
            .map(|f| DownloadRequestFile {
                filename: f.to_string(),
                size: 1000,
            })
            .collect()
    }

    /// Response shape as slskd 0.26 emits it: batch record with the enqueued
    /// `(filename, transfer id)` pairs attached, failures as real
    /// {filename, message} records.
    fn body(transfers: &[(&str, &str)], failures: &[(&str, &str)]) -> String {
        let transfers: Vec<_> = transfers
            .iter()
            .map(|(f, id)| serde_json::json!({ "id": id, "filename": f }))
            .collect();
        let failures: Vec<_> = failures
            .iter()
            .map(|(f, m)| serde_json::json!({ "filename": f, "message": m }))
            .collect();
        serde_json::json!({
            "batch": {
                "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "username": "peer",
                "direction": "Download",
                "createdAt": "2026-07-26T00:00:00Z",
                "transfers": transfers,
                "options": {}
            },
            "failures": failures
        })
        .to_string()
    }

    #[test]
    fn all_enqueued() {
        let files = batch(&[r"a\album\1.mp3", r"a\album\2.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(&[(r"a\album\1.mp3", "t1"), (r"a\album\2.mp3", "t2")], &[]),
        )
        .unwrap();
        assert!(results.iter().all(|r| r.status == EnqueueStatus::Enqueued));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn partial_failure_attributes_the_right_file() {
        let files = batch(&[r"a\album\1.mp3", r"a\album\2.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(
                &[(r"a\album\1.mp3", "t1")],
                &[(r"a\album\2.mp3", "Error: something broke")],
            ),
        )
        .unwrap();
        assert_eq!(results[0].status, EnqueueStatus::Enqueued);
        assert_eq!(results[1].filename, r"a\album\2.mp3");
        assert!(failed_with(
            &results[1].status,
            FailureCode::EnqueueFailed,
            "Error: something broke"
        ));
        assert_eq!(results[1].transfer_id, None);
        let EnqueueStatus::Failed(f) = &results[1].status else {
            unreachable!()
        };
        assert_eq!(
            (f.stage, f.retry),
            (Stage::Enqueue, RetryAdvice::OtherSource)
        );
        let cause = f.cause.as_ref().unwrap();
        assert_eq!(
            (cause.service, cause.status, cause.raw.as_str()),
            (Service::Slskd, None, "Error: something broke")
        );
    }

    #[test]
    fn all_failed() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(&[], &[(r"a\album\1.mp3", "Error: rejected")]),
        )
        .unwrap();
        assert!(failed_with(
            &results[0].status,
            FailureCode::EnqueueFailed,
            "Error: rejected"
        ));
    }

    #[test]
    fn already_in_progress_is_already_in_flight() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(&[], &[(r"a\album\1.mp3", "Skipped: Already in progress")]),
        )
        .unwrap();
        assert_eq!(
            results[0].status,
            EnqueueStatus::AlreadyInFlight(Cause::new(
                Service::Slskd,
                None,
                "Skipped: Already in progress"
            ))
        );
        assert_eq!(results[0].transfer_id, None);
    }

    #[test]
    fn a_near_miss_message_is_a_failure() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(&[], &[(r"a\album\1.mp3", "Already in progress")]),
        )
        .unwrap();
        assert!(failed_with(
            &results[0].status,
            FailureCode::EnqueueFailed,
            "Already in progress"
        ));
    }

    #[test]
    fn enqueued_transfer_carries_its_id() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results =
            parse_batch_enqueue_response("peer", &files, &body(&[(r"a\album\1.mp3", "abc")], &[]))
                .unwrap();
        assert_eq!(results[0].status, EnqueueStatus::Enqueued);
        assert_eq!(results[0].transfer_id.as_deref(), Some("abc"));
    }

    #[test]
    fn file_missing_from_response_is_an_error() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response("peer", &files, &body(&[], &[])).unwrap();
        assert!(failed_with(
            &results[0].status,
            FailureCode::EnqueueFailed,
            "Not reported in slskd enqueue response"
        ));
    }

    #[test]
    fn omitted_transfers_key_parses() {
        // WhenWritingNull: slskd omits null fields entirely.
        let files = batch(&[r"a\album\1.mp3"]);
        let body = serde_json::json!({
            "batch": { "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" },
            "failures": [{ "filename": r"a\album\1.mp3", "message": "Error: x" }]
        })
        .to_string();
        let results = parse_batch_enqueue_response("peer", &files, &body).unwrap();
        assert!(failed_with(
            &results[0].status,
            FailureCode::EnqueueFailed,
            "Error: x"
        ));
    }

    #[test]
    fn unparseable_body_is_invalid_response() {
        let files = batch(&[r"a\album\1.mp3"]);
        let err = parse_batch_enqueue_response("peer", &files, "<html>gateway</html>").unwrap_err();
        assert!(matches!(err, SoulseekError::InvalidResponse(_)));
    }

    #[test]
    fn transfer_endpoint_encodes_the_username() {
        assert_eq!(
            transfer_endpoint("a/b c", "id1", true).unwrap(),
            "transfers/downloads/a%2Fb%20c/id1?remove=true"
        );
        assert_eq!(
            user_downloads_endpoint("a/b c").unwrap(),
            "transfers/downloads/a%2Fb%20c"
        );

        // What the request actually hits once the URL is parsed.
        let base = Url::parse("http://slskd:5030").unwrap();
        let path = |endpoint: String| {
            base.join(&format!("api/v0/{endpoint}"))
                .unwrap()
                .path()
                .to_string()
        };
        assert_eq!(
            path(transfer_endpoint("a\\b", "id1", false).unwrap()),
            "/api/v0/transfers/downloads/a%5Cb/id1"
        );
        assert_eq!(
            path(user_downloads_endpoint("..a.").unwrap()),
            "/api/v0/transfers/downloads/%2E%2Ea%2E"
        );
        for name in [".", "..", ""] {
            assert!(
                matches!(
                    user_downloads_endpoint(name),
                    Err(SoulseekError::UnaddressablePeer { .. })
                ),
                "{name:?}"
            );
            assert!(transfer_endpoint(name, "id1", false).is_err(), "{name:?}");
        }
        assert!(transfer_endpoint("peer", "..", false).is_err());
    }

    /// A request as `new_request` draws one.
    const REQUEST: &str = "5b1d3c2a-9e4f-4a7b-8c6d-0f1e00000000";

    #[test]
    fn every_slskd_batch_of_a_request_has_its_own_id_naming_the_request() {
        let batches = plan_batches(
            vec![
                wanted("peer", r"music\A\01.flac"),
                wanted("other", r"music\A\02.flac"),
                wanted("peer", r"music\A\03.flac"),
            ],
            "A [1]",
            1,
            REQUEST,
        );
        assert_eq!(
            batches
                .iter()
                .map(|b| (b.username.as_str(), b.index, b.id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("peer", 0, "5b1d3c2a-9e4f-4a7b-8c6d-0f1e00000000"),
                ("peer", 1, "5b1d3c2a-9e4f-4a7b-8c6d-0f1e00000001"),
                ("other", 0, "5b1d3c2a-9e4f-4a7b-8c6d-0f1e00000002"),
            ]
        );
        let drawn = crate::DownloadBackend::new_request(
            &SoulseekClientBuilder::from_settings(Some("http://slskd".into()), Some("k".into()))
                .unwrap()
                .build()
                .unwrap(),
        );
        assert!(drawn.ends_with("00000000") && drawn.len() == 36, "{drawn}");
    }

    fn wanted(username: &str, filename: &str) -> (String, DownloadRequestFile) {
        (
            username.to_string(),
            DownloadRequestFile {
                filename: filename.to_string(),
                size: 1000,
            },
        )
    }

    #[test]
    fn every_file_of_a_call_shares_the_destination() {
        let batches = plan_batches(
            vec![
                wanted("peer", r"music\Album A\01.flac"),
                wanted("peer", r"other\Album B\02.flac"),
            ],
            "Album A [12345678]",
            3,
            REQUEST,
        );

        assert_eq!(
            batches.len(),
            1,
            "one batch per user, not per remote folder"
        );
        let request = serde_json::to_value(batches[0].request()).unwrap();
        assert_eq!(request["username"], "peer");
        assert_eq!(request["files"].as_array().unwrap().len(), 2);
        assert_eq!(request["options"]["destination"], "Album A [12345678]");
    }

    #[tokio::test]
    async fn a_retried_batch_reuses_its_client_id() {
        let batches = plan_batches(
            vec![wanted("peer", r"music\A\01.flac")],
            "A [1]",
            3,
            REQUEST,
        );
        let config = DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 0,
            max_retries: 3,
            retry_base_delay_ms: 0,
        };
        let mut sent: Vec<serde_json::Value> = Vec::new();

        let results = retry_batch(&config, &batches[0], |b| {
            sent.push(serde_json::to_value(b.request()).unwrap());
            let first = sent.len() == 1;
            async move {
                if first {
                    Err(SoulseekError::Api {
                        status: 502,
                        message: "bad gateway".into(),
                    })
                } else {
                    Ok(vec![])
                }
            }
        })
        .await;

        assert!(results.is_empty());
        assert_eq!(sent.len(), 2, "the transport error was retried");
        assert!(sent[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(sent[0]["id"], sent[1]["id"]);
    }

    #[tokio::test]
    async fn an_offline_peer_fails_its_batch_at_once_with_peer_offline() {
        let batches = plan_batches(
            vec![wanted("peer", r"music\A\01.flac")],
            "A [1]",
            3,
            REQUEST,
        );
        let config = DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 0,
            max_retries: 3,
            retry_base_delay_ms: 0,
        };
        let mut sent = 0;
        let results = retry_batch(&config, &batches[0], |_| {
            sent += 1;
            async {
                Err(SoulseekError::UserOffline {
                    username: "peer".into(),
                    status: 404,
                    body: "User peer appears to be offline".into(),
                })
            }
        })
        .await;
        assert_eq!(sent, 1);
        assert!(failed_with(
            &results[0].status,
            FailureCode::PeerOffline,
            "User 'peer' is offline"
        ));
        let EnqueueStatus::Failed(f) = &results[0].status else {
            panic!("not a failure");
        };
        let cause = f.cause.as_ref().unwrap();
        assert_eq!(
            (cause.status, cause.raw.as_str()),
            (Some(404), "User peer appears to be offline")
        );
    }

    /// slskd's 400 bodies to `POST searches` (`SearchesController.cs`,
    /// Soulseek.NET 10.0.2 `SearchAsync`).
    #[test]
    fn a_refused_search_is_told_apart_by_slskds_answer() {
        for body in [
            "Search query must contain at least one non-exclusion term (Parameter 'query')",
            "Search query must contain at least one non-exclusion term with length greater than 1 (Parameter 'query')",
            "Search text must not be a null or empty string, or one consisting only of whitespace (Parameter 'query')",
            "The field SearchText can not be null, empty, or consist of only whitespace",
        ] {
            assert!(
                matches!(search_refused(body.into()), SoulseekError::SearchQueryRejected { body: b } if b == body),
                "{body}"
            );
        }
        let duplicate = "An active search with token 1234 is already in progress";
        assert!(matches!(
            search_refused(duplicate.into()),
            SoulseekError::SearchAlreadyRunning { body } if body == duplicate
        ));
    }

    /// slskd's 404 to a poll is a search it no longer has; other answers
    /// stay as they were.
    #[test]
    fn a_poll_slskd_answers_404_is_a_search_it_no_longer_has() {
        let gone = poll_refused(SoulseekError::Api {
            status: 404,
            message: String::new(),
        });
        assert!(matches!(gone, SoulseekError::SearchGone { body } if body.is_empty()));
        let unavailable = poll_refused(SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".into(),
        });
        assert!(matches!(
            unavailable,
            SoulseekError::Api { status: 503, message } if message.starts_with("Circuit breaker is open")
        ));
    }

    /// A search this client is not running is an error, never an empty
    /// result: nothing answered the query.
    #[tokio::test]
    async fn polling_a_search_this_client_does_not_run_is_an_error() {
        let client = SoulseekClientBuilder::new()
            .base_url("http://127.0.0.1:9")
            .build()
            .unwrap();
        let polled = client.poll_search("gone".to_string()).await;
        assert!(matches!(
            polled,
            Err(SoulseekError::UnknownSearch { search_id }) if search_id == "gone"
        ));
    }

    const SEARCH_ID: &str = "3f2b9c1e-7d4a-4b8e-9f0a-1c2d3e4f5a6b";

    /// slskd's search record (`Search/Types/Search.cs:44-63`, nulls omitted
    /// as `Program.cs:981` configures). `Completed, TimedOut` is
    /// Soulseek.NET's `Completed | TimedOut`, set when the inactivity timer
    /// fires (`src/SearchInternal.cs:63,145`).
    fn status(complete: bool, responses: u32) -> Reply {
        let state = if complete {
            "Completed, TimedOut"
        } else {
            "InProgress"
        };
        status_in(state, responses)
    }

    /// slskd's search record in `state`, which `isComplete` follows (`Search.cs:52`).
    fn status_in(state: &str, responses: u32) -> Reply {
        Reply {
            status: 200,
            body: serde_json::json!({
                "fileCount": responses, "id": SEARCH_ID, "isComplete": state.starts_with("Completed"),
                "lockedFileCount": 0, "responseCount": responses, "responses": [],
                "searchText": "Marrow Choir O Holy Night", "startedAt": "2026-11-15T10:00:00.0000000Z",
                "state": state, "token": 17 })
            .to_string(),
        }
    }

    /// One peer's response (`Response.cs:37-46`, `File.cs:37-46`).
    fn one_response() -> Reply {
        Reply {
            status: 200,
            body: serde_json::json!([{
                "fileCount": 1, "hasFreeUploadSlot": true, "lockedFileCount": 0, "lockedFiles": [],
                "queueLength": 0, "token": 17, "uploadSpeed": 1048576, "username": "carol_shares",
                "files": [{"bitDepth": 16, "code": 1, "extension": "flac", "isLocked": false, "length": 263,
                           "filename": "@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac",
                           "sampleRate": 44100, "size": 28734112}]}])
            .to_string(),
        }
    }

    #[test]
    fn the_source_with_the_wanted_length_scores_higher() {
        let mut responses: Vec<SearchResponse> =
            serde_json::from_str(&one_response().body).unwrap();
        let other = one_response()
            .body
            .replace("carol_shares", "dave_shares")
            .replace(r#""length":263"#, r#""length":323"#);
        assert!(other.contains(r#""length":323"#));
        responses.extend(serde_json::from_str::<Vec<SearchResponse>>(&other).unwrap());
        let albums = processing::process_search_responses(
            &responses,
            "Marrow Choir",
            Some("Winter Songs"),
            &[WantedTrack {
                title: "O Holy Night".into(),
                duration_s: Some(263),
            }],
            false,
        )
        .albums;
        let score = |user: &str| albums.iter().find(|a| a.username == user).unwrap().score;
        assert!((score("carol_shares") - score("dave_shares") - 0.3 * 3.0 / 13.0).abs() < 1e-9);
        assert!(score("dave_shares") > 0.7);
    }

    #[test]
    fn a_peer_with_a_huge_queue_length_does_not_break_the_search() {
        let body = one_response()
            .body
            .replace(r#""queueLength":0"#, r#""queueLength":4294967295"#);
        assert!(body.contains("4294967295"));
        let responses: Vec<SearchResponse> = serde_json::from_str(&body).unwrap();
        let albums = processing::process_search_responses(
            &responses,
            "Marrow Choir",
            None,
            &[WantedTrack {
                title: "O Holy Night".into(),
                duration_s: None,
            }],
            false,
        )
        .albums;
        assert_eq!(albums[0].queue_length, 4_294_967_295);
        assert_eq!(albums[0].tracks[0].base.queue_length, 4_294_967_295);
    }

    /// carol_shares as in one_response(), and erin_shares with a file that matches nothing
    /// wanted, a cover image and two locked files.
    fn mixed_responses() -> String {
        let carol: serde_json::Value = serde_json::from_str(&one_response().body).unwrap();
        let erin = serde_json::json!({
            "fileCount": 2, "hasFreeUploadSlot": false, "lockedFileCount": 2,
            "lockedFiles": [{"code": 1, "extension": "flac", "filename": "@@pq7\\Private\\a.flac", "size": 1},
                            {"code": 1, "extension": "flac", "filename": "@@pq7\\Private\\b.flac", "size": 1}],
            "queueLength": 12, "token": 17, "uploadSpeed": 52000, "username": "erin_shares",
            "files": [{"bitRate": 192, "code": 1, "extension": "", "isVariableBitRate": true, "length": 181,
                       "filename": "@@pq7\\Share\\Misc\\Jingle Bells Remix.mp3", "size": 4345000},
                      {"code": 1, "extension": "jpg", "filename": "@@pq7\\Share\\Misc\\cover.jpg", "size": 51200}]});
        serde_json::json!([carol[0], erin]).to_string()
    }

    #[test]
    fn a_file_below_the_floor_is_kept_and_flagged_only_when_asked() {
        let responses: Vec<SearchResponse> = serde_json::from_str(&mixed_responses()).unwrap();
        let wanted = [WantedTrack {
            title: "O Holy Night".into(),
            duration_s: Some(263),
        }];
        let strict =
            processing::process_search_responses(&responses, "Marrow Choir", None, &wanted, false);
        assert_eq!((strict.not_audio, strict.below_floor), (1, 1));
        assert_eq!(
            strict
                .albums
                .iter()
                .map(|a| a.username.as_str())
                .collect::<Vec<_>>(),
            vec!["carol_shares"]
        );
        let kept =
            processing::process_search_responses(&responses, "Marrow Choir", None, &wanted, true);
        let erin = kept
            .albums
            .iter()
            .find(|a| a.username == "erin_shares")
            .expect("kept");
        assert!(erin.tracks[0].below_floor);
        assert!(
            !kept
                .albums
                .iter()
                .find(|a| a.username == "carol_shares")
                .unwrap()
                .tracks[0]
                .below_floor
        );
    }

    #[test]
    fn an_item_carries_its_file_and_source_facts() {
        let responses: Vec<SearchResponse> = serde_json::from_str(&mixed_responses()).unwrap();
        let wanted = [WantedTrack {
            title: "O Holy Night".into(),
            duration_s: Some(263),
        }];
        let mut albums =
            processing::process_search_responses(&responses, "Marrow Choir", None, &wanted, true)
                .albums;
        albums.sort_by(|a, b| a.username.cmp(&b.username));
        let carol: shared::download::DownloadableItem = albums[0].tracks[0].clone().into();
        assert_eq!(
            carol.facts,
            shared::download::FileFacts {
                extension: Some("flac".into()),
                bitrate_kbps: None,
                variable_bitrate: None,
                sample_rate_hz: Some(44100),
                bit_depth: Some(16),
                folder_audio_files: Some(1),
                source_has_free_slot: Some(true),
                source_upload_speed: Some(1_048_576),
                source_queue_length: Some(0),
                backend_facts: serde_json::json!({"declared_extension": "flac"})
                    .as_object()
                    .unwrap()
                    .clone()
            }
        );
        assert_eq!(carol.match_score, albums[0].tracks[0].match_score);
        let erin: shared::download::DownloadableItem = albums[1].tracks[0].clone().into();
        assert_eq!(
            (
                erin.facts.extension.as_deref(),
                erin.facts.bitrate_kbps,
                erin.facts.variable_bitrate
            ),
            (Some("mp3"), Some(192), Some(true))
        );
        assert!(
            erin.facts.backend_facts.is_empty(),
            "an empty declared extension is not kept"
        );
        assert!(erin.below_floor);
    }

    #[test]
    fn slskds_retry_limit_comes_from_its_options() {
        let options = serde_json::json!({"transfers": {"download": {"retry":
            {"attempts": 5, "delay": 5000, "maxDelay": 60000, "partial": "resume"}}}});
        assert_eq!(retry_limit_from_options(&options).unwrap(), 5);
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"transfers": {"download": {"retry": {"attempts": 0}}}}),
        ] {
            assert!(matches!(
                retry_limit_from_options(&bad),
                Err(SoulseekError::InvalidResponse(_))
            ));
        }
    }

    /// Through the trait, as the monitor and discovery call it. slskd answers an
    /// empty 404 for a peer it holds no transfer from (`TransfersController.cs:530-533`).
    #[tokio::test]
    async fn the_retry_limit_is_read_once_per_client() {
        let options = r#"{"transfers":{"download":{"retry":{"attempts":3}}}}"#;
        let none = || Reply {
            status: 404,
            body: String::new(),
        };
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: options.into(),
            },
            none(),
            none(),
        ]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        let sources = ["carol_shares".to_string()];
        assert!(crate::DownloadBackend::get_downloads_for(&client, &sources)
            .await
            .unwrap()
            .is_empty());
        assert!(crate::DownloadBackend::get_downloads_for(&client, &sources)
            .await
            .unwrap()
            .is_empty());
        let targets: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.target.clone())
            .collect();
        assert_eq!(
            targets,
            vec![
                "/api/v0/options",
                "/api/v0/transfers/downloads/carol_shares",
                "/api/v0/transfers/downloads/carol_shares"
            ]
        );
    }

    fn empty() -> Reply {
        Reply {
            status: 200,
            body: "[]".into(),
        }
    }

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    fn no_content() -> Reply {
        Reply {
            status: 204,
            body: String::new(),
        }
    }

    /// A client whose search for O Holy Night by Marrow Choir is running.
    async fn searching(
        replies: Vec<Reply>,
        deadline: Duration,
    ) -> (SoulseekClient, Arc<StdMutex<Vec<Seen>>>) {
        searching_limited(replies, deadline, 35, 220).await
    }

    /// `searching`, with a rate limit of `max_searches` in `window_seconds`.
    async fn searching_limited(
        replies: Vec<Reply>,
        deadline: Duration,
        max_searches: usize,
        window_seconds: i64,
    ) -> (SoulseekClient, Arc<StdMutex<Vec<Seen>>>) {
        let mut all = vec![ok(&format!(r#"{{"id":"{SEARCH_ID}"}}"#))];
        all.extend(replies);
        let (url, seen) = serve(all);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .rate_limit(max_searches, window_seconds)
            .build()
            .unwrap();
        client
            .start_search(
                SearchSpec::of(None, vec![track("Marrow Choir", "O Holy Night")]),
                deadline,
            )
            .await
            .unwrap();
        (client, seen)
    }

    #[tokio::test]
    async fn a_search_sends_the_callers_text_and_says_what_it_read() {
        let mut replies = vec![ok(&format!(r#"{{"id":"{SEARCH_ID}"}}"#)), status(true, 2)];
        replies.push(ok(&mixed_responses()));
        replies.push(no_content());
        let (url, seen) = serve(replies);
        // carol's 1 file and erin's 2 (the cover included); the locked ones are counted apart.
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        let spec = SearchSpec {
            text: Some("  Marrow Choir Holy Night choir ".into()),
            ..SearchSpec::of(None, vec![track("Marrow Choir", "O Holy Night")])
        };
        client
            .start_search(spec, Duration::seconds(120))
            .await
            .unwrap();
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            poll.stats,
            SearchStats {
                query_text: "Marrow Choir Holy Night choir".into(),
                response_count: 2,
                file_count: 3,
                locked_file_count: 2,
                not_audio: 1,
                below_floor: 1,
                retried_texts: vec![],
                unanswered_note: None,
                rejection_counts: std::collections::BTreeMap::from([(
                    "title_mismatch".to_string(),
                    1
                )]),
                top_rejected: vec![shared::download::RejectedCandidateSummary {
                    source: "erin_shares".into(),
                    label: "@@pq7\\Share\\Misc\\Jingle Bells Remix.mp3".into(),
                    score: Some(0.0),
                    reason: "title_mismatch".into(),
                }],
            }
        );
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["searchText"], "Marrow Choir Holy Night choir");
    }

    #[tokio::test]
    async fn blank_caller_text_sends_the_built_text_and_no_target_sends_nothing() {
        let (url, seen) = serve(vec![ok(&format!(r#"{{"id":"{SEARCH_ID}"}}"#))]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        let wanted = vec![track("Marrow Choir", "O Holy Night")];
        let blank = SearchSpec {
            text: Some("   ".into()),
            ..SearchSpec::of(None, wanted.clone())
        };
        client
            .start_search(blank, Duration::seconds(120))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["searchText"], "Marrow Choir O Holy Night");
        // Text alone names nothing to score what is found against.
        let untargeted = SearchSpec {
            text: Some("Marrow Choir".into()),
            ..SearchSpec::of(None, vec![])
        };
        let e = client
            .start_search(untargeted, Duration::seconds(120))
            .await
            .unwrap_err();
        assert!(
            matches!(e, SoulseekError::NoSearchTarget { tracks: 0 }),
            "{e:?}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// Review focus 1: slskd saves the record as complete before it writes
    /// the responses (`Search/SearchService.cs:284-285`, then `:359-361`).
    #[tokio::test]
    async fn a_search_slskd_completes_before_writing_its_responses_is_read_again() {
        let (client, seen) = searching(
            vec![status(true, 1), empty(), one_response(), no_content()],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(poll.state, SearchState::Completed);
        assert_eq!(poll.stats.file_count, 1);
        assert_eq!(poll.albums[0].tracks[0].base.username, "carol_shares");
        let targets: Vec<(String, String)> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.method.clone(), s.target.clone()))
            .collect();
        let search = format!("/api/v0/searches/{SEARCH_ID}");
        assert_eq!(
            targets,
            vec![
                ("POST".into(), "/api/v0/searches".into()),
                ("GET".into(), search.clone()),
                ("GET".into(), format!("{search}/responses")),
                ("GET".into(), format!("{search}/responses")),
                ("DELETE".into(), search)
            ]
        );
    }

    /// The id slskd gives a search sent again with a retry text.
    const RETRY_ID: &str = "8d1c0b9a-2e3f-4a5b-8c7d-6e5f4a3b2c1d";
    /// The id slskd gives the second retry.
    const LAST_RETRY_ID: &str = "5b4a3c2d-1e0f-4a9b-8c7d-0f1e2d3c4b5a";

    fn started(id: &str) -> Reply {
        ok(&format!(r#"{{"id":"{id}"}}"#))
    }

    fn methods_and_targets(seen: &StdMutex<Vec<Seen>>) -> Vec<(String, String)> {
        (seen.lock().unwrap().iter())
            .map(|s| (s.method.clone(), s.target.clone()))
            .collect()
    }

    fn texts_sent(seen: &StdMutex<Vec<Seen>>) -> Vec<String> {
        (seen.lock().unwrap().iter())
            .filter(|s| s.method == "POST")
            .map(|s| {
                let body: serde_json::Value = serde_json::from_str(&s.body).unwrap();
                body["searchText"].as_str().unwrap().to_string()
            })
            .collect()
    }

    /// The Soulseek server drops a search holding an excluded phrase: no peer answers it.
    #[tokio::test]
    async fn a_search_no_peer_answers_is_sent_again_with_wildcards_under_its_id() {
        let (client, seen) = searching(
            vec![
                status(true, 0),
                empty(),
                started(RETRY_ID),
                no_content(),
                status(true, 1),
                one_response(),
                no_content(),
            ],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(poll.state, SearchState::Completed);
        assert_eq!(poll.albums[0].tracks[0].base.username, "carol_shares");
        assert_eq!(
            (
                poll.stats.query_text.as_str(),
                poll.stats.retried_texts.clone(),
                poll.stats.response_count,
                poll.stats.unanswered_note
            ),
            (
                "Marrow Choir O Holy Night",
                vec!["*arrow Choir O *oly Night".to_string()],
                1,
                None
            )
        );
        assert_eq!(
            texts_sent(&seen),
            ["Marrow Choir O Holy Night", "*arrow Choir O *oly Night"]
        );
        let (first, retry) = (
            format!("/api/v0/searches/{SEARCH_ID}"),
            format!("/api/v0/searches/{RETRY_ID}"),
        );
        let t = |method: &str, target: &str| (method.to_string(), target.to_string());
        assert_eq!(
            methods_and_targets(&seen),
            vec![
                t("POST", "/api/v0/searches"),
                t("GET", &first),
                t("GET", &format!("{first}/responses")),
                t("POST", "/api/v0/searches"),
                t("DELETE", &first),
                t("GET", &retry),
                t("GET", &format!("{retry}/responses")),
                t("DELETE", &retry),
            ]
        );
        assert!(client.active_searches.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_search_nothing_answers_even_with_wildcards_says_the_server_may_block_it() {
        let (client, seen) = searching(
            vec![
                status(true, 0),
                empty(),
                started(RETRY_ID),
                no_content(),
                status(true, 0),
                empty(),
                started(LAST_RETRY_ID),
                no_content(),
                status(true, 0),
                empty(),
                no_content(),
            ],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.state, poll.stats.response_count, poll.albums.len()),
            (SearchState::Completed, 0, 0)
        );
        let retried = ["*arrow Choir O *oly Night", "Marrow *hoir O Holy *ight"];
        assert_eq!(poll.stats.retried_texts, retried);
        assert_eq!(
            poll.stats.unanswered_note.as_deref(),
            Some(unanswered_note(true).as_str())
        );
        assert!(unanswered_note(true).contains("may be blocking"));
        let mut sent = vec!["Marrow Choir O Holy Night"];
        sent.extend(retried);
        assert_eq!(texts_sent(&seen), sent);
        let last = (seen.lock().unwrap().last().cloned()).unwrap();
        assert_eq!(
            (last.method.as_str(), last.target),
            ("DELETE", format!("/api/v0/searches/{LAST_RETRY_ID}"))
        );
    }

    /// Less of the search's time is left than a search no peer answers takes to end.
    #[tokio::test]
    async fn a_search_without_responses_near_its_deadline_is_read_once() {
        let (client, seen) = searching(
            vec![status(true, 0), empty(), no_content()],
            Duration::seconds(10),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.state, poll.stats.file_count, poll.albums.len()),
            (SearchState::Completed, 0, 0)
        );
        assert!(poll.stats.retried_texts.is_empty());
        assert_eq!(
            poll.stats.unanswered_note.as_deref(),
            Some(unanswered_note(false).as_str())
        );
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_stopped_search_sends_no_retry_text() {
        let (client, seen) = searching(
            vec![ok(""), status(true, 0), empty(), no_content()],
            Duration::seconds(120),
        )
        .await;
        client.stop_search(SEARCH_ID).await.unwrap();
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert!(poll.stats.retried_texts.is_empty());
        assert_eq!(
            poll.stats.unanswered_note, None,
            "a stopped search says nothing of it"
        );
        assert_eq!(texts_sent(&seen).len(), 1);
    }

    /// The wait for the rate limit used up the time a retry text needed.
    #[tokio::test]
    async fn a_retry_text_the_rate_limit_held_past_its_time_is_not_sent() {
        let deadline = Duration::milliseconds(i64::from(SEARCH_INACTIVITY_MS) + 1_000);
        let (client, seen) =
            searching_limited(vec![status(true, 0), empty(), no_content()], deadline, 1, 2).await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert!(poll.stats.retried_texts.is_empty());
        assert_eq!(
            poll.stats.unanswered_note.as_deref(),
            Some(unanswered_note(false).as_str())
        );
        assert_eq!(texts_sent(&seen).len(), 1);
        // The permit the retry waited for was not spent: the next search does not wait.
        assert_eq!(client.search_pacer.total_recorded(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn search_pacer_smooths_burst_into_steady_state_spacing() {
        let pacer = SearchPacer::new(35, StdDuration::from_secs(220));
        let start = TokioInstant::now();
        let mut fired_at = Vec::new();
        for _ in 0..6 {
            let permit = pacer.acquire(SearchPriority::Interactive).await;
            fired_at.push(start.elapsed());
            permit.record_accepted();
        }
        // First 4 searches fire as a 1s-spaced burst: 0s, 1s, 2s, 3s.
        assert_eq!(fired_at[0], StdDuration::ZERO);
        assert_eq!(fired_at[1], StdDuration::from_secs(1));
        assert_eq!(fired_at[2], StdDuration::from_secs(2));
        assert_eq!(fired_at[3], StdDuration::from_secs(3));
        // 5th search waits until steady-state interval (220s / 35 ~= 6.285s) elapses.
        let steady = StdDuration::from_secs(220) / 35;
        assert!(
            fired_at[4] >= steady && fired_at[4] <= steady + StdDuration::from_millis(5),
            "5th search fired at {:?}",
            fired_at[4]
        );
        // 6th search is spaced by another steady-state interval after the 5th.
        let gap_5_6 = fired_at[5] - fired_at[4];
        assert!(
            gap_5_6 >= steady && gap_5_6 <= steady + StdDuration::from_millis(5),
            "gap between 5th and 6th search was {:?}",
            gap_5_6
        );
    }

    #[tokio::test(start_paused = true)]
    async fn search_pacer_reserves_headroom_for_interactive_and_yields_background() {
        let pacer = Arc::new(SearchPacer::new(35, StdDuration::from_secs(220)));
        // Fill 30 slots (leaving the 5 reserved for Interactive).
        {
            let mut state = pacer.state.lock().unwrap();
            let now = TokioInstant::now();
            for _ in 0..30 {
                state.timestamps.push_back(now);
            }
            // No recent burst debt, only window count = 30.
            state.last_sent = None;
            state.tat = None;
        }

        // Background sees 30/30 and must wait the full 220s window.
        let bg_pacer = Arc::clone(&pacer);
        let bg_start = TokioInstant::now();
        let bg_task = tokio::spawn(async move {
            let permit = bg_pacer.acquire(SearchPriority::Background).await;
            permit.record_accepted();
            bg_start.elapsed()
        });

        // Give the background task a turn to hit its sleep.
        tokio::task::yield_now().await;

        // Interactive can immediately use a reserved slot without waiting 220s.
        let int_start = TokioInstant::now();
        let permit = pacer.acquire(SearchPriority::Interactive).await;
        assert!(
            int_start.elapsed() < StdDuration::from_secs(1),
            "interactive waited {:?} despite reserved headroom",
            int_start.elapsed()
        );
        permit.record_accepted();
        drop(permit);

        let bg_elapsed = bg_task.await.unwrap();
        assert!(
            bg_elapsed >= StdDuration::from_secs(220),
            "background bypassed interactive reserve after {:?}",
            bg_elapsed
        );
    }

    #[tokio::test]
    async fn failed_search_post_does_not_burn_pacer_quota() {
        let (base_url, _seen) = serve(vec![Reply {
            status: 500,
            body: "slskd error".into(),
        }]);
        let client = SoulseekClientBuilder::new()
            .base_url(&base_url)
            .api_key("test-key")
            .rate_limit(1, 220)
            .build()
            .unwrap();
        let err = client
            .start_search(
                SearchSpec::of(None, vec![track("Marrow Choir", "O Holy Night")]),
                Duration::seconds(30),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SoulseekError::Api { status: 500, .. }));
        assert_eq!(client.search_pacer.total_recorded(), 0);
    }

    /// slskd refused the retry text: the search ends as the texts that ran found it.
    #[tokio::test]
    async fn a_retry_text_slskd_refuses_ends_the_search_unanswered() {
        let refused = Reply {
            status: 500,
            body: "Search failed".into(),
        };
        let (client, seen) = searching(
            vec![status(true, 0), empty(), refused, no_content()],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.state, poll.stats.response_count),
            (SearchState::Completed, 0)
        );
        assert!(poll.stats.retried_texts.is_empty());
        assert_eq!(
            poll.stats.unanswered_note.as_deref(),
            Some(unanswered_note(false).as_str())
        );
        let last = (seen.lock().unwrap().last().cloned()).unwrap();
        assert_eq!(
            (last.method.as_str(), last.target),
            ("DELETE", format!("/api/v0/searches/{SEARCH_ID}"))
        );
    }

    /// Soulseek.NET threw during the search (`SearchService.cs:331-345`): no peer may have been
    /// asked, so it is a failure, not a search the server dropped.
    #[tokio::test]
    async fn a_search_slskd_ended_errored_without_responses_fails_and_is_not_sent_again() {
        let (client, seen) = searching(
            vec![status_in("Completed, Errored", 0), empty(), no_content()],
            Duration::seconds(120),
        )
        .await;
        let e = client.poll_search(SEARCH_ID.into()).await.unwrap_err();
        assert!(
            matches!(&e, SoulseekError::SearchErrored { search_id } if search_id == SEARCH_ID),
            "{e:?}"
        );
        assert_eq!(texts_sent(&seen).len(), 1);
        assert!(client.active_searches.lock().await.is_empty());
    }

    /// What an errored search had found is read as any other search's.
    #[tokio::test]
    async fn an_errored_search_keeps_the_responses_it_had() {
        let (client, _) = searching(
            vec![
                status_in("Completed, Errored", 1),
                one_response(),
                no_content(),
            ],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(poll.albums[0].tracks[0].base.username, "carol_shares");
    }

    /// Someone cancelled the search in slskd itself.
    #[tokio::test]
    async fn a_search_cancelled_in_slskd_is_not_sent_again() {
        let (client, seen) = searching(
            vec![status_in("Completed, Cancelled", 0), empty(), no_content()],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.stats.retried_texts.len(), poll.stats.unanswered_note),
            (0, None)
        );
        assert_eq!(texts_sent(&seen).len(), 1);
    }

    /// A stop slskd did not take still ends the retries, and leaves the search to be cancelled at
    /// its deadline.
    #[tokio::test]
    async fn a_stop_slskd_refused_sends_no_retry_text_and_keeps_the_deadline_cancel() {
        let refused = Reply {
            status: 500,
            body: "boom".into(),
        };
        let (client, seen) = searching(
            vec![refused, status(true, 0), empty(), no_content()],
            Duration::seconds(120),
        )
        .await;
        client.stop_search(SEARCH_ID).await.unwrap_err();
        {
            let searches = client.active_searches.lock().await;
            let context = &searches[SEARCH_ID];
            assert!(context.stop_requested && !context.cancelled);
        }
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.stats.retried_texts.len(), poll.stats.unanswered_note),
            (0, None)
        );
        assert_eq!(texts_sent(&seen).len(), 1);
    }

    /// A stop reaches the slskd search a retry text started, not the one that ended.
    #[tokio::test]
    async fn a_stop_reaches_the_retry_that_runs() {
        let (client, seen) = searching(vec![ok("")], Duration::seconds(120)).await;
        if let Some(context) = client.active_searches.lock().await.get_mut(SEARCH_ID) {
            context.slskd_id = RETRY_ID.into();
            context.retried = 1;
        }
        client.stop_search(SEARCH_ID).await.unwrap();
        let stop = seen.lock().unwrap()[1].clone();
        assert_eq!(
            (stop.method.as_str(), stop.target),
            ("PUT", format!("/api/v0/searches/{RETRY_ID}"))
        );
        assert!(client.active_searches.lock().await[SEARCH_ID].cancelled);
    }

    /// Soulseek wildcard terms (`*` for a word's first letter) are sent as the caller wrote them.
    #[tokio::test]
    async fn a_callers_wildcards_reach_slskd_as_written() {
        let (url, seen) = serve(vec![started(SEARCH_ID)]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        let spec = SearchSpec {
            text: Some(" Ella Fitzgerald *inter Wonderland ".into()),
            ..SearchSpec::of(None, vec![track("Ella Fitzgerald", "Winter Wonderland")])
        };
        client
            .start_search(spec, Duration::seconds(120))
            .await
            .unwrap();
        assert_eq!(texts_sent(&seen), ["Ella Fitzgerald *inter Wonderland"]);
        assert_eq!(
            client.active_searches.lock().await[SEARCH_ID].retry_texts,
            [
                "*lla Fitzgerald *inter *onderland",
                "Ella *itzgerald *inter Wonderland"
            ]
        );
    }

    /// slskd leaves a record this way when its final update fails
    /// (`SearchService.cs:370-374`).
    #[tokio::test]
    async fn responses_slskd_counted_but_never_wrote_are_a_lost_search() {
        let mut replies = vec![status(true, 2)];
        replies.extend((0..=RESPONSES_REREADS).map(|_| empty()));
        replies.push(no_content());
        let (client, _) = searching(replies, Duration::seconds(120)).await;
        let e = client.poll_search(SEARCH_ID.into()).await.unwrap_err();
        assert!(
            matches!(e, SoulseekError::SearchResponsesLost { response_count: 2 }),
            "{e:?}"
        );
        assert_eq!(
            e.failure(Stage::SourceSearch, "m").code,
            FailureCode::SearchLost
        );
    }

    /// Review focus 4: slskd's inactivity timer restarts at every response,
    /// so a busy search outlives the deadline.
    #[tokio::test]
    async fn a_search_past_its_deadline_is_cancelled_and_its_responses_read() {
        let (client, seen) = searching(
            vec![
                status(false, 1),
                ok(""),
                status(true, 1),
                one_response(),
                no_content(),
            ],
            Duration::zero(),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.state, poll.stats.file_count),
            (SearchState::Completed, 1)
        );
        let cancel = &seen.lock().unwrap()[2];
        assert_eq!(
            (cancel.method.as_str(), cancel.target.as_str()),
            ("PUT", format!("/api/v0/searches/{SEARCH_ID}").as_str())
        );
    }

    /// slskd answers a cancel with an empty 404 only for a search it no longer
    /// holds (`SearchesController.cs:222-225`; empty because of `Program.cs:973`).
    #[tokio::test]
    async fn a_search_slskd_no_longer_holds_at_its_cancel_is_gone() {
        let (client, seen) = searching(
            vec![
                status(false, 1),
                Reply {
                    status: 404,
                    body: String::new(),
                },
                Reply {
                    status: 404,
                    body: String::new(),
                },
            ],
            Duration::zero(),
        )
        .await;
        let e = client.poll_search(SEARCH_ID.into()).await.unwrap_err();
        assert!(
            matches!(e, SoulseekError::SearchGone { ref body } if body.is_empty()),
            "{e:?}"
        );
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.method.clone())
            .collect();
        assert_eq!(methods, ["POST", "GET", "PUT", "DELETE"]);
    }

    #[test]
    fn a_poll_reads_waits_cancels_or_gives_up() {
        let s = Duration::seconds;
        let d = s(120);
        assert!(matches!(poll_step(true, s(500), d, false), PollStep::Read));
        assert!(matches!(poll_step(false, s(119), d, false), PollStep::Wait));
        assert!(matches!(
            poll_step(false, s(120), d, false),
            PollStep::Cancel
        ));
        assert!(matches!(poll_step(false, s(134), d, true), PollStep::Wait));
        assert!(matches!(
            poll_step(false, s(135), d, true),
            PollStep::GiveUp
        ));
    }

    #[test]
    fn settings_slskd_cannot_run_without_are_not_configured() {
        let set = || Some("x".to_string());
        for (url, key, missing) in [
            (None, set(), MissingSetting::Url),
            (set(), None, MissingSetting::ApiKey),
            (Some(String::new()), set(), MissingSetting::Empty),
            (set(), Some(String::new()), MissingSetting::Empty),
        ] {
            assert!(matches!(
                SoulseekClientBuilder::from_settings(url, key),
                Err(SoulseekError::NotConfigured(m)) if m == missing
            ));
        }
        assert!(SoulseekClientBuilder::from_settings(set(), set()).is_ok());
    }

    #[tokio::test]
    async fn spent_retries_keep_the_last_error_and_its_code() {
        let batches = plan_batches(
            vec![wanted("peer", r"music\A\01.flac")],
            "A [1]",
            3,
            REQUEST,
        );
        let config = DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 0,
            max_retries: 2,
            retry_base_delay_ms: 0,
        };
        let mut sent = 0;
        let results = retry_batch(&config, &batches[0], |_| {
            sent += 1;
            async {
                Err(SoulseekError::Api {
                    status: 429,
                    message: "Only one concurrent operation is permitted. Wait until the previous request completes".into(),
                })
            }
        })
        .await;
        assert_eq!(sent, 3);
        assert!(failed_with(
            &results[0].status,
            FailureCode::EnqueueFailed,
            "Failed after 2 retries: API error: 429 - Only one concurrent operation is permitted. Wait until the previous request completes"
        ));
        let EnqueueStatus::Failed(f) = &results[0].status else {
            unreachable!()
        };
        assert_eq!(f.retry, RetryAdvice::SameSourceLater);
        assert_eq!(f.cause.as_ref().unwrap().status, Some(429));
    }

    fn listed(id: &str, filename: &str, batch_id: Option<&str>) -> FileEntry {
        let mut entry = serde_json::json!({
            "id": id,
            "username": "peer",
            "direction": "Download",
            "filename": filename,
            "size": 1000,
            "state": "InProgress",
            "requestedAt": "2026-07-26T00:00:00Z",
            "bytesTransferred": 0,
            "percentComplete": 0.0
        });
        if let Some(batch_id) = batch_id {
            entry["batchId"] = batch_id.into();
        }
        serde_json::from_value(entry).unwrap()
    }

    #[test]
    fn a_replayed_batch_takes_only_its_own_transfers() {
        let batches = plan_batches(
            vec![
                wanted("peer", r"music\A\01.flac"),
                wanted("peer", r"music\A\02.flac"),
            ],
            "A [1]",
            3,
            REQUEST,
        );
        let ours = batches[0].id.to_uppercase();
        // 01 was enqueued by the attempt whose answer was lost. 02 failed in
        // that attempt because another batch was already fetching it.
        let listing = vec![
            listed("t-01", r"music\A\01.flac", Some(&ours)),
            listed(
                "t-other",
                r"music\A\02.flac",
                Some("ffffffff-0000-0000-0000-000000000000"),
            ),
        ];

        let results = replayed_batch_responses(&batches[0], &listing);

        assert_eq!(results[0].status, EnqueueStatus::Enqueued);
        assert_eq!(results[0].transfer_id.as_deref(), Some("t-01"));
        assert!(failed_with(
            &results[1].status,
            FailureCode::EnqueueFailed,
            "Not found in slskd after a retried enqueue"
        ));
        assert_eq!(results[1].transfer_id, None);
    }

    #[tokio::test]
    async fn one_transfer_is_read_until_slskd_no_longer_has_it() {
        let options = r#"{"transfers":{"download":{"retry":{"attempts":3}}}}"#;
        let record = |removed: bool| {
            serde_json::json!({
                "id": "890f943c-02e1-4d45-af76-d55e3d855684", "username": "peer", "direction": "Download",
                "filename": "shared\\Artist\\Album\\01. Track.flac", "size": 1024, "state": "InProgress",
                "requestedAt": "2026-07-19T05:11:22Z", "enqueuedAt": "2026-07-19T05:11:23Z",
                "startedAt": "2026-07-19T05:11:24Z", "bytesTransferred": 512, "averageSpeed": 250000.0,
                "attempts": 1, "removed": removed, "bytesRemaining": 512, "percentComplete": 50.0 })
            .to_string()
        };
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: options.into(),
            },
            Reply {
                status: 200,
                body: record(false),
            },
            Reply {
                status: 404,
                body: String::new(),
            },
            Reply {
                status: 200,
                body: record(true),
            },
        ]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        let id = "890f943c-02e1-4d45-af76-d55e3d855684";
        let row = crate::DownloadBackend::get_download(&client, "peer", id)
            .await
            .unwrap()
            .expect("listed");
        assert_eq!(
            (row.state, row.transferred),
            (shared::download::DownloadState::InProgress, 512)
        );
        assert_eq!(
            crate::DownloadBackend::get_download(&client, "peer", id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            crate::DownloadBackend::get_download(&client, "peer", id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            seen.lock().unwrap()[1].target,
            format!("/api/v0/transfers/downloads/peer/{id}")
        );
    }

    #[tokio::test]
    async fn a_queued_transfers_place_is_asked_of_the_peer() {
        let id = "890f943c-02e1-4d45-af76-d55e3d855684";
        // TransferNotFoundException's text (Soulseek.NET src/SoulseekClient.cs:1645), answered
        // as 500 by TransfersController.cs:614 and written as a JSON string (`:56`).
        let inactive = "A download of @@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac from user carol_shares is not active";
        let (url, seen) = serve(vec![
            ok("3"),
            Reply {
                status: 404,
                body: String::new(),
            },
            Reply {
                status: 500,
                body: serde_json::to_string(inactive).unwrap(),
            },
            // A peer's place read as a signed int (PlaceInQueueResponse.cs:36,50).
            ok("-1"),
            // SoulseekClient.cs:3790.
            Reply {
                status: 500,
                body: serde_json::to_string("User carol_shares appears to be offline").unwrap(),
            },
        ]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        assert_eq!(
            client.get_place_in_queue("carol_shares", id).await.unwrap(),
            Some(3)
        );
        assert_eq!(
            client.get_place_in_queue("carol_shares", id).await.unwrap(),
            None
        );
        let err = client
            .get_place_in_queue("carol_shares", id)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SoulseekError::Api { status: 500, message } if message == inactive),
            "{err:?}"
        );
        assert_eq!(
            client.get_place_in_queue("carol_shares", id).await.unwrap(),
            None
        );
        let err = client
            .get_place_in_queue("carol_shares", id)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SoulseekError::UserOffline { username, status: 500, body }
                if username == "carol_shares" && body == "User carol_shares appears to be offline"),
            "{err:?}"
        );
        assert_eq!(
            seen.lock().unwrap()[0].target,
            format!("/api/v0/transfers/downloads/carol_shares/{id}/position")
        );
    }

    #[tokio::test]
    async fn a_sources_presence_is_read_and_a_404_is_a_blocked_user() {
        let (url, seen) = serve(vec![
            ok(r#"{"isPrivileged":false,"presence":"Away"}"#),
            ok(r#"{"isPrivileged":true,"presence":"Online"}"#),
            ok(r#"{"isPrivileged":false,"presence":"Offline"}"#),
            Reply {
                status: 404,
                body: String::new(),
            },
            // The Soulseek server did not answer GetStatus in time (SoulseekClient.cs:3866),
            // written as a JSON string by the global handler (Program.cs:1051-1053).
            Reply {
                status: 500,
                body: serde_json::to_string("The wait timed out after 5000 milliseconds").unwrap(),
            },
        ]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        for expected in [
            SourcePresence::Away,
            SourcePresence::Online,
            SourcePresence::Offline,
            SourcePresence::Blocked,
        ] {
            assert_eq!(
                client.get_user_presence("carol_shares").await.unwrap(),
                expected
            );
        }
        let err = client.get_user_presence("carol_shares").await.unwrap_err();
        assert!(
            matches!(&err, SoulseekError::Api { status: 500, message }
                if message == "The wait timed out after 5000 milliseconds"),
            "{err:?}"
        );
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/api/v0/users/carol_shares/status"
        );
    }

    #[tokio::test]
    async fn a_search_is_stopped_and_one_slskd_no_longer_has_is_no_error() {
        let (url, seen) = serve(vec![
            ok(""),
            Reply {
                status: 404,
                body: String::new(),
            },
        ]);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        client.stop_search(SEARCH_ID).await.unwrap();
        client.stop_search(SEARCH_ID).await.unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target),
            ("PUT", format!("/api/v0/searches/{SEARCH_ID}"))
        );
        assert_eq!(sent.body, "");
        // An id that is no one path segment is never sent.
        assert!(client.stop_search("..").await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// A search stopped while this client polls it is given the grace a cancelled search has,
    /// not cancelled again at its deadline.
    #[tokio::test]
    async fn a_stopped_search_is_not_cancelled_again_at_its_deadline() {
        let (client, seen) = searching(
            vec![
                ok(""),
                status(false, 1),
                status(true, 1),
                one_response(),
                no_content(),
            ],
            Duration::zero(),
        )
        .await;
        client.stop_search(SEARCH_ID).await.unwrap();
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(poll.state, SearchState::Completed);
        let stops = (seen.lock().unwrap().iter())
            .filter(|sent| sent.method == "PUT")
            .count();
        assert_eq!(stops, 1);
    }
}
