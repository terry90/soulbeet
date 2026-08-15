use std::collections::HashMap;
use std::path::Path;

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub username: String,
    pub filename: String,
    pub file_size: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DownloadResponse {
    pub username: String,
    pub filename: String,
    pub size: u64,
    pub error: Option<String>,
}

/// Download states ordered by display priority (active first, errors last)
#[derive(Debug, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum DownloadState {
    InProgress,
    Initializing,
    Importing,
    Queued,
    Requested,
    Downloaded,
    Imported,
    ImportSkipped,
    Errored,
    TimedOut,
    ImportFailed,
    Aborted,
    Cancelled,
    Rejected,
    Unknown(String),
}

impl From<String> for DownloadState {
    fn from(s: String) -> Self {
        match s.as_str() {
            "Queued" => DownloadState::Queued,
            "Requested" => DownloadState::Requested,
            // Initial/unset state, before the request is sent to the peer
            "None" => DownloadState::Requested,
            "Initializing" => DownloadState::Initializing,
            "InProgress" => DownloadState::InProgress,
            "Completed" => DownloadState::Downloaded,
            "Succeeded" => DownloadState::Downloaded,
            "Aborted" => DownloadState::Aborted,
            "Cancelled" => DownloadState::Cancelled,
            "TimedOut" => DownloadState::TimedOut,
            "Rejected" => DownloadState::Rejected,
            "Errored" => DownloadState::Errored,
            "Importing" => DownloadState::Importing,
            "Imported" => DownloadState::Imported,
            "ImportSkipped" => DownloadState::ImportSkipped,
            "ImportFailed" => DownloadState::ImportFailed,
            _ => DownloadState::Unknown(s),
        }
    }
}

/// A single transfer entry from slskd's downloads listing.
///
/// Mirrors the JSON slskd 0.26 serializes for its `Transfer` type. slskd
/// omits null fields entirely (`WhenWritingNull`), and 0.26 stopped sending
/// `stateDescription` (marked `[JsonIgnore]` upstream) and `startOffset`
/// (property deleted) — requiring either fails every entry (#73).
///
/// Byte counts are signed upstream (`long`), so they are signed here too.
/// `bytesRemaining` is computed as `Size - BytesTransferred` and goes
/// negative whenever a peer sends more than it advertised; parsing it as
/// unsigned rejected the whole entry, which hid the transfer from the
/// monitor for the rest of its life (#78). It is not read anywhere, so it
/// is not modelled at all — serde ignores the key.
#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub id: String,
    pub username: String,
    pub direction: String,
    pub filename: String,
    pub size: i64,
    #[serde(deserialize_with = "deserialize_download_state")]
    pub state: Vec<DownloadState>,
    pub requested_at: String,
    pub enqueued_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub bytes_transferred: i64,
    #[serde(default)]
    pub average_speed: f64,
    #[serde(default)]
    pub elapsed_time: Option<String>,
    pub percent_complete: f64,
    #[serde(default)]
    pub remaining_time: Option<String>,
    #[serde(default)]
    pub exception: Option<String>,
}

