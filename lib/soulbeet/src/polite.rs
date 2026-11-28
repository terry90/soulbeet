//! One paced HTTP client for ListenBrainz, ListenBrainz Labs, Deezer and archive.org. It
//! spaces requests per host, waits out rate limits (a 429 or 503 holds every request to the
//! host, `HostGate`), retries a bot challenge page like an outage and never hands one to a
//! caller as data.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::traits::clip;

/// How much of a refusal's body an error keeps: it is stored in run reports,
/// shown on the Discovery page and logged, and can be a whole HTML page.
const BODY_CHARS: usize = 200;

pub const USER_AGENT: &str = concat!(
    "Soulbeet/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/terry90/soulbeet)"
);

#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub min_interval: Duration,
    pub backoff: Duration,
    pub retries: u32,
    pub timeout: Duration,
}

impl Pace {
    pub const METABRAINZ: Pace = Pace {
        min_interval: Duration::from_millis(1000),
        backoff: Duration::from_millis(1000),
        retries: 3,
        timeout: Duration::from_secs(60),
    };

    pub const DEEZER: Pace = Pace {
        min_interval: Duration::from_millis(250),
        backoff: Duration::from_millis(5000),
        retries: 3,
        timeout: Duration::from_secs(15),
    };

    /// Fast interactive pace for user-facing Deezer search so MusicBrainz
    /// fallback happens promptly on quota or upstream errors instead of stalling.
    pub const DEEZER_INTERACTIVE: Pace = Pace {
        min_interval: Duration::from_millis(250),
        backoff: Duration::from_secs(1),
        retries: 1,
        timeout: Duration::from_secs(6),
    };

    /// archive.org publishes no limit and blocks heavy automated traffic, so one request a
    /// second, shared by every user's searches.
    pub const ARCHIVE: Pace = Pace {
        min_interval: Duration::from_millis(1000),
        backoff: Duration::from_millis(5000),
        retries: 3,
        timeout: Duration::from_secs(30),
    };

