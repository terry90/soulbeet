//! `POST /oauth/token` (RFC 6749 sections 4.1.3 and 6) and
//! `POST /oauth/revoke` (RFC 7009).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Map, Value};

use super::grants::{
    code_of_client_in, grant_of_refresh_in, redeem_code_in, refresh_in, revoke_in, CodeRedemption,
    Issued, TokenRefusal, CODE_UNUSABLE, REFRESH_UNKNOWN,
};
use super::limits::WindowLimiter;
use super::metadata::mcp_url;
use super::{has_media_type, now_unix, oauth_error, rate_limited, resource_matches, Params};
use crate::base_url::BaseUrl;
use crate::connection::{ConnectionState, RequestBase};

const TOKEN_PARAMS: &[&str] = &[
    "grant_type",
    "client_id",
    "code",
    "redirect_uri",
    "code_verifier",
    "resource",
    "refresh_token",
];
// token_type_hint is ignored, as RFC 7009 section 2.1 allows.
const REVOKE_PARAMS: &[&str] = &["token", "client_id"];

pub(crate) async fn token(
    State(state): State<Arc<ConnectionState>>,
    Extension(RequestBase(base)): Extension<RequestBase>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match exchange(&state, &base, &headers, &body).await {
        Ok(issued) => issued_answer(issued),
        Err(refused) => refused,
    }
}

/// The tokens the request earns, after the checks of the request alone,
/// then a read that drops codes and refresh tokens the client does not
/// hold, then the client's and the grant's budgets: only requests naming a
/// real code or token count, so junk sent under a public client id neither
/// locks its users out nor takes the database's write lock.
async fn exchange(
    state: &ConnectionState,
    base: &BaseUrl,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Issued, Response> {
    let params = read_form(headers, body, TOKEN_PARAMS).map_err(IntoResponse::into_response)?;
    let grant_type = required(&params, "grant_type")?;
    let client_id = required(&params, "client_id")?;
    let resource = mcp_url(base);
    let redeemed = match grant_type {
        "authorization_code" => {
            let code = required(&params, "code")?;
            let redirect_uri = required(&params, "redirect_uri")?;
            let verifier = required(&params, "code_verifier")?;
            if !resource_matches(required(&params, "resource")?, base) {
                return Err(wrong_resource(base));
            }
            if !code_of_client_in(&state.db, code, client_id)
                .await
                .map_err(unavailable)?
            {
                return Err(refusal(TokenRefusal::InvalidGrant(CODE_UNUSABLE)));
            }
            admit(
                &state.limits.token_client,
                client_id,
                "Too many token requests from this app",
            )?;
            let redemption = CodeRedemption {
                code,
                client_id,
                redirect_uri,
                verifier,
                resource: &resource,
            };
            redeem_code_in(&state.db, &redemption, now_unix()).await
        }
        "refresh_token" => {
            let refresh = required(&params, "refresh_token")?;
            if params
                .get("resource")
                .is_some_and(|sent| !resource_matches(sent, base))
            {
                return Err(wrong_resource(base));
            }
            let Some(grant_id) = grant_of_refresh_in(&state.db, refresh, client_id)
                .await
                .map_err(unavailable)?
            else {
                return Err(refusal(TokenRefusal::InvalidGrant(REFRESH_UNKNOWN)));
            };
            admit(
                &state.limits.token_client,
                client_id,
                "Too many token requests from this app",
            )?;
            admit(
                &state.limits.token_grant,
                &grant_id,
                "Too many refreshes for this connection",
            )?;
            refresh_in(
                &state.db,
                state.key,
                refresh,
                client_id,
                &resource,
                now_unix(),
            )
            .await
        }
        _ => {
            return Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "Soulful supports the grant types authorization_code and refresh_token only",
            ))
        }
    };
    redeemed.map_err(refusal)
}

pub(crate) async fn revoke(
    State(state): State<Arc<ConnectionState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let revoked = async {
        let params =
            read_form(&headers, &body, REVOKE_PARAMS).map_err(IntoResponse::into_response)?;
        let token = required(&params, "token")?;
        let client_id = required(&params, "client_id")?;
        revoke_in(&state.db, token, client_id)
            .await
            .map_err(unavailable)
    };
    // 200 whether or not the token was one of the client's (RFC 7009 section 2.2).
    match revoked.await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(refused) => refused,
    }
}

