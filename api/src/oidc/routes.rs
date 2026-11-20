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
use super::flow::{authorization_url, flow_cookie, new_flow, take_flow, Flow, Purpose, Start};
use super::loggable;
use super::provider::provider;
use super::token::{exchange_code, verify_id_token, Identity, TokenError};
use crate::auth::{SsoError, SsoLinkResult};
use crate::base_url::BaseUrl;
use crate::config::{OidcConfig, CONFIG};
use crate::crypto;
use crate::db::DB;
use crate::server_fns::auth::{issue_session_cookie, AUTH_COOKIE_NAME};

/// Back to the login page, which shows what went wrong, still on the way to
/// `return_to` when the sign-in was.
fn to_login(error: SsoError, return_to: Option<&str>) -> Response {
    Redirect::to(&error.location(return_to)).into_response()
}

#[derive(Deserialize)]
pub(super) struct LoginParams {
    return_to: Option<String>,
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
    let started = Start::for_request(
        config.redirect_pin.as_ref(),
        CONFIG.public_url(),
        headers,
        uri,
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
    let start = Start::for_request(
        config.redirect_pin.as_ref(),
        CONFIG.public_url(),
        headers,
        uri,
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
    let provider = provider(config, false)
        .await
        .map_err(|e| format!("cannot reach the provider: {e}"))?;

    let flow = new_flow(purpose, start);
    let sealed = serde_json::to_string(&flow)
        .map_err(|e| e.to_string())
        .and_then(|json| crypto::encrypt(crate::secret_key::current(), &json))
        .map_err(|e| format!("cannot seal the login state: {e}"))?;

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
    let flow = match take_flow(&cookies, params.state.as_deref()) {
        Ok(flow) => flow,
        Err(error) => return to_login(error, None),
    };
    let identity = verified_identity(config, &flow, params).await;
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
                    let key = crate::secret_key::current();
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

/// Who the provider says signed in, for the flow `flow`.
async fn verified_identity(
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

    let provider = provider(config, false).await.map_err(failed)?;
    let id_token = exchange_code(&provider, config, &code, flow)
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

    Identity::from_claims(&claims, &config.username_claim).map_err(|e| {
        tracing::warn!("OIDC sign-in refused: {e}");
        SsoError::Failed
    })
}