    #[cfg(test)]
    pub(crate) const fn immediate() -> Pace {
        Pace {
            min_interval: Duration::ZERO,
            backoff: Duration::ZERO,
            retries: 3,
            timeout: Duration::from_secs(5),
        }
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
pub enum RemoteError {
    #[error("{service} is unavailable: {detail}")]
    Unavailable {
        service: &'static str,
        detail: String,
    },
    #[error("{service} is rate limiting requests")]
    RateLimited { service: &'static str },
    #[error("{service} answered with a browser check")]
    Challenged { service: &'static str },
    #[error("{service} answered HTTP {status}: {body}")]
    Status {
        service: &'static str,
        status: u16,
        body: String,
    },
    #[error("{service} sent an unreadable answer: {detail}")]
    Unreadable {
        service: &'static str,
        detail: String,
    },
}

impl RemoteError {
    pub fn service(&self) -> &'static str {
        match self {
            Self::Unavailable { service, .. }
            | Self::RateLimited { service }
            | Self::Challenged { service }
            | Self::Status { service, .. }
            | Self::Unreadable { service, .. } => service,
        }
    }
}

/// The pace every client of one host keeps: requests take their turn one at a time, spaced by
/// their pace, and a hold (an exhausted rate limit, a 429 or 503) holds every request to the
/// host until it is over.
pub struct HostGate {
    /// Taken in the order requests come, while each waits for its turn.
    turn: tokio::sync::Mutex<()>,
    /// No request goes out before this.
    next: std::sync::Mutex<Instant>,
}

/// One gate per host:port so every client of a host shares its pacing.
static GATES: LazyLock<std::sync::Mutex<HashMap<String, Arc<HostGate>>>> =
    LazyLock::new(Default::default);

impl HostGate {
    /// The gate of the host `base_url` names, shared by every client of it in the process.
    pub fn of(base_url: &str) -> Result<Arc<HostGate>, String> {
        let parsed = url::Url::parse(base_url).map_err(|e| e.to_string())?;
        let host = parsed
            .host_str()
            .ok_or_else(|| format!("no host in {base_url}"))?;
        let key = match parsed.port_or_known_default() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let mut gates = GATES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = gates.entry(key).or_insert_with(|| {
            Arc::new(HostGate {
                turn: tokio::sync::Mutex::new(()),
                next: std::sync::Mutex::new(Instant::now()),
            })
        });
        Ok(gate.clone())
    }

    /// Waits until a request may go out, then spaces the next one `interval` after it. A hold
    /// that grows meanwhile is waited out too.
    pub async fn wait_for_turn(&self, interval: Duration) {
        let _turn = self.turn.lock().await;
        loop {
            let until = {
                let mut next = self
                    .next
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = Instant::now();
                if *next <= now {
                    *next = now + interval;
                    return;
                }
                *next
            };
            tokio::time::sleep_until(until.into()).await;
        }
    }

    /// Holds every request to the host for `wait` from now, unless a longer hold is on.
    pub fn hold(&self, wait: Duration) {
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next = (*next).max(Instant::now() + wait);
    }
}

/// A whole-seconds header value, if present and numeric.
fn header_secs(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Calls in a row that end unavailable, rate limited or challenged after their retries before
/// the circuit breaker opens.
const FAILED_CALLS_BEFORE_GIVING_UP: u32 = 2;

/// How long an open circuit breaker waits before allowing a half-open probe.
const DEFAULT_CIRCUIT_COOLDOWN: Duration = Duration::from_secs(60);

/// Time-bounded circuit breaker state: tracks consecutive transient failures, when the
/// breaker opened, and whether a single half-open probe is currently in flight.
#[derive(Default)]
struct Streak {
    calls: u32,
    last: Option<RemoteError>,
    opened_at: Option<Instant>,
    probing: bool,
}

/// Releases `Streak::probing` if a half-open probe future is dropped or panics before
/// recording its outcome.
struct ProbeGuard<'a> {
    failed: &'a std::sync::Mutex<Streak>,
    armed: bool,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut failed = self
                .failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            failed.probing = false;
        }
    }
}

/// A paced HTTP client with a time-bounded circuit breaker: after consecutive failures it
/// fails fast for `cooldown`, then permits a half-open probe to recover automatically.
pub struct PoliteClient {
    http: Client,
    base: String,
    service: &'static str,
    pace: Pace,
    gate: Arc<HostGate>,
    token: Option<String>,
    is_quota: Option<fn(&[u8]) -> bool>,
    cooldown: Duration,
    failed: std::sync::Mutex<Streak>,
}

impl PoliteClient {
    pub fn new(
        base_url: &str,
        service: &'static str,
        pace: Pace,
    ) -> Result<PoliteClient, RemoteError> {
        Self::build(base_url, service, pace, Policy::default())
    }

    /// A client that follows no redirect: a 3xx answer is a `RemoteError::Status`.
    pub fn without_redirects(
        base_url: &str,
        service: &'static str,
        pace: Pace,
    ) -> Result<PoliteClient, RemoteError> {
        Self::build(base_url, service, pace, Policy::none())
    }

    fn build(
        base_url: &str,
        service: &'static str,
        pace: Pace,
        redirects: Policy,
    ) -> Result<PoliteClient, RemoteError> {
        let unavailable = |detail: String| RemoteError::Unavailable { service, detail };
        let base = base_url.trim_end_matches('/').to_string();
        let gate = HostGate::of(&base).map_err(unavailable)?;
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(pace.timeout)
            .connect_timeout(Duration::from_secs(5))
            .redirect(redirects)
            .build()
            .map_err(|e| unavailable(e.without_url().to_string()))?;
        Ok(PoliteClient {
            http,
            base,
            service,
            pace,
            gate,
            token: None,
            is_quota: None,
            cooldown: DEFAULT_CIRCUIT_COOLDOWN,
            failed: Default::default(),
        })
    }

    pub fn with_token(mut self, token: Option<String>) -> PoliteClient {
        self.token = token;
        self
    }

    /// For services that report a quota overrun inside a 200 answer.
    pub fn with_quota_check(mut self, is_quota: fn(&[u8]) -> bool) -> PoliteClient {
        self.is_quota = Some(is_quota);
        self
    }

