//! YouTube Music / YouTube playlist resolver via YouTube Music's `WEB_REMIX` InnerTube browse API.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::redirect::Policy;
use serde_json::{json, Value};
use shared::playlist_import::{ExternalPlaylist, ExternalTrack, PlaylistPlatform};

use super::{PlaylistImportError, MAX_PLAYLIST_TRACKS};

const YTM_BASE: &str = "https://music.youtube.com";
const MAX_CONTINUATION_PAGES: usize = 15;
const TRUNCATED_500_NOTE: &str = "Showing the first 500 tracks of this playlist.";

/// Ensures the browse ID starts with `VL` for playlist browsing.
pub fn normalize_browse_id(list_id: &str) -> String {
    if list_id.starts_with("VL") {
        list_id.to_string()
    } else {
        format!("VL{list_id}")
    }
}

/// Parses `MM:SS` or `HH:MM:SS` duration labels into seconds.
pub fn parse_duration_label(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let parts: Vec<&str> = trimmed.split(':').collect();
    match parts.as_slice() {
        [m, s] => {
            let mins: u32 = m.parse().ok()?;
            let secs: u32 = s.parse().ok()?;
            if secs >= 60 {
                return None;
            }
            mins.checked_mul(60)?.checked_add(secs).filter(|&d| d > 0)
        }
        [h, m, s] => {
            let hours: u32 = h.parse().ok()?;
            let mins: u32 = m.parse().ok()?;
            let secs: u32 = s.parse().ok()?;
            if mins >= 60 || secs >= 60 {
                return None;
            }
            hours
                .checked_mul(3600)?
                .checked_add(mins.checked_mul(60)?)?
                .checked_add(secs)
                .filter(|&d| d > 0)
        }
        _ => None,
    }
}

