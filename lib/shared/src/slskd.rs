use std::path::Path;

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};

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
    /// slskd's transfer id, present when the file was enqueued.
    pub transfer_id: Option<String>,
    pub status: crate::download::EnqueueStatus,
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
    /// slskd's `Remotely` qualifier: with `Queued`, the peer holds the
    /// transfer in its own upload queue. Never a row's state on its own.
    Remotely,
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
            "Remotely" => DownloadState::Remotely,
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
    /// The batch the transfer was enqueued in. Absent for transfers enqueued
    /// without one.
    #[serde(default)]
    pub batch_id: Option<String>,
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
    /// How many times slskd started the download: set to 1 before the first
    /// attempt and incremented on each retry (`Transfers/Types/Transfer.cs:155`).
    #[serde(default)]
    pub attempts: u32,
    /// Set by slskd only once its position endpoint was asked (`DownloadService.cs:711-729`,
    /// `Transfers/Types/Transfer.cs:142`); omitted while null (`docs/api/slskd.md:239`). It can
    /// return to null, as slskd writes its in-memory transfer back on each update
    /// (`DownloadService.cs:1087-1092,1611-1617`).
    #[serde(default)]
    pub place_in_queue: Option<i64>,
    /// slskd removed the record from its UI (`Transfers/Types/Transfer.cs:165`, sent since 0.26.0,
    /// docs/api/slskd.md:241). The single-transfer GET still returns it (`DownloadService.cs:688-704`
    /// has no `Removed` filter).
    #[serde(default)]
    pub removed: bool,
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
        out.errors.push(format!(
            "user entry without directories: {}",
            truncated(user)
        ));
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
    /// Scored below the search's floor, kept because the search asked to.
    #[serde(default)]
    pub below_floor: bool,
}

