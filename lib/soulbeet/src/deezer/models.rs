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

/// Artist reference embedded inside Deezer track and album payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerTrackArtist {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub picture_medium: Option<String>,
}

/// Album reference embedded inside Deezer track payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerTrackAlbum {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub cover_medium: Option<String>,
    #[serde(default)]
    pub cover_big: Option<String>,
}

/// A track hit from `/search/track` or `/artist/{id}/top`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerSearchTrack {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub title_short: Option<String>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub rank: u64,
    pub artist: DeezerTrackArtist,
    pub album: DeezerTrackAlbum,
}

/// An album hit from `/search/album`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerSearchAlbum {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub cover_medium: Option<String>,
    #[serde(default)]
    pub cover_big: Option<String>,
    #[serde(default)]
    pub nb_tracks: Option<u32>,
    #[serde(default)]
    pub record_type: Option<String>,
    pub artist: DeezerTrackArtist,
}

/// An artist hit from `/search/artist`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerSearchArtist {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub picture_medium: Option<String>,
    #[serde(default)]
    pub picture_big: Option<String>,
    #[serde(default)]
    pub nb_fan: Option<u64>,
    #[serde(default)]
    pub nb_album: Option<u32>,
}

/// A track inside a Deezer `/album/{id}` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerAlbumTrack {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub duration: u32,
    pub artist: DeezerTrackArtist,
}

/// Full album metadata and tracklist from `/album/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerAlbumDetail {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub cover_medium: Option<String>,
    #[serde(default)]
    pub cover_big: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub record_type: Option<String>,
    #[serde(default)]
    pub nb_tracks: Option<u32>,
    pub artist: DeezerTrackArtist,
    #[serde(default)]
    pub tracks: Vec<DeezerAlbumTrack>,
}

/// Artist metadata from `/artist/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerArtistDetail {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub picture_medium: Option<String>,
    #[serde(default)]
    pub picture_big: Option<String>,
    #[serde(default)]
    pub nb_fan: Option<u64>,
    #[serde(default)]
    pub nb_album: Option<u32>,
}

/// An album entry from `/artist/{id}/albums`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeezerArtistAlbum {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub cover_medium: Option<String>,
    #[serde(default)]
    pub cover_big: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub record_type: Option<String>,
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
pub(super) struct AlbumTrackItem {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub artist: Option<DeezerTrackArtist>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AlbumTracksEnvelope {
    #[serde(default)]
    pub data: Vec<AlbumTrackItem>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AlbumDetailResponse {
    pub id: Option<u64>,
    pub title: Option<String>,
    #[serde(default)]
    pub cover_medium: Option<String>,
    #[serde(default)]
    pub cover_big: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub record_type: Option<String>,
    #[serde(default)]
    pub nb_tracks: Option<u32>,
    #[serde(default)]
    pub artist: Option<DeezerTrackArtist>,
    #[serde(default)]
    pub tracks: Option<AlbumTracksEnvelope>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ArtistDetailResponse {
    pub id: Option<u64>,
    pub name: Option<String>,
    #[serde(default)]
    pub picture_medium: Option<String>,
    #[serde(default)]
    pub picture_big: Option<String>,
    #[serde(default)]
    pub nb_fan: Option<u64>,
    #[serde(default)]
    pub nb_album: Option<u32>,
    #[serde(default)]
    pub error: Option<ApiError>,
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
