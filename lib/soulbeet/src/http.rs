use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::Client;
use tokio::sync::Mutex;
use tracing::info;

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

const MB_INTERVAL: Duration = Duration::from_millis(1100);

pub async fn mb_rate_limit() {
    MB_LIMITER.wait(MB_INTERVAL).await;
}
