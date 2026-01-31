use serde::{Deserialize, Serialize};

use crate::musicbrainz::{Album, Track};

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
        }
    }

    pub fn with_state(mut self, state: DownloadState) -> Self {
        self.state = state;
        self
    }
}

/// Response from queueing downloads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedDownload {
    pub id: String,
    pub source: String,
    pub item: String,
    pub size: u64,
    pub error: Option<String>,
}

impl QueuedDownload {
    pub fn success(id: String, source: String, item: String, size: u64) -> Self {
        Self { id, source, item, size, error: None }
    }

    pub fn failed(id: String, source: String, item: String, error: String) -> Self {
        Self { id, source, item, size: 0, error: Some(error) }
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}
