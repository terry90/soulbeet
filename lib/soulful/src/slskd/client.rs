use crate::{
    error::{Result, SoulseekError},
    slskd::models::{
        AlbumResult, DownloadRequestFile, DownloadStatus, SearchResponse, SearchResult, TrackResult,
    },
};
use chrono::{DateTime, Duration, Utc};
use itertools::Itertools;
use regex::Regex;
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct SoulseekClient {
    base_url: Url,
    api_key: Option<String>,
    download_path: PathBuf,
    client: Client,
    search_timestamps: Arc<Mutex<Vec<DateTime<Utc>>>>,
    active_searches: Arc<Mutex<HashSet<String>>>,
    max_searches_per_window: usize,
    rate_limit_window: Duration,
}

#[derive(Default)]
pub struct SoulseekClientBuilder {
    base_url: Option<String>,
    api_key: Option<String>,
    download_path: Option<PathBuf>,
    max_searches_per_window: Option<usize>,
    rate_limit_window_seconds: Option<i64>,
}

impl SoulseekClientBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn base_url(mut self, url: &str) -> Self {
        // Simple Docker host resolution
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

    pub fn download_path(mut self, path: &str) -> Self {
        self.download_path = Some(PathBuf::from(path));
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

        let download_path = self
            .download_path
            .unwrap_or_else(|| PathBuf::from("./downloads"));

        Ok(SoulseekClient {
            base_url,
            api_key: self.api_key,
            download_path,
            client: Client::new(),
            search_timestamps: Arc::new(Mutex::new(Vec::new())),
            active_searches: Arc::new(Mutex::new(HashSet::new())),
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
                // For 204 No Content, deserialize from a JSON null
                serde_json::from_str("null").map_err(|e| {
                    error!("Deserialization error for empty body: {e}");
                    SoulseekError::Api {
                        status: status.as_u16(),
                        message: format!("JSON parse error: {e}"),
                    }
                })
            } else {
                serde_json::from_str(&text).map_err(|e| {
                    error!("Deserialization error: {e} for text: {text}");
                    SoulseekError::Api {
                        status: status.as_u16(),
                        message: format!("JSON parse error: {e}"),
                    }
                })
            }
        } else {
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "Could not read error body".to_string());
            error!("API Error: {status} - {text}");
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

    pub async fn search(
        &self,
        query: &str,
        timeout: Duration,
    ) -> Result<(Vec<TrackResult>, Vec<AlbumResult>)> {
        self.wait_for_rate_limit().await?;
        info!("Starting search for: '{}'", query);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct SearchRequest<'a> {
            search_text: &'a str,
            timeout: i64,
            filter_responses: bool,
            // minimum_response_file_count: u32,
            // minimum_peer_upload_speed: u32,
        }

        let request_body = SearchRequest {
            search_text: query,
            timeout: timeout.num_milliseconds(),
            filter_responses: true,
            // minimum_response_file_count: 1,
            // minimum_peer_upload_speed: 0,
        };

        #[derive(Deserialize)]
        struct SearchId {
            id: String,
        }

        let search_id_resp: SearchId = self
            .make_request(Method::POST, "searches", Some(&request_body))
            .await?;
        let search_id = search_id_resp.id;
        self.active_searches.lock().await.insert(search_id.clone());

        info!("Search initiated with ID: {search_id}");

        let start_time = Utc::now();
        let poll_interval = Duration::seconds(1);
        let mut all_responses: Vec<SearchResponse> = Vec::new();

        while (Utc::now() - start_time) < timeout {
            if !self.active_searches.lock().await.contains(&search_id) {
                info!("Search {search_id} was cancelled, stopping.");
                break;
            }

            let endpoint = format!("searches/{search_id}/responses");
            match self
                .make_request::<Vec<SearchResponse>, ()>(Method::GET, &endpoint, None)
                .await
            {
                Ok(current_responses) => {
                    debug!("{current_responses:?}");
                    if current_responses.len() > all_responses.len() {
                        info!(
                            "Found {} new responses ({} total)",
                            current_responses.len() - all_responses.len(),
                            current_responses.len()
                        );
                        all_responses = current_responses;
                    }
                }
                Err(SoulseekError::Api { status: 404, .. }) => {
                    // Search might have expired or been deleted
                    break;
                }
                Err(e) => {
                    warn!("Error polling for search results: {:?}", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                poll_interval.num_milliseconds() as u64,
            ))
            .await;
        }

        // Cleanup
        self.active_searches.lock().await.remove(&search_id);
        let _ = self.delete_search(&search_id).await;

        let (mut tracks, mut albums) = self.process_search_responses(&all_responses);

        tracks.sort_by(|a, b| {
            b.base
                .quality_score()
                .partial_cmp(&a.base.quality_score())
                .unwrap()
        });
        albums.sort_by(|a, b| b.quality_score().partial_cmp(&a.quality_score()).unwrap());

        info!(
            "Search completed. Final results: {} tracks and {} albums",
            tracks.len(),
            albums.len()
        );
        Ok((tracks, albums))
    }

    fn process_search_responses(
        &self,
        responses: &[SearchResponse],
    ) -> (Vec<TrackResult>, Vec<AlbumResult>) {
        let audio_extensions: HashSet<&str> = ["mp3", "flac", "ogg", "aac", "wma", "wav", "m4a"]
            .iter()
            .cloned()
            .collect();
        let mut all_tracks = Vec::new();
        let mut albums_by_path: HashMap<(String, String), Vec<TrackResult>> = HashMap::new();

        for resp in responses {
            for file in &resp.files {
                let path = Path::new(&file.filename);
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if !audio_extensions.contains(ext) {
                        continue;
                    }
                }

                let base_result = SearchResult {
                    username: resp.username.clone(),
                    filename: file.filename.clone(),
                    size: file.size,
                    bitrate: file.bit_rate,
                    duration: file.length,
                    has_free_upload_slot: resp.has_free_upload_slot,
                    upload_speed: resp.upload_speed,
                    queue_length: resp.queue_length,
                };

                let track = TrackResult::new(base_result);
                all_tracks.push(track.clone());

                if let Some(album_path) = self.extract_album_path(&file.filename) {
                    albums_by_path
                        .entry((resp.username.clone(), album_path))
                        .or_default()
                        .push(track);
                }
            }
        }

        let album_results = self.create_album_results(albums_by_path);

        let album_track_filenames: HashSet<_> = album_results
            .iter()
            .flat_map(|a| &a.tracks)
            .map(|t| &t.base.filename)
            .collect();
        let individual_tracks: Vec<_> = all_tracks
            .into_iter()
            .filter(|t| !album_track_filenames.contains(&t.base.filename))
            .collect();

        (individual_tracks, album_results)
    }

