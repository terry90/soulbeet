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
    Importing,
    Queued,
    Downloaded,
    Imported,
    ImportSkipped,
    Errored,
    ImportFailed,
    Aborted,
    Cancelled,
    Unknown(String),
}

impl From<String> for DownloadState {
    fn from(s: String) -> Self {
        match s.as_str() {
            "Queued" => DownloadState::Queued,
            "InProgress" => DownloadState::InProgress,
            "Completed" => DownloadState::Downloaded,
            "Aborted" => DownloadState::Aborted,
            "Cancelled" => DownloadState::Cancelled,
            "Errored" => DownloadState::Errored,
            "Importing" => DownloadState::Importing,
            "Imported" => DownloadState::Imported,
            "ImportSkipped" => DownloadState::ImportSkipped,
            "ImportFailed" => DownloadState::ImportFailed,
            _ => DownloadState::Unknown(s),
        }
    }
}

// The exact structure of a single file entry
#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub id: String,
    pub username: String,
    pub direction: String,
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub start_offset: u64,
    #[serde(deserialize_with = "deserialize_download_state")]
    pub state: Vec<DownloadState>,
    pub state_description: String,
    pub requested_at: String,
    pub enqueued_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub bytes_transferred: u64,
    #[serde(default)]
    pub average_speed: f64,
    pub bytes_remaining: u64,
    #[serde(default)]
    pub elapsed_time: Option<String>,
    pub percent_complete: f64,
    #[serde(default)]
    pub remaining_time: Option<String>,
    #[serde(default)]
    pub exception: Option<String>,
}

impl FileEntry {
    pub fn get_state(&self) -> Vec<DownloadState> {
        self.state.clone()
    }

    /// Create a new FileEntry from a DownloadResponse with a specified state.
    ///
    /// This is the primary factory method for creating FileEntry objects,
    /// reducing code duplication in the download module.
    pub fn from_download_response(
        response: &DownloadResponse,
        state: DownloadState,
        state_description: String,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            username: response.username.clone(),
            direction: "Download".to_string(),
            filename: response.filename.clone(),
            size: response.size,
            start_offset: 0,
            state: vec![state],
            state_description,
            requested_at: chrono::Utc::now().to_rfc3339(),
            enqueued_at: None,
            started_at: None,
            ended_at: None,
            bytes_transferred: 0,
            average_speed: 0.0,
            bytes_remaining: response.size,
            elapsed_time: None,
            percent_complete: 0.0,
            remaining_time: None,
            exception: response.error.clone(),
        }
    }

    /// Create a queued FileEntry from a DownloadResponse.
    pub fn queued(response: &DownloadResponse) -> Self {
        let mut entry = Self::from_download_response(
            response,
            DownloadState::Queued,
            "Queued for download".to_string(),
        );
        entry.enqueued_at = Some(chrono::Utc::now().to_rfc3339());
        entry
    }

    /// Create an errored FileEntry from a DownloadResponse.
    pub fn errored(response: &DownloadResponse) -> Self {
        Self::from_download_response(
            response,
            DownloadState::Errored,
            response.error.clone().unwrap_or_default(),
        )
    }

    /// Create a new FileEntry with a different state, preserving other fields.
    pub fn with_state(mut self, state: DownloadState, description: String) -> Self {
        self.state = vec![state];
        self.state_description = description;
        self
    }

    /// Create a timeout error entry from an existing FileEntry.
    pub fn as_timeout(&self) -> Self {
        Self {
            id: self.id.clone(),
            username: self.username.clone(),
            direction: "Download".to_string(),
            filename: self.filename.clone(),
            size: self.size,
            start_offset: 0,
            state: vec![DownloadState::Errored],
            state_description: "Download timed out after 1 hour".to_string(),
            requested_at: self.requested_at.clone(),
            enqueued_at: self.enqueued_at.clone(),
            started_at: self.started_at.clone(),
            ended_at: Some(chrono::Utc::now().to_rfc3339()),
            bytes_transferred: self.bytes_transferred,
            average_speed: self.average_speed,
            bytes_remaining: self.bytes_remaining,
            elapsed_time: self.elapsed_time.clone(),
            percent_complete: self.percent_complete,
            remaining_time: None,
            exception: Some("Per-track timeout".to_string()),
        }
    }
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

// Custom deserializer that flattens everything into Vec<FileEntry>
fn deserialize_flattened_files<'de, D>(deserializer: D) -> Result<Vec<FileEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    // First deserialize as generic JSON to traverse it manually
    let v = Value::deserialize(deserializer)?;

    let mut files = Vec::new();

    match &v {
        Value::Array(users) => {
            for user in users {
                // The slskd API returns: [ { "username": "...", "directories": [...] }, ... ]
                if let Some(directories) = user.get("directories").and_then(|d| d.as_array()) {
                    for dir in directories {
                        if let Some(dir_files) = dir.get("files").and_then(|f| f.as_array()) {
                            for file in dir_files {
                                match serde_json::from_value::<FileEntry>(file.clone()) {
                                    Ok(file_entry) => files.push(file_entry),
                                    Err(_) => continue,
                                }
                            }
                        }
                    }
                }
            }
        }
        Value::Object(obj) => {
            // Handle case where response is a single object instead of array
            if let Some(directories) = obj.get("directories").and_then(|d| d.as_array()) {
                for dir in directories {
                    if let Some(dir_files) = dir.get("files").and_then(|f| f.as_array()) {
                        for file in dir_files {
                            match serde_json::from_value::<FileEntry>(file.clone()) {
                                Ok(file_entry) => files.push(file_entry),
                                Err(_) => continue,
                            }
                        }
                    }
                }
            }
        }
        _ => {
            // Unexpected format - return empty
        }
    }

    Ok(files)
}

// Final struct you actually care about
#[derive(Debug, Deserialize)]
pub struct DownloadHistory {
    #[serde(deserialize_with = "deserialize_flattened_files")]
    pub files: Vec<FileEntry>,
}

pub struct FlattenedFiles(pub Vec<FileEntry>);

impl<'de> Deserialize<'de> for FlattenedFiles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_flattened_files(deserializer).map(FlattenedFiles)
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
