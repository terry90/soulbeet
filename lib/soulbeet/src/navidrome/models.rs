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
pub struct UserBody {
    pub user: SubsonicUser,
}

/// The signed-in user's own record (server/subsonic/responses/responses.go
/// `User`); Navidrome answers no other user's (server/subsonic/users.go
/// `GetUser`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicUser {
    pub admin_role: bool,
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

/// One search3 answer (responses.SearchResult3, server/subsonic/responses/responses.go:344-348):
/// an empty list is omitted.
#[derive(Debug, Deserialize)]
pub struct SearchResult3 {
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
    #[serde(default)]
    pub album: Vec<SubsonicAlbum>,
    #[serde(default)]
    pub artist: Vec<SubsonicArtist>,
}

/// What search3 searches for in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryKind {
    Songs,
    Albums,
    Artists,
}

/// getStarred2's body. Option so that an ok answer without it is reported as such.
#[derive(Debug, Deserialize)]
pub struct Starred2Body {
    #[serde(rename = "starred2")]
    pub starred: Option<Starred2>,
}

/// What this user starred (responses.Starred2, responses.go:356-360): an empty list is omitted.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Starred2 {
    pub artist: Vec<SubsonicArtist>,
    pub album: Vec<SubsonicAlbum>,
    pub song: Vec<SubsonicSong>,
}

/// getPlaylists' body. Option so that an ok answer without it is reported as such.
#[derive(Debug, Deserialize)]
pub struct PlaylistsBody {
    pub playlists: Option<Playlists>,
}

/// responses.Playlists (responses.go:329-331): an empty list is omitted.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Playlists {
    pub playlist: Vec<SubsonicPlaylist>,
}

/// getPlaylist's body. Option so that an ok answer without its playlist is reported as such.
/// Its `entry` list is not read: it leaves out missing files and tracks of libraries the user
/// cannot see, so an entry's index is not its place in the playlist.
#[derive(Debug, Deserialize)]
pub struct PlaylistBody {
    pub playlist: Option<SubsonicPlaylist>,
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
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub play_count: Option<u32>,
    /// When this user starred the song; absent when not starred.
    #[serde(default)]
    pub starred: Option<String>,
    /// The recording MBID (OpenSubsonic, sent empty when unknown).
    #[serde(default, rename = "musicBrainzId")]
    pub music_brainz_id: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
    /// kbps
    #[serde(default)]
    pub bit_rate: Option<u32>,
    /// bytes
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub track: Option<u32>,
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
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub play_count: Option<u64>,
    /// getAlbum's songs; empty in album lists.
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
}

/// responses.ArtistID3 (responses.go:231-248), as search3 and getStarred2 send it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicArtist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album_count: Option<u32>,
    /// When this user starred the artist; absent when not starred.
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(default)]
    pub user_rating: Option<u8>,
    /// OpenSubsonic, sent empty when unknown.
    #[serde(default, rename = "musicBrainzId")]
    pub music_brainz_id: Option<String>,
}

/// responses.Playlist (responses.go:305-327), as buildPlaylist fills it
/// (server/subsonic/playlists.go:128-149): `comment` and `public` are omitted when empty or false.
/// Navidrome leaves `owner` out only for a player it treats as a minimal client (:137-140); such
/// an answer fails to parse, since ownership cannot be told without it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub comment: String,
    pub song_count: u32,
    /// seconds
    pub duration: u32,
    #[serde(default)]
    pub public: bool,
    /// The owner's user name (:143).
    pub owner: String,
    pub created: String,
    pub changed: String,
}

/// The native playlist record (model.Playlist, model/playlist.go:18-42), as far as a write and
/// the delete guard need it. Name and owner are always written, so an answer without them does
/// not parse.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativePlaylist {
    pub id: String,
    pub name: String,
    /// The owner's user name, joined in (persistence/playlist_repository.go:258-262).
    pub owner_name: String,
    /// Replaced from its playlist file when the file changes (core/playlists/playlists.go).
    #[serde(default)]
    pub sync: bool,
    /// The stored count of every row, rows the account cannot see included
    /// (persistence/playlist_repository.go refreshCounters).
    #[serde(default)]
    pub song_count: u32,
    /// Smart playlist rules, an object; `null` for a static playlist (model/criteria/criteria.go).
    #[serde(default)]
    pub rules: Option<serde_json::Value>,
}

impl NativePlaylist {
    /// Its tracks come from `rules`, not from rows.
    pub fn is_smart(&self) -> bool {
        self.rules.as_ref().is_some_and(|r| !r.is_null())
    }
}

/// A row of a playlist's native track list (GET /api/playlist/{id}/tracks).
#[derive(Debug, Clone)]
pub struct PlaylistTrack {
    /// The track's 1-based place among all of the playlist's tracks, the row's own `id`, which
    /// Navidrome renumbers 1 to n after a removal (persistence/playlist_repository.go:487-488).
    /// Tracks Navidrome does not list to this user leave gaps.
    pub position: std::num::NonZeroU32,
    pub song: NativeSong,
}

impl SubsonicAlbum {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("Unknown Album")
    }
}

/// A song from Navidrome's native API (GET /api/song), as the logged-in user sees it
/// (model/mediafile.go, model/annotation.go).
/// The `path` field is the raw `media_file.path` from the database,
/// relative to the library root.
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
    // The fields below are 0 or empty when Navidrome does not know them (model/mediafile.go:49-63),
    // and the annotations are `omitempty` (model/annotation.go:6-12).
    #[serde(default)]
    pub album_id: String,
    #[serde(default)]
    pub track_number: u32,
    #[serde(default)]
    pub year: u32,
    /// seconds
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub suffix: String,
    /// kbps
    #[serde(default)]
    pub bit_rate: u32,
    /// Whether this user starred the song.
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub starred_at: Option<String>,
    /// This user's plays.
    #[serde(default)]
    pub play_count: u64,
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
