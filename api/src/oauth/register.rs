//! `POST /oauth/register`: dynamic client registration (RFC 7591 section 3)
//! of public clients.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use super::clients::{
    client_metadata, insert_dcr_in, InsertRefusal, MetadataRefusal, NewClient, DCR_CAP,
};
use super::secrets::new_secret;
use super::{has_media_type, now_unix, oauth_error, rate_limited};
use crate::connection::ConnectionState;

/// Room for 10 redirect URIs of 2,000 characters and the other members a
/// client sends; reading stops as soon as a body passes it.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub(crate) async fn register(
    State(state): State<Arc<ConnectionState>>,
    request: Request,
) -> Response {
    if let Err(wait) = state.limits.register.admit("register", Instant::now()) {
        return rate_limited(wait, "Too many app registrations on this server");
    }
    // RFC 7591 3.1 registers with application/json. A cross-site form can
    // post text/plain without a preflight.
    if !has_media_type(request.headers(), "application/json") {
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

fn invalid_metadata(description: &str) -> Response {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_client_metadata",
        description,
    )
}

/// The body, refused as soon as it passes `MAX_BODY_BYTES`, or with the
/// reason it could not be read.
async fn read_body(body: Body) -> Result<Vec<u8>, Response> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| invalid_metadata(&format!("The registration could not be read: {e}")))?;
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(invalid_metadata("The registration is larger than 64 KiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The client the body registers, or the refusal naming the first member
/// the rules refuse. Members the rules do not name are ignored.
async fn read_client(body: Body) -> Result<NewClient, Response> {
    let bytes = read_body(body).await?;
    let Ok(Value::Object(doc)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(invalid_metadata("The registration must be a JSON object"));
    };
    let client = client_metadata(&doc).map_err(|refused| match refused {
        MetadataRefusal::RedirectUris(rule) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", rule)
        }
        MetadataRefusal::ClientName(rule) => invalid_metadata(rule),
    })?;
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
    Ok(client)
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
                json!({"redirect_uris": [CLAUDE_CALLBACK], "token_endpoint_auth_method": "client_secret_basic"}),
                "invalid_client_metadata",
                "Soulful registers public clients only: token_endpoint_auth_method must be none",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "grant_types": ["client_credentials"]}),
                "invalid_client_metadata",
                "Soulful supports the grant types authorization_code and refresh_token only",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "response_types": ["token"]}),
                "invalid_client_metadata",
                "Soulful supports the response type code only",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "client_name": "c".repeat(201)}),
                "invalid_client_metadata",
                "client_name must be at most 200 characters",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "client_name": 7}),
                "invalid_client_metadata",
                "client_name must be a string",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "logo_uri": "x".repeat(64 * 1024)}),
                "invalid_client_metadata",
                "The registration is larger than 64 KiB",
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
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
            ),
            (
                json!({"client_name": "Claude"}),
                "invalid_redirect_uri",
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
            ),
            (
                json!({"redirect_uris": [long]}),
                "invalid_redirect_uri",
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK, 7]}),
                "invalid_redirect_uri",
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
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
            .body(Body::from(
                json!({"redirect_uris": [CLAUDE_CALLBACK]}).to_string(),
            ))
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

    #[tokio::test]
    async fn the_registration_budget_is_shared_by_every_caller() {
        let state = ConnectionState {
            limits: Arc::new(Limits {
                register: WindowLimiter::new(1, Duration::from_secs(3600)),
                ..Limits::default()
            }),
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let app = router(state);
        let body = json!({"redirect_uris": [CLAUDE_CALLBACK]});
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
    async fn the_budget_is_counted_before_the_registration_is_read() {
        let state = ConnectionState {
            limits: Arc::new(Limits {
                register: WindowLimiter::new(1, Duration::from_secs(3600)),
                ..Limits::default()
            }),
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let app = router(state);
        // Refused for its content, it still used the one registration.
        let (first, _, _) =
            post_json(app.clone(), "/oauth/register", json!({"redirect_uris": []})).await;
        let (second, _, doc) =
            post_json(app, "/oauth/register", json!({"redirect_uris": []})).await;
        assert_eq!(
            (first, second, doc["error"].as_str()),
            (
                StatusCode::BAD_REQUEST,
                StatusCode::TOO_MANY_REQUESTS,
                Some("rate_limited")
            )
        );
    }

    #[tokio::test]
    async fn a_body_that_fails_while_it_is_read_says_so() {
        let chunks: Vec<Result<&'static str, std::io::Error>> = vec![
            Ok(r#"{"redirect_uris": "#),
            Err(std::io::Error::other("connection reset")),
        ];
        let req = Request::post("/oauth/register")
            .header("host", HOST)
            .header("content-type", "application/json")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .unwrap();
        let (status, _, body) = send(
            router(ConnectionState::for_test(test_db::migrated().await)),
            req,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(doc["error"], "invalid_client_metadata");
        let description = doc["error_description"].as_str().unwrap();
        assert!(
            description.starts_with("The registration could not be read: ")
                && description.contains("connection reset"),
            "{description}"
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
            "INSERT INTO oauth_grants (id, user_id, client_id, redirect_uri, resource, scope, created_at) \
             SELECT 'g-' || client_id, 'u1', client_id, 'https://claude.ai/api/mcp/auth_callback', \
             'https://music.example.com/mcp', 'soulful', 0 FROM oauth_clients",
        )
        .execute(&db)
        .await
        .unwrap();
        let body = json!({"redirect_uris": [CLAUDE_CALLBACK]});
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
