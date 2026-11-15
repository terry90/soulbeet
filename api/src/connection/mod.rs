//! The paths Claude (MCP) connects through, `/.well-known/...`,
//! `/oauth/...` and `/mcp`, behind one guard that refuses them while MCP is
//! switched off, the instance is unsafe to expose, or the request's address
//! is not https.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{self, CACHE_CONTROL, VARY};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::json;

use crate::base_url::{explain_request, BaseExplanation, BaseUrl, BaseUrlError};
use crate::config::CONFIG;
use crate::connections::LoggedRequest;
use crate::mcp::status::{LiveStatus, StatusSource};
use crate::oauth::cimd::{CimdFetcher, ClientDocuments, UnbuiltFetcher};
use crate::oauth::limits::Limits;
use crate::security::GateReason;
use log::{base_and_problem, header_value, RequestLog, LIVE_LOG};

#[cfg(test)]
mod flow_tests;
pub(crate) mod log;
#[cfg(test)]
pub(crate) mod test_util;

/// What the connection router reads about the instance, a seam for tests.
pub struct ConnectionState {
    pub(crate) mcp_enabled: bool,
    pub(crate) public_url: Option<Result<BaseUrl, BaseUrlError>>,
    pub(crate) gate: fn() -> Vec<GateReason>,
    pub(crate) db: sqlx::SqlitePool,
    pub(crate) limits: Arc<Limits>,
    /// The key session cookies are signed with.
    pub(crate) key: &'static crate::secret_key::SecretKey,
    /// Where apps' client metadata documents are fetched from.
    pub(crate) documents: Arc<dyn ClientDocuments>,
    /// Where `get_status` reads its facts.
    pub(crate) status: Arc<dyn StatusSource>,
    /// Where the guard records each request.
    pub(crate) log: Arc<RequestLog>,
}

impl ConnectionState {
    /// The running instance's configuration and security checks.
    pub fn live() -> ConnectionState {
        ConnectionState {
            mcp_enabled: CONFIG.mcp_enabled(),
            public_url: CONFIG.public_url().cloned(),
            gate: crate::security::gate_reasons,
            db: (*crate::db::DB).clone(),
            limits: Arc::new(Limits::default()),
            key: crate::secret_key::current(),
            documents: match CimdFetcher::new() {
                Ok(fetcher) => Arc::new(fetcher),
                Err(e) => {
                    tracing::error!("Could not build the client metadata fetcher: {e}");
                    Arc::new(UnbuiltFetcher(e))
                }
            },
            status: Arc::new(LiveStatus),
            log: LIVE_LOG.clone(),
        }
    }
}

/// The address of the request, put into the request extensions by the guard.
#[derive(Clone, Debug)]
pub(crate) struct RequestBase(pub BaseUrl);

/// The connection routes, each answer passed through the guard. The
/// catch-alls are routes, not a fallback: the Dioxus router this merges into
/// has its own fallback, and axum refuses to merge two.
pub fn router(state: ConnectionState) -> Router {
    let state = Arc::new(state);
    Router::new()
        .merge(crate::oauth::routes())
        .merge(crate::mcp::routes(state.clone()))
        .route("/.well-known/", any(not_found))
        .route("/.well-known/{*rest}", any(not_found))
        .route("/oauth/", any(not_found))
        .route("/oauth/{*rest}", any(not_found))
        .route("/mcp/", any(not_found))
        .route("/mcp/{*rest}", any(not_found))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

async fn not_found() -> Response {
    refusal(StatusCode::NOT_FOUND, "not_found", "Nothing is served here")
}

/// Refuses the request or runs the route with its `RequestBase`, then marks
/// the answer uncached and dependent on the headers its address was read
/// from, next to any `Vary` the route set, and records the request and its
/// answer in the log.
async fn guard(
    State(state): State<Arc<ConnectionState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let explained = explain_request(request.headers(), request.uri(), state.public_url.as_ref());
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let [host_header, x_forwarded_host, x_forwarded_proto, forwarded] =
        ["host", "x-forwarded-host", "x-forwarded-proto", "forwarded"]
            .map(|name| header_value(request.headers(), name));
    let mut response = match admit(&state, &explained) {
        Err(refused) => refused,
        Ok(base) => {
            request.extensions_mut().insert(RequestBase(base));
            next.run(request).await
        }
    };
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.append(
        VARY,
        HeaderValue::from_static("Forwarded, X-Forwarded-Host, X-Forwarded-Proto, Host"),
    );
    let (base, base_problem) = base_and_problem(&explained.result);
    state.log.record(LoggedRequest {
        at: crate::oauth::now_unix(),
        method,
        path,
        base,
        base_problem,
        host_from: explained.host_from.as_str().into(),
        scheme_from: explained.scheme_from.as_str().into(),
        host_header,
        x_forwarded_host,
        x_forwarded_proto,
        forwarded,
        status: response.status().as_u16(),
        refusal: response
            .extensions()
            .get::<RefusalCode>()
            .map(|RefusalCode(code)| code.to_string()),
    });
    response
}

/// The request's address, or the refusal of the request.
fn admit(state: &ConnectionState, explained: &BaseExplanation) -> Result<BaseUrl, Response> {
    if !state.mcp_enabled {
        return Err(refusal(
            StatusCode::NOT_FOUND,
            "mcp_disabled",
            "Claude (MCP) access is turned off: MCP=off is set",
        ));
    }
    let reasons = (state.gate)();
    if !reasons.is_empty() {
        return Err(refusal_with(
            StatusCode::SERVICE_UNAVAILABLE,
            "mcp_unavailable",
            "Claude (MCP) access stays off until the security checks in Settings pass",
            json!({ "reasons": reasons }),
        ));
    }
    let base = match &explained.result {
        Ok(base) => base.clone(),
        Err(e) => {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                "address_unreadable",
                format!("Cannot tell this request's address: {e}"),
            ))
        }
    };
    if !base.is_https() && !base.is_loopback() {
        return Err(refusal_with(
            StatusCode::FORBIDDEN,
            "https_required",
            format!(
                "Soulful derived the address {base} for this request. Claude connects only \
                 over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: \
                 proto=https), or set PUBLIC_URL."
            ),
            json!({
                "derived_base": base.to_string(),
                "host_from": explained.host_from.as_str(),
                "scheme_from": explained.scheme_from.as_str(),
            }),
        ));
    }
    Ok(base)
}

