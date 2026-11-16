use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, Response};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::{Result, SoulseekError};

// --- Circuit Breaker ---

#[derive(Debug)]
struct CircuitBreakerState {
    failure_count: u64,
    last_failure_time: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<CircuitBreakerState>,
    failure_threshold: u64,
    reset_timeout: chrono::Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u64, reset_timeout_secs: i64) -> Self {
        Self {
            state: Mutex::new(CircuitBreakerState {
                failure_count: 0,
                last_failure_time: None,
            }),
            failure_threshold,
            reset_timeout: chrono::Duration::seconds(reset_timeout_secs),
        }
    }

    pub async fn is_open(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.failure_count < self.failure_threshold {
            return false;
        }
        if let Some(last_time) = state.last_failure_time {
            if Utc::now() - last_time > self.reset_timeout {
                state.failure_count = 0;
                state.last_failure_time = None;
                return false;
            }
        }
        true
    }

    pub async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.failure_count = 0;
    }

    pub async fn record_failure(&self) {
        let mut state = self.state.lock().await;
        state.failure_count += 1;
        state.last_failure_time = Some(Utc::now());
    }

    pub async fn failure_count(&self) -> u64 {
        self.state.lock().await.failure_count
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, 60)
    }
}

// --- Docker URL resolution ---

/// Resolve localhost URLs to host.docker.internal when running inside Docker.
pub fn resolve_docker_url(url: &str) -> String {
    let mut resolved = url.to_string();
    if Path::new("/.dockerenv").exists() && resolved.contains("localhost") {
        resolved = resolved.replace("localhost", "host.docker.internal");
        info!("Docker detected, rewriting URL to {}", resolved);
    }
    resolved
}

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 5000;

/// Status codes that warrant a retry (server-side transient errors).
fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Why `resilient_send` got no successful answer: `status` and `body` are the
/// last answer's, or no status and the transport error's text, without the
/// URL, when the last attempt got no answer.
#[derive(Debug)]
pub struct SendError {
    pub context: String,
    pub status: Option<u16>,
    pub body: String,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            None => write!(f, "{}: {}", self.context, self.body),
            Some(status) => write!(f, "{} ({}): {}", self.context, status, self.body),
        }
    }
}

impl SendError {
    /// A request of `context` that got no answer, or whose answer could not
    /// be read whole. reqwest's text ends with the URL (reqwest 0.12.28
    /// `src/error.rs:266-268`), whose query can hold an API key, so it is
    /// left out.
    pub fn unanswered(context: &str, e: reqwest::Error) -> SendError {
        SendError {
            context: context.to_string(),
            status: None,
            body: e.without_url().to_string(),
        }
    }
}

impl From<SendError> for SoulseekError {
    fn from(e: SendError) -> Self {
        SoulseekError::Api {
            status: e.status.unwrap_or(0),
            message: e.to_string(),
        }
    }
}

/// Execute an HTTP request with retries and exponential backoff.
///
/// Retries on network errors, 429, 500, 502, 503, 504.
/// Does NOT retry on 4xx client errors (except 429).
pub async fn resilient_send(
    build_request: impl Fn() -> RequestBuilder,
    context: &str,
) -> std::result::Result<Response, SendError> {
    let failed = |status, body| SendError {
        context: context.to_string(),
        status,
        body,
    };
    let mut attempt = 0;
    loop {
        if attempt > 0 {
            let delay = (BASE_DELAY_MS * 2u64.pow(attempt - 1)).min(MAX_DELAY_MS);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match build_request().send().await {
            Err(e) => {
                warn!("{}: attempt {} network error: {}", context, attempt + 1, e);
                // Timeouts indicate the server is hanging, not a transient blip.
                // Retry once in case it was a one-off, but don't keep waiting.
                if attempt == MAX_RETRIES || (e.is_timeout() && attempt >= 1) {
                    return Err(SendError::unanswered(context, e));
                }
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if resp.status().is_success() || status == 204 {
                    return Ok(resp);
                }
                if !is_retryable(status) || attempt == MAX_RETRIES {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(failed(Some(status), body));
                }
                warn!(
                    "{}: attempt {} got {}, retrying",
                    context,
                    attempt + 1,
                    status
                );
            }
        }
        attempt += 1;
    }
}

/// Build a reqwest Client with standard timeouts.
pub fn build_client(user_agent: &str) -> Client {
    Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("failed to build HTTP client")
}

// --- Per-service rate limiters ---
//
// Each service has its own limiter so they don't block each other.
// The interval is the minimum time between requests to that service.

struct RateLimiter(Mutex<Instant>);

impl RateLimiter {
    fn new() -> Self {
        Self(Mutex::new(Instant::now() - Duration::from_secs(2)))
    }

    async fn wait(&self, min_interval: Duration) {
        let mut last = self.0.lock().await;
        let elapsed = last.elapsed();
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }
        *last = Instant::now();
    }
}

