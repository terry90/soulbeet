//! Shared helpers for the connection router's tests.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;
use url::Url;

use rmcp::model::CallToolResult;
use shared::failure::Failure;
use shared::system::{HealthIssue, HealthLevel, ImporterHealth, SystemHealth};
use soulbeet::navidrome::ScanStatus;

use super::log::RequestLog;
use super::{router, ConnectionState};
use crate::auth::{Role, UserCtx};
use crate::base_url::BaseUrl;
use crate::core::delete_guard::test_support::{FakeNavidrome, Library};
use crate::core::discovery::test_support::TestDiscovery;
use crate::core::index::NavidromeOf;
use crate::core::jobs::runner::{Acquire, TestEffects};
use crate::core::ratings::{sync_running, AutoDeletePermit, SyncReport};
use crate::core::scan::ScanBy;
use crate::db::test_db;
use crate::ledger::store::RunningBatch;
use crate::mcp::status::{FolderFacts, NavidromeFacts, StatusFacts, StatusSource};
use crate::mcp::sync::RatingSync;
use crate::mcp::McpCaller;
use crate::models::folder::Folder;
use crate::models::user_settings::{UpdateUserSettings, UserSettings};
use crate::oauth::cimd::{
    client_id_url, parse_document, CimdError, ClientDocument, ClientDocuments,
};
use crate::oauth::clients::{find_in, insert_dcr_in, NewClient, DCR_CAP};
use crate::oauth::codes::{store_code_in, PendingRequest, SessionCheck};
use crate::oauth::grants::{redeem_code_in, CodeRedemption};
use crate::oauth::limits::Limits;
use crate::oauth::now_unix;
use crate::oauth::secrets::new_secret;
use crate::secret_key::SecretKey;
use crate::services::NavidromeLookupError;
use crate::test_navidrome::NavidromeFor;

impl ConnectionState {
    /// `on` an `Acquire::for_test` of `db`.
    pub(crate) fn for_test(db: SqlitePool) -> ConnectionState {
        let acquire = Acquire::for_test(db.clone()).0;
        ConnectionState::on(db, acquire)
    }

    /// MCP on, no `PUBLIC_URL`, no gate reasons, the default limits,
    /// `test_key()`, no client metadata documents, `sample_facts()`, no
    /// user linked to Navidrome, no NAVIDROME_MUSIC_PATH, acting through
    /// `acquire`, whose ledger writer serves every state made on it,
    /// discovery on a `TestDiscovery`, a rating sync that is always
    /// already running, and the delete guard on an empty `FakeNavidrome`.
    pub(crate) fn on(db: SqlitePool, acquire: Arc<Acquire>) -> ConnectionState {
        ConnectionState {
            mcp_off: None,
            public_url: None,
            gate: Vec::new,
            db,
            limits: Arc::new(Limits::default()),
            key: test_key(),
            documents: FixedDocuments::new(vec![]),
            status: Arc::new(FixedStatus(Ok(sample_facts()))),
            navidrome: Arc::new(NavidromeFor(Err(|| NavidromeLookupError::NotLinked))),
            navidrome_music_path: None,
            log: Arc::new(RequestLog::default()),
            acquire,
            discovery: Arc::new(TestDiscovery::default()),
            ratings: Arc::new(FixedSync::new(Err(sync_running()))),
            listeners: Arc::new(FakeNavidrome::default()),
        }
    }
}

/// The router on `lib`'s database and fake Navidrome, and alice's access token (user id
/// "alice", whom `Library` made); her grant's deletion switch as `allow_delete` says.
pub(crate) async fn app_on_library(lib: &Library, allow_delete: bool) -> (Router, String) {
    let state = ConnectionState {
        listeners: lib.fake.clone(),
        ..ConnectionState::on(lib.db.clone(), Acquire::for_test(lib.db.clone()).0)
    };
    let token = access_token(&lib.db, "alice", RESOURCE).await;
    sqlx::query("UPDATE oauth_grants SET allow_delete = ? WHERE user_id = 'alice'")
        .bind(allow_delete)
        .execute(&lib.db)
        .await
        .unwrap();
    (router(state), token)
}

/// The headers `pairs` names, each set once.
pub(crate) fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
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

/// `bytes` sent in chunks, without a Content-Length to tell their size.
pub(crate) fn streamed(bytes: Vec<u8>) -> Body {
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        bytes.chunks(64 * 1024).map(|c| Ok(c.to_vec())).collect();
    Body::from_stream(futures::stream::iter(chunks))
}

