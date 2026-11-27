//! Spotify public playlist resolver using the embed page's `__NEXT_DATA__` payload.

use std::time::Duration;

use reqwest::redirect::Policy;
use serde::Deserialize;
use shared::playlist_import::{ExternalPlaylist, ExternalTrack, PlaylistPlatform};

use super::{PlaylistImportError, MAX_PLAYLIST_TRACKS};

const SPOTIFY_BASE: &str = "https://open.spotify.com";
const SPOTIFY_TRUNCATED_NOTE: &str = "Spotify public previews include up to 100 tracks.";

#[derive(Debug, Deserialize)]
struct NextData {
    props: Option<NextProps>,
}

#[derive(Debug, Deserialize)]
struct NextProps {
    #[serde(rename = "pageProps")]
    page_props: Option<PageProps>,
}

#[derive(Debug, Deserialize)]
struct PageProps {
    state: Option<PageState>,
}

#[derive(Debug, Deserialize)]
struct PageState {
    data: Option<StateData>,
}

#[derive(Debug, Deserialize)]
struct StateData {
    entity: Option<SpotifyEntity>,
}

#[derive(Debug, Deserialize)]
struct SpotifyEntity {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default, rename = "trackList")]
    track_list: Vec<SpotifyEmbedTrack>,
}

#[derive(Debug, Deserialize)]
struct SpotifyEmbedTrack {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    artists: Vec<SpotifyEmbedArtist>,
}

#[derive(Debug, Deserialize)]
struct SpotifyEmbedArtist {
    #[serde(default)]
    name: Option<String>,
}

