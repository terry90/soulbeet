use serde::{Deserialize, Serialize};

/// Represents a search result which can be either a track or an album.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SearchResult {
    Track(Track),
    Album(Album),
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

impl Track {
    /// `duration` in seconds. MusicBrainz is written `{minutes}:{SS}`, with any
    /// number of minute digits and two second digits; anything else is `None`.
    /// A zero length is `None` too: no recording lasts zero seconds, so it
    /// means unknown.
    pub fn duration_secs(&self) -> Option<u32> {
        let (minutes, seconds) = self.duration.as_deref()?.split_once(':')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(minutes) || seconds.len() != 2 || !digits(seconds) {
            return None;
        }
        let secs = minutes
            .parse::<u32>()
            .ok()?
            .checked_mul(60)?
            .checked_add(seconds.parse::<u32>().ok()?)?;
        (secs > 0).then_some(secs)
    }
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
    /// The release group's primary type, then its secondary types, as
    /// MusicBrainz names them; None when the provider gives no release types,
    /// when MusicBrainz gives a type musicbrainz_rs does not know, or when
    /// the release group has no type set (D15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_types: Option<Vec<String>>,
    /// The release group's first release date (YYYY, YYYY-MM or YYYY-MM-DD):
    /// when any edition first came out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_release_date: Option<String>,
}

/// An artist a discography is for, or one that shares its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistSummary {
    pub mbid: String,
    pub name: String,
    pub disambiguation: Option<String>,
}

/// An artist's albums, EPs and singles, as an artist-only search lists them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discography {
    pub artist: ArtistSummary,
    /// The other artists carrying the name, to pick instead.
    pub others: Vec<ArtistSummary>,
    /// In discography order: the albums and EPs, then the singles.
    pub albums: Vec<Album>,
    /// Every album and EP MusicBrainz lists for the artist is in `albums`.
    pub albums_complete: bool,
    /// Every single MusicBrainz lists for the artist is in `albums`.
    pub singles_complete: bool,
}

impl Discography {
    /// Every album, EP and single MusicBrainz lists for the artist is in
    /// `albums`.
    pub fn complete(&self) -> bool {
        self.albums_complete && self.singles_complete
    }
}

/// Whether a search's title text asks for no title: empty, or "*", which
/// once matched any title.
pub fn asks_no_title(query: &str) -> bool {
    matches!(query.trim(), "" | "*")
}

/// An album with its full track listing.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct AlbumWithTracks {
    pub album: Album,
    pub tracks: Vec<Track>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> Track {
        Track {
            id: "t1".into(),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album_id: None,
            album_title: None,
            release_date: None,
            duration: None,
            mbid: None,
            release_mbid: None,
        }
    }

    #[test]
    fn track_durations_read_as_seconds() {
        let with = |d: Option<&str>| Track {
            duration: d.map(str::to_string),
            ..sample_track()
        };
        assert_eq!(with(Some("04:23")).duration_secs(), Some(263));
        assert_eq!(with(Some("75:03")).duration_secs(), Some(4503));
        assert_eq!(with(Some("4:7")).duration_secs(), None);
        assert_eq!(with(Some("00:00")).duration_secs(), None);
        assert_eq!(with(Some("71582789:00")).duration_secs(), None);
        assert_eq!(with(Some("")).duration_secs(), None);
        assert_eq!(with(None).duration_secs(), None);
    }

    #[test]
    fn a_title_of_nothing_or_a_star_asks_for_no_title() {
        for query in ["", "  ", "*", " * "] {
            assert!(asks_no_title(query), "{query:?}");
        }
        for query in ["Vena Sera", "**", "*Road"] {
            assert!(!asks_no_title(query), "{query:?}");
        }
    }
}
