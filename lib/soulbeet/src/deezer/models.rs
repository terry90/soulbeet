use serde::{Deserialize, Serialize};

/// A track from an artist radio, in radio order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeezerTrack {
    pub artist: String,
    pub title: String,
}

/// Deezer answers errors with HTTP 200 and an `error` object.
#[derive(Debug, Deserialize)]
pub(super) struct Envelope<T> {
    pub data: Option<Vec<T>>,
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
