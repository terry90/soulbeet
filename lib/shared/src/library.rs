use serde::{Deserialize, Serialize};

/// A track from a beets library
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub path: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub album_artist: String,
    pub library_path: String,
}

/// A group of duplicate tracks (same artist + title across different libraries)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub artist: String,
    pub title: String,
    pub tracks: Vec<LibraryTrack>,
}

/// Result of duplicate detection across libraries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateReport {
    pub duplicates: Vec<DuplicateGroup>,
    pub total_duplicate_tracks: usize,
    pub libraries_scanned: Vec<String>,
}
