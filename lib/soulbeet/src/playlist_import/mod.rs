//! Pure-Rust URL-only playlist importer for Spotify, YouTube Music, Deezer, and ListenBrainz.
//!
//! Enforces a strict HTTPS host allowlist to prevent SSRF and caps imported playlists at
//! `MAX_PLAYLIST_TRACKS` tracks.

pub mod deezer;
pub mod listenbrainz;
pub mod spotify;
pub mod youtube_music;

use shared::playlist_import::ExternalPlaylist;
use thiserror::Error;
use url::Url;

/// Maximum number of tracks resolved from a single external playlist URL.
pub const MAX_PLAYLIST_TRACKS: usize = 500;

/// Default hard cap on response body bytes read by playlist resolvers (8 MiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Builds a redirect policy restricted to `https://{allowed_host}` on the default HTTPS port.
pub(crate) fn pinned_https_redirect(allowed_host: &'static str) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 3 {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        if url.scheme() == "https"
            && url.host_str() == Some(allowed_host)
            && url.port().is_none()
        {
            attempt.follow()
        } else {
            attempt.error(format!("refused redirect outside {allowed_host}"))
        }
    })
}

/// Reads a `reqwest::Response` body up to `max_bytes`, rejecting oversized payloads both upfront
/// via `Content-Length` and incrementally while streaming chunks.
pub(crate) async fn read_bounded_bytes(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, PlaylistImportError> {
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            return Err(PlaylistImportError::Upstream(format!(
                "Playlist response exceeded maximum allowed size of {max_bytes} bytes."
            )));
        }
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        PlaylistImportError::Upstream(format!("Could not read playlist response: {e}"))
    })? {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(PlaylistImportError::Upstream(format!(
                "Playlist response exceeded maximum allowed size of {max_bytes} bytes."
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Errors returned while validating or fetching an external playlist URL.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlaylistImportError {
    #[error("{0}")]
    InvalidUrl(String),
    #[error("{0}")]
    NotFound(String),
    #[error("This playlist contains no importable tracks.")]
    EmptyPlaylist,
    #[error("{0}")]
    Upstream(String),
}

/// Validated playlist reference extracted from a user-supplied URL or URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPlaylistUrl {
    Spotify { id: String, source_url: String },
    YouTubeMusic { list_id: String, source_url: String },
    Deezer { id: String, source_url: String },
    ListenBrainz { mbid: String, source_url: String },
}

const SUPPORTED_HINT: &str =
    "Paste a Spotify, YouTube Music, Deezer, or ListenBrainz playlist link.";

/// Validates and parses an external playlist URL or `spotify:playlist:{id}` URI.
///
/// Only HTTPS URLs on explicitly allowlisted hosts (without embedded credentials or
/// custom ports) are accepted.
pub fn parse_playlist_url(raw: &str) -> Result<ParsedPlaylistUrl, PlaylistImportError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PlaylistImportError::InvalidUrl(format!(
            "Please enter a playlist URL. {SUPPORTED_HINT}"
        )));
    }

    if let Some(rest) = trimmed.strip_prefix("spotify:playlist:") {
        let id = rest.trim();
        if is_valid_spotify_id(id) {
            return Ok(ParsedPlaylistUrl::Spotify {
                id: id.to_string(),
                source_url: format!("https://open.spotify.com/playlist/{id}"),
            });
        }
        return Err(PlaylistImportError::InvalidUrl(
            "Invalid Spotify playlist URI.".into(),
        ));
    }

    let url = Url::parse(trimmed).map_err(|_| {
        PlaylistImportError::InvalidUrl(format!("That does not look like a valid URL. {SUPPORTED_HINT}"))
    })?;

    if url.scheme() != "https" {
        return Err(PlaylistImportError::InvalidUrl(format!(
            "Only https:// playlist links are supported. {SUPPORTED_HINT}"
        )));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(PlaylistImportError::InvalidUrl(
            "Playlist URLs must not contain credentials.".into(),
        ));
    }

    if url.port().is_some() {
        return Err(PlaylistImportError::InvalidUrl(
            "Playlist URLs with custom ports are not allowed.".into(),
        ));
    }

    let host = url
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| PlaylistImportError::InvalidUrl(SUPPORTED_HINT.into()))?;

    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();

    match host.as_str() {
        "open.spotify.com" => parse_spotify_path(&segments),
        "music.youtube.com" | "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
            parse_youtube_query(&url, &host)
        }
        "deezer.com" | "www.deezer.com" => parse_deezer_path(&segments),
        "listenbrainz.org" => parse_listenbrainz_path(&segments),
        _ => Err(PlaylistImportError::InvalidUrl(format!(
            "Unsupported playlist host \"{host}\". {SUPPORTED_HINT}"
        ))),
    }
}

