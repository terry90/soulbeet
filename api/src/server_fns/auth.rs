use crate::auth::AuthResponse;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::{
    auth::{self, RegistrationMode},
    base_url::{request_base, DerivedBase},
    config::CONFIG,
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

/// How this request's session cookie is set. Read from the headers before
/// anything is written, so a request whose address cannot be told changes
/// nothing.
#[cfg(feature = "server")]
pub(crate) struct SessionCookie {
    secure: bool,
}

#[cfg(feature = "server")]
impl SessionCookie {
    pub(crate) fn for_request(headers: &HeaderMap) -> Result<SessionCookie, ServerFnError> {
        let derived = request_base(headers)
            .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        Ok(SessionCookie {
            secure: secure_for(&derived, origin),
        })
    }
}

/// Sets the session cookie for the user's current credentials version.
#[cfg(feature = "server")]
pub(crate) fn issue_session_cookie(
    cookies: &Cookies,
    session: &SessionCookie,
    user: &models::user::User,
) -> Result<(), ServerFnError> {
    let token = auth::create_token(&user.id, user.credentials_version).map_err(server_error)?;
    cookies.add(build_auth_cookie(token, session.secure));
    Ok(())
}

/// Reads the user's row as it stands now and sets a cookie for its current
/// credentials version.
#[cfg(feature = "server")]
pub(crate) async fn start_session(
    cookies: &Cookies,
    session: &SessionCookie,
    user_id: &str,
) -> Result<models::user::User, ServerFnError> {
    let user = models::user::User::find_by_id(user_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("User not found"))?;
    issue_session_cookie(cookies, session, &user)?;
    Ok(user)
}

/// Why strangers cannot register in this mode; `None` only for `Open`.
#[cfg(feature = "server")]
pub(crate) fn registration_refusal(mode: RegistrationMode) -> Option<&'static str> {
    match mode {
        RegistrationMode::Open => None,
        RegistrationMode::Admin => {
            Some("Registration is closed. An admin creates accounts in Settings > Users.")
        }
        RegistrationMode::Navidrome => {
            Some("Accounts are created by signing in with your Navidrome account.")
        }
    }
}

#[post("/api/auth/register")]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    if let Some(msg) = registration_refusal(CONFIG.registration()) {
        return Err(refusal_error(msg));
    }
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

    let Some(url) = CONFIG.navidrome_url() else {
        return NavidromeAuthResult::NotConfigured;
    };

    let client = match NavidromeClientBuilder::new()
        .base_url(url)
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

