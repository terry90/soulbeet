//! The AirDC++ Web API over HTTP with Basic auth: every request carries the user name and
//! password, there is no session to keep. A 401 stops every later request with the same address,
//! user name and password, from every client of the process, until the settings are saved again,
//! so a wrong password never reaches AirDC++'s block of an address after 5 failed logins in 45 s.
//! Logs name the method, the path and the status only.

use super::error::{AirdcppError, Missing};
use crate::http::CircuitBreaker;
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tracing::{debug, warn};
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How Soulful signs in to the sidecar, as an admin set it.
#[derive(Clone)]
pub struct Connection {
    pub url: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("url", &self.url)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// The connection the stored settings name; a setting missing or empty is `NotConfigured`.
    pub fn from_settings(
        url: Option<String>,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Connection, AirdcppError> {
        let given = |value: Option<String>, missing| {
            value
                .filter(|v| !v.trim().is_empty())
                .ok_or(AirdcppError::NotConfigured(missing))
        };
        Ok(Connection {
            url: given(url, Missing::Url)?,
            username: given(username, Missing::Username)?,
            password: given(password, Missing::Password)?,
        })
    }

    /// The image's shipped login, which Soulbeet refuses.
    pub fn is_default_login(&self) -> bool {
        self.username == "admin" && self.password == "password"
    }

    /// What the refusals are kept under: no password is kept in the clear.
    fn refusal_key(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        for part in [&self.url, &self.username, &self.password] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        hash.finalize().into()
    }
}

/// The connections AirDC++ answered with a 401.
static REFUSED: LazyLock<Mutex<HashSet<[u8; 32]>>> = LazyLock::new(Default::default);

/// Forgets every 401: called when the settings are saved, so the next request signs in again.
pub fn forget_refusals() {
    REFUSED.lock().unwrap().clear();
}

/// The base of the Web API at `url`: an http or https address with no user name, password,
/// query or fragment in it, so no address Soulbeet logs or shows can hold a secret.
pub fn api_base(url: &str) -> Result<Url, AirdcppError> {
    let bad = |why: &str| AirdcppError::BadUrl(why.to_string());
    let url = Url::parse(url.trim()).map_err(|e| bad(&e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("it must start with http:// or https://"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad(
            "it must not hold a user name or password; set them in their own fields",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad("it must not hold a query or a fragment"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(bad("it names no host"));
    }
    let path = url.path().trim_end_matches('/');
    url.join(&format!("{path}/api/v1/"))
        .map_err(|e| bad(&e.to_string()))
}

/// One answer the sidecar gave that is not a 401, a 403 or a 412.
#[derive(Debug)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    /// The sidecar's own words: its JSON `message`, else the body.
    pub(crate) fn message(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.body.trim().to_string())
    }

    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether AirDC++ itself refused the request: a 4xx with its JSON message. A proxy in
    /// front of it answers otherwise.
    pub(crate) fn refused_by_airdcpp(&self) -> bool {
        (400..500).contains(&self.status)
            && serde_json::from_str::<serde_json::Value>(&self.body)
                .is_ok_and(|v| v.get("message").is_some_and(|m| m.is_string()))
    }

    /// The answer read as `T`, when it is a success.
    pub(crate) fn json<T: DeserializeOwned>(self) -> Result<T, AirdcppError> {
        if !self.ok() {
            return Err(self.refused());
        }
        let body = if self.body.trim().is_empty() {
            "null"
        } else {
            &self.body
        };
        serde_json::from_str(body).map_err(|e| AirdcppError::Unreadable(e.to_string()))
    }

    /// Ok for a success.
    pub(crate) fn done(self) -> Result<(), AirdcppError> {
        match self.ok() {
            true => Ok(()),
            false => Err(self.refused()),
        }
    }

    pub(crate) fn refused(&self) -> AirdcppError {
        AirdcppError::Status {
            status: self.status,
            message: self.message(),
        }
    }
}

/// The client of one sidecar.
pub struct Sidecar {
    base: Url,
    connection: Connection,
    http: Client,
    breaker: CircuitBreaker,
    /// The key of `REFUSED` for this connection.
    refusal: [u8; 32],
}

impl Sidecar {
    pub fn new(connection: Connection) -> Result<Sidecar, AirdcppError> {
        let base = api_base(&connection.url)?;
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| AirdcppError::Unreachable(e.without_url().to_string()))?;
        Ok(Sidecar {
            base,
            refusal: connection.refusal_key(),
            connection,
            http,
            breaker: CircuitBreaker::default(),
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Sends `method` to `path` (relative to `/api/v1/`) with `body`: the answer, or why there
    /// is none. A 401, a 403 and a 412 are errors of their own; any other status is the caller's
    /// to read.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Answer, AirdcppError> {
        if REFUSED.lock().unwrap().contains(&self.refusal) {
            return Err(AirdcppError::Unauthorized);
        }
        if self.breaker.is_open().await {
            return Err(AirdcppError::Unreachable(
                "several requests in a row failed; asking again in a minute".into(),
            ));
        }
        let url = self
            .base
            .join(path)
            .map_err(|e| AirdcppError::BadUrl(e.to_string()))?;
        let mut request = self
            .http
            .request(method.clone(), url)
            .basic_auth(&self.connection.username, Some(&self.connection.password));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                self.breaker.record_failure().await;
                // A failure never holds the request's URL.
                let e = e.without_url();
                warn!("AirDC++ {method} {path} failed: {e}");
                return Err(AirdcppError::Unreachable(e.to_string()));
            }
        };
        let status = response.status().as_u16();
        let text = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                self.breaker.record_failure().await;
                return Err(AirdcppError::Unreachable(e.without_url().to_string()));
            }
        };
        self.breaker.record_success().await;
        debug!("AirDC++ {method} {path} -> {status}");
        let answer = Answer { status, body: text };
        match status {
            401 => {
                REFUSED.lock().unwrap().insert(self.refusal);
                warn!("AirDC++ refused the user name and password; nothing more is sent until the settings change");
                Err(AirdcppError::Unauthorized)
            }
            403 => Err(AirdcppError::Forbidden(answer.message())),
            412 => Err(AirdcppError::ApiVersion),
            _ => Ok(answer),
        }
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, AirdcppError> {
        self.send(Method::GET, path, None).await?.json()
    }

    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, AirdcppError> {
        self.send(Method::POST, path, Some(body)).await?.json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn connection(url: &str) -> Connection {
        Connection {
            url: url.into(),
            username: "soulbeet".into(),
            password: "s3cret-pass".into(),
        }
    }

    #[test]
    fn an_address_with_a_secret_or_another_scheme_is_refused() {
        assert_eq!(
            api_base("http://airdcpp:5600").unwrap().as_str(),
            "http://airdcpp:5600/api/v1/"
        );
        assert_eq!(
            api_base("https://example.org/dc/").unwrap().as_str(),
            "https://example.org/dc/api/v1/"
        );
        for bad in [
            "http://admin:password@airdcpp:5600",
            "http://admin@airdcpp:5600",
            "ftp://airdcpp",
            "airdcpp:5600",
            "http://airdcpp:5600/?x=1",
        ] {
            assert!(
                matches!(api_base(bad), Err(AirdcppError::BadUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_missing_setting_is_not_configured() {
        let some = |s: &str| Some(s.to_string());
        assert!(matches!(
            Connection::from_settings(None, some("u"), some("p")),
            Err(AirdcppError::NotConfigured(Missing::Url))
        ));
        assert!(matches!(
            Connection::from_settings(some("http://x"), some(" "), some("p")),
            Err(AirdcppError::NotConfigured(Missing::Username))
        ));
        assert!(matches!(
            Connection::from_settings(some("http://x"), some("u"), None),
            Err(AirdcppError::NotConfigured(Missing::Password))
        ));
        let given = Connection::from_settings(some("http://x"), some("admin"), some("password"));
        assert!(given.unwrap().is_default_login());
        assert!(!format!("{:?}", connection("http://x")).contains("s3cret"));
    }

    #[tokio::test]
    async fn every_request_carries_basic_auth_and_a_401_stops_all_later_ones() {
        let (base, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"api_version":1}"#.into(),
            },
            Reply {
                status: 401,
                body: "Invalid username or password".into(),
            },
            Reply {
                status: 200,
                body: "[]".into(),
            },
        ]);
        let sidecar = Sidecar::new(connection(&base)).unwrap();
        let info: serde_json::Value = sidecar.get("system/system_info").await.unwrap();
        assert_eq!(info["api_version"], 1);
        assert!(matches!(
            sidecar.get::<serde_json::Value>("sessions/self").await,
            Err(AirdcppError::Unauthorized)
        ));
        for _ in 0..3 {
            assert!(matches!(
                sidecar.get::<serde_json::Value>("hubs").await,
                Err(AirdcppError::Unauthorized)
            ));
        }
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "nothing is sent after the 401");
            assert_eq!(seen[0].target, "/api/v1/system/system_info");
            // soulbeet:s3cret-pass
            assert_eq!(
                seen[0].authorization.as_deref(),
                Some("Basic c291bGJlZXQ6czNjcmV0LXBhc3M=")
            );
        }
        // Another client of the same settings sends nothing either, until they are saved again.
        let rebuilt = Sidecar::new(connection(&base)).unwrap();
        assert!(matches!(
            rebuilt.get::<serde_json::Value>("hubs").await,
            Err(AirdcppError::Unauthorized)
        ));
        assert_eq!(seen.lock().unwrap().len(), 2);
        forget_refusals();
        let hubs: Vec<serde_json::Value> = rebuilt.get("hubs").await.unwrap();
        assert!(hubs.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_403_a_412_and_an_unreadable_body_are_errors_of_their_own() {
        let (base, _) = serve(vec![
            Reply {
                status: 403,
                body: r#"{"message":"The permission queue_edit is required for accessing this method"}"#.into(),
            },
            Reply {
                status: 412,
                body: r#"{"message":"Unsupported API version"}"#.into(),
            },
            Reply {
                status: 200,
                body: "{not json".into(),
            },
        ]);
        let sidecar = Sidecar::new(connection(&base)).unwrap();
        match sidecar.get::<serde_json::Value>("queue/bundles/0/1").await {
            Err(AirdcppError::Forbidden(text)) => assert!(text.contains("queue_edit"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            sidecar.get::<serde_json::Value>("x").await,
            Err(AirdcppError::ApiVersion)
        ));
        assert!(matches!(
            sidecar.get::<serde_json::Value>("x").await,
            Err(AirdcppError::Unreadable(_))
        ));
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable_and_names_no_url() {
        let sidecar = Sidecar::new(connection("http://127.0.0.1:1")).unwrap();
        match sidecar.get::<serde_json::Value>("hubs").await {
            Err(AirdcppError::Unreachable(text)) => {
                assert!(!text.contains("127.0.0.1"), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }
}