fn runs_text(val: &Value) -> Option<String> {
    let runs = val.get("runs")?.as_array()?;
    let joined: String = runs
        .iter()
        .filter_map(|r| r.get("text").and_then(Value::as_str))
        .collect();
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn simple_or_runs_text(val: &Value) -> Option<String> {
    if let Some(s) = val.get("simpleText").and_then(Value::as_str) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    runs_text(val)
}

/// Extracts the playlist title and optional description from any of YouTube Music's
/// header renderer shapes.
fn extract_header(root: &Value) -> (Option<String>, Option<String>) {
    let mut title = None;
    let mut description = None;
    walk_value(root, &mut |obj| {
        if title.is_some() {
            return;
        }
        for key in [
            "musicResponsiveHeaderRenderer",
            "musicDetailHeaderRenderer",
            "musicEditablePlaylistDetailHeaderRenderer",
        ] {
            if let Some(hdr) = obj.get(key) {
                let inner = hdr
                    .get("header")
                    .and_then(|h| {
                        h.get("musicDetailHeaderRenderer")
                            .or_else(|| h.get("musicResponsiveHeaderRenderer"))
                    })
                    .unwrap_or(hdr);
                if let Some(t) = inner.get("title").and_then(simple_or_runs_text) {
                    title = Some(t);
                    description = inner
                        .get("description")
                        .and_then(|d| {
                            d.get("musicDescriptionShelfRenderer")
                                .and_then(|s| s.get("description"))
                                .or(Some(d))
                        })
                        .and_then(simple_or_runs_text);
                    return;
                }
            }
        }
    });
    (title, description)
}

fn is_bullet_separator(text: &str) -> bool {
    let t = text.trim();
    t == "•" || t == "·" || t == "\u{2022}"
}

fn looks_like_metadata_noise(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    if parse_duration_label(&lower).is_some() {
        return true;
    }
    lower.ends_with(" plays")
        || lower.ends_with(" play")
        || lower.ends_with(" views")
        || lower.ends_with(" view")
        || (lower.len() == 4 && lower.bytes().all(|b| b.is_ascii_digit()))
}

fn extract_artists_from_column(col: &Value) -> String {
    let Some(runs) = col
        .get("musicResponsiveListItemFlexColumnRenderer")
        .and_then(|r| r.get("text"))
        .and_then(|t| t.get("runs"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };

    // Split runs at bullet separators; take the first segment (which contains artist names,
    // separating them from trailing album/play-count/duration metadata if present).
    let mut parts: Vec<&str> = Vec::new();
    for run in runs {
        let Some(text) = run.get("text").and_then(Value::as_str) else {
            continue;
        };
        if is_bullet_separator(text) {
            if !parts.is_empty() {
                break;
            }
            continue;
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Keep joiner punctuation like ", " and " & " when already collecting artists.
        if parts.is_empty() && looks_like_metadata_noise(trimmed) {
            continue;
        }
        parts.push(text);
    }

    parts.concat().trim().to_string()
}

fn extract_column_text(col: &Value) -> Option<String> {
    col.get("musicResponsiveListItemFlexColumnRenderer")
        .and_then(|r| r.get("text"))
        .and_then(simple_or_runs_text)
}

fn extract_duration_from_item(item: &Value) -> Option<u32> {
    if let Some(fixed) = item.get("fixedColumns").and_then(Value::as_array) {
        for col in fixed {
            if let Some(text) = col
                .get("musicResponsiveListItemFixedColumnRenderer")
                .and_then(|r| r.get("text"))
                .and_then(simple_or_runs_text)
            {
                if let Some(secs) = parse_duration_label(&text) {
                    return Some(secs);
                }
            }
        }
    }
    // Some playlist shapes place duration as a trailing run inside flexColumns[1].
    if let Some(runs) = item
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|cols| cols.get(1))
        .and_then(|c| c.get("musicResponsiveListItemFlexColumnRenderer"))
        .and_then(|r| r.get("text"))
        .and_then(|t| t.get("runs"))
        .and_then(Value::as_array)
    {
        for run in runs.iter().rev() {
            if let Some(text) = run.get("text").and_then(Value::as_str) {
                if let Some(secs) = parse_duration_label(text) {
                    return Some(secs);
                }
            }
        }
    }
    None
}

fn parse_track_item(item: &Value) -> Option<ExternalTrack> {
    let cols = item.get("flexColumns")?.as_array()?;
    let title = cols.first().and_then(extract_column_text)?;
    let artist = cols
        .get(1)
        .map(extract_artists_from_column)
        .unwrap_or_default();
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    let album = cols
        .get(2)
        .and_then(extract_column_text)
        .filter(|a| !looks_like_metadata_noise(a));
    let duration_s = extract_duration_from_item(item);
    Some(ExternalTrack {
        artist,
        title,
        album,
        duration_s,
        recording_mbid: None,
    })
}

/// Extracts tracks and the next continuation token (if any) from a browse response page.
pub fn parse_browse_page(page: &Value) -> (Vec<ExternalTrack>, Option<String>) {
    let mut tracks = Vec::new();
    let mut continuation: Option<String> = None;

    walk_value(page, &mut |obj| {
        if let Some(item) = obj.get("musicResponsiveListItemRenderer") {
            if let Some(track) = parse_track_item(item) {
                tracks.push(track);
            }
        }
        if continuation.is_none() {
            if let Some(token) = obj
                .get("continuationCommand")
                .and_then(|c| c.get("token"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                continuation = Some(token.to_string());
            } else if let Some(token) = obj
                .get("nextContinuationData")
                .and_then(|c| c.get("continuation"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                continuation = Some(token.to_string());
            }
        }
    });

    (tracks, continuation)
}

fn walk_value<F: FnMut(&serde_json::Map<String, Value>)>(val: &Value, visitor: &mut F) {
    match val {
        Value::Object(map) => {
            visitor(map);
            for child in map.values() {
                walk_value(child, visitor);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk_value(child, visitor);
            }
        }
        _ => {}
    }
}

/// Fetches a YouTube Music or YouTube playlist via `https://music.youtube.com/youtubei/v1/browse`.
pub async fn fetch_playlist(
    list_id: &str,
    source_url: &str,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    fetch_playlist_from(YTM_BASE, list_id, source_url, true).await
}

pub(crate) async fn fetch_playlist_from(
    base_url: &str,
    list_id: &str,
    source_url: &str,
    enforce_ytm_host: bool,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let redirect = if enforce_ytm_host {
        Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 {
                return attempt.error("too many redirects");
            }
            let url = attempt.url();
            if url.scheme() == "https" && url.host_str() == Some("music.youtube.com") {
                attempt.follow()
            } else {
                attempt.error("refused redirect outside music.youtube.com")
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

    let endpoint = format!(
        "{}/youtubei/v1/browse?prettyPrint=false",
        base_url.trim_end_matches('/')
    );
    let client_ctx = json!({
        "client": {
            "clientName": "WEB_REMIX",
            "clientVersion": "1.20250101.01.00",
            "hl": "en",
            "gl": "US"
        }
    });

    let first_body = json!({
        "context": client_ctx,
        "browseId": normalize_browse_id(list_id),
    });

    let first_page = post_browse(&client, &endpoint, &first_body).await?;
    if let Some(err) = first_page.get("error") {
        let code = err.get("code").and_then(Value::as_u64).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Playlist not found");
        if code == 404 || code == 403 || code == 400 {
            return Err(PlaylistImportError::NotFound(format!(
                "YouTube Music playlist was not found or is private ({msg})."
            )));
        }
        return Err(PlaylistImportError::Upstream(format!(
            "YouTube Music error: {msg}"
        )));
    }

    let (title, description) = extract_header(&first_page);
    let (mut tracks, mut next_token) = parse_browse_page(&first_page);

    let mut seen_tokens = HashSet::new();
    let mut pages = 0usize;
    let mut truncated = false;

    while let Some(token) = next_token.take() {
        if tracks.len() >= MAX_PLAYLIST_TRACKS {
            truncated = true;
            break;
        }
        if pages >= MAX_CONTINUATION_PAGES || !seen_tokens.insert(token.clone()) {
            break;
        }
        let body = json!({
            "context": client_ctx,
            "continuation": token,
        });
        let page = post_browse(&client, &endpoint, &body).await?;
        let (page_tracks, token_after) = parse_browse_page(&page);
        if page_tracks.is_empty() {
            break;
        }
        tracks.extend(page_tracks);
        next_token = token_after;
        pages += 1;
    }

    if tracks.len() > MAX_PLAYLIST_TRACKS {
        tracks.truncate(MAX_PLAYLIST_TRACKS);
        truncated = true;
    }

    if tracks.is_empty() {
        return Err(PlaylistImportError::EmptyPlaylist);
    }

    Ok(ExternalPlaylist {
        platform: PlaylistPlatform::YouTubeMusic,
        source_url: source_url.to_string(),
        title: title.unwrap_or_else(|| "YouTube Music Playlist".to_string()),
        description,
        tracks,
        truncated_note: truncated.then(|| TRUNCATED_500_NOTE.to_string()),
    })
}

async fn post_browse(
    client: &reqwest::Client,
    endpoint: &str,
    body: &Value,
) -> Result<Value, PlaylistImportError> {
    let resp = client
        .post(endpoint)
        .header("Origin", YTM_BASE)
        .header("Referer", format!("{YTM_BASE}/"))
        .header("Accept-Language", "en-US,en;q=0.9")
        .json(body)
        .send()
        .await
        .map_err(|e| {
            PlaylistImportError::Upstream(format!("Could not reach YouTube Music: {e}"))
        })?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
        return Err(PlaylistImportError::NotFound(
            "YouTube Music playlist was not found or is private.".into(),
        ));
    }
    if !status.is_success() {
        return Err(PlaylistImportError::Upstream(format!(
            "YouTube Music returned HTTP {}",
            status.as_u16()
        )));
    }

    resp.json::<Value>().await.map_err(|e| {
        PlaylistImportError::Upstream(format!("Could not parse YouTube Music response: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn list_item(title: &str, artist: &str, album: Option<&str>, duration: &str) -> Value {
        let mut flex = vec![
            json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [{ "text": title }] }
                }
            }),
            json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [{ "text": artist }, { "text": " • " }, { "text": "2.4M plays" }] }
                }
            }),
        ];
        if let Some(alb) = album {
            flex.push(json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [{ "text": alb }] }
                }
            }));
        }
        json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": flex,
                "fixedColumns": [{
                    "musicResponsiveListItemFixedColumnRenderer": {
                        "text": { "runs": [{ "text": duration }] }
                    }
                }]
            }
        })
    }

    #[test]
    fn parses_durations_and_normalizes_browse_ids() {
        assert_eq!(parse_duration_label("3:45"), Some(225));
        assert_eq!(parse_duration_label("1:02:03"), Some(3723));
        assert_eq!(parse_duration_label("0:00"), None);
        assert_eq!(parse_duration_label("3:75"), None);
        assert_eq!(parse_duration_label("invalid"), None);

        assert_eq!(normalize_browse_id("PL12345678"), "VLPL12345678");
        assert_eq!(normalize_browse_id("VLPL12345678"), "VLPL12345678");
    }

    #[tokio::test]
    async fn follows_continuation_tokens_and_guards_cycles() {
        let page1 = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "tabs": [{
                        "tabRenderer": {
                            "content": {
                                "sectionListRenderer": {
                                    "contents": [{
                                        "musicResponsiveHeaderRenderer": {
                                            "title": { "runs": [{ "text": "Focus Beats" }] },
                                            "description": {
                                                "musicDescriptionShelfRenderer": {
                                                    "description": { "runs": [{ "text": "Instrumental flow" }] }
                                                }
                                            }
                                        }
                                    }]
                                }
                            }
                        }
                    }],
                    "secondaryContents": {
                        "sectionListRenderer": {
                            "contents": [{
                                "musicPlaylistShelfRenderer": {
                                    "contents": [
                                        list_item("Nuvole Bianche", "Ludovico Einaudi", Some("Una Mattina"), "5:57"),
                                        {
                                            "continuationItemRenderer": {
                                                "continuationEndpoint": {
                                                    "continuationCommand": { "token": "tok-page-2" }
                                                }
                                            }
                                        }
                                    ]
                                }
                            }]
                        }
                    }
                }
            }
        });
        // Second page repeats its own token to verify cycle guard stops cleanly.
        let page2 = json!({
            "onResponseReceivedActions": [{
                "appendContinuationItemsAction": {
                    "continuationItems": [
                        list_item("Avril 14th", "Aphex Twin", Some("Drukqs"), "2:05"),
                        {
                            "continuationItemRenderer": {
                                "continuationEndpoint": {
                                    "continuationCommand": { "token": "tok-page-2" }
                                }
                            }
                        }
                    ]
                }
            }]
        });

        let (base, seen) = serve(vec![
            Reply {
                status: 200,
                body: page1.to_string(),
            },
            Reply {
                status: 200,
                body: page2.to_string(),
            },
        ]);

        let pl = fetch_playlist_from(
            &base,
            "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            "https://music.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            false,
        )
        .await
        .unwrap();

        assert_eq!(pl.platform, PlaylistPlatform::YouTubeMusic);
        assert_eq!(pl.title, "Focus Beats");
        assert_eq!(pl.description.as_deref(), Some("Instrumental flow"));
        assert_eq!(pl.tracks.len(), 2);
        assert_eq!(
            pl.tracks[0],
            ExternalTrack {
                artist: "Ludovico Einaudi".into(),
                title: "Nuvole Bianche".into(),
                album: Some("Una Mattina".into()),
                duration_s: Some(357),
                recording_mbid: None,
            }
        );
        assert_eq!(
            pl.tracks[1],
            ExternalTrack {
                artist: "Aphex Twin".into(),
                title: "Avril 14th".into(),
                album: Some("Drukqs".into()),
                duration_s: Some(125),
                recording_mbid: None,
            }
        );

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].target, "/youtubei/v1/browse?prettyPrint=false");
        assert!(reqs[0].body.contains("VLPLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI"));
        assert!(reqs[1].body.contains("tok-page-2"));
    }
}
