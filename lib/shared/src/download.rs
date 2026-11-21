use serde::{Deserialize, Serialize};

use crate::failure::{Cause, Failure};
use crate::metadata::{Album, Track};

#[derive(Serialize, Clone, PartialEq, Deserialize, Debug, Default)]
pub struct DownloadQuery {
    pub album: Option<Album>,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub backend: Option<String>,
}

impl DownloadQuery {
    pub fn new(tracks: Vec<Track>) -> Self {
        Self {
            album: None,
            tracks,
            backend: None,
        }
    }

    pub fn album(mut self, album: Album) -> Self {
        self.album = Some(album);
        self
    }

    pub fn backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = Some(backend.into());
        self
    }
}

impl From<Track> for DownloadQuery {
    fn from(track: Track) -> Self {
        Self::new(vec![track])
    }
}

/// A downloadable item from a search result
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadableItem {
    /// Unique identifier for this item (backend-specific format)
    pub id: String,
    /// Source identifier (e.g., username for P2P, service for streaming)
    pub source: String,
    /// Display name for the track
    pub title: String,
    /// Artist name
    pub artist: String,
    /// Album name
    pub album: String,
    /// File size in bytes (if known)
    pub size: Option<u64>,
    /// Duration in seconds (if known)
    pub duration: Option<u32>,
    /// Quality descriptor (e.g., "FLAC", "320kbps MP3")
    pub quality: String,
    /// Quality score 0.0-1.0 for ranking
    pub quality_score: f64,
    /// Backend-specific data (JSON serialized)
    #[serde(default)]
    pub backend_data: Option<String>,
    /// How well the file matched what was searched, 0.0-1.0.
    #[serde(default)]
    pub match_score: f64,
    /// Scored below the backend's floor, kept because the search asked to.
    #[serde(default)]
    pub below_floor: bool,
    #[serde(default)]
    pub facts: FileFacts,
}

/// One file and its source as the backend reported them; None for what it did not report.
/// Stored with each search's candidates: a field added later reads as its default in what was
/// stored before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileFacts {
    /// Lowercase, from the remote file name.
    pub extension: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub variable_bitrate: Option<bool>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u32>,
    /// Audio files in the same remote folder in the source's answer.
    pub folder_audio_files: Option<u32>,
    pub source_has_free_slot: Option<bool>,
    /// Bytes per second.
    pub source_upload_speed: Option<u64>,
    pub source_queue_length: Option<u64>,
    /// Facts only this backend has, by name.
    pub backend_facts: serde_json::Map<String, serde_json::Value>,
}

/// What a download search asks a backend for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchSpec {
    pub album: Option<Album>,
    pub tracks: Vec<Track>,
    /// Sent in place of the text the backend builds from `album` and `tracks`;
    /// results are still scored against them.
    #[serde(default)]
    pub text: Option<String>,
    /// Keep files scored below the backend's floor, each flagged `below_floor`.
    #[serde(default)]
    pub keep_below_floor: bool,
}

impl SearchSpec {
    /// A search for `album` and `tracks` with the backend's own text, below-floor files dropped.
    pub fn of(album: Option<Album>, tracks: Vec<Track>) -> Self {
        Self {
            album,
            tracks,
            text: None,
            keep_below_floor: false,
        }
    }
}

/// What a finished search read, so an empty one says why. Stored with each search: a field
/// added later reads as its default in what was stored before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchStats {
    pub query_text: String,
    pub response_count: u32,
    pub file_count: u32,
    pub locked_file_count: u32,
    /// Files with an extension that is not audio.
    pub not_audio: u32,
    /// Audio files scored below the floor, kept or not.
    pub below_floor: u32,
}

/// A group of downloadable items (e.g., album from one source)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadableGroup {
    /// Source identifier
    pub source: String,
    /// Group identifier (e.g., folder path, album ID)
    pub group_id: String,
    /// Display title for the group
    pub title: String,
    /// Artist name
    pub artist: Option<String>,
    /// Number of items in group
    pub item_count: usize,
    /// Total size in bytes
    pub total_size: u64,
    /// Items in this group
    pub items: Vec<DownloadableItem>,
    /// Dominant quality in the group
    pub quality: String,
    /// Overall score for ranking
    pub score: f64,
}

impl DownloadableGroup {
    pub fn size_mb(&self) -> u64 {
        self.total_size / (1024 * 1024)
    }
}

