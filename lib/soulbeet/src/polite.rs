//! One paced HTTP client for ListenBrainz, ListenBrainz Labs and Deezer. It
//! spaces requests per host, waits out rate limits and never hands a bot
//! challenge page to a caller as data.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

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

type Gate = Arc<tokio::sync::Mutex<Instant>>;

/// One gate per host:port so every client of a host shares its pacing.
static GATES: LazyLock<std::sync::Mutex<HashMap<String, Gate>>> = LazyLock::new(Default::default);

fn gate_for(key: &str) -> Gate {
    GATES
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Instant::now())))
        .clone()
}

pub struct PoliteClient {
    http: Client,
    base: String,
    service: &'static str,
    pace: Pace,
    gate: Gate,
    token: Option<String>,
    is_quota: Option<fn(&[u8]) -> bool>,
}

impl PoliteClient {
    pub fn new(
        base_url: &str,
        service: &'static str,
        pace: Pace,
    ) -> Result<PoliteClient, RemoteError> {
        let unavailable = |detail: String| RemoteError::Unavailable { service, detail };
        let base = base_url.trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&base).map_err(|e| unavailable(e.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| unavailable(format!("no host in {base}")))?;
        let key = match parsed.port_or_known_default() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(pace.timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| unavailable(e.without_url().to_string()))?;
        Ok(PoliteClient {
            http,
            base,
            service,
            pace,
            gate: gate_for(&key),
            token: None,
            is_quota: None,
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

    async fn wait_for_turn(&self) {
        let mut next = self.gate.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep(*next - now).await;
        }
        *next = Instant::now() + self.pace.min_interval;
    }

    /// An exhausted limit holds every request to the host until it resets.
    async fn hold_gate(&self, headers: &HeaderMap) {
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if number("X-RateLimit-Remaining") == Some(0) {
            if let Some(secs) = number("X-RateLimit-Reset-In") {
                let mut next = self.gate.lock().await;
                *next = (*next).max(Instant::now() + Duration::from_secs(secs));
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.pace.backoff * 2u32.pow(attempt)
    }

    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
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
            self.wait_for_turn().await;
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
            self.hold_gate(&headers).await;
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
                if let Some(secs) = headers
                    .get("X-RateLimit-Reset-In")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                {
                    wait = Duration::from_secs(secs);
                }
                last = RemoteError::RateLimited { service };
                self.pause(attempt, wait).await;
                continue;
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
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
            return match serde_json::from_slice(&bytes) {
                Ok(value) => Ok(Some(value)),
                Err(_) if bytes.trim_ascii_start().first() == Some(&b'<') => {
                    Err(RemoteError::Challenged { service })
                }
                Err(e) => Err(RemoteError::Unreadable {
                    service,
                    detail: e.to_string(),
                }),
            };
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

    #[tokio::test]
    async fn a_browser_check_is_not_data() {
        let (base, seen) = serve(vec![ok(include_str!(
            "listenbrainz/fixtures/challenge.html"
        ))]);
        let c = PoliteClient::new(&base, "ListenBrainz", Pace::immediate()).unwrap();
        assert_eq!(
            c.get::<Vec<u8>>("/1/lb-radio/tags").await,
            Err(RemoteError::Challenged {
                service: "ListenBrainz"
            })
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
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
    }
}
