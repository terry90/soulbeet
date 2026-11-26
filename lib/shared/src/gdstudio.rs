//! The GD Studio download backend's own data, as its search hands it to `download`.

use serde::{Deserialize, Serialize};

/// The id GD Studio goes by as a download backend, in the registry and on every row and search.
pub const BACKEND_ID: &str = "gdstudio";

/// The one source every GD Studio file comes from: NetEase, through GD Studio's API.
pub const SOURCE: &str = "NetEase";

/// One NetEase track as a search resolved it, kept in `DownloadableItem::backend_data`: what
/// `download` asks for again and checks the bytes against. It holds no file URL, which expires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GdTrack {
    pub track_id: String,
    /// "flac" or "mp3".
    pub ext: String,
    /// Bytes.
    pub size: u64,
    /// Lowercase hex, the MD5 the file is named by on NetEase's servers.
    pub md5: String,
    /// What the file is tagged with before it lands: NetEase's files carry no tags.
    pub tags: GdTags,
}

/// The wanted track's tags.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GdTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub date: Option<String>,
    /// The wanted recording and release, with how well the file matched them; they are
    /// written only on a close match.
    pub musicbrainz: Option<GdMusicBrainz>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GdMusicBrainz {
    pub recording: String,
    pub release: String,
    pub title_score: f64,
    pub album_score: f64,
    pub length_diff_s: Option<u32>,
}
