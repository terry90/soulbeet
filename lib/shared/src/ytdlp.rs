//! The YouTube Music download backend's own data, as its search hands it to `download`.

use serde::{Deserialize, Serialize};

/// The id YouTube Music goes by as a download backend, in the registry and on every row and
/// search.
pub const BACKEND_ID: &str = "ytdlp";

/// One YouTube Music track a search matched, kept in `DownloadableItem::backend_data`: the
/// video `download` fetches and what it tags the file as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtTrack {
    pub video_id: String,
    /// The album's browse id; None for a song YouTube Music lists on no album.
    pub browse_id: Option<String>,
    /// The track the search was for, which the file is tagged as.
    pub wanted: WantedTrack,
    /// How well YouTube Music's title named the wanted one.
    pub title_score: f64,
    /// How well YouTube Music's album named the wanted one; 0 when the search named no album.
    pub album_score: f64,
}

/// A wanted track, as MusicBrainz names it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WantedTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    /// YYYY-MM-DD, or as much of it as is known.
    pub date: Option<String>,
    pub duration_s: Option<u32>,
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
}