    /// Overrides the circuit-breaker cooldown before a half-open probe is attempted.
    pub fn with_cooldown(mut self, cooldown: Duration) -> PoliteClient {
        self.cooldown = cooldown;
        self
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// None for 204.
    pub async fn get<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
    ) -> Result<Option<T>, RemoteError> {
        self.send::<(), T>(Method::GET, path_and_query, None).await
    }

    pub async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<Option<T>, RemoteError> {
        self.send(Method::POST, path, Some(body)).await
    }

    /// An exhausted limit holds every request to the host until it resets.
    fn hold_gate(&self, headers: &HeaderMap) {
        if header_secs(headers, "X-RateLimit-Remaining") == Some(0) {
            if let Some(secs) = header_secs(headers, "X-RateLimit-Reset-In") {
                self.gate.hold(Duration::from_secs(secs));
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.pace.backoff * 2u32.pow(attempt)
    }

    /// Once the service has failed `FAILED_CALLS_BEFORE_GIVING_UP` calls in a row, fails fast
    /// with the last failure for `self.cooldown`, then allows a single half-open probe that
    /// resets the breaker on success or re-opens it for another cooldown on failure.
    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>, RemoteError> {
        let mut probe_guard = {
            let mut failed = self
                .failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if failed.calls >= FAILED_CALLS_BEFORE_GIVING_UP {
                let cooldown_elapsed = failed
                    .opened_at
                    .is_some_and(|at| at.elapsed() >= self.cooldown);
                if cooldown_elapsed && !failed.probing {
                    failed.probing = true;
                    Some(ProbeGuard {
                        failed: &self.failed,
                        armed: true,
                    })
                } else if let Some(err) = failed.last.clone() {
                    return Err(err);
                } else {
                    None
                }
            } else {
                None
            }
        };

        let result = self.exchange(method, path, body).await;

        let mut failed = self
            .failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(guard) = probe_guard.as_mut() {
            guard.armed = false;
        }
        match &result {
            Err(
                e @ (RemoteError::Unavailable { .. }
                | RemoteError::RateLimited { .. }
                | RemoteError::Challenged { .. }),
            ) => {
                if probe_guard.is_some() {
                    failed.probing = false;
                }
                failed.calls = failed.calls.saturating_add(1);
                failed.last = Some(e.clone());
                if failed.calls >= FAILED_CALLS_BEFORE_GIVING_UP {
                    failed.opened_at = Some(Instant::now());
                }
            }
            _ => *failed = Streak::default(),
        }
        result
    }

    /// One call, with its retries.
    async fn exchange<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>, RemoteError> {
        let service = self.service;
        let url = format!("{}{path}", self.base);
        let mut last = RemoteError::Unavailable {
            service,
            detail: String::new(),
        };
        for attempt in 0..=self.pace.retries {
            let mut wait = self.backoff(attempt);
            self.gate.wait_for_turn(self.pace.min_interval).await;
            let mut request = self.http.request(method.clone(), &url);
            if let Some(token) = &self.token {
                request = request.header("Authorization", format!("Token {token}"));
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(e) => {
                    last = RemoteError::Unavailable {
                        service,
                        detail: e.without_url().to_string(),
                    };
                    self.pause(attempt, wait).await;
                    continue;
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            self.hold_gate(&headers);
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    last = RemoteError::Unavailable {
                        service,
                        detail: e.without_url().to_string(),
                    };
                    self.pause(attempt, wait).await;
                    continue;
                }
            };
            let quota = status.is_success() && self.is_quota.is_some_and(|f| f(&bytes));
            if status == StatusCode::TOO_MANY_REQUESTS || quota {
                if let Some(secs) = header_secs(&headers, "X-RateLimit-Reset-In") {
                    wait = Duration::from_secs(secs);
                }
                // The host asked every client to slow down, not only this call.
                self.gate.hold(wait);
                last = RemoteError::RateLimited { service };
                self.pause(attempt, wait).await;
                continue;
            }
            if status == StatusCode::SERVICE_UNAVAILABLE {
                self.gate.hold(wait);
            }
            if status.is_server_error() {
                last = RemoteError::Unavailable {
                    service,
                    detail: format!("HTTP {}", status.as_u16()),
                };
                self.pause(attempt, wait).await;
                continue;
            }
            if status == StatusCode::NO_CONTENT {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(RemoteError::Status {
                    service,
                    status: status.as_u16(),
                    body: clip(&String::from_utf8_lossy(&bytes), BODY_CHARS).to_string(),
                });
            }
            match serde_json::from_slice(&bytes) {
                Ok(value) => return Ok(Some(value)),
                // The bot challenge shows up around an outage and passes with it.
                Err(_) if bytes.trim_ascii_start().first() == Some(&b'<') => {
                    last = RemoteError::Challenged { service };
                    self.pause(attempt, wait).await;
                }
                Err(e) => {
                    return Err(RemoteError::Unreadable {
                        service,
                        detail: e.to_string(),
                    })
                }
            }
        }
        Err(last)
    }

    /// No sleep after the last attempt, there is nothing left to retry.
    async fn pause(&self, attempt: u32, wait: Duration) {
        if attempt < self.pace.retries {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, serve_with_headers, Reply};
    use std::time::{Duration, Instant};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn requests_carry_soulbeets_user_agent_and_the_token() {
        let (base, seen) = serve(vec![ok("[1]")]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate())
            .unwrap()
            .with_token(Some("tok-1".into()));
        assert_eq!(c.get::<Vec<u8>>("/1/x").await.unwrap(), Some(vec![1]));
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(sent.user_agent.as_deref(), Some(USER_AGENT));
        assert_eq!(sent.authorization.as_deref(), Some("Token tok-1"));
        assert_eq!(
            USER_AGENT,
            concat!(
                "Soulbeet/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/terry90/soulbeet)"
            )
        );
    }

    #[tokio::test]
    async fn requests_to_one_host_are_spaced_by_the_pace_across_clients() {
        let (base, _) = serve(vec![ok("[]"), ok("[]")]);
        let pace = Pace {
            min_interval: Duration::from_millis(200),
            ..Pace::immediate()
        };
        let (a, b) = (
            PoliteClient::new(&base, "ListenBrainz", pace).unwrap(),
            PoliteClient::new(&base, "ListenBrainz", pace).unwrap(),
        );
        let start = Instant::now();
        a.get::<Vec<u8>>("/a").await.unwrap();
        b.get::<Vec<u8>>("/b").await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn an_exhausted_rate_limit_waits_for_its_reset() {
        let (base, _) = serve_with_headers(vec![
            (
                ok("[]"),
                vec![
                    ("X-RateLimit-Remaining", "0".into()),
                    ("X-RateLimit-Reset-In", "1".into()),
                ],
            ),
            (ok("[]"), vec![]),
        ]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        let start = Instant::now();
        c.get::<Vec<u8>>("/a").await.unwrap();
        c.get::<Vec<u8>>("/b").await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_429_waits_for_its_reset_and_retries() {
        let (base, seen) = serve_with_headers(vec![
            (
                Reply {
                    status: 429,
                    body: String::new(),
                },
                vec![("X-RateLimit-Reset-In", "1".into())],
            ),
            (ok("[2]"), vec![]),
        ]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        let start = Instant::now();
        assert_eq!(c.get::<Vec<u8>>("/a").await.unwrap(), Some(vec![2]));
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_429_holds_every_client_of_the_host() {
        let (base, seen) = serve(vec![
            Reply {
                status: 429,
                body: String::new(),
            },
            ok("[1]"),
            ok("[2]"),
        ]);
        let pace = Pace {
            backoff: Duration::from_millis(300),
            retries: 1,
            ..Pace::immediate()
        };
        let (a, b) = (
            PoliteClient::new(&base, "Deezer", pace).unwrap(),
            PoliteClient::new(&base, "Deezer", pace).unwrap(),
        );
        let (first, second) = tokio::join!(a.get::<Vec<u8>>("/a"), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            b.get::<Vec<u8>>("/b").await
        });
        assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
        let seen = seen.lock().unwrap();
        for later in &seen[1..] {
            assert!(
                later.at.duration_since(seen[0].at) >= Duration::from_millis(300),
                "{} waited for the hold",
                later.target
            );
        }
    }

    #[tokio::test]
    async fn a_hold_that_grows_while_a_request_waits_is_waited_out() {
        let gate = HostGate::of("http://hold.invalid:1").unwrap();
        let start = Instant::now();
        gate.hold(Duration::from_millis(100));
        let waiting = tokio::spawn({
            let gate = gate.clone();
            async move { gate.wait_for_turn(Duration::ZERO).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.hold(Duration::from_millis(250));
        waiting.await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn server_errors_are_retried_then_reported() {
        let (base, seen) = serve(
            (0..4)
                .map(|_| Reply {
                    status: 503,
                    body: "down".into(),
                })
                .collect(),
        );
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert!(matches!(
            c.get::<Vec<u8>>("/a").await,
            Err(RemoteError::Unavailable {
                service: "ListenBrainz",
                ..
            })
        ));
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    const CHALLENGE: &str = include_str!("listenbrainz/fixtures/challenge.html");

    #[tokio::test]
    async fn a_browser_check_is_retried_like_an_outage() {
        let (base, seen) = serve(vec![ok(CHALLENGE), ok("[4]")]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert_eq!(
            c.get::<Vec<u8>>("/1/lb-radio/tags").await.unwrap(),
            Some(vec![4])
        );
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_browser_check_that_outlasts_the_retries_is_not_data() {
        let (base, seen) = serve((0..4).map(|_| ok(CHALLENGE)).collect());
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert_eq!(
            c.get::<Vec<u8>>("/1/lb-radio/tags").await,
            Err(RemoteError::Challenged {
                service: "ListenBrainz"
            })
        );
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    fn bad_gateway() -> Reply {
        Reply {
            status: 502,
            body: String::new(),
        }
    }

    #[tokio::test]
    async fn after_two_calls_in_a_row_fail_the_client_stops_asking() {
        let (base, seen) = serve((0..8).map(|_| bad_gateway()).collect());
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        let down = Err(RemoteError::Unavailable {
            service: "ListenBrainz",
            detail: "HTTP 502".into(),
        });
        assert_eq!(c.get::<Vec<u8>>("/a").await, down);
        assert_eq!(c.get::<Vec<u8>>("/b").await, down);
        assert_eq!(seen.lock().unwrap().len(), 8);
        assert_eq!(c.get::<Vec<u8>>("/c").await, down);
        assert_eq!(
            seen.lock().unwrap().len(),
            8,
            "the third call sends nothing"
        );

        // The next run builds its own client, which asks again.
        let (base, seen) = serve(vec![ok("[5]")]);
        let fresh = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert_eq!(fresh.get::<Vec<u8>>("/c").await.unwrap(), Some(vec![5]));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_answer_between_failures_starts_the_count_again() {
        let mut replies: Vec<Reply> = (0..4).map(|_| bad_gateway()).collect();
        replies.push(Reply {
            status: 404,
            body: "unknown".into(),
        });
        replies.extend((0..4).map(|_| bad_gateway()));
        replies.push(ok("[6]"));
        let (base, seen) = serve(replies);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert!(c.get::<Vec<u8>>("/a").await.is_err());
        assert!(matches!(
            c.get::<Vec<u8>>("/b").await,
            Err(RemoteError::Status { status: 404, .. })
        ));
        assert!(c.get::<Vec<u8>>("/c").await.is_err());
        assert_eq!(c.get::<Vec<u8>>("/d").await.unwrap(), Some(vec![6]));
        assert_eq!(seen.lock().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn a_quota_body_is_retried_like_a_429() {
        let quota = r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#;
        let (base, seen) = serve(vec![ok(quota), ok("[3]")]);
        let c = PoliteClient::new(&base, "Deezer", Pace::immediate())
            .unwrap()
            .with_quota_check(|b| b.windows(9).any(|w| w == br#""code":4}"#));
        assert_eq!(c.get::<Vec<u8>>("/x").await.unwrap(), Some(vec![3]));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn no_content_reads_as_none_and_other_statuses_keep_their_body() {
        let (base, _) = serve(vec![
            Reply {
                status: 204,
                body: String::new(),
            },
            Reply {
                status: 400,
                body: "bad".into(),
            },
        ]);
        let c = PoliteClient::new(&base, "ListenBrainz Labs", Pace::immediate()).unwrap();
        assert_eq!(c.get::<Vec<u8>>("/a").await.unwrap(), None);
        assert_eq!(
            c.get::<Vec<u8>>("/b").await,
            Err(RemoteError::Status {
                service: "ListenBrainz Labs",
                status: 400,
                body: "bad".into()
            })
        );
    }

    #[tokio::test]
    async fn a_refusal_keeps_only_the_start_of_a_long_body() {
        let page = "é".repeat(150) + &"x".repeat(150);
        let (base, _) = serve(vec![Reply {
            status: 404,
            body: page.clone(),
        }]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        let Err(RemoteError::Status { body, .. }) = c.get::<Vec<u8>>("/a").await else {
            panic!("expected a refusal");
        };
        assert_eq!(body, page.chars().take(200).collect::<String>());
    }

    #[test]
    fn the_paces_are_the_documented_ones() {
        assert_eq!(
            (Pace::METABRAINZ.min_interval, Pace::METABRAINZ.retries),
            (Duration::from_millis(1000), 3)
        );
        assert_eq!(
            (Pace::DEEZER.min_interval, Pace::DEEZER.backoff),
            (Duration::from_millis(250), Duration::from_millis(5000))
        );
        assert_eq!(
            (
                Pace::DEEZER_INTERACTIVE.min_interval,
                Pace::DEEZER_INTERACTIVE.backoff,
                Pace::DEEZER_INTERACTIVE.retries,
                Pace::DEEZER_INTERACTIVE.timeout
            ),
            (
                Duration::from_millis(250),
                Duration::from_secs(1),
                1,
                Duration::from_secs(6)
            )
        );
        assert_eq!(
            (
                Pace::ARCHIVE.min_interval,
                Pace::ARCHIVE.backoff,
                Pace::ARCHIVE.retries,
                Pace::ARCHIVE.timeout
            ),
            (
                Duration::from_secs(1),
                Duration::from_secs(5),
                3,
                Duration::from_secs(30)
            )
        );
    }

    #[tokio::test]
    async fn circuit_breaker_recovers_after_cooldown_on_successful_probe() {
        let mut replies: Vec<Reply> = (0..8).map(|_| bad_gateway()).collect();
        replies.push(ok("[7]"));
        replies.push(ok("[8]"));
        let (base, seen) = serve(replies);
        let c = PoliteClient::new(&base, "Deezer", Pace::immediate())
            .unwrap()
            .with_cooldown(Duration::from_millis(60));
        let down = Err(RemoteError::Unavailable {
            service: "Deezer",
            detail: "HTTP 502".into(),
        });

        // Two failed calls trip the circuit breaker.
        assert_eq!(c.get::<Vec<u8>>("/1").await, down);
        assert_eq!(c.get::<Vec<u8>>("/2").await, down);
        assert_eq!(seen.lock().unwrap().len(), 8);

        // During cooldown, requests fail fast without hitting the server.
        assert_eq!(c.get::<Vec<u8>>("/3").await, down);
        assert_eq!(seen.lock().unwrap().len(), 8);

        // After cooldown elapses, a half-open probe succeeds and resets the breaker.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(c.get::<Vec<u8>>("/4").await.unwrap(), Some(vec![7]));
        assert_eq!(c.get::<Vec<u8>>("/5").await.unwrap(), Some(vec![8]));
        assert_eq!(seen.lock().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn circuit_breaker_reopens_for_another_cooldown_if_half_open_probe_fails() {
        // 2 initial failures (4 attempts each = 8) + 1 failed probe (4 attempts) = 12
        let replies: Vec<Reply> = (0..12).map(|_| bad_gateway()).collect();
        let (base, seen) = serve(replies);
        let c = PoliteClient::new(&base, "Deezer", Pace::immediate())
            .unwrap()
            .with_cooldown(Duration::from_millis(60));

        assert!(c.get::<Vec<u8>>("/1").await.is_err());
        assert!(c.get::<Vec<u8>>("/2").await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 8);

        // Wait past cooldown; probe fails and re-opens the breaker.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(c.get::<Vec<u8>>("/probe").await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 12);

        // Immediately after the failed probe, requests are blocked again.
        assert!(c.get::<Vec<u8>>("/blocked").await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 12);
    }
}