fn normalize_ws(input: &str) -> String {
    input
        .replace('\u{00a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extracts and parses the `__NEXT_DATA__` JSON block from Spotify's embed HTML page.
pub fn parse_embed_html(
    html: &str,
    source_url: &str,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let json_str = extract_next_data_json(html).ok_or_else(|| {
        PlaylistImportError::Upstream(
            "Could not find playlist data in Spotify embed response.".into(),
        )
    })?;

    let parsed: NextData = serde_json::from_str(json_str).map_err(|e| {
        PlaylistImportError::Upstream(format!("Could not parse Spotify playlist data: {e}"))
    })?;

    let entity = parsed
        .props
        .and_then(|p| p.page_props)
        .and_then(|pp| pp.state)
        .and_then(|s| s.data)
        .and_then(|d| d.entity)
        .ok_or_else(|| {
            PlaylistImportError::NotFound(
                "Spotify playlist was not found or is private.".into(),
            )
        })?;

    let title = entity
        .name
        .map(|n| normalize_ws(&n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Spotify Playlist".to_string());
    let description = entity
        .subtitle
        .map(|s| normalize_ws(&s))
        .filter(|s| !s.is_empty());

    let raw_count = entity.track_list.len();
    let tracks: Vec<ExternalTrack> = entity
        .track_list
        .into_iter()
        .filter_map(|t| {
            let title = t.title.map(|s| normalize_ws(&s)).unwrap_or_default();
            let artist = t
                .subtitle
                .map(|s| normalize_ws(&s))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    t.artists
                        .into_iter()
                        .filter_map(|a| a.name.map(|n| normalize_ws(&n)))
                        .filter(|n| !n.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
            if title.is_empty() || artist.is_empty() {
                return None;
            }
            let duration_s = t
                .duration
                .filter(|&ms| ms > 0)
                .and_then(|ms| u32::try_from(ms / 1000).ok())
                .filter(|&s| s > 0);
            Some(ExternalTrack {
                artist,
                title,
                album: None,
                duration_s,
                recording_mbid: None,
            })
        })
        .take(MAX_PLAYLIST_TRACKS)
        .collect();

    if tracks.is_empty() {
        return Err(PlaylistImportError::EmptyPlaylist);
    }

    let truncated_note = (raw_count >= 100).then(|| SPOTIFY_TRUNCATED_NOTE.to_string());

    Ok(ExternalPlaylist {
        platform: PlaylistPlatform::Spotify,
        source_url: source_url.to_string(),
        title,
        description,
        tracks,
        truncated_note,
    })
}

fn extract_next_data_json(html: &str) -> Option<&str> {
    let marker = "id=\"__NEXT_DATA__\"";
    let id_pos = html.find(marker)?;
    // Walk back to the opening `<script` tag so we know this attribute belongs to a script tag.
    let before = &html[..id_pos];
    let script_start = before.rfind("<script")?;
    let tag_slice = &html[script_start..];
    let content_offset = tag_slice.find('>')? + 1;
    let after_open = &tag_slice[content_offset..];
    let end_pos = after_open.find("</script>")?;
    Some(after_open[..end_pos].trim())
}

/// Fetches a public Spotify playlist by ID via `https://open.spotify.com/embed/playlist/{id}`.
pub async fn fetch_playlist(
    id: &str,
    source_url: &str,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    fetch_playlist_from(SPOTIFY_BASE, id, source_url, true).await
}

pub(crate) async fn fetch_playlist_from(
    base_url: &str,
    id: &str,
    source_url: &str,
    enforce_spotify_host: bool,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let redirect = if enforce_spotify_host {
        Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 {
                return attempt.error("too many redirects");
            }
            let url = attempt.url();
            if url.scheme() == "https" && url.host_str() == Some("open.spotify.com") {
                attempt.follow()
            } else {
                attempt.error("refused redirect outside open.spotify.com")
            }
        })
    } else {
        Policy::limited(3)
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(redirect)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| PlaylistImportError::Upstream(format!("HTTP client error: {e}")))?;

    let url = format!("{}/embed/playlist/{id}", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.map_err(|e| {
        PlaylistImportError::Upstream(format!("Could not reach Spotify: {e}"))
    })?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
        return Err(PlaylistImportError::NotFound(
            "Spotify playlist was not found or is private.".into(),
        ));
    }
    if !status.is_success() {
        return Err(PlaylistImportError::Upstream(format!(
            "Spotify returned HTTP {}",
            status.as_u16()
        )));
    }

    let html = resp.text().await.map_err(|e| {
        PlaylistImportError::Upstream(format!("Could not read Spotify response: {e}"))
    })?;
    parse_embed_html(&html, source_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn sample_embed_html(track_count: usize) -> String {
        let mut tracks = Vec::with_capacity(track_count);
        for i in 0..track_count {
            tracks.push(serde_json::json!({
                "title": format!("Track {}", i + 1),
                "subtitle": "Artist\u{00a0}One, Artist Two",
                "duration": 215_000 + (i as u64 * 1000)
            }));
        }
        let payload = serde_json::json!({
            "props": {
                "pageProps": {
                    "state": {
                        "data": {
                            "entity": {
                                "name": "Late Night Synth",
                                "subtitle": "Curated nocturnal electronics",
                                "trackList": tracks
                            }
                        }
                    }
                }
            }
        });
        format!(
            "<!doctype html><html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{payload}</script></body></html>"
        )
    }

    #[test]
    fn parses_embed_html_and_normalizes_nbsp() {
        let html = sample_embed_html(2);
        let pl =
            parse_embed_html(&html, "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M")
                .unwrap();
        assert_eq!(pl.platform, PlaylistPlatform::Spotify);
        assert_eq!(pl.title, "Late Night Synth");
        assert_eq!(
            pl.description.as_deref(),
            Some("Curated nocturnal electronics")
        );
        assert_eq!(pl.truncated_note, None);
        assert_eq!(pl.tracks.len(), 2);
        assert_eq!(
            pl.tracks[0],
            ExternalTrack {
                artist: "Artist One, Artist Two".into(),
                title: "Track 1".into(),
                album: None,
                duration_s: Some(215),
                recording_mbid: None,
            }
        );
    }

    #[test]
    fn sets_truncated_note_at_100_tracks() {
        let html = sample_embed_html(100);
        let pl =
            parse_embed_html(&html, "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M")
                .unwrap();
        assert_eq!(pl.tracks.len(), 100);
        assert_eq!(
            pl.truncated_note.as_deref(),
            Some("Spotify public previews include up to 100 tracks.")
        );
    }

    #[tokio::test]
    async fn fetches_from_embed_endpoint_and_handles_404() {
        let html = sample_embed_html(3);
        let (base, seen) = serve(vec![
            Reply {
                status: 200,
                body: html,
            },
            Reply {
                status: 404,
                body: "Not Found".into(),
            },
        ]);
        let pl = fetch_playlist_from(
            &base,
            "37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            false,
        )
        .await
        .unwrap();
        assert_eq!(pl.tracks.len(), 3);
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/embed/playlist/37i9dQZF1DXcBWIGoYBM5M"
        );

        let err = fetch_playlist_from(
            &base,
            "37i9dQZF1DXcBWIGoYBM5M",
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PlaylistImportError::NotFound(_)));
    }
}
