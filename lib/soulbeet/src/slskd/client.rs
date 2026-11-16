use super::{processing, utils::WantedTrack};
use crate::{
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
    download::EnqueueStatus,
    failure::{
        Cause, Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
    },
    metadata::{Album, Track},
    slskd::{AlbumResult, DownloadResponse, FileEntry, FlattenedFiles, SearchState, TrackResult},
};
use std::{collections::HashMap, sync::Arc, time::Duration as StdDuration};
use tokio::sync::Mutex;
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
    start_time: DateTime<Utc>,
    timeout: Duration,
    /// slskd was told to stop the search at the deadline.
    cancelled: bool,
}

/// What one poll of a search found: the processed albums and the files in
/// slskd's responses, both empty while the search is in progress.
#[derive(Debug)]
pub struct SearchPoll {
    pub albums: Vec<AlbumResult>,
    pub state: SearchState,
    pub files_seen: u32,
}

/// The part of slskd's search record a poll reads (`GET searches/{id}`,
/// `SearchesController.cs:140`; `Search/Types/Search.cs:52,55`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchStatus {
    is_complete: bool,
    response_count: u32,
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

#[derive(Debug)]
pub struct SoulseekClient {
    base_url: Url,
    api_key: Option<String>,
    client: Client,
    search_timestamps: Arc<Mutex<Vec<DateTime<Utc>>>>,
    active_searches: Arc<Mutex<HashMap<String, SearchContext>>>,
    max_searches_per_window: usize,
    rate_limit_window: Duration,
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

