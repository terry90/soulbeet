use crate::auth::AuthResponse;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::{
    auth,
    base_url::{request_base, DerivedBase},
    login_limiter::{LoginLimiter, LOGIN_LIMITER},
    models,
    server_fns::{rate_limited_error, refusal_error, server_error, unauthorized_error},
    AuthSession,
};

#[cfg(feature = "server")]
use axum::http::HeaderMap;

#[cfg(feature = "server")]
use tower_cookies::{
    cookie::{time, SameSite},
    Cookie, Cookies,
};

pub const AUTH_COOKIE_NAME: &str = "auth_token";

/// Helper to configure the auth cookie consistently
#[cfg(feature = "server")]
fn build_auth_cookie(token: String, secure: bool) -> Cookie<'static> {
    use crate::auth::EXPIRATION_DAYS;

    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_expires(time::OffsetDateTime::now_utc() + time::Duration::days(EXPIRATION_DAYS));
    cookie
}

/// Whether the session cookie gets the Secure flag: the request came in over
/// https, unless the browser's `Origin` says it is on plain http and would drop
/// a Secure cookie.
#[cfg(feature = "server")]
pub(crate) fn secure_for(derived: &DerivedBase, origin: Option<&str>) -> bool {
    let browser_on_http = origin.is_some_and(|o| {
        o.split_once("://")
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http"))
    });
    derived.base.is_https() && !browser_on_http
}

/// Sets the session cookie for the user's current credentials version.
#[cfg(feature = "server")]
pub(crate) fn issue_session_cookie(
    cookies: &Cookies,
    headers: &HeaderMap,
    user: &models::user::User,
) -> Result<(), ServerFnError> {
    let derived = request_base(headers)
        .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let token = auth::create_token(&user.id, user.credentials_version).map_err(server_error)?;
    cookies.add(build_auth_cookie(token, secure_for(&derived, origin)));
    Ok(())
}

/// Reads the user's row as it stands now and sets a cookie for its current
/// credentials version.
#[cfg(feature = "server")]
pub(crate) async fn start_session(
    cookies: &Cookies,
    headers: &HeaderMap,
    user_id: &str,
) -> Result<models::user::User, ServerFnError> {
    let user = models::user::User::find_by_id(user_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("User not found"))?;
    issue_session_cookie(cookies, headers, &user)?;
    Ok(user)
}

#[post("/api/auth/register")]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    models::user::User::create(&username, &password)
        .await
        .map_err(server_error)
        .map(|_| ())
}

/// Categorized result of a Navidrome ping attempt.
#[cfg(feature = "server")]
enum NavidromeAuthResult {
    /// Credentials accepted by Navidrome.
    Success,
    /// Navidrome responded but credentials were wrong (Subsonic error code 40).
    AuthFailed,
    /// Navidrome could not be reached (network / timeout / circuit breaker).
    Unreachable,
    /// NAVIDROME_URL is unset or empty: this instance runs without Navidrome.
    NotConfigured,
}

/// Try to authenticate against Navidrome with the given credentials.
///
/// Builds a temporary NavidromeClient and calls `ping()`. The error type from
/// soulbeet lets us distinguish auth rejection (Subsonic error codes) from
/// network failures (status 408, 503, or request-level errors).
#[cfg(feature = "server")]
async fn try_navidrome_auth(username: &str, password: &str) -> NavidromeAuthResult {
    use soulbeet::NavidromeClientBuilder;

    let url = match std::env::var("NAVIDROME_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => return NavidromeAuthResult::NotConfigured,
    };

    let client = match NavidromeClientBuilder::new()
        .base_url(&url)
        .username(username)
        .password(password)
        .build()
    {
        Ok(c) => c,
        Err(_) => return NavidromeAuthResult::Unreachable,
    };

    match client.ping().await {
        Ok(()) => NavidromeAuthResult::Success,
        Err(e) => {
            let err_str = e.to_string();
            // Network-level failures include timeouts (408), service unavailable
            // (503), and connection errors. Subsonic auth failures produce API
            // error code 40 ("Wrong username or password") with a lower status.
            if err_str.contains("Navidrome request failed")
                || err_str.contains("Circuit breaker open")
            {
                tracing::warn!("Navidrome unreachable during login: {}", err_str);
                NavidromeAuthResult::Unreachable
            } else {
                tracing::info!("Navidrome auth failed for {}: {}", username, err_str);
                NavidromeAuthResult::AuthFailed
            }
        }
    }
}