/// Map slskd's TransferStates bitfield to a DownloadState.
///
/// TransferStates is a flags enum: Completed=16, Succeeded=32,
/// Cancelled=64, TimedOut=128, Errored=256, Rejected=512,
/// Aborted=1024. None=0, Requested=1, Queued=2, Initializing=4,
/// InProgress=8. Locally=2048, Remotely=4096.
///
/// Check terminal reasons first (Completed + reason flag), then active states.
fn bitfield_to_download_state(value: u64) -> DownloadState {
    const REQUESTED: u64 = 1;
    const QUEUED: u64 = 2;
    const INITIALIZING: u64 = 4;
    const IN_PROGRESS: u64 = 8;
    const COMPLETED: u64 = 16;
    const SUCCEEDED: u64 = 32;
    const CANCELLED: u64 = 64;
    const TIMED_OUT: u64 = 128;
    const ERRORED: u64 = 256;
    const REJECTED: u64 = 512;
    const ABORTED: u64 = 1024;

    if value & COMPLETED != 0 {
        if value & SUCCEEDED != 0 {
            return DownloadState::Downloaded;
        }
        if value & REJECTED != 0 {
            return DownloadState::Rejected;
        }
        if value & CANCELLED != 0 {
            return DownloadState::Cancelled;
        }
        if value & TIMED_OUT != 0 {
            return DownloadState::TimedOut;
        }
        if value & ERRORED != 0 {
            return DownloadState::Errored;
        }
        if value & ABORTED != 0 {
            return DownloadState::Aborted;
        }
        return DownloadState::Downloaded;
    }

    if value & IN_PROGRESS != 0 {
        return DownloadState::InProgress;
    }
    if value & INITIALIZING != 0 {
        return DownloadState::Initializing;
    }
    if value & QUEUED != 0 {
        return DownloadState::Queued;
    }
    if value & REQUESTED != 0 || value == 0 {
        // Requested, or None (0): the transfer exists but hasn't reached a
        // queue yet. Both precede Queued in the slskd lifecycle.
        return DownloadState::Requested;
    }

    DownloadState::Unknown(format!("bitfield:{}", value))
}

fn deserialize_download_state<'de, D>(deserializer: D) -> Result<Vec<DownloadState>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StateVisitor;

    impl<'de> Visitor<'de> for StateVisitor {
        type Value = Vec<DownloadState>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a comma-separated string or a sequence of DownloadStates")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![bitfield_to_download_state(value)])
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_u64(value as u64)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value
                .split(',')
                .map(|part| DownloadState::from(part.trim().to_string()))
                .collect())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut vec = Vec::new();
            while let Some(elem) = seq.next_element()? {
                vec.push(elem);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(StateVisitor)
}

/// The slskd downloads listing flattened to its file entries.
///
/// slskd returns `[ { "username": "...", "directories": [ { "files": [...] } ] }, ... ]`;
/// only the file entries matter. Entries that fail to parse are collected
/// into `errors` instead of being dropped: a silent skip is indistinguishable
/// from an empty transfer list, which is how the 0.26 schema drift surfaced
/// as "Download never appeared in slskd" with nothing in the logs (#73).
#[derive(Debug)]
pub struct FlattenedFiles {
    pub files: Vec<FileEntry>,
    pub errors: Vec<String>,
}

/// Cap embedded JSON in error strings: they end up in logs and in the
/// UI-facing failure reason, where a full transfer entry would be noise.
fn truncated(value: &Value) -> String {
    let s = value.to_string();
    if s.len() <= 200 {
        return s;
    }
    let mut end = 200;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn collect_user_files(user: &Value, out: &mut FlattenedFiles) {
    let Some(directories) = user.get("directories").and_then(|d| d.as_array()) else {
        out.errors
            .push(format!("user entry without directories: {}", truncated(user)));
        return;
    };
    for dir in directories {
        let Some(dir_files) = dir.get("files").and_then(|f| f.as_array()) else {
            out.errors
                .push(format!("directory entry without files: {}", truncated(dir)));
            continue;
        };
        for file in dir_files {
            match serde_json::from_value::<FileEntry>(file.clone()) {
                Ok(file_entry) => out.files.push(file_entry),
                Err(e) => out.errors.push(format!("{e}; entry: {}", truncated(file))),
            }
        }
    }
}

impl<'de> Deserialize<'de> for FlattenedFiles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize as generic JSON to traverse the grouping manually
        let v = Value::deserialize(deserializer)?;

        let mut out = FlattenedFiles {
            files: Vec::new(),
            errors: Vec::new(),
        };

        match &v {
            Value::Array(users) => {
                for user in users {
                    collect_user_files(user, &mut out);
                }
            }
            // Handle case where response is a single object instead of array
            Value::Object(_) => collect_user_files(&v, &mut out),
            other => {
                out.errors.push(format!(
                    "unexpected downloads response shape: {}",
                    truncated(other)
                ));
            }
        }

        Ok(out)
    }
}