        Ok(SoulseekClient {
            base_url,
            api_key: self.api_key,
            client,
            search_timestamps: Arc::new(Mutex::new(Vec::new())),
            active_searches: Arc::new(Mutex::new(HashMap::new())),
            max_searches_per_window: self.max_searches_per_window.unwrap_or(35),
            rate_limit_window: Duration::seconds(self.rate_limit_window_seconds.unwrap_or(220)),
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

    async fn wait_for_rate_limit(&self) -> Result<()> {
        let mut timestamps = self.search_timestamps.lock().await;
        let now = Utc::now();
        let window_start = now - self.rate_limit_window;
        timestamps.retain(|&ts| ts > window_start);
        if timestamps.len() >= self.max_searches_per_window {
            if let Some(&oldest) = timestamps.first() {
                let wait_duration = (oldest + self.rate_limit_window) - now;
                if !wait_duration.is_zero() {
                    info!(
                        "Rate limit reached ({}/{}), waiting for {:.1}s",
                        timestamps.len(),
                        self.max_searches_per_window,
                        wait_duration.as_seconds_f64()
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        wait_duration.num_milliseconds() as u64,
                    ))
                    .await;
                }
            }
        }
        timestamps.push(now);
        Ok(())
    }

    pub async fn start_search(
        &self,
        album: Option<Album>,
        tracks: Vec<Track>,
        timeout: Duration,
    ) -> Result<String> {
        let query = search_text(album.as_ref(), &tracks)?;
        self.wait_for_rate_limit().await?;

        let wanted: Vec<WantedTrack> = tracks
            .iter()
            .map(|t| WantedTrack {
                title: t.title.clone(),
                duration_s: t.duration_secs(),
            })
            .collect();

        info!(
            "Starting search for: '{}' with timeout {}ms",
            query,
            timeout.num_milliseconds()
        );

        let request_body = SearchRequest::new(&query);

        #[derive(Deserialize)]
        struct SearchId {
            id: String,
        }
        let search_id_resp: SearchId = self
            .make_request(Method::POST, "searches", Some(&request_body))
            .await
            .map_err(|e| match e {
                SoulseekError::Api {
                    status: 400,
                    message,
                } => search_refused(message),
                e => e,
            })?;
        let search_id = search_id_resp.id;

        self.active_searches.lock().await.insert(
            search_id.clone(),
            SearchContext {
                album: album.as_ref().map(|a| a.title.clone()),
                artist: album
                    .as_ref()
                    .map(|a| a.artist.clone())
                    .unwrap_or_else(|| tracks[0].artist.clone()),
                tracks: wanted,
                start_time: Utc::now(),
                timeout,
                cancelled: false,
            },
        );

        info!("Search initiated with ID: {search_id}");
        Ok(search_id)
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
        self.active_searches.lock().await.remove(&search_id);
        let _ = self.delete_search(&search_id).await;
        match &polled {
            Ok(poll) => info!(
                "Search {} completed with {} files from slskd",
                search_id, poll.files_seen
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
        let endpoint = format!("searches/{search_id}");

        loop {
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
                    return self
                        .read_responses(search_id, &context, status.response_count)
                        .await
                }
                PollStep::Wait => {
                    if (Utc::now() - poll_start) > long_poll_timeout {
                        return Ok(SearchPoll {
                            albums: Vec::new(),
                            state: SearchState::InProgress,
                            files_seen: 0,
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

    /// The responses of a search slskd completed, counting `response_count`.
    async fn read_responses(
        &self,
        search_id: &str,
        context: &SearchContext,
        response_count: u32,
    ) -> Result<SearchPoll> {
        let endpoint = format!("searches/{search_id}/responses");
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

        let mut albums = processing::process_search_responses(
            &responses,
            &context.artist,
            context.album.as_deref(),
            &context.tracks,
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
            files_seen: responses.iter().map(|r| r.files.len() as u32).sum(),
        })
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

    /// Every transfer slskd holds, across every peer.
    ///
    /// Unbounded on slskd's side, so this is reserved for the one case that
    /// needs it: spotting a file retried from a peer this batch never asked.
    /// Routine polling uses `get_downloads_for_users`.
    pub async fn get_all_downloads(&self) -> Result<Vec<FileEntry>> {
        let flattened: FlattenedFiles = self
            .make_request(Method::GET, "transfers/downloads", None::<()>)
            .await?;
        Self::usable_files(flattened.files, flattened.errors)
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
        "soulseek"
    }

    fn name(&self) -> &'static str {
        "Soulseek"
    }

    async fn start_search(&self, album: Option<&Album>, tracks: &[Track]) -> Result<String> {
        let timeout = Duration::seconds(120);
        self.start_search(album.cloned(), tracks.to_vec(), timeout)
            .await
    }

    async fn poll_search(&self, search_id: &str) -> Result<shared::download::SearchResult> {
        let poll = self.poll_search(search_id.to_string()).await?;
        Ok(shared::download::SearchResult {
            search_id: search_id.to_string(),
            groups: poll.albums.into_iter().map(Into::into).collect(),
            has_more: poll.state == SearchState::InProgress,
            state: poll.state.into(),
            files_seen: poll.files_seen,
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
    ) -> Result<Vec<shared::download::QueuedDownload>> {
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
    ) -> Result<Vec<shared::download::DownloadProgress>> {
        let limit = self.retry_limit().await?;
        let entries = self.get_downloads_for_users(sources).await?;
        Ok(entries
            .into_iter()
            .map(|e| e.into_progress(limit))
            .collect())
    }

    async fn get_downloads(&self) -> Result<Vec<shared::download::DownloadProgress>> {
        let limit = self.retry_limit().await?;
        let entries = self.get_all_downloads().await?;
        Ok(entries
            .into_iter()
            .map(|e| e.into_progress(limit))
            .collect())
    }

    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> Result<Option<shared::download::DownloadProgress>> {
        let limit = self.retry_limit().await?;
        Ok(self
            .get_download(source, id)
            .await?
            .map(|e| e.into_progress(limit)))
    }

    async fn cancel_download(&self, username: &str, download_id: &str, remove: bool) -> Result<()> {
        self.cancel_download(username, download_id, remove).await
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

/// The query text for a search: one track names itself, an album names the
/// album, and a search without an album needs exactly one track.
fn search_text(album: Option<&Album>, tracks: &[Track]) -> Result<String> {
    match (album, tracks) {
        (Some(album), [track]) => Ok(format!("{} {}", album.artist.trim(), track.title.trim())),
        (Some(album), _) => Ok(format!("{} {}", album.artist.trim(), album.title.trim())),
        (None, [track]) => Ok(format!("{} {}", track.artist.trim(), track.title.trim())),
        (None, _) => Err(SoulseekError::NoSearchTarget {
            tracks: tracks.len(),
        }),
    }
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
        }
    }

    /// slskd's `GET application` with its server in `state` (a
    /// `SoulseekClientStates` flags value, `Core/API/DTO/ServerState.cs`).
    fn application(state: &str, connected: bool, logged_in: bool) -> String {
        format!(
            r#"{{"server":{{"address":"vps.slsknet.org:2271","ipEndPoint":null,"state":"{state}","username":"marrow","isConnected":{connected},"isLoggedIn":{logged_in},"isTransitioning":false}}}}"#
        )
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
            search_text(None, std::slice::from_ref(&night)).unwrap(),
            "Marrow Choir O Holy Night"
        );
        assert_eq!(
            search_text(Some(&winter), std::slice::from_ref(&night)).unwrap(),
            "Marrow Choir O Holy Night"
        );
        assert_eq!(
            search_text(Some(&winter), &[night, silent]).unwrap(),
            "Marrow Choir Winter Songs"
        );
        assert_eq!(
            search_text(Some(&winter), &[]).unwrap(),
            "Marrow Choir Winter Songs"
        );
    }

    #[test]
    fn a_search_without_an_album_takes_exactly_one_track() {
        let two = vec![
            track("Marrow Choir", "O Holy Night"),
            track("Marrow Choir", "Silent Night"),
        ];
        for tracks in [Vec::new(), two] {
            let e = search_text(None, &tracks).unwrap_err();
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
        Reply {
            status: 200,
            body: serde_json::json!({
                "fileCount": responses, "id": SEARCH_ID, "isComplete": complete, "lockedFileCount": 0,
                "responseCount": responses, "responses": [], "searchText": "Marrow Choir O Holy Night",
                "startedAt": "2026-11-15T10:00:00.0000000Z",
                "state": if complete { "Completed, TimedOut" } else { "InProgress" }, "token": 17 })
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
        );
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
        );
        assert_eq!(albums[0].queue_length, 4_294_967_295);
        assert_eq!(albums[0].tracks[0].base.queue_length, 4_294_967_295);
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
        let mut all = vec![ok(&format!(r#"{{"id":"{SEARCH_ID}"}}"#))];
        all.extend(replies);
        let (url, seen) = serve(all);
        let client = SoulseekClientBuilder::new()
            .base_url(&url)
            .api_key("k")
            .build()
            .unwrap();
        client
            .start_search(None, vec![track("Marrow Choir", "O Holy Night")], deadline)
            .await
            .unwrap();
        (client, seen)
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
        assert_eq!(poll.files_seen, 1);
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

    #[tokio::test]
    async fn a_search_without_responses_is_read_once() {
        let (client, seen) = searching(
            vec![status(true, 0), empty(), no_content()],
            Duration::seconds(120),
        )
        .await;
        let poll = client.poll_search(SEARCH_ID.into()).await.unwrap();
        assert_eq!(
            (poll.state, poll.files_seen, poll.albums.len()),
            (SearchState::Completed, 0, 0)
        );
        assert_eq!(seen.lock().unwrap().len(), 4);
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
        assert_eq!((poll.state, poll.files_seen), (SearchState::Completed, 1));
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
}
