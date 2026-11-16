//! The MCP endpoint, `/mcp`: an Origin rule against DNS rebinding, then a
//! bearer token bound to the address it was issued for, then rmcp's
//! Streamable HTTP service without sessions.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, ORIGIN, VARY, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::{Extension, Router};

use crate::auth::UserCtx;
use crate::base_url::BaseUrl;
use crate::connection::{refusal, ConnectionState, RequestBase};
use crate::oauth::grants::access_in;
use crate::oauth::metadata::{mcp_url, resource_metadata_url};
use crate::oauth::{now_unix, oauth_error, SCOPE};

mod args;
mod batches;
pub(crate) mod contract;
mod facts;
mod jobs;
mod keys;
pub(crate) mod library;
pub(crate) mod metadata;
mod next;
mod paging;
pub(crate) mod playlists;
mod refs;
pub(crate) mod server;
pub(crate) mod settings;
mod sources;
pub(crate) mod status;
mod wait;

const NO_TOKEN: &str = "No access token was sent";
const UNKNOWN_TOKEN: &str = "The access token is unknown, expired or revoked";
const OTHER_ADDRESS: &str = "The access token was issued for another address of this server";

/// The `/mcp` route behind the gatekeeper. Only `/mcp` itself is the
/// endpoint (MCP 2026-07-28 streamable HTTP, "a single HTTP endpoint path").
/// A route layer, so the router this merges into keeps its own fallback.
pub(crate) fn routes(state: Arc<ConnectionState>) -> Router<Arc<ConnectionState>> {
    Router::new()
        .route_service("/mcp", server::service(state.clone()))
        .route_layer(from_fn_with_state(state, gatekeeper))
}

/// Who a tool call acts for: the token's user as their row stands, its
/// grant and deletion switch, and the address the call came to.
#[derive(Clone, Debug)]
pub(crate) struct McpCaller {
    pub ctx: UserCtx,
    pub grant_id: String,
    pub allow_delete: bool,
    pub base: BaseUrl,
}

/// An `Origin` a page that DNS rebinding points at this server cannot send:
/// an `https` origin (a page on another host needs a certificate for its own
/// name), a loopback one, or `base` itself, scheme, host and port alike.
/// `null` and anything that is not `scheme://host[:port]`, a trailing `/`
/// included (RFC 6454 section 6.1), are refused.
pub(crate) fn origin_allowed(origin: &str, base: &BaseUrl) -> bool {
    let only_authority = origin
        .split_once("://")
        .is_some_and(|(_, authority)| !authority.contains(['/', '?', '#']));
    only_authority
        && BaseUrl::parse(origin)
            .is_ok_and(|origin| origin.is_https() || origin.is_loopback() || origin == *base)
}

/// The `WWW-Authenticate` challenge of a `/mcp` 401 (RFC 6750 section 3,
/// RFC 9728 section 5.1). Every 401 names `invalid_token`, a missing token
/// included (D12).
pub(crate) fn challenge(base: &BaseUrl, description: &str) -> HeaderValue {
    let value = format!(
        r#"Bearer error="invalid_token", error_description="{description}", resource_metadata="{}", scope="{SCOPE}""#,
        resource_metadata_url(base)
    );
    HeaderValue::from_str(&value).expect("a base URL and Soulful's own texts are header-safe")
}

/// Refuses a request from a refused `Origin`, then one without a live token
/// issued for this address; passes the rest to the MCP service with its
/// `McpCaller`. Every answer depends on `Origin`, so it says so in `Vary`.
async fn gatekeeper(
    State(state): State<Arc<ConnectionState>>,
    Extension(RequestBase(base)): Extension<RequestBase>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut response = match admit(&state, base, request.headers()).await {
        Err(refused) => refused,
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
    };
    response
        .headers_mut()
        .append(VARY, HeaderValue::from_static("Origin"));
    response
}

