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
    #[serde(flatten)]
    pub body: T,
}

/// The status and error of an answer, read before its body so that a refusal
/// is reported whatever body the endpoint would have had.
#[derive(Debug, Deserialize)]
pub struct SubsonicAnswer {
    #[serde(rename = "subsonic-response")]
    pub response: SubsonicOutcome,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicOutcome {
    pub status: String,
    pub error: Option<SubsonicError>,
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
    /// Nanoseconds (`int64(status.ElapsedTime)`, server/subsonic/library_scanning.go:31); omitted when zero.
    #[serde(default)]
    pub elapsed_time: i64,
}

impl ScanStatus {
    /// Navidrome is idle and its last scan ended after `before`.
    pub fn ended_after(&self, before: chrono::DateTime<chrono::Utc>) -> bool {
        !self.scanning && self.last_scan > before
    }

    /// When the last scan began, read while Navidrome is idle: `lastScan - elapsedTime`, since an idle
    /// Navidrome sends `elapsedTime` as the last scan time less the start it stored
    /// (scanner/controller.go:131-143, scanner/scanner.go:112). None while scanning, when
    /// `elapsedTime` counts from that start to now.
    pub fn last_start(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.scanning {
            return None;
        }
        self.last_scan
            .checked_sub_signed(chrono::Duration::nanoseconds(self.elapsed_time))
    }
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
    /// bytes (model/mediafile.go `Size`)
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_scan_began_its_length_before_it_ended() {
        let parse = |json: &str| serde_json::from_str::<ScanStatus>(json).unwrap();
        let idle = parse(
            r#"{"scanning":false,"count":1342,"folderCount":120,"lastScan":"2026-11-15T10:02:00Z","scanType":"quick","elapsedTime":4000000000}"#,
        );
        assert_eq!(
            idle.last_start(),
            Some("2026-11-15T10:01:56Z".parse().unwrap())
        );
        let failed = parse(
            r#"{"scanning":false,"count":1342,"folderCount":120,"lastScan":"2026-11-15T10:02:00Z","error":"running GC: purging empty albums: database is locked","scanType":"quick","elapsedTime":-30000000000}"#,
        );
        assert_eq!(
            failed.last_start(),
            Some("2026-11-15T10:02:30Z".parse().unwrap())
        );
        let never = parse(
            r#"{"scanning":false,"count":0,"folderCount":0,"lastScan":"0001-01-01T00:00:00Z"}"#,
        );
        assert_eq!(never.last_start(), Some(never.last_scan));
        let running = parse(
            r#"{"scanning":true,"count":12,"folderCount":3,"lastScan":"2026-11-15T10:02:00Z","scanType":"quick","elapsedTime":9000000000}"#,
        );
        assert_eq!(running.last_start(), None);
    }

    #[test]
    fn a_scan_has_ended_once_a_later_one_ended() {
        let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        let status = |scanning, last: &str| ScanStatus {
            scanning,
            count: 0,
            last_scan: at(last),
            error: String::new(),
            elapsed_time: 0,
        };
        let before = at("2026-11-15T10:00:00Z");
        assert!(!status(false, "2026-11-15T10:00:00Z").ended_after(before));
        assert!(!status(true, "2026-11-15T10:00:05Z").ended_after(before));
        assert!(status(false, "2026-11-15T10:00:05Z").ended_after(before));
        assert!(status(false, "2026-11-15T10:00:05Z").ended_after(at("0001-01-01T00:00:00Z")));
    }
}
