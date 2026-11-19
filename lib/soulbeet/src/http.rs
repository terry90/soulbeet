use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, Response};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::error::SoulseekError;

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
                // Without the URL: a Last.fm request's query holds its API key.
                let e = e.without_url();
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

const MB_INTERVAL: Duration = Duration::from_millis(1100);
const LFM_INTERVAL: Duration = Duration::from_millis(1000);

pub async fn mb_rate_limit() {
    MB_LIMITER.wait(MB_INTERVAL).await;
}

pub async fn lastfm_rate_limit() {
    LFM_LIMITER.wait(LFM_INTERVAL).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_that_got_no_answer_does_not_name_its_url() {
        // Port 1 on loopback refuses the connection at once.
        let url =
            "http://127.0.0.1:1/2.0/?method=track.search&api_key=3f2b9c1e8d7a6b5c&format=json";
        let client = build_client("Soulbeet test");
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
