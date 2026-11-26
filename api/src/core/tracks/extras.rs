//! Fetching missing album cover art (`cover.jpg` / `cover.png`) from Cover Art Archive (via
//! MusicBrainz) and synced/plain lyrics (`{stem}.lrc`) from LRCLIB, writing files atomically
//! inside the caller's owned library folders and triggering a Navidrome scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shared::client::{TrackLyricLine, TrackLyrics};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::NativeSong;
use tracing::info;

use super::{
    find_sidecars, trigger_and_wait_scan, ScanWaitOutcome, TrackSelection, ALBUM_MAX_TRACKS,
    SCAN_WAIT_MAX, SONGS_PER_CALL,
};
use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::{paths, rules, Guard};
use crate::core::index::NavidromeOf;
use crate::core::library::{album_not_found, SongFacts};
use crate::folder_policy::PolicyRows;
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;

/// User-Agent sent to Cover Art Archive, MusicBrainz, and LRCLIB.
const USER_AGENT: &str = "Soulbeet/0.10.0";
/// HTTP timeout per upstream metadata/art/lyrics request.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on downloaded cover art image size (10 MiB).
const MAX_ART_BYTES: usize = 10 * 1024 * 1024;
/// Hard cap on LRCLIB JSON response body size (512 KiB).
const MAX_LYRICS_BYTES: usize = 512 * 1024;

