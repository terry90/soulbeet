use super::processing;
use crate::{
    error::{Result, SoulseekError},
    http::{resolve_docker_url, CircuitBreaker},
    slskd::models::{
        BatchEnqueueOptions, BatchEnqueueRequest, BatchEnqueueResponse, DownloadRequestFile,
        SearchResponse,
    },
};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
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
                        // No new data. slskd ends a search a short while after
                        // responses stop arriving; once it reports completion
                        // there is nothing more to wait for, so return what we
                        // have instead of spinning until our own 120s timeout.
                        if self.is_search_complete(&search_id).await {
                            self.active_searches.lock().await.remove(&search_id);
                            let _ = self.delete_search(&search_id).await;

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
                            albums.truncate(MAX_SEARCH_RESULTS);

                            info!("Search {} completed on slskd side", search_id);
                            return Ok((albums, false, SearchState::Completed));
                        }

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
        // Group by (username, destination) so every slskd batch can carry one
        // explicit destination directory for all of its files.
        let mut groups: HashMap<(String, Option<String>), Vec<DownloadRequestFile>> =
            HashMap::new();

        info!("Attempting to download: {} files...", req.len());

        for req in req {
            let destination = enqueue_destination(&req.base.filename);
            let list = groups.entry((req.base.username, destination)).or_default();
            if !list.iter().any(|f| f.filename == req.base.filename) {
                list.push(DownloadRequestFile {
                    filename: req.base.filename,
                    size: req.base.size,
                });
            }
        }

        let mut results = Vec::new();
        let config = &self.download_config;

        for ((username, destination), file_requests) in groups {
            let batches: Vec<_> = file_requests
                .chunks(config.batch_size)
                .map(|c| c.to_vec())
                .collect();

            info!(
                "Downloading {} files from '{}' into {:?} in {} batches (batch size: {})",
                file_requests.len(),
                username,
                destination,
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
                    .download_batch_with_retry(&username, destination.as_deref(), batch, batch_idx)
                    .await;
                results.extend(batch_results);
            }
        }

        Ok(results)
    }

    async fn download_batch_with_retry(
        &self,
        username: &str,
        destination: Option<&str>,
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

            match self
                .send_download_batch(username, destination, &batch, batch_idx)
                .await
            {
                Ok(responses) => return responses,
                Err(e) => {
                    warn!(
                        "Batch {} for '{}' attempt {} failed: {}",
                        batch_idx, username, attempt, e
                    );
                    // Stop retrying for non-retryable errors (e.g. user offline)
                    if !e.is_retryable() {
                        warn!(
                            "Batch {} for '{}': error is non-retryable, stopping",
                            batch_idx, username
                        );
                        return batch
                            .iter()
                            .map(|f| DownloadResponse {
                                username: username.to_string(),
                                filename: f.filename.clone(),
                                size: f.size as u64,
                                error: Some(e.to_string()),
                            })
                            .collect();
                    }
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
        destination: Option<&str>,
        batch: &[DownloadRequestFile],
        batch_idx: usize,
    ) -> Result<Vec<DownloadResponse>> {
        let url = self.base_url.join("api/v0/transfers/downloads/batches")?;

        info!(
            "Sending batch {} to '{}': {} files",
            batch_idx,
            username,
            batch.len()
        );
        debug!("Batch payload: {:?}", batch);

        let request = BatchEnqueueRequest {
            username,
            files: batch,
            options: BatchEnqueueOptions {
                destination: destination.map(str::to_string),
            },
        };

        let response = self
            .client
            .post(url)
            .header("X-API-Key", self.api_key.as_deref().unwrap_or(""))
            .json(&request)
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
            // slskd answers 404 with this text for offline users, and
            // deliberately for blacklisted ones too. Matched on any status so
            // classification survives a future status-code change.
            if resp_text.to_lowercase().contains("appears to be offline") {
                warn!("User '{}' is offline, not retrying", username);
                return Err(SoulseekError::UserOffline {
                    username: username.to_string(),
                });
            }

            // The batches route only exists since slskd 0.26.
            if status.as_u16() == 404 {
                return Err(SoulseekError::Api {
                    status: 404,
                    message: format!(
                        "transfers/downloads/batches not found; slskd 0.26 or newer is required ({resp_text})"
                    ),
                });
            }

            return Err(SoulseekError::Api {
                status: status.as_u16(),
                message: resp_text,
            });
        }

        parse_batch_enqueue_response(username, batch, &resp_text)
    }

    pub async fn get_all_downloads(&self) -> Result<Vec<FileEntry>> {
        let flattened: FlattenedFiles = self
            .make_request(Method::GET, "transfers/downloads", None::<()>)
            .await?;
        if !flattened.errors.is_empty() {
            if flattened.files.is_empty() {
                // Every entry failed to parse: schema drift, not an empty
                // queue. Returning Ok([]) here is what made slskd 0.26 look
                // like downloads never appeared (#73).
                return Err(SoulseekError::InvalidResponse(format!(
                    "all {} transfer entries failed to parse; first error: {}",
                    flattened.errors.len(),
                    flattened.errors[0]
                )));
            }
            warn!(
                "Skipped {} unparseable slskd transfer entries; first error: {}",
                flattened.errors.len(),
                flattened.errors[0]
            );
        }
        Ok(flattened.files)
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

    /// Whether slskd reports the search as finished. A search that slskd no
    /// longer knows about (404) counts as finished too. Errors are treated as
    /// "not complete" so polling falls back to its own timeout.
    async fn is_search_complete(&self, search_id: &str) -> bool {
        #[derive(Deserialize)]
        struct SearchStatus {
            #[serde(rename = "isComplete", default)]
            is_complete: bool,
            #[serde(default)]
            state: String,
        }

        let endpoint = format!("searches/{search_id}");
        match self
            .make_request::<SearchStatus, ()>(Method::GET, &endpoint, None)
            .await
        {
            Ok(status) => status.is_complete || status.state.contains("Completed"),
            Err(SoulseekError::Api { status: 404, .. }) => true,
            Err(e) => {
                debug!("Search state check failed for {}: {}", search_id, e);
                false
            }
        }
    }

    /// Check both slskd connectivity and Soulseek network connection.
    ///
    /// Returns Ok(()) if slskd is reachable and connected to Soulseek.
    /// Returns Err with a descriptive message if either check fails.
    pub async fn check_connection(&self) -> std::result::Result<(), String> {
        // First verify slskd is reachable (session endpoint)
        if let Err(e) = self
            .make_request::<serde_json::Value, ()>(Method::GET, "session", None)
            .await
        {
            return Err(match e {
                SoulseekError::Api {
                    status: status @ (401 | 403),
                    ..
                } => format!(
                    "slskd rejected the API key (HTTP {status}). Check the API key value, its role, \
                     and any CIDR restriction in slskd's web.authentication.api_keys config."
                ),
                other => format!("Cannot reach slskd (session endpoint failed: {other})"),
            });
        }

        // Then verify Soulseek network connection via application state
        #[derive(Deserialize)]
        struct ServerState {
            #[serde(rename = "isConnected")]
            is_connected: bool,
            #[serde(rename = "isLoggedIn")]
            is_logged_in: bool,
        }

        #[derive(Deserialize)]
        struct AppState {
            server: ServerState,
        }

        match self
            .make_request::<AppState, ()>(Method::GET, "application", None)
            .await
        {
            Ok(app_state) => {
                if !app_state.server.is_connected {
                    return Err(
                        "slskd is running but not connected to the Soulseek network. \
                         Check slskd's connection settings or restart slskd."
                            .to_string(),
                    );
                }
                if !app_state.server.is_logged_in {
                    return Err("slskd is connected to Soulseek but not logged in. \
                         Check slskd's Soulseek username and password."
                        .to_string());
                }
                Ok(())
            }
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

/// The explicit slskd batch destination for a remote file: the sanitized name
/// of its parent directory. Pinning it at enqueue time puts the file at
/// `<downloads>/<destination>/<file>` regardless of the server's
/// `transfers.download.destination.subdirectory` pattern, which is exactly
/// where `resolve_download_path`'s primary strategy looks.
fn enqueue_destination(filename: &str) -> Option<String> {
    let normalized = filename.replace('\\', "/");
    let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if components.len() >= 2 {
        // slskd rejects an empty destination, so a bare filename sends none
        // and falls back to the server-side pattern.
        Some(shared::slskd::sanitize_filename(
            components[components.len() - 2],
        ))
    } else {
        None
    }
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
            let enqueued = parsed
                .batch
                .transfers
                .iter()
                .any(|t| t.filename == file.filename);
            let failure = parsed
                .failures
                .iter()
                .find(|f| f.filename == file.filename);

            let error = if enqueued {
                None
            } else if let Some(failure) = failure {
                // slskd reports an already-queued file as a per-file failure,
                // but for Soulful that transfer is live and will be monitored.
                if failure.message.contains("Already in progress") {
                    None
                } else {
                    Some(failure.message.clone())
                }
            } else {
                Some("Not reported in slskd enqueue response".to_string())
            };

            DownloadResponse {
                username: username.to_string(),
                filename: file.filename.clone(),
                size: file.size as u64,
                error,
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
        let (results, has_more, state) = self.poll_search(search_id.to_string()).await?;
        Ok(shared::download::SearchResult {
            search_id: search_id.to_string(),
            groups: results.into_iter().map(Into::into).collect(),
            has_more,
            state: state.into(),
        })
    }

    async fn download(
        &self,
        items: Vec<shared::download::DownloadableItem>,
    ) -> Result<Vec<shared::download::QueuedDownload>> {
        let tracks: Vec<TrackResult> = items
            .into_iter()
            .filter_map(|item| item.to_slskd_track())
            .collect();

        let responses = self.download(tracks).await?;
        Ok(responses.into_iter().map(Into::into).collect())
    }

    async fn get_downloads(&self) -> Result<Vec<shared::download::DownloadProgress>> {
        let entries = self.get_all_downloads().await?;
        Ok(entries.into_iter().map(Into::into).collect())
    }

    async fn cancel_download(
        &self,
        username: &str,
        download_id: &str,
        remove: bool,
    ) -> Result<()> {
        self.cancel_download(username, download_id, remove).await
    }

    async fn health_check(&self) -> bool {
        match self.check_connection().await {
            Ok(()) => true,
            Err(msg) => {
                warn!("Health check failed: {}", msg);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// transfers attached, failures as real {filename, message} records.
    fn body(transfers: &[&str], failures: &[(&str, &str)]) -> String {
        let transfers: Vec<_> = transfers
            .iter()
            .map(|f| serde_json::json!({ "id": "11111111-2222-3333-4444-555555555555", "filename": f }))
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
            &body(&[r"a\album\1.mp3", r"a\album\2.mp3"], &[]),
        )
        .unwrap();
        assert!(results.iter().all(|r| r.error.is_none()));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn partial_failure_attributes_the_right_file() {
        let files = batch(&[r"a\album\1.mp3", r"a\album\2.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(
                &[r"a\album\1.mp3"],
                &[(r"a\album\2.mp3", "Error: something broke")],
            ),
        )
        .unwrap();
        assert!(results[0].error.is_none());
        assert_eq!(results[1].filename, r"a\album\2.mp3");
        assert_eq!(results[1].error.as_deref(), Some("Error: something broke"));
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
        assert_eq!(results[0].error.as_deref(), Some("Error: rejected"));
    }

    #[test]
    fn already_in_progress_is_success() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response(
            "peer",
            &files,
            &body(&[], &[(r"a\album\1.mp3", "Skipped: Already in progress")]),
        )
        .unwrap();
        assert!(results[0].error.is_none());
    }

    #[test]
    fn file_missing_from_response_is_an_error() {
        let files = batch(&[r"a\album\1.mp3"]);
        let results = parse_batch_enqueue_response("peer", &files, &body(&[], &[])).unwrap();
        assert_eq!(
            results[0].error.as_deref(),
            Some("Not reported in slskd enqueue response")
        );
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
        assert_eq!(results[0].error.as_deref(), Some("Error: x"));
    }

    #[test]
    fn unparseable_body_is_invalid_response() {
        let files = batch(&[r"a\album\1.mp3"]);
        let err = parse_batch_enqueue_response("peer", &files, "<html>gateway</html>").unwrap_err();
        assert!(matches!(err, SoulseekError::InvalidResponse(_)));
    }

    #[test]
    fn destination_is_the_sanitized_parent_directory() {
        assert_eq!(
            enqueue_destination(r"@@abcde\Music\Artist - Album\01 - track.mp3"),
            Some("Artist - Album".to_string())
        );
        assert_eq!(
            enqueue_destination(r"music\AC: DC\1.mp3"),
            Some("AC_ DC".to_string())
        );
        assert_eq!(enqueue_destination("loose_track.mp3"), None);
    }
}
