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
        let answer: SubsonicAnswer =
            serde_json::from_slice(&bytes).map_err(|e| NavidromeError::Parse(e.to_string()))?;

        if answer.response.status != "ok" {
            return Err(match answer.response.error {
                Some(SubsonicError { code, message }) => NavidromeError::Subsonic { code, message },
                None => NavidromeError::Parse(format!(
                    "status '{}' without an error",
                    answer.response.status
                )),
            });
        }

        let envelope: SubsonicEnvelope<T> =
            serde_json::from_slice(&bytes).map_err(|e| NavidromeError::Parse(e.to_string()))?;

        Ok(envelope.response.body)
    }

    pub async fn ping(&self) -> Result<()> {
        let _: PingBody = self.get("ping", &[]).await?;
        Ok(())
    }

    pub async fn start_scan(&self) -> Result<()> {
        let _: PingBody = self.post_form("startScan", &[]).await?;
        Ok(())
    }

    pub async fn get_scan_status(&self) -> Result<ScanStatus> {
        let body: ScanStatusBody = self.get("getScanStatus", &[]).await?;
        Ok(body.scan_status)
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
        let url = self.base_url.join("api/song")?;

        let resp = self
            .native_request(self.client.get(url).query(&[
                ("_start", start.to_string()),
                ("_end", end.to_string()),
                ("_sort", "id".to_string()),
                ("_order", "ASC".to_string()),
                ("title", text.to_string()),
                ("missing", "false".to_string()),
            ]))
            .await?;

        if !resp.status().is_success() {
            return Err(NavidromeError::Http {
                action: "Find songs",
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

/// A request that got no HTTP answer, without the URL it was sent to.
fn transport(e: reqwest::Error) -> NavidromeError {
    NavidromeError::Transport(e.without_url().to_string())
}

/// An answer that could not be read, without the URL it came from.
fn parse(e: reqwest::Error) -> NavidromeError {
    NavidromeError::Parse(e.without_url().to_string())
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

    #[tokio::test]
    async fn songs_are_found_by_text_among_present_files() {
        let songs = r#"[{"id":"f2c4d1a8b0e94c37a6d5e8b1c2a3f405","libraryId":1,"libraryPath":"/music",
        "path":"Zzqx Artist/Qzz Album/01 Song Number 1.mp3","title":"Song Number 1","album":"Qzz Album",
        "artist":"Zzqx Artist","size":32032,"suffix":"mp3","missing":false}]"#;
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

    #[tokio::test]
    async fn a_failed_request_does_not_reveal_the_subsonic_token() {
        // A port nothing listens on: the connection is refused.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = client(&format!("http://127.0.0.1:{port}"))
            .ping()
            .await
            .unwrap_err();
        assert!(matches!(err, NavidromeError::Transport(_)), "{err:?}");

        let message = err.to_string();
        assert!(message.contains("Navidrome request failed"), "{message}");
        for leak in ["u=alice", "t=", "s=", "rest/ping"] {
            assert!(!message.contains(leak), "{leak} in {message}");
        }
    }
}