async fn admit(
    state: &ConnectionState,
    base: BaseUrl,
    headers: &HeaderMap,
) -> Result<McpCaller, Response> {
    for origin in headers.get_all(ORIGIN) {
        let origin = String::from_utf8_lossy(origin.as_bytes());
        if !origin_allowed(&origin, &base) {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "origin_not_allowed",
                format!("Requests from {origin} are not accepted"),
            ));
        }
    }
    let Some(token) = bearer_token(headers) else {
        return Err(invalid_token(&base, NO_TOKEN));
    };
    match access_in(&state.db, token, now_unix()).await {
        Err(e) => {
            tracing::error!("Could not read an access token's grant: {e}");
            Err(refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "settings_unreadable",
                "Soulful's database is unavailable",
            ))
        }
        Ok(None) => Err(invalid_token(&base, UNKNOWN_TOKEN)),
        Ok(Some(access)) if access.resource != mcp_url(&base) => {
            Err(invalid_token(&base, OTHER_ADDRESS))
        }
        Ok(Some(access)) => Ok(McpCaller {
            ctx: access.ctx,
            grant_id: access.grant_id,
            allow_delete: access.allow_delete,
            base,
        }),
    }
}

/// The token of an `Authorization: Bearer <token>` header, the scheme in
/// any case (RFC 9110 section 11.1).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// A 401 with `challenge(base, description)` and the body
/// `{"error": "invalid_token", "error_description": description}`.
fn invalid_token(base: &BaseUrl, description: &str) -> Response {
    let mut response = oauth_error(StatusCode::UNAUTHORIZED, "invalid_token", description);
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, challenge(base, description));
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{HeaderMap, Method, Request, StatusCode};
    use serde_json::{from_str, json, Value};

    use super::server::INSTRUCTIONS;
    use super::*;
    use crate::connection::test_util::*;

    fn initialize() -> Value {
        from_str(INITIALIZE).unwrap()
    }

    fn challenge_at_host(description: &str) -> String {
        format!(
            r#"Bearer error="invalid_token", error_description="{description}", resource_metadata="https://music.example.com/.well-known/oauth-protected-resource/mcp", scope="soulful""#
        )
    }

    #[test]
    fn an_origin_is_allowed_when_it_cannot_be_a_rebinding_page() {
        let public = BaseUrl::parse("https://music.example.com").unwrap();
        let local = BaseUrl::parse("http://192.168.1.5:9765").unwrap();
        for o in [
            "https://claude.ai",
            "https://music.example.com",
            "http://localhost:6274",
            "http://127.0.0.1:6274",
        ] {
            assert!(origin_allowed(o, &public), "{o}");
        }
        for o in [
            "http://evil.example",
            "null",
            "not an origin",
            "http://192.168.1.5:9765",
            "https://claude.ai/",
            "https://claude.ai/x",
            "https://claude.ai?x",
            "https://claude.ai#x",
        ] {
            assert!(!origin_allowed(o, &public), "{o}");
        }
        assert!(origin_allowed("http://192.168.1.5:9765", &local));
        // A rebinding page on plain http reaching a LAN install by name
        // derives an https base for the same host and port.
        let rebound = BaseUrl::parse("https://evil.example:9765").unwrap();
        assert!(!origin_allowed("http://evil.example:9765", &rebound));
    }

    #[tokio::test]
    async fn a_call_without_a_token_gets_the_sign_in_challenge() {
        let (_, app, _) = mcp_app().await;
        let (status, headers, body) = post_rpc(app.clone(), &[("host", HOST)], initialize()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers["www-authenticate"],
            challenge_at_host("No access token was sent").as_str()
        );
        assert_eq!(
            body,
            json!({"error": "invalid_token", "error_description": "No access token was sent"})
        );
        assert_uncached_by_origin_and_address(&headers);
        let (status, headers, _) = post_rpc(
            app,
            &[("host", HOST), ("authorization", "Basic dTpw")],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers["www-authenticate"],
            challenge_at_host("No access token was sent").as_str()
        );
    }

    #[tokio::test]
    async fn an_unknown_or_expired_token_is_invalid() {
        let (db, app, token) = mcp_app().await;
        let (status, headers, _) = post_rpc(
            app.clone(),
            &[("host", HOST), ("authorization", "Bearer nope")],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers["www-authenticate"],
            challenge_at_host("The access token is unknown, expired or revoked").as_str()
        );
        sqlx::query("UPDATE oauth_tokens SET expires_at = 0 WHERE kind = 'access'")
            .execute(&db)
            .await
            .unwrap();
        let bearer = format!("Bearer {token}");
        let (status, _, _) = post_rpc(
            app,
            &[("host", HOST), ("authorization", &bearer)],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_token_for_another_address_is_refused() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let mut headers: Vec<(&str, &str)> = TUNNEL.to_vec();
        headers.push(("authorization", &bearer));
        let (status, answer, _) = post_rpc(app, &headers, initialize()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            answer["www-authenticate"],
            r#"Bearer error="invalid_token", error_description="The access token was issued for another address of this server", resource_metadata="https://tunnel.example.net/.well-known/oauth-protected-resource/mcp", scope="soulful""#
        );
    }

    #[tokio::test]
    async fn a_token_ends_with_its_users_credentials() {
        let (db, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let headers = [("host", HOST), ("authorization", bearer.as_str())];
        assert_eq!(
            post_rpc(app.clone(), &headers, initialize()).await.0,
            StatusCode::OK
        );
        sqlx::query(
            "UPDATE users SET credentials_version = credentials_version + 1 WHERE id = 'u1'",
        )
        .execute(&db)
        .await
        .unwrap();
        let (status, answer, _) = post_rpc(app, &headers, initialize()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            answer["www-authenticate"],
            challenge_at_host("The access token is unknown, expired or revoked").as_str()
        );
    }

    /// The headers every `/mcp` answer carries: uncached, and varying on
    /// `Origin` and on the headers its address is read from.
    fn assert_uncached_by_origin_and_address(headers: &HeaderMap) {
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(
            headers.get_all("vary").iter().collect::<Vec<_>>(),
            [
                "Origin",
                "Forwarded, X-Forwarded-Host, X-Forwarded-Proto, Host"
            ]
        );
    }

    #[tokio::test]
    async fn a_rebinding_origin_is_forbidden_even_with_a_token() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let (status, headers, body) = post_rpc(
            app.clone(),
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("origin", "http://evil.example"),
            ],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({"error": "origin_not_allowed", "message": "Requests from http://evil.example are not accepted"})
        );
        assert_uncached_by_origin_and_address(&headers);
        let (status, _, _) = post_rpc(
            app,
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("origin", "https://claude.ai"),
            ],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_database_failure_is_a_503() {
        let (db, app, token) = mcp_app().await;
        db.close().await;
        let bearer = format!("Bearer {token}");
        let (status, _, body) = post_rpc(
            app,
            &[("host", HOST), ("authorization", &bearer)],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({"error": "settings_unreadable", "message": "Soulful's database is unavailable"})
        );
    }

    #[tokio::test]
    async fn initialize_is_answered_without_a_session_for_a_public_host() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("bearer {token}");
        let (status, headers, body) = post_rpc(
            app,
            &[("host", HOST), ("authorization", &bearer)],
            initialize(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("mcp-session-id").is_none());
        let result = &body["result"];
        assert_eq!(
            (
                result["serverInfo"]["name"].as_str(),
                result["protocolVersion"].as_str()
            ),
            (Some("soulful"), Some("2025-11-25"))
        );
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["instructions"], INSTRUCTIONS);
        assert_uncached_by_origin_and_address(&headers);
    }

    #[tokio::test]
    async fn tools_list_needs_no_initialize() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let (status, _, body) = post_rpc(
            app,
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("mcp-protocol-version", "2025-11-25"),
            ],
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"][0]["name"], "get_status");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_a_protocol_error() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let (_, _, body) = post_rpc(
            app,
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("mcp-protocol-version", "2025-11-25"),
            ],
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nope", "arguments": {}}}),
        )
        .await;
        assert_eq!(
            (
                body["error"]["code"].as_i64(),
                body["error"]["message"].as_str()
            ),
            (Some(-32602), Some("Unknown tool: nope"))
        );
    }

    #[tokio::test]
    async fn get_and_delete_are_not_offered() {
        let (_, app, token) = mcp_app().await;
        for method in [Method::GET, Method::DELETE] {
            let req = Request::builder()
                .method(method.clone())
                .uri("/mcp")
                .header("host", HOST)
                .header("authorization", format!("Bearer {token}"))
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .unwrap();
            let (status, headers, _) = send(app.clone(), req).await;
            assert_eq!(
                (status, headers["allow"].to_str().unwrap()),
                (StatusCode::METHOD_NOT_ALLOWED, "POST"),
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn only_the_endpoint_path_is_served() {
        let (_, app, _) = mcp_app().await;
        for path in ["/mcp/x", "/mcp/"] {
            let (status, _, body) = get_json(app.clone(), path, &[("host", HOST)]).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                body,
                json!({"error": "not_found", "message": "Nothing is served here"}),
                "{path}"
            );
        }
    }
}