/// Who the credentials belong to, once the limiter has let the attempt through.
#[cfg(feature = "server")]
enum Credentials {
    /// Navidrome accepted them.
    Navidrome,
    /// The local password matched; `status` is what Navidrome's answer means.
    Local {
        user: models::user::User,
        status: shared::system::NavidromeStatus,
    },
}

/// Decides a sign-in. The limiter is checked before Navidrome is asked, every
/// failed local verification is recorded, and a success clears the account.
#[cfg(feature = "server")]
async fn authenticate_in(
    db: &sqlx::SqlitePool,
    limiter: &LoginLimiter,
    username: &str,
    password: &str,
    now: std::time::Instant,
    navidrome: impl std::future::Future<Output = NavidromeAuthResult>,
) -> Result<Credentials, ServerFnError> {
    use shared::system::NavidromeStatus;

    limiter.check(username, now).map_err(rate_limited_error)?;

    let rejected = || {
        limiter.record_failure(username, now);
        unauthorized_error("Invalid username or password")
    };

    let result = navidrome.await;
    if matches!(result, NavidromeAuthResult::Success) {
        limiter.record_success(username);
        return Ok(Credentials::Navidrome);
    }

    // Navidrome rejected the credentials, is down, or is not set up: fall back
    // to the local password.
    let user = models::user::User::verify_in(db, username, password)
        .await
        .map_err(|_| rejected())?;
    limiter.record_success(username);
    // Without Navidrome the status stays Unknown ("not configured") so the UI
    // does not report a server that is down.
    let status = match result {
        NavidromeAuthResult::AuthFailed => NavidromeStatus::InvalidCredentials,
        NavidromeAuthResult::Unreachable => NavidromeStatus::Offline,
        NavidromeAuthResult::NotConfigured | NavidromeAuthResult::Success => {
            NavidromeStatus::Unknown
        }
    };
    Ok(Credentials::Local { user, status })
}

#[post("/api/auth/login", cookies: Cookies, headers: HeaderMap)]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    use crate::crypto;
    use crate::db::DB;
    use crate::services::evict_navidrome_client;
    use models::user::User;
    use shared::system::NavidromeStatus;
    use std::time::Instant;

    let credentials = authenticate_in(
        &DB,
        &LOGIN_LIMITER,
        &username,
        &password,
        Instant::now(),
        try_navidrome_auth(&username, &password),
    )
    .await?;

    match credentials {
        Credentials::Navidrome => {
            // Look up or create user
            let user = match User::get_by_username(&username)
                .await
                .map_err(server_error)?
            {
                Some(u) => u,
                None => User::create(&username, &password)
                    .await
                    .map_err(server_error)?,
            };

            // Encrypt the Navidrome password and store it
            let encrypted = crypto::encrypt(&password).map_err(server_error)?;
            User::update_navidrome_token(
                &user.id,
                Some(&encrypted),
                NavidromeStatus::Connected.as_str(),
            )
            .await
            .map_err(server_error)?;

            // Keep local password hash current
            User::update_password(&user.id, &password)
                .await
                .map_err(server_error)?;

            // Reset the dismissed banner so it can reappear if credentials break again
            let _ = models::user_settings::UserSettings::reset_navidrome_banner(&user.id).await;

            // Evict any cached client so it picks up the new token
            evict_navidrome_client(&user.id).await;

            let user = start_session(&cookies, &headers, &user.id).await?;

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: NavidromeStatus::Connected,
                role: user.role,
            })
        }
        Credentials::Local { user, status } => {
            // Keep the existing token so it works again once Navidrome is back
            User::update_navidrome_token(
                &user.id,
                user.navidrome_token.as_deref(),
                status.as_str(),
            )
            .await
            .map_err(server_error)?;

            if status == NavidromeStatus::InvalidCredentials {
                // Evict cached client with stale credentials
                evict_navidrome_client(&user.id).await;
            }

            let user = start_session(&cookies, &headers, &user.id).await?;

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: status,
                role: user.role,
            })
        }
    }
}

#[post("/api/auth/refresh", auth: AuthSession, cookies: Cookies, headers: HeaderMap)]
pub async fn refresh_token() -> Result<(), ServerFnError> {
    start_session(&cookies, &headers, &auth.0.user_id)
        .await
        .map(|_| ())
}

#[post("/api/auth/logout", cookies: Cookies)]
pub async fn logout() -> Result<(), ServerFnError> {
    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, "");
    cookie.set_path("/");

    cookies.remove(cookie);

    Ok(())
}

