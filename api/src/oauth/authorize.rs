//! `GET /oauth/authorize` asks the signed-in user whether an app may use
//! Soulful; `POST /oauth/authorize` takes the answer and sends the browser
//! back to the app with a code or with `access_denied`.
//!
//! Only the user's answer goes back to the app. Every other refusal is
//! shown on a page: any https address can be registered, so a redirect on
//! a bad request would make Soulful an open redirector (RFC 9700 4.11.2).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::RETRY_AFTER;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Extension;
use shared::oauth::AUTHORIZE_PATH;
use tower_cookies::Cookies;
use url::Url;

use super::cimd::{client_id_url, CimdError, ClientDocument};
use super::clients::{
    find_in, is_loopback_uri, redirect_matches, upsert_cimd_in, ClientKind, KnownClient,
};
use super::codes::{store_code_in, store_request_in, take_request_in, PendingRequest};
use super::consent::{consent_page, error_page, ConsentView};
use super::limits::retry_after_seconds;
use super::metadata::mcp_url;
use super::secrets::new_secret;
use super::{now_unix, resource_matches, OFFLINE_ACCESS, SCOPE};
use crate::auth::UserCtx;
use crate::base_url::BaseUrl;
use crate::connection::{ConnectionState, RequestBase};
use crate::server_fns::guard::load_session;

const AUTHORIZE_PARAMS: &[&str] = &[
    "response_type",
    "client_id",
    "redirect_uri",
    "state",
    "code_challenge",
    "code_challenge_method",
    "scope",
    "resource",
];
const ANSWER_PARAMS: &[&str] = &["request", "decision"];

const UNKNOWN_APP: &str =
    "Soulful does not know this app. Start the connection again from the app.";
const UNREGISTERED_REDIRECT: &str = "This app asked to return to an address it did not register.";
const SESSION_ENDED: &str = "Your Soulful session ended. Start the connection again from the app.";
const REQUEST_GONE: &str =
    "This sign-in request expired or was already used. Start the connection again from the app.";
const NOT_UNDERSTOOD: &str =
    "Soulful did not understand the answer. Start the connection again from the app.";
const DATABASE_UNAVAILABLE: &str =
    "Soulful's database is unavailable. Start the connection again from the app.";