/// Keeps a page from being framed by another site: sets
/// `Content-Security-Policy: frame-ancestors 'none'` on a response that has
/// no policy of its own.
pub async fn deny_framing(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static("frame-ancestors 'none'"));
    response
}

/// The `error` code of a connection layer refusal, in its response's
/// extensions for the guard's log.
#[derive(Clone, Copy)]
struct RefusalCode(&'static str);

/// The connection layer's refusal: `{"error": code, "message": message}`.
pub(crate) fn refusal(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> Response {
    refusal_with(status, code, message, json!({}))
}

/// `refusal` with the members of `extra`, a JSON object, added to the body.
pub(crate) fn refusal_with(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    extra: serde_json::Value,
) -> Response {
    let serde_json::Value::Object(extra) = extra else {
        unreachable!("a refusal's extra members are a JSON object");
    };
    let mut body = serde_json::Map::new();
    body.insert("error".into(), code.into());
    body.insert("message".into(), message.into().into());
    body.extend(extra);
    let mut response = (status, Json(serde_json::Value::Object(body))).into_response();
    response.extensions_mut().insert(RefusalCode(code));
    response
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn the_endpoints_answer_404_while_mcp_is_off() {
        let state = ConnectionState {
            mcp_enabled: false,
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let (status, _, body) = get_json(
            router(state),
            "/.well-known/oauth-protected-resource/mcp",
            &[("host", HOST)],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({"error": "mcp_disabled", "message": "Claude (MCP) access is turned off: MCP=off is set"})
        );
    }

    #[tokio::test]
    async fn the_endpoints_answer_503_naming_each_gate_reason() {
        fn both() -> Vec<GateReason> {
            crate::security::gate_reasons_from(true, true)
        }
        let state = ConnectionState {
            gate: both,
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let (status, _, body) = get_json(router(state), "/oauth/token", &[("host", HOST)]).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("mcp_unavailable"))
        );
        assert_eq!(
            body["message"],
            "Claude (MCP) access stays off until the security checks in Settings pass"
        );
        assert_eq!(body["reasons"], serde_json::to_value(both()).unwrap());
        assert_eq!(
            (
                body["reasons"][0]["code"].as_str(),
                body["reasons"][1]["code"].as_str()
            ),
            (Some("weak_secret_key"), Some("default_admin_password"))
        );
    }

    #[tokio::test]
    async fn a_plain_http_address_off_loopback_is_refused() {
        let (status, _, body) = get_json(
            router(ConnectionState::for_test(test_db::migrated().await)),
            "/.well-known/oauth-protected-resource/mcp",
            &[("host", "192.168.1.5:9765")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "error": "https_required",
                "message": "Soulful derived the address http://192.168.1.5:9765 for this request. Claude connects only over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: proto=https), or set PUBLIC_URL.",
                "derived_base": "http://192.168.1.5:9765",
                "host_from": "host",
                "scheme_from": "local_host_is_http"
            })
        );
    }

    #[tokio::test]
    async fn an_address_that_cannot_be_read_is_refused() {
        let (status, _, body) = get_json(
            router(ConnectionState::for_test(test_db::migrated().await)),
            "/oauth/token",
            &[("host", HOST), ("x-forwarded-proto", "gopher")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // BaseUrlError::InvalidScheme's text in base_url.rs
        assert_eq!(
            body,
            json!({"error": "address_unreadable",
                "message": "Cannot tell this request's address: 'gopher' is not http or https"})
        );
    }

    #[tokio::test]
    async fn loopback_http_is_served() {
        // localhost:9765 has no route yet: the guard let it through to the 404.
        let (status, _, body) = get_json(
            router(ConnectionState::for_test(test_db::migrated().await)),
            "/oauth/nothing",
            &[("host", "localhost:9765")],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({"error": "not_found", "message": "Nothing is served here"})
        );
    }

    #[tokio::test]
    async fn the_bare_prefixes_are_not_found() {
        for path in ["/.well-known/", "/oauth/"] {
            let (status, _, body) = get_json(
                router(ConnectionState::for_test(test_db::migrated().await)),
                path,
                &[("host", HOST)],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                body,
                json!({"error": "not_found", "message": "Nothing is served here"}),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn framing_is_denied_unless_the_page_sets_its_own() {
        let plain = deny_framing(StatusCode::OK.into_response()).await;
        assert_eq!(
            plain.headers()["content-security-policy"],
            "frame-ancestors 'none'"
        );
        let own = deny_framing(
            (
                [(header::CONTENT_SECURITY_POLICY, "default-src 'none'")],
                "",
            )
                .into_response(),
        )
        .await;
        assert_eq!(
            own.headers()["content-security-policy"],
            "default-src 'none'"
        );
    }

    #[tokio::test]
    async fn the_log_keeps_where_each_address_came_from() {
        let state = ConnectionState::for_test(test_db::migrated().await);
        let log = state.log.clone();
        let long = "a".repeat(300);
        get_json(
            router(state),
            "/oauth/nothing?code=secret",
            &[("host", HOST), ("x-forwarded-proto", &long)],
        )
        .await;
        let entry = &log.recent()[0];
        assert_eq!(
            (
                entry.path.as_str(),
                entry.status,
                entry.refusal.as_deref(),
                entry.base.as_deref()
            ),
            ("/oauth/nothing", 400, Some("address_unreadable"), None)
        );
        assert!(entry
            .base_problem
            .as_deref()
            .unwrap()
            .ends_with("is not http or https"));
        assert_eq!(
            (entry.host_from.as_str(), entry.scheme_from.as_str()),
            ("host", "x_forwarded_proto")
        );
        assert_eq!(entry.x_forwarded_proto.as_ref().map(String::len), Some(200));
        let state = ConnectionState::for_test(test_db::migrated().await);
        let log = state.log.clone();
        get_json(
            router(state),
            "/oauth/nothing?code=secret",
            &[("host", "192.168.1.5:9765")],
        )
        .await;
        let entry = &log.recent()[0];
        assert_eq!(
            (
                entry.path.as_str(),
                entry.base.as_deref(),
                entry.status,
                entry.refusal.as_deref()
            ),
            (
                "/oauth/nothing",
                Some("http://192.168.1.5:9765"),
                403,
                Some("https_required")
            )
        );
        assert_eq!(
            (entry.host_from.as_str(), entry.scheme_from.as_str()),
            ("host", "local_host_is_http")
        );
    }

    #[tokio::test]
    async fn the_log_names_a_routes_refusal_and_no_refusal_for_an_answer() {
        let state = ConnectionState::for_test(test_db::migrated().await);
        let log = state.log.clone();
        let app = router(state);
        get_json(app.clone(), "/oauth/nothing", &[("host", HOST)]).await;
        get_json(
            app,
            "/.well-known/oauth-protected-resource/mcp",
            &[("host", HOST)],
        )
        .await;
        let recent = log.recent();
        let seen = |i: usize| {
            (
                recent[i].path.as_str(),
                recent[i].status,
                recent[i].refusal.as_deref(),
                recent[i].base.as_deref(),
                recent[i].host_header.as_deref(),
            )
        };
        assert_eq!(
            (seen(0), seen(1)),
            (
                (
                    "/.well-known/oauth-protected-resource/mcp",
                    200,
                    None,
                    Some("https://music.example.com"),
                    Some(HOST)
                ),
                (
                    "/oauth/nothing",
                    404,
                    Some("not_found"),
                    Some("https://music.example.com"),
                    Some(HOST)
                )
            )
        );
    }

    #[tokio::test]
    async fn every_answer_is_uncached_and_varies_on_the_address_headers() {
        for (path, host) in [
            ("/.well-known/oauth-nothing", HOST),
            ("/oauth/nothing", "192.168.1.5"),
        ] {
            let (_, headers, _) = get_json(
                router(ConnectionState::for_test(test_db::migrated().await)),
                path,
                &[("host", host)],
            )
            .await;
            assert_eq!(headers["cache-control"], "no-store", "{path}");
            assert_eq!(
                headers["vary"], "Forwarded, X-Forwarded-Host, X-Forwarded-Proto, Host",
                "{path}"
            );
        }
    }
}