/// Decides a sign-in. The limiter admits the attempt before Navidrome is
/// asked, a failed local verification is recorded, and a success clears the
/// account. A database error records nothing. The clock is read when each
/// decision is made, so a slow Navidrome call does not backdate a recorded
/// failure.
#[cfg(feature = "server")]
async fn authenticate_in(
    db: &sqlx::SqlitePool,
    limiter: &LoginLimiter,
    username: &str,
    password: &str,
    clock: impl Fn() -> std::time::Instant,
    navidrome: impl std::future::Future<Output = NavidromeAuthResult>,
) -> Result<Credentials, ServerFnError> {
    use shared::system::NavidromeStatus;

    let attempt = limiter
        .begin(username, clock())
        .map_err(rate_limited_error)?;

    let result = navidrome.await;
    if matches!(result, NavidromeAuthResult::Success) {
        attempt.succeeded();
        return Ok(Credentials::Navidrome);
    }

    // Navidrome rejected the credentials, is down, or is not set up: fall back
    // to the local password.
    let Some(user) = models::user::User::verify_in(db, username, password)
        .await
        .map_err(server_error)?
    else {
        attempt.failed(clock());
        return Err(unauthorized_error("Invalid username or password"));
    };
    attempt.succeeded();
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

/// The Soulful row that has the name Navidrome just verified.
#[cfg(feature = "server")]
pub(crate) struct ExistingAccount {
    /// The row holds a Navidrome token.
    pub linked: bool,
    /// The submitted password also verifies against the row's own hash.
    pub local_password_ok: bool,
}

/// What a Navidrome-verified login does to the Soulful account of that name.
#[cfg(feature = "server")]
#[derive(Debug, PartialEq)]
pub(crate) enum LoginPlan {
    Create,
    Link,
    Refresh {
        password_changed: bool,
    },
    /// The row is not linked and its password differs: it may not be the
    /// Navidrome user's, so it is not taken over.
    RefuseUnlinked,
    RefuseNoAccount,
}

#[cfg(feature = "server")]
pub(crate) fn navidrome_login_plan(
    existing: Option<ExistingAccount>,
    mode: RegistrationMode,
) -> LoginPlan {
    match existing {
        None => match mode {
            RegistrationMode::Navidrome | RegistrationMode::Open => LoginPlan::Create,
            RegistrationMode::Admin => LoginPlan::RefuseNoAccount,
        },
        Some(ExistingAccount {
            linked: true,
            local_password_ok,
        }) => LoginPlan::Refresh {
            password_changed: !local_password_ok,
        },
        Some(ExistingAccount {
            linked: false,
            local_password_ok: true,
        }) => LoginPlan::Link,
        Some(ExistingAccount {
            linked: false,
            local_password_ok: false,
        }) => LoginPlan::RefuseUnlinked,
    }
}

#[cfg(feature = "server")]
#[derive(Debug)] // User has no PartialEq; tests match on the variant
pub(crate) enum LoginOutcome {
    SignedIn(models::user::User),
    Refused(String),
}

/// The Success branch's database work: plans from `existing`, writes, and
/// returns the row read back after the writes. No cookie, no cache eviction.
#[cfg(feature = "server")]
pub(crate) async fn apply_navidrome_login(
    db: &sqlx::SqlitePool,
    existing: Option<models::user::User>,
    username: &str,
    password: &str,
    encrypted_password: &str,
    mode: RegistrationMode,
) -> Result<LoginOutcome, String> {
    use crate::security;
    use models::user::User;
    use shared::system::NavidromeStatus;

    let connected = NavidromeStatus::Connected.as_str();
    let account = existing.as_ref().map(|u| ExistingAccount {
        linked: u.navidrome_token.is_some(),
        local_password_ok: u.password_matches(password),
    });
    let id = match (navidrome_login_plan(account, mode), existing) {
        (LoginPlan::RefuseUnlinked, _) => {
            return Ok(LoginOutcome::Refused(format!(
                "A Soulful account named '{username}' exists but is not linked to Navidrome. \
                 Sign in with its Soulful password, change that password to your Navidrome \
                 password in Settings > Users, then sign in again. An admin can also set it \
                 for you."
            )));
        }
        (LoginPlan::RefuseNoAccount, _) => {
            return Ok(LoginOutcome::Refused(format!(
                "There is no Soulful account named '{username}'. An admin creates accounts in \
                 Settings > Users."
            )));
        }
        (LoginPlan::Create, None) => {
            let user = User::create_in(db, username, password).await?;
            User::update_navidrome_token_in(db, &user.id, Some(encrypted_password), connected)
                .await?;
            user.id
        }
        (LoginPlan::Link, Some(user)) => {
            User::link_navidrome_in(db, &user.id, encrypted_password, connected).await?;
            user.id
        }
        (LoginPlan::Refresh { password_changed }, Some(user)) => {
            User::update_navidrome_token_in(db, &user.id, Some(encrypted_password), connected)
                .await?;
            if password_changed {
                User::update_password_in(db, &user.id, password).await?;
                security::note_password_set(&user.id, password);
            }
            user.id
        }
        (LoginPlan::Create, Some(_)) | (LoginPlan::Link | LoginPlan::Refresh { .. }, None) => {
            unreachable!("the plan follows whether a row exists")
        }
    };
    User::find_by_id_in(db, &id)
        .await?
        .map(LoginOutcome::SignedIn)
        .ok_or_else(|| "User not found".to_string())
}

#[post("/api/auth/login", cookies: Cookies, headers: HeaderMap)]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    use crate::crypto;
    use crate::db::DB;
    use crate::services::evict_navidrome_client;
    use models::user::User;
    use shared::system::NavidromeStatus;
    use std::time::Instant;

    let session = SessionCookie::for_request(&headers)?;
    let rename_allowed = CONFIG.navidrome_url().is_none();

    let credentials = authenticate_in(
        &DB,
        &LOGIN_LIMITER,
        &username,
        &password,
        Instant::now,
        try_navidrome_auth(&username, &password),
    )
    .await?;

    match credentials {
        Credentials::Navidrome => {
            let encrypted = crypto::encrypt(&password).map_err(server_error)?;
            let existing = User::get_by_username(&username)
                .await
                .map_err(server_error)?;
            let user = match apply_navidrome_login(
                &DB,
                existing,
                &username,
                &password,
                &encrypted,
                CONFIG.registration(),
            )
            .await
            .map_err(server_error)?
            {
                LoginOutcome::SignedIn(user) => user,
                LoginOutcome::Refused(msg) => return Err(refusal_error(msg)),
            };

            // Reset the dismissed banner so it can reappear if credentials break again
            let _ = models::user_settings::UserSettings::reset_navidrome_banner(&user.id).await;

            // Evict any cached client so it picks up the new token
            evict_navidrome_client(&user.id).await;

            issue_session_cookie(&cookies, &session, &user)?;

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: NavidromeStatus::Connected,
                role: user.role,
                rename_allowed,
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

            let user = start_session(&cookies, &session, &user.id).await?;

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: status,
                role: user.role,
                rename_allowed,
            })
        }
    }
}

