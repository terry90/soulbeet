//! The provider's discovery document and signing keys, cached for an hour
//! and fetched again at once on a key rotation.

use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::loggable;
use crate::capped_body::read_capped;
use crate::config::OidcConfig;

const PROVIDER_TTL: Duration = Duration::from_secs(3600);
/// After a failed fetch, wait this long before asking the provider again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// A forced refresh (unknown signing key) reuses a copy at most this old, so
/// concurrent sign-ins after a key rotation fetch once.
const REFRESH_MIN_AGE: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// The most a discovery document, key set or token response may weigh. They
/// are a few kilobytes; a provider sending more is broken.
pub(super) const MAX_BODY: usize = 1024 * 1024;

pub(super) static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        // Endpoints come from the discovery document; never follow a redirect
        // to somewhere else with a code or a client secret.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build the OIDC HTTP client")
});

static PROVIDER: LazyLock<ProviderCache> = LazyLock::new(Default::default);

/// The parts of the discovery document this flow uses.
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

/// How the client secret is sent to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientAuth {
    Basic,
    Post,
}

impl ClientAuth {
    /// `client_secret_basic` unless the provider only lists
    /// `client_secret_post`. An empty list means the spec default, basic.
    fn choose(supported: &[String]) -> Self {
        let has = |method: &str| supported.iter().any(|m| m == method);
        if !has("client_secret_basic") && has("client_secret_post") {
            ClientAuth::Post
        } else {
            ClientAuth::Basic
        }
    }
}

pub(super) struct Provider {
    pub(super) issuer: String,
    pub(super) authorization_endpoint: url::Url,
    pub(super) token_endpoint: url::Url,
    pub(super) client_auth: ClientAuth,
    pub(super) keys: JwkSet,
    pub(super) fetched_at: Instant,
}

/// The provider as last fetched, and when a fetch last failed.
#[derive(Default)]
struct Cached {
    provider: Option<Arc<Provider>>,
    failed_at: Option<Instant>,
}

impl Cached {
    /// What the cache can answer without asking the provider: a usable copy,
    /// or, within the backoff after a failure, the stale copy or an error.
    /// `None` means fetch.
    fn answer(&self, refresh: bool) -> Option<Result<Arc<Provider>, String>> {
        if let Some(provider) = &self.provider {
            let max_age = if refresh {
                REFRESH_MIN_AGE
            } else {
                PROVIDER_TTL
            };
            if provider.fetched_at.elapsed() < max_age {
                return Some(Ok(provider.clone()));
            }
        }
        let failed_at = self.failed_at?;
        if failed_at.elapsed() >= FAILURE_BACKOFF {
            return None;
        }
        Some(self.provider.clone().ok_or_else(|| {
            "the provider was unreachable moments ago; not retrying yet".to_string()
        }))
    }
}

/// The cached provider. What it holds is locked only to read or replace it,
/// never across a fetch, and one fetch runs at a time.
#[derive(Default)]
struct ProviderCache {
    cached: std::sync::Mutex<Cached>,
    fetching: Mutex<()>,
}