async fn read_capped_json<T: serde::de::DeserializeOwned>(
    mut resp: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<T, String> {
    if let Some(len) = resp.content_length() {
        if len as usize > max_bytes {
            return Err(format!(
                "{label} response exceeds {max_bytes} byte limit ({len} bytes)"
            ));
        }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("Failed reading {label} response body: {e}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("{label} response exceeds {max_bytes} byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Failed to parse {label} response: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedArt {
    pub bytes: Vec<u8>,
    /// `"jpg"` or `"png"`.
    pub ext: &'static str,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedLyrics {
    pub content: String,
    pub synced: bool,
    pub source: String,
}

#[async_trait]
pub(crate) trait ExtrasFetcher: Send + Sync {
    async fn fetch_cover_art(
        &self,
        release_mbid: Option<&str>,
        recording_mbid: Option<&str>,
        artist: &str,
        album: &str,
    ) -> Result<Option<FetchedArt>, String>;

    async fn fetch_lyrics(
        &self,
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration_s: Option<u32>,
    ) -> Result<Option<FetchedLyrics>, String>;
}

/// Live HTTP fetcher backed by Cover Art Archive, MusicBrainz, and LRCLIB.
pub(crate) struct LiveExtrasFetcher {
    client: reqwest::Client,
    caa_base: String,
    mb_base: String,
    lrclib_base: String,
}

impl Default for LiveExtrasFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveExtrasFetcher {
    pub(crate) fn new() -> Self {
        Self::with_bases(
            "https://coverartarchive.org".into(),
            "https://musicbrainz.org".into(),
            "https://lrclib.net".into(),
        )
    }

    pub(crate) fn with_bases(caa_base: String, mb_base: String, lrclib_base: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            caa_base,
            mb_base,
            lrclib_base,
        }
    }

    async fn search_lyrics(
        &self,
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration_s: Option<u32>,
    ) -> Result<Option<FetchedLyrics>, String> {
        let url = format!("{}/api/search", self.lrclib_base);
        let resp = self
            .client
            .get(&url)
            .query(&[("artist_name", artist.trim()), ("track_name", title.trim())])
            .send()
            .await
            .map_err(|e| format!("LRCLIB search request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "LRCLIB search returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let items: Vec<LrcLibSearchItem> =
            read_capped_json(resp, MAX_LYRICS_BYTES, "LRCLIB search").await?;
        Ok(pick_best_lrclib_result(
            &items, artist, title, album, duration_s,
        ))
    }

    async fn fetch_caa(&self, release_mbid: &str) -> Result<Option<FetchedArt>, String> {
        let mbid = release_mbid.trim();
        if mbid.is_empty() || mbid.contains(['/', '\\', '?', '#']) {
            return Ok(None);
        }
        for suffix in ["front-500", "front"] {
            let url = format!("{}/release/{mbid}/{suffix}", self.caa_base);
            let mut resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("Cover Art Archive request failed: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            if !status.is_success() {
                return Err(format!(
                    "Cover Art Archive returned HTTP {}",
                    status.as_u16()
                ));
            }
            if let Some(len) = resp.content_length() {
                if len as usize > MAX_ART_BYTES {
                    return Err(format!(
                        "Cover art exceeds {} byte size limit ({len} bytes)",
                        MAX_ART_BYTES
                    ));
                }
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| format!("Failed reading cover art body: {e}"))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_ART_BYTES {
                    return Err(format!(
                        "Cover art exceeds {} byte size limit",
                        MAX_ART_BYTES
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            let Some(ext) = detect_image_ext(&bytes) else {
                return Err("Cover Art Archive returned an unsupported image format (expected JPEG or PNG)".into());
            };
            return Ok(Some(FetchedArt {
                bytes,
                ext,
                source: format!("coverartarchive:{mbid}"),
            }));
        }
        Ok(None)
    }

    async fn release_mbid_for_recording(
        &self,
        recording_mbid: &str,
    ) -> Result<Option<String>, String> {
        let mbid = recording_mbid.trim();
        if mbid.is_empty() || mbid.contains(['/', '\\', '?', '#']) {
            return Ok(None);
        }
        let url = format!("{}/ws/2/recording/{mbid}?inc=releases&fmt=json", self.mb_base);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("MusicBrainz recording lookup failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "MusicBrainz recording lookup returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let body: MbRecordingReleases = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse MusicBrainz recording response: {e}"))?;
        Ok(pick_best_release(&body.releases))
    }

    async fn search_release_mbids(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Vec<String>, String> {
        if artist.trim().is_empty() || album.trim().is_empty() {
            return Ok(vec![]);
        }
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist.trim()),
            escape_lucene(album.trim())
        );
        let url = format!("{}/ws/2/release/", self.mb_base);
        let resp = self
            .client
            .get(&url)
            .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "3")])
            .send()
            .await
            .map_err(|e| format!("MusicBrainz release search failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "MusicBrainz release search returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let body: MbReleaseSearch = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse MusicBrainz release search response: {e}"))?;
        Ok(body
            .releases
            .into_iter()
            .filter(|r| !r.id.is_empty())
            .map(|r| r.id)
            .collect())
    }
}

#[async_trait]
impl ExtrasFetcher for LiveExtrasFetcher {
    async fn fetch_cover_art(
        &self,
        release_mbid: Option<&str>,
        recording_mbid: Option<&str>,
        artist: &str,
        album: &str,
    ) -> Result<Option<FetchedArt>, String> {
        if let Some(rid) = release_mbid.map(str::trim).filter(|s| !s.is_empty()) {
            return self.fetch_caa(rid).await;
        }
        if let Some(rec) = recording_mbid.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(rid) = self.release_mbid_for_recording(rec).await? {
                return self.fetch_caa(&rid).await;
            }
            return Ok(None);
        }
        let candidates = self.search_release_mbids(artist, album).await?;
        for rid in candidates {
            if let Some(art) = self.fetch_caa(&rid).await? {
                return Ok(Some(art));
            }
        }
        Ok(None)
    }

    async fn fetch_lyrics(
        &self,
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration_s: Option<u32>,
    ) -> Result<Option<FetchedLyrics>, String> {
        if artist.trim().is_empty() || title.trim().is_empty() {
            return Ok(None);
        }
        let mut params: Vec<(&str, String)> = vec![
            ("artist_name", artist.trim().to_string()),
            ("track_name", title.trim().to_string()),
        ];
        if let Some(al) = album.map(str::trim).filter(|s| !s.is_empty()) {
            params.push(("album_name", al.to_string()));
        }
        if let Some(dur) = duration_s.filter(|d| *d > 0) {
            params.push(("duration", dur.to_string()));
        }
        let url = format!("{}/api/get", self.lrclib_base);
        let resp = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("LRCLIB request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return self.search_lyrics(artist, title, album, duration_s).await;
        }
        if !resp.status().is_success() {
            return Err(format!("LRCLIB returned HTTP {}", resp.status().as_u16()));
        }
        let body: LrcLibResponse = read_capped_json(resp, MAX_LYRICS_BYTES, "LRCLIB").await?;
        if body.instrumental {
            return Ok(None);
        }
        if let Some(synced) = body
            .synced_lyrics
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(Some(FetchedLyrics {
                content: synced,
                synced: true,
                source: "lrclib".into(),
            }));
        }
        if let Some(plain) = body
            .plain_lyrics
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(Some(FetchedLyrics {
                content: plain,
                synced: false,
                source: "lrclib".into(),
            }));
        }
        self.search_lyrics(artist, title, album, duration_s).await
    }
}

#[derive(Deserialize)]
struct MbRecordingReleases {
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(Deserialize)]
struct MbReleaseSearch {
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(Deserialize)]
struct MbRelease {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "release-group")]
    release_group: Option<MbReleaseGroup>,
}

#[derive(Deserialize)]
struct MbReleaseGroup {
    #[serde(default, rename = "primary-type")]
    primary_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrcLibResponse {
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    synced_lyrics: Option<String>,
    #[serde(default)]
    plain_lyrics: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrcLibSearchItem {
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    album_name: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    synced_lyrics: Option<String>,
    #[serde(default)]
    plain_lyrics: Option<String>,
}

/// Computes token-overlap similarity in `[0.0, 1.0]` between two normalized music strings.
fn text_similarity(a: &str, b: &str) -> f64 {
    let na = normalize_for_matching(a);
    let nb = normalize_for_matching(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let words_a: std::collections::HashSet<&str> = na.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = nb.split_whitespace().collect();
    let inter = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Picks the best LRCLIB search candidate matching `artist` and `title`, preferring synced lyrics
/// and closest duration when known.
fn pick_best_lrclib_result(
    items: &[LrcLibSearchItem],
    artist: &str,
    title: &str,
    album: Option<&str>,
    duration_s: Option<u32>,
) -> Option<FetchedLyrics> {
    let norm_artist = normalize_for_matching(artist);
    let norm_title = normalize_for_matching(title);
    let norm_album = album
        .map(normalize_for_matching)
        .filter(|s| !s.is_empty());

    let mut best: Option<(f64, FetchedLyrics)> = None;
    for item in items {
        if item.instrumental {
            continue;
        }
        let synced = item
            .synced_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        let plain = item
            .plain_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        let (content, is_synced) = match (synced, plain) {
            (Some(s), _) => (s, true),
            (None, Some(p)) => (p, false),
            (None, None) => continue,
        };

        let artist_sim = text_similarity(artist, &item.artist_name);
        let title_sim = text_similarity(title, &item.track_name);
        if artist_sim < 0.5 || title_sim < 0.5 {
            continue;
        }

        let mut score = artist_sim * 40.0 + title_sim * 40.0;
        if normalize_for_matching(&item.artist_name) == norm_artist {
            score += 15.0;
        }
        // Exact normalized title match must outweigh the synced bonus so a wrong-title synced
        // track (e.g. "Part 2") never beats an exact-title plain track ("Part 1").
        if normalize_for_matching(&item.track_name) == norm_title {
            score += 30.0;
        }
        if is_synced {
            score += 20.0;
        }
        if let (Some(want_dur), Some(cand_dur)) = (duration_s.filter(|d| *d > 0), item.duration) {
            let diff = (cand_dur - f64::from(want_dur)).abs();
            if diff > 30.0 {
                continue;
            }
            score += 20.0 * (1.0 - (diff / 30.0).min(1.0));
        }
        if let (Some(want_al), Some(cand_al)) = (&norm_album, item.album_name.as_deref()) {
            if normalize_for_matching(cand_al) == *want_al {
                score += 5.0;
            }
        }

        if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
            best = Some((
                score,
                FetchedLyrics {
                    content: content.to_string(),
                    synced: is_synced,
                    source: "lrclib".into(),
                },
            ));
        }
    }
    best.map(|(_, lyrics)| lyrics)
}

/// Parses an LRC timestamp tag body like `01:23.45`, `01:23.456`, `01:23:45`, or `01:23` into
/// milliseconds.
fn parse_lrc_timestamp_ms(tag: &str) -> Option<i64> {
    let (min_str, rest) = tag.split_once(':')?;
    if min_str.is_empty() || !min_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minutes: i64 = min_str.parse().ok()?;
    let (sec_str, frac_str) = match rest.split_once('.').or_else(|| rest.split_once(':')) {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    if sec_str.is_empty() || sec_str.len() > 2 || !sec_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds: i64 = sec_str.parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let millis: i64 = match frac_str {
        None => 0,
        Some(f) if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) => return None,
        Some(f) => match f.len() {
            1 => f.parse::<i64>().ok()? * 100,
            2 => f.parse::<i64>().ok()? * 10,
            3 => f.parse::<i64>().ok()?,
            _ => f[..3].parse::<i64>().ok()?,
        },
    };
    minutes
        .checked_mul(60_000)?
        .checked_add(seconds.checked_mul(1_000)?)?
        .checked_add(millis)
}

/// True if `tag` (the text inside `[...]`) is a known LRC metadata header tag like `ar:...`,
/// `ti:...`, `al:...`, `offset:...`, etc.
fn is_lrc_metadata_tag(tag: &str) -> bool {
    let Some((key, _)) = tag.split_once(':') else {
        return false;
    };
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "ar" | "ti" | "al" | "au" | "by" | "length" | "offset" | "re" | "ve" | "tool" | "la" | "#"
    )
}

/// Parses synced LRC or plain lyrics text into `TrackLyrics`.
/// Supports multiple timestamps per line (compressed LRC), centiseconds and milliseconds,
/// `[offset:...]` tags, strips metadata header tags (`[ar:]`, `[ti:]`, etc.), and falls back to
/// plain lines when no valid timestamps are found.
pub(crate) fn parse_lrc(
    raw: &str,
    synced: bool,
    display_artist: Option<String>,
    display_title: Option<String>,
) -> TrackLyrics {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut offset_ms: Option<i64> = None;
    let mut timed_lines: Vec<(i64, usize, String)> = Vec::new();
    let mut plain_lines: Vec<TrackLyricLine> = Vec::new();

    for (line_idx, raw_line) in raw.lines().enumerate() {
        let trimmed = raw_line.trim();
        // Check for pure metadata header tag like `[ar:Artist]` or `[offset:500]`
        if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if !inner.contains(']') && is_lrc_metadata_tag(inner) {
                if let Some((key, val)) = inner.split_once(':') {
                    if key.trim().eq_ignore_ascii_case("offset") {
                        if let Ok(parsed) = val.trim().parse::<i64>() {
                            offset_ms = Some(parsed);
                        }
                    }
                }
                continue;
            }
        }

        if synced {
            let mut rest = trimmed;
            let mut timestamps: Vec<i64> = Vec::new();
            while let Some(after_open) = rest.strip_prefix('[') {
                let Some(close_idx) = after_open.find(']') else {
                    break;
                };
                let tag = &after_open[..close_idx];
                if let Some(ms) = parse_lrc_timestamp_ms(tag) {
                    timestamps.push(ms);
                    rest = after_open[close_idx + 1..].trim_start();
                } else if is_lrc_metadata_tag(tag) {
                    rest = after_open[close_idx + 1..].trim_start();
                } else {
                    break;
                }
            }
            if !timestamps.is_empty() {
                let text = rest.trim().to_string();
                for ms in timestamps {
                    timed_lines.push((ms, line_idx, text.clone()));
                }
                continue;
            }
        }

        plain_lines.push(TrackLyricLine {
            start_ms: None,
            text: raw_line.trim_end().to_string(),
        });
    }

    if synced && !timed_lines.is_empty() {
        timed_lines.sort_by_key(|(ms, idx, _)| (*ms, *idx));
        return TrackLyrics {
            display_artist,
            display_title,
            lang: "und".to_string(),
            synced: true,
            offset_ms,
            lines: timed_lines
                .into_iter()
                .map(|(ms, _, text)| TrackLyricLine {
                    start_ms: Some(ms),
                    text,
                })
                .collect(),
        };
    }

    // Trim leading/trailing empty lines for plain lyrics fallback.
    while plain_lines
        .first()
        .is_some_and(|l| l.text.trim().is_empty())
    {
        plain_lines.remove(0);
    }
    while plain_lines
        .last()
        .is_some_and(|l| l.text.trim().is_empty())
    {
        plain_lines.pop();
    }

    TrackLyrics {
        display_artist,
        display_title,
        lang: "und".to_string(),
        synced: false,
        offset_ms,
        lines: plain_lines,
    }
}

fn pick_best_release(releases: &[MbRelease]) -> Option<String> {
    let valid = |r: &&MbRelease| !r.id.is_empty();
    let is_official = |r: &&MbRelease| {
        r.status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("Official"))
    };
    let is_album = |r: &&MbRelease| {
        r.release_group
            .as_ref()
            .and_then(|rg| rg.primary_type.as_deref())
            .is_some_and(|t| t.eq_ignore_ascii_case("Album"))
    };
    releases
        .iter()
        .find(|r| valid(r) && is_official(r) && is_album(r))
        .or_else(|| releases.iter().find(|r| valid(r) && is_official(r)))
        .or_else(|| releases.iter().find(valid))
        .map(|r| r.id.clone())
}

fn detect_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else {
        None
    }
}

fn escape_lucene(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(
            c,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Clone)]
pub struct ExtrasRequest {
    pub selection: TrackSelection,
    pub artwork: bool,
    pub lyrics: bool,
    pub dry_run: bool,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtworkOutcome {
    /// `"already_present"`, `"would_write"`, `"written"`, `"not_found"`, or `"error"`.
    pub status: String,
    pub file: Option<String>,
    pub source: Option<String>,
    pub bytes: Option<usize>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LyricsOutcome {
    /// `"already_present"`, `"would_write"`, `"written"`, `"not_found"`, or `"error"`.
    pub status: String,
    pub file: Option<String>,
    pub synced: Option<bool>,
    pub source: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtrasItemSuccess {
    pub song_id: String,
    pub song: SongFacts,
    pub file: String,
    pub artwork: Option<ArtworkOutcome>,
    pub lyrics: Option<LyricsOutcome>,
}

#[derive(Debug)]
pub struct ExtrasEntry {
    pub song_id: String,
    pub outcome: Result<ExtrasItemSuccess, Failure>,
}

#[derive(Debug)]
pub struct ExtrasReport {
    pub dry_run: bool,
    pub album_id: Option<String>,
    pub artwork_files_written: usize,
    pub lyrics_files_written: usize,
    pub scan_status: Option<String>,
    pub entries: Vec<ExtrasEntry>,
}

struct CheckedTrack {
    idx: usize,
    song: NativeSong,
    folder_root: PathBuf,
    file: PathBuf,
    dir: PathBuf,
}

/// Fetches and (when `!req.dry_run`) atomically writes missing album cover art and/or lyrics
/// sidecars for the selected songs inside the caller's owned library folders.
pub(crate) async fn fetch_artwork_and_lyrics(
    guard: &Guard<'_>,
    navidrome: &dyn NavidromeOf,
    queue: &ImportQueue,
    ctx: &UserCtx,
    fetcher: &dyn ExtrasFetcher,
    req: ExtrasRequest,
) -> Result<ExtrasReport, Failure> {
    if !req.artwork && !req.lyrics {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "At least one of artwork or lyrics must be true",
        ));
    }

    let user = ctx.id();

    let (song_ids, album_id) = match &req.selection {
        TrackSelection::Songs(ids) => {
            if ids.is_empty() || ids.len() > SONGS_PER_CALL {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!("Provide between 1 and {SONGS_PER_CALL} distinct song ids"),
                ));
            }
            (ids.clone(), None)
        }
        TrackSelection::Album(aid) => {
            let album = guard
                .listeners
                .album(&user, aid)
                .await
                .map_err(|e| e.failure(Stage::Retag, e.to_string()))?
                .ok_or_else(|| album_not_found(aid))?;
            if album.song.is_empty() {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "This album has no songs",
                ));
            }
            if album.song.len() > ALBUM_MAX_TRACKS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "This album has {} tracks, over the {ALBUM_MAX_TRACKS}-track limit",
                        album.song.len()
                    ),
                ));
            }
            (album.song.into_iter().map(|s| s.id).collect(), Some(aid.clone()))
        }
    };

    let rows = PolicyRows::load_in(guard.db)
        .await
        .map_err(|e| database_failure(Stage::Retag, e))?;
    let (policy_folders, _) = rows.inputs();
    let pending_discovery = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Retag, e))?;

    let mut entries: Vec<Option<ExtrasEntry>> = (0..song_ids.len()).map(|_| None).collect();
    let mut checked: Vec<CheckedTrack> = Vec::new();

    for (idx, song_id) in song_ids.into_iter().enumerate() {
        let song = match guard.read_song(&user, &song_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                entries[idx] = Some(ExtrasEntry {
                    song_id,
                    outcome: Err(Failure::new(
                        FailureCode::UnknownHandle,
                        Stage::Retag,
                        RetryAdvice::NotRetryable,
                        "Navidrome has no song with this id that this account can see",
                    )),
                });
                continue;
            }
            Err(f) => {
                entries[idx] = Some(ExtrasEntry {
                    song_id,
                    outcome: Err(Failure {
                        stage: Stage::Retag,
                        ..f
                    }),
                });
                continue;
            }
        };

        if pending_discovery
            .iter()
            .any(|t| t.song_id.as_deref() == Some(song.id.as_str()))
        {
            entries[idx] = Some(ExtrasEntry {
                song_id,
                outcome: Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "Cannot fetch artwork or lyrics for a pending discovery track; promote it into your library first",
                )),
            });
            continue;
        }

        let (file, _) = match paths::local_file_learning(guard.db, &song, guard.music_path, &user)
            .await
        {
            Ok(found) => found,
            Err(f) => {
                entries[idx] = Some(ExtrasEntry {
                    song_id,
                    outcome: Err(Failure {
                        stage: Stage::Retag,
                        ..f
                    }),
                });
                continue;
            }
        };

        if pending_discovery
            .iter()
            .any(|t| std::fs::canonicalize(&t.path).is_ok_and(|staged| staged == file))
        {
            entries[idx] = Some(ExtrasEntry {
                song_id,
                outcome: Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "Cannot fetch artwork or lyrics for a pending discovery track; promote it into your library first",
                )),
            });
            continue;
        }

        let folder_root = match rules::owning_folders(&file, &policy_folders, &user) {
            Ok(root) => root.to_path_buf(),
            Err(f) => {
                entries[idx] = Some(ExtrasEntry {
                    song_id,
                    outcome: Err(Failure {
                        stage: Stage::Retag,
                        ..f
                    }),
                });
                continue;
            }
        };

        if let Err(f) = rules::audio_file(&file) {
            entries[idx] = Some(ExtrasEntry {
                song_id,
                outcome: Err(Failure {
                    stage: Stage::Retag,
                    ..f
                }),
            });
            continue;
        }

        let Some(dir) = file.parent().map(Path::to_path_buf) else {
            entries[idx] = Some(ExtrasEntry {
                song_id,
                outcome: Err(Failure::new(
                    FailureCode::DeletePathUnresolved,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "Track file has no parent directory",
                )),
            });
            continue;
        };

        checked.push(CheckedTrack {
            idx,
            song,
            folder_root,
            file,
            dir,
        });
    }

    let mut art_by_dir: HashMap<PathBuf, ArtworkOutcome> = HashMap::new();
    let mut artwork_files_written = 0usize;
    let mut lyrics_files_written = 0usize;

    for track in checked {
        let sidecars = find_sidecars(&track.file);

        let artwork = if req.artwork {
            if let Some(cached) = art_by_dir.get(&track.dir) {
                Some(cached.clone())
            } else {
                let outcome = if !sidecars.album_art.is_empty() && !req.overwrite {
                    ArtworkOutcome {
                        status: "already_present".into(),
                        file: sidecars
                            .album_art
                            .first()
                            .map(|p| p.to_string_lossy().into_owned()),
                        source: None,
                        bytes: None,
                        error: None,
                    }
                } else {
                    let beets = queue
                        .inspect_file(&track.folder_root, &track.file)
                        .await
                        .ok()
                        .flatten();
                    let release_mbid = beets
                        .as_ref()
                        .and_then(|b| b.release_mbid.as_deref())
                        .filter(|s| !s.trim().is_empty());
                    let recording_mbid = track
                        .song
                        .mbz_recording_id
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| {
                            beets
                                .as_ref()
                                .and_then(|b| b.recording_mbid.as_deref())
                                .filter(|s| !s.trim().is_empty())
                        });
                    let artist = if !track.song.album_artist.trim().is_empty() {
                        track.song.album_artist.as_str()
                    } else {
                        track.song.artist.as_str()
                    };
                    match fetcher
                        .fetch_cover_art(release_mbid, recording_mbid, artist, &track.song.album)
                        .await
                    {
                        Ok(None) => ArtworkOutcome {
                            status: "not_found".into(),
                            file: None,
                            source: None,
                            bytes: None,
                            error: None,
                        },
                        Err(e) => ArtworkOutcome {
                            status: "error".into(),
                            file: None,
                            source: None,
                            bytes: None,
                            error: Some(cut_json(e, TEXT_MAX_CHARS)),
                        },
                        Ok(Some(art)) => {
                            let target = track.dir.join(format!("cover.{}", art.ext));
                            let target_str = target.to_string_lossy().into_owned();
                            let byte_len = art.bytes.len();
                            if req.dry_run {
                                ArtworkOutcome {
                                    status: "would_write".into(),
                                    file: Some(target_str),
                                    source: Some(art.source),
                                    bytes: Some(byte_len),
                                    error: None,
                                }
                            } else {
                                let write_target = target.clone();
                                let write_bytes = art.bytes;
                                match tokio::task::spawn_blocking(move || {
                                    atomic_write_file(&write_target, &write_bytes)
                                })
                                .await
                                {
                                    Ok(Ok(())) => {
                                        artwork_files_written += 1;
                                        info!("Wrote cover art {}", target.display());
                                        ArtworkOutcome {
                                            status: "written".into(),
                                            file: Some(target_str),
                                            source: Some(art.source),
                                            bytes: Some(byte_len),
                                            error: None,
                                        }
                                    }
                                    Ok(Err(e)) => ArtworkOutcome {
                                        status: "error".into(),
                                        file: Some(target_str),
                                        source: Some(art.source),
                                        bytes: Some(byte_len),
                                        error: Some(cut_json(
                                            format!("Could not write cover art: {e}"),
                                            TEXT_MAX_CHARS,
                                        )),
                                    },
                                    Err(e) => ArtworkOutcome {
                                        status: "error".into(),
                                        file: Some(target_str),
                                        source: Some(art.source),
                                        bytes: Some(byte_len),
                                        error: Some(cut_json(
                                            format!("Cover art write task failed: {e}"),
                                            TEXT_MAX_CHARS,
                                        )),
                                    },
                                }
                            }
                        }
                    }
                };
                art_by_dir.insert(track.dir.clone(), outcome.clone());
                Some(outcome)
            }
        } else {
            None
        };

        let lyrics = if req.lyrics {
            let existing_lrc = sidecars.track_files.iter().find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("lrc"))
            });
            let outcome = if let Some(existing) = existing_lrc.filter(|_| !req.overwrite) {
                LyricsOutcome {
                    status: "already_present".into(),
                    file: Some(existing.to_string_lossy().into_owned()),
                    synced: None,
                    source: None,
                    error: None,
                }
            } else {
                let album = (!track.song.album.trim().is_empty())
                    .then_some(track.song.album.as_str());
                let duration_s =
                    (track.song.duration > 0.0).then(|| track.song.duration.round() as u32);
                match fetcher
                    .fetch_lyrics(&track.song.artist, &track.song.title, album, duration_s)
                    .await
                {
                    Ok(None) => LyricsOutcome {
                        status: "not_found".into(),
                        file: None,
                        synced: None,
                        source: None,
                        error: None,
                    },
                    Err(e) => LyricsOutcome {
                        status: "error".into(),
                        file: None,
                        synced: None,
                        source: None,
                        error: Some(cut_json(e, TEXT_MAX_CHARS)),
                    },
                    Ok(Some(fetched)) => {
                        let target = track.file.with_extension("lrc");
                        let target_str = target.to_string_lossy().into_owned();
                        if req.dry_run {
                            LyricsOutcome {
                                status: "would_write".into(),
                                file: Some(target_str),
                                synced: Some(fetched.synced),
                                source: Some(fetched.source),
                                error: None,
                            }
                        } else {
                            let write_target = target.clone();
                            let write_bytes = fetched.content.into_bytes();
                            match tokio::task::spawn_blocking(move || {
                                atomic_write_file(&write_target, &write_bytes)
                            })
                            .await
                            {
                                Ok(Ok(())) => {
                                    lyrics_files_written += 1;
                                    info!("Wrote lyrics {}", target.display());
                                    LyricsOutcome {
                                        status: "written".into(),
                                        file: Some(target_str),
                                        synced: Some(fetched.synced),
                                        source: Some(fetched.source),
                                        error: None,
                                    }
                                }
                                Ok(Err(e)) => LyricsOutcome {
                                    status: "error".into(),
                                    file: Some(target_str),
                                    synced: Some(fetched.synced),
                                    source: Some(fetched.source),
                                    error: Some(cut_json(
                                        format!("Could not write lyrics: {e}"),
                                        TEXT_MAX_CHARS,
                                    )),
                                },
                                Err(e) => LyricsOutcome {
                                    status: "error".into(),
                                    file: Some(target_str),
                                    synced: Some(fetched.synced),
                                    source: Some(fetched.source),
                                    error: Some(cut_json(
                                        format!("Lyrics write task failed: {e}"),
                                        TEXT_MAX_CHARS,
                                    )),
                                },
                            }
                        }
                    }
                }
            };
            Some(outcome)
        } else {
            None
        };

        entries[track.idx] = Some(ExtrasEntry {
            song_id: track.song.id.clone(),
            outcome: Ok(ExtrasItemSuccess {
                song_id: track.song.id.clone(),
                song: SongFacts::from(&track.song),
                file: track.file.to_string_lossy().into_owned(),
                artwork,
                lyrics,
            }),
        });
    }

    let scan_status = if !req.dry_run && (artwork_files_written > 0 || lyrics_files_written > 0) {
        match navidrome.navidrome_of(&user).await {
            Ok(own_navi) => {
                let outcome = trigger_and_wait_scan(
                    guard.db,
                    navidrome,
                    &user,
                    &own_navi,
                    Stage::Index,
                    SCAN_WAIT_MAX,
                )
                .await;
                Some(
                    match outcome {
                        ScanWaitOutcome::Completed => "completed",
                        ScanWaitOutcome::NoScanAccount => "no_scan_account",
                        ScanWaitOutcome::TimedOut => "timed_out",
                        ScanWaitOutcome::Failed(_) => "failed",
                    }
                    .to_string(),
                )
            }
            Err(_) => Some("no_scan_account".to_string()),
        }
    } else {
        None
    };

    Ok(ExtrasReport {
        dry_run: req.dry_run,
        album_id,
        artwork_files_written,
        lyrics_files_written,
        scan_status,
        entries: entries.into_iter().flatten().collect(),
    })
}

