use serde::{Deserialize, Serialize};

/// A track from an artist radio, in radio order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeezerTrack {
    pub artist: String,
    pub title: String,
}

/// A track from a public Deezer playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerPlaylistTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_s: Option<u32>,
}

/// A public Deezer playlist with its ordered tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerPlaylist {
    pub id: u64,
    pub title: String,
    pub description: Option<String>,
    pub nb_tracks: u32,
    pub tracks: Vec<DeezerPlaylistTrack>,
}

/// Deezer answers errors with HTTP 200 and an `error` object.
#[derive(Debug, Deserialize)]
pub(super) struct Envelope<T> {
    pub data: Option<Vec<T>>,
    pub next: Option<String>,
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ApiError {
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ArtistHit {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RadioTrack {
    pub title: String,
    pub artist: RadioArtist,
}

#[derive(Debug, Deserialize)]
pub(super) struct RadioArtist {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistResponse {
    pub id: Option<u64>,
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub nb_tracks: u32,
    #[serde(default)]
    pub tracks: Option<PlaylistTracksPage>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistTracksPage {
    #[serde(default)]
    pub data: Vec<PlaylistTrackItem>,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistTrackItem {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub artist: Option<RadioArtist>,
    #[serde(default)]
    pub album: Option<PlaylistAlbum>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistAlbum {
    #[serde(default)]
    pub title: String,
}