fn is_valid_spotify_id(id: &str) -> bool {
    (10..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn parse_spotify_path(segments: &[&str]) -> Result<ParsedPlaylistUrl, PlaylistImportError> {
    // Handles `/playlist/{id}`, `/embed/playlist/{id}`, and `/intl-fr/playlist/{id}`.
    let id = match segments {
        ["playlist", id] => Some(*id),
        ["embed", "playlist", id] => Some(*id),
        [locale, "playlist", id] if locale.starts_with("intl-") => Some(*id),
        [locale, "embed", "playlist", id] if locale.starts_with("intl-") => Some(*id),
        _ => None,
    };
    match id {
        Some(id) if is_valid_spotify_id(id) => Ok(ParsedPlaylistUrl::Spotify {
            id: id.to_string(),
            source_url: format!("https://open.spotify.com/playlist/{id}"),
        }),
        _ => Err(PlaylistImportError::InvalidUrl(
            "Expected a Spotify playlist URL like https://open.spotify.com/playlist/{id}.".into(),
        )),
    }
}

fn is_valid_youtube_list_id(id: &str) -> bool {
    (8..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_youtube_query(url: &Url, host: &str) -> Result<ParsedPlaylistUrl, PlaylistImportError> {
    let list_id = url
        .query_pairs()
        .find(|(k, _)| k == "list")
        .map(|(_, v)| v.into_owned());
    match list_id {
        Some(id) if is_valid_youtube_list_id(&id) => {
            let canonical_host = if host == "music.youtube.com" {
                "music.youtube.com"
            } else {
                "www.youtube.com"
            };
            Ok(ParsedPlaylistUrl::YouTubeMusic {
                source_url: format!("https://{canonical_host}/playlist?list={id}"),
                list_id: id,
            })
        }
        _ => Err(PlaylistImportError::InvalidUrl(
            "Expected a YouTube or YouTube Music playlist URL containing a valid ?list= parameter."
                .into(),
        )),
    }
}

fn is_locale_segment(seg: &str) -> bool {
    // Matches short Deezer locale prefixes such as `en`, `fr`, `us`, `en-us`, `pt-br`.
    let bytes = seg.as_bytes();
    if bytes.len() == 2 {
        return bytes.iter().all(|b| b.is_ascii_alphabetic());
    }
    if bytes.len() == 5 && bytes[2] == b'-' {
        return bytes[0].is_ascii_alphabetic()
            && bytes[1].is_ascii_alphabetic()
            && bytes[3].is_ascii_alphabetic()
            && bytes[4].is_ascii_alphabetic();
    }
    false
}

fn parse_deezer_path(segments: &[&str]) -> Result<ParsedPlaylistUrl, PlaylistImportError> {
    let id = match segments {
        ["playlist", id] => Some(*id),
        [locale, "playlist", id] if is_locale_segment(locale) => Some(*id),
        _ => None,
    };
    match id {
        Some(id) if !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(ParsedPlaylistUrl::Deezer {
                id: id.to_string(),
                source_url: format!("https://www.deezer.com/playlist/{id}"),
            })
        }
        _ => Err(PlaylistImportError::InvalidUrl(
            "Expected a Deezer playlist URL like https://www.deezer.com/playlist/{id}.".into(),
        )),
    }
}

fn parse_listenbrainz_path(segments: &[&str]) -> Result<ParsedPlaylistUrl, PlaylistImportError> {
    let mbid = match segments {
        ["playlist", mbid] => Some(*mbid),
        _ => None,
    };
    match mbid.and_then(|raw| uuid::Uuid::try_parse(raw).ok()) {
        Some(uuid) => {
            let mbid = uuid.hyphenated().to_string();
            Ok(ParsedPlaylistUrl::ListenBrainz {
                source_url: format!("https://listenbrainz.org/playlist/{mbid}"),
                mbid,
            })
        }
        None => Err(PlaylistImportError::InvalidUrl(
            "Expected a ListenBrainz playlist URL like https://listenbrainz.org/playlist/{mbid}."
                .into(),
        )),
    }
}

/// Validates and resolves an external playlist URL into normalized track metadata.
pub async fn resolve_playlist_url(raw: &str) -> Result<ExternalPlaylist, PlaylistImportError> {
    match parse_playlist_url(raw)? {
        ParsedPlaylistUrl::Spotify { id, source_url } => {
            spotify::fetch_playlist(&id, &source_url).await
        }
        ParsedPlaylistUrl::YouTubeMusic {
            list_id,
            source_url,
        } => youtube_music::fetch_playlist(&list_id, &source_url).await,
        ParsedPlaylistUrl::Deezer { id, source_url } => {
            deezer::fetch_playlist(&id, &source_url).await
        }
        ParsedPlaylistUrl::ListenBrainz { mbid, source_url } => {
            listenbrainz::fetch_playlist(&mbid, &source_url).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spotify_urls_and_uris_with_locales_and_query_params() {
        let parsed = parse_playlist_url(
            "https://open.spotify.com/intl-fr/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abcdef12345",
        )
        .unwrap();
        assert_eq!(
            parsed,
            ParsedPlaylistUrl::Spotify {
                id: "37i9dQZF1DXcBWIGoYBM5M".into(),
                source_url: "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M".into(),
            }
        );

        let uri = parse_playlist_url("  spotify:playlist:37i9dQZF1DXcBWIGoYBM5M  ").unwrap();
        assert_eq!(uri, parsed);

        let embed =
            parse_playlist_url("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M")
                .unwrap();
        assert_eq!(embed, parsed);
    }

    #[test]
    fn parses_youtube_and_youtube_music_playlists() {
        for raw in [
            "https://music.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            "https://www.youtube.com/playlist?list=RDCLAK5uy_kmPRjHDECIcuVwnKsx2Ng7fyNgFKWNJFs&si=xyz",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&list=OLAK5uy_n0123456789abcdef",
            "https://m.youtube.com/playlist?list=PL1234567890",
        ] {
            let parsed = parse_playlist_url(raw).unwrap();
            assert!(matches!(parsed, ParsedPlaylistUrl::YouTubeMusic { .. }), "{raw}");
        }
    }

    #[test]
    fn parses_deezer_urls_with_optional_locale() {
        let a = parse_playlist_url("https://www.deezer.com/us/playlist/908622995?utm_source=x")
            .unwrap();
        let b = parse_playlist_url("https://deezer.com/playlist/908622995").unwrap();
        let c = parse_playlist_url("https://www.deezer.com/en-us/playlist/908622995/").unwrap();
        let expected = ParsedPlaylistUrl::Deezer {
            id: "908622995".into(),
            source_url: "https://www.deezer.com/playlist/908622995".into(),
        };
        assert_eq!(a, expected);
        assert_eq!(b, expected);
        assert_eq!(c, expected);
    }

    #[test]
    fn parses_listenbrainz_urls_and_normalizes_uuid() {
        let mbid = "48f2eb97-33e7-496c-8da1-25ead7280a81";
        let upper = mbid.to_uppercase();
        let parsed =
            parse_playlist_url(&format!("https://listenbrainz.org/playlist/{upper}/")).unwrap();
        assert_eq!(
            parsed,
            ParsedPlaylistUrl::ListenBrainz {
                mbid: mbid.into(),
                source_url: format!("https://listenbrainz.org/playlist/{mbid}"),
            }
        );
    }

    #[test]
    fn rejects_ssrf_hosts_non_https_credentials_ports_and_invalid_paths() {
        let rejected = [
            "",
            "   ",
            "http://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "file:///etc/passwd",
            "https://127.0.0.1/playlist/1234567890",
            "https://localhost/playlist/1234567890",
            "https://169.254.169.254/latest/meta-data/",
            "https://evil.open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com.evil.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "https://user:pass@open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com:8443/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com/album/37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com/playlist/short",
            "spotify:playlist:../etc",
            "https://music.youtube.com/watch?v=abc",
            "https://music.youtube.com/playlist?list=short",
            "https://www.deezer.com/playlist/notanumber",
            "https://listenbrainz.org/playlist/not-a-uuid",
            "https://example.com/playlist/12345",
        ];
        for raw in rejected {
            assert!(
                matches!(parse_playlist_url(raw), Err(PlaylistImportError::InvalidUrl(_))),
                "expected InvalidUrl for {raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn read_bounded_bytes_rejects_oversized_responses() {
        use crate::test_http::{serve, Reply};

        let oversized = "a".repeat(256);
        let (base, _) = serve(vec![
            Reply {
                status: 200,
                body: oversized,
            },
            Reply {
                status: 200,
                body: "ok".into(),
            },
        ]);
        let client = reqwest::Client::new();

        let resp = client.get(format!("{base}/big")).send().await.unwrap();
        let err = read_bounded_bytes(resp, 64).await.unwrap_err();
        assert!(
            matches!(err, PlaylistImportError::Upstream(ref m) if m.contains("maximum allowed size")),
            "{err:?}"
        );

        let resp_ok = client.get(format!("{base}/small")).send().await.unwrap();
        let bytes = read_bounded_bytes(resp_ok, 64).await.unwrap();
        assert_eq!(bytes, b"ok");
    }
}
