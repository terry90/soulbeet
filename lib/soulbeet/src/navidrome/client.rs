use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use std::{path::Path, time::Duration as StdDuration};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

use crate::error::{Result, SoulseekError};

use super::models::*;

const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
const CIRCUIT_BREAKER_FAILURE_THRESHOLD: u64 = 5;
const CIRCUIT_BREAKER_RESET_TIMEOUT_SECS: u64 = 60;
const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "Soulbeet";

#[derive(Debug)]
struct CircuitBreakerState {
    failure_count: u64,
    last_failure_time: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct CircuitBreaker {
    state: Mutex<CircuitBreakerState>,
    failure_threshold: u64,
    reset_timeout: Duration,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            state: Mutex::new(CircuitBreakerState {
                failure_count: 0,
                last_failure_time: None,
            }),
            failure_threshold: CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            reset_timeout: Duration::seconds(CIRCUIT_BREAKER_RESET_TIMEOUT_SECS as i64),
        }
    }
}

impl CircuitBreaker {
    async fn is_open(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.failure_count < self.failure_threshold {
            return false;
        }
        if let Some(last_time) = state.last_failure_time {
            if Utc::now() - last_time > self.reset_timeout {
                state.failure_count = 0;
                state.last_failure_time = None;
                return false;
            }
        }
        true
    }

    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.failure_count = 0;
    }

    async fn record_failure(&self) {
        let mut state = self.state.lock().await;
        state.failure_count += 1;
        state.last_failure_time = Some(Utc::now());
    }
}

pub struct NavidromeClient {
    base_url: Url,
    username: String,
    password: String,
    client: Client,
    circuit_breaker: CircuitBreaker,
    native_token: tokio::sync::Mutex<Option<String>>,
}

#[derive(Default)]
pub struct NavidromeClientBuilder {
    base_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

impl NavidromeClientBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn base_url(mut self, url: &str) -> Self {
        let mut resolved_url = url.to_string();
        if Path::new("/.dockerenv").exists() && resolved_url.contains("localhost") {
            resolved_url = resolved_url.replace("localhost", "host.docker.internal");
            info!(
                "Docker detected, using {} for Navidrome connection",
                resolved_url
            );
        }
        self.base_url = Some(resolved_url);
        self
    }

    pub fn username(mut self, username: &str) -> Self {
        self.username = Some(username.to_string());
        self
    }

    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_string());
        self
    }

    pub fn build(self) -> Result<NavidromeClient> {
        let base_url_str = self.base_url.ok_or(SoulseekError::NotConfigured)?;
        let mut normalized = base_url_str.trim_end_matches('/').to_string();
        normalized.push('/');
        let base_url = Url::parse(&normalized)?;
        let username = self.username.ok_or_else(|| SoulseekError::Api {
            status: 0,
            message: "Navidrome username not configured".to_string(),
        })?;
        let password = self.password.ok_or_else(|| SoulseekError::Api {
            status: 0,
            message: "Navidrome password not configured".to_string(),
        })?;

        let client = Client::builder()
            .connect_timeout(StdDuration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
            .timeout(StdDuration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .pool_idle_timeout(StdDuration::from_secs(90))
            .build()
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("Failed to build HTTP client: {}", e),
            })?;

        Ok(NavidromeClient {
            base_url,
            username,
            password,
            client,
            circuit_breaker: CircuitBreaker::default(),
            native_token: tokio::sync::Mutex::new(None),
        })
    }
}

