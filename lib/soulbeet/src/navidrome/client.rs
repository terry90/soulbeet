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
                raw: None,
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

    /// getAlbum with its songs, as this user sees the album; Ok(None) on DATA_NOT_FOUND.
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

    /// search3 for one kind: `{kind}Count=count` and `{kind}Offset=offset`, the other counts 0
    /// (server/subsonic/searching.go:33-44).
    pub async fn search3(
        &self,
        query: &str,
        kind: LibraryKind,
        offset: u32,
        count: u32,
    ) -> Result<SearchResult3> {
        let (count, offset) = (count.to_string(), offset.to_string());
        let asked = |k: LibraryKind| if k == kind { count.as_str() } else { "0" };
        let offset_name = match kind {
            LibraryKind::Songs => "songOffset",
            LibraryKind::Albums => "albumOffset",
            LibraryKind::Artists => "artistOffset",
        };
        let body: SearchResult3Body = self
            .get(
                "search3",
                &[
                    ("query", query),
                    ("songCount", asked(LibraryKind::Songs)),
                    ("albumCount", asked(LibraryKind::Albums)),
                    ("artistCount", asked(LibraryKind::Artists)),
                    (offset_name, &offset),
                ],
            )
            .await?;
        body.search_result
            .ok_or_else(|| NavidromeError::Parse("search3 answered without searchResult3".into()))
    }

    /// getStarred2: the artists, albums and songs this user starred.
    pub async fn get_starred(&self) -> Result<Starred2> {
        let body: Starred2Body = self.get("getStarred2", &[]).await?;
        body.starred
            .ok_or_else(|| NavidromeError::Parse("getStarred2 answered without starred2".into()))
    }

    /// getPlaylists: every playlist Navidrome lists to this user, by name. An admin is listed
    /// every user's playlists, private ones included (persistence/playlist_repository.go:84-93).
    pub async fn get_playlists(&self) -> Result<Vec<SubsonicPlaylist>> {
        let body: PlaylistsBody = self.get("getPlaylists", &[]).await?;
        body.playlists
            .map(|p| p.playlist)
            .ok_or_else(|| NavidromeError::Parse("getPlaylists answered without playlists".into()))
    }

    /// getPlaylist without its entries; Ok(None) on DATA_NOT_FOUND ("playlist not found",
    /// server/subsonic/playlists.go:46).
    pub async fn get_playlist(&self, id: &str) -> Result<Option<SubsonicPlaylist>> {
        let Some(body) = found(self.get::<PlaylistBody>("getPlaylist", &[("id", id)]).await)?
        else {
            return Ok(None);
        };
        let playlist = body.playlist.ok_or_else(|| {
            NavidromeError::Parse("getPlaylist answered without a playlist".into())
        })?;
        if playlist.id != id {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered another playlist for {id}"
            )));
        }
        Ok(Some(playlist))
    }

    /// Every track of playlist `id` with its place, missing files included, from the native API
    /// (server/nativeapi/native_api.go:156-160), which reads the playlist with the same own,
    /// public or admin filter as getPlaylist; Ok(None) on Navidrome's own 404, whose body is
    /// http.Error's "not found\n" (server/nativeapi/playlists.go:38-48). Any other 404, such as a
    /// proxy's for a path it does not forward, is an Http error with its body, since it says
    /// nothing about the playlist. No `_end` is sent, so the list is not cut short.
    /// An id that is empty, "." or ".." names no playlist and is not a path segment the URL can
    /// carry, so it is Ok(None) with no request.
    pub async fn playlist_tracks(&self, id: &str) -> Result<Option<Vec<PlaylistTrack>>> {
        if matches!(id, "" | "." | "..") {
            return Ok(None);
        }
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| NavidromeError::Config("base URL cannot hold a path".into()))?
            .pop_if_empty()
            .extend(["api", "playlist", id, "tracks"]);
        let resp = self
            .native_request(
                self.client
                    .get(url)
                    .query(&[("_sort", "id"), ("_order", "ASC")]),
            )
            .await?;
        if !resp.status().is_success() {
            let e = native_failure("Get playlist tracks", resp).await;
            return match &e {
                NavidromeError::Http {
                    status: 404,
                    raw: Some(raw),
                    ..
                } if raw == "not found\n" => Ok(None),
                _ => Err(e),
            };
        }
        let bytes = resp.bytes().await.map_err(parse)?;
        let rows: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_slice(&bytes).map_err(undecodable)?;
        rows.into_iter()
            .map(playlist_track)
            .collect::<Result<_>>()
            .map(Some)
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

    /// createPlaylist with `name` and one `songId` per id, in order. Navidrome answers with the
    /// playlist it wrote, as getPlaylist (server/subsonic/playlists.go). Ids this account cannot
    /// read are dropped without an error (persistence/playlist_repository.go keepAccessible), so
    /// the caller compares the playlist's tracks with what it sent.
    pub async fn create_playlist(
        &self,
        name: &str,
        song_ids: &[String],
    ) -> Result<SubsonicPlaylist> {
        let mut params = vec![("name", name)];
        params.extend(song_ids.iter().map(|id| ("songId", id.as_str())));
        let body: PlaylistBody = self.post_form("createPlaylist", &params).await?;
        body.playlist.ok_or_else(|| {
            NavidromeError::Parse("createPlaylist answered without a playlist".into())
        })
    }

    /// createPlaylist with `playlistId` and one `songId` per id: Navidrome replaces every entry of
    /// the playlist in one transaction (core/playlists/playlists.go Create), rows of libraries
    /// this account cannot see included, and drops ids it cannot read without an error. For a
    /// non-admin account it refuses another user's playlist, and for any account a smart or
    /// file-synced one (Subsonic error 50); an admin account writes another user's playlist
    /// without an error, so core checks the owner itself before every write. Ok(false) when
    /// Navidrome has no such playlist for this user. Navidrome keeps the old entries when the
    /// list is empty (persistence/playlist_repository.go Put rewrites rows only for a non-empty
    /// list), so core empties a playlist with `remove_playlist_tracks` and never calls this with
    /// an empty list.
    pub async fn replace_playlist_tracks(&self, id: &str, song_ids: &[String]) -> Result<bool> {
        let mut params = vec![("playlistId", id)];
        params.extend(song_ids.iter().map(|id| ("songId", id.as_str())));
        written(self.post_form("createPlaylist", &params).await)
    }

    /// updatePlaylist with one `songIndexToRemove` per index. Navidrome turns index i into the
    /// native row id i+1 and deletes those rows, then renumbers 1..n (core/playlists/playlists.go
    /// Update, persistence/playlist_track_repository.go), so `indexes` are `PlaylistTrack.position`
    /// less one (the native row id, which counts every row), not indexes into the entries
    /// getPlaylist lists, which leaves out missing files and other libraries. Ok(false) on 70.
    pub async fn remove_playlist_tracks(&self, id: &str, indexes: &[usize]) -> Result<bool> {
        let indexes: Vec<String> = indexes.iter().map(usize::to_string).collect();
        let mut params = vec![("playlistId", id)];
        params.extend(indexes.iter().map(|i| ("songIndexToRemove", i.as_str())));
        written(self.post_form("updatePlaylist", &params).await)
    }

    /// updatePlaylist with one `songIdToAdd` per id, appended after the playlist's highest row id
    /// (persistence/playlist_track_repository.go). Ids this account cannot read are dropped
    /// without an error. Ok(false) on 70.
    pub async fn append_playlist_tracks(&self, id: &str, song_ids: &[String]) -> Result<bool> {
        let mut params = vec![("playlistId", id)];
        params.extend(song_ids.iter().map(|id| ("songIdToAdd", id.as_str())));
        written(self.post_form("updatePlaylist", &params).await)
    }

    /// updatePlaylist with `name`, `comment` (an empty one clears it) and `public`
    /// ("true" or "false"), each only when Some. Ok(false) on 70.
    pub async fn update_playlist_meta(&self, id: &str, meta: &PlaylistMeta<'_>) -> Result<bool> {
        let mut params = vec![("playlistId", id)];
        if let Some(name) = meta.name {
            params.push(("name", name));
        }
        if let Some(comment) = meta.comment {
            params.push(("comment", comment));
        }
        if let Some(public) = meta.public {
            params.push(("public", if public { "true" } else { "false" }));
        }
        written(self.post_form("updatePlaylist", &params).await)
    }

    /// deletePlaylist. Ok(false) on 70. Navidrome answers a non-admin account's delete of a
    /// playlist it does not own with Subsonic error 50, which passes through as an error; an
    /// admin account deletes another user's playlist without one, so core checks the owner
    /// itself first.
    pub async fn delete_playlist(&self, id: &str) -> Result<bool> {
        written(self.post_form("deletePlaylist", &[("id", id)]).await)
    }

    /// star or unstar of a song (`id`) or an album (`albumId`). Navidrome skips an id it does not
    /// know with a warning and answers ok (server/subsonic/media_annotation.go setStar), so a
    /// caller checks that the target exists first.
    pub async fn set_star(&self, target: StarTarget<'_>, starred: bool) -> Result<()> {
        let endpoint = if starred { "star" } else { "unstar" };
        let param = match target {
            StarTarget::Song(id) => ("id", id),
            StarTarget::Album(id) => ("albumId", id),
        };
        let _: PingBody = self.post_form(endpoint, &[param]).await?;
        Ok(())
    }

    /// The native playlist record of `id`, as this user may read it. Ok(None) only for Navidrome's
    /// own 404, `{"error":"playlist(id:<id>) not found"}` (deluan/rest controller.go Get); any
    /// other non-success, a proxy's 404 included, is an Http error with its body. An id that is
    /// empty, "." or ".." names no playlist and is not a path segment the URL can carry, so it is
    /// Ok(None) with no request.
    pub async fn get_native_playlist(&self, id: &str) -> Result<Option<NativePlaylist>> {
        if matches!(id, "" | "." | "..") {
            return Ok(None);
        }
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| NavidromeError::Config("base URL cannot hold a path".into()))?
            .pop_if_empty()
            .extend(["api", "playlist", id]);
        let resp = self.native_request(self.client.get(url)).await?;
        if !resp.status().is_success() {
            let e = native_failure("Get playlist", resp).await;
            let none = format!("playlist(id:{id}) not found");
            return match &e {
                NavidromeError::Http {
                    status: 404,
                    raw: Some(raw),
                    ..
                } if serde_json::from_str::<NotFoundBody>(raw).is_ok_and(|b| b.error == none) => {
                    Ok(None)
                }
                _ => Err(e),
            };
        }
        let playlist: NativePlaylist = resp.json().await.map_err(parse)?;
        if playlist.id != id {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered another playlist for {id}"
            )));
        }
        Ok(Some(playlist))
    }

    /// The playlists holding song `id` that this user may see, from the native API
    /// (server/nativeapi/native_api.go:184-188, handler playlists.go:221-237): public ones and
    /// the user's own, every one for an admin (persistence/playlist_repository.go:84-93), sorted
    /// by name, each with its owner's name (:258-262). Navidrome answers `[]` when none does
    /// (:238-256) and a failed query as a 500 with the error's text (playlists.go:226-228), an
    /// Http error here. A body that is not a list is a Parse error. An id that is empty, "." or
    /// ".." names no song and is not a path segment the URL can carry, so it is a Parse error
    /// with no request: an empty list would say no playlist holds the song.
    pub async fn get_song_playlists(&self, id: &str) -> Result<Vec<NativePlaylist>> {
        if matches!(id, "" | "." | "..") {
            return Err(NavidromeError::Parse("No song id given".into()));
        }
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| NavidromeError::Config("base URL cannot hold a path".into()))?
            .pop_if_empty()
            .extend(["api", "song", id, "playlists"]);
        let resp = self.native_request(self.client.get(url)).await?;
        if !resp.status().is_success() {
            return Err(native_failure("Get song playlists", resp).await);
        }
        resp.json().await.map_err(parse)
    }

    /// The native records of the songs `ids` this user can see, a hundred ids to a request, in
    /// any order; an id Navidrome does not list is absent, one in a missing file has
    /// `missing: true`. Empty ids are dropped, since Navidrome drops an empty filter and would
    /// answer the first songs of the library (persistence/sql_restful.go), and an empty list sends
    /// no request.
    pub async fn get_native_songs(&self, ids: &[String]) -> Result<Vec<NativeSong>> {
        let ids: Vec<&String> = ids.iter().filter(|id| !id.is_empty()).collect();
        let mut found = Vec::new();
        for chunk in ids.chunks(100) {
            let mut query = vec![
                ("_start", "0".to_string()),
                ("_end", chunk.len().to_string()),
            ];
            query.extend(chunk.iter().map(|id| ("id", (*id).clone())));
            found.extend(self.native_songs("Get songs", &query).await?);
        }
        Ok(found)
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
                raw: None,
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
                raw: None,
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
            return Err(native_failure("Delete smart playlist", resp).await);
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
            return Err(native_failure("Get songs", resp).await);
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

    /// Present songs (missing=false) in the order Navidrome added them, rows `start` to `end`
    /// (exclusive), and how many present songs Navidrome counts now. The `missing` filter is
    /// `booleanFilter` (persistence/mediafile_repository.go:125, persistence/sql_restful.go:104-107
    /// in 0.64.2); deluan/rest takes `_end - _start` rows with no cap (controller.go:217-229 at
    /// the version 0.64.2's go.mod names) and sends the count as `X-Total-Count` (:63-64).
    /// The sort is `recently_added` (persistence/mediafile_repository.go:101), which maps to
    /// `media_file.created_at, media_file.id`, or to `media_file.updated_at, media_file.id` with
    /// RecentlyAddedByModTime (:143-148, off by default, conf/configuration.go:1004), each part
    /// in the asked order (persistence/sql_base_repository.go:184-206). Either is unique, since
    /// `media_file.id` is the table's primary key, so no page boundary falls inside a tie. Either
    /// is table-qualified: past an offset of DevOffsetOptimize (50,000 by default,
    /// conf/configuration.go:1122) Navidrome leaves out the earlier rows with a subquery that
    /// orders the joined tables by the sort (persistence/sql_base_repository.go:426-428,
    /// 461-469), where a plain `id` names both media_file's and library's and fails. The order
    /// can still shift between pages (a song added or removed, or a changed file with
    /// RecentlyAddedByModTime), so a caller checks that it read as many as the count.
    pub async fn list_songs(&self, start: u32, end: u32) -> Result<SongPage> {
        let url = self.base_url.join("api/song")?;
        let query = [
            ("_start", start.to_string()),
            ("_end", end.to_string()),
            ("_sort", "recently_added".to_string()),
            ("_order", "ASC".to_string()),
            ("missing", "false".to_string()),
        ];
        let resp = self
            .native_request(self.client.get(url).query(&query))
            .await?;
        if !resp.status().is_success() {
            return Err(native_failure("List songs", resp).await);
        }
        let total = (resp.headers().get("x-total-count"))
            .and_then(|count| count.to_str().ok()?.parse().ok())
            .ok_or_else(|| {
                NavidromeError::Parse("Navidrome listed songs without a valid X-Total-Count".into())
            })?;
        let songs = resp.json().await.map_err(parse)?;
        Ok(SongPage { songs, total })
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
            return Err(native_failure("List users", resp).await);
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
            return Err(native_failure(action, resp).await);
        }

        resp.json().await.map_err(parse)
    }
}

