use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::error::{CoreError, CoreResult};
use crate::models::{AudioQualityProfile, NavidromeCredentials};

/// Pure OpenSubsonic HTTP client for Navidrome sync, streaming URLs, cover art, and curation.
#[derive(Debug, Clone)]
pub struct SubsonicClient {
    http: Client,
    creds: NavidromeCredentials,
}

impl SubsonicClient {
    pub fn new(http: Client, creds: NavidromeCredentials) -> Self {
        Self { http, creds }
    }

    pub fn credentials(&self) -> &NavidromeCredentials {
        &self.creds
    }

    fn endpoint_url(&self, endpoint: &str, extra: &[(&str, String)]) -> CoreResult<Url> {
        let base = format!(
            "{}/rest/{}",
            self.creds.normalized_base_url(),
            endpoint.trim_start_matches('/')
        );
        let mut url = Url::parse(&base)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("u", &self.creds.username);
            q.append_pair("t", &self.creds.subsonic_token);
            q.append_pair("s", &self.creds.subsonic_salt);
            q.append_pair("v", &self.creds.api_version);
            q.append_pair("c", &self.creds.client_name);
            q.append_pair("f", "json");
            for (k, v) in extra {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// Profile-aware playback stream URL (`FLAC Direct` vs `Opus 160 kbps` transcode).
    pub fn stream_url(&self, song_id: &str, profile: &AudioQualityProfile) -> CoreResult<String> {
        let mut params = vec![("id", song_id.to_string())];
        match profile {
            AudioQualityProfile::LosslessFlac => {
                params.push(("format", "raw".to_string()));
            }
            AudioQualityProfile::CellularOpus160 => {
                params.push(("format", "opus".to_string()));
                params.push(("maxBitRate", "160".to_string()));
            }
            AudioQualityProfile::CustomTranscode {
                format,
                max_bit_rate,
            } => {
                params.push(("format", format.clone()));
                params.push(("maxBitRate", max_bit_rate.to_string()));
            }
        }
        Ok(self.endpoint_url("stream", &params)?.to_string())
    }

    /// Profile-aware download URL: raw file endpoint for lossless, transcoded stream for Opus.
    pub fn download_url(&self, song_id: &str, profile: &AudioQualityProfile) -> CoreResult<String> {
        match profile {
            AudioQualityProfile::LosslessFlac => Ok(self
                .endpoint_url("download", &[("id", song_id.to_string())])?
                .to_string()),
            AudioQualityProfile::CellularOpus160 | AudioQualityProfile::CustomTranscode { .. } => {
                self.stream_url(song_id, profile)
            }
        }
    }

    pub fn cover_art_url(&self, cover_art_id: &str, size: Option<u32>) -> CoreResult<String> {
        let mut params = vec![("id", cover_art_id.to_string())];
        if let Some(sz) = size {
            params.push(("size", sz.to_string()));
        }
        Ok(self.endpoint_url("getCoverArt", &params)?.to_string())
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        extra: &[(&str, String)],
    ) -> CoreResult<T> {
        let url = self.endpoint_url(endpoint, extra)?;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }
        let envelope: SubsonicEnvelope<T> = resp.json().await?;
        let inner = envelope.subsonic_response;
        if inner.status != "ok" {
            let err = inner.error.unwrap_or(SubsonicErrorPayload {
                code: 0,
                message: "unknown subsonic error".to_string(),
            });
            return Err(CoreError::Subsonic {
                code: err.code,
                message: err.message,
            });
        }
        Ok(inner.body)
    }

    pub async fn get_artists(&self) -> CoreResult<Vec<SubsonicArtist>> {
        let payload: ArtistsPayload = self.get_json("getArtists", &[]).await?;
        let mut out = Vec::new();
        if let Some(artists) = payload.artists {
            for idx in artists.index {
                out.extend(idx.artist);
            }
        }
        Ok(out)
    }

    pub async fn get_album_list2(
        &self,
        list_type: &str,
        size: usize,
        offset: usize,
    ) -> CoreResult<Vec<SubsonicAlbumSummary>> {
        let payload: AlbumList2Payload = self
            .get_json(
                "getAlbumList2",
                &[
                    ("type", list_type.to_string()),
                    ("size", size.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        Ok(payload.album_list2.map(|l| l.album).unwrap_or_default())
    }

    pub async fn get_album(&self, album_id: &str) -> CoreResult<SubsonicAlbumDetail> {
        let payload: AlbumDetailPayload = self
            .get_json("getAlbum", &[("id", album_id.to_string())])
            .await?;
        payload.album.ok_or_else(|| CoreError::Subsonic {
            code: 70,
            message: format!("album {album_id} not found"),
        })
    }

    pub async fn get_playlists(&self) -> CoreResult<Vec<SubsonicPlaylistSummary>> {
        let payload: PlaylistsPayload = self.get_json("getPlaylists", &[]).await?;
        Ok(payload.playlists.map(|p| p.playlist).unwrap_or_default())
    }

    pub async fn get_playlist(&self, playlist_id: &str) -> CoreResult<SubsonicPlaylistDetail> {
        let payload: PlaylistDetailPayload = self
            .get_json("getPlaylist", &[("id", playlist_id.to_string())])
            .await?;
        payload.playlist.ok_or_else(|| CoreError::Subsonic {
            code: 70,
            message: format!("playlist {playlist_id} not found"),
        })
    }

    pub async fn get_starred2(&self) -> CoreResult<SubsonicStarred2> {
        let payload: Starred2Payload = self.get_json("getStarred2", &[]).await?;
        Ok(payload.starred2.unwrap_or_default())
    }

    pub async fn get_genres(&self) -> CoreResult<Vec<SubsonicGenre>> {
        let payload: GenresPayload = self.get_json("getGenres", &[]).await?;
        Ok(payload.genres.map(|g| g.genre).unwrap_or_default())
    }

    /// Lightweight paginated song-only `search3` lookup (used for incremental rating/annotation refresh).
    pub async fn search3_songs(
        &self,
        query: &str,
        song_count: usize,
        song_offset: usize,
    ) -> CoreResult<Vec<SubsonicSong>> {
        let payload: Search3Payload = self
            .get_json(
                "search3",
                &[
                    ("query", query.to_string()),
                    ("artistCount", "0".to_string()),
                    ("albumCount", "0".to_string()),
                    ("songCount", song_count.to_string()),
                    ("songOffset", song_offset.to_string()),
                ],
            )
            .await?;
        Ok(payload.search_result3.map(|r| r.song).unwrap_or_default())
    }

    pub async fn scrobble(
        &self,
        song_id: &str,
        submission: bool,
        time_ms: Option<i64>,
    ) -> CoreResult<()> {
        let mut params = vec![
            ("id", song_id.to_string()),
            ("submission", submission.to_string()),
        ];
        if let Some(ms) = time_ms {
            params.push(("time", ms.to_string()));
        }
        let _: EmptyPayload = self.get_json("scrobble", &params).await?;
        Ok(())
    }

    pub async fn star(&self, song_id: &str, starred: bool) -> CoreResult<()> {
        let endpoint = if starred { "star" } else { "unstar" };
        let _: EmptyPayload = self
            .get_json(endpoint, &[("id", song_id.to_string())])
            .await?;
        Ok(())
    }

    pub async fn set_rating(&self, song_id: &str, rating: u8) -> CoreResult<()> {
        let _: EmptyPayload = self
            .get_json(
                "setRating",
                &[
                    ("id", song_id.to_string()),
                    ("rating", rating.min(5).to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn fetch_bytes(&self, url: &str) -> CoreResult<(Vec<u8>, Option<String>)> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        let bytes = resp.bytes().await?.to_vec();
        Ok((bytes, content_type))
    }
}

/// Returns true if an HTTP 200 media response `Content-Type` indicates a Subsonic JSON/XML/text
/// error envelope rather than binary audio or image bytes.
pub fn is_subsonic_error_content_type(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return false;
    };
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/json" | "text/xml" | "application/xml" | "text/plain" | "text/html"
    )
}

/// Parses a Subsonic HTTP 200 error payload (JSON envelope, XML `<error code="..." message="..."/>`,
/// or empty/plain-text body) into [`CoreError::Subsonic`].
pub fn parse_subsonic_error_bytes(bytes: &[u8], content_type: Option<&str>) -> CoreError {
    if bytes.is_empty() {
        return CoreError::Subsonic {
            code: 0,
            message: "empty media response from Subsonic server".to_string(),
        };
    }
    if let Ok(envelope) = serde_json::from_slice::<SubsonicEnvelope<EmptyPayload>>(bytes) {
        if let Some(err) = envelope.subsonic_response.error {
            return CoreError::Subsonic {
                code: err.code,
                message: err.message,
            };
        }
        return CoreError::Subsonic {
            code: 0,
            message: format!(
                "unexpected Subsonic JSON response (status={})",
                envelope.subsonic_response.status
            ),
        };
    }

    let text = String::from_utf8_lossy(bytes);
    if let Some(code) = extract_xml_attr(&text, "code").and_then(|c| c.parse::<i64>().ok()) {
        let message = extract_xml_attr(&text, "message")
            .unwrap_or_else(|| text.trim().to_string());
        return CoreError::Subsonic { code, message };
    }

    let snippet: String = text.trim().chars().take(200).collect();
    let ct = content_type.unwrap_or("unknown");
    CoreError::Subsonic {
        code: 0,
        message: if snippet.is_empty() {
            format!("unexpected {ct} response instead of media bytes")
        } else {
            format!("unexpected {ct} response: {snippet}")
        },
    }
}

fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{attr}={quote}");
        if let Some(start) = xml.find(&needle) {
            let rest = &xml[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

// --- OpenSubsonic Wire Deserialization ---

#[derive(Debug, Deserialize)]
struct SubsonicEnvelope<T> {
    #[serde(rename = "subsonic-response")]
    subsonic_response: SubsonicResponseInner<T>,
}

#[derive(Debug, Deserialize)]
struct SubsonicResponseInner<T> {
    status: String,
    #[serde(default)]
    error: Option<SubsonicErrorPayload>,
    #[serde(flatten)]
    body: T,
}

#[derive(Debug, Deserialize)]
struct SubsonicErrorPayload {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct EmptyPayload {}

#[derive(Debug, Deserialize)]
struct ArtistsPayload {
    #[serde(default)]
    artists: Option<ArtistsIndexContainer>,
}

#[derive(Debug, Deserialize)]
struct ArtistsIndexContainer {
    #[serde(default)]
    index: Vec<ArtistIndexBucket>,
}

#[derive(Debug, Deserialize)]
struct ArtistIndexBucket {
    #[serde(default)]
    artist: Vec<SubsonicArtist>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicArtist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album_count: i64,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default)]
    pub music_brainz_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumList2Payload {
    #[serde(default)]
    album_list2: Option<AlbumList2Container>,
}

#[derive(Debug, Deserialize)]
struct AlbumList2Container {
    #[serde(default)]
    album: Vec<SubsonicAlbumSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicAlbumSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub song_count: i64,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumDetailPayload {
    #[serde(default)]
    album: Option<SubsonicAlbumDetail>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicAlbumDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub song_count: i64,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicSong {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub track: Option<i64>,
    #[serde(default)]
    pub disc_number: Option<i64>,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub bit_rate: Option<i64>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default)]
    pub user_rating: Option<i64>,
    #[serde(default)]
    pub play_count: Option<i64>,
    #[serde(default)]
    pub music_brainz_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlaylistsPayload {
    #[serde(default)]
    playlists: Option<PlaylistsContainer>,
}

#[derive(Debug, Deserialize)]
struct PlaylistsContainer {
    #[serde(default)]
    playlist: Vec<SubsonicPlaylistSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicPlaylistSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub song_count: i64,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub changed: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlaylistDetailPayload {
    #[serde(default)]
    playlist: Option<SubsonicPlaylistDetail>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicPlaylistDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub song_count: i64,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub changed: Option<String>,
    #[serde(default)]
    pub entry: Vec<SubsonicSong>,
}

#[derive(Debug, Deserialize)]
struct Starred2Payload {
    #[serde(default)]
    starred2: Option<SubsonicStarred2>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubsonicStarred2 {
    #[serde(default)]
    pub artist: Vec<SubsonicArtist>,
    #[serde(default)]
    pub album: Vec<SubsonicAlbumSummary>,
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
}

#[derive(Debug, Deserialize)]
struct GenresPayload {
    #[serde(default)]
    genres: Option<GenresContainer>,
}

#[derive(Debug, Deserialize)]
struct GenresContainer {
    #[serde(default)]
    genre: Vec<SubsonicGenre>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicGenre {
    pub value: String,
    #[serde(default)]
    pub song_count: i64,
    #[serde(default)]
    pub album_count: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Search3Payload {
    #[serde(default)]
    search_result3: Option<SearchResult3Container>,
}

#[derive(Debug, Deserialize)]
struct SearchResult3Container {
    #[serde(default)]
    song: Vec<SubsonicSong>,
}