impl TrackResult {
    pub fn new(base: SearchResult, matched: MatchResult) -> Self {
        Self {
            base,
            artist: matched.guessed_artist,
            title: matched.matched_track,
            album: matched.guessed_album,
            match_score: matched.total_score,
            below_floor: false,
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
    /// A `long` upstream (`Search/Types/Response.cs:45`).
    pub queue_length: i64,
    #[serde(default)]
    pub variable_bit_rate: Option<bool>,
    /// The extension the peer's client sent, as `declared_extension` keeps it.
    #[serde(default)]
    pub declared_extension: Option<String>,
    /// Audio files in this file's remote folder in the peer's answer.
    #[serde(default)]
    pub folder_audio_files: u32,
}

/// An extension as Soulful keeps one: 1 to 8 ASCII letters or digits, lowercased.
fn extension_token(text: &str) -> Option<String> {
    let fits = (1..=8).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric());
    fits.then(|| text.to_ascii_lowercase())
}

/// The extension of a remote file name, whose folders a peer separates with
/// `\` or `/`: the text after the last `.` of its last part.
pub fn remote_extension(filename: &str) -> Option<String> {
    let name = filename.rsplit(['\\', '/']).next().unwrap_or(filename);
    extension_token(name.rsplit_once('.')?.1)
}

/// The extension a peer's client sent for a file (Soulseek.NET `File.cs`
/// takes whatever it wrote), without a leading `.`; None when it is empty or
/// not an extension.
pub fn declared_extension(field: &str) -> Option<String> {
    extension_token(field.strip_prefix('.').unwrap_or(field))
}

impl SearchResult {
    pub fn quality(&self) -> String {
        Path::new(&self.filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_lowercase()
    }

    /// `format_score` of the file plus the peer's terms: a free slot, its speed, its queue.
    /// A negative bitrate a peer reports scores as 0, under 128.
    pub fn quality_score(&self) -> f64 {
        let bitrate = self.bitrate.map(|br| u32::try_from(br).unwrap_or(0));
        let mut score = crate::download::format_score(&self.quality(), bitrate);
        if self.has_free_upload_slot {
            score += 0.1;
        }
        if self.upload_speed > 100 {
            score += 0.05;
        }
        if self.queue_length > 10 {
            score -= 0.1;
        }
        score.min(1.0)
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
    pub queue_length: i64,
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
            SearchState::TimedOut => crate::download::SearchState::TimedOut,
        }
    }
}

/// The id slskd goes by as a download backend, in the registry and on every row and search.
pub const BACKEND_ID: &str = "slskd";

impl From<TrackResult> for crate::download::DownloadableItem {
    fn from(track: TrackResult) -> Self {
        Self {
            id: track.base.filename.clone(),
            backend: BACKEND_ID.into(),
            source: track.base.username.clone(),
            title: track.title,
            artist: track.artist,
            album: track.album,
            size: Some(track.base.size as u64),
            duration: track.base.duration.map(|d| d as u32),
            quality: track.base.quality(),
            quality_score: track.base.quality_score(),
            backend_data: Some(serde_json::to_string(&track.base).unwrap_or_default()),
            match_score: track.match_score,
            below_floor: track.below_floor,
            facts: file_facts(&track.base),
        }
    }
}

/// The facts of one search file and its peer, a negative count read as unreported.
fn file_facts(base: &SearchResult) -> crate::download::FileFacts {
    let mut backend_facts = serde_json::Map::new();
    if let Some(declared) = &base.declared_extension {
        backend_facts.insert("declared_extension".into(), declared.clone().into());
    }
    crate::download::FileFacts {
        extension: remote_extension(&base.filename),
        bitrate_kbps: base.bitrate.and_then(|b| u32::try_from(b).ok()),
        variable_bitrate: base.variable_bit_rate,
        sample_rate_hz: base.sample_rate.and_then(|r| u32::try_from(r).ok()),
        bit_depth: base.bit_depth.and_then(|d| u32::try_from(d).ok()),
        folder_audio_files: Some(base.folder_audio_files),
        source_has_free_slot: Some(base.has_free_upload_slot),
        source_upload_speed: u64::try_from(base.upload_speed).ok(),
        source_queue_length: u64::try_from(base.queue_length).ok(),
        backend_facts,
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

/// Where the last 8 hex digits of a GUID start, in its hyphenated form.
const BATCH_NUMBER_AT: usize = 28;

/// The id of the `n`th slskd batch of the request `request`. One `download` call (a request) sends
/// its files in several slskd batches, each needing a GUID of its own (`TransfersController.cs:
/// 336-350`, 409 for one already used), and slskd lists every transfer with the batch it came in
/// (`Transfers/Types/Transfer.cs:49`, set at `Transfers/Downloads/DownloadService.cs:481` and kept
/// through slskd's retries, which reuse the record). A request is a GUID whose last 8 hex digits are
/// zero, its batch `n` the same GUID with `n` there, so `request_of_batch` reads the request back
/// from any of its batches. `request` is the request's batch 0 (`request_batch_id(guid, 0)`).
pub fn request_batch_id(request: &str, n: u32) -> String {
    let prefix = request.get(..BATCH_NUMBER_AT).unwrap_or(request);
    format!("{prefix}{n:08x}")
}

/// The request whose batch `batch_id` is (`request_batch_id`); None for a text that is no GUID.
/// A batch another client made reads as a request no `download` call had.
fn request_of_batch(batch_id: &str) -> Option<String> {
    (batch_id.len() == 36 && batch_id.is_ascii())
        .then(|| request_batch_id(&batch_id.to_ascii_lowercase(), 0))
}

impl FileEntry {
    /// The row for this transfer under slskd's `retry_limit` (its
    /// `transfers.download.retry.attempts`).
    pub fn into_progress(self, retry_limit: u32) -> crate::download::DownloadProgress {
        // Check all state elements, preferring error/rejection states over success.
        // This prevents "Completed, Rejected" from being treated as Downloaded
        // when only the first element is checked.
        let state = if let Some(priority_state) = self.state.iter().find(|s| {
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
            self.state
                .first()
                .cloned()
                .unwrap_or(DownloadState::Unknown("unknown".into()))
        };
        let queued_at_source = self.state.contains(&DownloadState::Queued)
            && self.state.contains(&DownloadState::Remotely);
        let pending = awaits_retry(
            &state,
            self.exception.as_deref(),
            Attempt {
                number: self.attempts,
                limit: retry_limit,
                ended: self.ended_at.is_some(),
            },
        );
        let (row_state, failure) = classify_transfer(&state, self.exception.as_deref());
        let row = crate::download::DownloadProgress {
            id: self.id,
            source: self.username,
            item: self.filename,
            // Clamped, not cast: slskd's counters are signed and the UI's are
            // not. A nonsensical negative must not wrap into a huge size.
            size: self.size.max(0) as u64,
            transferred: self.bytes_transferred.max(0) as u64,
            state: row_state,
            percent: self.percent_complete,
            speed: self.average_speed,
            error: None,
            backend: Some(BACKEND_ID.into()),
            batch_id: None,
            batch_label: None,
            local_path: None,
            import_output: None,
            retryable: false,
            failure: None,
            skip: None,
            queued_at_source,
            attempts: self.attempts,
            queue_position: self
                .place_in_queue
                .and_then(|place| u32::try_from(place).ok()),
            pending_retry: None,
            job: None,
            job_id: None,
            request: self.batch_id.as_deref().and_then(request_of_batch),
            index: None,
        };
        match failure {
            Some(failure) if pending => crate::download::DownloadProgress {
                state: crate::download::DownloadState::Queued,
                pending_retry: Some(failure),
                ..row
            },
            Some(failure) => row.with_failure(failure),
            None => row,
        }
    }
}

/// What Soulseek.NET says, in lowercase, when slskd is not logged in to the
/// Soulseek server: `The server connection must be connected and logged in
/// to {download files|fetch user endpoint|...} (currently: {state})`. slskd
/// has no typed signal for it, so the text is all there is. On a download
/// slskd stores it through `TryFail`, which cuts at the first colon, from
/// the `AggregateException` its retries end with: `One or more errors
/// occurred. ({text}) ({text}) ...`. With the default 3 attempts the cut
/// leaves `Disconnected)) ({text}) ({text})`, which still names it. With
/// `Retry.Attempts = 1` only `Disconnected))` is left, which says nothing.
const NOT_LOGGED_IN: &str = "must be connected and logged in";

/// Whether `text` says slskd is not logged in to the Soulseek server, in any case. Most calls
/// word it as NOT_LOGGED_IN says; the queue position ask words it `The server connection must be
/// Connected and LoggedIn to check download queue position (currently: {state})` (Soulseek.NET
/// `SoulseekClient.cs:1640`).
pub fn says_not_logged_in(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains(NOT_LOGGED_IN) || text.contains("must be connected and loggedin")
}

/// The exception, in lowercase, slskd's startup cleanup writes on every
/// download it left unfinished, with state `Completed, Errored` and the
/// attempt count unchanged (`Application.cs:100,367`). Nothing resumes it.
const APPLICATION_SHUT_DOWN: &str = "application shut down";

/// A transfer's attempt count and slskd's limit on it, and whether slskd
/// recorded an end for the last attempt.
struct Attempt {
    number: u32,
    limit: u32,
    ended: bool,
}

impl Attempt {
    /// Whether slskd will start another attempt. `number` 0 means slskd
    /// failed the record before its first attempt (`DownloadService.cs:1350`
    /// sets 1 just before it). Without an end time there is no sign of a
    /// retry, since slskd sets one on every finished attempt. How long a
    /// record may wait for the retry is the watcher's to bound.
    fn will_retry(&self) -> bool {
        (1..self.limit).contains(&self.number) && self.ended
    }
}

/// Whether a transfer in `state` is a failed attempt slskd will retry.
///
/// An errored or timed out attempt slskd will retry is queued: slskd copies
/// the attempt's terminal state over its `Queued, Locally` before the retry
/// starts (`Transfers/Downloads/DownloadService.cs:1230`,
/// `WithSoulseekTransfer`, which takes `State` from Soulseek.NET's transfer
/// at `Transfers/Extensions.cs:47`), and retries while the attempt count is
/// below its limit (`Attempt::will_retry`).
fn awaits_retry(state: &DownloadState, exception: Option<&str>, attempt: Attempt) -> bool {
    matches!(state, DownloadState::Errored | DownloadState::TimedOut)
        && attempt.will_retry()
        && !exception.is_some_and(|e| e.eq_ignore_ascii_case(APPLICATION_SHUT_DOWN))
}

/// The row state for a transfer and, for a failed or cancelled one, its
/// failure. The message is the exception slskd stored, else the state's own
/// text. Only failure states read the exception: a record waiting to retry
/// still carries the failed attempt's text.
fn classify_transfer(
    state: &DownloadState,
    exception: Option<&str>,
) -> (crate::download::DownloadState, Option<Failure>) {
    use crate::download::DownloadState as DS;
    let text = exception.map(str::to_ascii_lowercase).unwrap_or_default();
    let (stage, code, retry, default) = match state {
        DownloadState::Queued | DownloadState::Requested => return (DS::Queued, None),
        DownloadState::Initializing | DownloadState::InProgress => return (DS::InProgress, None),
        DownloadState::Downloaded => return (DS::Completed, None),
        DownloadState::Importing => return (DS::Importing, None),
        DownloadState::Imported => return (DS::Imported, None),
        DownloadState::ImportSkipped => return (DS::ImportSkipped, None),
        // Unrecognized states must never map to a terminal state: the
        // monitor treats terminal as finished and cancels the live slskd
        // transfer (issue #71). Treat as active; the per-track timeout
        // is the backstop if the state never advances.
        DownloadState::Remotely | DownloadState::Unknown(_) => return (DS::Queued, None),
        DownloadState::Rejected => {
            let (code, retry) = rejection(&text);
            (Stage::Transfer, code, retry, "Transfer rejected by peer")
        }
        DownloadState::Errored => {
            let (code, retry) = if says_not_logged_in(&text) {
                // Every peer fails the same way until slskd logs back in.
                (
                    FailureCode::BackendUnreachable,
                    RetryAdvice::SameSourceLater,
                )
            } else if text.contains("appears to be offline") {
                (FailureCode::PeerOffline, RetryAdvice::OtherSource)
            } else if text == APPLICATION_SHUT_DOWN {
                // slskd restarted mid-transfer; the peer is fine.
                (
                    FailureCode::TransferConnectionFailed,
                    RetryAdvice::SameSourceLater,
                )
            } else {
                (
                    FailureCode::TransferConnectionFailed,
                    RetryAdvice::OtherSource,
                )
            };
            (Stage::Transfer, code, retry, "Download error")
        }
        DownloadState::TimedOut => (
            Stage::Transfer,
            FailureCode::TransferTimedOut,
            RetryAdvice::OtherSource,
            "Download timed out",
        ),
        // Soulseek.NET aborts a download only when the peer's file size
        // differs from the one it expected.
        DownloadState::Aborted => {
            let code = if text.contains("does not match expected size") {
                FailureCode::TransferSizeMismatch
            } else {
                FailureCode::TransferAborted
            };
            (Stage::Transfer, code, RetryAdvice::OtherSource, "Aborted")
        }
        DownloadState::Cancelled => (
            Stage::Transfer,
            FailureCode::TransferCancelled,
            RetryAdvice::NotRetryable,
            "Cancelled",
        ),
        DownloadState::ImportFailed => (
            Stage::Import,
            FailureCode::ImportBeetsFailed,
            RetryAdvice::SameSourceLater,
            "Import failed",
        ),
    };
    let row_state = match state {
        DownloadState::Cancelled => DS::Cancelled,
        _ => DS::Failed(default.into()),
    };
    let failure = Failure::new(code, stage, retry, exception.unwrap_or(default));
    let failure = match exception {
        Some(raw) => failure.with_cause(Service::Slskd, None, raw),
        None => failure,
    };
    (row_state, Some(failure))
}

/// Code and retry advice for a peer's refusal, from its lowercased text.
///
/// Soulseek.NET stores the peer's reason as `Transfer rejected: {reason}`
/// when the peer refuses the transfer request, and bare when it sends
/// `UploadDenied` later. Three reasons mean "retry later": the exact
/// `Too many files` and `Too many megabytes` that clients send each other
/// for a full queue (old Nicotine+ sends `User limit of {n} files|megabytes
/// exceeded` instead), and slskd's `Overwhelmed with requests; try again
/// later.`. The `today` and `this week` forms are quotas that last that long.
fn rejection(text: &str) -> (FailureCode, RetryAdvice) {
    let reason = text.strip_prefix("transfer rejected: ").unwrap_or(text);
    let reason = reason.strip_suffix('.').unwrap_or(reason);
    let old_queue_limit = reason
        .strip_prefix("user limit of ")
        .is_some_and(|r| r.ends_with(" files exceeded") || r.ends_with(" megabytes exceeded"));
    match reason {
        "too many files" | "too many megabytes" | "overwhelmed with requests; try again later" => (
            FailureCode::TransferRejectedPeerBusy,
            RetryAdvice::SameSourceLater,
        ),
        _ if old_queue_limit => (
            FailureCode::TransferRejectedPeerBusy,
            RetryAdvice::SameSourceLater,
        ),
        "too many files today"
        | "too many megabytes today"
        | "too many files this week"
        | "too many megabytes this week" => (
            FailureCode::TransferRejectedPeerBusy,
            RetryAdvice::OtherSource,
        ),
        "too many failed transfers today" | "too many failed transfers this week" => (
            FailureCode::TransferRejectedFailureLimit,
            RetryAdvice::OtherSource,
        ),
        r if r.contains("file not shared") => (
            FailureCode::TransferRejectedNotShared,
            RetryAdvice::OtherSource,
        ),
        _ => (FailureCode::TransferRejectedOther, RetryAdvice::OtherSource),
    }
}

impl From<DownloadResponse> for crate::download::QueuedDownload {
    fn from(resp: DownloadResponse) -> Self {
        Self {
            id: resp.transfer_id.unwrap_or_else(|| resp.filename.clone()),
            source: resp.username,
            item: resp.filename,
            size: resp.size,
            status: resp.status,
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
            match_score: self.match_score,
            below_floor: self.below_floor,
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
        converted(state, None, 3).state
    }

    /// The row the monitor would see for a transfer entry with the given
    /// `state`, its `attempts` and, when given, the `exception` slskd stored
    /// on it, under slskd's default limit of 3 attempts.
    fn converted(
        state: serde_json::Value,
        exception: Option<&str>,
        attempts: u32,
    ) -> crate::download::DownloadProgress {
        let mut entry = json!({
            "id": "890f943c-02e1-4d45-af76-d55e3d855684",
            "username": "peer",
            "direction": "Download",
            "filename": "shared\\Artist\\Album\\01. Track.flac",
            "size": 1024,
            "state": state,
            "requestedAt": "2026-07-19T05:11:22Z",
            "endedAt": "2026-07-19T05:11:30.1234567Z",
            "bytesTransferred": 0,
            "bytesRemaining": 1024,
            "percentComplete": 0.0,
            "attempts": attempts
        });
        if let Some(exception) = exception {
            entry["exception"] = json!(exception);
        }
        let entry: FileEntry = serde_json::from_value(entry).expect("FileEntry should deserialize");
        entry.into_progress(3)
    }

    #[test]
    fn a_listed_transfer_names_slskd_as_its_backend() {
        let row = converted(json!("Queued, Remotely"), None, 0);
        assert_eq!(row.backend.as_deref(), Some(BACKEND_ID));
    }

    #[test]
    fn a_row_names_the_request_whose_batch_its_transfer_came_in() {
        let request = request_batch_id("890f943c-02e1-4d45-af76-d55e3d855684", 0);
        assert_eq!(request, "890f943c-02e1-4d45-af76-d55e00000000");
        let row = |batch: Option<String>| {
            let mut entry: FileEntry = serde_json::from_value(json!({
                "id": "0d6a2c1e-7a5b-4f4e-9a51-3c0f6d1b2e7a",
                "username": "peer",
                "direction": "Download",
                "filename": "shared\\Artist\\Album\\01. Track.flac",
                "size": 1024,
                "state": "Queued, Remotely",
                "requestedAt": "2026-07-19T05:11:22",
                "bytesTransferred": 0,
                "percentComplete": 0.0
            }))
            .unwrap();
            entry.batch_id = batch;
            entry.into_progress(3).request
        };
        for n in [0, 1, 0xffff_ffff] {
            let batch = request_batch_id(&request, n);
            assert_eq!(batch.len(), 36);
            assert_eq!(row(Some(batch.to_ascii_uppercase())), Some(request.clone()));
        }
        assert_eq!(row(None), None);
        assert_eq!(row(Some("not a guid".into())), None);
    }

    #[test]
    fn a_transfer_the_peer_has_queued_is_queued_at_source() {
        for (state, attempts, expected) in [
            (json!("Queued, Remotely"), 1, true),
            (json!("Queued, Locally"), 1, false),
            (json!("Requested"), 0, false),
            (json!("InProgress"), 1, false),
            // A failed attempt slskd will retry reads as Queued (step 3 D5), but no peer holds it.
            (json!("Completed, Errored"), 1, false),
        ] {
            let row = converted(state.clone(), None, attempts);
            assert_eq!(row.queued_at_source, expected, "{state}");
            assert!(matches!(row.state, DS::Queued | DS::InProgress), "{state}");
            assert_eq!(row.failure, None, "{state}");
        }
    }

    #[test]
    fn a_failed_attempt_slskd_will_retry_reads_as_queued() {
        use FailureCode::*;
        for (state, attempts, queued, pending) in [
            (
                json!("Completed, Errored"),
                1,
                true,
                Some(TransferConnectionFailed),
            ),
            (
                json!("Completed, TimedOut"),
                2,
                true,
                Some(TransferTimedOut),
            ),
            (json!("Completed, Errored"), 3, false, None),
            (json!("Completed, Errored"), 0, false, None),
            (json!("Completed, Rejected"), 1, false, None),
            (json!("Completed, Aborted"), 1, false, None),
            (json!("Completed, Cancelled"), 1, false, None),
        ] {
            let row = converted(state.clone(), Some("Connection reset by peer"), attempts);
            assert_eq!(
                row.state == DS::Queued,
                queued,
                "{state} attempts {attempts}"
            );
            assert_eq!(
                row.pending_retry.map(|f| f.code),
                pending,
                "{state} attempts {attempts}"
            );
            if queued {
                assert_eq!((row.failure, row.error), (None, None));
            }
        }
        let last = converted(
            json!("Completed, Errored"),
            Some("Connection reset by peer"),
            3,
        );
        assert_eq!(
            last.failure.unwrap().code,
            FailureCode::TransferConnectionFailed
        );

        // slskd's startup cleanup: never resumed, whatever the attempt count.
        let restarted = converted(
            json!("Completed, Errored"),
            Some("Application shut down"),
            1,
        );
        assert!(
            matches!(restarted.state, DS::Failed(_)),
            "{:?}",
            restarted.state
        );
        assert_eq!(restarted.pending_retry, None);
        let f = restarted.failure.unwrap();
        assert_eq!(
            (f.code, f.retry),
            (
                FailureCode::TransferConnectionFailed,
                RetryAdvice::SameSourceLater
            )
        );
    }

    /// No end time is no sign of a retry: slskd sets one on every finished
    /// attempt.
    #[test]
    fn a_failed_attempt_with_no_end_time_is_not_a_pending_retry() {
        let entry: FileEntry = serde_json::from_value(json!({
            "id": "890f943c-02e1-4d45-af76-d55e3d855684", "username": "peer",
            "direction": "Download", "filename": "a.flac", "size": 1024,
            "state": "Completed, Errored", "requestedAt": "2026-07-19T05:11:22Z",
            "bytesTransferred": 0, "percentComplete": 0.0, "attempts": 1,
            "exception": "Connection reset by peer"
        }))
        .unwrap();
        assert!(matches!(entry.into_progress(3).state, DS::Failed(_)));
    }

    /// A peer may report a negative speed or queue: read as unreported, not as a huge number.
    #[test]
    fn a_negative_count_a_peer_reports_is_unreported() {
        let r: SearchResult = serde_json::from_value(json!({"username": "p", "filename": "a.flac",
            "size": 1, "bitrate": -1, "duration": null, "hasFreeUploadSlot": false,
            "uploadSpeed": -1, "queueLength": -1}))
        .unwrap();
        let facts = file_facts(&r);
        assert_eq!(
            (
                facts.source_upload_speed,
                facts.source_queue_length,
                facts.bitrate_kbps
            ),
            (None, None, None)
        );
    }

    /// The score as it was computed before `format_score` held the format part.
    fn score_before_format_score(r: &SearchResult) -> f64 {
        let weights: std::collections::HashMap<&str, f64> = [
            ("flac", 1.0),
            ("wav", 0.85),
            ("m4a", 0.65),
            ("aac", 0.65),
            ("mp3", 0.55),
            ("ogg", 0.6),
            ("wma", 0.4),
        ]
        .into_iter()
        .collect();
        let mut base = *weights.get(r.quality().as_str()).unwrap_or(&0.3);
        if let Some(br) = r.bitrate {
            if br >= 320 {
                base += 0.2;
            } else if br >= 256 {
                base += 0.1;
            } else if br < 128 {
                base -= 0.3;
            }
        }
        if r.has_free_upload_slot {
            base += 0.1;
        }
        if r.upload_speed > 100 {
            base += 0.05;
        }
        if r.queue_length > 10 {
            base -= 0.1;
        }
        base.min(1.0)
    }

    #[test]
    fn a_files_score_is_its_format_score_plus_the_peers_terms_as_before() {
        let names = [
            "a.flac", "a.FLAC", "a.wav", "a.m4a", "a.aac", "a.ogg", "a.mp3", "a.wma", "a.opus", "a",
        ];
        for name in names {
            for bitrate in [
                None,
                Some(-1),
                Some(0),
                Some(96),
                Some(128),
                Some(256),
                Some(320),
            ] {
                for (slot, speed, queue) in [(false, 0, 0), (true, 101, 0), (true, 500, 11)] {
                    let r = SearchResult {
                        username: "p".into(),
                        filename: format!("Music\\{name}"),
                        size: 1,
                        bitrate,
                        duration: None,
                        sample_rate: None,
                        bit_depth: None,
                        has_free_upload_slot: slot,
                        upload_speed: speed,
                        queue_length: queue,
                        variable_bit_rate: None,
                        declared_extension: None,
                        folder_audio_files: 1,
                    };
                    assert_eq!(
                        r.quality_score(),
                        score_before_format_score(&r),
                        "{name} {bitrate:?} {slot} {speed} {queue}"
                    );
                }
            }
        }
    }

    #[test]
    fn backend_data_written_with_a_32_bit_queue_length_still_reads() {
        let r: SearchResult = serde_json::from_value(json!({"username": "p", "filename": "a.flac",
            "size": 1, "bitrate": null, "duration": null, "hasFreeUploadSlot": false,
            "uploadSpeed": 0, "queueLength": 12}))
        .unwrap();
        assert_eq!(r.queue_length, 12);
    }

    /// An item stored before items kept their match score and file facts
    /// still converts back for a download, with no score and no flag.
    #[test]
    fn an_item_stored_before_file_facts_still_converts() {
        let base = r#"{"username":"p","filename":"a.flac","size":1,"bitrate":null,"duration":null,"hasFreeUploadSlot":false,"uploadSpeed":0,"queueLength":0}"#;
        let item: crate::download::DownloadableItem = serde_json::from_value(json!({
            "id": "a.flac", "backend": "slskd", "source": "p", "title": "A", "artist": "B", "album": "C",
            "size": 1, "duration": null, "quality": "flac", "quality_score": 0.9,
            "backend_data": base}))
        .unwrap();
        assert_eq!(
            (item.match_score, item.below_floor, &item.facts),
            (0.0, false, &crate::download::FileFacts::default())
        );
        let track = item.to_slskd_track().expect("converts");
        assert_eq!((track.match_score, track.below_floor), (0.0, false));
        assert_eq!(
            (track.base.declared_extension, track.base.folder_audio_files),
            (None, 0)
        );
    }

    #[test]
    fn a_converted_search_file_names_slskd_as_its_backend() {
        let track: TrackResult = serde_json::from_value(json!({
            "username": "p", "filename": "a.flac", "size": 1, "bitrate": null,
            "duration": null, "hasFreeUploadSlot": false, "uploadSpeed": 0,
            "queueLength": 0, "artist": "B", "title": "A", "album": "C", "match_score": 0.9}))
        .unwrap();
        let item: crate::download::DownloadableItem = track.into();
        assert_eq!(item.backend, BACKEND_ID);
    }

    #[test]
    fn an_extension_is_read_from_the_last_part_of_a_remote_name() {
        for (name, wanted) in [
            (
                "@@kqzbm\\Music\\Winter Songs\\03 - O Holy Night.FLAC",
                Some("flac"),
            ),
            ("Music/Winter Songs/03 - O Holy Night.mp3", Some("mp3")),
            ("x\\Album.2020\\track", None),
            ("x\\Album\\track.", None),
            ("x\\Album\\track.not an extension", None),
            ("x\\Album\\track.abcdefghi", None),
        ] {
            assert_eq!(remote_extension(name).as_deref(), wanted, "{name}");
        }
        for (field, wanted) in [
            ("flac", Some("flac")),
            (".MP3", Some("mp3")),
            ("", None),
            (".", None),
            ("fl ac", None),
        ] {
            assert_eq!(declared_extension(field).as_deref(), wanted, "{field:?}");
        }
    }

    /// Reasons a peer gives for refusing an upload, as slskd and other
    /// clients send them (`slskd-failure-texts.md` sections 2.2 and 2.3).
    /// Soulseek.NET stores each either as `Transfer rejected: {reason}` or
    /// bare, so every reason is checked in both forms.
    #[test]
    fn rejected_transfers_get_the_code_their_exception_names() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (reason, code, retry) in [
            ("File not shared.", TransferRejectedNotShared, OtherSource),
            ("File not shared", TransferRejectedNotShared, OtherSource),
            ("Too many files", TransferRejectedPeerBusy, SameSourceLater),
            (
                "Too many megabytes",
                TransferRejectedPeerBusy,
                SameSourceLater,
            ),
            (
                "Overwhelmed with requests; try again later.",
                TransferRejectedPeerBusy,
                SameSourceLater,
            ),
            (
                "Too many files today",
                TransferRejectedPeerBusy,
                OtherSource,
            ),
            (
                "Too many megabytes today",
                TransferRejectedPeerBusy,
                OtherSource,
            ),
            (
                "Too many files this week",
                TransferRejectedPeerBusy,
                OtherSource,
            ),
            (
                "Too many megabytes this week",
                TransferRejectedPeerBusy,
                OtherSource,
            ),
            (
                "Too many failed transfers today",
                TransferRejectedFailureLimit,
                OtherSource,
            ),
            (
                "Too many failed transfers this week",
                TransferRejectedFailureLimit,
                OtherSource,
            ),
            // Old Nicotine+ forms of `Too many files` and `Too many megabytes`.
            (
                "User limit of 50 files exceeded",
                TransferRejectedPeerBusy,
                SameSourceLater,
            ),
            (
                "User limit of 1000 megabytes exceeded",
                TransferRejectedPeerBusy,
                SameSourceLater,
            ),
            ("Banned", TransferRejectedOther, OtherSource),
            ("Banned (no leechers)", TransferRejectedOther, OtherSource),
            ("Cancelled", TransferRejectedOther, OtherSource),
            ("Pending shutdown.", TransferRejectedOther, OtherSource),
            ("File read error.", TransferRejectedOther, OtherSource),
            (
                "Enqueue failed due to internal error",
                TransferRejectedOther,
                OtherSource,
            ),
            ("Disallowed extension", TransferRejectedOther, OtherSource),
        ] {
            for exception in [format!("Transfer rejected: {reason}"), reason.to_string()] {
                let row = converted(json!("Completed, Rejected"), Some(&exception), 3);
                let f = row.failure.clone().expect("a failure");
                assert_eq!((f.code, f.retry), (code, retry), "{exception:?}");
                assert_eq!(f.stage, Stage::Transfer);
                assert_eq!(row.error.as_deref(), Some(exception.as_str()));
                assert_eq!(row.state, DS::Failed(exception.clone()));
                assert_eq!(f.cause.map(|c| c.raw), Some(exception));
            }
        }

        let bare = converted(json!(528u64), None, 3);
        let f = bare.failure.expect("a failure");
        assert_eq!((f.code, f.retry), (TransferRejectedOther, OtherSource));
        assert_eq!(f.cause, None);
        assert_eq!(bare.error.as_deref(), Some("Transfer rejected by peer"));
        assert_eq!(bare.state, DS::Failed("Transfer rejected by peer".into()));
    }

    #[test]
    fn failed_transfers_keep_the_exception_as_their_message_else_the_state_text() {
        let closed = "Transfer failed: The connection was closed unexpectedly";
        let dropped = converted(json!("Completed, Errored"), Some(closed), 3);
        let f = dropped.failure.expect("a failure");
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::TransferConnectionFailed,
                Stage::Transfer,
                RetryAdvice::OtherSource
            )
        );
        assert_eq!(f.cause.map(|c| c.raw).as_deref(), Some(closed));
        assert_eq!(dropped.error.as_deref(), Some(closed));
        assert_eq!(dropped.state, DS::Failed(closed.into()));

        // Was None on origin/master: the visible change decided under table A.
        let bare = converted(json!("Completed, Errored"), None, 3);
        assert_eq!(bare.error.as_deref(), Some("Download error"));
        assert_eq!(bare.state, DS::Failed("Download error".into()));
        assert_eq!(bare.failure.expect("a failure").cause, None);

        // slskd restarted mid-transfer: the peer itself is fine.
        let restart = converted(
            json!("Completed, Errored"),
            Some("Application shut down"),
            3,
        )
        .failure
        .expect("a failure");
        assert_eq!(
            (restart.code, restart.retry),
            (
                FailureCode::TransferConnectionFailed,
                RetryAdvice::SameSourceLater
            )
        );

        let offline = converted(json!(272u64), Some("User peer appears to be offline"), 3)
            .failure
            .expect("a failure");
        assert_eq!(
            (offline.code, offline.stage, offline.retry),
            (
                FailureCode::PeerOffline,
                Stage::Transfer,
                RetryAdvice::OtherSource
            )
        );

        let mismatch = converted(
            json!("Completed, Aborted"),
            Some("Transfer aborted: the remote size of 12 does not match expected size 10"),
            3,
        )
        .failure
        .expect("a failure");
        assert_eq!(
            (mismatch.code, mismatch.retry),
            (FailureCode::TransferSizeMismatch, RetryAdvice::OtherSource)
        );
        let aborted = converted(json!(1040u64), None, 3);
        assert_eq!(aborted.error.as_deref(), Some("Aborted"));
        let f = aborted.failure.expect("a failure");
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::TransferAborted, RetryAdvice::OtherSource)
        );

        let waited = "The wait timed out after 15000 milliseconds";
        let timed_out = converted(json!("Completed, TimedOut"), Some(waited), 3);
        assert_eq!(timed_out.error.as_deref(), Some(waited));
        let f = timed_out.failure.expect("a failure");
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::TransferTimedOut, RetryAdvice::OtherSource)
        );
        assert_eq!(
            converted(json!(144u64), None, 3).error.as_deref(),
            Some("Download timed out")
        );

        let cancelled = converted(
            json!("Completed, Cancelled"),
            Some("The operation was canceled."),
            3,
        );
        assert_eq!(cancelled.state, DS::Cancelled);
        assert_eq!(
            cancelled.error.as_deref(),
            Some("The operation was canceled.")
        );
        let f = cancelled.failure.expect("a failure");
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::TransferCancelled, RetryAdvice::NotRetryable)
        );
        let bare_cancel = converted(json!(80u64), None, 3);
        assert_eq!(bare_cancel.state, DS::Cancelled);
        assert_eq!(bare_cancel.error.as_deref(), Some("Cancelled"));

        // Not a state slskd sends; Soulful's parser accepts it.
        let import = converted(json!("ImportFailed"), None, 3);
        assert_eq!(import.error.as_deref(), Some("Import failed"));
        let f = import.failure.expect("a failure");
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::ImportBeetsFailed,
                Stage::Import,
                RetryAdvice::SameSourceLater
            )
        );
    }

    /// slskd logged out of Soulseek: every peer fails the same way until it
    /// logs back in. Soulseek.NET throws before the transfer starts, so slskd
    /// stores the text through `TryFail`, which drops everything up to the
    /// first colon of the `AggregateException` its retries end in
    /// (`DownloadService.cs:1059`, `Common/Retry.cs:149`).
    #[test]
    fn a_transfer_slskd_could_not_start_while_logged_out_waits_for_slskd() {
        let refused = "The server connection must be connected and logged in to download files (currently: Disconnected)";
        let after_retries = format!("Disconnected)) ({refused}) ({refused})");
        for exception in [refused.to_string(), after_retries] {
            let row = converted(json!("Completed, Errored"), Some(&exception), 3);
            let f = row.failure.expect("a failure");
            assert_eq!(
                (f.code, f.stage, f.retry),
                (
                    FailureCode::BackendUnreachable,
                    Stage::Transfer,
                    RetryAdvice::SameSourceLater
                ),
                "{exception:?}"
            );
            assert_eq!(row.error.as_deref(), Some(exception.as_str()));
            assert_eq!(f.cause.map(|c| c.raw), Some(exception));
        }
    }

    #[test]
    fn a_succeeded_transfer_keeps_no_failure_from_an_earlier_attempt() {
        let row = converted(
            json!("Completed, Succeeded"),
            Some("Transfer failed: The connection was closed unexpectedly"),
            3,
        );
        assert_eq!(row.state, DS::Completed);
        assert_eq!(row.failure, None);
        assert_eq!(row.error, None);
    }

    /// slskd's retry writes `Queued, Locally` but leaves the failed
    /// attempt's exception on the record until the next attempt starts.
    #[test]
    fn a_transfer_waiting_to_retry_keeps_no_failure_from_the_failed_attempt() {
        let row = converted(
            json!("Queued, Locally"),
            Some("Transfer failed: The connection was closed unexpectedly"),
            3,
        );
        assert_eq!(row.state, DS::Queued);
        assert_eq!(row.failure, None);
        assert_eq!(row.error, None);
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
                    "attempts": 2,
                    "placeInQueue": 7,
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
        let row = files.files[0].clone().into_progress(3);
        assert_eq!(row.state, DS::Completed);
        assert_eq!(row.queue_position, Some(7));
        assert_eq!(row.attempts, 2);
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
        let progress = files.files[0].clone().into_progress(3);
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
