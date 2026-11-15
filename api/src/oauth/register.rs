//! `POST /oauth/register`: dynamic client registration (RFC 7591 section 3)
//! of public clients.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use super::clients::{insert_dcr_in, redirect_uri_allowed, InsertRefusal, NewClient, DCR_CAP};
use super::limits::retry_after_seconds;
use super::secrets::new_secret;
use super::{now_unix, oauth_error};
use crate::connection::ConnectionState;

/// Room for 10 redirect URIs of 2,000 characters and the other members a
/// client sends; a larger body is refused unread.
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_REDIRECT_URIS: usize = 10;
const MAX_REDIRECT_URI_CHARS: usize = 2_000;
const MAX_CLIENT_NAME_CHARS: usize = 200;

pub(crate) async fn register(
    State(state): State<Arc<ConnectionState>>,
    request: Request,
) -> Response {
    if let Err(wait) = state.limits.register.admit("register", Instant::now()) {
        let seconds = retry_after_seconds(wait);
        let mut refused = oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("Too many app registrations on this server. Try again in {seconds} seconds."),
        );
        refused.headers_mut().insert(RETRY_AFTER, seconds.into());
        return refused;
    }
    // RFC 7591 3.1 registers with application/json. A cross-site form can
    // post text/plain without a preflight.
    if !is_json(request.headers()) {
        return invalid_metadata("The registration must be sent as application/json");
    }
    let client = match read_client(request.into_body()).await {
        Ok(client) => client,
        Err(refused) => return refused,
    };
    let client_id = format!("dcr_{}", new_secret());
    let issued_at = now_unix();
    match insert_dcr_in(&state.db, &client_id, &client, issued_at, DCR_CAP).await {
        Ok(()) => {}
        Err(InsertRefusal::Full) => {
            return oauth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_clients",
                format!(
                    "Soulful holds {DCR_CAP} registered apps, all connected. Disconnect one in \
                     Settings > Connected apps."
                ),
            )
        }
        Err(InsertRefusal::Database(e)) => {
            tracing::error!("Could not register an OAuth client: {e}");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Soulful's database is unavailable",
            );
        }
    }
    let mut doc = Map::new();
    doc.insert("client_id".into(), client_id.into());
    doc.insert("client_id_issued_at".into(), issued_at.into());
    if let Some(name) = client.client_name {
        doc.insert("client_name".into(), name.into());
    }
    doc.insert("redirect_uris".into(), client.redirect_uris.into());
    doc.insert("token_endpoint_auth_method".into(), "none".into());
    doc.insert(
        "grant_types".into(),
        json!(["authorization_code", "refresh_token"]),
    );
    doc.insert("response_types".into(), json!(["code"]));
    (StatusCode::CREATED, Json(Value::Object(doc))).into_response()
}

/// The media type's essence is `application/json`, parameters aside.
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

fn invalid_metadata(description: &str) -> Response {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_client_metadata",
        description,
    )
}

/// The client the body registers, or the refusal naming the first member
/// the rules refuse. Members the rules do not name are ignored.
async fn read_client(body: Body) -> Result<NewClient, Response> {
    let not_an_object = || invalid_metadata("The registration must be a JSON object");
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| not_an_object())?;
    let Ok(Value::Object(doc)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(not_an_object());
    };
    let redirect_uris = redirect_uris(doc.get("redirect_uris"))?;
    if doc
        .get("token_endpoint_auth_method")
        .is_some_and(|m| m != "none")
    {
        return Err(invalid_metadata(
            "Soulful registers public clients only: token_endpoint_auth_method must be none",
        ));
    }
    let grant_type_allowed = |g: &Value| g == "authorization_code" || g == "refresh_token";
    if doc.get("grant_types").is_some_and(|g| {
        !g.as_array()
            .is_some_and(|g| g.iter().all(grant_type_allowed))
    }) {
        return Err(invalid_metadata(
            "Soulful supports the grant types authorization_code and refresh_token only",
        ));
    }
    if doc
        .get("response_types")
        .is_some_and(|r| *r != json!(["code"]))
    {
        return Err(invalid_metadata(
            "Soulful supports the response type code only",
        ));
    }
    let client_name = match doc.get("client_name") {
        None => None,
        Some(Value::String(name)) if name.chars().count() <= MAX_CLIENT_NAME_CHARS => {
            Some(name.clone())
        }
        Some(Value::String(_)) => {
            return Err(invalid_metadata(
                "client_name must be at most 200 characters",
            ))
        }
        Some(_) => return Err(invalid_metadata("client_name must be a string")),
    };
    Ok(NewClient {
        client_name,
        redirect_uris,
    })
}