/// An answer whose Content-Length announces `len` bytes and whose body
/// never comes, so only a reader that trusts the header finishes.
pub(crate) fn announced_never_sent(len: usize) -> axum::response::Response {
    let never = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
    let mut response = axum::response::Response::new(Body::from_stream(never));
    response
        .headers_mut()
        .insert(axum::http::header::CONTENT_LENGTH, HeaderValue::from(len));
    response
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

/// The session check of a session `signed_in` issued: credentials version
/// 0, never signed out.
pub(crate) fn fresh_session() -> SessionCheck {
    SessionCheck {
        credentials_version: 0,
        session_id: "fresh-session".into(),
    }
}

/// Runs `write` while another connection of `db` holds the write lock with
/// a write of its own, which it commits 200 ms later. A transaction that
/// reads before it takes the write lock fails behind it: SQLite answers its
/// first write SQLITE_BUSY at once instead of waiting, since waiting could
/// deadlock. One begun with BEGIN IMMEDIATE waits for the lock and succeeds.
/// `db` is a `test_db::migrated_file` database, with several connections.
pub(crate) async fn behind_a_writer<T: Send + 'static>(
    db: &SqlitePool,
    write: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    let mut writer = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("INSERT INTO revoked_sessions (jti, expires_at) VALUES (?, 0)")
        .bind(new_secret())
        .execute(&mut *writer)
        .await
        .unwrap();
    let write = tokio::spawn(write);
    tokio::time::sleep(Duration::from_millis(200)).await;
    writer.commit().await.unwrap();
    write.await.unwrap()
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
    alice_app(ConnectionState::for_test(test_db::migrated().await)).await
}

/// `mcp_app` with tools getting the caller's Navidrome from `navidrome`.
pub(crate) async fn mcp_app_with(navidrome: Arc<dyn NavidromeOf>) -> (SqlitePool, Router, String) {
    alice_app(ConnectionState {
        navidrome,
        ..ConnectionState::for_test(test_db::migrated().await)
    })
    .await
}

/// A router on `state`, whose database gains u1 (alice), and u1's access token for music.example.com.
pub(crate) async fn alice_app(state: ConnectionState) -> (SqlitePool, Router, String) {
    let db = state.db.clone();
    add_user(&db, "u1", "alice").await;
    let token = access_token(&db, "u1", RESOURCE).await;
    (db, router(state), token)
}

/// `mcp_app` on an `Acquire::for_test` of the same database: u1 (alice) with folder f1 (the test
/// effects' library folder, on disk while they live) as her default folder, the acquisition, its
/// test effects and her access token.
pub(crate) async fn app_with_acquire() -> (SqlitePool, Arc<Acquire>, Arc<TestEffects>, String) {
    let db = test_db::migrated().await;
    let (acquire, effects) = Acquire::for_test(db.clone());
    add_user(&db, "u1", "alice").await;
    add_folder(&db, "u1", "f1", "Music", effects.library()).await;
    set_default_folder(&db, "u1", Some("f1")).await;
    let token = access_token(&db, "u1", RESOURCE).await;
    (db, acquire, effects, token)
}

/// `user_id`'s folder `folder_id` named `name`, at `path`.
pub(crate) async fn add_folder(
    db: &SqlitePool,
    user_id: &str,
    folder_id: &str,
    name: &str,
    path: &std::path::Path,
) {
    sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?)")
        .bind(folder_id)
        .bind(user_id)
        .bind(name)
        .bind(path.to_str().unwrap())
        .execute(db)
        .await
        .unwrap();
}

