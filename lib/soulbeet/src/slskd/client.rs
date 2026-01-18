use super::processing;
use crate::{
    error::{Result, SoulseekError},
    slskd::models::{DownloadRequestFile, SearchResponse},
};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
    musicbrainz::{Album, Track},
    slskd::{AlbumResult, DownloadResponse, FileEntry, FlattenedFiles, SearchState, TrackResult},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration as StdDuration,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

const MAX_SEARCH_RESULTS: usize = 50;

/// HTTP client timeouts
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Circuit breaker configuration
const CIRCUIT_BREAKER_FAILURE_THRESHOLD: u64 = 5;
const CIRCUIT_BREAKER_RESET_TIMEOUT_SECS: u64 = 60;

/// Circuit breaker state for protecting against cascading failures
#[derive(Debug)]
pub struct CircuitBreaker {
    failure_count: AtomicU64,
    last_failure_time: Mutex<Option<DateTime<Utc>>>,
    failure_threshold: u64,
    reset_timeout: Duration,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            failure_count: AtomicU64::new(0),
            last_failure_time: Mutex::new(None),
            failure_threshold: CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            reset_timeout: Duration::seconds(CIRCUIT_BREAKER_RESET_TIMEOUT_SECS as i64),
        }
    }
}

impl CircuitBreaker {
    /// Check if the circuit breaker is open (blocking requests)
    pub async fn is_open(&self) -> bool {
        let failures = self.failure_count.load(Ordering::Relaxed);
        if failures < self.failure_threshold {
            return false;
        }

        // Check if reset timeout has passed
        let last_failure = self.last_failure_time.lock().await;
        if let Some(last_time) = *last_failure {
            if Utc::now() - last_time > self.reset_timeout {
                // Reset the circuit breaker
                drop(last_failure);
                self.reset().await;
                return false;
            }
        }

        true
    }

    /// Record a successful request
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
    }

    /// Record a failed request
    pub async fn record_failure(&self) {
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        let mut last_failure = self.last_failure_time.lock().await;
        *last_failure = Some(Utc::now());
    }

    /// Reset the circuit breaker
    pub async fn reset(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
        let mut last_failure = self.last_failure_time.lock().await;
        *last_failure = None;
    }

    /// Get current failure count
    pub fn failure_count(&self) -> u64 {
        self.failure_count.load(Ordering::Relaxed)
    }
}

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
    track_titles: Vec<String>,
    start_time: DateTime<Utc>,
    timeout: Duration,
    seen_response_count: usize,
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
        let mut resolved_url = url.to_string();
        if Path::new("/.dockerenv").exists() && resolved_url.contains("localhost") {
            resolved_url = resolved_url.replace("localhost", "host.docker.internal");
            info!(
                "Docker detected, using {} for slskd connection",
                resolved_url
            );
        }
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

    pub fn build(self) -> Result<SoulseekClient> {
        let base_url_str = self.base_url.ok_or(SoulseekError::NotConfigured)?;
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
        })
    }
}

