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
    presented_code_in, presented_refresh_in, redeem_code_in, refresh_in, revoke_in, CodeRedemption,
    Issued, Presented, TokenRefusal, CODE_UNUSABLE, REFRESH_UNKNOWN,
};
use super::limits::{Limits, WindowLimiter};
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

/// What a token request asks to redeem.
enum Redeem<'a> {
    Code(CodeRedemption<'a>),
    Refresh(&'a str),
}

/// The tokens the request earns, after the checks of the request alone,
/// then a read that finds what its code or refresh token can still do,
/// then the budgets of the token's grant and user. Only a request that
/// would issue tokens counts, and a request no budget admits counts against
/// none; a code or token that can change nothing is refused before the
/// database's write lock.
async fn exchange(
    state: &ConnectionState,
    base: &BaseUrl,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Issued, Response> {
    let params = read_form(headers, body, TOKEN_PARAMS).map_err(|refused| match refused {
        // RFC 8707 section 2 allows several resources; Soulful issues tokens for one.
        FormRefusal::Twice("resource") => wrong_resource(base),
        refused => refused.into_response(),
    })?;
    let grant_type = required(&params, "grant_type")?;
    let client_id = required(&params, "client_id")?;
    let resource = mcp_url(base);
    let redeem = match grant_type {
        "authorization_code" => {
            let code = required(&params, "code")?;
            let redirect_uri = required(&params, "redirect_uri")?;
            let verifier = required(&params, "code_verifier")?;
            if !resource_matches(required(&params, "resource")?, base) {
                return Err(wrong_resource(base));
            }
            Redeem::Code(CodeRedemption {
                code,
                client_id,
                redirect_uri,
                verifier,
                resource: &resource,
            })
        }
        "refresh_token" => {
            let refresh = required(&params, "refresh_token")?;
            if params
                .get("resource")
                .is_some_and(|sent| !resource_matches(sent, base))
            {
                return Err(wrong_resource(base));
            }
            Redeem::Refresh(refresh)
        }
        _ => {
            return Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "Soulful supports the grant types authorization_code and refresh_token only",
            ))
        }
    };
    let now = now_unix();
    let (presented, unknown) = match &redeem {
        Redeem::Code(r) => (
            presented_code_in(&state.db, r.code, client_id, now).await,
            CODE_UNUSABLE,
        ),
        Redeem::Refresh(refresh) => (
            presented_refresh_in(&state.db, refresh, client_id, now).await,
            REFRESH_UNKNOWN,
        ),
    };
    let Some(presented) = presented.map_err(|e| unavailable("token", e))? else {
        return Err(refusal(TokenRefusal::InvalidGrant(unknown)));
    };
    if presented.issues {
        admit(&state.limits, &presented)?;
    }
    let redeemed = match redeem {
        Redeem::Code(r) => redeem_code_in(&state.db, &r, now).await,
        Redeem::Refresh(refresh) => {
            refresh_in(&state.db, state.key, refresh, client_id, &resource, now).await
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
            .map_err(|e| unavailable("revocation", e))
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

/// Counts the request against the budgets of `presented`'s grant (a refresh)
/// and user when both admit it, or the 429 of the first that does not,
/// which counts it against neither.
fn admit(limits: &Limits, presented: &Presented) -> Result<(), Response> {
    const GRANT: &str = "Too many refreshes for this connection";
    const USER: &str = "Too many token requests for this Soulful account";
    let mut budgets: Vec<(&WindowLimiter, &str, &str)> = Vec::with_capacity(2);
    if let Some(grant_id) = &presented.grant_id {
        budgets.push((&limits.token_grant, grant_id, GRANT));
    }
    budgets.push((&limits.token_user, &presented.user_id, USER));
    let now = Instant::now();
    if let Some((wait, what)) = budgets
        .iter()
        .find_map(|(limiter, key, what)| limiter.wait(key, now).map(|wait| (wait, *what)))
    {
        return Err(rate_limited(wait, what));
    }
    for (limiter, key, what) in budgets {
        // Another request may have filled the window since the check.
        limiter
            .admit(key, now)
            .map_err(|wait| rate_limited(wait, what))?;
    }
    Ok(())
}

fn refusal(refused: TokenRefusal) -> Response {
    match refused {
        TokenRefusal::InvalidGrant(text) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", text)
        }
        TokenRefusal::InvalidTarget(text) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_target", text)
        }
        TokenRefusal::Database(e) => unavailable("token", e),
    }
}

/// The 500 for a database failure of an OAuth `request` ("token" or
/// "revocation"); the failure's text goes to the log only.
fn unavailable(request: &str, e: String) -> Response {
    tracing::error!("An OAuth {request} request could not use the database: {e}");
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
                json!({"error": "invalid_target", "error_description": "resource must be https://music.example.com/mcp"})
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

    /// A router on `db` with `limits`.
    fn limited(db: &SqlitePool, limits: Limits) -> Router {
        router(ConnectionState {
            limits: Arc::new(limits),
            ..ConnectionState::for_test(db.clone())
        })
    }

    #[tokio::test]
    async fn the_token_endpoint_limits_each_user_not_each_app() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        add_user(&db, "u2", "bob").await;
        let app = limited(
            &db,
            Limits {
                token_user: WindowLimiter::new(1, Duration::from_secs(60)),
                ..Limits::default()
            },
        );
        let first = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let second = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let bobs = issue_code(&db, "u2", "soulful offline_access", RESOURCE, now_unix()).await;
        let (first, _, _) = token(app.clone(), AT_HOST, &code_form(&first)).await;
        let (second, headers, doc) = token(app.clone(), AT_HOST, &code_form(&second)).await;
        assert_eq!(
            (first, second),
            (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
        );
        let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((59..=60).contains(&wait), "{wait}");
        assert_eq!(
            doc,
            json!({"error": "rate_limited", "error_description": format!("Too many token requests for this Soulful account. Try again in {wait} seconds.")})
        );
        // bob's Claude shares alice's client id and is not locked out.
        assert_eq!(
            token(app, AT_HOST, &code_form(&bobs)).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_used_code_counts_against_no_budget_and_ends_its_grant_once() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "mallory").await;
        add_user(&db, "u2", "bob").await;
        let app = limited(
            &db,
            Limits {
                token_user: WindowLimiter::new(2, Duration::from_secs(60)),
                ..Limits::default()
            },
        );
        let used = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        assert_eq!(
            token(app.clone(), AT_HOST, &code_form(&used)).await.0,
            StatusCode::OK
        );
        // More than the 30 a minute an account may send.
        for i in 0..40 {
            let (status, _, doc) = token(app.clone(), AT_HOST, &code_form(&used)).await;
            assert_eq!(
                (status, doc),
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "invalid_grant", "error_description": "The code is unknown, expired or already used"})
                ),
                "{i}"
            );
        }
        // The first replay ended the grant; the code now names none.
        assert_eq!(count(&db, "oauth_grants").await, 0);
        let grant_left: Option<String> =
            sqlx::query_scalar("SELECT grant_id FROM oauth_codes WHERE user_id = 'u1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(grant_left, None);
        let bobs = issue_code(&db, "u2", "soulful offline_access", RESOURCE, now_unix()).await;
        let mallorys = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        assert_eq!(
            (
                token(app.clone(), AT_HOST, &code_form(&bobs)).await.0,
                token(app, AT_HOST, &code_form(&mallorys)).await.0
            ),
            (StatusCode::OK, StatusCode::OK)
        );
    }

    #[tokio::test]
    async fn a_refresh_one_budget_refuses_uses_up_no_other() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let app = limited(
            &db,
            Limits {
                token_grant: WindowLimiter::new(1, Duration::from_secs(60)),
                token_user: WindowLimiter::new(3, Duration::from_secs(60)),
                ..Limits::default()
            },
        );
        let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let (_, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        let (status, _, doc) = token(
            app.clone(),
            AT_HOST,
            &refresh_form(doc["refresh_token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let refresh = doc["refresh_token"].as_str().unwrap().to_string();
        for _ in 0..3 {
            let (_, _, refused) = token(app.clone(), AT_HOST, &refresh_form(&refresh)).await;
            assert_eq!(refused["error"], "rate_limited");
            assert!(refused["error_description"]
                .as_str()
                .unwrap()
                .starts_with("Too many refreshes for this connection."));
        }
        // Two of alice's three units are spent: the refused refreshes took none.
        let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, now_unix()).await;
        let (status, _, doc) = token(app.clone(), AT_HOST, &code_form(&code)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, refused) = token(
            app,
            AT_HOST,
            &refresh_form(doc["refresh_token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(refused["error_description"]
            .as_str()
            .unwrap()
            .starts_with("Too many token requests for this Soulful account."));
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

    #[tokio::test]
    async fn the_token_endpoint_limits_each_grants_refreshes() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let app = limited(
            &db,
            Limits {
                token_grant: WindowLimiter::new(1, Duration::from_secs(60)),
                ..Limits::default()
            },
        );
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
