//! YouTube Music / YouTube playlist resolver via YouTube Music's `WEB_REMIX` InnerTube browse API.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::redirect::Policy;
use serde_json::{json, Value};
use shared::playlist_import::{ExternalPlaylist, ExternalTrack, PlaylistPlatform};

use super::{
    pinned_https_redirect, read_bounded_bytes, PlaylistImportError, MAX_PLAYLIST_TRACKS,
    MAX_RESPONSE_BYTES,
};

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

fn is_four_digit_year(text: &str) -> bool {
    let t = text.trim();
    t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit())
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
        || matches!(lower.as_str(), "song" | "video")
}

fn has_navigation_endpoint(run: &Value) -> bool {
    run.get("navigationEndpoint").is_some_and(|v| !v.is_null())
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

    let has_other_linked_run = runs.iter().any(|r| {
        has_navigation_endpoint(r)
            && r.get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| !is_bullet_separator(t) && !looks_like_metadata_noise(t))
    });

    // Split runs at bullet separators; take the first segment that contains artist names,
    // skipping leading noise badges (such as "Song" / "Video") and bare unlinked year runs
    // when linked artist runs exist alongside them.
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
        if parts.is_empty() && looks_like_metadata_noise(trimmed) {
            continue;
        }
        if parts.is_empty()
            && is_four_digit_year(trimmed)
            && !has_navigation_endpoint(run)
            && has_other_linked_run
        {
            continue;
        }
        parts.push(text);
    }

    parts.concat().trim().to_string()
}

fn extract_album_from_column(col: &Value) -> Option<String> {
    let text_obj = col
        .get("musicResponsiveListItemFlexColumnRenderer")
        .and_then(|r| r.get("text"))?;
    if let Some(runs) = text_obj.get("runs").and_then(Value::as_array) {
        let has_linked_run = runs.iter().any(|r| {
            has_navigation_endpoint(r)
                && r.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !is_bullet_separator(t) && !looks_like_metadata_noise(t))
        });
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
            if parts.is_empty() && looks_like_metadata_noise(trimmed) {
                continue;
            }
            if parts.is_empty()
                && is_four_digit_year(trimmed)
                && !has_navigation_endpoint(run)
                && has_linked_run
            {
                continue;
            }
            parts.push(text);
        }
        let joined = parts.concat();
        let trimmed = joined.trim();
        return (!trimmed.is_empty() && !looks_like_metadata_noise(trimmed))
            .then(|| trimmed.to_string());
    }
    simple_or_runs_text(text_obj).filter(|a| !looks_like_metadata_noise(a))
}

fn extract_column_text(col: &Value) -> Option<String> {
    col.get("musicResponsiveListItemFlexColumnRenderer")
        .and_then(|r| r.get("text"))
        .and_then(simple_or_runs_text)
}

fn has_playable_video_id(item: &Value) -> bool {
    let non_empty = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    };
    if non_empty(item.pointer("/playlistItemData/videoId")) {
        return true;
    }
    if non_empty(item.pointer(
        "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
    )) {
        return true;
    }
    if let Some(runs) = item
        .pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs")
        .and_then(Value::as_array)
    {
        for run in runs {
            if non_empty(run.pointer("/navigationEndpoint/watchEndpoint/videoId")) {
                return true;
            }
        }
    }
    false
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
    if item
        .get("musicItemRendererDisplayPolicy")
        .and_then(Value::as_str)
        == Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
    {
        return None;
    }
    if !has_playable_video_id(item) {
        return None;
    }
    let cols = item.get("flexColumns")?.as_array()?;
    let title = cols.first().and_then(extract_column_text)?;
    let artist = cols
        .get(1)
        .map(extract_artists_from_column)
        .unwrap_or_default();
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    let album = cols.get(2).and_then(extract_album_from_column);
    let duration_s = extract_duration_from_item(item);
    Some(ExternalTrack {
        artist,
        title,
        album,
        duration_s,
        recording_mbid: None,
    })
}

fn continuation_token_from_item(entry: &Value) -> Option<String> {
    entry
        .pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn continuation_token_from_array(continuations: &Value) -> Option<String> {
    let arr = continuations.as_array()?;
    for entry in arr {
        if let Some(token) = entry
            .pointer("/nextContinuationData/continuation")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(token.to_string());
        }
    }
    None
}

fn extract_items_and_token(
    items: &[Value],
    tracks: &mut Vec<ExternalTrack>,
    continuation: &mut Option<String>,
) {
    for entry in items {
        if let Some(item) = entry.get("musicResponsiveListItemRenderer") {
            if let Some(track) = parse_track_item(item) {
                tracks.push(track);
            }
        }
        if continuation.is_none() {
            if let Some(token) = continuation_token_from_item(entry) {
                *continuation = Some(token);
            }
        }
    }
}