    fn extract_album_path(&self, filename: &str) -> Option<String> {
        Path::new(filename)
            .parent()
            .and_then(|p| p.to_str())
            .map(String::from)
    }

    fn create_album_results(
        &self,
        albums_by_path: HashMap<(String, String), Vec<TrackResult>>,
    ) -> Vec<AlbumResult> {
        let mut results = Vec::new();
        for ((username, path), tracks) in albums_by_path {
            if tracks.len() < 2 {
                continue;
            }

            let total_size = tracks.iter().map(|t| t.base.size).sum();
            let dominant_quality = tracks
                .iter()
                .map(|t| t.base.quality())
                .counts()
                .into_iter()
                .max_by_key(|&(_, count)| count)
                .map(|(val, _)| val)
                .unwrap_or_else(|| "unknown".to_string());

            let album_title = self.extract_album_title(&path);
            let artist = self.determine_album_artist(&tracks, &path);
            let year = self.extract_year(&path, &album_title);

            let first_track = &tracks[0].base;

            results.push(AlbumResult {
                username: username.clone(),
                album_path: path,
                album_title,
                artist,
                track_count: tracks.len(),
                total_size,
                tracks: tracks.clone(),
                dominant_quality,
                year,
                has_free_upload_slot: first_track.has_free_upload_slot,
                upload_speed: first_track.upload_speed,
                queue_length: first_track.queue_length,
            });
        }
        results
    }

    fn extract_album_title(&self, album_path: &str) -> String {
        let album_dir = Path::new(album_path)
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("");
        let re_lead = Regex::new(r"^\d+\s*[-\.\s]+").unwrap();
        let re_year = Regex::new(r"\s*[-\(\[]?\d{4}[-\)\]]?\s*$").unwrap();
        let cleaned = re_lead.replace(album_dir, "");
        let cleaned = re_year.replace(&cleaned, "");
        cleaned.trim().to_string()
    }

    fn determine_album_artist(&self, tracks: &[TrackResult], _album_path: &str) -> Option<String> {
        tracks
            .iter()
            .filter_map(|t| t.artist.as_ref())
            .counts()
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(val, _)| val.clone())
    }

    fn extract_year(&self, album_path: &str, album_title: &str) -> Option<String> {
        let text_to_search = format!("{album_path} {album_title}");
        let re = Regex::new(r"\((\d{4})\)|\[(\d{4})\]|(\d{4})").unwrap();
        re.captures(&text_to_search)
            .and_then(|caps| (1..=3).find_map(|i| caps.get(i).map(|m| m.as_str().to_string())))
    }

    pub async fn download(&self, username: &str, filename: &str, file_size: i64) -> Result<String> {
        info!("Attempting to download: {} from {}", filename, username);

        let download_path_str = self.download_path.to_str().unwrap_or("").to_string();

        let payload = vec![DownloadRequestFile {
            filename,
            size: file_size,
            path: download_path_str,
        }];

        let endpoint = format!("transfers/downloads/{username}");

        #[derive(Deserialize)]
        struct DownloadResponse {
            id: String,
        }

        // slskd sometimes returns an array, sometimes a single object
        let resp_text = self
            .client
            .post(self.base_url.join(&format!("api/v0/{endpoint}"))?)
            .header("X-API-Key", self.api_key.as_deref().unwrap_or(""))
            .json(&payload)
            .send()
            .await?
            .text()
            .await?;

        if let Ok(single_resp) = serde_json::from_str::<DownloadResponse>(&resp_text) {
            Ok(single_resp.id)
        } else if let Ok(multi_resp) = serde_json::from_str::<Vec<DownloadResponse>>(&resp_text) {
            multi_resp
                .first()
                .map(|d| d.id.clone())
                .ok_or_else(|| SoulseekError::Api {
                    status: 200,
                    message: "Empty download response array".to_string(),
                })
        } else {
            Ok(filename.to_string()) // Fallback to filename if no ID is returned
        }
    }

    pub async fn get_all_downloads(&self) -> Result<Vec<DownloadStatus>> {
        self.make_request(Method::GET, "transfers/downloads", None::<()>)
            .await
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
            Err(SoulseekError::Api { status: 404, .. }) => Ok(()), // Ignore not found
            Err(e) => Err(e),
        }
    }

    pub async fn check_connection(&self) -> bool {
        self.make_request::<serde_json::Value, ()>(Method::GET, "session", None)
            .await
            .is_ok()
    }
}
