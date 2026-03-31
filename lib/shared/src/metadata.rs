use serde::{Deserialize, Serialize};

/// Metadata provider identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    MusicBrainz,
    LastFm,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::MusicBrainz => write!(f, "musicbrainz"),
            Provider::LastFm => write!(f, "lastfm"),
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "musicbrainz" => Ok(Provider::MusicBrainz),
            "lastfm" => Ok(Provider::LastFm),
            _ => Err(format!("Unknown provider: {}", s)),
        }
    }
}

/// Wrapper for search results that includes provider information.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResults {
    pub provider: Provider,
    pub results: Vec<SearchResult>,
}

/// Represents a search result which can be either a track or an album.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SearchResult {
    Track(Track),
    Album(Album),
    AlbumGroup(AlbumGroup),
}

/// A track from a metadata provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Provider-specific identifier for lookups.
    pub id: String,
    /// The title of the track.
    pub title: String,
    /// A formatted string of the artist(s).
    pub artist: String,
    /// Provider-specific identifier of the album the track belongs to.
    pub album_id: Option<String>,
    /// The title of the album the track belongs to.
    pub album_title: Option<String>,
    /// The release date of the album (YYYY-MM-DD).
    pub release_date: Option<String>,
    /// The duration of the track in a formatted MM:SS string.
    pub duration: Option<String>,
    /// The MusicBrainz recording ID, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    /// The MusicBrainz release ID for the album, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_mbid: Option<String>,
}

/// An album from a metadata provider.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Album {
    /// Provider-specific identifier for lookups.
    pub id: String,
    /// The title of the album.
    pub title: String,
    /// A formatted string of the artist(s).
    pub artist: String,
    /// The release date of the album (YYYY-MM-DD).
    pub release_date: Option<String>,
    /// The MusicBrainz release ID, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    /// URL to the album cover image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

/// This represents a general album entity, containing specific versions.
/// Equivalent to a MusicBrainz release group.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct AlbumGroup {
    /// Provider-specific identifier for lookups.
    pub id: String,
    /// The title of the album.
    pub title: String,
    /// A formatted string of the artist(s).
    pub artist: String,
    /// The release date of the album (YYYY-MM-DD).
    pub release_date: Option<String>,
    /// The MusicBrainz release ID, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    /// URL to the album cover image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// List of known editions of the album (e.g. deluxe, remastered, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editions: Vec<Album>,
}

/// An album with its full track listing.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct AlbumWithTracks {
    pub album: Album,
    pub tracks: Vec<Track>,
}