/// State of a search operation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SearchState {
    InProgress,
    Completed,
    TimedOut,
}

/// Result of a download search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub search_id: String,
    pub groups: Vec<DownloadableGroup>,
    pub has_more: bool,
    pub state: SearchState,
    /// What the backend read; all empty while in progress.
    #[serde(default)]
    pub stats: SearchStats,
}

/// State of a download operation
#[derive(Debug, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum DownloadState {
    Queued,
    InProgress,
    Completed,
    /// Waiting for the library importer; payload is the jobs ahead, including the running one (always >= 1)
    ImportQueued(u32),
    Importing,
    Imported,
    ImportSkipped,
    Failed(String),
    Cancelled,
}

/// Progress of a single download
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Unique identifier
    pub id: String,
    /// Source identifier
    pub source: String,
    /// Item being downloaded (title or filename)
    pub item: String,
    /// Total size in bytes
    pub size: u64,
    /// Bytes transferred
    pub transferred: u64,
    /// Current state
    pub state: DownloadState,
    /// Progress percentage (0.0-100.0)
    pub percent: f64,
    /// Average download speed in bytes per second
    #[serde(default)]
    pub speed: f64,
    /// Error message if failed
    pub error: Option<String>,
    /// Which download backend owns this transfer
    #[serde(default)]
    pub backend: Option<String>,
    /// Groups this download with others from the same batch
    #[serde(default)]
    pub batch_id: Option<String>,
    /// Human-readable batch label (album name)
    #[serde(default)]
    pub batch_label: Option<String>,
    /// Where the downloaded file sits on disk once known
    #[serde(default)]
    pub local_path: Option<String>,
    /// Tail of the importer's output for this row
    #[serde(default)]
    pub import_output: Option<String>,
    /// The server keeps this row's import job, so Retry import can resubmit it
    #[serde(default)]
    pub retryable: bool,
    /// Why the row failed or was skipped, typed; on an imported row, the step after the import
    /// that failed, which its note also says
    #[serde(default)]
    pub failure: Option<Failure>,
    /// Which kind of skip an ImportSkipped row is
    #[serde(default)]
    pub skip: Option<SkipKind>,
    /// The transfer waits in the source's own queue, not in the backend's
    #[serde(default)]
    pub queued_at_source: bool,
    /// How many times the backend tried this transfer.
    #[serde(default)]
    pub attempts: u32,
    /// The transfer's place in the source's queue, when last asked.
    #[serde(default)]
    pub queue_position: Option<u32>,
    /// A failed attempt slskd says it will retry, which the row reads as
    /// Queued: the failure that attempt stated. Never sent to the browser. A
    /// watcher reads the row as this failure once it has held it pending for
    /// `RETRY_PENDING_FOR`.
    #[serde(skip)]
    pub pending_retry: Option<Failure>,
    /// The ledger job slot this row is the file of. Never sent to the browser.
    #[serde(skip)]
    pub job: Option<JobSlot>,
    /// The id of the backend request (`DownloadBackend::new_request`) that made this transfer;
    /// None for one Soulful did not make. Never sent to the browser.
    #[serde(skip)]
    pub request: Option<String>,
    /// For an imported row, whether Navidrome lists its file; None before its wait and without Navidrome
    #[serde(default)]
    pub index: Option<IndexState>,
}

/// Whether Navidrome lists an imported row's file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Waiting,
    Listed,
    Failed(Failure),
}

/// One file of a ledger job: the job's id and the file's slot in it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobSlot {
    pub job: String,
    pub slot: u32,
}

/// Whether a source is online now, as its backend reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePresence {
    Online,
    Away,
    Offline,
    /// slskd's blacklist holds this user, so nothing can be downloaded from it.
    Blocked,
}

/// Why an import was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    AlreadyInLibrary,
    NoMatch { candidates: Option<u32> },
    NothingToImport,
}

impl DownloadProgress {
    pub fn queued(id: String, source: String, item: String, size: u64) -> Self {
        Self {
            id,
            source,
            item,
            size,
            transferred: 0,
            state: DownloadState::Queued,
            percent: 0.0,
            speed: 0.0,
            error: None,
            backend: None,
            batch_id: None,
            batch_label: None,
            local_path: None,
            import_output: None,
            retryable: false,
            failure: None,
            skip: None,
            queued_at_source: false,
            attempts: 0,
            queue_position: None,
            pending_retry: None,
            job: None,
            request: None,
            index: None,
        }
    }

