use serde::{Deserialize, Serialize};

/// Supported external platforms for URL-based playlist import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistPlatform {
    Spotify,
    YouTubeMusic,
    Deezer,
    ListenBrainz,
}

impl PlaylistPlatform {
    pub fn display_name(self) -> &'static str {
        match self {
            PlaylistPlatform::Spotify => "Spotify",
            PlaylistPlatform::YouTubeMusic => "YouTube Music",
            PlaylistPlatform::Deezer => "Deezer",
            PlaylistPlatform::ListenBrainz => "ListenBrainz",
        }
    }
}

/// A single track resolved from an external playlist URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalTrack {
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
}

/// A resolved external playlist with metadata and ordered tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPlaylist {
    pub platform: PlaylistPlatform,
    pub source_url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tracks: Vec<ExternalTrack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_note: Option<String>,
}

/// How a previewed playlist track matched the user's Navidrome library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMatchStatus {
    InLibrary,
    Fuzzy,
    Missing,
}

/// One track in a playlist import preview, annotated with library match info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPreviewTrack {
    pub index: usize,
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
    pub match_status: ImportMatchStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_label: Option<String>,
}

/// Preview response returned before committing a playlist import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistImportPreview {
    pub platform: PlaylistPlatform,
    pub source_url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_note: Option<String>,
    pub library_checked: bool,
    pub tracks: Vec<ImportPreviewTrack>,
}

/// Whether a playlist import creates a new playlist or appends to an existing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaylistImportTarget {
    Create {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
        #[serde(default)]
        allow_duplicate_name: bool,
    },
    Existing {
        playlist_id: String,
    },
}

/// A selected track submitted to commit a playlist import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPlaylistImportTrack {
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
}

/// Request payload to commit a previewed playlist import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPlaylistImportRequest {
    pub target: PlaylistImportTarget,
    pub folder_id: String,
    pub tracks: Vec<CommitPlaylistImportTrack>,
}

/// Summary returned once a playlist import batch and playlist write are started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPlaylistImportResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub playlist_id: String,
    pub playlist_name: String,
    pub already_in_library: usize,
    pub queued_for_download: usize,
    pub already_running: usize,
    pub refused: usize,
}