// MusicBrainz: 1 req/sec (documented hard limit, 503 if exceeded)
static MB_LIMITER: LazyLock<RateLimiter> = LazyLock::new(RateLimiter::new);
// Last.fm: 1 req/sec (undocumented, matching MB for safety)
static LFM_LIMITER: LazyLock<RateLimiter> = LazyLock::new(RateLimiter::new);
// ListenBrainz: ~2 req/sec (uses response headers, but we preemptively limit)
static LB_LIMITER: LazyLock<RateLimiter> = LazyLock::new(RateLimiter::new);

const MB_INTERVAL: Duration = Duration::from_millis(1100);
const LFM_INTERVAL: Duration = Duration::from_millis(1000);
const LB_INTERVAL: Duration = Duration::from_millis(500);

pub async fn mb_rate_limit() {
    MB_LIMITER.wait(MB_INTERVAL).await;
}

pub async fn lastfm_rate_limit() {
    LFM_LIMITER.wait(LFM_INTERVAL).await;
}

pub async fn lb_rate_limit() {
    LB_LIMITER.wait(LB_INTERVAL).await;
}

// --- MBID cache (avoids repeated MusicBrainz lookups for the same artist) ---

static MBID_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Look up an artist MBID, checking the cache first.
/// Returns None if not found or if MusicBrainz doesn't have a match.
pub async fn cached_mbid_lookup(client: &Client, artist: &str) -> Result<Option<String>> {
    let key = artist.to_lowercase();

    // Check cache
    {
        let cache = MBID_CACHE.lock().await;
        if let Some(cached) = cache.get(&key) {
            return Ok(cached.clone());
        }
    }

    // Rate limit, then fetch
    mb_rate_limit().await;

    // Wrap in double quotes to escape Lucene special chars (AC/DC, Guns N' Roses, etc.)
    let quoted = format!("\"{}\"", artist.replace('"', "\\\""));
    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query=artist:{}&fmt=json&limit=1",
        url::form_urlencoded::byte_serialize(quoted.as_bytes()).collect::<String>()
    );

    let client_clone = client.clone();
    let url_clone = url.clone();
    let resp = match resilient_send(
        || client_clone.get(&url_clone),
        &format!("MB lookup {}", artist),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("MusicBrainz lookup failed for '{}': {}", artist, e);
            // Cache the failure so we don't retry immediately
            MBID_CACHE.lock().await.insert(key, None);
            return Ok(None);
        }
    };

    if !resp.status().is_success() {
        MBID_CACHE.lock().await.insert(key, None);
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct MbResponse {
        #[serde(default)]
        artists: Vec<MbArtist>,
    }
    #[derive(serde::Deserialize)]
    struct MbArtist {
        id: String,
        #[serde(default)]
        score: Option<u32>,
    }

    let data: MbResponse = resp.json().await.map_err(|e| SoulseekError::Api {
        status: 500,
        message: format!("Failed to parse MusicBrainz response: {}", e),
    })?;

    let result = data.artists.into_iter().next().and_then(|a| {
        if a.score.unwrap_or(0) >= 90 {
            Some(a.id)
        } else {
            None
        }
    });

    if result.is_some() {
        MBID_CACHE.lock().await.insert(key, result.clone());
        return Ok(result);
    }

    // Full name didn't match. Try the primary artist from compound names
    // like "A, B, C" or "A featuring B" or "A & B".
    if let Some(primary) = extract_primary_artist(artist) {
        let primary_key = primary.to_lowercase();
        // Check if we already have the primary cached
        {
            let cache = MBID_CACHE.lock().await;
            if let Some(cached) = cache.get(&primary_key) {
                let result = cached.clone();
                // Also cache under the full compound name
                drop(cache);
                MBID_CACHE.lock().await.insert(key, result.clone());
                return Ok(result);
            }
        }

        mb_rate_limit().await;
        let quoted = format!("\"{}\"", primary.replace('"', "\\\""));
        let url = format!(
            "https://musicbrainz.org/ws/2/artist/?query=artist:{}&fmt=json&limit=1",
            url::form_urlencoded::byte_serialize(quoted.as_bytes()).collect::<String>()
        );

        let client_clone = client.clone();
        let url_clone = url.clone();
        if let Ok(resp) = resilient_send(
            || client_clone.get(&url_clone),
            &format!("MB lookup (primary) {}", primary),
        )
        .await
        {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<MbResponse>().await {
                    let fallback = data.artists.into_iter().next().and_then(|a| {
                        if a.score.unwrap_or(0) >= 90 {
                            Some(a.id)
                        } else {
                            None
                        }
                    });
                    if fallback.is_some() {
                        MBID_CACHE
                            .lock()
                            .await
                            .insert(primary_key, fallback.clone());
                        MBID_CACHE.lock().await.insert(key, fallback.clone());
                        return Ok(fallback);
                    }
                }
            }
        }
    }

    // Cache the miss
    MBID_CACHE.lock().await.insert(key, None);
    Ok(None)
}

