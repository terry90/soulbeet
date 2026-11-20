//! The provider's discovery document and signing keys, cached for an hour
//! and fetched again at once on a key rotation.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use serde::{de::DeserializeOwned, Deserialize};
use tokio::sync::Mutex;

use crate::config::OidcConfig;

const PROVIDER_TTL: Duration = Duration::from_secs(3600);
/// After a failed fetch, wait this long before asking the provider again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// A forced refresh (unknown signing key) reuses a copy at most this old, so
/// concurrent sign-ins after a key rotation fetch once.
const REFRESH_MIN_AGE: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        // Endpoints come from the discovery document; never follow a redirect
        // to somewhere else with a code or a client secret.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build the OIDC HTTP client")
});

static PROVIDER: LazyLock<Mutex<ProviderCache>> = LazyLock::new(Default::default);

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

#[derive(Default)]
struct ProviderCache {
    provider: Option<Arc<Provider>>,
    failed_at: Option<Instant>,
}

impl ProviderCache {
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

/// The cached provider, fetched when missing, stale or `refresh` is set. A
/// failed fetch keeps the stale copy, if any, and is not retried until
/// [`FAILURE_BACKOFF`] has passed.
pub(super) async fn provider(config: &OidcConfig, refresh: bool) -> Result<Arc<Provider>, String> {
    let mut cache = PROVIDER.lock().await;
    if let Some(answer) = cache.answer(refresh) {
        return answer;
    }
    match fetch_provider(config).await {
        Ok(provider) => {
            let provider = Arc::new(provider);
            cache.provider = Some(provider.clone());
            cache.failed_at = None;
            Ok(provider)
        }
        Err(e) => {
            cache.failed_at = Some(Instant::now());
            match &cache.provider {
                Some(stale) => {
                    tracing::warn!("OIDC: cannot refresh the provider, using the cached copy: {e}");
                    Ok(stale.clone())
                }
                None => Err(e),
            }
        }
    }
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
            "the discovery document names the issuer '{}', not '{issuer_url}'",
            discovery.issuer
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
    let keys: JwkSet = get_json(endpoints.jwks.as_str()).await?;

    Ok(Provider {
        issuer: discovery.issuer,
        authorization_endpoint: endpoints.authorization,
        token_endpoint: endpoints.token,
        client_auth: ClientAuth::choose(&discovery.token_endpoint_auth_methods_supported),
        keys,
        fetched_at: Instant::now(),
    })
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
    response
        .json()
        .await
        .map_err(|e| format!("GET {url} returned an unexpected body: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc::testing::test_provider;
    use serde_json::json;

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

        // The document names the issuer it was fetched for.
        let other = "https://other.example.com";
        assert!(checked_endpoints(&discovery(other, secure), issuer).is_err());
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
        let fresh = ProviderCache {
            provider: Some(cached(Duration::from_secs(60))),
            failed_at: None,
        };
        assert!(matches!(fresh.answer(false), Some(Ok(_))));
        // A forced refresh fetches unless the copy is only seconds old.
        assert!(fresh.answer(true).is_none());
        let just_fetched = ProviderCache {
            provider: Some(cached(Duration::ZERO)),
            failed_at: None,
        };
        assert!(matches!(just_fetched.answer(true), Some(Ok(_))));

        let stale = ProviderCache {
            provider: Some(cached(PROVIDER_TTL * 2)),
            failed_at: None,
        };
        assert!(stale.answer(false).is_none());

        // Right after a failure: the stale copy, or an error, without fetching.
        let failed = ProviderCache {
            provider: Some(cached(PROVIDER_TTL * 2)),
            failed_at: ago(Duration::from_secs(1)),
        };
        assert!(matches!(failed.answer(false), Some(Ok(_))));
        let failed_empty = ProviderCache {
            provider: None,
            failed_at: ago(Duration::from_secs(1)),
        };
        assert!(matches!(failed_empty.answer(false), Some(Err(_))));
        // After the backoff, fetch again.
        let retry = ProviderCache {
            provider: None,
            failed_at: ago(FAILURE_BACKOFF * 2),
        };
        assert!(retry.answer(false).is_none());
    }
}
