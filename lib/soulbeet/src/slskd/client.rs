use super::processing;
use crate::{
    error::{Result, SoulseekError},
    slskd::models::{DownloadRequestFile, SearchResponse},
};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
    musicbrainz::Track,
    slskd::{AlbumResult, DownloadResponse, FileEntry, FlattenedFiles, SearchState, TrackResult},
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::sync::Mutex;
use tracing::{debug, info};
use url::Url;

const MAX_SEARCH_RESULTS: usize = 50;

#[derive(Debug, Clone)]
struct SearchContext {
    artist: String,
    album: String,
    track_titles: Vec<String>,
    start_time: DateTime<Utc>,
    timeout: Duration,
    seen_response_count: usize,
}

#[derive(Debug, Clone)]
pub struct SoulseekClient {
    base_url: Url,
    api_key: Option<String>,
    client: Client,
    search_timestamps: Arc<Mutex<Vec<DateTime<Utc>>>>,
    active_searches: Arc<Mutex<HashMap<String, SearchContext>>>,
    max_searches_per_window: usize,
    rate_limit_window: Duration,
}

#[derive(Default)]
pub struct SoulseekClientBuilder {
    base_url: Option<String>,
    api_key: Option<String>,
    max_searches_per_window: Option<usize>,
    rate_limit_window_seconds: Option<i64>,
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

    pub fn build(self) -> Result<SoulseekClient> {
        let base_url_str = self.base_url.ok_or(SoulseekError::NotConfigured)?;
        let base_url = Url::parse(base_url_str.trim_end_matches('/'))?;

        Ok(SoulseekClient {
            base_url,
            api_key: self.api_key,
            client: Client::new(),
            search_timestamps: Arc::new(Mutex::new(Vec::new())),
            active_searches: Arc::new(Mutex::new(HashMap::new())),
            max_searches_per_window: self.max_searches_per_window.unwrap_or(35),
            rate_limit_window: Duration::seconds(self.rate_limit_window_seconds.unwrap_or(220)),
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
        let url = self.base_url.join(&format!("api/v0/{endpoint}"))?;
        debug!("Request: {} {}", method, url);
        let mut request = self.client.request(method, url);
        if let Some(key) = &self.api_key {
            request = request.header("X-API-Key", key);
        }
        if let Some(b) = body {
            request = request.json(&b);
        }
        let response = request.send().await?;
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
        artist: String,
        album: String,
        tracks: Vec<Track>,
        timeout: Duration,
    ) -> Result<String> {
        self.wait_for_rate_limit().await?;

        let track_titles: Vec<String> = tracks.iter().map(|t| t.title.clone()).collect();

        let query = match tracks.len() {
            1 => format!("{} {}", artist.trim(), tracks[0].title.trim()),
            _ => format!("{} {}", artist.trim(), album.trim()),
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
                artist,
                album,
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
                            &context.album,
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
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn download(&self, req: Vec<TrackResult>) -> Result<Vec<DownloadResponse>> {
        let mut requests_by_username: HashMap<String, Vec<DownloadRequestFile>> = HashMap::new();

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

        info!("Attempting to download: {} files...", req.len());
        for req in req {
            let list = requests_by_username.entry(req.base.username).or_default();
            list.push(DownloadRequestFile {
                filename: req.base.filename,
                size: req.base.size,
            });
        }

        let mut res = vec![];

        for (username, file_requests) in requests_by_username.into_iter() {
            let endpoint = format!("transfers/downloads/{username}");
            let url = self.base_url.join(&format!("api/v0/{endpoint}"))?;

            info!(
                "Sending download request to {} with {} files",
                url,
                file_requests.len()
            );
            debug!(
                "Payload: {:?}",
                serde_json::to_string(&file_requests).unwrap_or_default()
            );

            let response = self
                .client
                .post(url)
                .header("X-API-Key", self.api_key.as_deref().unwrap_or(""))
                .json(&file_requests)
                .send()
                .await?;

            let status = response.status();
            let resp_text = response.text().await?;

            if !status.is_success() {
                tracing::error!(
                    "Slskd returned error status: {} - Body: {}",
                    status,
                    resp_text
                );
                for req_file in &file_requests {
                    res.push(DownloadResponse {
                        username: username.clone(),
                        filename: req_file.filename.clone(),
                        size: req_file.size as u64,
                        error: Some(resp_text.clone()),
                    });
                }
                continue;
            }

            if resp_text.trim().is_empty() {
                info!("Slskd returned empty success response. Assuming files queued.");
                for req_file in &file_requests {
                    res.push(DownloadResponse {
                        username: username.clone(),
                        filename: req_file.filename.clone(),
                        size: req_file.size as u64,
                        error: None,
                    });
                }
                // TODO: Check slskd response
            } else if let Ok(single_res) = serde_json::from_str::<SlskdDownloadResponse>(&resp_text)
            {
                let size = file_requests
                    .iter()
                    .find(|f| f.filename == single_res.filename)
                    .map(|f| f.size)
                    .unwrap_or(0);
                res.push(DownloadResponse {
                    username: username.clone(),
                    filename: single_res.filename,
                    size: size as u64,
                    error: None,
                });
            } else if let Ok(multi_res) =
                serde_json::from_str::<Vec<SlskdDownloadResponse>>(&resp_text)
            {
                res.extend(multi_res.into_iter().map(|d| {
                    let size = file_requests
                        .iter()
                        .find(|f| f.filename == d.filename)
                        .map(|f| f.size)
                        .unwrap_or(0);
                    DownloadResponse {
                        username: username.clone(),
                        filename: d.filename,
                        size: size as u64,
                        error: None,
                    }
                }));
            } else if let Ok(batch_res) = serde_json::from_str::<SlskdBatchResponse>(&resp_text) {
                res.extend(batch_res.enqueued.into_iter().map(|d| {
                    let size = file_requests
                        .iter()
                        .find(|f| f.filename == d.filename)
                        .map(|f| f.size)
                        .unwrap_or(0);
                    DownloadResponse {
                        username: username.clone(),
                        filename: d.filename,
                        size: size as u64,
                        error: None,
                    }
                }));
                for failed_item in batch_res.failed {
                    let (filename_opt, error_msg) = if let Some(s) = failed_item.as_str() {
                        (Some(s.to_string()), "Download failed".to_string())
                    } else {
                        tracing::warn!("Slskd reported failed is not a string: {}", failed_item);
                        continue;
                    };

                    if let Some(filename) = filename_opt {
                        let size = file_requests
                            .iter()
                            .find(|f| f.filename == filename)
                            .map(|f| f.size)
                            .unwrap_or(0);

                        res.push(DownloadResponse {
                            username: username.clone(),
                            filename,
                            size: size as u64,
                            error: Some(error_msg),
                        });
                    } else {
                        tracing::warn!(
                            "Slskd reported failed download without filename: {}",
                            failed_item
                        );
                    }
                }
            } else {
                tracing::error!("Failed to parse response from slskd: '{}'", resp_text);
            }
        }

        Ok(res)
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