impl NavidromeClient {
    fn auth_params(&self) -> Vec<(&str, String)> {
        let salt: String = (0..12)
            .map(|_| {
                let idx = rand::random::<u8>() % 36;
                if idx < 10 {
                    (b'0' + idx) as char
                } else {
                    (b'a' + idx - 10) as char
                }
            })
            .collect();
        let token = format!("{:x}", md5::compute(format!("{}{}", self.password, salt)));
        vec![
            ("u", self.username.clone()),
            ("t", token),
            ("s", salt),
            ("v", API_VERSION.to_string()),
            ("c", CLIENT_NAME.to_string()),
            ("f", "json".to_string()),
        ]
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T> {
        if self.circuit_breaker.is_open().await {
            warn!("Circuit breaker open, rejecting request to {}", endpoint);
            return Err(SoulseekError::Api {
                status: 503,
                message: "Circuit breaker open - Navidrome appears unavailable".to_string(),
            });
        }

        let mut url = self
            .base_url
            .join(&format!("rest/{}", endpoint))
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        {
            let mut query = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                query.append_pair(k, &v);
            }
            for (k, v) in extra_params {
                query.append_pair(k, v);
            }
        }

        debug!("Navidrome GET {}", endpoint);

        let response = match self.client.get(url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.circuit_breaker.record_failure().await;
                return Err(SoulseekError::Api {
                    status: if e.is_timeout() { 408 } else { 503 },
                    message: format!("Navidrome request failed: {}", e),
                });
            }
        };

        if !response.status().is_success() {
            self.circuit_breaker.record_failure().await;
            return Err(SoulseekError::Api {
                status: response.status().as_u16(),
                message: format!("Navidrome HTTP error: {}", response.status()),
            });
        }

        self.circuit_breaker.record_success().await;

        let envelope: SubsonicEnvelope<T> =
            response.json().await.map_err(|e| SoulseekError::Api {
                status: 500,
                message: format!("Failed to parse Navidrome response: {}", e),
            })?;

        if envelope.response.status != "ok" {
            let err = envelope.response.error.unwrap_or(SubsonicError {
                code: 0,
                message: "Unknown error".to_string(),
            });
            return Err(SoulseekError::Api {
                status: err.code as u16,
                message: err.message,
            });
        }