/// A native track row (model.PlaylistTrack, model/playlist.go:224-229) as its place and song: the
/// row's own `id` is the place and hides the media file's, which is `mediaFileId`.
/// Navidrome numbers a playlist's tracks from 1 (persistence/playlist_repository.go:485-500), so a
/// place 0 is refused with the rest of what is no place.
fn playlist_track(mut row: serde_json::Map<String, serde_json::Value>) -> Result<PlaylistTrack> {
    let unplaced = || {
        NavidromeError::Parse(
            "Navidrome answered a playlist track without its place or song id".into(),
        )
    };
    let position = match row.remove("id") {
        Some(serde_json::Value::String(place))
            if !place.is_empty() && place.bytes().all(|b| b.is_ascii_digit()) =>
        {
            place.parse().map_err(|_| unplaced())?
        }
        _ => return Err(unplaced()),
    };
    let Some(song_id @ serde_json::Value::String(_)) = row.remove("mediaFileId") else {
        return Err(unplaced());
    };
    row.insert("id".into(), song_id);
    let song = serde_json::from_value(serde_json::Value::Object(row)).map_err(undecodable)?;
    Ok(PlaylistTrack { position, song })
}

/// A native API answer that is not a success, as an Http error of `action` whose cause keeps the
/// answer's text; an empty one explains nothing and is not kept.
async fn native_failure(action: &'static str, resp: reqwest::Response) -> NavidromeError {
    let status = resp.status().as_u16();
    let raw = resp.text().await.ok().filter(|text| !text.is_empty());
    NavidromeError::Http {
        action,
        status,
        body: None,
        raw,
    }
}