/// Why a form could not be read.
enum FormRefusal {
    NotForm,
    /// The parameter sent more than once.
    Twice(&'static str),
}

impl IntoResponse for FormRefusal {
    fn into_response(self) -> Response {
        let description = match self {
            FormRefusal::NotForm => "The request must be form encoded".to_string(),
            FormRefusal::Twice(name) => format!("{name} is given more than once"),
        };
        oauth_error(StatusCode::BAD_REQUEST, "invalid_request", description)
    }
}

/// The parameters of a form encoded body (RFC 6749 section 3.2).
fn read_form(
    headers: &HeaderMap,
    body: &[u8],
    names: &[&'static str],
) -> Result<Params, FormRefusal> {
    if !has_media_type(headers, "application/x-www-form-urlencoded") {
        return Err(FormRefusal::NotForm);
    }
    Params::read(body, names).map_err(FormRefusal::Twice)
}

fn required<'a>(params: &'a Params, name: &str) -> Result<&'a str, Response> {
    params.get(name).ok_or_else(|| {
        oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("{name} is required"),
        )
    })
}

fn wrong_resource(base: &BaseUrl) -> Response {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_target",
        format!("resource must be {}", mcp_url(base)),
    )
}

/// Counts one request against `key`'s budget in `limiter`, or the 429 that
/// says "{what}. Try again in {n} seconds."
fn admit(limiter: &WindowLimiter, key: &str, what: &str) -> Result<(), Response> {
    limiter
        .admit(key, Instant::now())
        .map_err(|wait| rate_limited(wait, what))
}

fn refusal(refused: TokenRefusal) -> Response {
    match refused {
        TokenRefusal::InvalidGrant(text) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", text)
        }
        TokenRefusal::InvalidTarget(text) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_target", text)
        }
        TokenRefusal::Database(e) => unavailable(e),
    }
}

/// The 500 for a database failure; its text goes to the log only.
fn unavailable(e: String) -> Response {
    tracing::error!("An OAuth token request could not use the database: {e}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Soulful's database is unavailable",
    )
}