    pub fn failed(id: String, source: String, item: String, failure: Failure) -> Self {
        Self::queued(id, source, item, 0).failed_with(failure)
    }

    /// This row Failed with `failure`, any earlier failure or skip dropped.
    pub fn failed_with(self, failure: Failure) -> Self {
        self.without_failure()
            .with_state(DownloadState::Failed(String::new()))
            .with_failure(failure)
    }

    /// The one writer of `error` for a failure: error = Some(failure.message),
    /// a Failed(_) state gets failure.message as its payload, other states are kept.
    pub fn with_failure(mut self, failure: Failure) -> Self {
        self.error = Some(failure.message.clone());
        if let DownloadState::Failed(note) = &mut self.state {
            note.clone_from(&failure.message);
        }
        self.failure = Some(failure);
        self
    }

    /// This row with no failure and no skip, and without the note when
    /// `with_failure` wrote it. A note with no failure, the one on an
    /// imported row, stays.
    pub fn without_failure(mut self) -> Self {
        if self.failure.take().is_some() {
            self.error = None;
        }
        self.skip = None;
        self
    }

    pub fn with_backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = Some(backend.into());
        self
    }

    pub fn with_state(mut self, state: DownloadState) -> Self {
        self.state = state;
        self
    }

    pub fn with_batch(mut self, batch_id: String, batch_label: String) -> Self {
        self.batch_id = Some(batch_id);
        self.batch_label = Some(batch_label);
        self
    }

    pub fn with_job(mut self, job: JobSlot) -> Self {
        self.job = Some(job);
        self
    }

    pub fn with_import_details(
        mut self,
        local_path: Option<String>,
        import_output: Option<String>,
    ) -> Self {
        self.local_path = local_path;
        self.import_output = import_output;
        self
    }

    /// Whether the server still moves this row on by itself: its transfer, its import or its
    /// wait for Navidrome is not over. Any other row changes only when its user acts on it.
    pub fn in_flight(&self) -> bool {
        match self.state {
            DownloadState::Queued
            | DownloadState::InProgress
            | DownloadState::Completed
            | DownloadState::ImportQueued(_)
            | DownloadState::Importing => true,
            DownloadState::Imported => self.index == Some(IndexState::Waiting),
            DownloadState::ImportSkipped | DownloadState::Failed(_) | DownloadState::Cancelled => {
                false
            }
        }
    }
}

/// Applies `rows`, a `DownloadEvent::Snapshot`, to the panel's rows. A row the panel holds in
/// flight that the snapshot leaves out ended in a row the panel never got, so the snapshot
/// replaces every row in flight. The rows already over stay until the user clears them.
pub fn apply_snapshot(
    panel: &mut std::collections::HashMap<String, DownloadProgress>,
    rows: Vec<DownloadProgress>,
) {
    panel.retain(|_, row| !row.in_flight());
    for row in rows {
        panel.insert(row.item.clone(), row);
    }
}

/// How a skipped import is run again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportChoice {
    /// Let beets decide, as the first import did.
    Auto,
    /// Import with the files' own tags.
    AsIs,
    /// One of the candidates beets found for this import.
    Candidate { data_source: String, id: String },
    /// A MusicBrainz link or ID as the user typed it.
    MusicBrainz(String),
}

/// What the server still keeps for a row after it refused a retry or a
/// matches request. A refusal carries it as its details, so the row offers
/// only what the server can still do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeptImport {
    /// Nothing: the kept import is gone.
    Gone,
    /// The import stays kept, with whatever matches beets recorded for it.
    /// Only the choice sent, if any, was refused.
    Kept,
}

/// A refused retry or matches request: why, and what the server still keeps for the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRefusal {
    pub failure: Failure,
    pub kept: KeptImport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchKind {
    Track,
    Album,
}

/// A match beets weighed for a skipped import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCandidate {
    pub data_source: String,
    pub id: String,
    pub artist: String,
    /// The track title, or the album name for an album match.
    pub title: String,
    /// The album a track match belongs to.
    pub album: Option<String>,
    pub year: Option<u32>,
    pub country: Option<String>,
    pub disambig: Option<String>,
    /// How close the match is, 1000 being exact.
    pub per_mille: u16,
    /// What differs, by beets' penalty names, the weightiest first.
    pub differs: Vec<String>,
    pub length_secs: Option<u32>,
    /// How the files fit an album match's tracks.
    pub tracks: Option<TrackFit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackFit {
    pub matched: u32,
    pub total: u32,
    /// Files no track matched, which an import of this match leaves behind.
    pub extra_files: u32,
}