#[post("/api/auth/refresh", auth: AuthSession, cookies: Cookies, headers: HeaderMap)]
pub async fn refresh_token() -> Result<(), ServerFnError> {
    let session = SessionCookie::for_request(&headers)?;
    start_session(&cookies, &session, &auth.0.user_id)
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
        rename_allowed: CONFIG.navidrome_url().is_none(),
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
        authenticate_in(db, limiter, name, password, || now, async { result }).await
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
            limiter.begin("ann", now).err(),
            Some(LoginBlock::Account { .. })
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
            limiter.begin("ghost", now).err(),
            Some(LoginBlock::Account { .. })
        ));
    }

    #[tokio::test]
    async fn a_blocked_account_is_refused_before_navidrome_is_asked() {
        let db = test_db::migrated().await;
        User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            limiter.begin("ann", now).unwrap().failed(now);
        }
        let asked = std::cell::Cell::new(false);
        let err = authenticate_in(&db, &limiter, "ann", "right", || now, async {
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
            limiter.begin("ann", now).unwrap().failed(now);
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
        limiter.begin("ann", now).unwrap().failed(now);
        assert!(limiter.begin("ann", now).is_ok());
    }

    #[tokio::test]
    async fn a_navidrome_success_clears_the_failure_count() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES {
            limiter.begin("ann", now).unwrap().failed(now);
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
        limiter.begin("ann", now).unwrap().failed(now);
        assert!(limiter.begin("ann", now).is_ok());
    }

    #[tokio::test]
    async fn a_failure_is_stamped_after_navidrome_answers() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let start = Instant::now();
        let later = start + std::time::Duration::from_secs(30);
        for _ in 0..FREE_FAILURES {
            limiter.begin("ghost", start).unwrap().failed(start);
        }
        let calls = std::cell::Cell::new(0);
        let clock = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                start
            } else {
                later
            }
        };
        let err = authenticate_in(&db, &limiter, "ghost", "pw", clock, async {
            NavidromeAuthResult::AuthFailed
        })
        .await
        .err()
        .unwrap();
        assert_eq!(code(&err), 401);
        // The failure sits at `later`, so the wait runs from there.
        assert!(matches!(
            limiter.begin("ghost", later).err(),
            Some(LoginBlock::Account { .. })
        ));
    }

    #[tokio::test]
    async fn a_database_error_is_a_server_error_and_records_nothing() {
        let db = test_db::migrated().await;
        db.close().await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            let err = sign_in(
                &db,
                &limiter,
                "ann",
                "pw",
                now,
                NavidromeAuthResult::NotConfigured,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(code(&err), 500);
        }
        assert!(limiter.begin("ann", now).is_ok());
    }

    fn account(linked: bool, local_password_ok: bool) -> Option<ExistingAccount> {
        Some(ExistingAccount {
            linked,
            local_password_ok,
        })
    }

    #[test]
    fn a_prehijacked_name_is_refused() {
        // attacker registered "bob" with their own password; Bob's Navidrome password does not verify locally
        let plan = navidrome_login_plan(account(false, false), RegistrationMode::Open);
        assert_eq!(plan, LoginPlan::RefuseUnlinked);
    }

    #[test]
    fn an_unlinked_row_with_the_same_password_is_linked() {
        assert_eq!(
            navidrome_login_plan(account(false, true), RegistrationMode::Navidrome),
            LoginPlan::Link
        );
    }

    #[test]
    fn a_linked_row_is_refreshed_and_notes_a_password_change() {
        assert_eq!(
            navidrome_login_plan(account(true, false), RegistrationMode::Admin),
            LoginPlan::Refresh {
                password_changed: true
            }
        );
        assert_eq!(
            navidrome_login_plan(account(true, true), RegistrationMode::Admin),
            LoginPlan::Refresh {
                password_changed: false
            }
        );
    }

    #[test]
    fn new_names_follow_the_registration_mode() {
        assert_eq!(
            navidrome_login_plan(None, RegistrationMode::Navidrome),
            LoginPlan::Create
        );
        assert_eq!(
            navidrome_login_plan(None, RegistrationMode::Open),
            LoginPlan::Create
        );
        assert_eq!(
            navidrome_login_plan(None, RegistrationMode::Admin),
            LoginPlan::RefuseNoAccount
        );
    }

    #[test]
    fn only_open_mode_allows_register() {
        assert!(registration_refusal(RegistrationMode::Open).is_none());
        assert!(registration_refusal(RegistrationMode::Admin).is_some());
        assert!(registration_refusal(RegistrationMode::Navidrome).is_some());
    }

    async fn row(db: &sqlx::SqlitePool, name: &str) -> Option<User> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(name)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    /// Bob's row, linked to Navidrome (version 1) with local password `password`.
    async fn linked_bob(db: &sqlx::SqlitePool, password: &str) -> User {
        let bob = User::create_in(db, "bob", password).await.unwrap();
        User::link_navidrome_in(db, &bob.id, "enc-old", "connected")
            .await
            .unwrap();
        row(db, "bob").await.unwrap()
    }

    fn signed_in(outcome: LoginOutcome) -> User {
        match outcome {
            LoginOutcome::SignedIn(user) => user,
            other => panic!("expected a sign-in, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_prehijacked_row_is_refused_and_left_alone() {
        let db = test_db::migrated().await;
        let bob = User::create_in(&db, "bob", "attacker-pw").await.unwrap();
        let outcome = apply_navidrome_login(
            &db,
            Some(bob),
            "bob",
            "navidrome-pw",
            "enc",
            RegistrationMode::Open,
        )
        .await
        .unwrap();
        match outcome {
            LoginOutcome::Refused(msg) => assert!(
                msg.starts_with("A Soulful account named 'bob' exists but is not linked"),
                "{msg}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let bob = row(&db, "bob").await.unwrap();
        assert_eq!(bob.navidrome_token, None);
        assert_eq!(bob.credentials_version, 0);
        assert!(bob.password_matches("attacker-pw"));
    }

    #[tokio::test]
    async fn an_unlinked_row_with_the_same_password_is_linked_with_one_bump() {
        let db = test_db::migrated().await;
        let bob = User::create_in(&db, "bob", "pw").await.unwrap();
        let u = signed_in(
            apply_navidrome_login(
                &db,
                Some(bob),
                "bob",
                "pw",
                "enc",
                RegistrationMode::Navidrome,
            )
            .await
            .unwrap(),
        );
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(u.credentials_version, 1);
    }

    #[tokio::test]
    async fn a_linked_row_with_a_new_password_gets_one_bump_and_a_new_hash() {
        let db = test_db::migrated().await;
        let bob = linked_bob(&db, "old").await;
        let u = signed_in(
            apply_navidrome_login(
                &db,
                Some(bob),
                "bob",
                "new",
                "enc",
                RegistrationMode::Navidrome,
            )
            .await
            .unwrap(),
        );
        assert_eq!(u.credentials_version, 2);
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
        assert!(u.password_matches("new"));
        assert!(!u.password_matches("old"));
    }

    #[tokio::test]
    async fn an_unchanged_password_does_not_bump() {
        let db = test_db::migrated().await;
        let bob = linked_bob(&db, "pw").await;
        let u = signed_in(
            apply_navidrome_login(
                &db,
                Some(bob),
                "bob",
                "pw",
                "enc",
                RegistrationMode::Navidrome,
            )
            .await
            .unwrap(),
        );
        assert_eq!(u.credentials_version, 1);
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
    }

    #[tokio::test]
    async fn a_new_name_in_admin_mode_creates_nothing() {
        let db = test_db::migrated().await;
        let outcome =
            apply_navidrome_login(&db, None, "carol", "pw", "enc", RegistrationMode::Admin)
                .await
                .unwrap();
        assert!(matches!(outcome, LoginOutcome::Refused(_)), "{outcome:?}");
        assert!(row(&db, "carol").await.is_none());
    }

    #[tokio::test]
    async fn a_new_name_in_navidrome_mode_is_created_linked() {
        let db = test_db::migrated().await;
        let u = signed_in(
            apply_navidrome_login(&db, None, "carol", "pw", "enc", RegistrationMode::Navidrome)
                .await
                .unwrap(),
        );
        assert_eq!(u.username, "carol");
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(u.navidrome_status, "connected");
        assert_eq!(u.role, crate::auth::Role::User);
        assert!(u.password_matches("pw"));
    }
}
