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

/// Which remote catalog provider served a unified search or detail lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogProvider {
    Deezer,
    Musicbrainz,
}

impl CatalogProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deezer => "deezer",
            Self::Musicbrainz => "musicbrainz",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Deezer => "Deezer",
            Self::Musicbrainz => "MusicBrainz",
        }
    }
}

/// A remote catalog track returned by unified search, album detail, or artist top tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_mbid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_label: Option<String>,
    pub provider: CatalogProvider,
}

impl CatalogTrack {
    pub fn to_track(&self) -> Track {
        Track {
            id: self.id.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album_id: self.album_id.clone(),
            album_title: self.album_title.clone(),
            release_date: self.release_date.clone(),
            duration: self.duration.clone().or_else(|| {
                self.duration_secs.map(|s| format!("{:02}:{:02}", s / 60, s % 60))
            }),
            mbid: self.mbid.clone(),
            release_mbid: self.release_mbid.clone(),
        }
    }

    pub fn from_mb_track(track: Track, variant_label: Option<String>) -> Self {
        let duration_secs = track.duration_secs();
        let cover_url = track.release_mbid.as_ref().map(|rmbid| {
            format!("https://coverartarchive.org/release/{rmbid}/front-250")
        });
        Self {
            id: track.id,
            title: track.title,
            artist: track.artist,
            artist_id: None,
            album_id: track.album_id,
            album_title: track.album_title,
            release_date: track.release_date,
            duration: track.duration,
            duration_secs,
            mbid: track.mbid,
            release_mbid: track.release_mbid,
            cover_url,
            variant_label,
            provider: CatalogProvider::Musicbrainz,
        }
    }
}

/// A remote catalog album returned by unified search or an artist's discography.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
    pub provider: CatalogProvider,
}

impl CatalogAlbum {
    pub fn to_album(&self) -> Album {
        Album {
            id: self.id.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            release_date: self.release_date.clone(),
            mbid: self.mbid.clone(),
            cover_url: self.cover_url.clone(),
            release_types: self.release_types.clone(),
            first_release_date: self.first_release_date.clone(),
        }
    }

    pub fn from_mb_album(album: Album) -> Self {
        let cover_url = album.cover_url.or_else(|| {
            album
                .mbid
                .as_ref()
                .map(|m| format!("https://coverartarchive.org/release/{m}/front-250"))
        });
        Self {
            id: album.id,
            title: album.title,
            artist: album.artist,
            artist_id: None,
            release_date: album.release_date,
            first_release_date: album.first_release_date,
            cover_url,
            mbid: album.mbid,
            release_types: album.release_types,
            track_count: None,
            provider: CatalogProvider::Musicbrainz,
        }
    }
}

/// A remote catalog artist returned by unified search or disambiguation lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogArtist {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disambiguation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    pub provider: CatalogProvider,
}

/// Response for `POST /api/metadata/search/unified`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedCatalogSearch {
    pub query: String,
    pub provider: CatalogProvider,
    /// True when the primary provider returned nothing or failed and MusicBrainz served the result.
    #[serde(default)]
    pub fallback_used: bool,
    #[serde(default)]
    pub tracks: Vec<CatalogTrack>,
    #[serde(default)]
    pub albums: Vec<CatalogAlbum>,
    #[serde(default)]
    pub artists: Vec<CatalogArtist>,
}

/// Full remote album detail (album metadata + ordered tracklist).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogAlbumDetail {
    pub album: CatalogAlbum,
    #[serde(default)]
    pub tracks: Vec<CatalogTrack>,
}

/// Full remote artist detail (artist info + top tracks + discography).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistCatalog {
    pub artist: CatalogArtist,
    pub provider: CatalogProvider,
    #[serde(default)]
    pub others: Vec<CatalogArtist>,
    #[serde(default)]
    pub top_tracks: Vec<CatalogTrack>,
    #[serde(default)]
    pub albums: Vec<CatalogAlbum>,
    #[serde(default)]
    pub albums_complete: bool,
    #[serde(default)]
    pub singles_complete: bool,
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
