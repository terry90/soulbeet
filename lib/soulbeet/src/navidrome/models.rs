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

/// Navidrome's own version, sent on every answer (server/subsonic/helpers.go:32,
/// responses/responses.go:14); absent from a server that is not Navidrome.
#[derive(Debug, Deserialize)]
pub struct VersionBody {
    #[serde(rename = "serverVersion")]
    pub server_version: Option<String>,
}

/// Whether the Navidrome reporting `server_version` keeps every song's average rating, which
/// release 0.60.0 added (model/annotation.go:12 `AverageRating`; 0.59.0 has none). The version
/// reads "{release} ({sha})" (consts/version.go:20-26), and a local build's release is the last
/// tag with "-SNAPSHOT" appended (Makefile:12), so a snapshot is at least that release. "dev",
/// a branch name, a release candidate or anything else that is not three numbers is not known
/// to keep them.
pub fn keeps_song_averages(server_version: &str) -> bool {
    let release = server_version.split(' ').next().unwrap_or_default();
    let release = release.strip_suffix("-SNAPSHOT").unwrap_or(release);
    let numbers: Option<Vec<u32>> = release
        .split('.')
        .map(|n| {
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            n.parse().ok()
        })
        .collect();
    matches!(numbers.as_deref(), Some(&[major, minor, _]) if (major, minor) >= (0, 60))
}

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

/// getSong's body. Option so that an ok answer without its song is reported as such.
#[derive(Debug, Deserialize)]
pub struct SongBody {
    pub song: Option<SubsonicSong>,
}

/// getAlbum's body. Option so that an ok answer without its album is reported as such.
#[derive(Debug, Deserialize)]
pub struct AlbumBody {
    pub album: Option<SubsonicAlbum>,
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
    pub play_count: Option<u32>,
    /// When this user starred the song; absent when not starred.
    #[serde(default)]
    pub starred: Option<String>,
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
    /// When this user starred the album; absent when not starred.
    #[serde(default)]
    pub starred: Option<String>,
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

/// A song from Navidrome's native API (GET /api/song), as the logged-in user sees it
/// (model/mediafile.go, model/annotation.go).
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
    pub library_id: i64,
    /// The library's root as Navidrome sees it.
    pub library_path: String,
    /// The file is no longer found in the library's folder.
    pub missing: bool,
    /// The file's modification time as Navidrome's scanner last read it (model/mediafile.go:104,
    /// model/metadata/map_mediafile.go:80), or when Navidrome last marked the song missing or
    /// found again (persistence/mediafile_repository.go:461, :477).
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    /// This user's rating, 0 when unrated (`omitempty`).
    #[serde(default)]
    pub rating: u8,
    /// The mean of every user's rating above 0. `omitempty`, so absent when nobody rated the
    /// song, and on Navidrome before 0.60.0, which has no average.
    #[serde(default)]
    pub average_rating: Option<f64>,
    #[serde(default, rename = "mbzRecordingID")]
    pub mbz_recording_id: Option<String>,
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
    fn song_averages_are_kept_from_release_0_60_0_on() {
        for (version, keeps) in [
            ("0.60.3 (34c6f12a)", true),
            ("0.60.0 (1b2c3d4e)", true),
            ("1.0.0 (1b2c3d4e)", true),
            ("0.64.2", true),
            ("0.60.0-SNAPSHOT (1b2c3d4e)", true),
            ("0.59.0 (fa46aefa)", false),
            ("0.59.0-SNAPSHOT (fa46aefa)", false),
            ("0.60.0-rc1 (1b2c3d4e)", false),
            ("dev", false),
            ("master (9ed35cb)", false),
            ("0.60 (1b2c3d4e)", false),
            ("0.60.x (1b2c3d4e)", false),
            ("", false),
        ] {
            assert_eq!(keeps_song_averages(version), keeps, "{version}");
        }
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