/// The matches an import skipped for, best first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchChoices {
    pub kind: MatchKind,
    pub candidates: Vec<MatchCandidate>,
}

const MAX_LABEL_BYTES: usize = 200;

/// Folder name for a download batch: `<label> [<first 8 alphanumerics of batch_id>]`,
/// safe on Windows and Unix file systems.
pub fn batch_folder(label: &str, batch_id: &str) -> String {
    let mut label: String = label
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if label.len() > MAX_LABEL_BYTES {
        let mut end = MAX_LABEL_BYTES;
        while !label.is_char_boundary(end) {
            end -= 1;
        }
        label.truncate(end);
    }
    let label = label.trim_end_matches(['.', ' ']);
    let label = if label.is_empty() { "Download" } else { label };
    let short: String = batch_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("{label} [{short}]")
}

/// What the backend did with one file of an enqueue request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnqueueStatus {
    Enqueued,
    /// The backend already has an unfinished transfer of this file from this
    /// peer and did not start another one. The cause is the backend's own
    /// answer.
    AlreadyInFlight(Cause),
    /// One of the user's batches is already downloading this file into the
    /// requested folder, so it was not enqueued again. That batch shows its
    /// row.
    AlreadyRunning,
    Failed(Failure),
}

/// Response from queueing downloads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedDownload {
    /// The backend's transfer id when `Enqueued`, else the remote filename.
    pub id: String,
    pub source: String,
    pub item: String,
    pub size: u64,
    pub status: EnqueueStatus,
}

/// Wrapper for all download-related WebSocket events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DownloadEvent {
    /// Standard download progress updates
    Progress(Vec<DownloadProgress>),
    /// The rows of every unfinished file, first on each socket opened with `snapshot=true` and
    /// again after the socket missed events. The client applies it with `apply_snapshot`.
    Snapshot(Vec<DownloadProgress>),
    /// A track moved to another peer. The client keys rows by remote path and
    /// every peer serves the track under its own, so a plain progress update
    /// would leave the abandoned transfer's row sitting next to the retry's.
    /// Failover rebinds one download rather than starting a second, and the
    /// panel has to show it that way.
    Replaced {
        previous_item: String,
        entry: Box<DownloadProgress>,
    },
    /// Auto-download lifecycle events (displayed inline on search rows, not in download panel)
    AutoDownload(AutoDownloadEvent),
    /// The server's build id, the first frame on a socket whose client asked
    /// for it. A bundle from another build may misread every event after it.
    Build(String),
}