/// 1 to 10 strings of at most 2,000 characters, each one
/// `redirect_uri_allowed` accepts.
fn redirect_uris(member: Option<&Value>) -> Result<Vec<String>, Response> {
    let refused = |description: &str| {
        oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", description)
    };
    let uris: Option<Vec<&str>> = member
        .and_then(Value::as_array)
        .filter(|uris| (1..=MAX_REDIRECT_URIS).contains(&uris.len()))
        .and_then(|uris| uris.iter().map(Value::as_str).collect());
    let Some(uris) = uris.filter(|uris| {
        uris.iter()
            .all(|uri| uri.chars().count() <= MAX_REDIRECT_URI_CHARS)
    }) else {
        return Err(refused(
            "Register 1 to 10 redirect URIs of at most 2,000 characters",
        ));
    };
    for uri in &uris {
        redirect_uri_allowed(uri).map_err(refused)?;
    }
    Ok(uris.into_iter().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::json;

    use crate::connection::test_util::*;
    use crate::connection::{router, ConnectionState};
    use crate::db::test_db;
    use crate::oauth::clients::DCR_CAP;
    use crate::oauth::limits::{Limits, WindowLimiter};
    use crate::oauth::now_unix;

    #[tokio::test]
    async fn a_public_client_registers_with_its_redirect_uri() {
        let db = test_db::migrated().await;
        // Body as rmcp 3.10.0 sends it (src/transport/auth.rs register_client), with Claude's callback.
        let body = json!({"client_name": "Claude", "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            "grant_types": ["authorization_code", "refresh_token"], "token_endpoint_auth_method": "none",
            "response_types": ["code"], "application_type": "web"});
        let before = now_unix();
        let (status, headers, doc) = post_json(
            router(ConnectionState::for_test(db.clone())),
            "/oauth/register",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers["cache-control"], "no-store");
        let id = doc["client_id"].as_str().unwrap();
        assert!(id.starts_with("dcr_") && id.len() == 47, "{id}");
        assert!((before..=before + 1).contains(&doc["client_id_issued_at"].as_i64().unwrap()));
        assert_eq!(doc["client_name"], "Claude");
        assert_eq!(
            doc["redirect_uris"],
            json!(["https://claude.ai/api/mcp/auth_callback"])
        );
        assert_eq!(doc["token_endpoint_auth_method"], "none");
        assert_eq!(
            doc["grant_types"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(doc["response_types"], json!(["code"]));
        let row: (String, Option<String>, String) = sqlx::query_as(
            "SELECT kind, client_name, redirect_uris FROM oauth_clients WHERE client_id = ?",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "dcr".into(),
                Some("Claude".into()),
                r#"["https://claude.ai/api/mcp/auth_callback"]"#.into()
            )
        );
    }

    #[tokio::test]
    async fn a_confidential_or_misdirected_client_is_refused() {
        let db = test_db::migrated().await;
        let long = format!("https://claude.ai/{}", "a".repeat(2000));
        for (body, error, description) in [
            (
                json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "client_secret_basic"}),
                "invalid_client_metadata",
                "Soulful registers public clients only: token_endpoint_auth_method must be none",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "grant_types": ["client_credentials"]}),
                "invalid_client_metadata",
                "Soulful supports the grant types authorization_code and refresh_token only",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "response_types": ["token"]}),
                "invalid_client_metadata",
                "Soulful supports the response type code only",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "client_name": "c".repeat(201)}),
                "invalid_client_metadata",
                "client_name must be at most 200 characters",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "client_name": 7}),
                "invalid_client_metadata",
                "client_name must be a string",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "logo_uri": "x".repeat(64 * 1024)}),
                "invalid_client_metadata",
                "The registration must be a JSON object",
            ),
            (
                json!(["not", "an", "object"]),
                "invalid_client_metadata",
                "The registration must be a JSON object",
            ),
            (
                json!({"redirect_uris": ["http://claude.ai/api/mcp/auth_callback"]}),
                "invalid_redirect_uri",
                "redirect URIs must be https, or http on a loopback address, with no fragment",
            ),
            (
                json!({"redirect_uris": []}),
                "invalid_redirect_uri",
                "Register 1 to 10 redirect URIs of at most 2,000 characters",
            ),
            (
                json!({"client_name": "Claude"}),
                "invalid_redirect_uri",
                "Register 1 to 10 redirect URIs of at most 2,000 characters",
            ),
            (
                json!({"redirect_uris": [long]}),
                "invalid_redirect_uri",
                "Register 1 to 10 redirect URIs of at most 2,000 characters",
            ),
            (
                json!({"redirect_uris": [CALLBACK, 7]}),
                "invalid_redirect_uri",
                "Register 1 to 10 redirect URIs of at most 2,000 characters",
            ),
        ] {
            let (status, _, doc) = post_json(
                router(ConnectionState::for_test(db.clone())),
                "/oauth/register",
                body.clone(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(
                doc,
                json!({"error": error, "error_description": description}),
                "{body}"
            );
        }
        // A cross-site form can post text/plain without a preflight.
        let req = Request::post("/oauth/register")
            .header("host", HOST)
            .header("content-type", "text/plain")
            .body(Body::from(json!({"redirect_uris": [CALLBACK]}).to_string()))
            .unwrap();
        let (status, _, body) = send(router(ConnectionState::for_test(db.clone())), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": "invalid_client_metadata",
                "error_description": "The registration must be sent as application/json"})
        );
        assert_eq!(count(&db, "oauth_clients").await, 0);
    }

    // Limits gains fields in later tasks; a test sets the one limit it needs
    // by assignment, so it does not change when they come.
    #[allow(clippy::field_reassign_with_default)]
    #[tokio::test]
    async fn the_registration_budget_is_shared_by_every_caller() {
        let mut limits = Limits::default();
        limits.register = WindowLimiter::new(1, Duration::from_secs(3600));
        let state = ConnectionState {
            limits: Arc::new(limits),
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let app = router(state);
        let body = json!({"redirect_uris": [CALLBACK]});
        let (first, _, _) = post_json(app.clone(), "/oauth/register", body.clone()).await;
        let (second, headers, doc) = post_json(app, "/oauth/register", body).await;
        assert_eq!(
            (first, second),
            (StatusCode::CREATED, StatusCode::TOO_MANY_REQUESTS)
        );
        let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((3599..=3600).contains(&wait), "{wait}");
        assert_eq!(
            doc,
            json!({"error": "rate_limited",
            "error_description": format!("Too many app registrations on this server. Try again in {wait} seconds.")})
        );
    }

    #[tokio::test]
    async fn a_full_server_or_an_unavailable_database_refuses_the_registration() {
        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
             INSERT INTO oauth_clients (client_id, kind, redirect_uris, created_at) \
             SELECT 'dcr_' || i, 'dcr', '[]', 0 FROM n",
        )
        .bind(DCR_CAP)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_grants (id, user_id, client_id, resource, scope, created_at) \
             SELECT 'g-' || client_id, 'u1', client_id, 'https://music.example.com/mcp', 'soulful', 0 \
             FROM oauth_clients",
        )
        .execute(&db)
        .await
        .unwrap();
        let body = json!({"redirect_uris": [CALLBACK]});
        let (status, _, doc) = post_json(
            router(ConnectionState::for_test(db.clone())),
            "/oauth/register",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            doc,
            json!({"error": "too_many_clients", "error_description":
                "Soulful holds 100 registered apps, all connected. Disconnect one in Settings > Connected apps."})
        );
        assert_eq!(count(&db, "oauth_clients").await, DCR_CAP);

        db.close().await;
        let (status, _, doc) = post_json(
            router(ConnectionState::for_test(db)),
            "/oauth/register",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            doc,
            json!({"error": "server_error", "error_description": "Soulful's database is unavailable"})
        );
    }
}
