//! Shared helpers for the connection router's tests.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;

use super::ConnectionState;
use crate::oauth::limits::Limits;
use crate::secret_key::SecretKey;

impl ConnectionState {
    /// MCP on, no `PUBLIC_URL`, no gate reasons, the default limits, `test_key()`.
    pub(crate) fn for_test(db: SqlitePool) -> ConnectionState {
        ConnectionState {
            mcp_enabled: true,
            public_url: None,
            gate: Vec::new,
            db,
            limits: Arc::new(Limits::default()),
            key: test_key(),
        }
    }
}

/// A named host: with no proxy hint it derives `https://music.example.com`.
pub(crate) const HOST: &str = "music.example.com";

/// Claude's redirect URI (claude.com `connectors/building/authentication.md:211`).
pub(crate) const CLAUDE_CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";

/// The S256 code challenge of RFC 7636 appendix B.
pub(crate) const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// The key `ConnectionState::for_test` signs and checks sessions with.
pub(crate) fn test_key() -> &'static SecretKey {
    static KEY: OnceLock<SecretKey> = OnceLock::new();
    KEY.get_or_init(|| SecretKey::new("k".repeat(32)))
}

/// A user row with a password nobody can sign in with.
pub(crate) async fn add_user(db: &SqlitePool, user_id: &str, username: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
        .bind(user_id)
        .bind(username)
        .execute(db)
        .await
        .unwrap();
}

/// Adds the user and answers the `Cookie` header value of their session: the
/// cookie `issue_session_cookie` sets after a login.
pub(crate) async fn signed_in(db: &SqlitePool, user_id: &str, username: &str) -> String {
    add_user(db, user_id, username).await;
    format!(
        "{}={}",
        crate::AUTH_COOKIE_NAME,
        crate::auth::create_token(test_key(), user_id, 0).unwrap()
    )
}

/// A `GET` of `path` on `HOST` with `cookie`; the body as text.
pub(crate) async fn get(
    app: Router,
    path: &str,
    cookie: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::get(path).header("host", HOST);
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    send_text(app, req.body(Body::empty()).unwrap()).await
}

/// A `POST` of `pairs` as `application/x-www-form-urlencoded` to `path`, with `headers`.
pub(crate) fn form_request(
    path: &str,
    headers: &[(&str, &str)],
    pairs: &[(&str, &str)],
) -> Request<Body> {
    let mut req = Request::post(path).header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    req.body(Body::from(body)).unwrap()
}

/// `form_request` of `pairs` to `path` on `HOST` with `cookie`; the body as text.
pub(crate) async fn post_form(
    app: Router,
    path: &str,
    cookie: Option<&str>,
    pairs: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut headers = vec![("host", HOST)];
    if let Some(cookie) = cookie {
        headers.push(("cookie", cookie));
    }
    send_text(app, form_request(path, &headers, pairs)).await
}

async fn send_text(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let (status, headers, body) = send(app, req).await;
    let text = String::from_utf8(body.to_vec()).expect("the body is UTF-8");
    (status, headers, text)
}

/// The request id the consent page's form carries.
pub(crate) fn request_id_of(page: &str) -> String {
    let (_, rest) = page
        .split_once(r#"name="request" value=""#)
        .unwrap_or_else(|| panic!("no request id in the page: {page}"));
    rest.split('"').next().unwrap().to_string()
}

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
