//! Sign-in through an OpenID Connect provider (Pocket ID, Authelia,
//! Authentik, Keycloak, ...).
//!
//! The authorization code flow with PKCE (S256), a `state` against login CSRF
//! and a `nonce` bound to the ID token. The three travel in a short-lived,
//! encrypted cookie, so the server keeps no per-login state. The provider's
//! discovery document and signing keys are cached for an hour, and fetched
//! again at once when an ID token is signed by a key not in the cache (a key
//! rotation).
//!
//! A failed fetch is not retried for 30 seconds, and a stale copy keeps
//! serving meanwhile, so an unreachable provider does not stall every login.
//!
//! A successful sign-in starts the same session as a password login. An
//! account is found by its OIDC identity (issuer and subject); on its first
//! sign-in, by the username claim (ignoring ASCII case, as every sign-in
//! does), which links an existing account of that name to the identity.
//!
//! A sign-in started from the login page of an app's authorize request
//! (`/login/return`) goes back to that request afterwards.

use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::{
    extract::Query,
    http::{HeaderMap, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{
    jwk::{AlgorithmParameters, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse},
    Algorithm, DecodingKey, Validation,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tower_cookies::{
    cookie::{time, SameSite},
    Cookie, Cookies,
};

use crate::auth::SsoError;
use crate::config::{OidcConfig, CONFIG};
use crate::crypto;
use crate::db::DB;
use crate::models::user::{username_fits, NameMatch, User, UserChangeError};
use crate::server_fns::auth::{issue_session_cookie, SessionCookie};
use sqlx::SqlitePool;

pub use crate::auth::{OIDC_CALLBACK_PATH, OIDC_LOGIN_PATH};

/// Prefix of the cookie that holds the state, nonce and PKCE verifier between
/// the two legs of a sign-in. The state completes the name, so logins started
/// in two tabs do not overwrite each other.
const FLOW_COOKIE_PREFIX: &str = "oidc_flow_";
const FLOW_COOKIE_PATH: &str = "/api/auth/oidc";
const FLOW_TTL_SECS: i64 = 600;
const PROVIDER_TTL: Duration = Duration::from_secs(3600);
/// After a failed fetch, wait this long before asking the provider again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// A forced refresh (unknown signing key) reuses a copy at most this old, so
/// concurrent sign-ins after a key rotation fetch once.
const REFRESH_MIN_AGE: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Signature algorithms accepted on ID tokens. Asymmetric only: HMAC would be
/// keyed with the client secret, which this flow never verifies with.
const ALLOWED_ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// The two routes of the flow, to merge into the app router.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(OIDC_LOGIN_PATH, get(login))
        .route(OIDC_CALLBACK_PATH, get(callback))
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
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
enum ClientAuth {
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

struct Provider {
    issuer: String,
    authorization_endpoint: url::Url,
    token_endpoint: String,
    client_auth: ClientAuth,
    keys: JwkSet,
    fetched_at: Instant,
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
async fn provider(config: &OidcConfig, refresh: bool) -> Result<Arc<Provider>, String> {
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

async fn fetch_provider(config: &OidcConfig) -> Result<Provider, String> {
    let issuer_url = config.issuer_url.trim_end_matches('/');
    let discovery: Discovery =
        get_json(&format!("{issuer_url}/.well-known/openid-configuration")).await?;

    // OpenID Connect Discovery 1.0, section 4.3: the document must name the
    // issuer it was fetched for.
    if discovery.issuer.trim_end_matches('/') != issuer_url {
        return Err(format!(
            "the discovery document names the issuer '{}', not '{}'",
            discovery.issuer, config.issuer_url
        ));
    }

    let authorization_endpoint = url::Url::parse(&discovery.authorization_endpoint)
        .map_err(|e| format!("invalid authorization_endpoint: {e}"))?;
    let keys: JwkSet = get_json(&discovery.jwks_uri).await?;

    Ok(Provider {
        issuer: discovery.issuer,
        authorization_endpoint,
        token_endpoint: discovery.token_endpoint,
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

/// What the callback needs to check that it answers the login it started.
#[derive(Serialize, Deserialize)]
struct Flow {
    state: String,
    nonce: String,
    verifier: String,
    expires_at: i64,
    /// The authorize request to go back to, already checked
    #[serde(default)]
    return_to: Option<String>,
}

/// 256 random bits, base64url: a state, nonce or PKCE verifier (RFC 7636
/// wants 43 to 128 characters; this is 43).
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorization_url(provider: &Provider, config: &OidcConfig, flow: &Flow) -> url::Url {
    let mut url = provider.authorization_endpoint.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &config.redirect_url)
        .append_pair("scope", &config.scopes)
        .append_pair("state", &flow.state)
        .append_pair("nonce", &flow.nonce)
        .append_pair("code_challenge", &pkce_challenge(&flow.verifier))
        .append_pair("code_challenge_method", "S256");
    url
}

fn flow_cookie_name(state: &str) -> String {
    format!("{FLOW_COOKIE_PREFIX}{state}")
}

/// A value from the callback URL, safe to log: anyone can craft that URL.
/// Shortened, and quoted with `{:?}` so newlines cannot forge log lines.
fn loggable(value: &str) -> String {
    format!("{:?}", value.chars().take(200).collect::<String>())
}

fn flow_cookie(state: &str, value: String, config: &OidcConfig) -> Cookie<'static> {
    let mut cookie = Cookie::new(flow_cookie_name(state), value);
    cookie.set_path(FLOW_COOKIE_PATH);
    cookie.set_http_only(true);
    // Lax, not Strict: the callback is a top-level navigation coming from the
    // provider's site, and must carry this cookie.
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(config.redirect_url.starts_with("https://"));
    cookie.set_max_age(time::Duration::seconds(FLOW_TTL_SECS));
    cookie
}

/// Back to the login page, which shows what went wrong.
fn to_login(error: SsoError) -> Response {
    Redirect::to(&format!("/login/error/{}", error.code())).into_response()
}

#[derive(Deserialize)]
struct LoginParams {
    return_to: Option<String>,
}

/// `GET /api/auth/oidc/login`: send the browser to the provider.
async fn login(cookies: Cookies, Query(params): Query<LoginParams>) -> Response {
    let Some(config) = CONFIG.oidc() else {
        return to_login(SsoError::Disabled);
    };

    let provider = match provider(config, false).await {
        Ok(provider) => provider,
        Err(e) => {
            tracing::error!("OIDC sign-in failed: cannot reach the provider: {e}");
            return to_login(SsoError::Failed);
        }
    };

    let flow = Flow {
        state: random_token(),
        nonce: random_token(),
        verifier: random_token(),
        expires_at: chrono::Utc::now().timestamp() + FLOW_TTL_SECS,
        // Only a request Soulbeet itself may send the browser back to.
        return_to: params
            .return_to
            .as_deref()
            .and_then(shared::oauth::accepted_return_to)
            .map(str::to_owned),
    };
    let sealed = match serde_json::to_string(&flow)
        .map_err(|e| e.to_string())
        .and_then(|json| crypto::encrypt(crate::secret_key::current(), &json))
    {
        Ok(sealed) => sealed,
        Err(e) => {
            tracing::error!("OIDC sign-in failed: cannot seal the login state: {e}");
            return to_login(SsoError::Failed);
        }
    };

    cookies.add(flow_cookie(&flow.state, sealed, config));
    Redirect::to(authorization_url(&provider, config, &flow).as_str()).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /api/auth/oidc/callback`: the provider sends the browser back here.
async fn callback(
    cookies: Cookies,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<CallbackParams>,
) -> Response {
    // Read before anything is written, as a password login does.
    let session = match SessionCookie::for_request(&headers, &uri) {
        Ok(session) => session,
        Err(e) => {
            tracing::error!("OIDC sign-in failed: {e}");
            return to_login(SsoError::Failed);
        }
    };
    match complete_sign_in(&cookies, params).await {
        Ok((user, return_to)) => {
            tracing::info!("User '{}' signed in through OIDC", user.username);
            match issue_session_cookie(&cookies, &session, &user) {
                Ok(()) => Redirect::to(return_to.as_deref().unwrap_or("/")).into_response(),
                Err(e) => {
                    tracing::error!("OIDC sign-in failed: cannot start a session: {e}");
                    to_login(SsoError::Failed)
                }
            }
        }
        Err(error) => to_login(error),
    }
}

/// The signed-in account, and the authorize request to go back to.
async fn complete_sign_in(
    cookies: &Cookies,
    params: CallbackParams,
) -> Result<(User, Option<String>), SsoError> {
    let config = CONFIG.oidc().ok_or(SsoError::Disabled)?;

    // The login state is single use: drop it whatever happens next. The
    // state names its cookie; a callback without one matches no login.
    let sealed = params.state.as_deref().and_then(|state| {
        let name = flow_cookie_name(state);
        let sealed = cookies.get(&name).map(|c| c.value().to_string());
        if sealed.is_some() {
            let mut removal = Cookie::new(name, "");
            removal.set_path(FLOW_COOKIE_PATH);
            cookies.remove(removal);
        }
        sealed
    });

    if let Some(error) = params.error {
        tracing::warn!(
            "OIDC sign-in refused by the provider: {}: {}",
            loggable(&error),
            loggable(&params.error_description.unwrap_or_default())
        );
        return Err(SsoError::Denied);
    }

    let flow = sealed
        .and_then(|sealed| crypto::decrypt(crate::secret_key::current(), &sealed).ok())
        .and_then(|json| serde_json::from_str::<Flow>(&json).ok())
        .filter(|flow| flow.expires_at > chrono::Utc::now().timestamp())
        .ok_or_else(|| {
            tracing::warn!(
                "OIDC sign-in failed: no valid login state (expired, or the login \
                 started in another browser or on another URL than OIDC_REDIRECT_URL)"
            );
            SsoError::Failed
        })?;

    if params.state.as_deref() != Some(flow.state.as_str()) {
        tracing::warn!("OIDC sign-in failed: the state does not match this login");
        return Err(SsoError::Failed);
    }
    let code = params.code.ok_or_else(|| {
        tracing::warn!("OIDC sign-in failed: the provider sent no code");
        SsoError::Failed
    })?;

    let failed = |e: String| {
        tracing::error!("OIDC sign-in failed: {e}");
        SsoError::Failed
    };

    let provider = provider(config, false).await.map_err(failed)?;
    let id_token = exchange_code(&provider, config, &code, &flow.verifier)
        .await
        .map_err(failed)?;
    let claims = match verify_id_token(&provider, config, &id_token, &flow.nonce) {
        Err(TokenError::UnknownKey) => {
            // The provider may have rotated its keys since they were cached.
            let provider = self::provider(config, true).await.map_err(failed)?;
            verify_id_token(&provider, config, &id_token, &flow.nonce)
        }
        result => result,
    }
    .map_err(|e| failed(format!("invalid ID token: {e}")))?;

    let identity = Identity::from_claims(&claims, &config.username_claim).map_err(|e| {
        tracing::warn!("OIDC sign-in refused: {e}");
        SsoError::Failed
    })?;

    let user = find_or_create_account(&DB, &identity, config.auto_register).await?;
    Ok((sync_navidrome_status(&DB, user).await, flow.return_to))
}

/// An account without a Navidrome password cannot reach Navidrome. Say so
/// when Navidrome is configured, so the UI asks for one password login.
async fn sync_navidrome_status(db: &SqlitePool, mut user: User) -> User {
    use shared::system::NavidromeStatus;

    if user.navidrome_token.is_some() {
        return user;
    }
    let status = if CONFIG.navidrome_url().is_some() {
        NavidromeStatus::NotLinked
    } else {
        NavidromeStatus::Unknown
    };
    if user.navidrome_status != status.as_str() {
        match User::update_navidrome_token_in(db, &user.id, None, status.as_str()).await {
            Ok(()) => user.navidrome_status = status.as_str().to_string(),
            Err(e) => tracing::warn!("OIDC: cannot update the Navidrome status: {e}"),
        }
    }
    user
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
}

async fn exchange_code(
    provider: &Provider,
    config: &OidcConfig,
    code: &str,
    verifier: &str,
) -> Result<String, String> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", config.redirect_url.as_str()),
        ("code_verifier", verifier),
    ];

    let mut request = HTTP
        .post(&provider.token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json");
    match (config.client_secret.as_deref(), provider.client_auth) {
        (Some(secret), ClientAuth::Basic) => {
            // RFC 6749, section 2.3.1: both are form-encoded before Basic.
            let encode =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            request = request.basic_auth(encode(&config.client_id), Some(encode(secret)));
        }
        (Some(secret), ClientAuth::Post) => {
            form.push(("client_id", config.client_id.as_str()));
            form.push(("client_secret", secret));
        }
        (None, _) => form.push(("client_id", config.client_id.as_str())),
    }

    let response = request
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        return Err(format!("the token endpoint answered {status}: {body}"));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("unexpected token response: {e}"))?
        .id_token
        .ok_or_else(|| "the token response has no id_token".to_string())
}

/// ID token claims. `iss`, `aud` and `exp` are also checked by `jsonwebtoken`.
#[derive(Deserialize)]
struct IdClaims {
    /// Checked against the discovery document's issuer
    iss: String,
    sub: String,
    /// A string or an array; its value is checked by `jsonwebtoken`
    aud: Value,
    azp: Option<String>,
    nonce: Option<String>,
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

#[derive(Debug)]
enum TokenError {
    /// No cached key fits the token; the keys may have been rotated.
    UnknownKey,
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::UnknownKey => write!(f, "signed by an unknown key"),
            TokenError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

/// Whether `jwk` is a signing key for `alg`.
fn key_fits(jwk: &Jwk, alg: Algorithm) -> bool {
    let family_fits = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => matches!(
            alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        ),
        AlgorithmParameters::EllipticCurve(_) => {
            matches!(alg, Algorithm::ES256 | Algorithm::ES384)
        }
        AlgorithmParameters::OctetKeyPair(_) => alg == Algorithm::EdDSA,
        AlgorithmParameters::OctetKey(_) => false,
    };
    let use_fits = !matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption));
    let alg_fits = jwk
        .common
        .key_algorithm
        .is_none_or(|key_alg| algorithm_of(key_alg) == Some(alg));
    family_fits && use_fits && alg_fits
}

fn algorithm_of(key_alg: KeyAlgorithm) -> Option<Algorithm> {
    Algorithm::from_str(&key_alg.to_string()).ok()
}

/// The key that signed a token: the one named by `kid`, or the only one that
/// fits when the token names none.
fn find_key<'a>(keys: &'a JwkSet, kid: Option<&str>, alg: Algorithm) -> Option<&'a Jwk> {
    let mut fitting = keys.keys.iter().filter(|jwk| key_fits(jwk, alg));
    match kid {
        Some(kid) => fitting.find(|jwk| jwk.common.key_id.as_deref() == Some(kid)),
        None => match (fitting.next(), fitting.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        },
    }
}

fn verify_id_token(
    provider: &Provider,
    config: &OidcConfig,
    token: &str,
    nonce: &str,
) -> Result<IdClaims, TokenError> {
    let header = jsonwebtoken::decode_header(token)
        .map_err(|e| TokenError::Invalid(format!("unreadable header: {e}")))?;
    if !ALLOWED_ALGORITHMS.contains(&header.alg) {
        return Err(TokenError::Invalid(format!(
            "unsupported algorithm {:?}",
            header.alg
        )));
    }

    let jwk = find_key(&provider.keys, header.kid.as_deref(), header.alg)
        .ok_or(TokenError::UnknownKey)?;
    let key = DecodingKey::from_jwk(jwk)
        .map_err(|e| TokenError::Invalid(format!("unusable signing key: {e}")))?;

    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[&config.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

    let claims = jsonwebtoken::decode::<IdClaims>(token, &key, &validation)
        .map_err(|e| TokenError::Invalid(e.to_string()))?
        .claims;

    if claims.nonce.as_deref() != Some(nonce) {
        return Err(TokenError::Invalid("the nonce does not match".to_string()));
    }
    // OpenID Connect Core 1.0, section 3.1.3.7: with several audiences, or an
    // azp at all, this client must be the authorized party.
    let several_audiences = claims.aud.as_array().is_some_and(|aud| aud.len() > 1);
    if (several_audiences || claims.azp.is_some())
        && claims.azp.as_deref() != Some(config.client_id.as_str())
    {
        return Err(TokenError::Invalid(
            "issued to another authorized party (azp)".to_string(),
        ));
    }

    Ok(claims)
}

/// Who signed in: the provider's stable identifier (issuer and subject,
/// OpenID Connect Core 1.0, section 5.7) and the Soulbeet username.
#[derive(Debug, PartialEq)]
struct Identity {
    issuer: String,
    subject: String,
    username: String,
}

impl Identity {
    fn from_claims(claims: &IdClaims, username_claim: &str) -> Result<Self, String> {
        let username = if username_claim == "sub" {
            Some(claims.sub.as_str())
        } else {
            claims.other.get(username_claim).and_then(Value::as_str)
        }
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            format!(
                "the ID token has no '{username_claim}' claim (set OIDC_USERNAME_CLAIM, \
                 or add the scope that carries it to OIDC_SCOPES)"
            )
        })?;
        if !username_fits(username) {
            return Err(format!(
                "the '{username_claim}' claim is too long for a username"
            ));
        }

        // An unverified address must not take over the account of that name.
        if username_claim == "email"
            && claims.other.get("email_verified").and_then(Value::as_bool) == Some(false)
        {
            return Err(format!("the email '{username}' is not verified"));
        }

        Ok(Self {
            issuer: claims.iss.clone(),
            subject: claims.sub.clone(),
            username: username.to_string(),
        })
    }
}

/// Which account an identity signs in to.
#[derive(Debug)]
enum Account {
    /// Already linked to this identity
    Linked(User),
    /// Has the username (ignoring case) and no identity yet: link it
    Unlinked(User),
    /// Nobody has the username: create the account
    New,
    /// The username belongs to an account linked to another identity, or
    /// to several accounts that differ only in case
    Conflict,
    /// Nobody has the username and accounts are not created
    NotRegistered,
}

/// `same_name`: the accounts whose username matches the identity's.
fn choose_account(by_identity: Option<User>, same_name: NameMatch, auto_register: bool) -> Account {
    if let Some(user) = by_identity {
        return Account::Linked(user);
    }
    match same_name {
        NameMatch::One(user) if user.oidc_subject.is_none() => Account::Unlinked(user),
        NameMatch::One(_) | NameMatch::Several => Account::Conflict,
        NameMatch::None if auto_register => Account::New,
        NameMatch::None => Account::NotRegistered,
    }
}

async fn find_or_create_account(
    db: &SqlitePool,
    identity: &Identity,
    auto_register: bool,
) -> Result<User, SsoError> {
    let db_error = |e: String| {
        tracing::error!("OIDC sign-in failed: {e}");
        SsoError::Failed
    };

    let by_identity = User::find_by_oidc_identity_in(db, &identity.issuer, &identity.subject)
        .await
        .map_err(db_error)?;
    let same_name = match by_identity {
        Some(_) => NameMatch::None,
        None => User::find_by_name_in(db, &identity.username)
            .await
            .map_err(db_error)?,
    };

    match choose_account(by_identity, same_name, auto_register) {
        Account::Linked(user) => Ok(user),
        Account::Unlinked(user) => {
            if !User::link_oidc_identity_in(db, &user.id, &identity.issuer, &identity.subject)
                .await
                .map_err(db_error)?
            {
                // Linked by a concurrent sign-in in the meantime.
                return Err(SsoError::Conflict);
            }
            tracing::info!(
                "Linked the account '{}' to its OIDC identity",
                user.username
            );
            // Read back: the link changed the credentials version.
            User::find_by_id_in(db, &user.id)
                .await
                .map_err(db_error)?
                .ok_or(SsoError::Failed)
        }
        Account::New => {
            match User::create_oidc_in(db, &identity.username, &identity.issuer, &identity.subject)
                .await
            {
                Ok(user) => {
                    tracing::info!("Created the account '{}' on OIDC sign-in", user.username);
                    Ok(user)
                }
                // Created by a concurrent sign-in or registration meanwhile.
                Err(UserChangeError::UsernameTaken) => Err(SsoError::Conflict),
                Err(e) => Err(db_error(e.to_string())),
            }
        }
        Account::Conflict => {
            tracing::warn!(
                "OIDC sign-in refused: the account '{}' is linked to another OIDC identity, \
                 or several accounts have that name in other letter case",
                identity.username
            );
            Err(SsoError::Conflict)
        }
        Account::NotRegistered => {
            tracing::info!(
                "OIDC sign-in refused: no account '{}', and OIDC_AUTO_REGISTER is off \
                 (it is by default with REGISTRATION=admin)",
                identity.username
            );
            Err(SsoError::NotRegistered)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use serde_json::json;

    fn claims(value: Value) -> IdClaims {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_long_and_distinct() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
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

    #[test]
    fn find_key_by_kid_and_algorithm() {
        let keys: JwkSet = serde_json::from_value(json!({ "keys": [
            { "kty": "RSA", "kid": "rsa", "use": "sig", "alg": "RS256", "n": "AQAB", "e": "AQAB" },
            { "kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB" },
            { "kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": "AQAB" },
        ]}))
        .unwrap();

        let kid = |jwk: Option<&Jwk>| jwk.and_then(|k| k.common.key_id.clone());
        assert_eq!(
            kid(find_key(&keys, Some("rsa"), Algorithm::RS256)),
            Some("rsa".into())
        );
        assert_eq!(
            kid(find_key(&keys, Some("ed"), Algorithm::EdDSA)),
            Some("ed".into())
        );
        // The key says RS256: not usable for anything else, even RSA.
        assert!(find_key(&keys, Some("rsa"), Algorithm::PS256).is_none());
        // Wrong family for the algorithm.
        assert!(find_key(&keys, Some("ed"), Algorithm::RS256).is_none());
        // An encryption key never verifies a signature.
        assert!(find_key(&keys, Some("enc"), Algorithm::RS256).is_none());
        assert!(find_key(&keys, Some("missing"), Algorithm::RS256).is_none());
        // Without a kid, only an unambiguous key is used.
        assert_eq!(
            kid(find_key(&keys, None, Algorithm::EdDSA)),
            Some("ed".into())
        );
    }

    #[test]
    fn identity_from_username_claim() {
        let c = claims(json!({
            "iss": ISSUER, "sub": "abc", "aud": "soulbeet", "preferred_username": " alice "
        }));
        assert_eq!(
            Identity::from_claims(&c, "preferred_username").unwrap(),
            Identity {
                issuer: ISSUER.into(),
                subject: "abc".into(),
                username: "alice".into()
            }
        );
        assert_eq!(Identity::from_claims(&c, "sub").unwrap().username, "abc");
        assert!(Identity::from_claims(&c, "email").is_err());
    }

    #[test]
    fn identity_refuses_unverified_email() {
        let c = claims(json!({
            "iss": ISSUER, "sub": "abc", "aud": ["soulbeet"], "email": "a@example.com",
            "email_verified": false
        }));
        assert!(Identity::from_claims(&c, "email").is_err());

        let c = claims(json!({
            "iss": ISSUER, "sub": "abc", "aud": ["soulbeet"], "email": "a@example.com",
            "email_verified": true
        }));
        assert_eq!(
            Identity::from_claims(&c, "email").unwrap().username,
            "a@example.com"
        );
    }

    /// A throwaway Ed25519 key, only for these tests.
    const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIB+2XiVDwrLs+fb3j6GY6K1Ltn7QqMeBcNI5DUjMxJ9e
-----END PRIVATE KEY-----";
    const TEST_KEY_X: &str = "N2BuOb1DeE-Kl98L_u7momt6p9k_aNeU1z6oJcR7KOU";
    const ISSUER: &str = "https://id.example.com";

    fn test_provider() -> Provider {
        Provider {
            issuer: ISSUER.to_string(),
            authorization_endpoint: url::Url::parse("https://id.example.com/authorize").unwrap(),
            token_endpoint: "https://id.example.com/token".to_string(),
            client_auth: ClientAuth::Basic,
            keys: serde_json::from_value(json!({ "keys": [
                { "kty": "OKP", "kid": "k1", "crv": "Ed25519", "x": TEST_KEY_X },
            ]}))
            .unwrap(),
            fetched_at: Instant::now(),
        }
    }

    fn test_config() -> OidcConfig {
        OidcConfig {
            issuer_url: ISSUER.to_string(),
            client_id: "soulbeet".to_string(),
            client_secret: None,
            redirect_url: "https://soulbeet.example.com/api/auth/oidc/callback".to_string(),
            scopes: "openid profile".to_string(),
            username_claim: "preferred_username".to_string(),
            provider_name: "Test".to_string(),
            auto_register: true,
        }
    }

    fn sign(kid: &str, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_string());
        let key = jsonwebtoken::EncodingKey::from_ed_pem(TEST_KEY_PEM.as_bytes()).unwrap();
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn valid_claims() -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "iss": ISSUER, "aud": "soulbeet", "sub": "abc", "nonce": "n0nce",
            "iat": now, "exp": now + 300, "preferred_username": "alice",
        })
    }

    fn verify(token: &str) -> Result<IdClaims, TokenError> {
        verify_id_token(&test_provider(), &test_config(), token, "n0nce")
    }

    #[test]
    fn verifies_a_valid_id_token() {
        let claims = verify(&sign("k1", valid_claims())).unwrap();
        assert_eq!(claims.sub, "abc");
        assert_eq!(claims.other["preferred_username"], "alice");
    }

    #[test]
    fn rejects_bad_id_tokens() {
        let with = |key: &str, value: Value| {
            let mut claims = valid_claims();
            claims[key] = value;
            sign("k1", claims)
        };
        let invalid = |token: String| matches!(verify(&token), Err(TokenError::Invalid(_)));

        assert!(invalid(with("nonce", json!("replayed"))));
        assert!(invalid(with("aud", json!("another-client"))));
        assert!(invalid(with("iss", json!("https://evil.example.com"))));
        assert!(invalid(with(
            "exp",
            json!(chrono::Utc::now().timestamp() - 3600)
        )));
        assert!(invalid(with("azp", json!("another-client"))));
        assert!(invalid(with("aud", json!(["soulbeet", "another-client"]))));
        assert!(verify(&with("aud", json!(["soulbeet"]))).is_ok());

        let mut claims = valid_claims();
        claims.as_object_mut().unwrap().remove("nonce");
        assert!(invalid(sign("k1", claims)));

        // Signed by a key the cache does not have: the caller refreshes.
        assert!(matches!(
            verify(&sign("rotated", valid_claims())),
            Err(TokenError::UnknownKey)
        ));

        // A tampered payload breaks the signature.
        let token = sign("k1", valid_claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&with_username(valid_claims(), "admin")).unwrap());
        parts[1] = &forged;
        assert!(invalid(parts.join(".")));
    }

    #[test]
    fn rejects_symmetric_and_unsigned_tokens() {
        let hs256 = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &valid_claims(),
            &jsonwebtoken::EncodingKey::from_secret(b"client-secret"),
        )
        .unwrap();
        assert!(matches!(verify(&hs256), Err(TokenError::Invalid(_))));

        let none = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(valid_claims().to_string())
        );
        assert!(matches!(verify(&none), Err(TokenError::Invalid(_))));
    }

    fn with_username(mut claims: Value, username: &str) -> Value {
        claims["preferred_username"] = json!(username);
        claims
    }

    fn identity(subject: &str, username: &str) -> Identity {
        identity_at(ISSUER, subject, username)
    }

    fn identity_at(issuer: &str, subject: &str, username: &str) -> Identity {
        Identity {
            issuer: issuer.to_string(),
            subject: subject.to_string(),
            username: username.to_string(),
        }
    }

    async fn sign_in(db: &SqlitePool, subject: &str, username: &str) -> Result<User, SsoError> {
        find_or_create_account(db, &identity(subject, username), true).await
    }

    #[tokio::test]
    async fn creates_an_account_then_finds_it_by_subject() {
        let db = test_db::migrated().await;
        let created = sign_in(&db, "sub-a", "alice").await.unwrap();
        assert!(!created.has_password());
        // Renamed at the provider: still the same account.
        let again = sign_in(&db, "sub-a", "alice-renamed").await.unwrap();
        assert_eq!(again.id, created.id);
        assert_eq!(again.username, "alice");
    }

    #[tokio::test]
    async fn the_same_subject_at_another_issuer_is_another_identity() {
        // Providers such as Gitea number their users, so after a switch of
        // provider the new one's user "1" must not get the old one's account.
        let db = test_db::migrated().await;
        let alice = sign_in(&db, "1", "alice").await.unwrap();
        assert_eq!(alice.oidc_issuer.as_deref(), Some(ISSUER));

        let other = "https://other.example.com";
        let stranger = find_or_create_account(&db, &identity_at(other, "1", "mallory"), true)
            .await
            .unwrap();
        assert_ne!(stranger.id, alice.id);
        // Under alice's name, it is not let in either.
        assert_eq!(
            find_or_create_account(&db, &identity_at(other, "2", "alice"), true)
                .await
                .unwrap_err(),
            SsoError::Conflict
        );
        assert_eq!(sign_in(&db, "1", "alice").await.unwrap().id, alice.id);
    }

    #[tokio::test]
    async fn a_new_account_is_a_plain_user() {
        // The seeded admin is there.
        let db = test_db::migrated().await;
        let user = sign_in(&db, "sub-b", "bob").await.unwrap();
        assert_eq!(user.role, crate::auth::Role::User);
    }

    #[tokio::test]
    async fn links_an_existing_account_once_ignoring_case() {
        let db = test_db::migrated().await;
        let local = User::create_in(&db, "alice", "pw").await.unwrap();
        let linked = sign_in(&db, "sub-a", "Alice").await.unwrap();
        assert_eq!(linked.id, local.id);
        assert_eq!(linked.oidc_subject.as_deref(), Some("sub-a"));
        // Linking ends the sessions the account had.
        assert_eq!(linked.credentials_version, local.credentials_version + 1);
        assert!(linked.password_matches("pw"));

        // Another identity with that name does not get in.
        assert_eq!(
            sign_in(&db, "sub-other", "alice").await.unwrap_err(),
            SsoError::Conflict
        );
    }

    #[tokio::test]
    async fn refuses_unknown_names_without_auto_register() {
        let db = test_db::migrated().await;
        let refused = find_or_create_account(&db, &identity("sub-c", "carol"), false).await;
        assert_eq!(refused.unwrap_err(), SsoError::NotRegistered);
        assert!(matches!(
            User::find_by_name_in(&db, "carol").await.unwrap(),
            NameMatch::None
        ));
    }

    #[test]
    fn several_accounts_of_the_name_are_a_conflict() {
        assert!(matches!(
            choose_account(None, NameMatch::Several, true),
            Account::Conflict
        ));
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

    #[test]
    fn loggable_escapes_newlines_and_truncates() {
        let logged = loggable("x\nINFO forged line");
        assert!(!logged.contains('\n'));
        assert!(logged.contains("\\n"));
        assert!(loggable(&"a".repeat(1000)).len() < 210);
    }
}
