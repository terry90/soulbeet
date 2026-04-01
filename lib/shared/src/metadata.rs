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

/// Compare two MusicBrainz-style date strings, which can be in the format "YYYY", "YYYY-MM", or "YYYY-MM-DD".
///
/// The comparison logic is as follows: \
/// First compare the year, then specificity, then full string for tie-breaking.
/// This ensures that more specific dates (e.g. "1980-05") are sorted before less specific ones ("1980"),
/// but when years are different, they are sorted chronologically regardless of specificity
/// i.e. "1980-05" will come before "1980", but both will come after "1979-12".
///
/// This is a design choice, based on the intuition that a more specific date probably means more popular/definitive release.
pub fn compare_musicbrainz_dates(
    date1: &Option<impl AsRef<str>>,
    date2: &Option<impl AsRef<str>>,
) -> std::cmp::Ordering {
    let date_a = date1.as_ref().map(|d| d.as_ref()).filter(|s| !s.is_empty());
    let date_b = date2.as_ref().map(|d| d.as_ref()).filter(|s| !s.is_empty());

    match (date_a, date_b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            let year_a = &a[..a.len().min(4)];
            let year_b = &b[..b.len().min(4)];

            year_a.cmp(year_b) // 1. compare year
                .then_with(|| b.len().cmp(&a.len())) // 2. more specific first
                .then_with(|| a.cmp(b)) // 3. full chronological order
        }
    }
}