/// Replace characters that are invalid in filenames with `_`.
/// Mirrors slskd's `ReplaceInvalidFileNameCharacters` on Windows hosts and,
/// since slskd sanitizes idempotently, produces names slskd will not alter
/// again on either OS. Used both to pre-sanitize the batch enqueue
/// `destination` and to predict where slskd placed a downloaded file.
pub fn sanitize_filename(name: &str) -> String {
    let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if invalid.contains(&c) || c == '\0' {
            result.push('_');
        } else {
            result.push(c);
        }
    }
    result
}

/// Sanitize like slskd running on a Unix host: only `/` and NUL are invalid.
/// slskd 0.26 sanitizes basenames with the host OS's invalid set, so a Linux
/// slskd keeps characters like `:` that [`sanitize_filename`] would replace;
/// path resolution has to try both candidates.
pub fn sanitize_filename_unix(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '/' || c == '\0' {
            result.push('_');
        } else {
            result.push(c);
        }
    }
    result
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchResult {
    pub guessed_artist: String,
    pub guessed_album: String,
    pub matched_track: String,
    pub artist_score: f64,
    pub album_score: f64,
    pub track_score: f64,
    pub total_score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackResult {
    #[serde(flatten)]
    pub base: SearchResult,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub match_score: f64,
}

impl TrackResult {
    pub fn new(base: SearchResult, matched: MatchResult) -> Self {
        Self {
            base,
            artist: matched.guessed_artist,
            title: matched.matched_track,
            album: matched.guessed_album,
            match_score: matched.total_score,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub username: String,
    pub filename: String,
    pub size: i64,
    pub bitrate: Option<i32>,
    pub duration: Option<i32>,
    #[serde(default)]
    pub sample_rate: Option<i32>,
    #[serde(default)]
    pub bit_depth: Option<i32>,
    pub has_free_upload_slot: bool,
    pub upload_speed: i32,
    pub queue_length: i32,
}

impl SearchResult {
    pub fn quality(&self) -> String {
        Path::new(&self.filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_lowercase()
    }

    pub fn quality_score(&self) -> f64 {
        let quality_weights: HashMap<&str, f64> = [
            ("flac", 1.0),
            ("wav", 0.85),
            ("m4a", 0.65),
            ("aac", 0.65),
            ("mp3", 0.55),
            ("ogg", 0.6),
            ("wma", 0.4),
        ]
        .iter()
        .cloned()
        .collect();

        let mut base_score = *quality_weights.get(self.quality().as_str()).unwrap_or(&0.3);

        if let Some(br) = self.bitrate {
            if br >= 320 {
                base_score += 0.2;
            } else if br >= 256 {
                base_score += 0.1;
            } else if br < 128 {
                base_score -= 0.3;
            }
        }

        if self.has_free_upload_slot {
            base_score += 0.1;
        }
        if self.upload_speed > 100 {
            base_score += 0.05;
        }
        if self.queue_length > 10 {
            base_score -= 0.1;
        }

        base_score.min(1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumResult {
    pub username: String,
    pub album_path: String,
    pub album_title: String,
    pub artist: Option<String>,
    pub track_count: usize,
    pub total_size: i64,
    pub tracks: Vec<TrackResult>,
    pub dominant_quality: String,
    pub has_free_upload_slot: bool,
    pub upload_speed: i32,
    pub queue_length: i32,
    pub score: f64,
}

impl AlbumResult {
    pub fn size_mb(&self) -> i64 {
        self.total_size / (1024 * 1024)
    }

    pub fn average_track_size_mb(&self) -> f64 {
        if self.track_count > 0 {
            self.size_mb() as f64 / self.track_count as f64
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SearchState {
    InProgress,
    Completed,
    NotFound,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub search_id: String,
    pub results: Vec<AlbumResult>,
    pub has_more: bool,
    pub total_results: usize,
    pub state: SearchState,
}

// Conversions to abstract types

impl From<SearchState> for crate::download::SearchState {
    fn from(state: SearchState) -> Self {
        match state {
            SearchState::InProgress => crate::download::SearchState::InProgress,
            SearchState::Completed => crate::download::SearchState::Completed,
            SearchState::NotFound => crate::download::SearchState::NotFound,
            SearchState::TimedOut => crate::download::SearchState::TimedOut,
        }
    }
}

impl From<TrackResult> for crate::download::DownloadableItem {
    fn from(track: TrackResult) -> Self {
        Self {
            id: track.base.filename.clone(),
            source: track.base.username.clone(),
            title: track.title,
            artist: track.artist,
            album: track.album,
            size: Some(track.base.size as u64),
            duration: track.base.duration.map(|d| d as u32),
            quality: track.base.quality(),
            quality_score: track.base.quality_score(),
            backend_data: Some(serde_json::to_string(&track.base).unwrap_or_default()),
        }
    }
}

impl From<AlbumResult> for crate::download::DownloadableGroup {
    fn from(album: AlbumResult) -> Self {
        Self {
            source: album.username.clone(),
            group_id: album.album_path.clone(),
            title: album.album_title,
            artist: album.artist,
            item_count: album.track_count,
            total_size: album.total_size as u64,
            items: album.tracks.into_iter().map(Into::into).collect(),
            quality: album.dominant_quality,
            score: album.score,
        }
    }
}

impl From<FileEntry> for crate::download::DownloadProgress {
    fn from(entry: FileEntry) -> Self {
        // Check all state elements, preferring error/rejection states over success.
        // This prevents "Completed, Rejected" from being treated as Downloaded
        // when only the first element is checked.
        let state = if let Some(priority_state) = entry.state.iter().find(|s| {
            matches!(
                s,
                DownloadState::Rejected
                    | DownloadState::Errored
                    | DownloadState::Aborted
                    | DownloadState::Cancelled
                    | DownloadState::TimedOut
                    | DownloadState::ImportFailed
            )
        }) {
            priority_state.clone()
        } else {
            entry
                .state
                .first()
                .cloned()
                .unwrap_or(DownloadState::Unknown("unknown".into()))
        };
        Self {
            id: entry.id,
            source: entry.username,
            item: entry.filename,
            // Clamped, not cast: slskd's counters are signed and the UI's are
            // not. A nonsensical negative must not wrap into a huge size.
            size: entry.size.max(0) as u64,
            transferred: entry.bytes_transferred.max(0) as u64,
            state: state.into(),
            percent: entry.percent_complete,
            speed: entry.average_speed,
            error: entry.exception,
            backend: Some("slskd".into()),
            batch_id: None,
            batch_label: None,
        }
    }
}

impl From<DownloadState> for crate::download::DownloadState {
    fn from(state: DownloadState) -> Self {
        use crate::download::DownloadState as DS;
        match state {
            DownloadState::Queued => DS::Queued,
            DownloadState::Requested => DS::Queued,
            DownloadState::Initializing => DS::InProgress,
            DownloadState::InProgress => DS::InProgress,
            DownloadState::Downloaded => DS::Completed,
            DownloadState::Importing => DS::Importing,
            DownloadState::Imported => DS::Imported,
            DownloadState::ImportSkipped => DS::ImportSkipped,
            DownloadState::Errored => DS::Failed("Download error".into()),
            DownloadState::ImportFailed => DS::Failed("Import failed".into()),
            DownloadState::Aborted => DS::Failed("Aborted".into()),
            DownloadState::Cancelled => DS::Cancelled,
            DownloadState::Rejected => DS::Failed("Transfer rejected by peer".into()),
            DownloadState::TimedOut => DS::Failed("Download timed out".into()),
            // Unrecognized states must never map to a terminal state: the
            // monitor treats terminal as finished and cancels the live slskd
            // transfer (issue #71). Treat as active; the per-track timeout
            // is the backstop if the state never advances.
            DownloadState::Unknown(_) => DS::Queued,
        }
    }
}

impl From<DownloadResponse> for crate::download::QueuedDownload {
    fn from(resp: DownloadResponse) -> Self {
        Self {
            id: resp.filename.clone(),
            source: resp.username,
            item: resp.filename,
            size: resp.size,
            error: resp.error,
        }
    }
}

impl crate::download::DownloadableItem {
    /// Convert back to slskd TrackResult for download
    pub fn to_slskd_track(&self) -> Option<TrackResult> {
        let base: SearchResult = self
            .backend_data
            .as_ref()
            .and_then(|data| serde_json::from_str(data).ok())?;
        Some(TrackResult {
            base,
            artist: self.artist.clone(),
            title: self.title.clone(),
            album: self.album.clone(),
            match_score: self.quality_score,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::DownloadState as DS;
    use serde_json::json;

    /// Deserialize a transfer entry exactly as slskd's API would return it,
    /// with the given `state` value (string or numeric bitfield), and return
    /// the DownloadState the monitor would see.
    fn mapped(state: serde_json::Value) -> DS {
        let entry: FileEntry = serde_json::from_value(json!({
            "id": "890f943c-02e1-4d45-af76-d55e3d855684",
            "username": "peer",
            "direction": "Download",
            "filename": "shared\\Artist\\Album\\01. Track.flac",
            "size": 1024,
            "state": state,
            "requestedAt": "2026-07-19T05:11:22Z",
            "bytesTransferred": 0,
            "bytesRemaining": 1024,
            "percentComplete": 0.0
        }))
        .expect("FileEntry should deserialize");
        crate::download::DownloadProgress::from(entry).state
    }

    /// A verbatim `GET /api/v0/transfers/downloads` payload as slskd 0.26.0
    /// serializes it: no `stateDescription` (marked [JsonIgnore] upstream),
    /// no `startOffset` (property deleted), `removed` newly present, null
    /// fields omitted entirely (WhenWritingNull). Requiring a field slskd
    /// no longer sends makes every entry fail to parse, the transfer list
    /// comes back empty, and completed downloads are never noticed (#73).
    #[test]
    fn parses_slskd_0_26_downloads_payload() {
        let payload = json!([{
            "username": "peer",
            "directories": [{
                "directory": "shared\\Artist\\Album",
                "fileCount": 1,
                "files": [{
                    "id": "890f943c-02e1-4d45-af76-d55e3d855684",
                    "username": "peer",
                    "direction": "Download",
                    "filename": "shared\\Artist\\Album\\01. Track.flac",
                    "size": 1024,
                    "state": "Completed, Succeeded",
                    "requestedAt": "2026-07-19T05:11:22Z",
                    "enqueuedAt": "2026-07-19T05:11:23Z",
                    "startedAt": "2026-07-19T05:11:24Z",
                    "endedAt": "2026-07-19T05:11:30Z",
                    "bytesTransferred": 1024,
                    "averageSpeed": 250000.0,
                    "attempts": 1,
                    "removed": false,
                    "bytesRemaining": 0,
                    "elapsedTime": "00:00:06",
                    "percentComplete": 100.0,
                    "remainingTime": "00:00:00"
                }]
            }]
        }]);

        let files: FlattenedFiles =
            serde_json::from_value(payload).expect("payload should deserialize");
        assert_eq!(files.files.len(), 1, "transfer entry was silently dropped");
        assert_eq!(
            crate::download::DownloadProgress::from(files.files[0].clone()).state,
            DS::Completed
        );
    }

    /// slskd computes `bytesRemaining` as `Size - BytesTransferred` and
    /// types it `long`, so a peer that sends more than it advertised makes
    /// it negative. Parsing that as `u64` failed the entry, and since the
    /// monitor only sees transfers it can parse, the download stayed
    /// invisible until the batch was written off (#78).
    #[test]
    fn parses_transfer_that_overshot_its_advertised_size() {
        let payload = json!([{
            "username": "peer",
            "directories": [{
                "directory": "d",
                "fileCount": 1,
                "files": [{
                    "id": "890f943c-02e1-4d45-af76-d55e3d855684",
                    "username": "peer",
                    "direction": "Download",
                    "filename": "d\\a.flac",
                    "size": 1024,
                    "state": "InProgress",
                    "requestedAt": "2026-07-19T05:11:22Z",
                    "bytesTransferred": 5120,
                    "bytesRemaining": -4096,
                    "percentComplete": 500.0
                }]
            }]
        }]);

        let files: FlattenedFiles =
            serde_json::from_value(payload).expect("payload should deserialize");
        assert_eq!(files.errors.len(), 0, "overshooting transfer was rejected");
        assert_eq!(files.files.len(), 1, "overshooting transfer was dropped");
        let progress = crate::download::DownloadProgress::from(files.files[0].clone());
        assert_eq!(progress.transferred, 5120);
        assert_eq!(progress.size, 1024);
    }

    /// Entries that fail to parse must be reported, not silently skipped:
    /// a silent skip is indistinguishable from "no downloads in progress",
    /// which is exactly how the 0.26 schema drift went unnoticed.
    #[test]
    fn flattener_surfaces_parse_errors() {
        let payload = json!([{
            "username": "peer",
            "directories": [{
                "directory": "d",
                "fileCount": 2,
                "files": [
                    {
                        "id": "890f943c-02e1-4d45-af76-d55e3d855684",
                        "username": "peer",
                        "direction": "Download",
                        "filename": "d\\a.flac",
                        "size": 1024,
                        "state": "InProgress",
                        "requestedAt": "2026-07-19T05:11:22Z",
                        "bytesTransferred": 0,
                        "bytesRemaining": 1024,
                        "percentComplete": 0.0
                    },
                    { "garbage": true }
                ]
            }]
        }]);

        let files: FlattenedFiles =
            serde_json::from_value(payload).expect("payload should deserialize");
        assert_eq!(files.files.len(), 1);
        assert_eq!(files.errors.len(), 1, "parse failure was swallowed");
    }

    #[test]
    fn active_transfer_states_stay_active() {
        // Every state a live slskd transfer passes through before completion.
        // Mapping any of these to a terminal state makes the monitor declare
        // the batch finished and cancel the transfer (issue #71).
        for state in [
            "None",
            "Requested",
            "Queued, Locally",
            "Queued, Remotely",
            "Initializing",
            "InProgress",
        ] {
            let got = mapped(json!(state));
            assert!(
                matches!(got, DS::Queued | DS::InProgress),
                "string state {state:?} mapped to {got:?}"
            );
        }
        // Same states as numeric bitfields: None, Requested,
        // Queued|Locally, Queued|Remotely, Initializing, InProgress.
        for value in [0u64, 1, 2050, 4098, 4, 8] {
            let got = mapped(json!(value));
            assert!(
                matches!(got, DS::Queued | DS::InProgress),
                "bitfield state {value} mapped to {got:?}"
            );
        }
    }

    #[test]
    fn completed_states_map_to_their_outcome() {
        assert_eq!(mapped(json!("Completed, Succeeded")), DS::Completed);
        assert_eq!(mapped(json!("Completed, Cancelled")), DS::Cancelled);
        for state in [
            "Completed, Errored",
            "Completed, Rejected",
            "Completed, Aborted",
            "Completed, TimedOut",
        ] {
            let got = mapped(json!(state));
            assert!(
                matches!(got, DS::Failed(_)),
                "string state {state:?} mapped to {got:?}"
            );
        }
        // Completed|Succeeded, Completed|Cancelled as bitfields
        assert_eq!(mapped(json!(48u64)), DS::Completed);
        assert_eq!(mapped(json!(80u64)), DS::Cancelled);
        // Completed|Errored, Completed|Rejected, Completed|Aborted, Completed|TimedOut
        for value in [272u64, 528, 1040, 144] {
            let got = mapped(json!(value));
            assert!(
                matches!(got, DS::Failed(_)),
                "bitfield state {value} mapped to {got:?}"
            );
        }
    }

    #[test]
    fn unrecognized_states_never_map_terminal() {
        for state in [json!("SomethingNew"), json!("Weird, Flags"), json!(8192u64)] {
            let got = mapped(state.clone());
            assert!(
                matches!(got, DS::Queued | DS::InProgress),
                "unrecognized state {state} mapped to {got:?}"
            );
        }
    }
}
