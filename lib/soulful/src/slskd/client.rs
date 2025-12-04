// Make sure the new utils module is accessible, e.g., mod utils;
use super::utils;
use crate::{
    error::{Result, SoulseekError},
    slskd::models::{DownloadRequestFile, SearchResponse},
};
use chrono::{DateTime, Duration, Utc};
use itertools::Itertools;
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{
    musicbrainz::Track,
    slskd::{
        AlbumResult, DownloadResponse, FileEntry, FlattenedFiles, MatchResult, SearchResult,
        TrackResult,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct SoulseekClient {
    base_url: Url,
    api_key: Option<String>,
    // download_path: PathBuf,
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
        // let download_path = self
        //     .download_path
        //     .unwrap_or_else(|| PathBuf::from("./downloads"));
        Ok(SoulseekClient {
            base_url,
            api_key: self.api_key,
            // download_path,
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

    pub async fn search(
        &self,
        artist: String,
        album: String,
        tracks: Vec<Track>,
        timeout: Duration,
    ) -> Result<Vec<AlbumResult>> {
        self.wait_for_rate_limit().await?;

        let track_titles: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();

        let query = format!("{} {}", artist.trim(), album.trim());
        info!("Starting search for: '{}'", query);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct SearchRequest<'a> {
            search_text: &'a str,
            timeout: i64,
            filter_responses: bool,
        }
        let request_body = SearchRequest {
            search_text: &query,
            timeout: timeout.num_milliseconds(),
            filter_responses: true,
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
                    if current_responses.len() > all_responses.len() {
                        info!(
                            "Found {} new responses ({} total)",
                            current_responses.len() - all_responses.len(),
                            current_responses.len()
                        );
                        all_responses = current_responses;
                    }
                }
                Err(SoulseekError::Api { status: 404, .. }) => break,
                Err(e) => warn!("Error polling for search results: {:?}", e),
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                poll_interval.num_milliseconds() as u64,
            ))
            .await;
        }

        self.active_searches.lock().await.remove(&search_id);
        let _ = self.delete_search(&search_id).await;

        let mut albums =
            self.process_search_responses(&all_responses, &artist, &album, &track_titles);

        albums.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        info!(
            "Search completed. Final results: {} albums/tracks",
            albums.len()
        );
        Ok(albums)
    }

    fn process_search_responses(
        &self,
        responses: &[SearchResponse],
        searched_artist: &str,
        searched_album: &str,
        expected_tracks: &[&str],
    ) -> Vec<AlbumResult> {
        const MIN_SCORE_THRESHOLD: f64 = 0.6;
        let audio_extensions: HashSet<&str> = ["flac", "wav", "m4a", "ogg", "aac", "wma", "mp3"]
            .iter()
            .copied()
            .collect();

        let scored_files: Vec<(MatchResult, SearchResult)> = responses
            .iter()
            .flat_map(|resp| {
                resp.files.iter().filter_map(|file| {
                    let path = Path::new(&file.filename);
                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_lowercase());

                    if let Some(ext) = ext {
                        if !audio_extensions.contains(ext.as_str()) {
                            return None;
                        }
                    }

                    let rank_result = utils::rank_match(
                        &file.filename,
                        Some(searched_artist),
                        Some(searched_album),
                        expected_tracks,
                    );

                    if rank_result.total_score < MIN_SCORE_THRESHOLD {
                        return None;
                    }

                    let search_result = SearchResult {
                        username: resp.username.clone(),
                        filename: file.filename.clone(),
                        size: file.size,
                        bitrate: file.bit_rate,
                        duration: file.length,
                        has_free_upload_slot: resp.has_free_upload_slot,
                        upload_speed: resp.upload_speed,
                        queue_length: resp.queue_length,
                    };
                    Some((rank_result, search_result))
                })
            })
            .collect();

        self.find_best_albums(&scored_files, expected_tracks)
    }

    fn find_best_albums(
        &self,
        scored_files: &[(MatchResult, SearchResult)],
        expected_tracks: &[&str],
    ) -> Vec<AlbumResult> {
        if expected_tracks.is_empty() {
            return vec![];
        }

        let album_groups = scored_files.iter().into_group_map_by(|(rank, search)| {
            (
                search.username.clone(),
                rank.guessed_artist.clone(),
                rank.guessed_album.clone(),
            )
        });

        album_groups
            .into_iter()
            .filter_map(|((username, artist, album_title), files_in_group)| {
                // Specific search: find the single best file for each expected track.
                let mut best_files_for_album = HashMap::new();

                for expected_track_title in expected_tracks {
                    if let Some(best_file_for_track) = files_in_group
                        .iter()
                        // Find all files that matched this specific track
                        .filter(|(rank, _)| &rank.matched_track == expected_track_title)
                        // Find the best one among them
                        .max_by(|(r1, s1), (r2, s2)| {
                            r1.total_score
                                .partial_cmp(&r2.total_score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| {
                                    s1.quality_score().partial_cmp(&s2.quality_score()).unwrap()
                                })
                        })
                    {
                        best_files_for_album.insert(*expected_track_title, best_file_for_track);
                    }
                }

                // If we didn't find a file for every track we were looking for, this album is incomplete.
                if best_files_for_album.len() != expected_tracks.len() {
                    return None;
                }

                let final_tracks: Vec<_> = best_files_for_album
                    .values()
                    .map(|(mr, sr)| TrackResult::new(sr.clone(), mr.clone()))
                    .collect();

                if final_tracks.is_empty() {
                    return None;
                }

                let completeness = if !expected_tracks.is_empty() {
                    final_tracks.len() as f64 / expected_tracks.len() as f64
                } else {
                    1.0 // Generic searches are considered "complete" by definition.
                };

                let total_size: i64 = final_tracks.iter().map(|t| t.base.size).sum();
                let dominant_quality = final_tracks
                    .iter()
                    .map(|t| t.base.quality())
                    .counts()
                    .into_iter()
                    .max_by_key(|&(_, count)| count)
                    .map(|(val, _)| val)
                    .unwrap_or_default();

                let first_track = final_tracks[0].base.clone();
                let album_path = first_track.filename.clone();

                let avg_score: f64 = final_tracks.iter().map(|t| t.match_score).sum::<f64>()
                    / final_tracks.len() as f64;
                let avg_format_score = final_tracks
                    .iter()
                    .map(|t| t.base.quality_score())
                    .sum::<f64>()
                    / final_tracks.len() as f64;

                let album_quality_score =
                    (avg_score * 0.3) + (completeness * 0.3) + (avg_format_score * 0.4);

                Some(AlbumResult {
                    username,
                    album_path,
                    album_title,
                    artist: Some(artist),
                    track_count: final_tracks.len(),
                    total_size,
                    tracks: final_tracks,
                    dominant_quality,
                    has_free_upload_slot: first_track.has_free_upload_slot,
                    upload_speed: first_track.upload_speed,
                    queue_length: first_track.queue_length,
                    score: album_quality_score,
                })
            })
            .collect()
    }

    pub async fn download(&self, req: Vec<TrackResult>) -> Result<Vec<DownloadResponse>> {
        let mut requests_by_username: HashMap<String, Vec<DownloadRequestFile>> = HashMap::new();

        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct SlskdDownloadResponse {
            id: String,
            filename: String,
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
                continue;
            }

            if resp_text.trim().is_empty() {
                info!("Slskd returned empty success response. Assuming files queued.");
                for req_file in &file_requests {
                    res.push(DownloadResponse {
                        id: "pending".to_string(),
                        filename: req_file.filename.clone(),
                    });
                }
                // TODO: Check slskd response
            } else if let Ok(single_res) = serde_json::from_str::<SlskdDownloadResponse>(&resp_text)
            {
                res.push(DownloadResponse {
                    id: single_res.id,
                    filename: single_res.filename,
                });
            } else if let Ok(multi_res) =
                serde_json::from_str::<Vec<SlskdDownloadResponse>>(&resp_text)
            {
                res.extend(multi_res.into_iter().map(|d| DownloadResponse {
                    id: d.id,
                    filename: d.filename,
                }));
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
