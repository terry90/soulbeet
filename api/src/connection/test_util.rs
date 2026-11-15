//! Shared helpers for the connection router's tests.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;
use url::Url;

use super::{router, ConnectionState};
use crate::db::test_db;
use crate::oauth::cimd::{
    client_id_url, parse_document, CimdError, ClientDocument, ClientDocuments,
};
use crate::oauth::clients::{find_in, insert_dcr_in, NewClient, DCR_CAP};
use crate::oauth::codes::{store_code_in, PendingRequest};
use crate::oauth::grants::{redeem_code_in, CodeRedemption};
use crate::oauth::limits::Limits;
use crate::oauth::now_unix;
use crate::oauth::secrets::new_secret;
use crate::secret_key::SecretKey;

impl ConnectionState {
    /// MCP on, no `PUBLIC_URL`, no gate reasons, the default limits,
    /// `test_key()`, and no client metadata documents.
    pub(crate) fn for_test(db: SqlitePool) -> ConnectionState {
        ConnectionState {
            mcp_enabled: true,
            public_url: None,
            gate: Vec::new,
            db,
            limits: Arc::new(Limits::default()),
            key: test_key(),
            documents: FixedDocuments::new(vec![]),
        }
    }
}

/// A named host: with no proxy hint it derives `https://music.example.com`.
pub(crate) const HOST: &str = "music.example.com";

/// Claude's redirect URI (claude.com `connectors/building/authentication.md:211`).
pub(crate) const CLAUDE_CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";

// The document Claude Code serves, fetched 2026-11-15 from its client_id URL
// (cited by claude.com connectors/building/authentication.md:220).
pub(crate) const CLAUDE_CODE_DOCUMENT: &str = r#"{"client_id":"https://claude.ai/oauth/claude-code-client-metadata","client_name":"Claude Code","client_uri":"https://claude.ai","redirect_uris":["http://localhost/callback","http://127.0.0.1/callback"],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}"#;
pub(crate) const CLAUDE_CODE_ID: &str = "https://claude.ai/oauth/claude-code-client-metadata";

/// Claude Code's document as `parse_document` reads it at its URL.
pub(crate) fn claude_code() -> ClientDocument {
    let url = client_id_url(CLAUDE_CODE_ID).expect("Claude Code's client id is a client id URL");
    parse_document(&url, CLAUDE_CODE_DOCUMENT.as_bytes()).expect("Claude Code's document parses")
}

/// Answers each URL's document from a fixed list and counts the fetches.
pub(crate) struct FixedDocuments {
    docs: Vec<(String, Result<ClientDocument, CimdError>)>,
    calls: AtomicUsize,
}

impl FixedDocuments {
    pub(crate) fn new(docs: Vec<(&str, Result<ClientDocument, CimdError>)>) -> Arc<FixedDocuments> {
        Arc::new(FixedDocuments {
            docs: docs
                .into_iter()
                .map(|(url, doc)| (url.to_string(), doc))
                .collect(),
            calls: AtomicUsize::new(0),
        })
    }

    /// How many fetches were asked for.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ClientDocuments for FixedDocuments {
    async fn fetch(&self, url: &Url) -> Result<ClientDocument, CimdError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.docs
            .iter()
            .find(|(known, _)| known == url.as_str())
            .map(|(_, doc)| doc.clone())
            .unwrap_or_else(|| {
                Err(CimdError::Fetch(format!(
                    "no document for {url} in this test"
                )))
            })
    }
}

/// Serves `app` over TCP on a free loopback port.
pub(crate) async fn serve_on_loopback(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is free");
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the test server runs");
    });
    (addr, server)
}

/// The S256 code challenge of RFC 7636 appendix B.
pub(crate) const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// The code verifier of RFC 7636 appendix B, `CHALLENGE`'s.
pub(crate) const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

/// The MCP endpoint at `HOST`.
pub(crate) const RESOURCE: &str = "https://music.example.com/mcp";

