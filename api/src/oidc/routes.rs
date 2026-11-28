//! The routes of a sign-in, and the start of a link from Settings.

use axum::{
    extract::Query,
    http::{HeaderMap, Uri},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tower_cookies::Cookies;

use super::account::{link_from_session, sign_in_account};
use super::flow::{
    authorization_url, flow_cookie, new_flow, seal_flow, take_flow, Flow, Purpose, Start,
};
use super::loggable;
use super::provider::{ProviderCache, PROVIDER};
use super::token::{exchange_code, verify_id_token, Identity, TokenError};
use crate::auth::{SsoError, SsoLinkResult};
use crate::base_url::BaseUrl;
use crate::config::{OidcConfig, CONFIG};
use crate::db::DB;
use crate::secret_key::SecretKey;
use crate::server_fns::auth::{issue_session_cookie, AUTH_COOKIE_NAME};
use crate::server_fns::guard::load_session;

/// Back to the login page, which shows what went wrong, still on the way to
/// `return_to` when the sign-in was.
fn to_login(error: SsoError, return_to: Option<&str>) -> Response {
    Redirect::to(&error.location(return_to)).into_response()
}

#[derive(Deserialize)]
pub(super) struct LoginParams {
    return_to: Option<String>,
    /// Sent by the move to the pinned address
    #[serde(default)]
    moved: bool,
}

/// `GET /api/auth/oidc/login`: send the browser to the provider.
pub(super) async fn login(
    cookies: Cookies,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<LoginParams>,
) -> Response {
    start_sign_in(&cookies, &headers, &uri, params).await
}

/// `POST /api/auth/oidc/login`, the login page's button: the same, and the
/// browser sends its Origin, which tells a page on plain http, as for a
/// password login.
pub(super) async fn login_form(
    cookies: Cookies,
    headers: HeaderMap,
    uri: Uri,
    Form(params): Form<LoginParams>,
) -> Response {
    start_sign_in(&cookies, &headers, &uri, params).await
}

async fn start_sign_in(
    cookies: &Cookies,
    headers: &HeaderMap,
    uri: &Uri,
    params: LoginParams,
) -> Response {
    // Only a request Soulbeet itself may send the browser back to.
    let return_to = params
        .return_to
        .as_deref()
        .and_then(shared::oauth::accepted_return_to)
        .map(str::to_owned);
    let Some(config) = CONFIG.oidc() else {
        return to_login(SsoError::Disabled, return_to.as_deref());
    };
    let started = Start::for_request_with_proxy(
        config.redirect_pin.as_ref(),
        CONFIG.public_url(),
        headers,
        uri,
        params.moved,
        crate::config::trust_proxy_headers(),
    );
    let begun = match started {
        // Started there, the flow cookie is where the callback reads it.
        Ok(start) if start.elsewhere => {
            return Redirect::to(&start.login_url(return_to.as_deref())).into_response();
        }
        Ok(start) => {
            let purpose = Purpose::SignIn {
                return_to: return_to.clone(),
            };
            begin(cookies, config, purpose, &start).await
        }
        Err(e) => Err(e),
    };
    match begun {
        Ok(url) => Redirect::to(url.as_str()).into_response(),
        Err(e) => {
            tracing::error!("OIDC sign-in failed: {e}");
            to_login(SsoError::Failed, return_to.as_deref())
        }
    }
}

/// Why linking cannot start.
pub(crate) enum LinkStartError {
    /// The provider sends the browser back to this address, and the page
    /// is at another one, where the account's session is
    Elsewhere(BaseUrl),
    Failed(String),
}

/// Starts linking the signed-in account `user_id` to an identity at the
/// provider from the request `headers` and `uri`: the address to send the
/// browser to. The callback links it only while this browser is still
/// signed in to that account.
pub(crate) async fn begin_link(
    cookies: &Cookies,
    config: &OidcConfig,
    user_id: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<url::Url, LinkStartError> {
    let start = Start::for_request_with_proxy(
        config.redirect_pin.as_ref(),
        CONFIG.public_url(),
        headers,
        uri,
        false,
        crate::config::trust_proxy_headers(),
    )
    .map_err(LinkStartError::Failed)?;
    if start.elsewhere {
        return Err(LinkStartError::Elsewhere(start.callback));
    }
    let purpose = Purpose::Link {
        user_id: user_id.to_string(),
    };
    begin(cookies, config, purpose, &start)
        .await
        .map_err(LinkStartError::Failed)
}

/// Seals a new flow for `purpose` in this browser's cookie and returns the
/// provider's address to send the browser to.
async fn begin(
    cookies: &Cookies,
    config: &OidcConfig,
    purpose: Purpose,
    start: &Start,
) -> Result<url::Url, String> {
    let provider = PROVIDER
        .provider(config, false)
        .await
        .map_err(|e| format!("cannot reach the provider: {e}"))?;

    let flow = new_flow(purpose, start);
    let sealed = seal_flow(&flow, crate::secret_key::current())?;
    cookies.add(flow_cookie(&flow.state, sealed, &flow.session));
    Ok(authorization_url(&provider, config, &flow))
}

#[derive(Deserialize)]
pub(super) struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /api/auth/oidc/callback`: the provider sends the browser back here.
pub(super) async fn callback(cookies: Cookies, Query(params): Query<CallbackParams>) -> Response {
    // Without a flow, there is no app request to keep.
    let Some(config) = CONFIG.oidc() else {
        return to_login(SsoError::Disabled, None);
    };
    let key = crate::secret_key::current();
    let flow = match take_flow(&cookies, params.state.as_deref(), key) {
        Ok(flow) => flow,
        Err(error) => {
            let token = cookies.get(AUTH_COOKIE_NAME).map(|c| c.value().to_string());
            let to = without_flow(&DB, key, token.as_deref(), error).await;
            return Redirect::to(&to).into_response();
        }
    };
    let identity = verified_identity(&PROVIDER, config, &flow, params).await;
    let session = flow.session;

    // `return_to`: the app request a failed sign-in still goes back to.
    let (user, to, return_to) = match flow.purpose {
        Purpose::SignIn { return_to } => {
            let user = match identity {
                Ok(identity) => sign_in_account(&identity, config).await,
                Err(error) => Err(error),
            };
            match user {
                Ok(user) => {
                    tracing::info!("User {} signed in through OIDC", loggable(&user.username));
                    let to = return_to.clone().unwrap_or_else(|| "/".to_string());
                    (user, to, return_to)
                }
                Err(error) => return to_login(error, return_to.as_deref()),
            }
        }
        Purpose::Link { user_id } => {
            let token = cookies.get(AUTH_COOKIE_NAME).map(|c| c.value().to_string());
            let linked = match identity {
                Ok(identity) => {
                    link_from_session(&DB, key, token.as_deref(), &user_id, &identity).await
                }
                Err(SsoError::Denied) => Err(SsoLinkResult::Denied),
                Err(_) => Err(SsoLinkResult::Failed),
            };
            match linked {
                Ok(user) => (user, SsoLinkResult::Linked.location(), None),
                Err(result) => return Redirect::to(&result.location()).into_response(),
            }
        }
    };
    // A link ended the account's sessions: this browser gets a new one.
    match issue_session_cookie(&cookies, &session, &user) {
        Ok(()) => Redirect::to(&to).into_response(),
        Err(e) => {
            tracing::error!("OIDC sign-in failed: cannot start a session: {e}");
            to_login(SsoError::Failed, return_to.as_deref())
        }
    }
}

/// Where a callback goes that this browser holds no flow for (it expired,
/// was sealed with another key, or started at another address), with the
/// session `session_token` the browser sent. Signed in, the browser was
/// linking from Settings, so the outcome is shown there; else on the login
/// page.
async fn without_flow(
    db: &sqlx::SqlitePool,
    key: &SecretKey,
    session_token: Option<&str>,
    error: SsoError,
) -> String {
    if session_token.is_some() && load_session(session_token, db, key).await.is_ok() {
        SsoLinkResult::Failed.location()
    } else {
        error.location(None)
    }
}

/// Who the provider says signed in, for the flow `flow`, with the provider
/// as `providers` holds it.
async fn verified_identity(
    providers: &ProviderCache,
    config: &OidcConfig,
    flow: &Flow,
    params: CallbackParams,
) -> Result<Identity, SsoError> {
    if let Some(error) = params.error {
        tracing::warn!(
            "OIDC sign-in refused by the provider: {}: {}",
            loggable(&error),
            loggable(&params.error_description.unwrap_or_default())
        );
        return Err(SsoError::Denied);
    }
    let code = params.code.ok_or_else(|| {
        tracing::warn!("OIDC sign-in failed: the provider sent no code");
        SsoError::Failed
    })?;

    let failed = |e: String| {
        tracing::error!("OIDC sign-in failed: {e}");
        SsoError::Failed
    };

    let provider = providers.provider(config, false).await.map_err(failed)?;
    let id_token = exchange_code(&provider, config, &code, flow)
        .await
        .map_err(failed)?;
    let claims = match verify_id_token(&provider, config, &id_token, &flow.nonce) {
        Err(TokenError::UnknownKey) => {
            // The provider may have rotated its keys since they were cached.
            let provider = providers.provider(config, true).await.map_err(failed)?;
            verify_id_token(&provider, config, &id_token, &flow.nonce)
        }
        result => result,
    }
    .map_err(|e| failed(format!("invalid ID token: {e}")))?;

    Identity::from_claims(&claims, &config.username_claim).map_err(|e| {
        tracing::warn!("OIDC sign-in refused: {e}");
        SsoError::Failed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc::testing::{sign_id_token, test_flow, StubProvider};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The provider's answer to a sign-in: a code.
    fn with_code() -> CallbackParams {
        CallbackParams {
            code: Some("the-code".to_string()),
            state: None,
            error: None,
            error_description: None,
        }
    }

    /// The provider `stub` as a sign-in cached it a minute ago, when it
    /// published the key `kid`.
    fn cached_a_minute_ago(stub: &StubProvider, kid: &str) -> ProviderCache {
        let fetched_at = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        ProviderCache::holding(Some(Arc::new(stub.copy(&[kid], fetched_at))), None)
    }

    /// A sign-in as `username` whose ID token the provider signs with `kid`.
    async fn sign_in(
        stub: &StubProvider,
        providers: &ProviderCache,
        kid: &str,
        username: &str,
    ) -> Result<Identity, SsoError> {
        let flow = test_flow();
        stub.answer_id_token(sign_id_token(kid, stub.claims(&flow, username)));
        verified_identity(providers, &stub.config(), &flow, with_code()).await
    }

    #[test]
    fn the_move_to_the_pinned_address_keeps_the_request_and_says_it_moved() {
        // A bookmarked link to the login route on the LAN address, with
        // OIDC_REDIRECT_URL at the public name.
        let pin = BaseUrl::parse("https://music.example.com").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "192.168.1.5:9765".parse().unwrap(),
        );
        let login = Uri::from_static(crate::auth::OIDC_LOGIN_PATH);
        let start = Start::for_request(Some(&pin), None, &headers, &login, false).unwrap();
        assert!(start.elsewhere);

        let back = "/oauth/authorize?client_id=c&state=a%26b";
        let moved: Uri = start.login_url(Some(back)).parse().unwrap();
        let Query(params) = Query::<LoginParams>::try_from_uri(&moved).unwrap();
        assert!(params.moved);
        assert_eq!(params.return_to.as_deref(), Some(back));
        // Any other request to the login route is not the move.
        let Query(params) = Query::<LoginParams>::try_from_uri(&login).unwrap();
        assert!(!params.moved);
    }

    #[tokio::test]
    async fn a_link_whose_flow_is_gone_ends_in_settings_while_signed_in() {
        use crate::db::test_db;
        use crate::models::user::User;

        let db = test_db::migrated().await;
        let key = SecretKey::new("k".repeat(32));
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let session = crate::auth::create_token(&key, &ann.id, 0).unwrap();
        assert_eq!(
            without_flow(&db, &key, Some(&session), SsoError::Failed).await,
            "/settings/sso/failed"
        );
        // Signed out, or with a session that ended or does not verify: a
        // sign-in, shown on the login page.
        let other_key =
            crate::auth::create_token(&SecretKey::new("x".repeat(32)), &ann.id, 0).unwrap();
        User::update_password_in(&db, &ann.id, "new").await.unwrap();
        for token in [None, Some(session.as_str()), Some(other_key.as_str())] {
            assert_eq!(
                without_flow(&db, &key, token, SsoError::Failed).await,
                "/login/error/failed"
            );
        }
    }

    #[tokio::test]
    async fn a_rotated_key_is_fetched_once_and_the_sign_in_goes_through() {
        let stub = StubProvider::start(&["new"]).await;
        let providers = cached_a_minute_ago(&stub, "old");

        // Signed with a key the cached copy does not have: the provider's
        // keys are fetched again, and the token verifies with them.
        let identity = sign_in(&stub, &providers, "new", "alice").await.unwrap();
        assert_eq!(identity.username, "alice");
        assert_eq!(identity.issuer, stub.issuer);
        assert_eq!(stub.fetches(), 1);

        // The new key is cached now.
        let identity = sign_in(&stub, &providers, "new", "bob").await.unwrap();
        assert_eq!(identity.username, "bob");
        assert_eq!(stub.fetches(), 1);

        // A key the provider does not publish: refused, without asking a
        // provider that was asked moments ago again.
        assert_eq!(
            sign_in(&stub, &providers, "forged", "mallory").await,
            Err(SsoError::Failed)
        );
        assert_eq!(stub.fetches(), 1);
    }

    #[tokio::test]
    async fn a_provider_down_at_a_key_rotation_is_asked_again_only_after_the_backoff() {
        let stub = StubProvider::start(&["new"]).await;
        let providers = cached_a_minute_ago(&stub, "old");
        stub.set_down(true);

        // The new keys cannot be fetched: the sign-in fails, and the ones
        // right after it do not ask the provider again.
        for _ in 0..3 {
            assert_eq!(
                sign_in(&stub, &providers, "new", "alice").await,
                Err(SsoError::Failed)
            );
        }
        assert_eq!(stub.fetches(), 1);
        // Meanwhile a token signed with a cached key still signs in.
        let identity = sign_in(&stub, &providers, "old", "bob").await.unwrap();
        assert_eq!(identity.username, "bob");
        assert_eq!(stub.fetches(), 1);

        // After the backoff, the next sign-in asks again and gets the keys.
        stub.set_down(false);
        providers.end_backoff();
        let identity = sign_in(&stub, &providers, "new", "alice").await.unwrap();
        assert_eq!(identity.username, "alice");
        assert_eq!(stub.fetches(), 2);
    }
}
