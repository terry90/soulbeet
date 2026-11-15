//! The OAuth 2.1 authorization server Claude connects through.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use url::Url;

use crate::base_url::BaseUrl;
use crate::connection::{ConnectionState, RequestBase};

pub(crate) mod authorize;
pub(crate) mod cimd;
pub(crate) mod clients;
pub(crate) mod codes;
pub(crate) mod consent;
pub(crate) mod limits;
pub(crate) mod metadata;
pub(crate) mod register;
pub(crate) mod secrets;

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
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/oauth/register", post(register::register))
        .route(
            shared::oauth::AUTHORIZE_PATH,
            get(authorize::show).post(authorize::answer),
        )
}

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