/// A request reaching Soulful through a tunnel: it derives
/// `https://tunnel.example.net`.
pub(crate) const TUNNEL: &[(&str, &str)] = &[
    ("host", "soulful:9765"),
    ("x-forwarded-host", "tunnel.example.net"),
    ("x-forwarded-proto", "https"),
];

/// A code of `user_id` (who must exist) for the registered client `dcr_c`,
/// `CLAUDE_CALLBACK`, `CHALLENGE`, `scope` and `resource`, issued at `at`;
/// `dcr_c` is registered at `at` when it is not yet known.
pub(crate) async fn issue_code(
    db: &SqlitePool,
    user_id: &str,
    scope: &str,
    resource: &str,
    at: i64,
) -> String {
    if find_in(db, "dcr_c").await.unwrap().is_none() {
        let client = NewClient {
            client_name: Some("Claude".into()),
            redirect_uris: vec![CLAUDE_CALLBACK.into()],
        };
        insert_dcr_in(db, "dcr_c", &client, at, DCR_CAP)
            .await
            .unwrap();
    }
    let request = PendingRequest {
        user_id: user_id.into(),
        client: find_in(db, "dcr_c").await.unwrap().unwrap(),
        redirect_uri: CLAUDE_CALLBACK.into(),
        state: None,
        code_challenge: CHALLENGE.into(),
        scope: scope.into(),
        resource: resource.into(),
    };
    let code = new_secret();
    store_code_in(db, &code, &request, at).await.unwrap();
    code
}

/// The redemption of `code` that `issue_code` issued: `dcr_c`,
/// `CLAUDE_CALLBACK`, `VERIFIER`, `RESOURCE`.
pub(crate) fn good(code: &str) -> CodeRedemption<'_> {
    CodeRedemption {
        code,
        client_id: "dcr_c",
        redirect_uri: CLAUDE_CALLBACK,
        verifier: VERIFIER,
        resource: RESOURCE,
    }
}

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
    (status, headers, json_of(status, &body))
}

/// `body` parsed as JSON, `Null` when empty.
fn json_of(status: StatusCode, body: &[u8]) -> serde_json::Value {
    if body.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{status} body is not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    })
}

/// An `initialize` request of the 2025-11-25 revision.
pub(crate) const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

/// An access token of `user_id` (who must exist) for `resource`, issued now
/// through a code and its redemption.
pub(crate) async fn access_token(db: &SqlitePool, user_id: &str, resource: &str) -> String {
    let code = issue_code(db, user_id, "soulful offline_access", resource, now_unix()).await;
    let redemption = CodeRedemption {
        resource,
        ..good(&code)
    };
    redeem_code_in(db, &redemption, now_unix())
        .await
        .unwrap()
        .access_token
}

/// A router on a database where u1 (alice) exists, and u1's access token for music.example.com.
pub(crate) async fn mcp_app() -> (SqlitePool, Router, String) {
    let db = test_db::migrated().await;
    add_user(&db, "u1", "alice").await;
    let token = access_token(&db, "u1", RESOURCE).await;
    (db.clone(), router(ConnectionState::for_test(db)), token)
}

/// A `POST` of the JSON-RPC message `body` to `/mcp` with `headers`. The
/// answer's body is the JSON of the first non-empty SSE `data:` line, or the
/// body itself when it is not an event stream (`Null` when empty).
pub(crate) async fn post_rpc(
    app: Router,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let mut req = Request::post("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let (status, headers, body) = send(app, req.body(Body::from(body.to_string())).unwrap()).await;
    let is_stream = headers
        .get("content-type")
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    let json = if is_stream {
        let text = String::from_utf8(body.to_vec()).expect("an event stream is UTF-8");
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .find(|data| !data.is_empty())
            .map(|data| json_of(status, data.as_bytes()))
            .unwrap_or(serde_json::Value::Null)
    } else {
        json_of(status, &body)
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
