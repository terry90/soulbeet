//! Shared helpers for the connection router's tests.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

use super::ConnectionState;

impl ConnectionState {
    /// MCP on, no `PUBLIC_URL`, no gate reasons.
    pub(crate) fn for_test() -> ConnectionState {
        ConnectionState {
            mcp_enabled: true,
            public_url: None,
            gate: Vec::new,
        }
    }
}

/// A named host: with no proxy hint it derives `https://music.example.com`.
pub(crate) const HOST: &str = "music.example.com";

/// Sends `req` to `app` and reads the whole answer, the body within 5 s.
pub(crate) async fn send(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = app.oneshot(req).await.expect("a router never fails");
    let (parts, body) = response.into_parts();
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(body, usize::MAX),
    )
    .await
    .expect("the body arrives within 5 s")
    .expect("the body reads");
    (parts.status, parts.headers, bytes)
}

/// A `GET` of `path` with `headers`; the body parsed as JSON, `Null` when empty.
pub(crate) async fn get_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let mut req = Request::get(path);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let (status, headers, body) = send(app, req.body(Body::empty()).unwrap()).await;
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|e| {
            panic!(
                "{status} body is not JSON ({e}): {}",
                String::from_utf8_lossy(&body)
            )
        })
    };
    (status, headers, json)
}
