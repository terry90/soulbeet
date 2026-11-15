use serde::Deserialize;

/// Generic Subsonic API response wrapper.
/// The API nests all responses under `subsonic-response`.
#[derive(Debug, Deserialize)]
pub struct SubsonicEnvelope<T> {
    #[serde(rename = "subsonic-response")]
    pub response: SubsonicResponse<T>,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicResponse<T> {
    pub status: String,
    pub error: Option<SubsonicError>,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicError {
    pub code: u32,
    pub message: String,
}

// Body types for different endpoints

#[derive(Debug, Deserialize)]
pub struct PingBody {}

#[derive(Debug, Deserialize)]
pub struct ScanStatusBody {
    #[serde(rename = "scanStatus")]
    pub scan_status: ScanStatus,
}

/// Navidrome's scan status (server/subsonic/responses/responses.go
/// `ScanStatus`). `lastScan` is always sent and is `0001-01-01T00:00:00Z`
/// before the first scan.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: i64,
    pub last_scan: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct AlbumList2Body {
    #[serde(rename = "albumList2")]
    pub album_list: Option<AlbumList2>,
}

#[derive(Debug, Deserialize)]
pub struct AlbumList2 {
    #[serde(default)]
    pub album: Vec<SubsonicAlbum>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult3Body {
    #[serde(rename = "searchResult3")]
    pub search_result: Option<SearchResult3>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult3 {
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
}

// Core data types

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicSong {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub average_rating: Option<f64>,
    #[serde(default)]
    pub play_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicAlbum {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub average_rating: Option<f64>,
}

impl SubsonicAlbum {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("Unknown Album")
    }
}

/// A player registered with Navidrome (native API: GET /api/player).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub id: String,
    pub name: String,
    pub client: String,
    pub report_real_path: bool,
}

/// A song from Navidrome's native API (GET /api/song).
/// The `path` field is the raw `media_file.path` from the database,
/// relative to the library root (not transformed by ReportRealPath).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSong {
    pub id: String,
    pub path: String,
    pub title: String,
}