/// Events from the auto-download pipeline, sent over WebSocket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AutoDownloadEvent {
    /// Backend search started
    Searching {
        batch_id: String,
        query: String,
        backend_count: usize,
    },
    /// Results scored and ranked
    ScoringResults {
        batch_id: String,
        result_count: usize,
        best_score: f64,
    },
    /// Source selected for download
    PickedSource {
        batch_id: String,
        source: String,
        score: f64,
        quality: String,
        track_count: usize,
    },
    /// Download queued with the picked source
    Downloading { batch_id: String },
    /// Best score below threshold, client should show manual source picker
    FallbackToManual {
        batch_id: String,
        /// The stored search `results` came from, which a pick names.
        #[serde(default)]
        search_id: String,
        results: Vec<DownloadableGroup>,
        best_score: f64,
        threshold: f64,
    },
    /// Auto-download pipeline failed
    Failed { batch_id: String, failure: Failure },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::{FailureCode, RetryAdvice, Stage};

    /// Stats and file facts stored before a field was added still read.
    #[test]
    fn stored_stats_and_facts_missing_a_field_read_it_as_its_default() {
        let stats: SearchStats =
            serde_json::from_str(r#"{"query_text": "Kowloon Wake Up", "file_count": 23}"#).unwrap();
        assert_eq!(
            (stats.file_count, stats.not_audio, stats.below_floor),
            (23, 0, 0)
        );
        let facts: FileFacts = serde_json::from_str(r#"{"extension": "flac"}"#).unwrap();
        assert_eq!(
            facts,
            FileFacts {
                extension: Some("flac".into()),
                ..FileFacts::default()
            }
        );
    }

    #[test]
    fn a_row_failed_with_a_failure_keeps_no_earlier_one() {
        let earlier = Failure::new(
            FailureCode::ImportNoMatch,
            Stage::Import,
            RetryAdvice::NeedsDecision,
            "beets found no matches.",
        );
        let now = Failure::new(
            FailureCode::ImportFilesGone,
            Stage::Import,
            RetryAdvice::NotRetryable,
            "The kept files are gone",
        );
        let kept = DownloadProgress {
            skip: Some(SkipKind::NoMatch { candidates: None }),
            ..DownloadProgress::queued("t-1".into(), "p".into(), "1.flac".into(), 4)
                .with_state(DownloadState::ImportSkipped)
                .with_failure(earlier)
        };
        let failed = kept.failed_with(now.clone());
        assert_eq!(
            (failed.state, failed.error, failed.failure, failed.skip),
            (
                DownloadState::Failed("The kept files are gone".into()),
                Some("The kept files are gone".into()),
                Some(now),
                None
            )
        );
    }

    #[test]
    fn a_query_that_names_a_metadata_provider_still_reads() {
        let old = serde_json::json!({"album": null, "tracks": [], "metadata_provider": "lastfm"});
        let q: DownloadQuery = serde_json::from_value(old).unwrap();
        assert_eq!(
            serde_json::to_value(&q).unwrap().get("metadata_provider"),
            None
        );
    }

    fn row_in(item: &str, state: DownloadState) -> DownloadProgress {
        DownloadProgress::queued(format!("t-{item}"), "p".into(), item.into(), 1).with_state(state)
    }

    #[test]
    fn only_a_row_the_server_still_moves_on_is_in_flight() {
        for state in [
            DownloadState::Queued,
            DownloadState::InProgress,
            DownloadState::Completed,
            DownloadState::ImportQueued(2),
            DownloadState::Importing,
        ] {
            assert!(row_in("a.flac", state.clone()).in_flight(), "{state:?}");
        }
        for state in [
            DownloadState::Imported,
            DownloadState::ImportSkipped,
            DownloadState::Failed("Per-track timeout".into()),
            DownloadState::Cancelled,
        ] {
            assert!(!row_in("a.flac", state.clone()).in_flight(), "{state:?}");
        }
        let imported = |index| DownloadProgress {
            index,
            ..row_in("a.flac", DownloadState::Imported)
        };
        assert!(imported(Some(IndexState::Waiting)).in_flight());
        assert!(!imported(Some(IndexState::Listed)).in_flight());
    }

    #[test]
    fn a_snapshot_replaces_the_rows_in_flight_and_keeps_the_rows_already_over() {
        let waiting = DownloadProgress {
            index: Some(IndexState::Waiting),
            ..row_in("waiting.flac", DownloadState::Imported)
        };
        let mut panel: std::collections::HashMap<String, DownloadProgress> = [
            // Ended while the socket missed its last row.
            row_in("ended.flac", DownloadState::InProgress),
            row_in("running.flac", DownloadState::InProgress),
            // Listed while the socket missed its last row.
            waiting,
            row_in(
                "failed.flac",
                DownloadState::Failed("Per-track timeout".into()),
            ),
            row_in("skipped.flac", DownloadState::ImportSkipped),
        ]
        .into_iter()
        .map(|row| (row.item.clone(), row))
        .collect();
        let running = DownloadProgress {
            percent: 60.0,
            ..row_in("running.flac", DownloadState::InProgress)
        };
        let new = row_in("new.flac", DownloadState::Queued);

        apply_snapshot(&mut panel, vec![running.clone(), new.clone()]);

        let mut items: Vec<&str> = panel.keys().map(String::as_str).collect();
        items.sort();
        assert_eq!(
            items,
            vec!["failed.flac", "new.flac", "running.flac", "skipped.flac"]
        );
        assert_eq!(panel["running.flac"], running);
        assert_eq!(panel["new.flac"], new);
    }

    #[test]
    fn rows_from_older_servers_deserialize() {
        let v = serde_json::json!({"id":"1","source":"p","item":"a.flac","size":1,"transferred":0,
            "state":"Queued","percent":0.0,"error":null});
        let row: DownloadProgress = serde_json::from_value(v).unwrap();
        assert_eq!(row.local_path, None);
        assert_eq!(row.import_output, None);
        assert_eq!(row.failure, None);
        assert_eq!(row.skip, None);
        assert!(!row.queued_at_source);
        assert_eq!(row.job, None);
        assert_eq!(row.index, None);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.queue_position, None);
        let sent = serde_json::to_value(row.with_job(JobSlot {
            job: "j1".into(),
            slot: 0,
        }))
        .unwrap();
        assert!(sent.get("job").is_none(), "{sent}");
    }

    #[test]
    fn with_failure_writes_error_and_the_failed_payload_from_the_message() {
        let f = Failure::new(
            FailureCode::TransferNeverAppeared,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            "Download never appeared in slskd",
        );
        let row = DownloadProgress::queued("1".into(), "p".into(), "a.flac".into(), 1)
            .with_state(DownloadState::Failed("old".into()))
            .with_failure(f.clone());
        assert_eq!(
            row.state,
            DownloadState::Failed("Download never appeared in slskd".into())
        );
        assert_eq!(
            row.error.as_deref(),
            Some("Download never appeared in slskd")
        );
        assert_eq!(row.failure, Some(f.clone()));
        let skipped = DownloadProgress::queued("1".into(), "p".into(), "a.flac".into(), 1)
            .with_state(DownloadState::ImportSkipped)
            .with_failure(f);
        assert_eq!(skipped.state, DownloadState::ImportSkipped);
    }

    #[test]
    fn without_failure_drops_the_failure_its_note_and_the_skip() {
        let f = Failure::new(
            FailureCode::ImportNoMatch,
            Stage::Import,
            RetryAdvice::NeedsDecision,
            "No close match",
        );
        let skipped = DownloadProgress {
            skip: Some(SkipKind::NoMatch { candidates: None }),
            ..DownloadProgress::queued("1".into(), "p".into(), "a.flac".into(), 1)
                .with_state(DownloadState::ImportSkipped)
                .with_failure(f)
        };
        let cleared = skipped.without_failure();
        assert_eq!(
            (&cleared.failure, cleared.skip, &cleared.error),
            (&None, None, &None)
        );

        let note = "Imported as-is (untagged)";
        let imported = DownloadProgress {
            error: Some(note.into()),
            ..DownloadProgress::queued("1".into(), "p".into(), "a.flac".into(), 1)
                .with_state(DownloadState::Imported)
        };
        assert_eq!(imported.without_failure().error.as_deref(), Some(note));
    }

    #[test]
    fn skip_kind_round_trips() {
        for (kind, wire) in [
            (
                SkipKind::NoMatch {
                    candidates: Some(2),
                },
                serde_json::json!({"no_match": {"candidates": 2}}),
            ),
            (
                SkipKind::NoMatch { candidates: None },
                serde_json::json!({"no_match": {"candidates": null}}),
            ),
            (
                SkipKind::AlreadyInLibrary,
                serde_json::json!("already_in_library"),
            ),
            (
                SkipKind::NothingToImport,
                serde_json::json!("nothing_to_import"),
            ),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), wire);
            assert_eq!(serde_json::from_value::<SkipKind>(wire).unwrap(), kind);
        }
    }

    #[test]
    fn import_queued_round_trips_and_sorts_between_completed_and_importing() {
        let s = DownloadState::ImportQueued(2);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!({"ImportQueued":2})
        );
        assert!(DownloadState::Completed < DownloadState::ImportQueued(1));
        assert!(DownloadState::ImportQueued(1) < DownloadState::ImportQueued(3));
        assert!(DownloadState::ImportQueued(9) < DownloadState::Importing);
    }

    #[test]
    fn batch_folder_sanitizes_hostile_labels() {
        let id = "1f2e3d4c-aaaa-bbbb-cccc-ddddeeeeffff";
        assert_eq!(batch_folder("Blue Train", id), "Blue Train [1f2e3d4c]");
        assert_eq!(batch_folder("AC/DC: Live?", id), "AC_DC_ Live_ [1f2e3d4c]");
        assert_eq!(batch_folder("  dots... ", id), "dots [1f2e3d4c]");
        assert_eq!(batch_folder("tab\there\u{7}", id), "tab_here_ [1f2e3d4c]");
        assert_eq!(batch_folder("", id), "Download [1f2e3d4c]");
        let long = batch_folder(&"é".repeat(300), id);
        assert!(long.len() <= 200 + " [1f2e3d4c]".len());
        assert!(long.ends_with(" [1f2e3d4c]"));
        assert!(!long.contains('/'));
    }
}
