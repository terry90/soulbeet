//! The OAuth 2.1 authorization server Claude connects through.

use std::sync::Arc;

use axum::routing::get;
use axum::{Extension, Json, Router};
use serde_json::Value;

use crate::connection::{ConnectionState, RequestBase};

pub(crate) mod metadata;

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
}

async fn protected_resource(Extension(RequestBase(base)): Extension<RequestBase>) -> Json<Value> {
    Json(metadata::protected_resource(&base))
}

async fn authorization_server(Extension(RequestBase(base)): Extension<RequestBase>) -> Json<Value> {
    Json(metadata::authorization_server(&base))
}
