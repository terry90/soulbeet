use serde::{Deserialize, Serialize};

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
    NotFound,
    TimedOut,
}

/// Result of a download search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub search_id: String,
    pub groups: Vec<DownloadableGroup>,
    pub has_more: bool,
    pub state: SearchState,
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
        }
    }

    pub fn failed(id: String, source: String, item: String, error: String) -> Self {
        Self {
            id,
            source,
            item,
            size: 0,
            transferred: 0,
            state: DownloadState::Failed(error.clone()),
            percent: 0.0,
            speed: 0.0,
            error: Some(error),
            backend: None,
            batch_id: None,
            batch_label: None,
            local_path: None,
            import_output: None,
            retryable: false,
        }
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

    pub fn with_import_details(
        mut self,
        local_path: Option<String>,
        import_output: Option<String>,
    ) -> Self {
        self.local_path = local_path;
        self.import_output = import_output;
        self
    }
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
    /// peer and did not start another one.
    AlreadyInFlight,
    Failed(String),
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
    /// A track moved to another peer. The client keys rows by remote path and
    /// every peer serves the track under its own, so a plain progress update
    /// would leave the abandoned transfer's row sitting next to the retry's.
    /// Failover rebinds one download rather than starting a second, and the
    /// panel has to show it that way.
    Replaced {
        previous_item: String,
        entry: DownloadProgress,
    },
    /// Auto-download lifecycle events (displayed inline on search rows, not in download panel)
    AutoDownload(AutoDownloadEvent),
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
        results: Vec<DownloadableGroup>,
        best_score: f64,
        threshold: f64,
    },
    /// Auto-download pipeline failed
    Failed { batch_id: String, error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_older_servers_deserialize() {
        let v = serde_json::json!({"id":"1","source":"p","item":"a.flac","size":1,"transferred":0,
            "state":"Queued","percent":0.0,"error":null});
        let row: DownloadProgress = serde_json::from_value(v).unwrap();
        assert_eq!(row.local_path, None);
        assert_eq!(row.import_output, None);
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
