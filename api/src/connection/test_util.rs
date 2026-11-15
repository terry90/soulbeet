//! Shared helpers for the connection router's tests.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;

use super::ConnectionState;
use crate::oauth::limits::Limits;

impl ConnectionState {
    /// MCP on, no `PUBLIC_URL`, no gate reasons, the default limits.
    pub(crate) fn for_test(db: SqlitePool) -> ConnectionState {
        ConnectionState {
            mcp_enabled: true,
            public_url: None,
            gate: Vec::new,
            db,
            limits: Arc::new(Limits::default()),
        }
    }
}

/// A named host: with no proxy hint it derives `https://music.example.com`.
pub(crate) const HOST: &str = "music.example.com";

/// Claude's redirect URI (claude.com `connectors/building/authentication.md:211`).
pub(crate) const CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";

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
    send_json(app, req.body(Body::empty()).unwrap()).await
}

/// A `POST` of `body` as `application/json` to `path` on `HOST`; the answer
/// parsed as `get_json` parses it.
pub(crate) async fn post_json(
    app: Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let req = Request::post(path)
        .header("host", HOST)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send_json(app, req).await
}

async fn send_json(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
    let (status, headers, body) = send(app, req).await;
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

/// The rows in `table`, a test's literal table name.
pub(crate) async fn count(db: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(db)
        .await
        .unwrap()
}
