use reqwest::Client;
use std::time::Duration as StdDuration;
use tracing::{debug, info, warn};
use url::Url;

use crate::error::{Result, SoulseekError};
use crate::http::{resolve_docker_url, CircuitBreaker};

use super::models::*;

const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "Soulbeet";

/// Return the path components after `Discovery/<profile>/`, joined by `/`, or
/// `None` if `path` is not a file under that directory.
///
/// This is the single source of truth for discovery path matching. It matches by
/// path *component* (`Discovery` immediately followed by `<profile>`), so
/// `Discovery_Archive` or `album-balanced-edition` can never be mistaken for the
/// profile directory, and it handles a `staging/` or library-root prefix. Because
/// beets writes the same `Artist/Album/track.ext` tail on disk and in Navidrome's
/// relative path, comparing tails resolves a local file to its Navidrome song
/// exactly and collision-free.
pub fn discovery_path_tail(path: &str, profile: &str) -> Option<String> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    for i in 0..comps.len().saturating_sub(1) {
        if comps[i] == "Discovery" && comps[i + 1] == profile {
            let tail = comps[i + 2..].join("/");
            return if tail.is_empty() { None } else { Some(tail) };
        }
    }
    None
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
        self.base_url = Some(resolve_docker_url(url));
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
        self.search_all_songs().await
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
        let mut params: Vec<(&str, &str)> = vec![("name", name)];
        for id in song_ids {
            params.push(("songId", id));
        }
        let body: PlaylistBody = self.get("createPlaylist", &params).await?;
        body.playlist.ok_or_else(|| SoulseekError::Api {
            status: 500,
            message: "No playlist returned after creation".to_string(),
        })
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        let _: PingBody = self.get("deletePlaylist", &[("id", id)]).await?;
        Ok(())
    }

    /// Fetch a playlist with its ordered track entries.
    ///
    /// Returns `Ok(None)` only when the playlist genuinely does not exist
    /// (Subsonic error code 70, "data not found"); transient failures
    /// (timeouts, 5xx, circuit-breaker) surface as `Err`, so callers can tell
    /// "deleted in Navidrome" apart from "Navidrome temporarily unreachable" and
    /// avoid orphaning a playlist by recreating it on a blip.
    pub async fn get_playlist_opt(&self, id: &str) -> Result<Option<SubsonicPlaylistDetail>> {
        match self
            .get::<PlaylistBody>("getPlaylist", &[("id", id)])
            .await
        {
            Ok(body) => Ok(body.playlist),
            Err(SoulseekError::Api { status: 70, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Apply an add/remove delta to a playlist in a single `updatePlaylist` call.
    ///
    /// `indices_to_remove` are 0-based positions in the playlist's CURRENT track
    /// list. Navidrome resolves every removal index against the original list (no
    /// index-shift), then appends the added tracks, so add and remove are safe to
    /// send together.
    pub async fn update_playlist_diff(
        &self,
        playlist_id: &str,
        song_ids_to_add: &[String],
        indices_to_remove: &[usize],
    ) -> Result<()> {
        if song_ids_to_add.is_empty() && indices_to_remove.is_empty() {
            return Ok(());
        }
        let idx_strings: Vec<String> = indices_to_remove.iter().map(|i| i.to_string()).collect();
        let mut params: Vec<(&str, &str)> = vec![("playlistId", playlist_id)];
        for id in song_ids_to_add {
            params.push(("songIdToAdd", id));
        }
        for s in &idx_strings {
            params.push(("songIndexToRemove", s));
        }
        let _: PingBody = self.get("updatePlaylist", &params).await?;
        Ok(())
    }

    /// Rename a playlist via `updatePlaylist`.
    pub async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        let _: PingBody = self
            .get("updatePlaylist", &[("playlistId", playlist_id), ("name", name)])
            .await?;
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

    /// Fetch all songs from Navidrome using paginated search3 requests.
    /// Uses an empty query which matches everything in Navidrome's search implementation.
    /// Returns songs with userRating and path fields populated.
    pub async fn search_all_songs(&self) -> Result<Vec<SubsonicSong>> {
        let mut all_songs = Vec::new();
        let mut offset = 0u32;
        let page_size = 5000u32;
        loop {
            let offset_str = offset.to_string();
            let size_str = page_size.to_string();
            let body: SearchResult3Body = self
                .get(
                    "search3",
                    &[
                        ("query", ""),
                        ("artistCount", "0"),
                        ("albumCount", "0"),
                        ("songCount", &size_str),
                        ("songOffset", &offset_str),
                    ],
                )
                .await?;
            let songs = body.search_result.map(|r| r.song).unwrap_or_default();
            let count = songs.len() as u32;
            all_songs.extend(songs);
            if count < page_size {
                break;
            }
            offset += page_size;
        }
        info!("Fetched {} songs via search3", all_songs.len());
        Ok(all_songs)
    }

    // --- Navidrome Native API (JWT auth; real media_file paths) ---

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

    /// Return true if the playlist is a Navidrome smart playlist (has criteria
    /// rules). Used to detect playlists created by the legacy smart-playlist code
    /// path so they can be migrated to explicitly-managed static playlists.
    ///
    /// The native `GET /api/playlist/{id}` returns the `rules` JSON; a static
    /// playlist has no rules. A missing playlist (404) is reported as not-smart so
    /// the caller recreates it.
    pub async fn playlist_is_smart(&self, playlist_id: &str) -> Result<bool> {
        let url = self
            .base_url
            .join(&format!("api/playlist/{}", playlist_id))
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        let resp = self.native_request(self.client.get(url)).await?;

        if resp.status().as_u16() == 404 {
            return Ok(false);
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SoulseekError::Api {
                status,
                message: format!("Get playlist failed ({})", status),
            });
        }

        #[derive(serde::Deserialize)]
        struct PlaylistRules {
            #[serde(default)]
            rules: Option<serde_json::Value>,
        }

        let pl: PlaylistRules = resp.json().await.map_err(|e| SoulseekError::Api {
            status: 0,
            message: format!("Failed to parse playlist response: {}", e),
        })?;
        Ok(pl.rules.as_ref().is_some_and(|r| !r.is_null()))
    }

    /// Fetch every Navidrome song that lives under the `Discovery/<profile>/`
    /// directory, paginating the native `api/song` endpoint to completion.
    ///
    /// The native API returns the raw `media_file.path` (relative to the library
    /// root), independent of the per-player `ReportRealPath` flag, so it is the
    /// reliable source for resolving discovery files to Navidrome song IDs.
    /// Matching is by path *component* (`Discovery` followed by `<profile>`), so a
    /// `Discovery_Archive` or `album-balanced` directory can never be mistaken for
    /// the profile directory.
    pub async fn get_discovery_songs(&self, profile: &str) -> Result<Vec<NativeSong>> {
        let url = self
            .base_url
            .join("api/song")
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        const PAGE: u32 = 1000;
        let mut start = 0u32;
        let mut out = Vec::new();
        loop {
            let resp = self
                .native_request(self.client.get(url.clone()).query(&[
                    ("_start", start.to_string()),
                    ("_end", (start + PAGE).to_string()),
                    ("_sort", "path".to_string()),
                    ("_order", "ASC".to_string()),
                ]))
                .await?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                return Err(SoulseekError::Api {
                    status,
                    message: format!("Get songs failed ({})", status),
                });
            }

            let songs: Vec<NativeSong> = resp.json().await.map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("Failed to parse songs response: {}", e),
            })?;
            let page_len = songs.len() as u32;
            out.extend(
                songs
                    .into_iter()
                    .filter(|s| !s.missing && discovery_path_tail(&s.path, profile).is_some()),
            );
            if page_len < PAGE {
                break;
            }
            start += PAGE;
        }
        Ok(out)
    }

    /// List all players visible to the authenticated user via Navidrome's native API.
    /// Non-admin users only see their own players.
    pub async fn get_players(&self) -> Result<Vec<PlayerInfo>> {
        let url = self
            .base_url
            .join("api/player")
            .map_err(|e| SoulseekError::Api {
                status: 0,
                message: format!("URL error: {}", e),
            })?;

        let resp = self.native_request(self.client.get(url)).await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SoulseekError::Api {
                status,
                message: format!("Get players failed ({})", status),
            });
        }

        resp.json().await.map_err(|e| SoulseekError::Api {
            status: 0,
            message: format!("Failed to parse players response: {}", e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::discovery_path_tail;

    #[test]
    fn extracts_tail_under_discovery_profile() {
        assert_eq!(
            discovery_path_tail("Discovery/Balanced/Artist/Album/t.flac", "Balanced").as_deref(),
            Some("Artist/Album/t.flac")
        );
        // staging/ prefix and an absolute library root both resolve the same tail.
        assert_eq!(
            discovery_path_tail("staging/Discovery/Balanced/A/B/t.mp3", "Balanced").as_deref(),
            Some("A/B/t.mp3")
        );
        assert_eq!(
            discovery_path_tail("/srv/music/Discovery/Adventurous/X/t.flac", "Adventurous")
                .as_deref(),
            Some("X/t.flac")
        );
    }

    #[test]
    fn local_and_relative_paths_share_a_tail() {
        // The core resolution property: a local absolute path and Navidrome's
        // library-relative path produce the same tail, so they match.
        let local = "/srv/music/Discovery/Adventurous/Boards of Canada/Geogaddi/01 track.flac";
        let navi = "Discovery/Adventurous/Boards of Canada/Geogaddi/01 track.flac";
        assert_eq!(
            discovery_path_tail(local, "Adventurous"),
            discovery_path_tail(navi, "Adventurous")
        );
        assert!(discovery_path_tail(local, "Adventurous").is_some());
    }

    #[test]
    fn rejects_substring_lookalikes_and_other_profiles() {
        assert_eq!(
            discovery_path_tail("Discovery_Archive/Balanced/t.flac", "Balanced"),
            None
        );
        assert_eq!(
            discovery_path_tail("Music/album-balanced/t.flac", "Balanced"),
            None
        );
        assert_eq!(
            discovery_path_tail("Discovery/Adventurous/t.flac", "Balanced"),
            None
        );
        assert_eq!(discovery_path_tail("Other/Balanced/t.flac", "Balanced"), None);
    }

    #[test]
    fn none_when_profile_dir_has_no_file_tail() {
        assert_eq!(discovery_path_tail("Discovery/Balanced", "Balanced"), None);
    }
}
