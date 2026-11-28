//! The paths MCP clients connect through, `/.well-known/...`,
//! `/oauth/...` and `/mcp`, behind one guard that refuses them while MCP is
//! switched off, the instance is unsafe to expose, or the request's address
//! is not https.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{self, CACHE_CONTROL, VARY};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::Map;

use crate::base_url::{explain_request, BaseExplanation, BaseUrl, BaseUrlError};
use crate::config::CONFIG;
use crate::connections::LoggedRequest;
use crate::core::delete_guard::{Listeners, LiveListeners};
use crate::core::discovery::DiscoveryEffects;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::jobs::runner::{Acquire, ACQUIRE};
use crate::mcp::status::{LiveStatus, StatusSource};
use crate::mcp::sync::{LiveSync, RatingSync};
use crate::oauth::cimd::{CimdFetcher, ClientDocuments, UnbuiltFetcher};
use crate::oauth::limits::Limits;
use crate::security::GateReason;
use crate::server_fns::discovery::LiveDiscovery;
use log::{base_and_problem, cut, header_value, Recording, RequestLog, LIVE_LOG};

#[cfg(test)]
mod flow_tests;
pub(crate) mod log;
#[cfg(test)]
pub(crate) mod test_util;

/// What the connection router reads about the instance, a seam for tests.
pub struct ConnectionState {
    /// Why `MCP` switches the endpoints off; None when it is on.
    pub(crate) mcp_off: Option<String>,
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
    /// Where the tools get the caller's Navidrome client.
    pub(crate) navidrome: Arc<dyn NavidromeOf>,
    /// NAVIDROME_MUSIC_PATH, the default Navidrome library's root on this
    /// machine, when it is set.
    pub(crate) navidrome_music_path: Option<PathBuf>,
    /// Where the guard records each request.
    pub(crate) log: Arc<RequestLog>,
    /// What the acquisition tools act through.
    pub(crate) acquire: Arc<Acquire>,
    /// What the discovery tools start runs and import through.
    pub(crate) discovery: Arc<dyn DiscoveryEffects>,
    /// Where `sync_ratings` runs the caller's rating sync.
    pub(crate) ratings: Arc<dyn RatingSync>,
    /// Navidrome as each user sees it, which the delete tools' guard checks through.
    pub(crate) listeners: Arc<dyn Listeners>,
    /// Coordinator for 1-Click Existing Library Import batches.
    pub(crate) library_import: Arc<crate::core::library_import::LibraryImportCoordinator>,
}