/// Finds the primary `musicPlaylistShelfRenderer` object without descending into unrelated
/// shelves or `sectionListRenderer.continuations`.
fn find_playlist_shelf(val: &Value) -> Option<&Value> {
    match val {
        Value::Object(map) => {
            if let Some(shelf) = map.get("musicPlaylistShelfRenderer") {
                return Some(shelf);
            }
            for (k, child) in map {
                if k == "musicShelfRenderer" || k == "continuations" {
                    continue;
                }
                if let Some(found) = find_playlist_shelf(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(find_playlist_shelf),
        _ => None,
    }
}

/// Extracts tracks and the next continuation token strictly from the page-1
/// `musicPlaylistShelfRenderer` (never from `musicShelfRenderer` Suggestions or
/// `sectionListRenderer.continuations`).
fn parse_initial_browse_page(page: &Value) -> Option<(Vec<ExternalTrack>, Option<String>)> {
    let shelf = find_playlist_shelf(page)?;
    let mut tracks = Vec::new();
    let mut continuation = None;
    if let Some(contents) = shelf.get("contents").and_then(Value::as_array) {
        extract_items_and_token(contents, &mut tracks, &mut continuation);
    }
    if continuation.is_none() {
        if let Some(conts) = shelf.get("continuations") {
            continuation = continuation_token_from_array(conts);
        }
    }
    Some((tracks, continuation))
}

/// Extracts tracks and the next continuation token strictly from continuation response
/// containers (`appendContinuationItemsAction.continuationItems` and
/// `continuationContents.musicPlaylistShelfContinuation`).
fn parse_continuation_page(page: &Value) -> (Vec<ExternalTrack>, Option<String>) {
    let mut tracks = Vec::new();
    let mut continuation = None;

    if let Some(actions) = page
        .get("onResponseReceivedActions")
        .and_then(Value::as_array)
    {
        for action in actions {
            if let Some(items) = action
                .pointer("/appendContinuationItemsAction/continuationItems")
                .and_then(Value::as_array)
            {
                extract_items_and_token(items, &mut tracks, &mut continuation);
            }
        }
    }

    if let Some(shelf) = page.pointer("/continuationContents/musicPlaylistShelfContinuation") {
        if let Some(contents) = shelf.get("contents").and_then(Value::as_array) {
            extract_items_and_token(contents, &mut tracks, &mut continuation);
        }
        if continuation.is_none() {
            if let Some(conts) = shelf.get("continuations") {
                continuation = continuation_token_from_array(conts);
            }
        }
    }

    (tracks, continuation)
}

/// Extracts tracks and the next continuation token (if any) from a browse response page,
/// strictly scoped to `musicPlaylistShelfRenderer` (page 1) or playlist continuation containers.
pub fn parse_browse_page(page: &Value) -> (Vec<ExternalTrack>, Option<String>) {
    if let Some(initial) = parse_initial_browse_page(page) {
        return initial;
    }
    parse_continuation_page(page)
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

fn check_browse_error(page: &Value) -> Result<(), PlaylistImportError> {
    let Some(err) = page.get("error") else {
        return Ok(());
    };
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
    Err(PlaylistImportError::Upstream(format!(
        "YouTube Music error: {msg}"
    )))
}

pub(crate) async fn fetch_playlist_from(
    base_url: &str,
    list_id: &str,
    source_url: &str,
    enforce_ytm_host: bool,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let redirect = if enforce_ytm_host {
        pinned_https_redirect("music.youtube.com")
    } else {
        Policy::none()
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
    check_browse_error(&first_page)?;

    let (title, description) = extract_header(&first_page);
    let (mut tracks, mut next_token) = parse_initial_browse_page(&first_page).unwrap_or_default();

    let mut seen_tokens = HashSet::new();
    let mut pages = 0usize;
    let mut truncated = false;

    while let Some(token) = next_token.take() {
        if tracks.len() >= MAX_PLAYLIST_TRACKS {
            truncated = true;
            break;
        }
        if !seen_tokens.insert(token.clone()) {
            break;
        }
        if pages >= MAX_CONTINUATION_PAGES {
            truncated = true;
            break;
        }
        let body = json!({
            "context": client_ctx,
            "continuation": token,
        });
        let page = post_browse(&client, &endpoint, &body).await?;
        check_browse_error(&page)?;
        let (page_tracks, token_after) = parse_continuation_page(&page);
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

    let truncated_note = if truncated {
        if tracks.len() >= MAX_PLAYLIST_TRACKS {
            Some(TRUNCATED_500_NOTE.to_string())
        } else {
            Some(format!(
                "Showing the first {} tracks of this playlist.",
                tracks.len()
            ))
        }
    } else {
        None
    };

    Ok(ExternalPlaylist {
        platform: PlaylistPlatform::YouTubeMusic,
        source_url: source_url.to_string(),
        title: title.unwrap_or_else(|| "YouTube Music Playlist".to_string()),
        description,
        tracks,
        truncated_note,
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

    let bytes = read_bounded_bytes(resp, MAX_RESPONSE_BYTES).await?;
    serde_json::from_slice::<Value>(&bytes).map_err(|e| {
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
                "playlistItemData": { "videoId": "vid-playable" },
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

    #[tokio::test]
    async fn ignores_suggestions_shelf_and_section_list_continuations() {
        let page1 = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "secondaryContents": {
                        "sectionListRenderer": {
                            "contents": [
                                {
                                    "musicPlaylistShelfRenderer": {
                                        "contents": [
                                            list_item("Nuvole Bianche", "Ludovico Einaudi", Some("Una Mattina"), "5:57")
                                        ]
                                    }
                                },
                                {
                                    "musicShelfRenderer": {
                                        "title": { "runs": [{ "text": "Suggestions" }] },
                                        "contents": [
                                            list_item("Unrelated Suggestion", "Other Artist", Some("Other Album"), "3:33")
                                        ]
                                    }
                                }
                            ],
                            "continuations": [{
                                "nextContinuationData": {
                                    "continuation": "tok-suggestions-do-not-follow"
                                }
                            }]
                        }
                    }
                }
            }
        });

        let (base, seen) = serve(vec![Reply {
            status: 200,
            body: page1.to_string(),
        }]);

        let pl = fetch_playlist_from(
            &base,
            "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            "https://music.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            false,
        )
        .await
        .unwrap();

        assert_eq!(pl.tracks.len(), 1);
        assert_eq!(pl.tracks[0].title, "Nuvole Bianche");
        // Section-list continuation must NOT be followed.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn skips_greyed_out_and_unplayable_items() {
        let greyed_out = json!({
            "musicResponsiveListItemRenderer": {
                "musicItemRendererDisplayPolicy": "MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT",
                "playlistItemData": { "videoId": "vid-grey" },
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Unavailable Track" }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Some Artist" }] } } }
                ]
            }
        });
        let no_video_id = json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Deleted Track" }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Some Artist" }] } } }
                ]
            }
        });
        let page = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "secondaryContents": {
                        "sectionListRenderer": {
                            "contents": [{
                                "musicPlaylistShelfRenderer": {
                                    "contents": [
                                        greyed_out,
                                        no_video_id,
                                        list_item("Playable Track", "Good Artist", Some("Good Album"), "4:01")
                                    ]
                                }
                            }]
                        }
                    }
                }
            }
        });

        let (tracks, next) = parse_browse_page(&page);
        assert_eq!(next, None);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Playable Track");
    }

    #[test]
    fn preserves_four_digit_artist_and_album_names() {
        let page = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "secondaryContents": {
                        "sectionListRenderer": {
                            "contents": [{
                                "musicPlaylistShelfRenderer": {
                                    "contents": [
                                        list_item("Arrival. The Meuse-Argonne", "1914", Some("The Blind Leading the Blind"), "6:20"),
                                        list_item("Blank Space", "Taylor Swift", Some("1989"), "3:51"),
                                        list_item("Little Red Corvette", "1999", Some("1999"), "4:56")
                                    ]
                                }
                            }]
                        }
                    }
                }
            }
        });

        let (tracks, _) = parse_browse_page(&page);
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].artist, "1914");
        assert_eq!(tracks[0].album.as_deref(), Some("The Blind Leading the Blind"));
        assert_eq!(tracks[1].artist, "Taylor Swift");
        assert_eq!(tracks[1].album.as_deref(), Some("1989"));
        assert_eq!(tracks[2].artist, "1999");
        assert_eq!(tracks[2].album.as_deref(), Some("1999"));
    }

    #[tokio::test]
    async fn continuation_api_error_is_surfaced() {
        let page1 = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "secondaryContents": {
                        "sectionListRenderer": {
                            "contents": [{
                                "musicPlaylistShelfRenderer": {
                                    "contents": [
                                        list_item("Track 1", "Artist 1", None, "3:00"),
                                        {
                                            "continuationItemRenderer": {
                                                "continuationEndpoint": {
                                                    "continuationCommand": { "token": "tok-err" }
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
        let page2_err = json!({
            "error": { "code": 500, "message": "Backend unavailable" }
        });

        let (base, _) = serve(vec![
            Reply {
                status: 200,
                body: page1.to_string(),
            },
            Reply {
                status: 200,
                body: page2_err.to_string(),
            },
        ]);

        let err = fetch_playlist_from(
            &base,
            "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            "https://music.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PlaylistImportError::Upstream(_)), "{err:?}");
    }
}