        Ok(envelope.response.body)
    }

    pub async fn ping(&self) -> Result<()> {
        let _: PingBody = self.get("ping", &[]).await?;
        Ok(())
    }

    pub async fn start_scan(&self) -> Result<()> {
        let _: PingBody = self.get("startScan", &[]).await?;
        Ok(())
    }

    pub async fn get_scan_status(&self) -> Result<bool> {
        #[derive(serde::Deserialize)]
        struct ScanStatusBody {
            #[serde(rename = "scanStatus")]
            scan_status: Option<ScanStatus>,
        }
        #[derive(serde::Deserialize)]
        struct ScanStatus {
            scanning: bool,
        }
        let body: ScanStatusBody = self.get("getScanStatus", &[]).await?;
        Ok(body.scan_status.map(|s| s.scanning).unwrap_or(false))
    }

    pub async fn get_all_albums(&self) -> Result<Vec<SubsonicAlbum>> {
        let mut all_albums = Vec::new();
        let mut offset = 0u32;
        let page_size = 500;

        loop {
            let offset_str = offset.to_string();
            let size_str = page_size.to_string();
            let body: AlbumList2Body = self
                .get(
                    "getAlbumList2",
                    &[
                        ("type", "alphabeticalByName"),
                        ("size", &size_str),
                        ("offset", &offset_str),
                    ],
                )
                .await?;

            let albums = body.album_list.map(|al| al.album).unwrap_or_default();

            let count = albums.len();
            all_albums.extend(albums);

            if count < page_size as usize {
                break;
            }
            offset += page_size;
        }

        info!("Fetched {} albums from Navidrome", all_albums.len());
        Ok(all_albums)
    }

    pub async fn get_album(&self, id: &str) -> Result<SubsonicAlbumDetail> {
        let body: AlbumBody = self.get("getAlbum", &[("id", id)]).await?;
        body.album.ok_or_else(|| SoulseekError::Api {
            status: 404,
            message: format!("Album {} not found", id),
        })
    }

    pub async fn get_all_songs_with_ratings(&self) -> Result<Vec<SubsonicSong>> {
        let albums = self.get_all_albums().await?;
        let mut all_songs = Vec::new();

        for album in &albums {
            match self.get_album(&album.id).await {
                Ok(detail) => all_songs.extend(detail.song),
                Err(e) => {
                    warn!("Failed to get album {}: {}", album.display_name(), e);
                }
            }
        }

        info!(
            "Fetched {} songs from {} albums",
            all_songs.len(),
            albums.len()
        );
        Ok(all_songs)
    }

    pub async fn set_rating(&self, id: &str, rating: u8) -> Result<()> {
        let rating_str = rating.to_string();
        let _: PingBody = self
            .get("setRating", &[("id", id), ("rating", &rating_str)])
            .await?;
        Ok(())
    }

    pub async fn get_playlists(&self) -> Result<Vec<SubsonicPlaylist>> {
        let body: PlaylistsBody = self.get("getPlaylists", &[]).await?;
        Ok(body.playlists.map(|p| p.playlist).unwrap_or_default())
    }

    pub async fn create_playlist(
        &self,
        name: &str,
        song_ids: &[String],
    ) -> Result<SubsonicPlaylistDetail> {
        if self.circuit_breaker.is_open().await {
            return Err(SoulseekError::Api {
                status: 503,
                message: "Circuit breaker open".to_string(),
            });
        }

        let mut url =
            self.base_url
                .join("rest/createPlaylist")
                .map_err(|e| SoulseekError::Api {
                    status: 0,
                    message: format!("URL error: {}", e),
                })?;

        {
            let mut query = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                query.append_pair(k, &v);
            }
            query.append_pair("name", name);
            for id in song_ids {
                query.append_pair("songId", id);
            }
        }

        let response = match self.client.get(url).send().await {
            Ok(resp) => {
                self.circuit_breaker.record_success().await;
                resp
            }
            Err(e) => {
                self.circuit_breaker.record_failure().await;
                return Err(SoulseekError::Api {
                    status: 503,
                    message: format!("Navidrome create playlist failed: {}", e),
                });
            }
        };

        let envelope: SubsonicEnvelope<PlaylistBody> =
            response.json().await.map_err(|e| SoulseekError::Api {
                status: 500,
                message: format!("Failed to parse response: {}", e),
            })?;

        if envelope.response.status != "ok" {
            let err = envelope.response.error.unwrap_or(SubsonicError {
                code: 0,
                message: "Unknown error".to_string(),
            });
            return Err(SoulseekError::Api {
                status: err.code as u16,
                message: err.message,
            });
        }

        envelope
            .response
            .body
            .playlist
            .ok_or_else(|| SoulseekError::Api {
                status: 500,
                message: "No playlist returned after creation".to_string(),
            })
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        let _: PingBody = self.get("deletePlaylist", &[("id", id)]).await?;
        Ok(())
    }

    pub async fn update_playlist_songs(
        &self,
        playlist_id: &str,
        song_ids_to_add: &[String],
    ) -> Result<()> {
        if self.circuit_breaker.is_open().await {
            return Err(SoulseekError::Api {
                status: 503,
                message: "Circuit breaker open".to_string(),
            });
        }

        let mut url =
            self.base_url
                .join("rest/updatePlaylist")
                .map_err(|e| SoulseekError::Api {
                    status: 0,
                    message: format!("URL error: {}", e),
                })?;

        {
            let mut query = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                query.append_pair(k, &v);
            }
            query.append_pair("playlistId", playlist_id);
            for id in song_ids_to_add {
                query.append_pair("songIdToAdd", id);
            }
        }

        let response = match self.client.get(url).send().await {
            Ok(resp) => {
                self.circuit_breaker.record_success().await;
                resp
            }
            Err(e) => {
                self.circuit_breaker.record_failure().await;
                return Err(SoulseekError::Api {
                    status: 503,
                    message: format!("Navidrome update playlist failed: {}", e),
                });
            }
        };

        let envelope: SubsonicEnvelope<PingBody> =
            response.json().await.map_err(|e| SoulseekError::Api {
                status: 500,
                message: format!("Failed to parse response: {}", e),
            })?;

        if envelope.response.status != "ok" {
            let err = envelope.response.error.unwrap_or(SubsonicError {
                code: 0,
                message: "Unknown error".to_string(),
            });
            return Err(SoulseekError::Api {
                status: err.code as u16,
                message: err.message,
            });
        }

        Ok(())
    }

    pub async fn get_starred(&self) -> Result<StarredContent> {
        let body: StarredBody = self.get("getStarred2", &[]).await?;
        Ok(body.starred.unwrap_or(StarredContent {
            song: vec![],
            album: vec![],
        }))
    }

    pub async fn search(&self, query: &str) -> Result<SearchResult3> {
        let body: SearchResult3Body = self
            .get(
                "search3",
                &[("query", query), ("songCount", "50"), ("albumCount", "20")],
            )
            .await?;
        Ok(body.search_result.unwrap_or(SearchResult3 {
            song: vec![],
            album: vec![],
        }))
    }

    // --- Navidrome Native API (for smart playlists) ---

    /// Get a cached JWT token, or fetch a fresh one from Navidrome.
    async fn native_token(&self) -> Result<String> {
        let cached = self.native_token.lock().await;
        if let Some(ref token) = *cached {
            return Ok(token.clone());
        }
        drop(cached);
        self.native_login_fresh().await
    }

    /// Fetch a fresh JWT token from Navidrome, replacing any cached value.
    async fn native_login_fresh(&self) -> Result<String> {
        let url = self
            .base_url
            .join("auth/login")
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        #[derive(serde::Serialize)]
        struct LoginReq {
            username: String,
            password: String,
        }
        #[derive(serde::Deserialize)]
        struct LoginResp {
            token: String,
        }

        let resp = self
            .client
            .post(url)
            .json(&LoginReq {
                username: self.username.clone(),
                password: self.password.clone(),
            })
            .send()
            .await
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("Native login failed: {}", e),
            })?;

        if !resp.status().is_success() {
            return Err(SoulseekError::Api {
                status: resp.status().as_u16(),
                message: "Navidrome native login failed".to_string(),
            });
        }

        let login: LoginResp = resp.json().await.map_err(|e| SoulseekError::Api {
            status: 0,
            message: format!("Failed to parse login response: {}", e),
        })?;
        *self.native_token.lock().await = Some(login.token.clone());
        Ok(login.token)
    }

    /// Send an authenticated native API request. Retries once with a fresh
    /// token if the first attempt gets a 401.
    async fn native_request(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let send = |r: reqwest::RequestBuilder, token: &str| {
            r.header("x-nd-authorization", format!("Bearer {}", token))
        };

        let token = self.native_token().await?;
        let retry_req = req.try_clone();
        let resp = send(req, &token)
            .send()
            .await
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("Native API request failed: {}", e),
            })?;

        if resp.status().as_u16() == 401 {
            if let Some(retry_req) = retry_req {
                let token = self.native_login_fresh().await?;
                return send(retry_req, &token)
                    .send()
                    .await
                    .map_err(|e| SoulseekError::Api {
                        status: 0,
                        message: format!("Native API request failed: {}", e),
                    });
            }
        }

        Ok(resp)
    }

    /// Create a smart playlist via Navidrome's native API.
    pub async fn create_smart_playlist(
        &self,
        name: &str,
        comment: &str,
        filepath_contains: &str,
    ) -> Result<String> {
        let url = self
            .base_url
            .join("api/playlist")
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        let body = serde_json::json!({
            "name": name,
            "comment": comment,
            "public": false,
            "rules": {
                "all": [
                    { "contains": { "filepath": filepath_contains } }
                ],
                "sort": "-dateAdded",
                "order": "desc"
            }
        });

        let resp = self.native_request(self.client.post(url).json(&body)).await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SoulseekError::Api {
                status,
                message: format!("Create smart playlist failed ({}): {}", status, body),
            });
        }

        #[derive(serde::Deserialize)]
        struct PlaylistResp {
            id: String,
        }

        let pl: PlaylistResp = resp.json().await.map_err(|e| SoulseekError::Api {
            status: 0,
            message: format!("Failed to parse playlist response: {}", e),
        })?;
        Ok(pl.id)
    }

    /// Delete a smart playlist via Navidrome's native API.
    pub async fn delete_smart_playlist(&self, playlist_id: &str) -> Result<()> {
        let url = self
            .base_url
            .join(&format!("api/playlist/{}", playlist_id))
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        let resp = self.native_request(self.client.delete(url)).await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SoulseekError::Api {
                status,
                message: format!("Delete smart playlist failed ({})", status),
            });
        }

        Ok(())
    }
}