/// The body deluan/rest answers a record it has none of with.
#[derive(serde::Deserialize)]
struct NotFoundBody {
    error: String,
}

/// What a star or unstar acts on.
#[derive(Debug, Clone, Copy)]
pub enum StarTarget<'a> {
    Song(&'a str),
    Album(&'a str),
}

/// A page of list_songs: its songs, and how many present songs Navidrome counted.
#[derive(Debug)]
pub struct SongPage {
    pub songs: Vec<NativeSong>,
    pub total: u64,
}

/// The playlist fields updatePlaylist changes; a field left None is not sent.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaylistMeta<'a> {
    pub name: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub public: Option<bool>,
}

/// A read's answer, None when Navidrome has no record the user can see (DATA_NOT_FOUND).
fn found<T>(answer: Result<T>) -> Result<Option<T>> {
    match answer {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e),
    }
}

/// A write's answer, false when Navidrome has no record of the id for this user (DATA_NOT_FOUND).
fn written(answer: Result<PingBody>) -> Result<bool> {
    found(answer).map(|body| body.is_some())
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
    use crate::test_http::{serve, serve_with_headers, Reply};

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
    const NATIVE_SONG: &str = r#"[{"id":"3sG8kPq0WmZ1xYbN4vTcDe","libraryId":1,"libraryPath":"/music","libraryName":"Music Library","folderId":"5tR1yUi8OpA3sDf6GhJ9kL","path":"Marrow Choir/Winter Songs/03 - O Holy Night.flac","title":"O Holy Night","album":"Winter Songs","artist":"Marrow Choir","albumArtist":"Marrow Choir","albumId":"7hQ2nLr5VxZ0cYbM8wTkAf","trackNumber":3,"discNumber":1,"year":2019,"size":28734112,"suffix":"flac","duration":263.4,"bitRate":912,"mbzRecordingID":"0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c","missing":false,"birthTime":"2026-11-01T18:20:03.114208655+01:00","createdAt":"2026-11-01T18:20:07.551902337+01:00","updatedAt":"2026-11-01T18:20:03.114208655+01:00","rating":1,"playCount":3}]"#;
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

    // getPlaylists as alice sees it (server/subsonic/playlists.go:18-30, buildPlaylist :128-149,
    // responses.Playlist responses.go:305-327): `comment` and `public` are omitted when empty or
    // false, `owner` is the owner's name (:143), and `readonly` is always sent to a client that is
    // not legacy (responses.go:325), true for a playlist alice does not own.
    const PLAYLISTS: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"playlists":{"playlist":[{"id":"9fR2kLm4NpQ7sT1vW3xY5z","name":"Christmas","comment":"traditional","songCount":1,"duration":263,"public":true,"owner":"alice","created":"2026-11-15T09:12:03.418Z","changed":"2026-11-15T09:14:47.002Z","coverArt":"pl-9fR2kLm4NpQ7sT1vW3xY5z_6737f1d3","readonly":false},{"id":"3hJ8gFd2SaZ6xC4vB1nM7q","name":"Bob's mix","songCount":0,"duration":0,"owner":"bob","created":"2026-11-10T18:00:00Z","changed":"2026-11-10T18:00:00Z","coverArt":"pl-3hJ8gFd2SaZ6xC4vB1nM7q_6730f0a0","readonly":true}]}}}"#;
    // getStarred2 (server/subsonic/album_lists.go:190-202, responses.Starred2 responses.go:356-360,
    // ArtistID3 :231-248): empty lists are omitted.
    const STARRED: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"starred2":{"artist":[{"id":"2kD9pWq4ZxC1vBnM6yTjLs","name":"Marrow Choir","albumCount":1,"starred":"2026-11-03T09:10:00.000Z","musicBrainzId":"","sortName":"","roles":[]}]}}}"#;

    /// The song object inside SONG (it ends `"samplingRate":44100}}}`: song, body, envelope).
    fn song_child() -> &'static str {
        &SONG[SONG.find(r#""song":"#).unwrap() + 7..SONG.len() - 2]
    }

    /// getPlaylist of the first playlist of PLAYLISTS, with that song as its one entry
    /// (server/subsonic/playlists.go:41-57).
    fn playlist() -> String {
        format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"playlist":{{"id":"9fR2kLm4NpQ7sT1vW3xY5z","name":"Christmas","comment":"traditional","songCount":1,"duration":263,"public":true,"owner":"alice","created":"2026-11-15T09:12:03.418Z","changed":"2026-11-15T09:14:47.002Z","readonly":false,"entry":[{}]}}}}}}"#,
            song_child()
        )
    }

    #[tokio::test]
    async fn search3_asks_for_one_kind_from_an_offset() {
        let body = format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"searchResult3":{{"song":[{}]}}}}}}"#,
            song_child()
        );
        let (url, seen) = serve(vec![
            Reply { status: 200, body },
            Reply {
                status: 200,
                body: r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true}}"#.into(),
            },
        ]);
        let c = client(&url);
        let found = c
            .search3("o holy night", LibraryKind::Songs, 25, 26)
            .await
            .unwrap();
        let s = &found.song[0];
        assert_eq!(
            (
                s.suffix.as_deref(),
                s.bit_rate,
                s.size,
                s.music_brainz_id.as_deref(),
                s.track
            ),
            (
                Some("flac"),
                Some(912),
                Some(28734112),
                Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"),
                Some(3)
            )
        );
        let target = seen.lock().unwrap()[0].target.clone();
        for part in [
            "query=o+holy+night",
            "songCount=26",
            "songOffset=25",
            "albumCount=0",
            "artistCount=0",
        ] {
            assert!(
                target.split(['?', '&']).any(|p| p == part),
                "{part} in {target}"
            );
        }
        let err = c
            .search3("marrow", LibraryKind::Artists, 0, 26)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "search3 answered without searchResult3"),
            "{err:?}"
        );
        let target = seen.lock().unwrap()[1].target.clone();
        for part in [
            "artistCount=26",
            "artistOffset=0",
            "songCount=0",
            "albumCount=0",
        ] {
            assert!(
                target.split(['?', '&']).any(|p| p == part),
                "{part} in {target}"
            );
        }
    }

    #[tokio::test]
    async fn starred_items_and_playlists_are_read() {
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: STARRED.into(),
            },
            Reply {
                status: 200,
                body: PLAYLISTS.into(),
            },
            Reply {
                status: 200,
                body: playlist(),
            },
            Reply {
                status: 200,
                body: failed(70, "playlist not found"),
            },
            Reply {
                status: 200,
                body: playlist(),
            },
        ]);
        let c = client(&url);
        let starred = c.get_starred().await.unwrap();
        assert_eq!(
            (
                starred.artist[0].name.as_str(),
                starred.artist[0].album_count,
                starred.album.len(),
                starred.song.len()
            ),
            ("Marrow Choir", Some(1), 0, 0)
        );
        let lists = c.get_playlists().await.unwrap();
        assert_eq!(
            lists
                .iter()
                .map(|p| (
                    p.name.as_str(),
                    p.owner.as_str(),
                    p.public,
                    p.comment.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("Christmas", "alice", true, "traditional"),
                ("Bob's mix", "bob", false, "")
            ]
        );
        let pl = c
            .get_playlist("9fR2kLm4NpQ7sT1vW3xY5z")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                pl.name.as_str(),
                pl.song_count,
                pl.owner.as_str(),
                pl.public,
                pl.comment.as_str()
            ),
            ("Christmas", 1, "alice", true, "traditional")
        );
        assert!(c.get_playlist("gone").await.unwrap().is_none());
        let err = c.get_playlist("3hJ8gFd2SaZ6xC4vB1nM7q").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered another playlist for 3hJ8gFd2SaZ6xC4vB1nM7q"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_ok_answer_without_its_body_is_unreadable() {
        let empty = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true}}"#;
        let (url, _) = serve(
            (0..3)
                .map(|_| Reply {
                    status: 200,
                    body: empty.into(),
                })
                .collect(),
        );
        let c = client(&url);
        let unreadable = |e: NavidromeError| match e {
            NavidromeError::Parse(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            unreadable(c.get_starred().await.unwrap_err()),
            "getStarred2 answered without starred2"
        );
        assert_eq!(
            unreadable(c.get_playlists().await.unwrap_err()),
            "getPlaylists answered without playlists"
        );
        assert_eq!(
            unreadable(c.get_playlist("9fR2kLm4NpQ7sT1vW3xY5z").await.unwrap_err()),
            "getPlaylist answered without a playlist"
        );
    }

    #[tokio::test]
    async fn playlist_tracks_carry_their_place_and_song() {
        // GET /api/playlist/{id}/tracks rows are model.PlaylistTrack (model/playlist.go:224-229):
        // its own `id` is the track's place and hides the media file's, which is `mediaFileId`.
        // Place 3 is a track of a library alice cannot see (persistence/playlist_repository.go
        // tracksQuery, :378-400).
        let record = &NATIVE_SONG[1..NATIVE_SONG.len() - 1];
        let row = |place: &str, song: &str, missing: bool| {
            record
                .replace(
                    r#""id":"3sG8kPq0WmZ1xYbN4vTcDe""#,
                    &format!(
                        r#""id":"{place}","mediaFileId":"{song}","playlistId":"9fR2kLm4NpQ7sT1vW3xY5z""#
                    ),
                )
                .replace(r#""missing":false"#, &format!(r#""missing":{missing}"#))
        };
        let rows = format!(
            "[{},{},{}]",
            row("1", "s9", false),
            row("2", "s1", false),
            row("4", "s7", true)
        );
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 200,
                body: rows,
            },
            // http.Error(w, "not found", 404) when the playlist is not one alice may read
            // (server/nativeapi/playlists.go:38-48).
            Reply {
                status: 404,
                body: "not found\n".into(),
            },
            Reply {
                status: 200,
                body: format!("[{}]", row("x", "s9", false)),
            },
            // A row with its place and no mediaFileId.
            Reply {
                status: 200,
                body: format!(
                    "[{}]",
                    record.replace(r#""id":"3sG8kPq0WmZ1xYbN4vTcDe""#, r#""id":"1""#)
                ),
            },
            Reply {
                status: 200,
                body: format!("[{}]", row("0", "s9", false)),
            },
            // nginx in front of Navidrome forwarding /rest but not /api.
            Reply {
                status: 404,
                body: "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n".into(),
            },
        ]);
        let c = client(&url);
        let tracks = c
            .playlist_tracks("9fR2kLm4NpQ7sT1vW3xY5z")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tracks
                .iter()
                .map(|t| (t.position.get(), t.song.id.as_str(), t.song.missing))
                .collect::<Vec<_>>(),
            vec![(1, "s9", false), (2, "s1", false), (4, "s7", true)]
        );
        assert_eq!(tracks[0].song.title, "O Holy Night");
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/playlist/9fR2kLm4NpQ7sT1vW3xY5z/tracks?_sort=id&_order=ASC"
        );
        assert!(c.playlist_tracks("gone").await.unwrap().is_none());
        // A place that is no number, a row without mediaFileId, and place 0.
        for _ in 0..3 {
            let err = c
                .playlist_tracks("9fR2kLm4NpQ7sT1vW3xY5z")
                .await
                .unwrap_err();
            assert!(
                matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered a playlist track without its place or song id"),
                "{err:?}"
            );
        }
        let err = c
            .playlist_tracks("9fR2kLm4NpQ7sT1vW3xY5z")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Http { status: 404, raw: Some(raw), .. } if raw.contains("nginx")),
            "{err:?}"
        );
        // No playlist has an id that is not a path segment, so nothing is asked.
        for id in ["", ".", ".."] {
            assert!(c.playlist_tracks(id).await.unwrap().is_none(), "{id}");
        }
        assert_eq!(seen.lock().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn a_failed_native_answer_keeps_its_body_for_the_cause() {
        // deluan/rest's GetAll on a repository error (controller.go:58-65): RespondWithError's
        // JSON (render.go:9-23) with SQLite's text for SQLITE_BUSY, then, as GetAll does not
        // return there, the null list it goes on to write.
        let body = r#"{"error":"database is locked"}null"#;
        let (url, _) = serve(vec![
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 500,
                body: body.into(),
            },
            Reply {
                status: 500,
                body: "".into(),
            },
        ]);
        let c = client(&url);
        let err = c
            .playlist_tracks("9fR2kLm4NpQ7sT1vW3xY5z")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Get playlist tracks failed: HTTP 500");
        let cause = err
            .failure(shared::failure::Stage::LibraryCheck, err.to_string())
            .cause
            .unwrap();
        assert_eq!((cause.status, cause.raw.as_str()), (Some(500), body));
        // An empty body explains nothing, so the cause is the message.
        let err = c.find_songs("o holy night", 0, 100).await.unwrap_err();
        let cause = err
            .failure(shared::failure::Stage::LibraryCheck, "m")
            .cause
            .unwrap();
        assert_eq!(
            (cause.status, cause.raw.as_str()),
            (Some(500), "Find songs failed: HTTP 500")
        );
    }

    #[tokio::test]
    async fn an_album_carries_its_songs_and_year() {
        let (url, _) = serve(vec![Reply {
            status: 200,
            body: ALBUM.replace(r#""song":[]"#, &format!(r#""song":[{}]"#, song_child())),
        }]);
        let album = client(&url)
            .get_album("7hQ2nLr5VxZ0cYbM8wTkAf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (album.year, album.song.len(), album.song[0].id.as_str()),
            (Some(2019), 1, "3sG8kPq0WmZ1xYbN4vTcDe")
        );
    }

    #[test]
    fn a_native_record_carries_its_file_facts_and_annotations() {
        let song: NativeSong =
            serde_json::from_str(&NATIVE_SONG[1..NATIVE_SONG.len() - 1]).unwrap();
        assert_eq!(
            (
                song.suffix.as_str(),
                song.bit_rate,
                song.duration,
                song.album_id.as_str(),
                song.play_count,
                song.starred
            ),
            ("flac", 912, 263.4, "7hQ2nLr5VxZ0cYbM8wTkAf", 3, false)
        );
        assert_eq!(
            (song.track_number, song.year, song.starred_at),
            (3, 2019, None)
        );
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

    const ADVENT: &str = "6vN2bQ8rT4yH1kL9mX3zPa";
    const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true}}"#;
    const PLAYLIST: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"playlist":{"id":"6vN2bQ8rT4yH1kL9mX3zPa","name":"Advent","songCount":1,"duration":263,"owner":"alice","created":"2026-11-15T09:00:00.123456Z","changed":"2026-11-15T09:00:04.5Z","coverArt":"pl-6vN2bQ8rT4yH1kL9mX3zPa_6737a3d4","entry":[{"id":"3sG8kPq0WmZ1xYbN4vTcDe","parent":"7hQ2nLr5VxZ0cYbM8wTkAf","isDir":false,"title":"O Holy Night","album":"Winter Songs","artist":"Marrow Choir","track":3,"duration":263,"albumId":"7hQ2nLr5VxZ0cYbM8wTkAf","type":"music","mediaType":"song"}]}}}"#;
    /// model.Playlist as 0.64.2 marshals it (model/playlist.go:18-41).
    const NATIVE_PLAYLIST: &str = r#"{"id":"6vN2bQ8rT4yH1kL9mX3zPa","name":"Advent","comment":"","duration":263,"size":28734112,"songCount":1,"ownerName":"alice","ownerId":"8uY3tR6eW9qA2sD5fG8hJ1","public":false,"path":"","sync":false,"uploadedImage":"","createdAt":"2026-11-15T09:00:00.123456Z","updatedAt":"2026-11-15T09:00:04.5Z","rules":null,"evaluatedAt":null}"#;
    /// The rules create_smart_playlist sends, as Navidrome writes them back.
    const SMART_RULES: &str = r#"{"all":[{"contains":{"filepath":"Discovery/Explore"}}],"sort":"-dateAdded","order":"desc"}"#;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn form_values<'a>(body: &'a str, key: &str) -> Vec<&'a str> {
        body.split('&')
            .filter_map(|p| p.strip_prefix(&format!("{key}=")))
            .collect()
    }

    fn reply(body: impl Into<String>) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn a_playlist_is_created_with_its_songs_in_order() {
        let (url, seen) = serve(vec![reply(PLAYLIST)]);
        let made = client(&url)
            .create_playlist(
                "Advent",
                &ids(&["3sG8kPq0WmZ1xYbN4vTcDe", "0bF4hJ7kL2zX9cV5nM1qWe"]),
            )
            .await
            .unwrap();
        assert_eq!((made.id.as_str(), made.owner.as_str()), (ADVENT, "alice"));
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/rest/createPlaylist")
        );
        assert_eq!(form_values(&sent.body, "name"), vec!["Advent"]);
        assert_eq!(
            form_values(&sent.body, "songId"),
            vec!["3sG8kPq0WmZ1xYbN4vTcDe", "0bF4hJ7kL2zX9cV5nM1qWe"]
        );
    }

    #[tokio::test]
    async fn a_create_answered_without_a_playlist_is_unreadable() {
        let (url, _) = serve(vec![reply(OK)]);
        let err = client(&url)
            .create_playlist("Advent", &[])
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unreadable Navidrome response: createPlaylist answered without a playlist"
        );
    }

    #[tokio::test]
    async fn replacing_tracks_names_the_playlist_and_a_missing_one_is_false() {
        let (url, seen) = serve(vec![reply(PLAYLIST), reply(failed(70, "data not found"))]);
        let c = client(&url);
        assert!(c
            .replace_playlist_tracks(ADVENT, &ids(&["3sG8kPq0WmZ1xYbN4vTcDe"]))
            .await
            .unwrap());
        assert!(!c
            .replace_playlist_tracks("gone", &ids(&["3sG8kPq0WmZ1xYbN4vTcDe"]))
            .await
            .unwrap());
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (
                form_values(&sent.body, "playlistId"),
                form_values(&sent.body, "name")
            ),
            (vec![ADVENT], vec![])
        );
    }

    #[tokio::test]
    async fn entries_are_removed_by_index_and_appended_with_update_playlist() {
        let (url, seen) = serve(vec![
            reply(OK),
            reply(OK),
            reply(failed(70, "data not found")),
        ]);
        let c = client(&url);
        assert!(c.remove_playlist_tracks(ADVENT, &[0, 2]).await.unwrap());
        assert!(c
            .append_playlist_tracks(ADVENT, &ids(&["0bF4hJ7kL2zX9cV5nM1qWe"]))
            .await
            .unwrap());
        assert!(!c.remove_playlist_tracks("gone", &[0]).await.unwrap());
        let seen = seen.lock().unwrap();
        assert_eq!(
            (
                seen[0].target.as_str(),
                form_values(&seen[0].body, "songIndexToRemove")
            ),
            ("/rest/updatePlaylist", vec!["0", "2"])
        );
        assert_eq!(
            form_values(&seen[1].body, "songIdToAdd"),
            vec!["0bF4hJ7kL2zX9cV5nM1qWe"]
        );
    }

    #[tokio::test]
    async fn meta_sends_only_what_changes() {
        let (url, seen) = serve(vec![reply(OK), reply(OK), reply(OK)]);
        let c = client(&url);
        c.update_playlist_meta(
            ADVENT,
            &PlaylistMeta {
                name: Some("Advent 2026"),
                comment: Some(""),
                public: None,
            },
        )
        .await
        .unwrap();
        c.update_playlist_meta(
            ADVENT,
            &PlaylistMeta {
                name: None,
                comment: None,
                public: Some(true),
            },
        )
        .await
        .unwrap();
        c.update_playlist_meta(
            ADVENT,
            &PlaylistMeta {
                name: None,
                comment: None,
                public: Some(false),
            },
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            (
                form_values(&seen[0].body, "name"),
                form_values(&seen[0].body, "comment"),
                form_values(&seen[0].body, "public")
            ),
            (vec!["Advent+2026"], vec![""], vec![])
        );
        assert_eq!(
            (
                form_values(&seen[1].body, "name"),
                form_values(&seen[1].body, "public")
            ),
            (vec![], vec!["true"])
        );
        assert_eq!(form_values(&seen[2].body, "public"), vec!["false"]);
    }

    #[tokio::test]
    async fn a_delete_is_confirmed_or_keeps_navidromes_refusal() {
        // A non-owner's delete: DeletePlaylist answers ErrNotAuthorized with
        // ErrorAuthorizationFail (server/subsonic/playlists.go, responses/errors.go).
        let refusal = failed(50, "User is not authorized for the given operation");
        let (url, _) = serve(vec![
            reply(OK),
            reply(refusal),
            reply(failed(70, "data not found")),
        ]);
        let c = client(&url);
        assert!(c.delete_playlist(ADVENT).await.unwrap());
        let err = c.delete_playlist(ADVENT).await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Subsonic { code: 50, message } if message == "User is not authorized for the given operation"),
            "{err:?}"
        );
        assert!(!err.is_not_found());
        assert!(!c.delete_playlist("gone").await.unwrap());
    }

    #[tokio::test]
    async fn the_native_playlist_says_whether_it_is_smart_or_synced() {
        let smart =
            NATIVE_PLAYLIST.replace(r#""rules":null"#, &format!(r#""rules":{SMART_RULES}"#));
        let synced = NATIVE_PLAYLIST.replace(
            r#""path":"","sync":false"#,
            r#""path":"/music/Playlists/Advent.m3u","sync":true"#,
        );
        let (url, seen) = serve(vec![
            reply(LOGIN),
            reply(NATIVE_PLAYLIST),
            reply(smart),
            reply(synced),
            reply(NATIVE_PLAYLIST),
        ]);
        let c = client(&url);
        let p = c.get_native_playlist(ADVENT).await.unwrap().unwrap();
        assert_eq!(
            (p.id.as_str(), p.is_smart(), p.sync, p.song_count),
            (ADVENT, false, false, 1)
        );
        assert!(c
            .get_native_playlist(ADVENT)
            .await
            .unwrap()
            .unwrap()
            .is_smart());
        assert!(c.get_native_playlist(ADVENT).await.unwrap().unwrap().sync);
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/playlist/6vN2bQ8rT4yH1kL9mX3zPa"
        );
        // A record of another playlist than the one asked for is never taken for it.
        let err = c.get_native_playlist("other").await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Parse(m) if m == "Navidrome answered another playlist for other"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn only_navidromes_own_404_is_a_missing_native_playlist() {
        let (url, seen) = serve(vec![
            reply(LOGIN),
            Reply {
                status: 404,
                body: r#"{"error":"playlist(id:gone) not found"}"#.into(),
            },
            Reply {
                status: 404,
                body: "<html><title>404 Not Found</title></html>".into(),
            },
            Reply {
                status: 404,
                body: r#"{"error":"playlist(id:other) not found"}"#.into(),
            },
        ]);
        let c = client(&url);
        assert!(c.get_native_playlist("gone").await.unwrap().is_none());
        for _ in 0..2 {
            let err = c.get_native_playlist("gone").await.unwrap_err();
            assert!(
                matches!(
                    &err,
                    NavidromeError::Http {
                        action: "Get playlist",
                        status: 404,
                        ..
                    }
                ),
                "{err:?}"
            );
        }
        for id in ["", ".", ".."] {
            assert!(c.get_native_playlist(id).await.unwrap().is_none());
        }
        assert_eq!(seen.lock().unwrap().len(), 4, "those ids send nothing");
    }

    /// What 0.64.2 writes for GET /api/song/{id}/playlists: `json.Marshal(model.Playlists)`
    /// (model/playlist.go:18-42), `ownerName` joined in (persistence/playlist_repository.go:258-262).
    const SONG_PLAYLISTS: &str = r#"[{"id":"6vN2bQ8rT4yH1kL9mX3zPa","name":"Advent","comment":"","duration":2841.2,"size":341200231,"songCount":12,"ownerName":"bob","ownerId":"2b8f1c0e7d","public":false,"path":"","sync":false,"uploadedImage":"","createdAt":"2026-11-02T18:04:51.112Z","updatedAt":"2026-11-03T09:12:40.004Z","rules":null,"evaluatedAt":null},{"id":"0aQ7wE3rT6yU9iO2pL5kJh","name":"Recently added","comment":"","duration":0,"size":0,"songCount":50,"ownerName":"alice","ownerId":"8uY3tR6eW9","public":false,"path":"","sync":false,"uploadedImage":"","createdAt":"2026-11-02T18:04:51.112Z","updatedAt":"2026-11-03T09:12:40.004Z","rules":{"all":[{"inTheLast":{"dateAdded":30}}],"sort":"-dateAdded","limit":50},"evaluatedAt":"2026-11-15T10:00:00Z"}]"#;

    #[tokio::test]
    async fn a_songs_playlists_are_read_with_their_owner_and_rules() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: LOGIN.into(),
            },
            Reply {
                status: 200,
                body: SONG_PLAYLISTS.into(),
            },
            Reply {
                status: 200,
                body: "[]".into(),
            },
        ]);
        let c = client(&url);
        let lists = c
            .get_song_playlists("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap();
        assert_eq!(
            lists
                .iter()
                .map(|p| (p.name.as_str(), p.owner_name.as_str(), p.is_smart()))
                .collect::<Vec<_>>(),
            vec![("Advent", "bob", false), ("Recently added", "alice", true)]
        );
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/song/3sG8kPq0WmZ1xYbN4vTcDe/playlists"
        );
        assert!(c.get_song_playlists("gone").await.unwrap().is_empty());
        for id in ["", ".", ".."] {
            assert!(
                matches!(c.get_song_playlists(id).await, Err(NavidromeError::Parse(m)) if m == "No song id given")
            );
        }
        assert_eq!(seen.lock().unwrap().len(), 3, "those ids send nothing");
    }

    #[tokio::test]
    async fn a_song_playlists_answer_that_cannot_be_read_is_an_error_not_an_empty_list() {
        // A failed query: http.Error's 500 with the error's text (server/nativeapi/playlists.go:226-228).
        let without_owner = SONG_PLAYLISTS.replace(r#""ownerName":"bob","#, "");
        let (url, _) = serve(vec![
            reply(LOGIN),
            Reply {
                status: 500,
                body: "database is locked\n".into(),
            },
            reply("null"),
            reply(without_owner),
        ]);
        let c = client(&url);
        let err = c
            .get_song_playlists("3sG8kPq0WmZ1xYbN4vTcDe")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Http { action: "Get song playlists", status: 500, raw: Some(raw), .. } if raw == "database is locked\n"),
            "{err:?}"
        );
        for _ in 0..2 {
            let err = c
                .get_song_playlists("3sG8kPq0WmZ1xYbN4vTcDe")
                .await
                .unwrap_err();
            assert!(matches!(err, NavidromeError::Parse(_)), "{err:?}");
        }
    }

    /// nginx's 502 page, as step 8's one_failed_lookup_makes_the_result_partial sends it.
    const BAD_GATEWAY: &str = "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n";

    #[tokio::test]
    async fn present_songs_are_listed_a_page_at_a_time_in_the_order_navidrome_added_them() {
        let (url, seen) = serve_with_headers(vec![
            (reply(LOGIN), vec![]),
            (reply(NATIVE_SONG), vec![("X-Total-Count", "1".into())]),
            (reply(NATIVE_SONG), vec![]),
            (
                Reply {
                    status: 502,
                    body: BAD_GATEWAY.into(),
                },
                vec![],
            ),
        ]);
        let c = client(&url);
        let page = c.list_songs(0, 1000).await.unwrap();
        assert_eq!(
            (
                page.songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                page.total
            ),
            (vec!["3sG8kPq0WmZ1xYbN4vTcDe"], 1)
        );
        assert_eq!(
            seen.lock().unwrap()[1].target,
            "/api/song?_start=0&_end=1000&_sort=recently_added&_order=ASC&missing=false"
        );
        // Without the count, a page cannot be checked against it.
        let err = c.list_songs(0, 1000).await.unwrap_err();
        assert!(matches!(&err, NavidromeError::Parse(_)), "{err:?}");
        let err = c.list_songs(1000, 2000).await.unwrap_err();
        assert!(
            matches!(&err, NavidromeError::Http { action: "List songs", status: 502, body: None, raw: Some(r) } if r == BAD_GATEWAY),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn native_songs_are_read_by_id_a_hundred_at_a_time() {
        let (url, seen) = serve(vec![reply(LOGIN), reply(NATIVE_SONG), reply("[]")]);
        let c = client(&url);
        let mut wanted: Vec<String> = (0..101).map(|i| format!("{i:022}")).collect();
        wanted.push(String::new());
        let found = c.get_native_songs(&wanted).await.unwrap();
        assert_eq!(
            found.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["3sG8kPq0WmZ1xYbN4vTcDe"]
        );
        assert!(c.get_native_songs(&[]).await.unwrap().is_empty());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "an empty list sends nothing");
        assert!(
            seen[1]
                .target
                .starts_with("/api/song?_start=0&_end=100&id="),
            "{}",
            seen[1].target
        );
        assert_eq!(
            (
                seen[1].target.matches("id=").count(),
                seen[2].target.matches("id=").count()
            ),
            (100, 1)
        );
    }

    #[tokio::test]
    async fn a_song_or_album_is_starred_and_unstarred() {
        let (url, seen) = serve(vec![reply(OK), reply(OK)]);
        let c = client(&url);
        c.set_star(StarTarget::Song("3sG8kPq0WmZ1xYbN4vTcDe"), true)
            .await
            .unwrap();
        c.set_star(StarTarget::Album("7hQ2nLr5VxZ0cYbM8wTkAf"), false)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            (seen[0].target.as_str(), form_values(&seen[0].body, "id")),
            ("/rest/star", vec!["3sG8kPq0WmZ1xYbN4vTcDe"])
        );
        assert_eq!(
            (
                seen[1].target.as_str(),
                form_values(&seen[1].body, "albumId")
            ),
            ("/rest/unstar", vec!["7hQ2nLr5VxZ0cYbM8wTkAf"])
        );
    }
}