/// The parameters Soulful reads from a query or form. Following RFC 6749
/// section 3.1, a parameter without a value counts as absent, other
/// parameters are ignored, and one sent twice refuses the request.
struct Params(Vec<(&'static str, String)>);

impl Params {
    fn read(raw: &[u8], names: &[&'static str]) -> Result<Params, Response> {
        let mut found: Vec<(&'static str, String)> = Vec::new();
        for (name, value) in url::form_urlencoded::parse(raw) {
            let Some(&name) = names.iter().find(|n| **n == name) else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            if found.iter().any(|(seen, _)| *seen == name) {
                return Err(error_page(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "This app sent {name} more than once. Start the connection again from \
                         the app."
                    ),
                ));
            }
            found.push((name, value.into_owned()));
        }
        Ok(Params(found))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The user the session cookie signs in, if it still counts.
async fn session(state: &ConnectionState, cookies: &Cookies) -> Option<UserCtx> {
    let token = cookies
        .get(crate::AUTH_COOKIE_NAME)
        .map(|c| c.value().to_string());
    load_session(token.as_deref(), &state.db, state.key)
        .await
        .ok()
}

pub(crate) async fn show(
    State(state): State<Arc<ConnectionState>>,
    Extension(RequestBase(base)): Extension<RequestBase>,
    cookies: Cookies,
    uri: Uri,
) -> Response {
    let params = match Params::read(uri.query().unwrap_or("").as_bytes(), AUTHORIZE_PARAMS) {
        Ok(params) => params,
        Err(refused) => return refused,
    };
    let Some(user) = session(&state, &cookies).await else {
        let back = match uri.query() {
            Some(query) => format!("{AUTHORIZE_PATH}?{query}"),
            None => AUTHORIZE_PATH.to_string(),
        };
        let back: String = url::form_urlencoded::byte_serialize(back.as_bytes()).collect();
        return Redirect::to(&format!("/login?return_to={back}")).into_response();
    };
    let Some(client_id) = params.get("client_id") else {
        return error_page(StatusCode::BAD_REQUEST, UNKNOWN_APP);
    };
    // A CIMD client is known by its URL's host, a registered one by its
    // redirect's host (D8).
    let (client, client_id_host) = if client_id.starts_with("https://") {
        match document_client(&state, &user, client_id).await {
            Ok((client, host)) => (client, Some(host)),
            Err(refused) => return refused,
        }
    } else {
        match find_in(&state.db, client_id).await {
            Ok(Some(client)) => (client, None),
            Ok(None) => return error_page(StatusCode::BAD_REQUEST, UNKNOWN_APP),
            Err(e) => {
                tracing::error!("Could not read an OAuth client: {e}");
                return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
            }
        }
    };
    let redirect = params
        .get("redirect_uri")
        .filter(|sent| {
            client
                .redirect_uris
                .iter()
                .any(|r| redirect_matches(r, sent))
        })
        .and_then(|sent| Some((sent, Url::parse(sent).ok()?.host_str()?.to_string())));
    let Some((redirect_uri, redirect_host)) = redirect else {
        return error_page(StatusCode::BAD_REQUEST, UNREGISTERED_REDIRECT);
    };
    let (scope, code_challenge) = match validate(&params, &base) {
        Ok(granted) => granted,
        Err((error, description)) => {
            return error_page(
                StatusCode::BAD_REQUEST,
                &format!(
                    "This app sent a request Soulful cannot accept ({error}: {description}). \
                     Start the connection again from the app."
                ),
            )
        }
    };
    let request_id = new_secret();
    let pending = PendingRequest {
        user_id: user.user_id.clone(),
        client,
        redirect_uri: redirect_uri.to_string(),
        state: params.get("state").map(str::to_string),
        code_challenge: code_challenge.to_string(),
        scope,
        resource: mcp_url(&base),
    };
    if let Err(e) = store_request_in(&state.db, &request_id, &pending, now_unix()).await {
        tracing::error!("Could not store an OAuth authorize request: {e}");
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
    }
    consent_page(&ConsentView {
        app_host: client_id_host.as_deref().unwrap_or(&redirect_host),
        client_name: pending.client.client_name.as_deref(),
        redirect_host: &redirect_host,
        loopback_only: pending
            .client
            .redirect_uris
            .iter()
            .all(|r| is_loopback_uri(r)),
        username: &user.username,
        request_id: &request_id,
    })
}

/// The client a client id URL names, from its document fetched now, and the
/// URL's host; or the page that says why it cannot be used.
async fn document_client(
    state: &ConnectionState,
    user: &UserCtx,
    client_id: &str,
) -> Result<(KnownClient, String), Response> {
    let unusable = |e: CimdError| {
        error_page(
            StatusCode::BAD_REQUEST,
            &format!("Soulful could not use this app's client metadata at {client_id}: {e}"),
        )
    };
    let url = client_id_url(client_id).map_err(unusable)?;
    if let Err(wait) = state.limits.cimd_fetch.admit(&user.user_id, Instant::now()) {
        let seconds = retry_after_seconds(wait);
        let mut refused = error_page(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("Too many app sign-ins in a minute. Try again in {seconds} seconds."),
        );
        refused.headers_mut().insert(RETRY_AFTER, seconds.into());
        return Err(refused);
    }
    let doc = state.documents.fetch(&url).await.map_err(unusable)?;
    let host = url.host_str().expect("an https URL has a host").to_string();
    let client = KnownClient {
        client_id: doc.client_id,
        kind: ClientKind::Cimd,
        client_name: Some(doc.client_name),
        redirect_uris: doc.redirect_uris,
    };
    Ok((client, host))
}

/// The granted scope and the code challenge, or the OAuth error code and
/// description of the first rule the request breaks.
fn validate<'a>(
    params: &'a Params,
    base: &BaseUrl,
) -> Result<(String, &'a str), (&'static str, String)> {
    if params.get("response_type") != Some("code") {
        return Err((
            "unsupported_response_type",
            "response_type must be code".into(),
        ));
    }
    let code_challenge = match (
        params.get("code_challenge"),
        params.get("code_challenge_method"),
    ) {
        (Some(challenge), Some("S256")) => challenge,
        _ => return Err(("invalid_request", "PKCE with S256 is required".into())),
    };
    let requested: Vec<&str> = params
        .get("scope")
        .map(|scope| scope.split(' ').collect())
        .unwrap_or_default();
    if requested
        .iter()
        .any(|s| *s != SCOPE && *s != OFFLINE_ACCESS)
    {
        return Err((
            "invalid_scope",
            "Soulful grants the scopes soulful and offline_access only".into(),
        ));
    }
    if !params
        .get("resource")
        .is_some_and(|resource| resource_matches(resource, base))
    {
        return Err((
            "invalid_target",
            format!("resource must be {}", mcp_url(base)),
        ));
    }
    let scope = if requested.contains(&OFFLINE_ACCESS) {
        format!("{SCOPE} {OFFLINE_ACCESS}")
    } else {
        SCOPE.to_string()
    };
    Ok((scope, code_challenge))
}

pub(crate) async fn answer(
    State(state): State<Arc<ConnectionState>>,
    Extension(RequestBase(base)): Extension<RequestBase>,
    cookies: Cookies,
    body: Bytes,
) -> Response {
    let params = match Params::read(&body, ANSWER_PARAMS) {
        Ok(params) => params,
        Err(refused) => return refused,
    };
    let Some(user) = session(&state, &cookies).await else {
        return error_page(StatusCode::UNAUTHORIZED, SESSION_ENDED);
    };
    let (Some(request_id), Some(allow)) = (
        params.get("request"),
        match params.get("decision") {
            Some("allow") => Some(true),
            Some("deny") => Some(false),
            _ => None,
        },
    ) else {
        return error_page(StatusCode::BAD_REQUEST, NOT_UNDERSTOOD);
    };
    let now = now_unix();
    let pending = match take_request_in(&state.db, request_id, &user.user_id, now).await {
        Ok(Some(pending)) => pending,
        Ok(None) => return error_page(StatusCode::BAD_REQUEST, REQUEST_GONE),
        Err(e) => {
            tracing::error!("Could not read an OAuth authorize request: {e}");
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
        }
    };
    if !allow {
        return back_to_app(
            &pending,
            &base,
            &[
                ("error", "access_denied"),
                ("error_description", "The user did not allow the connection"),
            ],
        );
    }
    // The code's client must exist: a CIMD client is stored once allowed.
    if pending.client.kind == ClientKind::Cimd {
        let Some(client_name) = pending.client.client_name.clone() else {
            tracing::error!("A stored CIMD authorize request has no client name");
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
        };
        let doc = ClientDocument {
            client_id: pending.client.client_id.clone(),
            client_name,
            redirect_uris: pending.client.redirect_uris.clone(),
        };
        if let Err(e) = upsert_cimd_in(&state.db, &doc, now).await {
            tracing::error!("Could not store an OAuth client: {e}");
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
        }
    }
    let code = new_secret();
    if let Err(e) = store_code_in(&state.db, &code, &pending, now).await {
        tracing::error!("Could not store an OAuth authorization code: {e}");
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
    }
    back_to_app(&pending, &base, &[("code", &code)])
}

/// A 303 to the request's redirect URI with `first`, then `state` when the
/// app sent one and `iss` (RFC 9207), added to its query.
fn back_to_app(pending: &PendingRequest, base: &BaseUrl, first: &[(&str, &str)]) -> Response {
    let Ok(mut to) = Url::parse(&pending.redirect_uri) else {
        tracing::error!("A stored OAuth redirect URI does not parse");
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, DATABASE_UNAVAILABLE);
    };
    {
        let mut query = to.query_pairs_mut();
        query.extend_pairs(first);
        if let Some(state) = &pending.state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", &base.to_string());
    }
    Redirect::to(to.as_str()).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{HeaderMap, StatusCode};
    use axum::Router;
    use sqlx::SqlitePool;
    use tower_cookies::CookieManagerLayer;
    use url::Url;

    use crate::connection::test_util::*;
    use crate::connection::{router, ConnectionState};
    use crate::db::test_db;
    use crate::oauth::cimd::{CimdError, ClientDocument};
    use crate::oauth::clients::{insert_dcr_in, NewClient, DCR_CAP};
    use crate::oauth::limits::{Limits, WindowLimiter};
    use crate::oauth::now_unix;
    use crate::oauth::secrets::hash;

    /// The authorize query Claude sends, with `changes` applied (None removes the pair).
    fn query_with(changes: &[(&'static str, Option<&'static str>)]) -> String {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", "dcr_test"),
            ("redirect_uri", CLAUDE_CALLBACK),
            ("state", "a&b"),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("scope", "soulful offline_access"),
            ("resource", "https://music.example.com/mcp"),
        ];
        for &(name, value) in changes {
            pairs.retain(|(k, _)| *k != name);
            if let Some(v) = value {
                pairs.push((name, v));
            }
        }
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    }

    fn query() -> String {
        query_with(&[])
    }

    /// A database holding DCR client dcr_test, and the router with the cookie layer.
    async fn app() -> (SqlitePool, Router) {
        let db = test_db::migrated().await;
        insert_dcr_in(
            &db,
            "dcr_test",
            &NewClient {
                client_name: Some("<script>alert(1)</script>".into()),
                redirect_uris: vec![CLAUDE_CALLBACK.into()],
            },
            0,
            DCR_CAP,
        )
        .await
        .unwrap();
        let app = router(ConnectionState::for_test(db.clone())).layer(CookieManagerLayer::new());
        (db, app)
    }

    /// Where a 303 to the app goes: the URL without query, and its pairs in order.
    fn redirect_of(headers: &HeaderMap) -> (String, Vec<(String, String)>) {
        let to = Url::parse(headers["location"].to_str().unwrap()).unwrap();
        (
            to[..url::Position::AfterPath].to_string(),
            to.query_pairs().into_owned().collect(),
        )
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn without_a_session_the_browser_goes_to_sign_in_and_comes_back() {
        let (_, app) = app().await;
        let (status, headers, _) = get(app, &format!("/oauth/authorize?{}", query()), None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = headers["location"].to_str().unwrap();
        assert!(location.starts_with("/login?return_to="), "{location}");
        let back = Url::parse(&format!("https://music.example.com{location}"))
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "return_to")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(back, format!("/oauth/authorize?{}", query()));
        assert!(back.contains("state=a%26b"));
        assert_eq!(
            shared::oauth::accepted_return_to(&back),
            Some(back.as_str())
        );
    }

    #[tokio::test]
    async fn an_unknown_app_or_return_address_gets_a_page_not_a_redirect() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        for (change, text) in [
            (
                ("client_id", Some("dcr_nobody")),
                "Soulful does not know this app. Start the connection again from the app.",
            ),
            (
                ("client_id", None),
                "Soulful does not know this app. Start the connection again from the app.",
            ),
            (
                ("redirect_uri", Some("https://evil.example/cb")),
                "This app asked to return to an address it did not register.",
            ),
            (
                ("redirect_uri", None),
                "This app asked to return to an address it did not register.",
            ),
        ] {
            let (status, headers, page) = get(
                app.clone(),
                &format!("/oauth/authorize?{}", query_with(&[change])),
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{change:?}");
            assert!(headers.get("location").is_none(), "{change:?}");
            assert!(page.contains(text), "{change:?}");
        }
    }

    #[tokio::test]
    async fn a_parameter_sent_twice_gets_a_page() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let twice = format!(
            "{}&{}",
            query(),
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("redirect_uri", "https://evil.example/cb")
                .finish()
        );
        for cookie in [None, Some(cookie.as_str())] {
            let (status, headers, page) =
                get(app.clone(), &format!("/oauth/authorize?{twice}"), cookie).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(headers.get("location").is_none());
            assert!(page.contains(
                "This app sent redirect_uri more than once. Start the connection again from the app."
            ));
        }
        assert_eq!(count(&db, "oauth_requests").await, 0);
    }

    #[tokio::test]
    async fn request_errors_are_shown_on_a_page_not_sent_to_the_app() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        for (change, error, description) in [
            (
                ("response_type", Some("token")),
                "unsupported_response_type",
                "response_type must be code",
            ),
            (
                ("code_challenge", None),
                "invalid_request",
                "PKCE with S256 is required",
            ),
            (
                ("code_challenge_method", Some("plain")),
                "invalid_request",
                "PKCE with S256 is required",
            ),
            (
                ("scope", Some("soulful admin")),
                "invalid_scope",
                "Soulful grants the scopes soulful and offline_access only",
            ),
            (
                ("resource", Some("https://other.example/mcp")),
                "invalid_target",
                "resource must be https://music.example.com/mcp",
            ),
            (
                ("resource", None),
                "invalid_target",
                "resource must be https://music.example.com/mcp",
            ),
        ] {
            let (status, headers, page) = get(
                app.clone(),
                &format!("/oauth/authorize?{}", query_with(&[change])),
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{change:?}");
            assert!(headers.get("location").is_none(), "{change:?}");
            let text = format!(
                "This app sent a request Soulful cannot accept ({error}: {description}). \
                 Start the connection again from the app."
            );
            assert!(page.contains(&text), "{change:?}: {page}");
        }
        assert_eq!(count(&db, "oauth_requests").await, 0);
    }

    #[tokio::test]
    async fn consent_names_the_app_the_redirect_and_the_user() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (status, headers, page) =
            get(app, &format!("/oauth/authorize?{}", query()), Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers["content-security-policy"],
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"
        );
        assert_eq!(headers["referrer-policy"], "no-referrer");
        for text in [
            "<title>Connect claude.ai to Soulful</title>",
            "claude.ai wants to use Soulful as alice.",
            "It calls itself \"&lt;script&gt;alert(1)&lt;/script&gt;\".",
            "When you answer, Soulful sends you back to claude.ai.",
            r#"<form method="post" action="/oauth/authorize">"#,
            r#"<button type="submit" name="decision" value="allow">Allow</button>"#,
            r#"<button type="submit" name="decision" value="deny">Deny</button>"#,
        ] {
            assert!(page.contains(text), "{text}");
        }
        assert!(!page.contains("<script>") && !page.contains("runs on a computer"));
        assert_eq!(request_id_of(&page).len(), 43);
        assert_eq!(count(&db, "oauth_requests").await, 1);
    }

    #[tokio::test]
    async fn a_resource_with_uppercase_host_and_default_port_is_this_endpoint() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let q = query_with(&[("resource", Some("https://Music.Example.com:443/mcp"))]);
        let (status, _, page) = get(app, &format!("/oauth/authorize?{q}"), Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let resource: String = sqlx::query_scalar("SELECT resource FROM oauth_requests")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(resource, "https://music.example.com/mcp");
    }

    #[tokio::test]
    async fn allowing_sends_a_single_use_code_with_state_and_issuer() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (_, _, page) = get(
            app.clone(),
            &format!("/oauth/authorize?{}", query()),
            Some(&cookie),
        )
        .await;
        let id = request_id_of(&page);
        let before = now_unix();
        let (status, headers, _) = post_form(
            app.clone(),
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &id), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (to, got) = redirect_of(&headers);
        assert_eq!(
            (to.as_str(), got.len(), got[0].0.as_str()),
            (CLAUDE_CALLBACK, 3, "code")
        );
        let code = &got[0].1;
        assert_eq!(code.len(), 43);
        assert_eq!(
            got[1..],
            pairs(&[("state", "a&b"), ("iss", "https://music.example.com")])
        );
        let row: (String, String, String, String, String, String, i64) = sqlx::query_as(
            "SELECT user_id, client_id, redirect_uri, code_challenge, scope, resource, expires_at \
             FROM oauth_codes WHERE code_hash = ?",
        )
        .bind(hash(code))
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            (
                row.0.as_str(),
                row.1.as_str(),
                row.2.as_str(),
                row.3.as_str(),
                row.4.as_str(),
                row.5.as_str()
            ),
            (
                "u1",
                "dcr_test",
                CLAUDE_CALLBACK,
                CHALLENGE,
                "soulful offline_access",
                "https://music.example.com/mcp"
            )
        );
        assert!((before + 60..=before + 61).contains(&row.6), "{}", row.6);
        let (status, _, page) = post_form(
            app,
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &id), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains(
            "This sign-in request expired or was already used. Start the connection again from the app."
        ));
        assert_eq!(count(&db, "oauth_codes").await, 1);
    }

    #[tokio::test]
    async fn another_users_session_cannot_answer_the_request() {
        let (db, app) = app().await;
        let alice = signed_in(&db, "u1", "alice").await;
        let bob = signed_in(&db, "u2", "bob").await;
        let (_, _, page) = get(
            app.clone(),
            &format!("/oauth/authorize?{}", query()),
            Some(&alice),
        )
        .await;
        let id = request_id_of(&page);
        let (status, _, page) = post_form(
            app.clone(),
            "/oauth/authorize",
            Some(&bob),
            &[("request", &id), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("expired or was already used"));
        let (status, _, _) = post_form(
            app,
            "/oauth/authorize",
            Some(&alice),
            &[("request", &id), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn an_answer_without_a_session_is_refused() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (_, _, page) = get(
            app.clone(),
            &format!("/oauth/authorize?{}", query()),
            Some(&cookie),
        )
        .await;
        let (status, _, page) = post_form(
            app,
            "/oauth/authorize",
            None,
            &[("request", &request_id_of(&page)), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            page.contains("Your Soulful session ended. Start the connection again from the app.")
        );
        assert_eq!(count(&db, "oauth_requests").await, 1);
    }

    #[tokio::test]
    async fn an_answer_other_than_allow_or_deny_keeps_the_request() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (_, _, page) = get(
            app.clone(),
            &format!("/oauth/authorize?{}", query()),
            Some(&cookie),
        )
        .await;
        let id = request_id_of(&page);
        for answer in [
            vec![("request", id.as_str()), ("decision", "maybe")],
            vec![("request", id.as_str())],
        ] {
            let (status, headers, page) =
                post_form(app.clone(), "/oauth/authorize", Some(&cookie), &answer).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{answer:?}");
            assert!(headers.get("location").is_none(), "{answer:?}");
            assert!(page.contains(
                "Soulful did not understand the answer. Start the connection again from the app."
            ));
        }
        assert_eq!(count(&db, "oauth_requests").await, 1);
        let (status, _, _) = post_form(
            app,
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &id), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn denying_sends_access_denied() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (_, _, page) = get(
            app.clone(),
            &format!("/oauth/authorize?{}", query()),
            Some(&cookie),
        )
        .await;
        let (status, headers, _) = post_form(
            app,
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &request_id_of(&page)), ("decision", "deny")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            redirect_of(&headers),
            (
                CLAUDE_CALLBACK.to_string(),
                pairs(&[
                    ("error", "access_denied"),
                    ("error_description", "The user did not allow the connection"),
                    ("state", "a&b"),
                    ("iss", "https://music.example.com")
                ])
            )
        );
        assert_eq!(
            (
                count(&db, "oauth_codes").await,
                count(&db, "oauth_requests").await
            ),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn a_loopback_only_app_gets_a_warning() {
        let (db, app) = app().await;
        insert_dcr_in(
            &db,
            "dcr_cli",
            &NewClient {
                client_name: None,
                redirect_uris: vec!["http://127.0.0.1/callback".into()],
            },
            0,
            DCR_CAP,
        )
        .await
        .unwrap();
        let cookie = signed_in(&db, "u1", "alice").await;
        let q = query_with(&[
            ("client_id", Some("dcr_cli")),
            ("redirect_uri", Some("http://127.0.0.1:50000/callback")),
        ]);
        let (status, _, page) = get(app, &format!("/oauth/authorize?{q}"), Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Connect 127.0.0.1 to Soulful"));
        assert!(page.contains(
            "This app runs on a computer (it returns to 127.0.0.1). Allow it only if you started it yourself."
        ));
        assert!(!page.contains("It calls itself"));
    }

    /// A database, the documents the router fetches (Claude Code's id answers
    /// `doc`), and the router with the cookie layer.
    async fn cimd_app(
        doc: Result<ClientDocument, CimdError>,
    ) -> (SqlitePool, Arc<FixedDocuments>, Router) {
        let db = test_db::migrated().await;
        let docs = FixedDocuments::new(vec![(CLAUDE_CODE_ID, doc)]);
        let state = ConnectionState {
            documents: docs.clone(),
            ..ConnectionState::for_test(db.clone())
        };
        (db, docs, router(state).layer(CookieManagerLayer::new()))
    }

    fn claude_code_query(redirect: &'static str) -> String {
        query_with(&[
            ("client_id", Some(CLAUDE_CODE_ID)),
            ("redirect_uri", Some(redirect)),
        ])
    }

    #[tokio::test]
    async fn a_metadata_document_app_is_shown_by_its_url_host() {
        let (db, docs, app) = cimd_app(Ok(claude_code())).await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let q = claude_code_query("http://localhost:3118/callback");
        let (status, _, page) =
            get(app.clone(), &format!("/oauth/authorize?{q}"), Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        for text in [
            "Connect claude.ai to Soulful",
            "It calls itself \"Claude Code\".",
            "This app runs on a computer (it returns to localhost). Allow it only if you started it yourself.",
        ] {
            assert!(page.contains(text), "{text}");
        }
        assert_eq!(count(&db, "oauth_clients").await, 0);
        let (status, headers, _) = post_form(
            app,
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &request_id_of(&page)), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(headers["location"]
            .to_str()
            .unwrap()
            .starts_with("http://localhost:3118/callback?code="));
        let row: (String, Option<String>, String) = sqlx::query_as(
            "SELECT kind, client_name, redirect_uris FROM oauth_clients WHERE client_id = ?",
        )
        .bind(CLAUDE_CODE_ID)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "cimd".into(),
                Some("Claude Code".into()),
                r#"["http://localhost/callback","http://127.0.0.1/callback"]"#.into()
            )
        );
        assert_eq!(docs.calls(), 1);
    }

    #[tokio::test]
    async fn a_second_authorization_updates_the_stored_document() {
        let db = test_db::migrated().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let renamed = ClientDocument {
            client_name: "Claude Code 2".into(),
            ..claude_code()
        };
        for doc in [claude_code(), renamed] {
            let docs = FixedDocuments::new(vec![(CLAUDE_CODE_ID, Ok(doc))]);
            let app = router(ConnectionState {
                documents: docs,
                ..ConnectionState::for_test(db.clone())
            })
            .layer(CookieManagerLayer::new());
            let q = claude_code_query("http://localhost:3118/callback");
            let (_, _, page) =
                get(app.clone(), &format!("/oauth/authorize?{q}"), Some(&cookie)).await;
            let (status, _, _) = post_form(
                app,
                "/oauth/authorize",
                Some(&cookie),
                &[("request", &request_id_of(&page)), ("decision", "allow")],
            )
            .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
        }
        let names: Vec<Option<String>> =
            sqlx::query_scalar("SELECT client_name FROM oauth_clients")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(names, [Some("Claude Code 2".to_string())]);
        assert_eq!(count(&db, "oauth_codes").await, 2);
    }

    #[tokio::test]
    async fn the_document_is_fetched_only_after_sign_in() {
        let (_, docs, app) = cimd_app(Ok(claude_code())).await;
        let (status, headers, _) = get(
            app,
            &format!(
                "/oauth/authorize?{}",
                claude_code_query("http://localhost:3118/callback")
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(headers["location"]
            .to_str()
            .unwrap()
            .starts_with("/login?return_to="));
        assert_eq!(docs.calls(), 0);
    }

    #[tokio::test]
    async fn an_unusable_document_is_named_on_the_page() {
        let (db, _, app) = cimd_app(Err(CimdError::Status(404))).await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (status, headers, page) = get(
            app,
            &format!(
                "/oauth/authorize?{}",
                claude_code_query("http://localhost:3118/callback")
            ),
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("location").is_none());
        assert!(page.contains("Soulful could not use this app&#39;s client metadata at https://claude.ai/oauth/claude-code-client-metadata: it answered HTTP 404"), "{page}");

        let (db, docs, app) = cimd_app(Ok(claude_code())).await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let (status, _, page) = get(
            app.clone(),
            &format!(
                "/oauth/authorize?{}",
                claude_code_query("https://evil.example/cb")
            ),
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("This app asked to return to an address it did not register."));
        let q = query_with(&[("client_id", Some("https://claude.ai/"))]);
        let (status, _, page) = get(app, &format!("/oauth/authorize?{q}"), Some(&cookie)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page
            .contains("client metadata at https://claude.ai/: a client id URL must have a path"));
        assert_eq!(docs.calls(), 1);
        assert_eq!(count(&db, "oauth_requests").await, 0);
    }

    // Limits gains fields in later tasks; a test sets the one limit it needs
    // by assignment, so it does not change when they come.
    #[allow(clippy::field_reassign_with_default)]
    #[tokio::test]
    async fn metadata_fetches_per_user_are_limited() {
        let db = test_db::migrated().await;
        let mut limits = Limits::default();
        limits.cimd_fetch = WindowLimiter::new(1, Duration::from_secs(60));
        let docs = FixedDocuments::new(vec![(CLAUDE_CODE_ID, Ok(claude_code()))]);
        let app = router(ConnectionState {
            documents: docs.clone(),
            limits: Arc::new(limits),
            ..ConnectionState::for_test(db.clone())
        })
        .layer(CookieManagerLayer::new());
        let alice = signed_in(&db, "u1", "alice").await;
        let bob = signed_in(&db, "u2", "bob").await;
        let path = format!(
            "/oauth/authorize?{}",
            claude_code_query("http://localhost:3118/callback")
        );
        let (first, _, _) = get(app.clone(), &path, Some(&alice)).await;
        let (second, headers, page) = get(app.clone(), &path, Some(&alice)).await;
        assert_eq!(
            (first, second),
            (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
        );
        let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((59..=60).contains(&wait), "{wait}");
        assert!(page.contains(&format!(
            "Too many app sign-ins in a minute. Try again in {wait} seconds."
        )));
        assert_eq!(docs.calls(), 1);
        let (other_user, _, _) = get(app, &path, Some(&bob)).await;
        assert_eq!(other_user, StatusCode::OK);
    }

    #[tokio::test]
    async fn pending_requests_per_user_are_capped() {
        let (db, app) = app().await;
        let cookie = signed_in(&db, "u1", "alice").await;
        let mut newest = String::new();
        for _ in 0..25 {
            let (status, _, page) = get(
                app.clone(),
                &format!("/oauth/authorize?{}", query()),
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            newest = request_id_of(&page);
        }
        assert_eq!(count(&db, "oauth_requests").await, 20);
        let (status, _, _) = post_form(
            app,
            "/oauth/authorize",
            Some(&cookie),
            &[("request", &newest), ("decision", "allow")],
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }
}