/// Makes `folder_id` `user_id`'s default folder; None clears the choice, as Settings does.
pub(crate) async fn set_default_folder(db: &SqlitePool, user_id: &str, folder_id: Option<&str>) {
    UserSettings::upsert_in(
        db,
        user_id,
        UpdateUserSettings {
            default_download_folder_id: Some(folder_id.unwrap_or_default().to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// The jobs of `core::jobs::test_support` are alice's (user id `alice`): a database with her,
/// the acquisition acting on it, its effects and her access token.
pub(crate) async fn jobs_app() -> (SqlitePool, Arc<Acquire>, Arc<TestEffects>, String) {
    let (acq, effects) = Acquire::for_alice().await;
    let db = acq.db.clone();
    let token = access_token(&db, "alice", RESOURCE).await;
    (db, acq, effects, token)
}

/// The router of `ConnectionState::on(db, acquire)`.
pub(crate) fn app_of(db: &SqlitePool, acquire: &Arc<Acquire>) -> Router {
    router(ConnectionState::on(db.clone(), acquire.clone()))
}

/// `app_of(db, acquire)` with `navidrome` as the caller's Navidrome.
pub(crate) fn app_with_navidrome(
    db: &SqlitePool,
    acquire: &Arc<Acquire>,
    navidrome: Arc<dyn NavidromeOf>,
) -> Router {
    router(ConnectionState {
        navidrome,
        ..ConnectionState::on(db.clone(), acquire.clone())
    })
}

/// The id of `user_id`'s one grant, as it is: nothing is redeemed.
pub(crate) async fn grant_of(db: &SqlitePool, user_id: &str) -> String {
    sqlx::query_scalar("SELECT id FROM oauth_grants WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// The id of the grant `user_id` holds for `resource`, made with an access token of theirs.
pub(crate) async fn grant_id_of(db: &SqlitePool, user_id: &str, resource: &str) -> String {
    access_token(db, user_id, resource).await;
    sqlx::query_scalar("SELECT id FROM oauth_grants WHERE user_id = ? AND resource = ?")
        .bind(user_id)
        .bind(resource)
        .fetch_one(db)
        .await
        .unwrap()
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

/// A `tools/call` of `name` with `arguments` as the user the token is for;
/// the result object.
pub(crate) async fn call_tool(
    app: Router,
    token: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let bearer = format!("Bearer {token}");
    let (_, _, body) = post_rpc(
        app,
        &[
            ("host", HOST),
            ("authorization", &bearer),
            ("mcp-protocol-version", "2025-11-25"),
        ],
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": arguments}}),
    )
    .await;
    body["result"].clone()
}

/// The JSON document a tool result's first block holds.
pub(crate) fn document(result: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// The rows in `table`, a test's literal table name.
pub(crate) async fn count(db: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(db)
        .await
        .unwrap()
}

/// The text of a tool result's first block.
pub(crate) fn text_of(r: &CallToolResult) -> &str {
    r.content[0]
        .as_text()
        .expect("a tool result's first block is text")
        .text
        .as_str()
}

/// Answers a clone of its value for every user.
pub(crate) struct FixedStatus(pub Result<StatusFacts, Failure>);

#[async_trait]
impl StatusSource for FixedStatus {
    async fn gather(&self, _ctx: &UserCtx) -> Result<StatusFacts, Failure> {
        self.0.clone()
    }
}

/// Answers a clone of its value for every sync, recording each user id and permit it is
/// called with.
pub(crate) struct FixedSync {
    answer: Result<SyncReport, Failure>,
    calls: Mutex<Vec<(String, AutoDeletePermit)>>,
}

impl FixedSync {
    pub(crate) fn new(answer: Result<SyncReport, Failure>) -> FixedSync {
        FixedSync {
            answer,
            calls: Mutex::new(vec![]),
        }
    }

    /// Each call's user id and permit, in order.
    pub(crate) fn calls(&self) -> Vec<(String, AutoDeletePermit)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RatingSync for FixedSync {
    async fn sync(&self, ctx: &UserCtx, permit: AutoDeletePermit) -> Result<SyncReport, Failure> {
        self.calls
            .lock()
            .unwrap()
            .push((ctx.user_id.clone(), permit));
        self.answer.clone()
    }
}

/// A status where everything answers: slskd online, beets degraded by Soulful's
/// own probe issue, alice not a Navidrome admin and no linked account one, one
/// default folder inside the one known library root, one running batch.
pub(crate) fn sample_facts() -> StatusFacts {
    StatusFacts {
        health: SystemHealth {
            downloader_online: true,
            downloader_failure: None,
            downloaders: vec![shared::system::DownloaderHealth {
                id: "slskd".into(),
                name: "Soulseek".into(),
                online: true,
                failure: None,
            }],
            // Soulful's own beets probe issue, lib/soulbeet/src/beets/probe.rs:121-125
            importer: ImporterHealth {
                level: HealthLevel::Degraded,
                facts: vec![],
                issues: vec![HealthIssue {
                    severity: HealthLevel::Degraded,
                    title: "beets config check timed out".into(),
                    detail: "A plugin may be waiting on the network or for input.".into(),
                }],
            },
            navidrome_online: true,
            navidrome_failure: None,
        },
        navidrome: Some(NavidromeFacts {
            admin: Ok(false),
            scan_by: Ok(ScanBy::Nobody),
            scan: Ok(ScanStatus {
                scanning: false,
                count: 1342,
                // the lastScan of Navidrome's answer in the client tests
                // (lib/soulbeet/src/navidrome/client.rs SCAN_STATUS)
                last_scan: "2026-11-14T21:03:11.519322Z".parse().unwrap(),
                error: String::new(),
                elapsed_time: 0,
            }),
        }),
        folders: vec![FolderFacts {
            folder: Folder {
                id: "f1".into(),
                user_id: "u1".into(),
                name: "Music".into(),
                path: "/music/alice".into(),
                shared: false,
            },
            free: Ok(1000),
        }],
        library_roots: vec!["/music".into()],
        default_folder_id: Some("f1".into()),
        running_batches: vec![RunningBatch {
            batch_id: "b1".into(),
            label: Some("Come Over".into()),
            unfinished_jobs: 2,
        }],
    }
}

/// alice (u1, a plain user) calling through grant g1 at `HOST`.
pub(crate) fn alice_caller(allow_delete: bool) -> McpCaller {
    McpCaller {
        ctx: UserCtx {
            user_id: "u1".into(),
            username: "alice".into(),
            role: Role::User,
        },
        grant_id: "g1".into(),
        allow_delete,
        allow_admin: false,
        base: BaseUrl::parse("https://music.example.com").unwrap(),
    }
}