impl SoulseekClient {
    async fn make_request<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        endpoint: &str,
        body: Option<B>,
    ) -> Result<T> {
        // Check circuit breaker before making request
        if self.circuit_breaker.is_open().await {
            warn!(
                "Circuit breaker is open ({} consecutive failures), rejecting request to {}",
                self.circuit_breaker.failure_count(),
                endpoint
            );
            return Err(SoulseekError::Api {
                status: 503,
                message: "Circuit breaker is open - slskd appears to be unavailable".to_string(),
            });
        }

        let url = self.base_url.join(&format!("api/v0/{endpoint}"))?;
        debug!("Request: {} {}", method, url);
        let mut request = self.client.request(method, url);
        if let Some(key) = &self.api_key {
            request = request.header("X-API-Key", key);
        }
        if let Some(b) = body {
            request = request.json(&b);
        }

        let response = match request.send().await {
            Ok(resp) => {
                self.circuit_breaker.record_success();
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

        Self::handle_response(response).await
    }

    async fn handle_response<T: DeserializeOwned>(response: Response) -> Result<T> {
        let status = response.status();
        if status.is_success() {
            let text = response.text().await?;
            if text.trim().is_empty() {
                serde_json::from_str("null").map_err(|e| SoulseekError::Api {
                    status: status.as_u16(),
                    message: format!("JSON parse error: {e}"),
                })
            } else {
                serde_json::from_str(&text).map_err(|e| SoulseekError::Api {
                    status: status.as_u16(),
                    message: format!("JSON parse error: {e}"),
                })
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
        self.wait_for_rate_limit().await?;

        let track_titles: Vec<String> = tracks.iter().map(|t| t.title.clone()).collect();

        let query = match album {
            Some(ref album) => match tracks.len() {
                1 => format!("{} {}", album.artist.trim(), tracks[0].title.trim()),
                _ => format!("{} {}", album.artist.trim(), album.title.trim()),
            },
            // No album, should be a single track search
            None => format!("{} {}", tracks[0].artist.trim(), tracks[0].title.trim()),
        };

        info!(
            "Starting search for: '{}' with timeout {}ms",
            query,
            timeout.num_milliseconds()
        );

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct SearchRequest<'a> {
            search_text: &'a str,
            timeout: i64,
            filter_responses: bool,
            minimum_peer_upload_speed: u32,
        }
        let request_body = SearchRequest {
            search_text: &query,
            timeout: timeout.num_milliseconds(),
            filter_responses: true,
            minimum_peer_upload_speed: 10,
        };

        #[derive(Deserialize)]
        struct SearchId {
            id: String,
        }
        let search_id_resp: SearchId = self
            .make_request(Method::POST, "searches", Some(&request_body))
            .await?;
        let search_id = search_id_resp.id;

        self.active_searches.lock().await.insert(
            search_id.clone(),
            SearchContext {
                album: album.as_ref().map(|a| a.title.clone()),
                artist: album
                    .as_ref()
                    .map(|a| a.artist.clone())
                    .unwrap_or_else(|| tracks[0].artist.clone()),
                track_titles,
                start_time: Utc::now(),
                timeout,
                seen_response_count: 0,
            },
        );

        info!("Search initiated with ID: {search_id}");
        Ok(search_id)
    }

    pub async fn poll_search(
        &self,
        search_id: String,
    ) -> Result<(Vec<AlbumResult>, bool, SearchState)> {
        let poll_start = Utc::now();
        // Long-poll duration: hold the request for up to 10 seconds waiting for new data
        let long_poll_timeout = Duration::seconds(10);

        loop {
            let context = {
                let guard = self.active_searches.lock().await;
                guard.get(&search_id).cloned()
            };

            let context = match context {
                Some(ctx) => ctx,
                None => return Ok((vec![], false, SearchState::NotFound)),
            };

            if (Utc::now() - context.start_time) >= context.timeout {
                info!("Search timeout reached");
                self.active_searches.lock().await.remove(&search_id);
                let _ = self.delete_search(&search_id).await;
                return Ok((vec![], false, SearchState::Completed));
            }

            let endpoint = format!("searches/{}/responses", search_id);
            match self
                .make_request::<Vec<SearchResponse>, ()>(Method::GET, &endpoint, None)
                .await
            {
                Ok(current_responses) => {
                    let total_len = current_responses.len();

                    if total_len > context.seen_response_count {
                        // Update seen count
                        {
                            let mut guard = self.active_searches.lock().await;
                            if let Some(ctx) = guard.get_mut(&search_id) {
                                ctx.seen_response_count = total_len;
                            }
                        }

                        let track_titles_ref: Vec<&str> =
                            context.track_titles.iter().map(|s| s.as_str()).collect();
                        let mut albums = processing::process_search_responses(
                            &current_responses,
                            &context.artist,
                            context.album.as_deref(),
                            &track_titles_ref,
                        );

                        albums.sort_by(|a, b| {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });

                        if albums.len() > MAX_SEARCH_RESULTS {
                            albums.truncate(MAX_SEARCH_RESULTS);
                            self.active_searches.lock().await.remove(&search_id);
                            let _ = self.delete_search(&search_id).await;
                            return Ok((albums, false, SearchState::Completed));
                        } else {
                            return Ok((albums, true, SearchState::InProgress));
                        }
                    } else {
                        // No new data
                        if (Utc::now() - poll_start) > long_poll_timeout {
                            // Long poll expired, return "no update" but "in progress"
                            return Ok((vec![], true, SearchState::InProgress));
                        }

                        // Wait a bit before retrying slskd
                        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                        continue;
                    }
                }
                Err(SoulseekError::Api { status: 404, .. }) => {
                    self.active_searches.lock().await.remove(&search_id);
                    info!("Search 404");
                    return Ok((vec![], false, SearchState::NotFound));
                }
                Err(e) => {
                    // Clean up search context on any error to prevent leaks
                    self.active_searches.lock().await.remove(&search_id);
                    let _ = self.delete_search(&search_id).await;
                    warn!("Search {} failed with error, cleaning up: {}", search_id, e);
                    return Err(e);
                }
            }
        }
    }

    pub async fn download(&self, req: Vec<TrackResult>) -> Result<Vec<DownloadResponse>> {
        let mut requests_by_username: HashMap<String, Vec<DownloadRequestFile>> = HashMap::new();

        info!("Attempting to download: {} files...", req.len());

        for req in req {
            let list = requests_by_username.entry(req.base.username).or_default();
            if !list.iter().any(|f| f.filename == req.base.filename) {
                list.push(DownloadRequestFile {
                    filename: req.base.filename,
                    size: req.base.size,
                });
            }
        }

        let mut results = Vec::new();
        let config = &self.download_config;

        for (username, file_requests) in requests_by_username {
            let batches: Vec<_> = file_requests
                .chunks(config.batch_size)
                .map(|c| c.to_vec())
                .collect();

            info!(
                "Downloading {} files from '{}' in {} batches (batch size: {})",
                file_requests.len(),
                username,
                batches.len(),
                config.batch_size
            );

            for (batch_idx, batch) in batches.into_iter().enumerate() {
                if batch_idx > 0 {
                    debug!("Waiting {}ms before next batch", config.batch_delay_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(config.batch_delay_ms))
                        .await;
                }

                let batch_results = self
                    .download_batch_with_retry(&username, batch, batch_idx)
                    .await;
                results.extend(batch_results);
            }
        }

        Ok(results)
    }

    async fn download_batch_with_retry(
        &self,
        username: &str,
        batch: Vec<DownloadRequestFile>,
        batch_idx: usize,
    ) -> Vec<DownloadResponse> {
        let config = &self.download_config;
        // Cap exponential backoff at 30 seconds to prevent excessive waits
        const MAX_BACKOFF_MS: u64 = 30_000;

        let mut last_error: Option<SoulseekError> = None;

        for attempt in 0..=config.max_retries {
            if attempt > 0 {
                let delay = std::cmp::min(
                    config.retry_base_delay_ms * (1 << (attempt - 1)),
                    MAX_BACKOFF_MS,
                );
                warn!(
                    "Retrying batch {} for '{}' (attempt {}/{}), waiting {}ms",
                    batch_idx, username, attempt, config.max_retries, delay
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
            }

            match self.send_download_batch(username, &batch, batch_idx).await {
                Ok(responses) => return responses,
                Err(e) => {
                    // Log every error, not just the final one
                    warn!(
                        "Batch {} for '{}' attempt {} failed: {}",
                        batch_idx, username, attempt, e
                    );
                    last_error = Some(e);
                }
            }
        }

        // All retries exhausted - return error responses for all files in batch
        let error_msg = last_error
            .map(|e| format!("Failed after {} retries: {}", config.max_retries, e))
            .unwrap_or_else(|| format!("Failed after {} retries", config.max_retries));

        warn!(
            "Batch {} for '{}' failed after all retries: {}",
            batch_idx, username, error_msg
        );

        batch
            .iter()
            .map(|f| DownloadResponse {
                username: username.to_string(),
                filename: f.filename.clone(),
                size: f.size as u64,
                error: Some(error_msg.clone()),
            })
            .collect()
    }

    async fn send_download_batch(
        &self,
        username: &str,
        batch: &[DownloadRequestFile],
        batch_idx: usize,
    ) -> Result<Vec<DownloadResponse>> {
        let endpoint = format!("transfers/downloads/{username}");
        let url = self.base_url.join(&format!("api/v0/{endpoint}"))?;

        info!(
            "Sending batch {} to '{}': {} files",
            batch_idx,
            username,
            batch.len()
        );
        debug!("Batch payload: {:?}", batch);

        let response = self
            .client
            .post(url)
            .header("X-API-Key", self.api_key.as_deref().unwrap_or(""))
            .json(&batch)
            .send()
            .await?;

        let status = response.status();
        let resp_text = response.text().await?;

        info!(
            "Batch {} response: status={}, body_len={}",
            batch_idx,
            status,
            resp_text.len()
        );

        if !status.is_success() {
            // Handle "already in progress" as success - the file is already queued
            if status.as_u16() == 500 && resp_text.contains("already in progress") {
                info!(
                    "Batch {} for '{}': files already queued (treating as success)",
                    batch_idx, username
                );
                return Ok(batch
                    .iter()
                    .map(|f| DownloadResponse {
                        username: username.to_string(),
                        filename: f.filename.clone(),
                        size: f.size as u64,
                        error: None, // No error - already queued is fine
                    })
                    .collect());
            }

            return Err(SoulseekError::Api {
                status: status.as_u16(),
                message: resp_text,
            });
        }

        Ok(self.parse_download_response(username, batch, &resp_text))
    }

    fn parse_download_response(
        &self,
        username: &str,
        batch: &[DownloadRequestFile],
        resp_text: &str,
    ) -> Vec<DownloadResponse> {
        // Log the raw response for debugging (truncate if too long)
        let log_text = if resp_text.len() > 500 {
            format!("{}... (truncated, {} bytes total)", &resp_text[..500], resp_text.len())
        } else {
            resp_text.to_string()
        };
        debug!("slskd download response: '{}'", log_text);

        // slskd API returns { "enqueued": N, "failed": N } as counts
        #[derive(Deserialize, Debug)]
        struct SlskdCountResponse {
            enqueued: Option<i32>,
            failed: Option<i32>,
        }

        // Some versions may return file details
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct SlskdDownloadResponse {
            filename: String,
        }

        #[derive(Deserialize, Debug)]
        struct SlskdBatchResponse {
            enqueued: Vec<SlskdDownloadResponse>,
            #[serde(default)]
            failed: Vec<serde_json::Value>,
        }

        // Helper to find file size from batch
        let find_size = |filename: &str| -> u64 {
            batch
                .iter()
                .find(|f| f.filename == filename)
                .map(|f| f.size as u64)
                .unwrap_or(0)
        };

        // Helper to create success response for all batch files
        let all_success = || -> Vec<DownloadResponse> {
            batch
                .iter()
                .map(|f| DownloadResponse {
                    username: username.to_string(),
                    filename: f.filename.clone(),
                    size: f.size as u64,
                    error: None,
                })
                .collect()
        };

        // Helper to create error response for all batch files
        let all_error = |msg: &str| -> Vec<DownloadResponse> {
            batch
                .iter()
                .map(|f| DownloadResponse {
                    username: username.to_string(),
                    filename: f.filename.clone(),
                    size: f.size as u64,
                    error: Some(msg.to_string()),
                })
                .collect()
        };

        // Empty response with 2xx status = success
        if resp_text.trim().is_empty() {
            info!(
                "Empty success response from slskd, assuming {} files queued",
                batch.len()
            );
            return all_success();
        }

        // Try to parse as JSON first
        let json_value: std::result::Result<serde_json::Value, _> = serde_json::from_str(resp_text);

        if json_value.is_err() {
            // Not valid JSON - check for known text responses
            let lower = resp_text.to_lowercase();

            if lower.contains("already") && (lower.contains("queue") || lower.contains("progress")) {
                info!("slskd reports files already queued (text response)");
                return all_success();
            }

            if lower.contains("error") || lower.contains("failed") {
                warn!("slskd returned error text: {}", resp_text);
                return all_error(&format!("slskd error: {}", resp_text));
            }

            // Unknown text response - log and assume success for 2xx
            warn!(
                "Unexpected non-JSON response from slskd: '{}'. Assuming success.",
                resp_text
            );
            return all_success();
        }

        // Parse JSON responses in order of likelihood

        // Try count response first (most common for slskd)
        if let Ok(count_resp) = serde_json::from_str::<SlskdCountResponse>(resp_text) {
            // Only process if we got at least one of the fields
            if count_resp.enqueued.is_some() || count_resp.failed.is_some() {
                let enqueued = count_resp.enqueued.unwrap_or(0) as usize;
                let failed = count_resp.failed.unwrap_or(0) as usize;

                info!(
                    "slskd count response: {} enqueued, {} failed (batch: {} files)",
                    enqueued, failed, batch.len()
                );

                // Validate the counts make sense
                if enqueued + failed > batch.len() * 2 {
                    warn!(
                        "slskd count response doesn't match batch size, enqueued={} failed={} batch={}",
                        enqueued, failed, batch.len()
                    );
                }

                // All succeeded
                if enqueued >= batch.len() && failed == 0 {
                    return all_success();
                }

                // All failed
                if failed >= batch.len() && enqueued == 0 {
                    return all_error("All files failed to enqueue");
                }

                // Partial success - we can't know which files failed without detailed response
                // Mark all as success but log the partial failure
                if enqueued > 0 && failed > 0 {
                    warn!(
                        "Partial enqueue: {} succeeded, {} failed. Cannot determine which files failed.",
                        enqueued, failed
                    );
                    // Return success for all since we can't distinguish
                    return all_success();
                }

                // Some were enqueued
                if enqueued > 0 {
                    return all_success();
                }

                // Default to error if nothing was enqueued
                return all_error("No files were enqueued");
            }
        }

        // Try batch response with arrays (detailed response)
        if let Ok(batch_resp) = serde_json::from_str::<SlskdBatchResponse>(resp_text) {
            if !batch_resp.enqueued.is_empty() || !batch_resp.failed.is_empty() {
                let mut results: Vec<DownloadResponse> = batch_resp
                    .enqueued
                    .into_iter()
                    .map(|d| DownloadResponse {
                        username: username.to_string(),
                        filename: d.filename.clone(),
                        size: find_size(&d.filename),
                        error: None,
                    })
                    .collect();

                for failed in batch_resp.failed {
                    let (filename, error_msg) = if let Some(s) = failed.as_str() {
                        (s.to_string(), "Download failed".to_string())
                    } else if let Some(obj) = failed.as_object() {
                        // Try to extract filename and error from object
                        let fname = obj
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let err = obj
                            .get("error")
                            .or_else(|| obj.get("message"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("Download failed")
                            .to_string();
                        (fname, err)
                    } else {
                        warn!("Unexpected failed item format: {}", failed);
                        continue;
                    };

                    results.push(DownloadResponse {
                        username: username.to_string(),
                        filename: filename.clone(),
                        size: find_size(&filename),
                        error: Some(error_msg),
                    });
                }

                if !results.is_empty() {
                    return results;
                }
            }
        }

        // Try array of files
        if let Ok(multi) = serde_json::from_str::<Vec<SlskdDownloadResponse>>(resp_text) {
            if !multi.is_empty() {
                return multi
                    .into_iter()
                    .map(|d| DownloadResponse {
                        username: username.to_string(),
                        filename: d.filename.clone(),
                        size: find_size(&d.filename),
                        error: None,
                    })
                    .collect();
            }
        }

        // Try single file response
        if let Ok(single) = serde_json::from_str::<SlskdDownloadResponse>(resp_text) {
            return vec![DownloadResponse {
                username: username.to_string(),
                filename: single.filename.clone(),
                size: find_size(&single.filename),
                error: None,
            }];
        }

        // Could not parse response - log warning but don't fail
        // Since we got a 2xx status, assume the operation succeeded
        warn!(
            "Could not parse slskd response format: '{}'. Assuming success for {} files.",
            log_text,
            batch.len()
        );
        all_success()
    }

    pub async fn get_all_downloads(&self) -> Result<Vec<FileEntry>> {
        let flattened: FlattenedFiles = self
            .make_request(Method::GET, "transfers/downloads", None::<()>)
            .await?;
        Ok(flattened.0)
    }

    pub async fn cancel_download(
        &self,
        username: &str,
        download_id: &str,
        remove: bool,
    ) -> Result<()> {
        let endpoint = format!("transfers/downloads/{username}/{download_id}?remove={remove}");
        info!("Cancelling download: {}", download_id);
        self.make_request(Method::DELETE, &endpoint, None::<()>)
            .await
    }

    pub async fn clear_all_completed_downloads(&self) -> Result<()> {
        info!("Clearing all completed downloads");
        self.make_request(
            Method::DELETE,
            "transfers/downloads/all/completed",
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

    pub async fn check_connection(&self) -> bool {
        self.make_request::<serde_json::Value, ()>(Method::GET, "session", None)
            .await
            .is_ok()
    }
}