impl ConnectionState {
    /// The running instance's configuration and security checks.
    pub fn live() -> ConnectionState {
        let db = (*crate::db::DB).clone();
        ConnectionState {
            mcp_off: CONFIG.mcp_off().map(str::to_string),
            public_url: CONFIG.public_url().cloned(),
            gate: crate::security::gate_reasons,
            db: db.clone(),
            limits: Arc::new(Limits::default()),
            key: crate::secret_key::current(),
            documents: match CimdFetcher::new() {
                Ok(fetcher) => Arc::new(fetcher),
                Err(e) => {
                    tracing::error!("Could not build the client metadata fetcher: {e}");
                    Arc::new(UnbuiltFetcher(e))
                }
            },
            status: Arc::new(LiveStatus { db }),
            navidrome: Arc::new(ServerNavidrome),
            navidrome_music_path: CONFIG.navidrome_music_path().map(Path::to_path_buf),
            log: LIVE_LOG.clone(),
            acquire: ACQUIRE.clone(),
            discovery: Arc::new(LiveDiscovery),
            ratings: Arc::new(LiveSync),
            listeners: Arc::new(LiveListeners),
            library_import: crate::core::library_import::COORDINATOR.clone(),
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
/// from, next to any `Vary` the route set. The request is recorded in the
/// log with its answer, or without one when the client leaves first and
/// the route's future is dropped.
async fn guard(
    State(state): State<Arc<ConnectionState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let explained = explain_request(request.headers(), request.uri(), state.public_url.as_ref());
    let [host_header, x_forwarded_host, x_forwarded_proto, forwarded] =
        ["host", "x-forwarded-host", "x-forwarded-proto", "forwarded"]
            .map(|name| header_value(request.headers(), name));
    let (base, base_problem) = base_and_problem(&explained.result);
    let recording = Recording::start(
        state.log.clone(),
        LoggedRequest {
            at: crate::oauth::now_unix(),
            method: request.method().to_string(),
            path: cut(request.uri().path()),
            base,
            base_problem,
            host_from: explained.host_from.as_str().into(),
            scheme_from: explained.scheme_from.as_str().into(),
            host_header,
            x_forwarded_host,
            x_forwarded_proto,
            forwarded,
            status: None,
            refusal: None,
        },
    );
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
    recording.answered(
        response.status().as_u16(),
        response
            .extensions()
            .get::<RefusalCode>()
            .map(|RefusalCode(code)| code.to_string()),
    );
    response
}

/// The request's address, or the refusal of the request.
fn admit(state: &ConnectionState, explained: &BaseExplanation) -> Result<BaseUrl, Response> {
    if let Some(why) = &state.mcp_off {
        return Err(refusal(
            StatusCode::NOT_FOUND,
            "mcp_disabled",
            format!("MCP access is turned off: {why}"),
        ));
    }
    // Which check fails is shown only to the admin, in Settings: an anonymous
    // caller learns nothing about the instance's weaknesses.
    if !(state.gate)().is_empty() {
        return Err(refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "mcp_unavailable",
            GATE_CLOSED,
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
        let mut extra = Map::new();
        extra.insert("derived_base".into(), base.to_string().into());
        extra.insert("host_from".into(), explained.host_from.as_str().into());
        extra.insert("scheme_from".into(), explained.scheme_from.as_str().into());
        return Err(refusal_with(
            StatusCode::FORBIDDEN,
            "https_required",
            format!(
                "Soulbeet derived the address {base} for this request. MCP clients connect only \
                 over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: \
                 proto=https), or set PUBLIC_URL."
            ),
            extra,
        ));
    }
    Ok(base)
}

/// What the endpoints answer while a security check fails.
pub(crate) const GATE_CLOSED: &str =
    "MCP access stays off until the security checks in Settings pass";

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
    refusal_with(status, code, message, Map::new())
}

/// `refusal` with the members of `extra` added to the body.
pub(crate) fn refusal_with(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    extra: Map<String, serde_json::Value>,
) -> Response {
    let mut body = Map::new();
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
    use serde_json::json;

    #[tokio::test]
    async fn the_endpoints_answer_404_while_mcp_is_off() {
        for (switch, message) in [
            ("off", "MCP access is turned off: MCP=off is set"),
            (
                "disabled",
                "MCP access is turned off: MCP is set to 'disabled', which is neither on nor off",
            ),
        ] {
            let state = ConnectionState {
                mcp_off: crate::config::parse_mcp_switch(Some(switch)),
                ..ConnectionState::for_test(test_db::migrated().await)
            };
            let (status, _, body) = get_json(
                router(state),
                "/.well-known/oauth-protected-resource/mcp",
                &[("host", HOST)],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{switch}");
            assert_eq!(
                body,
                json!({"error": "mcp_disabled", "message": message}),
                "{switch}"
            );
        }
    }

    #[tokio::test]
    async fn the_guard_names_the_first_check_that_fails() {
        fn failing() -> Vec<GateReason> {
            crate::security::gate_reasons_from(true, false)
        }
        // Each request fails every check from its row on: the switch, the
        // gate, the address, then plain http.
        let unreadable: &[(&str, &str)] = &[("host", HOST), ("x-forwarded-proto", "gopher")];
        let plain_lan: &[(&str, &str)] = &[("host", "192.168.1.5:9765")];
        for (mcp_off, gate, headers, want) in [
            (true, true, unreadable, "mcp_disabled"),
            (true, false, plain_lan, "mcp_disabled"),
            (false, true, unreadable, "mcp_unavailable"),
            (false, true, plain_lan, "mcp_unavailable"),
            (false, false, unreadable, "address_unreadable"),
            (false, false, plain_lan, "https_required"),
        ] {
            let state = ConnectionState {
                mcp_off: mcp_off.then(|| "MCP=off is set".to_string()),
                gate: if gate { failing } else { Vec::new },
                ..ConnectionState::for_test(test_db::migrated().await)
            };
            let (_, _, body) = get_json(router(state), "/oauth/token", headers).await;
            assert_eq!(
                body["error"].as_str(),
                Some(want),
                "{mcp_off} {gate} {headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_endpoints_answer_503_without_naming_the_failing_checks() {
        fn both() -> Vec<GateReason> {
            crate::security::gate_reasons_from(true, true)
        }
        let state = ConnectionState {
            gate: both,
            ..ConnectionState::for_test(test_db::migrated().await)
        };
        let (status, _, body) = get_json(router(state), "/oauth/token", &[("host", HOST)]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "error": "mcp_unavailable",
                "message": "MCP access stays off until the security checks in Settings pass"
            })
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
                "message": "Soulbeet derived the address http://192.168.1.5:9765 for this request. MCP clients connect only over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: proto=https), or set PUBLIC_URL.",
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
            (
                "/oauth/nothing",
                Some(400),
                Some("address_unreadable"),
                None
            )
        );
        // "'aaa...' is not http or https", cut to 200 characters.
        assert_eq!(
            entry.base_problem.as_deref(),
            Some(format!("'{}", "a".repeat(199)).as_str())
        );
        assert_eq!(
            (entry.host_from.as_str(), entry.scheme_from.as_str()),
            ("host", "x_forwarded_proto")
        );
        assert_eq!(entry.x_forwarded_proto.as_ref().map(String::len), Some(200));
        assert_eq!(entry.status, Some(400));
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
                Some(403),
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
                    Some(200),
                    None,
                    Some("https://music.example.com"),
                    Some(HOST)
                ),
                (
                    "/oauth/nothing",
                    Some(404),
                    Some("not_found"),
                    Some("https://music.example.com"),
                    Some(HOST)
                )
            )
        );
    }

    #[tokio::test]
    async fn a_long_path_and_address_problem_are_cut_in_the_log() {
        let state = ConnectionState::for_test(test_db::migrated().await);
        let log = state.log.clone();
        let path = format!("/oauth/{}", "p".repeat(300));
        let proto = "x".repeat(300);
        get_json(
            router(state),
            &path,
            &[("host", HOST), ("x-forwarded-proto", &proto)],
        )
        .await;
        let entry = &log.recent()[0];
        assert_eq!(entry.path, path[..200]);
        assert_eq!(
            entry.base_problem.as_deref().map(|p| p.chars().count()),
            Some(200)
        );
    }

    /// Fetches no document: the client leaves while it waits.
    struct NeverAnswers;

    #[async_trait::async_trait]
    impl crate::oauth::cimd::ClientDocuments for NeverAnswers {
        async fn fetch(
            &self,
            _url: &url::Url,
        ) -> Result<crate::oauth::cimd::ClientDocument, crate::oauth::cimd::CimdError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_request_the_client_leaves_is_logged_without_a_status() {
        use tower::ServiceExt;
        let db = test_db::migrated().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let state = ConnectionState {
            documents: Arc::new(NeverAnswers),
            ..ConnectionState::for_test(db)
        };
        let log = state.log.clone();
        let app = router(state).layer(tower_cookies::CookieManagerLayer::new());
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", CLAUDE_CODE_ID)
            .finish();
        let req = axum::http::Request::get(format!("/oauth/authorize?{query}"))
            .header("host", HOST)
            .header("cookie", cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let left =
            tokio::time::timeout(std::time::Duration::from_millis(100), app.oneshot(req)).await;
        assert!(left.is_err(), "the fetch never answers");
        let recent = log.recent();
        assert_eq!(
            recent
                .iter()
                .map(|e| (e.path.as_str(), e.status, e.base.as_deref()))
                .collect::<Vec<_>>(),
            [("/oauth/authorize", None, Some("https://music.example.com"))]
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
