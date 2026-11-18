//! The OAuth 2.1 authorization server MCP clients connect through.

use std::sync::Arc;
use std::time::Duration;

use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use url::Url;

use crate::base_url::BaseUrl;
use crate::connection::{refusal, ConnectionState, RequestBase};

pub(crate) mod authorize;
pub(crate) mod cimd;
pub(crate) mod clients;
pub(crate) mod codes;
pub(crate) mod consent;
pub(crate) mod grants;
pub(crate) mod limits;
pub(crate) mod metadata;
pub(crate) mod register;
pub(crate) mod secrets;
pub(crate) mod token;

/// The scope every grant carries.
pub(crate) const SCOPE: &str = "soulful";
/// The scope a client asks for to get a refresh token.
pub(crate) const OFFLINE_ACCESS: &str = "offline_access";

/// The OAuth routes, merged into the connection router behind its guard.
/// The root protected resource path answers the same document as the
/// RFC 9728 path-insertion URL of `/mcp`.
pub(crate) fn routes() -> Router<Arc<ConnectionState>> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource).fallback(only_get),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource).fallback(only_get),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server).fallback(only_get),
        )
        .route(
            "/oauth/register",
            post(register::register).fallback(only_post),
        )
        .route(
            shared::oauth::AUTHORIZE_PATH,
            get(authorize::show)
                .post(authorize::answer)
                .fallback(|| method_not_allowed("Only GET and POST are served here")),
        )
        .route("/oauth/token", post(token::token).fallback(only_post))
        .route("/oauth/revoke", post(token::revoke).fallback(only_post))
}

/// How often `start_prune_task` prunes (D4).
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// An OAuth error answer, `{"error", "error_description"}` (RFC 6749
/// section 5.2, RFC 7591 section 3.2.2).
pub(crate) fn oauth_error(
    status: StatusCode,
    error: &str,
    description: impl Into<String>,
) -> Response {
    let body = json!({ "error": error, "error_description": description.into() });
    (status, Json(body)).into_response()
}

/// A 429 `rate_limited` answer with `Retry-After`, saying "{what}. Try
/// again in {n} seconds."
pub(crate) fn rate_limited(wait: Duration, what: &str) -> Response {
    let seconds = limits::retry_after_seconds(wait);
    let mut refused = oauth_error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("{what}. {}", limits::try_again_in(seconds)),
    );
    refused.headers_mut().insert(RETRY_AFTER, seconds.into());
    refused
}

/// The request's media type is `essence`, parameters aside.
pub(crate) fn has_media_type(headers: &HeaderMap, essence: &str) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|sent| sent.trim().eq_ignore_ascii_case(essence))
}

/// The parameters Soulful reads from a query or form. Following RFC 6749
/// sections 3.1 and 3.2, a parameter without a value counts as absent and
/// other parameters are ignored.
pub(crate) struct Params(Vec<(&'static str, String)>);

impl Params {
    /// The parameters of `raw` named in `names`; Err is the first one sent
    /// more than once.
    pub(crate) fn read(raw: &[u8], names: &[&'static str]) -> Result<Params, &'static str> {
        let mut found: Vec<(&'static str, String)> = Vec::new();
        for (name, value) in url::form_urlencoded::parse(raw) {
            let Some(&name) = names.iter().find(|n| **n == name) else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            if found.iter().any(|(seen, _)| *seen == name) {
                return Err(name);
            }
            found.push((name, value.into_owned()));
        }
        Ok(Params(found))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Prunes expired OAuth state from the database now and every hour (D4), and the delete
/// confirmation tokens that expired a day before.
pub fn start_prune_task() {
    tokio::spawn(async {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            match grants::prune_in(&crate::db::DB, now_unix()).await {
                Ok(grants::Pruned {
                    requests,
                    codes,
                    tokens,
                    grants,
                    clients,
                }) => tracing::debug!(
                    "Pruned OAuth state: {requests} consent requests, {codes} codes, \
                     {tokens} tokens, {grants} grants, {clients} clients"
                ),
                Err(e) => tracing::error!("Could not prune OAuth state: {e}"),
            }
            match crate::core::deletion::tokens::prune_in(&crate::db::DB, now_unix()).await {
                Ok(tokens) => tracing::debug!("Pruned {tokens} delete confirmation tokens"),
                Err(e) => tracing::error!("Could not prune delete confirmation tokens: {e}"),
            }
        }
    });
}

/// `sent` names the MCP endpoint at `base`: once parsed it is
/// `mcp_url(base)`, so the scheme and host may be written in any case and the
/// default port written out (MCP 2026-07-28 `basic/authorization/index.mdx`,
/// "Canonical Server URI"). Another path, a trailing slash, a query or a
/// fragment is another resource.
pub(crate) fn resource_matches(sent: &str, base: &BaseUrl) -> bool {
    Url::parse(sent).is_ok_and(|url| url.as_str() == metadata::mcp_url(base))
}

/// The clock of every OAuth handler, in unix seconds.
pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Another method than a route serves, in D1's shape: axum adds the
/// `Allow` header naming the methods it serves.
async fn method_not_allowed(message: &'static str) -> Response {
    refusal(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        message,
    )
}

async fn only_get() -> Response {
    method_not_allowed("Only GET is served here").await
}

async fn only_post() -> Response {
    method_not_allowed("Only POST is served here").await
}

async fn protected_resource(Extension(RequestBase(base)): Extension<RequestBase>) -> Json<Value> {
    Json(metadata::protected_resource(&base))
}

async fn authorization_server(Extension(RequestBase(base)): Extension<RequestBase>) -> Json<Value> {
    Json(metadata::authorization_server(&base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_matches_in_its_canonical_form_only() {
        let base = BaseUrl::parse("https://music.example.com").unwrap();
        for ok in [
            "https://music.example.com/mcp",
            "https://Music.Example.com:443/mcp",
            "HTTPS://MUSIC.EXAMPLE.COM/mcp",
        ] {
            assert!(resource_matches(ok, &base), "{ok}");
        }
        for bad in [
            "https://music.example.com/mcp/",
            "https://music.example.com/MCP",
            "https://music.example.com/mcp?x=1",
            "https://music.example.com/mcp#x",
            "https://music.example.com:8443/mcp",
            "http://music.example.com/mcp",
            "https://user@music.example.com/mcp",
            "not a url",
        ] {
            assert!(!resource_matches(bad, &base), "{bad}");
        }
    }
}
