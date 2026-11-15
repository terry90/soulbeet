//! The OAuth 2.1 authorization server Claude connects through.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};

use crate::connection::{ConnectionState, RequestBase};

pub(crate) mod clients;
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