impl ProviderCache {
    fn cached(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.cached.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The provider, from `fetch` when the copy is missing, stale or
    /// `refresh` is set. While another fetch runs, a sign-in that has a copy
    /// uses it, and one that has none or needs new keys waits for that
    /// fetch. A failed fetch keeps the stale copy, if any, and is not retried
    /// until [`FAILURE_BACKOFF`] has passed.
    async fn get<F>(
        &self,
        refresh: bool,
        fetch: impl FnOnce() -> F,
    ) -> Result<Arc<Provider>, String>
    where
        F: Future<Output = Result<Provider, String>>,
    {
        if let Some(answer) = self.cached().answer(refresh) {
            return answer;
        }
        let _fetching = match self.fetching.try_lock() {
            Ok(fetching) => fetching,
            Err(_) => {
                let stale = self.cached().provider.clone();
                match stale {
                    Some(stale) if !refresh => return Ok(stale),
                    _ => self.fetching.lock().await,
                }
            }
        };
        // A fetch that ended meanwhile may have answered.
        if let Some(answer) = self.cached().answer(refresh) {
            return answer;
        }

        let fetched = fetch().await;
        let mut cached = self.cached();
        match fetched {
            Ok(provider) => {
                let provider = Arc::new(provider);
                cached.provider = Some(provider.clone());
                cached.failed_at = None;
                Ok(provider)
            }
            Err(e) => {
                cached.failed_at = Some(Instant::now());
                match &cached.provider {
                    Some(stale) => {
                        tracing::warn!(
                            "OIDC: cannot refresh the provider, using the cached copy: {e}"
                        );
                        Ok(stale.clone())
                    }
                    None => Err(e),
                }
            }
        }
    }
}

/// The provider of `config`, cached (see [`ProviderCache::get`]).
pub(super) async fn provider(config: &OidcConfig, refresh: bool) -> Result<Arc<Provider>, String> {
    PROVIDER.get(refresh, || fetch_provider(config)).await
}

/// The endpoints a discovery document names.
#[derive(Debug)]
struct Endpoints {
    authorization: url::Url,
    token: url::Url,
    jwks: url::Url,
}

/// The endpoints of `discovery`, fetched for the issuer `issuer_url`. Each
/// uses TLS (OpenID Connect Core 1.0, sections 3.1.2.1 and 3.1.3; RFC 6749,
/// sections 3.1 and 3.2), unless the issuer itself is on plain http, as on a
/// LAN without TLS.
fn checked_endpoints(discovery: &Discovery, issuer_url: &str) -> Result<Endpoints, String> {
    let issuer_url = issuer_url.trim_end_matches('/');
    // OpenID Connect Discovery 1.0, section 4.3: the document must name the
    // issuer it was fetched for.
    if discovery.issuer.trim_end_matches('/') != issuer_url {
        return Err(format!(
            "the discovery document names the issuer {}, not '{issuer_url}'",
            loggable(&discovery.issuer)
        ));
    }
    let issuer_on_http = url::Url::parse(issuer_url).is_ok_and(|url| url.scheme() == "http");
    let endpoint = |name: &str, value: &str| {
        let url = url::Url::parse(value).map_err(|e| format!("invalid {name}: {e}"))?;
        match url.scheme() {
            "https" => Ok(url),
            "http" if issuer_on_http => Ok(url),
            _ if issuer_on_http => Err(format!("the {name} {url} is neither https nor http")),
            _ => Err(format!(
                "the {name} {url} is not https, and the issuer {issuer_url} is"
            )),
        }
    };
    Ok(Endpoints {
        authorization: endpoint("authorization_endpoint", &discovery.authorization_endpoint)?,
        token: endpoint("token_endpoint", &discovery.token_endpoint)?,
        jwks: endpoint("jwks_uri", &discovery.jwks_uri)?,
    })
}

async fn fetch_provider(config: &OidcConfig) -> Result<Provider, String> {
    let issuer_url = config.issuer_url.trim_end_matches('/');
    let discovery: Discovery =
        get_json(&format!("{issuer_url}/.well-known/openid-configuration")).await?;
    let endpoints = checked_endpoints(&discovery, issuer_url)?;
    let published: PublishedKeys = get_json(endpoints.jwks.as_str()).await?;

    Ok(Provider {
        issuer: discovery.issuer,
        authorization_endpoint: endpoints.authorization,
        token_endpoint: endpoints.token,
        client_auth: ClientAuth::choose(&discovery.token_endpoint_auth_methods_supported),
        keys: readable_keys(published),
        fetched_at: Instant::now(),
    })
}

/// The provider's key set, each key still unread.
#[derive(Deserialize)]
struct PublishedKeys {
    keys: Vec<Value>,
}

/// The keys of `published` that `jsonwebtoken` can read. One it cannot (an
/// X25519 encryption key, a curve it does not know) is skipped, so it does
/// not take the signing keys down with it.
fn readable_keys(published: PublishedKeys) -> JwkSet {
    let keys = published
        .keys
        .into_iter()
        .filter_map(|key| {
            let kid = key.get("kid").and_then(Value::as_str).map(loggable);
            serde_json::from_value::<Jwk>(key)
                .inspect_err(|e| {
                    tracing::warn!(
                        "OIDC: skipping the provider key {} that cannot be read: {e}",
                        kid.as_deref().unwrap_or("without a kid")
                    )
                })
                .ok()
        })
        .collect();
    JwkSet { keys }
}

async fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let response = HTTP
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    // Redirects are not followed (see HTTP), and a 3xx is no error status.
    if response.status().is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .unwrap_or("nowhere");
        return Err(format!(
            "GET {url} redirected to {location}; set OIDC_ISSUER_URL to the issuer's final URL"
        ));
    }
    let body = read_capped(response, MAX_BODY)
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("GET {url} returned an unexpected body: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::{serve_on_loopback, streamed};
    use crate::oidc::testing::{json_of_size, test_provider, TEST_KEY_X};
    use axum::{routing::get, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

    #[tokio::test]
    async fn a_body_past_the_limit_is_refused() {
        let app = Router::new()
            .route(
                "/at-limit",
                get(|| async { streamed(json_of_size(MAX_BODY)) }),
            )
            .route("/sized", get(|| async { json_of_size(MAX_BODY + 1) }))
            .route(
                "/streamed",
                get(|| async { streamed(json_of_size(MAX_BODY + 1)) }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let url = |path: &str| format!("http://{addr}{path}");

        assert!(get_json::<Value>(&url("/at-limit")).await.is_ok());
        // Refused from its Content-Length, or once the chunks read pass it.
        for path in ["/sized", "/streamed"] {
            let refused = get_json::<Value>(&url(path)).await.unwrap_err();
            assert!(refused.contains("larger than"), "{path}: {refused}");
        }
    }

    #[test]
    fn a_key_the_library_cannot_read_is_skipped_not_the_whole_set() {
        // An X25519 encryption key and a curve jsonwebtoken does not know,
        // next to the signing key.
        let published = json!({ "keys": [
            { "kty": "OKP", "kid": "enc", "use": "enc", "crv": "X25519", "x": TEST_KEY_X },
            { "kty": "EC", "kid": "k256", "crv": "secp256k1", "x": "AQAB", "y": "AQAB" },
            "not a key",
            { "kty": "OKP", "kid": "k1", "crv": "Ed25519", "x": TEST_KEY_X },
        ]});
        assert!(serde_json::from_value::<JwkSet>(published.clone()).is_err());

        let keys = readable_keys(serde_json::from_value(published).unwrap());
        let kids: Vec<_> = keys
            .keys
            .iter()
            .map(|jwk| jwk.common.key_id.as_deref())
            .collect();
        assert_eq!(kids, [Some("k1")]);
    }

    const NAMES: [&str; 3] = ["authorization_endpoint", "token_endpoint", "jwks_uri"];

    /// A discovery document of `issuer` naming `endpoints` in [`NAMES`]'s order.
    fn discovery(issuer: &str, endpoints: [&str; 3]) -> Discovery {
        let mut document = json!({ "issuer": issuer });
        for (name, endpoint) in NAMES.iter().zip(endpoints) {
            document[name] = json!(endpoint);
        }
        serde_json::from_value(document).unwrap()
    }

    #[test]
    fn the_endpoints_use_tls_unless_the_issuer_does_not() {
        let issuer = "https://id.example.com";
        let secure = [
            "https://id.example.com/authorize",
            "https://id.example.com/token",
            "https://id.example.com/jwks",
        ];
        let endpoints = checked_endpoints(&discovery(issuer, secure), issuer).unwrap();
        assert_eq!(endpoints.authorization.as_str(), secure[0]);
        assert_eq!(endpoints.token.as_str(), secure[1]);
        assert_eq!(endpoints.jwks.as_str(), secure[2]);

        // An https issuer whose document lists a plain http endpoint (a
        // proxy rewriting the scheme) would get the code, the verifier and
        // the client secret sent in the clear.
        for (i, name) in NAMES.iter().enumerate() {
            let mut listed = secure;
            let plain = secure[i].replacen("https:", "http:", 1);
            listed[i] = &plain;
            let refused = checked_endpoints(&discovery(issuer, listed), issuer).unwrap_err();
            assert!(refused.contains(name), "{refused}");
        }

        // An issuer on plain http (a LAN without TLS) may list either.
        let lan = "http://192.168.1.2:1411";
        let mixed = [
            "http://192.168.1.2:1411/authorize",
            "https://id.example.com/token",
            "http://192.168.1.2:1411/jwks",
        ];
        assert!(checked_endpoints(&discovery(lan, mixed), lan).is_ok());
        // But no other scheme, and nothing that is no URL.
        for odd in ["ftp://192.168.1.2/jwks", "/jwks"] {
            let listed = [mixed[0], mixed[1], odd];
            assert!(checked_endpoints(&discovery(lan, listed), lan).is_err());
        }

        // The document names the issuer it was fetched for, and what it
        // names cannot forge a log line.
        let other = "https://other.example.com\nINFO forged";
        let refused = checked_endpoints(&discovery(other, secure), issuer).unwrap_err();
        assert!(!refused.contains('\n'), "{refused}");
        assert!(checked_endpoints(&discovery("https://id.example.com/", secure), issuer).is_ok());
    }

    #[test]
    fn client_auth_prefers_basic() {
        let methods = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(ClientAuth::choose(&[]), ClientAuth::Basic);
        assert_eq!(
            ClientAuth::choose(&methods(&["client_secret_post", "client_secret_basic"])),
            ClientAuth::Basic
        );
        assert_eq!(
            ClientAuth::choose(&methods(&["client_secret_post"])),
            ClientAuth::Post
        );
    }

    fn cached(age: Duration) -> Arc<Provider> {
        let mut provider = test_provider();
        provider.fetched_at = Instant::now().checked_sub(age).unwrap();
        Arc::new(provider)
    }

    fn ago(age: Duration) -> Option<Instant> {
        Instant::now().checked_sub(age)
    }

    #[test]
    fn provider_cache_answers() {
        let fresh = Cached {
            provider: Some(cached(Duration::from_secs(60))),
            failed_at: None,
        };
        assert!(matches!(fresh.answer(false), Some(Ok(_))));
        // A forced refresh fetches unless the copy is only seconds old.
        assert!(fresh.answer(true).is_none());
        let just_fetched = Cached {
            provider: Some(cached(Duration::ZERO)),
            failed_at: None,
        };
        assert!(matches!(just_fetched.answer(true), Some(Ok(_))));

        let stale = Cached {
            provider: Some(cached(PROVIDER_TTL * 2)),
            failed_at: None,
        };
        assert!(stale.answer(false).is_none());

        // Right after a failure: the stale copy, or an error, without fetching.
        let failed = Cached {
            provider: Some(cached(PROVIDER_TTL * 2)),
            failed_at: ago(Duration::from_secs(1)),
        };
        assert!(matches!(failed.answer(false), Some(Ok(_))));
        let failed_empty = Cached {
            provider: None,
            failed_at: ago(Duration::from_secs(1)),
        };
        assert!(matches!(failed_empty.answer(false), Some(Err(_))));
        // After the backoff, fetch again.
        let retry = Cached {
            provider: None,
            failed_at: ago(FAILURE_BACKOFF * 2),
        };
        assert!(retry.answer(false).is_none());
    }

    /// A cache holding `provider` as fetched now and then, and the failure
    /// at `failed_at`.
    fn holding(provider: Option<Arc<Provider>>, failed_at: Option<Instant>) -> ProviderCache {
        let cache = ProviderCache::default();
        *cache.cached() = Cached {
            provider,
            failed_at,
        };
        cache
    }

    /// A fetch answering `result` at once, counted in `fetches`.
    fn counted(
        fetches: &AtomicUsize,
        result: Result<Provider, String>,
    ) -> impl FnOnce() -> std::future::Ready<Result<Provider, String>> + '_ {
        move || {
            fetches.fetch_add(1, Ordering::SeqCst);
            std::future::ready(result)
        }
    }

    fn renewed() -> Provider {
        Provider {
            issuer: "https://renewed.example.com".to_string(),
            ..test_provider()
        }
    }

    #[tokio::test]
    async fn a_slow_fetch_runs_alone_and_holds_up_no_sign_in_that_has_a_copy() {
        let stale = cached(PROVIDER_TTL * 2);
        let cache = holding(Some(stale.clone()), None);
        let (answer, answered) = tokio::sync::oneshot::channel::<()>();

        // The sign-in that found the copy stale fetches, and the provider is
        // slow to answer.
        let slow = cache.get(false, || async {
            answered.await.unwrap();
            Ok(renewed())
        });
        tokio::pin!(slow);
        assert!(futures::poll!(&mut slow).is_pending());

        // Meanwhile another sign-in uses the stale copy at once, without a
        // second fetch.
        let fetches = AtomicUsize::new(0);
        let other = cache.get(false, counted(&fetches, Ok(renewed())));
        tokio::pin!(other);
        let Poll::Ready(other) = futures::poll!(&mut other) else {
            panic!("a sign-in with a copy waited for the fetch");
        };
        assert!(Arc::ptr_eq(&other.unwrap(), &stale));

        // One that needs new keys waits for that fetch instead of starting
        // another.
        let forced = cache.get(true, counted(&fetches, Ok(renewed())));
        tokio::pin!(forced);
        assert!(futures::poll!(&mut forced).is_pending());

        answer.send(()).unwrap();
        assert_eq!(slow.await.unwrap().issuer, "https://renewed.example.com");
        assert_eq!(forced.await.unwrap().issuer, "https://renewed.example.com");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sign_ins_without_a_copy_wait_for_the_one_fetch() {
        let cache = holding(None, None);
        let (answer, answered) = tokio::sync::oneshot::channel::<()>();
        let first = cache.get(false, || async {
            answered.await.unwrap();
            Err("unreachable".to_string())
        });
        tokio::pin!(first);
        assert!(futures::poll!(&mut first).is_pending());

        let fetches = AtomicUsize::new(0);
        let second = cache.get(false, counted(&fetches, Ok(renewed())));
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());

        // The fetch failed: both hear it, and the second does not ask again
        // within the backoff.
        answer.send(()).unwrap();
        assert!(first.await.is_err());
        assert!(second.await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_stale_copy_and_backs_off() {
        let stale = cached(PROVIDER_TTL * 2);
        let cache = holding(Some(stale.clone()), None);
        let fetches = AtomicUsize::new(0);

        let kept = cache
            .get(false, counted(&fetches, Err("down".into())))
            .await;
        assert!(Arc::ptr_eq(&kept.unwrap(), &stale));
        // Within the backoff, not even new keys are asked for.
        for refresh in [false, true] {
            let kept = cache.get(refresh, counted(&fetches, Ok(renewed()))).await;
            assert!(Arc::ptr_eq(&kept.unwrap(), &stale));
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);

        // After it, the next sign-in fetches again.
        cache.cached().failed_at = ago(FAILURE_BACKOFF * 2);
        let renewed = cache.get(false, counted(&fetches, Ok(renewed()))).await;
        assert_eq!(renewed.unwrap().issuer, "https://renewed.example.com");
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert!(cache.cached().failed_at.is_none());
    }
}
