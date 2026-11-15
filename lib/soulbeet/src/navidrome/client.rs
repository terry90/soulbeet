use reqwest::Client;
use std::time::Duration as StdDuration;
use tracing::{debug, info, warn};
use url::Url;

use crate::http::{resolve_docker_url, CircuitBreaker};

use super::error::{NavidromeError, Result};
use super::models::*;

const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "Soulbeet";
/// Subsonic error code for an id Navidrome has no record of, or one in a library this
/// user cannot access (Navidrome 0.64.2 server/subsonic/responses/errors.go:11,
/// browsing.go:197,253, the library filter persistence/sql_base_repository.go:246).
const DATA_NOT_FOUND: u32 = 70;

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
        let base_url_str = self
            .base_url
            .ok_or_else(|| NavidromeError::Config("base URL missing".to_string()))?;
        let mut normalized = base_url_str.trim_end_matches('/').to_string();
        normalized.push('/');
        let base_url = Url::parse(&normalized)?;
        let username = self
            .username
            .ok_or_else(|| NavidromeError::Config("username missing".to_string()))?;
        let password = self
            .password
            .ok_or_else(|| NavidromeError::Config("password missing".to_string()))?;

        let client = Client::builder()
            .connect_timeout(StdDuration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
            .timeout(StdDuration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .pool_idle_timeout(StdDuration::from_secs(90))
            .build()
            .map_err(|e| NavidromeError::Config(format!("HTTP client cannot be built: {e}")))?;

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
        let mut url = self.base_url.join(&format!("rest/{}", endpoint))?;
        {
            let mut query = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                query.append_pair(k, &v);
            }
            for (k, v) in extra_params {
                query.append_pair(k, v);
            }
        }
        self.subsonic(endpoint, self.client.get(url)).await
    }

    /// A Subsonic write as a form body. Navidrome copies the form into the
    /// query before every handler (server/subsonic/middlewares.go:32
    /// `postFormToQueryParams`, applied to all routes at
    /// server/subsonic/api.go:80).
    async fn post_form<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T> {
        let url = self.base_url.join(&format!("rest/{}", endpoint))?;
        let mut form = self.auth_params();
        form.extend(extra_params.iter().map(|(k, v)| (*k, v.to_string())));
        self.subsonic(endpoint, self.client.post(url).form(&form))
            .await
    }

    async fn subsonic<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        if self.circuit_breaker.is_open().await {
            warn!("Circuit breaker open, rejecting request to {}", endpoint);
            return Err(NavidromeError::CircuitOpen);
        }

        debug!("Navidrome request {}", endpoint);

        // The request carries the Subsonic token and salt, a working
        // credential, so no error built here may include it.
        let response = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.circuit_breaker.record_failure().await;
                return Err(transport(e));
            }
        };

        if !response.status().is_success() {
            self.circuit_breaker.record_failure().await;
            return Err(NavidromeError::Http {
                action: "Navidrome request",
                status: response.status().as_u16(),
                body: None,
            });
        }

        self.circuit_breaker.record_success().await;

        let bytes = response.bytes().await.map_err(parse)?;
        let answer: SubsonicAnswer = serde_json::from_slice(&bytes).map_err(undecodable)?;

        if answer.response.status != "ok" {
            return Err(match answer.response.error {
                Some(SubsonicError { code, message }) => NavidromeError::Subsonic { code, message },
                None => NavidromeError::Parse(format!(
                    "status '{}' without an error",
                    answer.response.status
                )),
            });
        }

        let envelope: SubsonicEnvelope<T> = serde_json::from_slice(&bytes).map_err(undecodable)?;

        Ok(envelope.response.body)
    }

    pub async fn ping(&self) -> Result<()> {
        let _: PingBody = self.get("ping", &[]).await?;
        Ok(())
    }

    /// Navidrome's version as ping reports it (`serverVersion`, e.g. "0.60.3 (34c6f12a)"); None
    /// when the answer has none.
    pub async fn server_version(&self) -> Result<Option<String>> {
        let body: VersionBody = self.get("ping", &[]).await?;
        Ok(body.server_version)
    }

    pub async fn start_scan(&self) -> Result<()> {
        let _: PingBody = self.post_form("startScan", &[]).await?;
        Ok(())
    }

    pub async fn get_scan_status(&self) -> Result<ScanStatus> {
        let body: ScanStatusBody = self.get("getScanStatus", &[]).await?;
        Ok(body.scan_status)
    }

    /// getUser of the user this client signs in as.
    pub async fn get_own_user(&self) -> Result<SubsonicUser> {
        let body: UserBody = self.get("getUser", &[("username", &self.username)]).await?;
        Ok(body.user)
    }

    /// getSong, as this user sees the song; Ok(None) on DATA_NOT_FOUND.
    pub async fn get_song(&self, id: &str) -> Result<Option<SubsonicSong>> {
        let Some(body) = found(self.get::<SongBody>("getSong", &[("id", id)]).await)? else {
            return Ok(None);
        };
        let song = body
            .song
            .ok_or_else(|| NavidromeError::Parse("getSong answered without a song".into()))?;
        if song.id != id {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered another song for {id}"
            )));
        }
        Ok(Some(song))
    }

    /// getAlbum without its songs, as this user sees the album; Ok(None) on DATA_NOT_FOUND.
    pub async fn get_album(&self, id: &str) -> Result<Option<SubsonicAlbum>> {
        let Some(body) = found(self.get::<AlbumBody>("getAlbum", &[("id", id)]).await)? else {
            return Ok(None);
        };
        let album = body
            .album
            .ok_or_else(|| NavidromeError::Parse("getAlbum answered without an album".into()))?;
        if album.id != id {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered another album for {id}"
            )));
        }
        Ok(Some(album))
    }

    /// setRating as a form post. Navidrome then recomputes the song's average
    /// (persistence/sql_annotations.go:141-158). An unknown id is Subsonic error 70
    /// "data not found" (server/subsonic/api.go:304-305).
    pub async fn set_rating(&self, id: &str, rating: u8) -> Result<()> {
        let rating = rating.to_string();
        let _: PingBody = self
            .post_form("setRating", &[("id", id), ("rating", &rating)])
            .await?;
        Ok(())
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

    pub async fn get_all_songs_with_ratings(&self) -> Result<Vec<SubsonicSong>> {
        self.search_all_songs().await
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
        let url = self.base_url.join("auth/login")?;

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
            .map_err(transport)?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(NavidromeError::NativeLoginRefused);
        }
        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "Navidrome native login",
                status: resp.status().as_u16(),
                body: None,
            });
        }

        let login: LoginResp = resp.json().await.map_err(parse)?;
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
        let resp = send(req, &token).send().await.map_err(transport)?;

        if resp.status().as_u16() == 401 {
            if let Some(retry_req) = retry_req {
                let token = self.native_login_fresh().await?;
                return send(retry_req, &token).send().await.map_err(transport);
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
        let url = self.base_url.join("api/playlist")?;

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

        let resp = self
            .native_request(self.client.post(url).json(&body))
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(NavidromeError::Http {
                action: "Create smart playlist",
                status,
                body: Some(body),
            });
        }

        #[derive(serde::Deserialize)]
        struct PlaylistResp {
            id: String,
        }

        let pl: PlaylistResp = resp.json().await.map_err(parse)?;
        Ok(pl.id)
    }

    /// Delete a smart playlist via Navidrome's native API.
    pub async fn delete_smart_playlist(&self, playlist_id: &str) -> Result<()> {
        let url = self
            .base_url
            .join(&format!("api/playlist/{}", playlist_id))?;

        let resp = self.native_request(self.client.delete(url)).await?;

        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "Delete smart playlist",
                status: resp.status().as_u16(),
                body: None,
            });
        }

        Ok(())
    }

    /// Query songs via the native API, filtered to a specific path prefix.
    /// Returns up to `limit` songs whose path contains the given prefix.
    /// The native API returns raw `media_file.path` values (relative to library root).
    pub async fn get_songs_by_path_prefix(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<NativeSong>> {
        let url = self.base_url.join("api/song")?;

        let resp = self
            .native_request(self.client.get(url).query(&[
                ("_start", "0".to_string()),
                ("_end", limit.to_string()),
                ("_sort", "path".to_string()),
                ("_order", "ASC".to_string()),
            ]))
            .await?;

        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "Get songs",
                status: resp.status().as_u16(),
                body: None,
            });
        }

        let songs: Vec<NativeSong> = resp.json().await.map_err(parse)?;

        Ok(songs
            .into_iter()
            .filter(|s| s.path.contains(prefix))
            .collect())
    }

    /// Present songs (missing=false) the native `title` filter matches for `text`, ordered by id,
    /// rows `start` to `end` (exclusive): a recording MBID matches the MBID columns, other text
    /// matches every word against title, album and artist names.
    pub async fn find_songs(&self, text: &str, start: u32, end: u32) -> Result<Vec<NativeSong>> {
        self.native_songs(
            "Find songs",
            &[
                ("_start", start.to_string()),
                ("_end", end.to_string()),
                ("_sort", "id".to_string()),
                ("_order", "ASC".to_string()),
                ("title", text.to_string()),
                ("missing", "false".to_string()),
            ],
        )
        .await
    }

    /// Present songs (missing=false), the one Navidrome added last first, rows `start` to `end`
    /// (exclusive). The `created_at` sort is `media_file.created_at`
    /// (persistence/mediafile_repository.go:86), set when a file is first stored (`:132`) and kept
    /// by later scans (persistence/sql_base_repository.go:431).
    pub async fn newest_songs(&self, start: u32, end: u32) -> Result<Vec<NativeSong>> {
        self.native_songs(
            "Find songs",
            &[
                ("_start", start.to_string()),
                ("_end", end.to_string()),
                ("_sort", "created_at".to_string()),
                ("_order", "DESC".to_string()),
                ("missing", "false".to_string()),
            ],
        )
        .await
    }

    /// The native record of song `id` (an `id` filter, persistence/mediafile_repository.go:120),
    /// as this user sees it; a missing file comes back with `missing: true`.
    /// Ok(None) for an empty list, and for an empty id with no request, since Navidrome drops a
    /// filter whose value is empty (persistence/sql_restful.go:25-28) and would answer the first
    /// song of the library. Any record but the one asked for is refused.
    pub async fn get_native_song(&self, id: &str) -> Result<Option<NativeSong>> {
        if id.is_empty() {
            return Ok(None);
        }
        let mut songs = self
            .native_songs(
                "Get song",
                &[
                    ("_start", "0".to_string()),
                    ("_end", "1".to_string()),
                    ("id", id.to_string()),
                ],
            )
            .await?;
        if songs.len() > 1 {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered more than one song for {id}"
            )));
        }
        match songs.pop() {
            None => Ok(None),
            Some(song) if song.id == id => Ok(Some(song)),
            Some(_) => Err(NavidromeError::Parse(format!(
                "Navidrome answered another song for {id}"
            ))),
        }
    }

    /// The name of every Navidrome user (`GET /api/user`, server/nativeapi/native_api.go:70), as
    /// an admin may list them; Ok(None) when this user is not an admin, as Navidrome refuses
    /// anyone else with rest.ErrPermissionDenied (persistence/user_repository.go:234-256), which
    /// deluan/rest answers with 403. No `_end` is sent, so the list is not cut short
    /// (persistence/sql_base_repository.go:136 limits only a positive Max).
    pub async fn user_names(&self) -> Result<Option<Vec<String>>> {
        #[derive(serde::Deserialize)]
        struct NavidromeUser {
            #[serde(rename = "userName")]
            user_name: String,
        }
        let url = self.base_url.join("api/user")?;
        let resp = self.native_request(self.client.get(url)).await?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "List users",
                status: resp.status().as_u16(),
                body: None,
            });
        }
        let users: Vec<NavidromeUser> = resp.json().await.map_err(parse)?;
        Ok(Some(users.into_iter().map(|u| u.user_name).collect()))
    }

    /// The native song list for `query`; a non-success status is an Http error of `action`.
    async fn native_songs(
        &self,
        action: &'static str,
        query: &[(&str, String)],
    ) -> Result<Vec<NativeSong>> {
        let url = self.base_url.join("api/song")?;

        let resp = self
            .native_request(self.client.get(url).query(query))
            .await?;

        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action,
                status: resp.status().as_u16(),
                body: None,
            });
        }

        resp.json().await.map_err(parse)
    }

    /// List all players visible to the authenticated user via Navidrome's native API.
    /// Non-admin users only see their own players.
    pub async fn get_players(&self) -> Result<Vec<PlayerInfo>> {
        let url = self.base_url.join("api/player")?;

        let resp = self.native_request(self.client.get(url)).await?;

        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "Get players",
                status: resp.status().as_u16(),
                body: None,
            });
        }

        resp.json().await.map_err(parse)
    }
}