/// Extract the primary artist from compound names.
/// Returns None if the name doesn't look compound.
fn extract_primary_artist(artist: &str) -> Option<&str> {
    // Order matters: check longer patterns first
    let separators = [
        " featuring ",
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
        ", ",
        " & ",
        " x ",
    ];
    for sep in separators {
        if let Some(idx) = artist.to_lowercase().find(sep) {
            let primary = artist[..idx].trim();
            if !primary.is_empty() && primary != artist {
                return Some(primary);
            }
        }
    }
    None
}

// --- Recording metadata cache (resolves MBIDs to artist + track + year) ---

#[derive(Clone, Debug)]
pub struct RecordingInfo {
    pub artist: String,
    pub title: String,
    pub release_year: Option<u16>,
}

static RECORDING_CACHE: LazyLock<Mutex<HashMap<String, Option<RecordingInfo>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Look up a recording by MBID, returning artist name, track title, and earliest release year.
pub async fn cached_recording_lookup(client: &Client, mbid: &str) -> Result<Option<RecordingInfo>> {
    {
        let cache = RECORDING_CACHE.lock().await;
        if let Some(cached) = cache.get(mbid) {
            return Ok(cached.clone());
        }
    }

    mb_rate_limit().await;

    let url = format!(
        "https://musicbrainz.org/ws/2/recording/{}?inc=artist-credits+releases&fmt=json",
        mbid
    );

    let client_clone = client.clone();
    let url_clone = url.clone();
    let resp = match resilient_send(
        || client_clone.get(&url_clone),
        &format!("MB recording {}", mbid),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!("MusicBrainz recording lookup failed for '{}': {}", mbid, e);
            RECORDING_CACHE.lock().await.insert(mbid.to_string(), None);
            return Ok(None);
        }
    };

    if !resp.status().is_success() {
        RECORDING_CACHE.lock().await.insert(mbid.to_string(), None);
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct MbRecording {
        #[serde(default)]
        title: String,
        #[serde(default, rename = "artist-credit")]
        artist_credit: Vec<MbArtistCredit>,
        #[serde(default)]
        releases: Vec<MbRelease>,
    }
    #[derive(serde::Deserialize)]
    struct MbArtistCredit {
        artist: MbCreditArtist,
        #[serde(default)]
        joinphrase: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct MbCreditArtist {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct MbRelease {
        #[serde(default)]
        date: Option<String>,
    }

    let data: MbRecording = match resp.json().await {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to parse MB recording response: {}", e);
            RECORDING_CACHE.lock().await.insert(mbid.to_string(), None);
            return Ok(None);
        }
    };

    if data.title.is_empty() || data.artist_credit.is_empty() {
        RECORDING_CACHE.lock().await.insert(mbid.to_string(), None);
        return Ok(None);
    }

    // Build full artist name from credits (e.g. "Artist A feat. Artist B")
    let artist = data
        .artist_credit
        .iter()
        .enumerate()
        .map(|(i, ac)| {
            let mut s = ac.artist.name.clone();
            if let Some(ref jp) = ac.joinphrase {
                s.push_str(jp);
            } else if i < data.artist_credit.len() - 1 {
                s.push_str(", ");
            }
            s
        })
        .collect::<String>();

    // Find earliest release year
    let release_year = data
        .releases
        .iter()
        .filter_map(|r| r.date.as_ref())
        .filter_map(|d| d.split('-').next()?.parse::<u16>().ok())
        .min();

    let info = RecordingInfo {
        artist,
        title: data.title,
        release_year,
    };

    RECORDING_CACHE
        .lock()
        .await
        .insert(mbid.to_string(), Some(info.clone()));
    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_that_got_no_answer_does_not_name_its_url() {
        // Port 1 on loopback refuses the connection at once.
        let url =
            "http://127.0.0.1:1/2.0/?method=track.search&api_key=3f2b9c1e8d7a6b5c&format=json";
        let client = build_client("Soulful test");
        let e = resilient_send(|| client.get(url), "Last.fm API")
            .await
            .unwrap_err();
        assert_eq!((e.status, e.context.as_str()), (None, "Last.fm API"));
        assert!(!e.body.contains("3f2b9c1e8d7a6b5c"), "{}", e.body);
        // reqwest's text for a refused connection, without " for url (...)".
        assert_eq!(e.body, "error sending request");
    }

    #[test]
    fn send_errors_read_as_before() {
        let refused = SoulseekError::from(SendError {
            context: "Last.fm API".into(),
            status: Some(400),
            body: "b".into(),
        });
        assert_eq!(refused.to_string(), "API error: 400 - Last.fm API (400): b");
        let unsent = SoulseekError::from(SendError {
            context: "Last.fm API".into(),
            status: None,
            body: "error sending request".into(),
        });
        assert_eq!(
            unsent.to_string(),
            "API error: 0 - Last.fm API: error sending request"
        );
    }
}