/// The RFC 6749 section 5.1 answer.
fn issued_answer(issued: Issued) -> Response {
    let mut doc = Map::new();
    doc.insert("access_token".into(), issued.access_token.into());
    doc.insert("token_type".into(), "Bearer".into());
    doc.insert("expires_in".into(), issued.expires_in.into());
    if let Some(refresh) = issued.refresh_token {
        doc.insert("refresh_token".into(), refresh.into());
    }
    doc.insert("scope".into(), issued.scope.into());
    Json(Value::Object(doc)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::Router;
    use serde_json::{json, Value};
    use sqlx::SqlitePool;

    use crate::connection::test_util::*;
    use crate::connection::{router, ConnectionState};
    use crate::db::test_db;
    use crate::oauth::limits::{Limits, WindowLimiter};
    use crate::oauth::now_unix;

    /// A router on a database where u1 exists, and a code for u1 issued now.
    async fn app_with_code() -> (SqlitePool, Router, String) {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        (db.clone(), router(ConnectionState::for_test(db)), code)
    }

    fn code_form(code: &str) -> Vec<(&'static str, String)> {
        vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.into()),
            ("client_id", "dcr_c".into()),
            ("redirect_uri", CLAUDE_CALLBACK.into()),
            ("code_verifier", VERIFIER.into()),
            ("resource", RESOURCE.into()),
        ]
    }

    fn refresh_form(refresh: &str) -> Vec<(&'static str, String)> {
        vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", refresh.into()),
            ("client_id", "dcr_c".into()),
        ]
    }

    async fn token(
        app: Router,
        headers: &[(&str, &str)],
        form: &[(&'static str, String)],
    ) -> (StatusCode, HeaderMap, Value) {
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, headers, body) =
            send(app, form_request("/oauth/token", headers, &pairs)).await;
        (
            status,
            headers,
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
        )
    }

    const AT_HOST: &[(&str, &str)] = &[("host", HOST)];

    #[tokio::test]
    async fn the_token_endpoint_takes_a_form_and_answers_json() {
        let (_, app, code) = app_with_code().await;
        let (status, headers, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (
                headers["cache-control"].to_str().unwrap(),
                headers["content-type"].to_str().unwrap()
            ),
            ("no-store", "application/json")
        );
        assert_eq!(
            (
                doc["token_type"].as_str(),
                doc["expires_in"].as_i64(),
                doc["scope"].as_str()
            ),
            (Some("Bearer"), Some(3600), Some("soulful offline_access"))
        );
        assert_eq!(
            (
                doc["access_token"].as_str().unwrap().len(),
                doc["refresh_token"].as_str().unwrap().len()
            ),
            (43, 43)
        );
        let (status, _, doc) = token(
            app.clone(),
            AT_HOST,
            &refresh_form(doc["refresh_token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(
            (status, doc["expires_in"].as_i64()),
            (StatusCode::OK, Some(3600))
        );
        let (status, _, doc) = token(app, AT_HOST, &code_form(&code)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            doc,
            json!({"error": "invalid_grant", "error_description": "The code is unknown, expired or already used"})
        );
    }

    #[tokio::test]
    async fn a_missing_field_is_named() {
        let (_, app, code) = app_with_code().await;
        for (drop, field) in [
            ("grant_type", "grant_type"),
            ("client_id", "client_id"),
            ("code_verifier", "code_verifier"),
            ("resource", "resource"),
        ] {
            let form: Vec<_> = code_form(&code)
                .into_iter()
                .filter(|(k, _)| *k != drop)
                .collect();
            let (status, _, doc) = token(app.clone(), AT_HOST, &form).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{drop}");
            assert_eq!(
                doc,
                json!({"error": "invalid_request", "error_description": format!("{field} is required")}),
                "{drop}"
            );
        }
        // An empty value counts as absent (RFC 6749 section 3.2).
        let empty: Vec<_> = code_form(&code)
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    if k == "code_verifier" {
                        String::new()
                    } else {
                        v
                    },
                )
            })
            .collect();
        let (status, _, doc) = token(app.clone(), AT_HOST, &empty).await;
        assert_eq!(
            (status, doc),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request", "error_description": "code_verifier is required"})
            )
        );
        let mut twice = code_form(&code);
        twice.push(("code", "another".into()));
        let (status, _, doc) = token(app.clone(), AT_HOST, &twice).await;
        assert_eq!(
            (status, doc),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request", "error_description": "code is given more than once"})
            )
        );
        let mut two_resources = code_form(&code);
        two_resources.push(("resource", RESOURCE.into()));
        let (status, _, doc) = token(app.clone(), AT_HOST, &two_resources).await;
        assert_eq!(
            (status, doc),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request", "error_description": "resource is given more than once"})
            )
        );
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(code_form(&code))
            .finish();
        let as_json = Request::post("/oauth/token")
            .header("host", HOST)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _, body) = send(app.clone(), as_json).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error": "invalid_request", "error_description": "The request must be form encoded"})
        );
        // None of these touched the code.
        assert_eq!(
            token(app, AT_HOST, &code_form(&code)).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_token_cannot_be_fetched_or_refreshed_through_another_address() {
        let (_, app, code) = app_with_code().await;
        let (status, _, doc) = token(app.clone(), TUNNEL, &code_form(&code)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            doc,
            json!({"error": "invalid_target", "error_description": "resource must be https://tunnel.example.net/mcp"})
        );
        let (status, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        assert_eq!(status, StatusCode::OK);
        let refresh = doc["refresh_token"].as_str().unwrap().to_string();
        let (status, _, doc) = token(app, TUNNEL, &refresh_form(&refresh)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            doc,
            json!({"error": "invalid_target", "error_description": "The refresh token was issued for another address of this server"})
        );
    }

    // Limits gains fields in later tasks; a test sets the one limit it needs
    // by assignment, so it does not change when they come.
    #[allow(clippy::field_reassign_with_default)]
    #[tokio::test]
    async fn the_token_endpoint_limits_each_client() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let mut limits = Limits::default();
        limits.token_client = WindowLimiter::new(1, Duration::from_secs(60));
        let app = router(ConnectionState {
            limits: Arc::new(limits),
            ..ConnectionState::for_test(db.clone())
        });
        let first = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let second = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let (first, _, _) = token(app.clone(), AT_HOST, &code_form(&first)).await;
        let (second, headers, doc) = token(app, AT_HOST, &code_form(&second)).await;
        assert_eq!(
            (first, second),
            (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
        );
        let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((59..=60).contains(&wait), "{wait}");
        assert_eq!(
            doc,
            json!({"error": "rate_limited", "error_description": format!("Too many token requests from this app. Try again in {wait} seconds.")})
        );
    }

    #[tokio::test]
    async fn codes_nobody_issued_do_not_count_against_the_app() {
        let (_, app, code) = app_with_code().await;
        // More than the 30 a minute the app may send.
        for i in 0..40 {
            let (status, _, doc) =
                token(app.clone(), AT_HOST, &code_form(&format!("made-up-{i}"))).await;
            assert_eq!(
                (status, doc),
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "invalid_grant", "error_description": "The code is unknown, expired or already used"})
                ),
                "{i}"
            );
        }
        assert_eq!(
            token(app, AT_HOST, &code_form(&code)).await.0,
            StatusCode::OK
        );
    }

    #[allow(clippy::field_reassign_with_default)]
    #[tokio::test]
    async fn the_token_endpoint_limits_each_grants_refreshes() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let mut limits = Limits::default();
        limits.token_grant = WindowLimiter::new(1, Duration::from_secs(60));
        let app = router(ConnectionState {
            limits: Arc::new(limits),
            ..ConnectionState::for_test(db.clone())
        });
        let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let (_, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        let (status, _, doc) = token(
            app.clone(),
            AT_HOST,
            &refresh_form(doc["refresh_token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, refused) = token(
            app.clone(),
            AT_HOST,
            &refresh_form(doc["refresh_token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert_eq!(
            refused,
            json!({"error": "rate_limited", "error_description": format!("Too many refreshes for this connection. Try again in {wait} seconds.")})
        );
        // A new authorization is a new grant with its own budget.
        let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let (_, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        assert_eq!(
            token(
                app,
                AT_HOST,
                &refresh_form(doc["refresh_token"].as_str().unwrap())
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_unknown_refresh_token_is_refused() {
        let (_, app, _) = app_with_code().await;
        let (status, _, doc) = token(app, AT_HOST, &refresh_form("made-up")).await;
        assert_eq!(
            (status, doc),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_grant", "error_description": "The refresh token is unknown, expired or revoked"})
            )
        );
    }

    #[tokio::test]
    async fn other_grant_types_are_unsupported() {
        let (_, app, _) = app_with_code().await;
        let form = vec![
            ("grant_type", "client_credentials".to_string()),
            ("client_id", "dcr_c".to_string()),
        ];
        let (status, _, doc) = token(app, AT_HOST, &form).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            doc,
            json!({"error": "unsupported_grant_type",
            "error_description": "Soulful supports the grant types authorization_code and refresh_token only"})
        );
    }

    #[tokio::test]
    async fn an_unavailable_database_is_a_server_error() {
        let (db, app, code) = app_with_code().await;
        db.close().await;
        let (status, _, doc) = token(app, AT_HOST, &code_form(&code)).await;
        assert_eq!(
            (status, doc),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "server_error", "error_description": "Soulful's database is unavailable"})
            )
        );
    }

    #[tokio::test]
    async fn revocation_answers_200_whatever_the_token() {
        let (db, app, code) = app_with_code().await;
        let (_, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        let refresh = doc["refresh_token"].as_str().unwrap();
        let revoke = |t: &str| {
            send(
                app.clone(),
                form_request(
                    "/oauth/revoke",
                    AT_HOST,
                    &[
                        ("token", t),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", "dcr_c"),
                    ],
                ),
            )
        };
        let (status, _, body) = revoke("made-up").await;
        assert_eq!(
            (status, body.is_empty(), count(&db, "oauth_grants").await),
            (StatusCode::OK, true, 1)
        );
        let (status, _, _) = revoke(refresh).await;
        assert_eq!(
            (status, count(&db, "oauth_grants").await),
            (StatusCode::OK, 0)
        );
        let (status, _, body) = send(
            app,
            form_request("/oauth/revoke", AT_HOST, &[("token", "x")]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error": "invalid_request", "error_description": "client_id is required"})
        );
    }
}