/// A read's answer, None when Navidrome has no record the user can see (DATA_NOT_FOUND).
fn found<T>(answer: Result<T>) -> Result<Option<T>> {
    match answer {
        Ok(body) => Ok(Some(body)),
        Err(NavidromeError::Subsonic {
            code: DATA_NOT_FOUND,
            ..
        }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A request that got no HTTP answer, without the URL it was sent to.
fn transport(e: reqwest::Error) -> NavidromeError {
    NavidromeError::Transport(e.without_url().to_string())
}

/// An answer that could not be read, without the URL it came from.
fn parse(e: reqwest::Error) -> NavidromeError {
    NavidromeError::Parse(e.without_url().to_string())
}

/// What a Subsonic answer that is not the JSON asked for reads as: the text of reqwest's decode
/// error, which read these answers before Subsonic errors were read first.
const UNDECODABLE: &str = "error decoding response body";

/// A Subsonic answer serde could not read, as UNDECODABLE; serde's reason is logged.
fn undecodable(e: serde_json::Error) -> NavidromeError {
    debug!("Undecodable Navidrome answer: {e}");
    NavidromeError::Parse(UNDECODABLE.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn client(base_url: &str) -> NavidromeClient {
        NavidromeClientBuilder::new()
            .base_url(base_url)
            .username("alice")
            .password("secret-password")
            .build()
            .unwrap()
    }

    /// Navidrome's answer to a Subsonic request it refuses with `code`
    /// (server/subsonic/api.go `sendError`, always HTTP 200).
    fn subsonic_failure(code: u32) -> &'static str {
        match code {
            40 => {
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0","openSubsonic":true,"error":{"code":40,"message":"Wrong username or password"}}}"#
            }
            10 => {
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0","openSubsonic":true,"error":{"code":10,"message":"required 'u' parameter is missing"}}}"#
            }
            _ => unreachable!(),
        }
    }

    const SCAN_STATUS: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"scanStatus":{"scanning":true,"count":1342,"folderCount":120,"lastScan":"2026-11-14T21:03:11.519322Z","scanType":"quick"}}}"#;

    #[tokio::test]
    async fn an_answer_that_is_not_subsonic_json_reads_as_it_always_did() {
        // A proxy's login page in front of Navidrome answers 200 with HTML.
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: "<!doctype html><title>Sign in</title>".into(),
        }]);
        let e = client(&url).get_scan_status().await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "Unreadable Navidrome response: error decoding response body"
        );
    }

    #[tokio::test]
    async fn the_server_version_is_read_from_ping() {
        // server/subsonic/helpers.go:32 sets serverVersion on every answer.
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.60.3 (34c6f12a)","openSubsonic":true}}"#.into(),
            },
            Reply {
                status: 200,
                body: r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.into(),
            },
        ]);
        let navi = client(&url);
        assert_eq!(
            navi.server_version().await.unwrap().as_deref(),
            Some("0.60.3 (34c6f12a)")
        );
        assert_eq!(navi.server_version().await.unwrap(), None);
        assert!(seen.lock().unwrap()[0].target.starts_with("/rest/ping?"));
    }

    #[tokio::test]
    async fn a_scan_is_started_with_a_form_post() {
        let (url, seen) = serve(vec![Reply {
            status: 200,
            body: SCAN_STATUS.into(),
        }]);
        client(&url).start_scan().await.unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/rest/startScan")
        );
        assert_eq!(
            sent.content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        for part in ["u=alice", "v=1.16.1", "c=Soulbeet", "f=json"] {
            assert!(
                sent.body.split('&').any(|p| p == part),
                "{part} in {}",
                sent.body
            );
        }
    }

    #[tokio::test]
    async fn the_scan_status_has_its_count_time_and_error() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: SCAN_STATUS.into(),
        }]);
        let status = client(&url).get_scan_status().await.unwrap();
        assert!(status.scanning);
        assert_eq!(status.count, 1342);
        assert_eq!(
            status.last_scan.to_rfc3339(),
            "2026-11-14T21:03:11.519322+00:00"
        );
        assert_eq!(status.error, "");
    }

    // server/subsonic/users.go:16-33, responses/responses.go:363-380
    const USER: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"user":{"username":"alice","scrobblingEnabled":true,"adminRole":false,"settingsRole":false,"downloadRole":true,"uploadRole":false,"playlistRole":false,"coverArtRole":false,"commentRole":false,"podcastRole":false,"streamRole":true,"jukeboxRole":false,"shareRole":false,"videoConversionRole":false,"folder":[1]}}}"#;

    #[tokio::test]
    async fn the_own_user_says_whether_it_is_an_admin() {
        let (url, seen) = serve(vec![Reply {
            status: 200,
            body: USER.into(),
        }]);
        let user = client(&url).get_own_user().await.unwrap();
        assert!(!user.admin_role);
        let sent = seen.lock().unwrap()[0].clone();
        // client(&url) signs in as alice
        assert!(
            sent.target.starts_with("/rest/getUser?") && sent.target.contains("username=alice"),
            "{}",
            sent.target
        );
    }

    #[tokio::test]
    async fn songs_are_found_by_text_among_present_files() {
        let songs = r#"[{"id":"f2c4d1a8b0e94c37a6d5e8b1c2a3f405","libraryId":1,"libraryPath":"/music",
        "path":"Zzqx Artist/Qzz Album/01 Song Number 1.mp3","title":"Song Number 1","album":"Qzz Album",
        "artist":"Zzqx Artist","albumArtist":"Zzqx Artist","size":32032,"suffix":"mp3","missing":false,
        "updatedAt":"2026-11-14T20:58:41.208733912Z"}]"#;
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"token":"jwt"}"#.into(),
            },
            Reply {
                status: 200,
                body: songs.into(),
            },
        ]);
        let found = client(&url)
            .find_songs("Zzqx Artist Song Number 1", 0, 100)
            .await
            .unwrap();
        assert_eq!(
            found
                .iter()
                .map(|s| (s.path.as_str(), s.size))
                .collect::<Vec<_>>(),
            vec![("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 32032)]
        );
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/song?_start=0&_end=100&_sort=id&_order=ASC&title=Zzqx+Artist+Song+Number+1&missing=false"
        );
    }

    #[tokio::test]
    async fn the_newest_songs_are_listed_newest_first() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"token":"jwt"}"#.into(),
            },
            Reply {
                status: 200,
                body: "[]".into(),
            },
        ]);
        assert!(client(&url)
            .newest_songs(100, 200)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/song?_start=100&_end=200&_sort=created_at&_order=DESC&missing=false"
        );
    }

    #[tokio::test]
    async fn a_failed_answer_is_navidromes_own_error() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Internal Error"}}}"#.into(),
        }]);
        let err = client(&url).get_scan_status().await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Subsonic { code: 0, message } if message == "Internal Error"),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "Navidrome error 0: Internal Error");
    }

    #[tokio::test]
    async fn a_refused_login_is_subsonic_error_40() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: subsonic_failure(40).into(),
        }]);
        let err = client(&url).ping().await.unwrap_err();
        assert!(
            matches!(err, NavidromeError::Subsonic { code: 40, .. }),
            "{err:?}"
        );
        assert!(err.is_wrong_credentials());
    }

    #[tokio::test]
    async fn another_subsonic_error_keeps_its_code_and_is_not_a_refusal() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: subsonic_failure(10).into(),
        }]);
        let err = client(&url).ping().await.unwrap_err();
        assert!(
            matches!(err, NavidromeError::Subsonic { code: 10, .. }),
            "{err:?}"
        );
        assert!(!err.is_wrong_credentials());
    }

    #[tokio::test]
    async fn a_proxy_error_is_an_http_status() {
        let (url, _) = serve(vec![Reply {
            status: 502,
            body: "<html>Bad Gateway</html>".into(),
        }]);
        let err = client(&url).ping().await.unwrap_err();
        assert!(
            matches!(err, NavidromeError::Http { status: 502, .. }),
            "{err:?}"
        );
        assert!(!err.is_wrong_credentials());
    }

    #[tokio::test]
    async fn an_answer_that_is_not_subsonic_is_a_parse_error() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: "<html>a login page</html>".into(),
        }]);
        let err = client(&url).ping().await.unwrap_err();
        assert!(matches!(err, NavidromeError::Parse(_)), "{err:?}");
        assert!(!err.is_wrong_credentials());
    }

    const SONG: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"song":{"id":"3sG8kPq0WmZ1xYbN4vTcDe","parent":"7hQ2nLr5VxZ0cYbM8wTkAf","isDir":false,"title":"O Holy Night","album":"Winter Songs","artist":"Marrow Choir","track":3,"size":28734112,"contentType":"audio/flac","suffix":"flac","starred":"2026-11-02T18:04:51.112Z","duration":263,"bitRate":912,"path":"Marrow Choir/Winter Songs/01-03 - O Holy Night.flac","discNumber":1,"albumId":"7hQ2nLr5VxZ0cYbM8wTkAf","artistId":"2kD9pWq4ZxC1vBnM6yTjLs","type":"music","userRating":4,"averageRating":3.5,"bpm":0,"comment":"","sortName":"","mediaType":"song","musicBrainzId":"0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c","isrc":[],"genres":[],"replayGain":{},"channelCount":2,"samplingRate":44100}}}"#;
    const ALBUM: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"album":{"id":"7hQ2nLr5VxZ0cYbM8wTkAf","name":"Winter Songs","artist":"Marrow Choir","artistId":"2kD9pWq4ZxC1vBnM6yTjLs","songCount":12,"duration":2841,"starred":"2026-11-03T09:12:40.004Z","year":2019,"userRating":0,"genres":[],"musicBrainzId":"","isCompilation":false,"sortName":"","discTitles":[],"song":[]}}}"#;
    /// What a legacy client gets: no OpenSubsonic part, so no userRating (helpers.go:427-431).
    const LEGACY_ALBUM: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"album":{"id":"7hQ2nLr5VxZ0cYbM8wTkAf","name":"Winter Songs","artist":"Marrow Choir","songCount":12,"duration":2841,"year":2019,"song":[]}}}"#;
    const NATIVE_SONG: &str = r#"[{"id":"3sG8kPq0WmZ1xYbN4vTcDe","libraryId":1,"libraryPath":"/music","libraryName":"Music Library","folderId":"5tR1yUi8OpA3sDf6GhJ9kL","path":"Marrow Choir/Winter Songs/03 - O Holy Night.flac","title":"O Holy Night","album":"Winter Songs","artist":"Marrow Choir","albumArtist":"Marrow Choir","albumId":"7hQ2nLr5VxZ0cYbM8wTkAf","size":28734112,"suffix":"flac","duration":263.4,"bitRate":912,"mbzRecordingID":"0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c","missing":false,"birthTime":"2026-11-01T18:20:03.114208655+01:00","createdAt":"2026-11-01T18:20:07.551902337+01:00","updatedAt":"2026-11-01T18:20:03.114208655+01:00","rating":1,"playCount":3}]"#;
    const LOGIN: &str = r#"{"id":"8uY3tR6eW9qA2sD5fG8hJ1","name":"Alice","username":"alice","isAdmin":false,"token":"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.e30.c2ln"}"#;

    fn failed(code: u32, message: &str) -> String {
        format!(
            r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"error":{{"code":{code},"message":"{message}"}}}}}}"#
        )
    }

    #[tokio::test]
    async fn a_song_is_read_with_its_rating_star_and_album() {
        let (url, seen) = serve(vec![Reply {
            status: 200,
            body: SONG.into(),
        }]);
        let song = client(&url)
            .get_song("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                song.user_rating,
                song.starred.as_deref(),
                song.album_id.as_deref()
            ),
            (
                Some(4),
                Some("2026-11-02T18:04:51.112Z"),
                Some("7hQ2nLr5VxZ0cYbM8wTkAf")
            )
        );
        let sent = seen.lock().unwrap()[0].clone();
        assert!(sent.target.starts_with("/rest/getSong?"), "{}", sent.target);
        assert!(
            sent.target.contains("id=3sG8kPq0WmZ1xYbN4vTcDe"),
            "{}",
            sent.target
        );
    }

    #[tokio::test]
    async fn a_song_or_album_navidrome_cannot_find_is_none() {
        // browsing.go:251 and :197
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: failed(70, "Song not found"),
            },
            Reply {
                status: 200,
                body: failed(70, "Album not found"),
            },
        ]);
        let c = client(&url);
        assert!(c.get_song("gone").await.unwrap().is_none());
        assert!(c.get_album("gone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn another_refusal_of_a_read_is_an_error() {
        // mapToSubsonicError, server/subsonic/api.go:290-291
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: failed(0, "Internal Server Error: database is locked"),
            },
            Reply {
                status: 200,
                body: SCAN_STATUS.into(),
            },
            Reply {
                status: 200,
                body: SCAN_STATUS.into(),
            },
        ]);
        let c = client(&url);
        let err = c.get_song("3sG8kPq0WmZ1xYbN4vTcDe").await.unwrap_err();
        assert!(
            matches!(err, NavidromeError::Subsonic { code: 0, .. }),
            "{err:?}"
        );
        let err = c.get_song("3sG8kPq0WmZ1xYbN4vTcDe").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "getSong answered without a song"),
            "{err:?}"
        );
        let err = c.get_album("7hQ2nLr5VxZ0cYbM8wTkAf").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "getAlbum answered without an album"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_album_is_read_with_its_rating_and_star_or_without_a_rating_when_not_sent() {
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: ALBUM.into(),
            },
            Reply {
                status: 200,
                body: LEGACY_ALBUM.into(),
            },
        ]);
        let c = client(&url);
        let album = c
            .get_album("7hQ2nLr5VxZ0cYbM8wTkAf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (album.user_rating, album.starred.as_deref()),
            (Some(0), Some("2026-11-03T09:12:40.004Z"))
        );
        let legacy = c
            .get_album("7hQ2nLr5VxZ0cYbM8wTkAf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((legacy.user_rating, legacy.starred), (None, None));
    }

    #[tokio::test]
    async fn a_native_song_is_read_by_id_with_its_library_path_size_and_ratings() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 200,
                body: NATIVE_SONG.into(),
            },
            Reply {
                status: 200,
                body: "[]".into(),
            },
            // What Navidrome answers when the id filter is dropped: the first song of the library.
            Reply {
                status: 200,
                body: NATIVE_SONG.replace("3sG8kPq0WmZ1xYbN4vTcDe", "0bF4hJ7kL2zX9cV5nM1qWe"),
            },
        ]);
        let c = client(&url);
        let song = c
            .get_native_song("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                song.library_id,
                song.library_path.as_str(),
                song.path.as_str(),
                song.size,
                song.missing
            ),
            (
                1,
                "/music",
                "Marrow Choir/Winter Songs/03 - O Holy Night.flac",
                28734112,
                false
            )
        );
        assert_eq!(
            (song.rating, song.average_rating, song.album_artist.as_str()),
            (1, None, "Marrow Choir")
        );
        assert_eq!(
            song.mbz_recording_id.as_deref(),
            Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c")
        );
        // Go's RFC 3339 with nanoseconds and the server's offset, read as the same instant.
        assert_eq!(
            song.updated_at,
            "2026-11-01T17:20:03.114208655Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        );
        assert!(c.get_native_song("gone").await.unwrap().is_none());
        assert!(c.get_native_song("").await.unwrap().is_none());
        assert_eq!(seen.lock().unwrap().len(), 3, "an empty id sends nothing");
        let err = c
            .get_native_song("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered another song for 3sG8kPq0WmZ1xYbN4vTcDe"),
            "{err:?}"
        );
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/song?_start=0&_end=1&id=3sG8kPq0WmZ1xYbN4vTcDe"
        );
    }

    #[tokio::test]
    async fn an_admin_lists_every_user_and_anyone_else_is_refused() {
        // model/user.go:8-12, as persistence/user_repository.go ReadAll answers an admin.
        let users = r#"[{"id":"8uY3tR6eW9qA2sD5fG8hJ1","userName":"alice","name":"Alice","email":"","isAdmin":true,"lastLoginAt":null,"lastAccessAt":null,"createdAt":"2026-10-01T09:00:00Z","updatedAt":"2026-10-01T09:00:00Z","scrobbleFilter":""},{"id":"2wE5rT8yU1iO4pA7sD0fG3","userName":"Erin","name":"Erin","email":"","isAdmin":false,"lastLoginAt":null,"lastAccessAt":null,"createdAt":"2026-10-02T09:00:00Z","updatedAt":"2026-10-02T09:00:00Z","scrobbleFilter":""}]"#;
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 200,
                body: users.into(),
            },
            // The body is not read.
            Reply {
                status: 403,
                body: "".into(),
            },
            Reply {
                status: 500,
                body: "".into(),
            },
        ]);
        let c = client(&url);
        assert_eq!(
            c.user_names().await.unwrap(),
            Some(vec!["alice".to_string(), "Erin".to_string()])
        );
        assert_eq!(seen.lock().unwrap()[1].target, "/api/user");
        assert_eq!(c.user_names().await.unwrap(), None);
        let err = c.user_names().await.unwrap_err();
        assert!(
            matches!(err, NavidromeError::Http { status: 500, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_about_another_item_is_refused() {
        let record = &NATIVE_SONG[1..NATIVE_SONG.len() - 1];
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: SONG.into(),
            },
            Reply {
                status: 200,
                body: ALBUM.into(),
            },
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 200,
                body: format!("[{record},{record}]"),
            },
        ]);
        let c = client(&url);
        let err = c.get_song("0bF4hJ7kL2zX9cV5nM1qWe").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered another song for 0bF4hJ7kL2zX9cV5nM1qWe"),
            "{err:?}"
        );
        let err = c.get_album("1aB2cD3eF4gH5iJ6kL7mNo").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered another album for 1aB2cD3eF4gH5iJ6kL7mNo"),
            "{err:?}"
        );
        let err = c
            .get_native_song("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered more than one song for 3sG8kPq0WmZ1xYbN4vTcDe"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_rating_is_set_with_a_form_post_and_an_unknown_id_is_refused() {
        // setRating answers an empty ok response (server/subsonic/media_annotation.go:36).
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: SCAN_STATUS.into(),
            },
            Reply {
                status: 200,
                body: failed(70, "data not found"),
            },
        ]);
        let c = client(&url);
        c.set_rating("3sG8kPq0WmZ1xYbN4vTcDe", 1).await.unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/rest/setRating")
        );
        for part in ["id=3sG8kPq0WmZ1xYbN4vTcDe", "rating=1"] {
            assert!(
                sent.body.split('&').any(|p| p == part),
                "{part} in {}",
                sent.body
            );
        }
        assert!(matches!(
            c.set_rating("gone", 1).await,
            Err(NavidromeError::Subsonic { code: 70, .. })
        ));
    }

    #[tokio::test]
    async fn a_failed_request_does_not_reveal_the_subsonic_token() {
        // A server that hangs up on every request. A port left free instead
        // could be taken by another test's server meanwhile.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        let err = client(&url).ping().await.unwrap_err();
        assert!(matches!(err, NavidromeError::Transport(_)), "{err:?}");

        let message = err.to_string();
        assert!(message.contains("Navidrome request failed"), "{message}");
        for leak in ["u=alice", "t=", "s=", "rest/ping"] {
            assert!(!message.contains(leak), "{leak} in {message}");
        }
    }
}
