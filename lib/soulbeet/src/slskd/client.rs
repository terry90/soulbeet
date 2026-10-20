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
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
    download::EnqueueStatus,
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

    /// Enqueue `req` into `destination`, a folder under slskd's download
    /// directory that every file of the call shares.
    pub async fn download(
        &self,
        req: Vec<TrackResult>,
        destination: &str,
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
        for batch in plan_batches(wanted, destination, config.batch_size) {
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

/// Everything but unreserved characters, `.` included. Soulseek usernames
/// are free-form, so a peer called `a/b`, `a\b` or `x?y` would otherwise
/// re-point the request at a different route.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'~');

/// slskd's exact answer for a file it already has an unfinished transfer
/// of (`DownloadService.EnqueueAsync`).
const ALREADY_IN_PROGRESS: &str = "Skipped: Already in progress";

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

fn user_downloads_endpoint(username: &str) -> Result<String> {
    Ok(format!("transfers/downloads/{}", path_segment(username)?))
}

fn transfer_endpoint(username: &str, id: &str, remove: bool) -> Result<String> {
    Ok(format!(
        "{}/{}?remove={remove}",
        user_downloads_endpoint(username)?,
        path_segment(id)?
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
/// their first file, and a file asked for twice is sent once.
fn plan_batches(
    wanted: Vec<(String, DownloadRequestFile)>,
    destination: &str,
    batch_size: usize,
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
                .map(|(index, chunk)| PendingBatch {
                    id: uuid::Uuid::new_v4().to_string(),
                    username: username.clone(),
                    destination: destination.to_string(),
                    files: chunk.to_vec(),
                    index,
                })
                .collect::<Vec<_>>()
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

    let mut last_error: Option<SoulseekError> = None;

    for attempt in 0..=config.max_retries {
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

        match send(batch).await {
            Ok(responses) => return responses,
            Err(e) => {
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
                    return batch.responses(EnqueueStatus::Failed(e.to_string()));
                }
                last_error = Some(e);
            }
        }
    }

    let error_msg = last_error
        .map(|e| format!("Failed after {} retries: {}", config.max_retries, e))
        .unwrap_or_else(|| format!("Failed after {} retries", config.max_retries));

    warn!(
        "Batch {} for '{}' failed after all retries: {}",
        batch.index, batch.username, error_msg
    );

    batch.responses(EnqueueStatus::Failed(error_msg))
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
                    None => EnqueueStatus::Failed(
                        "Not found in slskd after a retried enqueue".to_string(),
                    ),
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
                    EnqueueStatus::AlreadyInFlight
                }
                (None, Some(f)) => EnqueueStatus::Failed(f.message.clone()),
                (None, None) => {
                    EnqueueStatus::Failed("Not reported in slskd enqueue response".to_string())
                }
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
        destination: &str,
    ) -> Result<Vec<shared::download::QueuedDownload>> {
        let tracks: Vec<TrackResult> = items
            .into_iter()
            .filter_map(|item| item.to_slskd_track())
            .collect();

        let responses = self.download(tracks, destination).await?;
        Ok(responses.into_iter().map(Into::into).collect())
    }

    async fn get_downloads_for(
        &self,
        sources: &[String],
    ) -> Result<Vec<shared::download::DownloadProgress>> {
        let entries = self.get_downloads_for_users(sources).await?;
        Ok(entries.into_iter().map(Into::into).collect())
    }

    async fn get_downloads(&self) -> Result<Vec<shared::download::DownloadProgress>> {
        let entries = self.get_all_downloads().await?;
        Ok(entries.into_iter().map(Into::into).collect())
    }

    async fn cancel_download(&self, username: &str, download_id: &str, remove: bool) -> Result<()> {
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
    use shared::download::EnqueueStatus;

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
        assert_eq!(
            results[1].status,
            EnqueueStatus::Failed("Error: something broke".into())
        );
        assert_eq!(results[1].transfer_id, None);
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
        assert_eq!(
            results[0].status,
            EnqueueStatus::Failed("Error: rejected".into())
        );
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
        assert_eq!(results[0].status, EnqueueStatus::AlreadyInFlight);
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
        assert_eq!(
            results[0].status,
            EnqueueStatus::Failed("Already in progress".into())
        );
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
        assert_eq!(
            results[0].status,
            EnqueueStatus::Failed("Not reported in slskd enqueue response".into())
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
        assert_eq!(results[0].status, EnqueueStatus::Failed("Error: x".into()));
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
        let batches = plan_batches(vec![wanted("peer", r"music\A\01.flac")], "A [1]", 3);
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
        assert!(matches!(results[1].status, EnqueueStatus::Failed(_)));
        assert_eq!(results[1].transfer_id, None);
    }
}