#[get("/api/auth/me", auth: AuthSession)]
pub async fn get_current_user() -> Result<Option<AuthResponse>, ServerFnError> {
    let user = auth.0;

    let status = models::user::User::find_by_id(&user.user_id)
        .await
        .ok()
        .flatten()
        .map(|u| shared::system::NavidromeStatus::from(u.navidrome_status))
        .unwrap_or_default();

    Ok(Some(AuthResponse {
        username: user.username,
        user_id: user.user_id,
        navidrome_status: status,
        role: user.role,
    }))
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::base_url::{BaseUrl, HostSource, SchemeSource};

    fn derived(base: &str) -> DerivedBase {
        DerivedBase {
            base: BaseUrl::parse(base).unwrap(),
            host_from: HostSource::Host,
            scheme_from: SchemeSource::HostKind,
        }
    }

    #[test]
    fn cookie_is_secure_for_an_https_base() {
        assert_eq!(build_auth_cookie("t".into(), true).secure(), Some(true));
        assert!(secure_for(
            &derived("https://music.example.com"),
            Some("https://music.example.com")
        ));
        assert!(secure_for(&derived("https://music.example.com"), None));
    }

    #[test]
    fn cookie_is_not_secure_for_an_http_base() {
        assert!(!secure_for(&derived("http://192.168.1.5:9765"), None));
    }

    #[test]
    fn cookie_is_not_secure_when_the_browser_origin_is_http() {
        // http://nas:9765 typed in the browser: base derives https, Origin says http
        assert!(!secure_for(
            &derived("https://nas:9765"),
            Some("http://nas:9765")
        ));
    }

    use crate::db::test_db;
    use crate::login_limiter::{LoginBlock, FREE_FAILURES};
    use crate::models::user::User;
    use crate::server_fns::RATE_LIMITED_CODE;
    use std::time::Instant;

    fn code(e: &ServerFnError) -> u16 {
        match e {
            ServerFnError::ServerError { code, .. } => *code,
            other => panic!("unexpected error {other:?}"),
        }
    }

    async fn sign_in(
        db: &sqlx::SqlitePool,
        limiter: &LoginLimiter,
        name: &str,
        password: &str,
        now: Instant,
        result: NavidromeAuthResult,
    ) -> Result<Credentials, ServerFnError> {
        authenticate_in(db, limiter, name, password, now, async { result }).await
    }

    #[tokio::test]
    async fn not_configured_with_a_wrong_password_records_a_failure() {
        let db = test_db::migrated().await;
        User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            let err = sign_in(
                &db,
                &limiter,
                "ann",
                "wrong",
                now,
                NavidromeAuthResult::NotConfigured,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(code(&err), 401);
        }
        assert!(matches!(
            limiter.check("ann", now),
            Err(LoginBlock::Account { .. })
        ));
    }

    #[tokio::test]
    async fn an_unknown_user_records_a_failure() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            let err = sign_in(
                &db,
                &limiter,
                "ghost",
                "pw",
                now,
                NavidromeAuthResult::AuthFailed,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(code(&err), 401);
        }
        assert!(matches!(
            limiter.check("ghost", now),
            Err(LoginBlock::Account { .. })
        ));
    }

    #[tokio::test]
    async fn a_blocked_account_is_refused_before_navidrome_is_asked() {
        let db = test_db::migrated().await;
        User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            limiter.record_failure("ann", now);
        }
        let asked = std::cell::Cell::new(false);
        let err = authenticate_in(&db, &limiter, "ann", "right", now, async {
            asked.set(true);
            NavidromeAuthResult::NotConfigured
        })
        .await
        .err()
        .unwrap();
        assert_eq!(code(&err), RATE_LIMITED_CODE);
        assert!(err.to_string().contains("for this account"));
        assert!(!asked.get());
    }

    #[tokio::test]
    async fn a_success_clears_the_failure_count() {
        let db = test_db::migrated().await;
        User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES {
            limiter.record_failure("ann", now);
        }
        let ok = sign_in(
            &db,
            &limiter,
            "ann",
            "right",
            now,
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap();
        assert!(matches!(
            ok,
            Credentials::Local {
                status: shared::system::NavidromeStatus::Unknown,
                ..
            }
        ));
        limiter.record_failure("ann", now);
        assert_eq!(limiter.check("ann", now), Ok(()));
    }

    #[tokio::test]
    async fn a_navidrome_success_clears_the_failure_count() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES {
            limiter.record_failure("ann", now);
        }
        let ok = sign_in(
            &db,
            &limiter,
            "ann",
            "pw",
            now,
            NavidromeAuthResult::Success,
        )
        .await
        .unwrap();
        assert!(matches!(ok, Credentials::Navidrome));
        limiter.record_failure("ann", now);
        assert_eq!(limiter.check("ann", now), Ok(()));
    }
}