/// Atomically writes `bytes` to `target` using a hidden `.tmp` sibling in the same directory so
/// Navidrome's watcher never observes a half-written cover or `.lrc` file.
pub(crate) fn atomic_write_file(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = target.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Target path has no parent directory",
        ));
    };
    let tmp = parent.join(format!(".soulbeet-extras-{}.tmp", uuid::Uuid::new_v4()));
    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, target)?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::jobs::runner::Acquire;
    use crate::core::test_ctx;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::NavidromeFor;

    struct FakeFetcher {
        art: Option<FetchedArt>,
        lyrics: Option<FetchedLyrics>,
        art_calls: AtomicUsize,
        lyrics_calls: AtomicUsize,
    }

    impl FakeFetcher {
        fn with_both() -> Self {
            Self {
                art: Some(FetchedArt {
                    bytes: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
                    ext: "jpg",
                    source: "coverartarchive:test-release".into(),
                }),
                lyrics: Some(FetchedLyrics {
                    content: "[00:01.00] O Holy Night".into(),
                    synced: true,
                    source: "lrclib".into(),
                }),
                art_calls: AtomicUsize::new(0),
                lyrics_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ExtrasFetcher for FakeFetcher {
        async fn fetch_cover_art(
            &self,
            _release_mbid: Option<&str>,
            _recording_mbid: Option<&str>,
            _artist: &str,
            _album: &str,
        ) -> Result<Option<FetchedArt>, String> {
            self.art_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.art.clone())
        }

        async fn fetch_lyrics(
            &self,
            _artist: &str,
            _title: &str,
            _album: Option<&str>,
            _duration_s: Option<u32>,
        ) -> Result<Option<FetchedLyrics>, String> {
            self.lyrics_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.lyrics.clone())
        }
    }

    #[tokio::test]
    async fn writes_cover_once_per_album_dir_and_lyrics_per_track() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        let (acq, _) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));
        let fetcher = FakeFetcher::with_both();

        let report = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s1".into(), "s2".into()]),
                artwork: true,
                lyrics: true,
                dry_run: false,
                overwrite: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(fetcher.art_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetcher.lyrics_calls.load(Ordering::SeqCst), 2);
        assert_eq!(report.artwork_files_written, 1);
        assert_eq!(report.lyrics_files_written, 2);

        let dir = lib.root.join("music/alice/A");
        assert_eq!(
            std::fs::read(dir.join("cover.jpg")).unwrap(),
            vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("x.lrc")).unwrap(),
            "[00:01.00] O Holy Night"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("y.lrc")).unwrap(),
            "[00:01.00] O Holy Night"
        );

        for entry in &report.entries {
            let ok = entry.outcome.as_ref().unwrap();
            assert_eq!(ok.artwork.as_ref().unwrap().status, "written");
            assert_eq!(ok.lyrics.as_ref().unwrap().status, "written");
            assert_eq!(ok.lyrics.as_ref().unwrap().synced, Some(true));
        }
    }

    #[tokio::test]
    async fn skips_existing_when_overwrite_false_and_replaces_when_overwrite_true() {
        let lib = Library::new().await;
        let dir = lib.root.join("music/alice/A");
        std::fs::write(dir.join("cover.jpg"), b"old-art").unwrap();
        std::fs::write(dir.join("x.lrc"), b"old-lyrics").unwrap();

        let (acq, _) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));
        let fetcher = FakeFetcher::with_both();

        // overwrite: false -> already_present, zero fetcher calls, files untouched.
        let skipped = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s1".into()]),
                artwork: true,
                lyrics: true,
                dry_run: false,
                overwrite: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(fetcher.art_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fetcher.lyrics_calls.load(Ordering::SeqCst), 0);
        assert_eq!(skipped.artwork_files_written, 0);
        assert_eq!(skipped.lyrics_files_written, 0);
        let ok = skipped.entries[0].outcome.as_ref().unwrap();
        assert_eq!(ok.artwork.as_ref().unwrap().status, "already_present");
        assert_eq!(ok.lyrics.as_ref().unwrap().status, "already_present");
        assert_eq!(std::fs::read(dir.join("cover.jpg")).unwrap(), b"old-art");
        assert_eq!(std::fs::read(dir.join("x.lrc")).unwrap(), b"old-lyrics");

        // overwrite: true -> fetches and replaces both files.
        let overwritten = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s1".into()]),
                artwork: true,
                lyrics: true,
                dry_run: false,
                overwrite: true,
            },
        )
        .await
        .unwrap();

        assert_eq!(fetcher.art_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetcher.lyrics_calls.load(Ordering::SeqCst), 1);
        assert_eq!(overwritten.artwork_files_written, 1);
        assert_eq!(overwritten.lyrics_files_written, 1);
        assert_eq!(
            std::fs::read(dir.join("cover.jpg")).unwrap(),
            vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("x.lrc")).unwrap(),
            "[00:01.00] O Holy Night"
        );
    }

    #[tokio::test]
    async fn dry_run_reports_would_write_and_touches_nothing() {
        let lib = Library::new().await;
        let dir = lib.root.join("music/alice/A");
        let (acq, _) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));
        let fetcher = FakeFetcher::with_both();

        let report = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s1".into()]),
                artwork: true,
                lyrics: true,
                dry_run: true,
                overwrite: false,
            },
        )
        .await
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.artwork_files_written, 0);
        assert_eq!(report.lyrics_files_written, 0);
        assert!(report.scan_status.is_none());
        let ok = report.entries[0].outcome.as_ref().unwrap();
        assert_eq!(ok.artwork.as_ref().unwrap().status, "would_write");
        assert_eq!(ok.artwork.as_ref().unwrap().bytes, Some(7));
        assert_eq!(ok.lyrics.as_ref().unwrap().status, "would_write");
        assert_eq!(ok.lyrics.as_ref().unwrap().synced, Some(true));
        assert!(!dir.join("cover.jpg").exists());
        assert!(!dir.join("x.lrc").exists());
    }

    #[tokio::test]
    async fn refuses_files_outside_callers_owned_folders() {
        let lib = Library::new().await;
        lib.song("s-bob", "bob/B/y.flac", b"audio");
        let (acq, _) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));
        let fetcher = FakeFetcher::with_both();

        let report = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s-bob".into()]),
                artwork: true,
                lyrics: true,
                dry_run: false,
                overwrite: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(fetcher.art_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fetcher.lyrics_calls.load(Ordering::SeqCst), 0);
        let err = report.entries[0].outcome.as_ref().unwrap_err();
        assert_eq!(err.code, FailureCode::DeleteOutsideLibrary);
        assert!(!lib.root.join("music/bob/B/cover.jpg").exists());
        assert!(!lib.root.join("music/bob/B/y.lrc").exists());
    }

    #[tokio::test]
    async fn refuses_request_with_both_artwork_and_lyrics_disabled() {
        let lib = Library::new().await;
        let (acq, _) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));
        let fetcher = FakeFetcher::with_both();

        let err = fetch_artwork_and_lyrics(
            &lib.guard(),
            &navidrome,
            &acq.queue,
            &test_ctx("alice"),
            &fetcher,
            ExtrasRequest {
                selection: TrackSelection::Songs(vec!["s1".into()]),
                artwork: false,
                lyrics: false,
                dry_run: true,
                overwrite: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, FailureCode::InvalidInput);
    }

    #[test]
    fn detects_jpeg_and_png_magic_bytes() {
        assert_eq!(detect_image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(detect_image_ext(&[0x89, b'P', b'N', b'G', 0x0D]), Some("png"));
        assert_eq!(detect_image_ext(b"GIF89a"), None);
    }

    #[test]
    fn parse_lrc_handles_multi_timestamps_centiseconds_milliseconds_and_metadata_tags() {
        let raw = "\u{feff}[ar:Marrow Choir]\n\
                   [ti:O Holy Night]\n\
                   [al:Winter Songs]\n\
                   [offset:-250]\n\
                   [00:12.34]First verse line\n\
                   [00:25.123]Millisecond precision\n\
                   [00:40.50][01:20.00]Repeated chorus\n\
                   [01:05:20]Colon fractional line\n";
        let parsed = parse_lrc(raw, true, Some("Marrow Choir".into()), Some("O Holy Night".into()));
        assert!(parsed.synced);
        assert_eq!(parsed.offset_ms, Some(-250));
        assert_eq!(parsed.display_artist.as_deref(), Some("Marrow Choir"));
        assert_eq!(parsed.display_title.as_deref(), Some("O Holy Night"));
        assert_eq!(
            parsed.lines,
            vec![
                TrackLyricLine { start_ms: Some(12_340), text: "First verse line".into() },
                TrackLyricLine { start_ms: Some(25_123), text: "Millisecond precision".into() },
                TrackLyricLine { start_ms: Some(40_500), text: "Repeated chorus".into() },
                TrackLyricLine { start_ms: Some(65_200), text: "Colon fractional line".into() },
                TrackLyricLine { start_ms: Some(80_000), text: "Repeated chorus".into() },
            ]
        );
    }

    #[test]
    fn parse_lrc_falls_back_to_plain_lines_when_unsynced_or_untagged() {
        let raw = "[ar:Air]\n[ti:Playground Love]\n\nI'm a high school lover\nAnd you're my favorite flavor\n\n";
        let parsed = parse_lrc(raw, true, Some("Air".into()), Some("Playground Love".into()));
        assert!(!parsed.synced);
        assert_eq!(
            parsed.lines,
            vec![
                TrackLyricLine { start_ms: None, text: "I'm a high school lover".into() },
                TrackLyricLine { start_ms: None, text: "And you're my favorite flavor".into() },
            ]
        );

        let plain = parse_lrc("Line one\nLine two", false, None, None);
        assert!(!plain.synced);
        assert_eq!(plain.lines.len(), 2);
        assert_eq!(plain.lines[0].start_ms, None);
    }

    #[tokio::test]
    async fn lrclib_falls_back_to_search_on_404_and_prefers_synced_closest_duration() {
        use axum::{routing::get, Json, Router};

        let app = Router::new()
            .route(
                "/api/get",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/search",
                get(|| async {
                    Json(serde_json::json!([
                        {
                            "artistName": "Totally Different Band",
                            "trackName": "Unrelated Track",
                            "duration": 210.0,
                            "instrumental": false,
                            "syncedLyrics": "[00:01.00] Wrong song"
                        },
                        {
                            "artistName": "Air",
                            "trackName": "Playground Love",
                            "albumName": "The Virgin Suicides",
                            "duration": 213.0,
                            "instrumental": false,
                            "plainLyrics": "Plain version only"
                        },
                        {
                            "artistName": "Air",
                            "trackName": "Playground Love",
                            "albumName": "The Virgin Suicides (Live Extended)",
                            "duration": 280.0,
                            "instrumental": false,
                            "syncedLyrics": "[00:01.00] Too far duration"
                        },
                        {
                            "artistName": "Air",
                            "trackName": "Playground Love",
                            "albumName": "The Virgin Suicides",
                            "duration": 212.0,
                            "instrumental": false,
                            "syncedLyrics": "[00:10.00] I'm a high school lover"
                        }
                    ]))
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = format!("http://{addr}");
        let fetcher = LiveExtrasFetcher::with_bases(base.clone(), base.clone(), base);
        let found = fetcher
            .fetch_lyrics("Air", "Playground Love", Some("The Virgin Suicides"), Some(213))
            .await
            .unwrap()
            .expect("search fallback should find synced lyrics");
        assert!(found.synced);
        assert_eq!(found.content, "[00:10.00] I'm a high school lover");
    }

    #[test]
    fn parse_lrc_handles_spaces_between_timestamps_and_rejects_overflow() {
        assert_eq!(parse_lrc_timestamp_ms("99999999999999999999:00.00"), None);
        assert_eq!(parse_lrc_timestamp_ms("999999999999999:00.00"), None);
        let parsed = parse_lrc(
            "[00:10.00] [00:20.00]Hello world",
            true,
            None,
            None,
        );
        assert!(parsed.synced);
        assert_eq!(
            parsed.lines,
            vec![
                TrackLyricLine { start_ms: Some(10_000), text: "Hello world".into() },
                TrackLyricLine { start_ms: Some(20_000), text: "Hello world".into() },
            ]
        );
    }

    #[test]
    fn lrclib_exact_title_plain_beats_wrong_title_synced() {
        let items = vec![
            LrcLibSearchItem {
                track_name: "Shine On Part 2".into(),
                artist_name: "Pink Floyd".into(),
                album_name: None,
                duration: Some(200.0),
                instrumental: false,
                synced_lyrics: Some("[00:01.00] Wrong part".into()),
                plain_lyrics: None,
            },
            LrcLibSearchItem {
                track_name: "Shine On Part 1".into(),
                artist_name: "Pink Floyd".into(),
                album_name: None,
                duration: Some(200.0),
                instrumental: false,
                synced_lyrics: None,
                plain_lyrics: Some("Exact part 1 lyrics".into()),
            },
        ];
        let picked = pick_best_lrclib_result(&items, "Pink Floyd", "Shine On Part 1", None, Some(200))
            .expect("should pick exact title");
        assert!(!picked.synced);
        assert_eq!(picked.content, "Exact part 1 lyrics");
    }
}
