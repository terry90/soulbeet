use crate::auth::{AuthResponse, LoginOptions};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::{
    auth::{self, LinkProof, RegistrationMode, NO_PASSWORD_TO_LINK},
    base_url::{request_base_with, BaseUrl, BaseUrlError},
    config::CONFIG,
    login_limiter::{LoginLimiter, LOGIN_LIMITER},
    models,
    server_fns::{rate_limited_error, refusal_error, server_error, unauthorized_error},
    AuthSession,
};

#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};

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
pub(crate) fn secure_for(base: &BaseUrl, origin: Option<&str>) -> bool {
    let browser_on_http = origin.is_some_and(|o| {
        o.split_once("://")
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http"))
    });
    base.is_https() && !browser_on_http
}

/// How this request's session cookie is set. Read from the headers before
/// anything is written, so a request whose address cannot be told changes
/// nothing.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionCookie {
    secure: bool,
}

#[cfg(feature = "server")]
impl SessionCookie {
    /// Whether cookies set on the answer get the Secure flag.
    pub(crate) fn secure(&self) -> bool {
        self.secure
    }

    pub(crate) fn for_request(
        headers: &HeaderMap,
        uri: &Uri,
    ) -> Result<SessionCookie, ServerFnError> {
        Self::for_request_with(headers, uri, CONFIG.public_url())
    }

    /// `for_request` with `public_url`, the parsed `PUBLIC_URL`.
    pub(crate) fn for_request_with(
        headers: &HeaderMap,
        uri: &Uri,
        public_url: Option<&Result<BaseUrl, BaseUrlError>>,
    ) -> Result<SessionCookie, ServerFnError> {
        let base = request_base_with(headers, uri, public_url)
            .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        Ok(SessionCookie {
            secure: secure_for(&base, origin),
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
    let token = auth::create_token(
        crate::secret_key::current(),
        &user.id,
        user.credentials_version,
    )
    .map_err(server_error)?;
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

/// What the login page offers besides the password form. Public: it is read
/// before anyone signs in.
#[get("/api/auth/options")]
pub async fn get_login_options() -> Result<LoginOptions, ServerFnError> {
    Ok(LoginOptions {
        sso_provider: CONFIG.oidc().map(|oidc| oidc.provider_name.clone()),
    })
}

#[post("/api/auth/register")]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    if let Some(msg) = registration_refusal(CONFIG.registration()) {
        return Err(refusal_error(msg));
    }
    super::user::check_username_length(&username)?;
    models::user::User::create(&username, &password)
        .await
        .map_err(super::user::user_change_error)
        .map(|_| ())
}

/// Categorized result of a Navidrome ping attempt.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum NavidromeAuthResult {
    /// Credentials accepted by Navidrome.
    Success,
    /// Navidrome refused the credentials (Subsonic error code 40).
    AuthFailed,
    /// Navidrome gave no verdict on the credentials: it could not be reached,
    /// a proxy in front of it answered with an HTTP error, its answer could
    /// not be read, or it sent a Subsonic error other than 40.
    Unreachable,
    /// NAVIDROME_URL is unset or empty: this instance runs without Navidrome.
    NotConfigured,
}

/// Try to authenticate against this server's Navidrome with the given
/// credentials.
#[cfg(feature = "server")]
async fn try_navidrome_auth(username: &str, password: &str) -> NavidromeAuthResult {
    match CONFIG.navidrome_url() {
        Some(url) => navidrome_auth_at(url, username, password).await,
        None => NavidromeAuthResult::NotConfigured,
    }
}

/// Try to authenticate against the Navidrome at `url` with the given
/// credentials, with a temporary client's `ping()`.
#[cfg(feature = "server")]
async fn navidrome_auth_at(url: &str, username: &str, password: &str) -> NavidromeAuthResult {
    use soulbeet::NavidromeClientBuilder;

    let ping = match NavidromeClientBuilder::new()
        .base_url(url)
        .username(username)
        .password(password)
        .build()
    {
        Ok(client) => client.ping().await,
        Err(e) => Err(e),
    };
    let result = ping_verdict(&ping);
    if let Err(e) = &ping {
        match result {
            // The name may come from the single sign-on provider.
            NavidromeAuthResult::AuthFailed => tracing::info!(
                "Navidrome refused the credentials for {}: {e}",
                crate::oidc::loggable(username)
            ),
            _ => tracing::warn!("Navidrome gave no verdict during login: {e}"),
        }
    }
    result
}

/// What a login ping's outcome says about the credentials. Only Navidrome's
/// own refusal counts against them. Navidrome sends no other code for a
/// login ping that would: 10 means a required parameter was missing, 0 an
/// internal error, and 41 (token authentication not supported) does not
/// exist in Navidrome, which always accepts tokens.
#[cfg(feature = "server")]
fn ping_verdict(ping: &Result<(), soulbeet::NavidromeError>) -> NavidromeAuthResult {
    match ping {
        Ok(()) => NavidromeAuthResult::Success,
        Err(e) if e.is_wrong_credentials() => NavidromeAuthResult::AuthFailed,
        Err(_) => NavidromeAuthResult::Unreachable,
    }
}

/// Why a name that matches several accounts cannot sign in.
#[cfg(feature = "server")]
fn several_accounts(username: &str) -> String {
    format!(
        "Several Soulbeet accounts match '{username}' when letter case is ignored, so none of \
         them can sign in. Ask an admin to delete the accounts that are not yours."
    )
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
///
/// For an account linked to Navidrome, Navidrome's refusal is final; its
/// local password counts only while Navidrome is unreachable or not set up.
#[cfg(feature = "server")]
async fn authenticate_in(
    db: &sqlx::SqlitePool,
    limiter: &LoginLimiter,
    username: &str,
    password: &str,
    clock: impl Fn() -> std::time::Instant,
    navidrome: impl std::future::Future<Output = NavidromeAuthResult>,
) -> Result<Credentials, ServerFnError> {
    use models::user::NameMatch;
    use shared::system::NavidromeStatus;

    // No account has a longer name, so nothing is counted for it: the limiter
    // never holds an attacker's oversized names.
    if !models::user::username_fits(username) {
        return Err(unauthorized_error("Invalid username or password"));
    }
    let attempt = limiter
        .begin(username, clock())
        .map_err(rate_limited_error)?;

    let result = navidrome.await;
    if matches!(result, NavidromeAuthResult::Success) {
        attempt.succeeded();
        return Ok(Credentials::Navidrome);
    }

    let user = match models::user::User::find_by_name_in(db, username)
        .await
        .map_err(server_error)?
    {
        // Not a wrong password, so nothing is counted.
        NameMatch::Several => return Err(refusal_error(several_accounts(username))),
        NameMatch::None => None,
        NameMatch::One(user) => Some(user),
    };
    let refused_by_navidrome = matches!(result, NavidromeAuthResult::AuthFailed);
    let Some(user) = user.filter(|u| {
        !(refused_by_navidrome && u.navidrome_token.is_some()) && u.password_matches(password)
    }) else {
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
/// The variants that act on an existing row carry it (`R`), so applying a
/// plan never looks the row up again.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LoginPlan<R> {
    Create,
    Link(R),
    Refresh {
        row: R,
        password_changed: bool,
    },
    /// The row is not linked and its password differs: it may not be the
    /// Navidrome user's, so it is not taken over.
    RefuseUnlinked,
    RefuseNoAccount,
}

#[cfg(feature = "server")]
pub(crate) fn navidrome_login_plan<R>(
    existing: Option<(R, ExistingAccount)>,
    mode: RegistrationMode,
) -> LoginPlan<R> {
    match existing {
        None => match mode {
            RegistrationMode::Navidrome | RegistrationMode::Open => LoginPlan::Create,
            RegistrationMode::Admin => LoginPlan::RefuseNoAccount,
        },
        Some((
            row,
            ExistingAccount {
                linked: true,
                local_password_ok,
            },
        )) => LoginPlan::Refresh {
            row,
            password_changed: !local_password_ok,
        },
        Some((
            row,
            ExistingAccount {
                linked: false,
                local_password_ok: true,
            },
        )) => LoginPlan::Link(row),
        Some((
            _,
            ExistingAccount {
                linked: false,
                local_password_ok: false,
            },
        )) => LoginPlan::RefuseUnlinked,
    }
}

#[cfg(feature = "server")]
#[derive(Debug)] // User has no PartialEq; tests match on the variant
pub(crate) enum LoginOutcome {
    SignedIn(models::user::User),
    Refused(String),
}

/// The Success branch's database work: finds the row of the verified name,
/// plans, writes, and returns the row read back after the writes. No cookie,
/// no cache eviction.
#[cfg(feature = "server")]
pub(crate) async fn apply_navidrome_login(
    db: &sqlx::SqlitePool,
    username: &str,
    password: &str,
    encrypted_password: &str,
    mode: RegistrationMode,
) -> Result<LoginOutcome, String> {
    use crate::security;
    use models::user::{NameMatch, User};
    use shared::system::NavidromeStatus;

    let existing = match User::find_by_name_in(db, username).await? {
        NameMatch::None => None,
        NameMatch::One(user) => Some(user),
        NameMatch::Several => return Ok(LoginOutcome::Refused(several_accounts(username))),
    };
    let connected = NavidromeStatus::Connected.as_str();
    let existing = existing.map(|u| {
        let account = ExistingAccount {
            linked: u.navidrome_token.is_some(),
            local_password_ok: u.password_matches(password),
        };
        (u, account)
    });
    let id = match navidrome_login_plan(existing, mode) {
        LoginPlan::RefuseUnlinked => {
            return Ok(LoginOutcome::Refused(format!(
                "A Soulbeet account named '{username}' exists but is not linked to Navidrome. \
                 If it is yours, sign in to it with its Soulbeet password or single sign-on, \
                 then use Connect Navidrome in Settings > Users. If it is not, or you cannot \
                 sign in to it, ask an admin."
            )));
        }
        LoginPlan::RefuseNoAccount => {
            return Ok(LoginOutcome::Refused(format!(
                "There is no Soulbeet account named '{username}'. An admin creates accounts in \
                 Settings > Users."
            )));
        }
        LoginPlan::Create => {
            let user = User::create_in(db, username, password)
                .await
                .map_err(|e| e.to_string())?;
            User::update_navidrome_token_in(db, &user.id, Some(encrypted_password), connected)
                .await?;
            user.id
        }
        LoginPlan::Link(user) => {
            User::link_navidrome_in(db, &user.id, encrypted_password, connected).await?;
            user.id
        }
        LoginPlan::Refresh {
            row: user,
            password_changed,
        } => {
            User::update_navidrome_token_in(db, &user.id, Some(encrypted_password), connected)
                .await?;
            if password_changed {
                User::update_password_in(db, &user.id, password).await?;
                security::note_password_set(&user.id, password);
            }
            user.id
        }
    };
    User::find_by_id_in(db, &id)
        .await?
        .map(LoginOutcome::SignedIn)
        .ok_or_else(|| "User not found".to_string())
}

#[post("/api/auth/login", cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    use crate::crypto;
    use crate::db::DB;
    use crate::services::evict_navidrome_client;
    use models::user::User;
    use shared::system::NavidromeStatus;
    use std::time::Instant;

    let session = SessionCookie::for_request(&headers, &uri)?;
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
            let encrypted =
                crypto::encrypt(crate::secret_key::current(), &password).map_err(server_error)?;
            let user = match apply_navidrome_login(
                &DB,
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

/// Connects the signed-in account to Navidrome: stores the Navidrome
/// password once Navidrome accepts it for the account's own username, as a
/// Navidrome login does, without signing anyone out. `navidrome` asks
/// Navidrome about the name it is given. Attempts are held back like
/// sign-ins: a wrong password counts as a failed sign-in of that name.
#[cfg(feature = "server")]
pub(crate) async fn connect_navidrome_in<F>(
    db: &sqlx::SqlitePool,
    limiter: &LoginLimiter,
    user: &auth::UserCtx,
    encrypted_password: &str,
    clock: impl Fn() -> std::time::Instant,
    navidrome: impl FnOnce(String) -> F,
) -> Result<(), ServerFnError>
where
    F: std::future::Future<Output = NavidromeAuthResult>,
{
    use shared::system::NavidromeStatus;

    let attempt = limiter
        .begin(&user.username, clock())
        .map_err(rate_limited_error)?;
    match navidrome(user.username.clone()).await {
        NavidromeAuthResult::Success => {
            attempt.succeeded();
            models::user::User::update_navidrome_token_in(
                db,
                &user.user_id,
                Some(encrypted_password),
                NavidromeStatus::Connected.as_str(),
            )
            .await
            .map_err(server_error)
        }
        NavidromeAuthResult::AuthFailed => {
            attempt.failed(clock());
            Err(refusal_error(format!(
                "Navidrome refused this password for '{}'.",
                user.username
            )))
        }
        NavidromeAuthResult::Unreachable => Err(refusal_error(
            "Navidrome did not answer, so nothing changed. Try again later.",
        )),
        NavidromeAuthResult::NotConfigured => {
            Err(refusal_error("This server does not use Navidrome."))
        }
    }
}

/// What proves `user` is the signed-in person's before they link it to
/// single sign-on, with `navidrome_configured` saying NAVIDROME_URL is set.
#[cfg(feature = "server")]
pub(crate) fn link_proof(user: &models::user::User, navidrome_configured: bool) -> LinkProof {
    if user.has_password() {
        LinkProof::Password
    } else if navidrome_configured && user.navidrome_token.is_some() {
        LinkProof::NavidromePassword
    } else {
        LinkProof::NoPassword
    }
}

/// Checks that `password` is the one the login form signs in to `user`
/// with, before a link hands the account to whoever the provider vouches
/// for: a session left open is not enough. Held back like sign-ins of that
/// name, and a wrong password counts as a failed one. For an account linked
/// to Navidrome, Navidrome's verdict decides while it gives one, as at the
/// login form. `navidrome` asks Navidrome about the name it is given.
#[cfg(feature = "server")]
pub(crate) async fn prove_password_in<F>(
    limiter: &LoginLimiter,
    user: &models::user::User,
    password: &str,
    navidrome_configured: bool,
    clock: impl Fn() -> std::time::Instant,
    navidrome: impl FnOnce(String) -> F,
) -> Result<(), ServerFnError>
where
    F: std::future::Future<Output = NavidromeAuthResult>,
{
    let proof = link_proof(user, navidrome_configured);
    if proof == LinkProof::NoPassword {
        return Err(refusal_error(NO_PASSWORD_TO_LINK));
    }
    let attempt = limiter
        .begin(&user.username, clock())
        .map_err(rate_limited_error)?;
    let verdict = if navidrome_configured && user.navidrome_token.is_some() {
        navidrome(user.username.clone()).await
    } else {
        NavidromeAuthResult::NotConfigured
    };
    let proven = match verdict {
        NavidromeAuthResult::Success => true,
        NavidromeAuthResult::AuthFailed => false,
        NavidromeAuthResult::Unreachable if proof == LinkProof::NavidromePassword => {
            return Err(refusal_error(
                "Navidrome did not answer, so nothing changed. Try again later.",
            ));
        }
        NavidromeAuthResult::Unreachable | NavidromeAuthResult::NotConfigured => {
            user.password_matches(password)
        }
    };
    if proven {
        attempt.succeeded();
        Ok(())
    } else {
        attempt.failed(clock());
        Err(refusal_error("Wrong password."))
    }
}

/// `prove_password_in` with this server's limiter and Navidrome.
#[cfg(feature = "server")]
pub(crate) async fn prove_password(
    user: &models::user::User,
    password: &str,
) -> Result<(), ServerFnError> {
    prove_password_in(
        &LOGIN_LIMITER,
        user,
        password,
        CONFIG.navidrome_url().is_some(),
        std::time::Instant::now,
        |username| async move { try_navidrome_auth(&username, password).await },
    )
    .await
}

/// Connects the signed-in account to Navidrome with its Navidrome password.
#[post("/api/auth/navidrome/connect", auth: AuthSession)]
pub async fn connect_navidrome(password: String) -> Result<(), ServerFnError> {
    use crate::crypto;
    use crate::db::DB;
    use crate::services::evict_navidrome_client;
    use std::time::Instant;

    if password.is_empty() {
        return Err(refusal_error("Enter your Navidrome password."));
    }
    let encrypted =
        crypto::encrypt(crate::secret_key::current(), &password).map_err(server_error)?;
    let password = &password;
    connect_navidrome_in(
        &DB,
        &LOGIN_LIMITER,
        &auth.0,
        &encrypted,
        Instant::now,
        |username| async move { try_navidrome_auth(&username, password).await },
    )
    .await?;
    tracing::info!(
        "User {} connected Navidrome",
        crate::oidc::loggable(&auth.0.username)
    );

    // The banner may come back if the credentials break again.
    let _ = models::user_settings::UserSettings::reset_navidrome_banner(&auth.0.user_id).await;
    // A cached client still holds the old password.
    evict_navidrome_client(&auth.0.user_id).await;
    Ok(())
}

#[post("/api/auth/refresh", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn refresh_token() -> Result<(), ServerFnError> {
    let session = SessionCookie::for_request(&headers, &uri)?;
    start_session(&cookies, &session, &auth.0.user_id)
        .await
        .map(|_| ())
}

/// Ends the session of `token` on the server, so a copy of the cookie stops
/// working too. The user's other sessions stay. A token that no longer
/// verifies has nothing to end.
#[cfg(feature = "server")]
pub(crate) async fn end_session_in(
    db: &sqlx::SqlitePool,
    key: &crate::secret_key::SecretKey,
    token: &str,
    now: i64,
) -> Result<(), String> {
    let Ok(claims) = auth::verify_token(key, token) else {
        return Ok(());
    };
    models::revoked_session::RevokedSession::revoke_in(db, &claims.jti, claims.exp as i64, now)
        .await
}

/// Ends the session the request's cookie holds, then clears the cookie. The
/// cookie stays when the token could not be revoked, so a failed sign-out
/// leaves the browser signed in rather than out while the token still works.
#[cfg(feature = "server")]
pub(crate) async fn sign_out_in(
    cookies: &Cookies,
    db: &sqlx::SqlitePool,
    key: &crate::secret_key::SecretKey,
    now: i64,
) -> Result<(), String> {
    if let Some(token) = cookies.get(AUTH_COOKIE_NAME) {
        end_session_in(db, key, token.value(), now).await?;
    }
    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, "");
    cookie.set_path("/");
    cookies.remove(cookie);
    Ok(())
}

#[post("/api/auth/logout", cookies: Cookies)]
pub async fn logout() -> Result<(), ServerFnError> {
    sign_out_in(
        &cookies,
        &crate::db::DB,
        crate::secret_key::current(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .map_err(server_error)
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
    use axum::http::{HeaderName, HeaderValue};

    fn derived(base: &str) -> BaseUrl {
        BaseUrl::parse(base).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    fn path_only() -> Uri {
        Uri::from_static("/api/auth/login")
    }

    fn cookie_for(
        pairs: &[(&str, &str)],
        public_url: Option<&Result<BaseUrl, BaseUrlError>>,
    ) -> Result<SessionCookie, ServerFnError> {
        SessionCookie::for_request_with(&headers(pairs), &path_only(), public_url)
    }

    fn secure_cookie_for(pairs: &[(&str, &str)]) -> bool {
        cookie_for(pairs, None).unwrap().secure
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
    fn a_lan_browser_on_plain_http_gets_a_cookie_it_keeps() {
        // http://nas:9765 typed in the browser, no proxy: the host derives
        // https, and the browser's Origin says http.
        assert!(!secure_cookie_for(&[
            ("host", "nas:9765"),
            ("origin", "http://nas:9765"),
        ]));
        assert!(!secure_cookie_for(&[("host", "192.168.1.5:9765")]));
    }

    #[test]
    fn a_named_host_gets_a_secure_cookie() {
        assert!(secure_cookie_for(&[("host", "music.example.com")]));
        assert!(secure_cookie_for(&[
            ("host", "soulful:9765"),
            ("x-forwarded-host", "music.example.com"),
            ("x-forwarded-proto", "https"),
            ("origin", "https://music.example.com"),
        ]));
    }

    #[test]
    fn a_request_without_an_address_is_refused() {
        let err = cookie_for(&[], None).err().unwrap();
        assert_eq!(code(&err), 409);
    }

    #[test]
    fn the_public_url_pin_decides_the_cookie() {
        let pin = BaseUrl::parse("https://music.example.com");
        assert!(
            cookie_for(&[("host", "192.168.1.5:9765")], Some(&pin))
                .unwrap()
                .secure
        );
        let bad = BaseUrl::parse("music.example.com");
        let err = cookie_for(&[("host", "music.example.com")], Some(&bad))
            .err()
            .unwrap();
        assert_eq!(code(&err), 409);
    }

    #[test]
    fn an_http2_request_without_host_gets_a_cookie() {
        let uri = Uri::from_static("http://music.example.com/api/auth/login");
        let session = SessionCookie::for_request_with(&HeaderMap::new(), &uri, None).unwrap();
        assert!(session.secure);
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
    use crate::server_fns::{RATE_LIMITED_CODE, REFUSAL_CODE};
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
    async fn an_account_without_a_password_is_closed_to_the_password_form() {
        // Made through single sign-on: it has no password, and no password,
        // the empty one included, opens it, whatever Navidrome answers.
        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "bob", "https://id.example.com", "sub-bob")
            .await
            .unwrap();
        let now = Instant::now();
        for password in ["", " ", "sub-bob", "bob"] {
            assert!(!sso.password_matches(password), "{password:?}");
            for result in [
                NavidromeAuthResult::NotConfigured,
                NavidromeAuthResult::Unreachable,
                NavidromeAuthResult::AuthFailed,
            ] {
                let refused = sign_in(&db, &LoginLimiter::new(), "bob", password, now, result)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(code(&refused), 401, "{password:?}, {result:?}");
            }
        }

        // A password an admin sets opens it.
        User::update_password_in(&db, &sso.id, "set-by-admin")
            .await
            .unwrap();
        let signed_in = sign_in(
            &db,
            &LoginLimiter::new(),
            "bob",
            "set-by-admin",
            now,
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap();
        assert!(matches!(signed_in, Credentials::Local { user, .. } if user.id == sso.id));
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

    #[tokio::test]
    async fn an_overlong_username_is_refused_without_being_counted() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        let name = "a".repeat(models::user::MAX_USERNAME_BYTES + 1);
        let asked = std::cell::Cell::new(false);
        for _ in 0..=FREE_FAILURES {
            let err = authenticate_in(&db, &limiter, &name, "pw", || now, async {
                asked.set(true);
                NavidromeAuthResult::AuthFailed
            })
            .await
            .err()
            .unwrap();
            assert_eq!(code(&err), 401);
        }
        assert!(!asked.get());
        // Nothing was recorded for the name, so it is not held back either.
        assert!(limiter.begin(&name, now).is_ok());
    }

    fn account(linked: bool, local_password_ok: bool) -> Option<((), ExistingAccount)> {
        Some((
            (),
            ExistingAccount {
                linked,
                local_password_ok,
            },
        ))
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
            LoginPlan::Link(())
        );
    }

    #[test]
    fn a_linked_row_is_refreshed_and_notes_a_password_change() {
        assert_eq!(
            navidrome_login_plan(account(true, false), RegistrationMode::Admin),
            LoginPlan::Refresh {
                row: (),
                password_changed: true
            }
        );
        assert_eq!(
            navidrome_login_plan(account(true, true), RegistrationMode::Admin),
            LoginPlan::Refresh {
                row: (),
                password_changed: false
            }
        );
    }

    #[test]
    fn new_names_follow_the_registration_mode() {
        let plan = |mode| navidrome_login_plan(None::<((), ExistingAccount)>, mode);
        assert_eq!(plan(RegistrationMode::Navidrome), LoginPlan::Create);
        assert_eq!(plan(RegistrationMode::Open), LoginPlan::Create);
        assert_eq!(plan(RegistrationMode::Admin), LoginPlan::RefuseNoAccount);
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
    async fn an_account_made_by_single_sign_on_is_not_joined_by_a_navidrome_sign_in() {
        // Mallory claimed the name "bob" at the provider before Navidrome's
        // Bob used Soulbeet. Bob's Navidrome login must not hand her his
        // Navidrome password or let both of them into one account.
        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "bob", "https://id.example.com", "sub-mallory")
            .await
            .unwrap();
        for mode in [
            RegistrationMode::Navidrome,
            RegistrationMode::Open,
            RegistrationMode::Admin,
        ] {
            let outcome = apply_navidrome_login(&db, "Bob", "navidrome-pw", "enc", mode)
                .await
                .unwrap();
            assert!(matches!(outcome, LoginOutcome::Refused(_)), "{outcome:?}");
        }
        let after = row(&db, "bob").await.unwrap();
        assert_eq!(after.navidrome_token, None);
        assert_eq!(after.credentials_version, sso.credentials_version);
        assert!(!after.has_password());
        assert!(!after.password_matches("navidrome-pw"));
        assert_eq!(after.oidc_subject.as_deref(), Some("sub-mallory"));
    }

    fn ctx_of(user: &User) -> crate::auth::UserCtx {
        crate::auth::UserCtx {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
        }
    }

    /// Connects Navidrome for `user`, Navidrome answering `result`; returns
    /// the name Navidrome was asked about.
    async fn connect(
        db: &sqlx::SqlitePool,
        limiter: &LoginLimiter,
        user: &User,
        now: Instant,
        result: NavidromeAuthResult,
    ) -> (Result<(), ServerFnError>, Option<String>) {
        let asked = std::cell::RefCell::new(None);
        let outcome = connect_navidrome_in(
            db,
            limiter,
            &ctx_of(user),
            "enc",
            || now,
            |name| {
                *asked.borrow_mut() = Some(name);
                async { result }
            },
        )
        .await;
        (outcome, asked.into_inner())
    }

    #[tokio::test]
    async fn an_account_made_by_single_sign_on_connects_its_own_navidrome_name() {
        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "bob", "https://id.example.com", "sub-bob")
            .await
            .unwrap();
        let limiter = LoginLimiter::new();
        let (outcome, asked) = connect(
            &db,
            &limiter,
            &sso,
            Instant::now(),
            NavidromeAuthResult::Success,
        )
        .await;
        outcome.unwrap();
        // Navidrome is asked about the account's own name, nobody else's.
        assert_eq!(asked.as_deref(), Some("bob"));
        let after = row(&db, "bob").await.unwrap();
        assert_eq!(after.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(after.navidrome_status, "connected");
        // Nobody is signed out, and nothing else about the account changes.
        assert_eq!(after.credentials_version, sso.credentials_version);
        assert!(!after.has_password());
        assert_eq!(after.oidc_subject.as_deref(), Some("sub-bob"));
    }

    #[tokio::test]
    async fn an_account_with_its_own_password_connects_navidrome_too() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "soulbeet-pw").await.unwrap();
        let (outcome, _) = connect(
            &db,
            &LoginLimiter::new(),
            &ann,
            Instant::now(),
            NavidromeAuthResult::Success,
        )
        .await;
        outcome.unwrap();
        let after = row(&db, "ann").await.unwrap();
        assert_eq!(after.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(after.credentials_version, ann.credentials_version);
        assert!(after.password_matches("soulbeet-pw"));
    }

    #[tokio::test]
    async fn a_wrong_navidrome_password_connects_nothing_and_counts_as_a_failed_sign_in() {
        // Mallory made "bob" through single sign-on: guessing Navidrome Bob's
        // password from her session is held back like the login form.
        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "bob", "https://id.example.com", "sub-mallory")
            .await
            .unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            let (outcome, _) =
                connect(&db, &limiter, &sso, now, NavidromeAuthResult::AuthFailed).await;
            // Not a 401: the session stays.
            assert_eq!(code(&outcome.unwrap_err()), REFUSAL_CODE);
        }
        let (outcome, asked) =
            connect(&db, &limiter, &sso, now, NavidromeAuthResult::Success).await;
        assert_eq!(code(&outcome.unwrap_err()), RATE_LIMITED_CODE);
        assert_eq!(asked, None);
        // The login form shares the count.
        assert!(limiter.begin("BOB", now).is_err());
        let after = row(&db, "bob").await.unwrap();
        assert_eq!(after.navidrome_token, None);
        assert_eq!(after.navidrome_status, sso.navidrome_status);
    }

    #[tokio::test]
    async fn no_verdict_from_navidrome_connects_nothing_and_counts_nothing() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for result in [
            NavidromeAuthResult::Unreachable,
            NavidromeAuthResult::NotConfigured,
        ] {
            for _ in 0..=FREE_FAILURES {
                let (outcome, _) = connect(&db, &limiter, &ann, now, result).await;
                assert_eq!(code(&outcome.unwrap_err()), REFUSAL_CODE);
            }
        }
        assert!(limiter.begin("ann", now).is_ok());
        assert_eq!(row(&db, "ann").await.unwrap().navidrome_token, None);
    }

    /// `user` proves its password before a link, Navidrome answering
    /// `result`; returns the name Navidrome was asked about.
    async fn prove(
        limiter: &LoginLimiter,
        user: &User,
        password: &str,
        navidrome_configured: bool,
        result: NavidromeAuthResult,
    ) -> (Result<(), ServerFnError>, Option<String>) {
        let asked = std::cell::RefCell::new(None);
        let outcome = prove_password_in(
            limiter,
            user,
            password,
            navidrome_configured,
            Instant::now,
            |name| {
                *asked.borrow_mut() = Some(name);
                async { result }
            },
        )
        .await;
        (outcome, asked.into_inner())
    }

    #[tokio::test]
    async fn linking_takes_the_password_the_login_form_takes() {
        use crate::auth::LinkProof;

        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "ann-pw").await.unwrap();
        assert_eq!(link_proof(&ann, true), LinkProof::Password);
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for result in [
            NavidromeAuthResult::NotConfigured,
            NavidromeAuthResult::AuthFailed,
        ] {
            let (ok, _) = prove(&limiter, &ann, "ann-pw", true, result).await;
            ok.unwrap();
        }
        // A wrong password counts as a failed sign-in of the name, and is
        // not a 401: the session stays.
        for _ in 0..=FREE_FAILURES {
            let (refused, _) =
                prove(&limiter, &ann, "guess", true, NavidromeAuthResult::Success).await;
            assert_eq!(code(&refused.unwrap_err()), REFUSAL_CODE);
        }
        let (held, asked) =
            prove(&limiter, &ann, "ann-pw", true, NavidromeAuthResult::Success).await;
        assert_eq!(code(&held.unwrap_err()), RATE_LIMITED_CODE);
        assert_eq!(asked, None);
        assert!(limiter.begin("ANN", now).is_err());
    }

    #[tokio::test]
    async fn for_an_account_linked_to_navidrome_navidrome_decides_while_it_answers() {
        let db = test_db::migrated().await;
        let bob = linked_bob(&db, "old-pw").await;
        let limiter = LoginLimiter::new();
        let (ok, asked) = prove(&limiter, &bob, "new-pw", true, NavidromeAuthResult::Success).await;
        ok.unwrap();
        assert_eq!(asked.as_deref(), Some("bob"));
        let (refused, _) = prove(
            &limiter,
            &bob,
            "old-pw",
            true,
            NavidromeAuthResult::AuthFailed,
        )
        .await;
        assert_eq!(code(&refused.unwrap_err()), REFUSAL_CODE);
        // Away, its own password counts.
        let (ok, _) = prove(
            &limiter,
            &bob,
            "old-pw",
            true,
            NavidromeAuthResult::Unreachable,
        )
        .await;
        ok.unwrap();
        let (refused, _) = prove(
            &limiter,
            &bob,
            "new-pw",
            false,
            NavidromeAuthResult::NotConfigured,
        )
        .await;
        assert_eq!(code(&refused.unwrap_err()), REFUSAL_CODE);
    }

    #[tokio::test]
    async fn an_account_without_a_password_links_with_its_navidrome_password_or_not_at_all() {
        use crate::auth::{LinkProof, NO_PASSWORD_TO_LINK};

        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "sso", "https://id.example.com", "sub-sso")
            .await
            .unwrap();
        let limiter = LoginLimiter::new();
        let now = Instant::now();

        // Nothing proves it: an admin sets a password first. Nothing is
        // counted and Navidrome is not asked.
        for navidrome in [true, false] {
            assert_eq!(link_proof(&sso, navidrome), LinkProof::NoPassword);
            for password in ["", "sub-sso"] {
                let (refused, asked) = prove(
                    &limiter,
                    &sso,
                    password,
                    navidrome,
                    NavidromeAuthResult::Success,
                )
                .await;
                let refused = refused.unwrap_err();
                assert_eq!(code(&refused), REFUSAL_CODE);
                assert!(
                    refused.to_string().contains(NO_PASSWORD_TO_LINK),
                    "{refused}"
                );
                assert_eq!(asked, None);
            }
        }

        // With a stored Navidrome login, its Navidrome password does.
        User::update_navidrome_token_in(&db, &sso.id, Some("enc"), "connected")
            .await
            .unwrap();
        let sso = row(&db, "sso").await.unwrap();
        assert_eq!(link_proof(&sso, true), LinkProof::NavidromePassword);
        assert_eq!(link_proof(&sso, false), LinkProof::NoPassword);
        let (ok, asked) = prove(
            &limiter,
            &sso,
            "navi-pw",
            true,
            NavidromeAuthResult::Success,
        )
        .await;
        ok.unwrap();
        assert_eq!(asked.as_deref(), Some("sso"));
        // No verdict from Navidrome: nothing proven, nothing counted.
        for _ in 0..=FREE_FAILURES {
            let (refused, _) = prove(
                &limiter,
                &sso,
                "navi-pw",
                true,
                NavidromeAuthResult::Unreachable,
            )
            .await;
            assert_eq!(code(&refused.unwrap_err()), REFUSAL_CODE);
        }
        assert!(limiter.begin("sso", now).is_ok());
        let (refused, _) = prove(
            &limiter,
            &sso,
            "guess",
            true,
            NavidromeAuthResult::AuthFailed,
        )
        .await;
        assert_eq!(code(&refused.unwrap_err()), REFUSAL_CODE);
    }

    #[tokio::test]
    async fn a_name_from_the_provider_cannot_forge_a_log_line_when_navidrome_refuses_it() {
        use crate::test_log::LogCapture;
        use crate::test_navidrome::{subsonic_failed, NavidromeStub};

        // Eve chose this name at the provider and signed in with it.
        let db = test_db::migrated().await;
        let forged = "eve\nINFO User 'admin' connected Navidrome";
        let eve = User::create_oidc_in(&db, forged, "https://id.example.com", "sub-eve")
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/ping",
            &[&subsonic_failed(40, "Wrong username or password")],
        );
        let url = navi.url.clone();
        let log = LogCapture::start();

        let outcome = connect_navidrome_in(
            &db,
            &LoginLimiter::new(),
            &ctx_of(&eve),
            "enc",
            Instant::now,
            |name| async move { navidrome_auth_at(&url, &name, "guess").await },
        )
        .await;

        assert_eq!(code(&outcome.unwrap_err()), REFUSAL_CODE);
        let lines = log.lines();
        let refused = lines
            .iter()
            .find(|line| line.starts_with("Navidrome refused the credentials"))
            .expect("the refusal is logged");
        assert!(refused.contains(r"eve\nINFO"), "{refused}");
        assert!(lines.iter().all(|line| !line.contains('\n')), "{lines:?}");
    }

    #[tokio::test]
    async fn a_prehijacked_row_is_refused_and_left_alone() {
        let db = test_db::migrated().await;
        User::create_in(&db, "bob", "attacker-pw").await.unwrap();
        let outcome =
            apply_navidrome_login(&db, "bob", "navidrome-pw", "enc", RegistrationMode::Open)
                .await
                .unwrap();
        match outcome {
            LoginOutcome::Refused(msg) => {
                assert!(
                    msg.starts_with("A Soulbeet account named 'bob' exists but is not linked"),
                    "{msg}"
                );
                // The way in that works, for an account with or without a
                // password of its own.
                assert!(
                    msg.contains("Connect Navidrome in Settings > Users"),
                    "{msg}"
                );
            }
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
        User::create_in(&db, "bob", "pw").await.unwrap();
        let u = signed_in(
            apply_navidrome_login(&db, "bob", "pw", "enc", RegistrationMode::Navidrome)
                .await
                .unwrap(),
        );
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(u.credentials_version, 1);
    }

    #[tokio::test]
    async fn a_linked_row_with_a_new_password_gets_one_bump_and_a_new_hash() {
        let db = test_db::migrated().await;
        linked_bob(&db, "old").await;
        let u = signed_in(
            apply_navidrome_login(&db, "bob", "new", "enc", RegistrationMode::Navidrome)
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
        linked_bob(&db, "pw").await;
        let u = signed_in(
            apply_navidrome_login(&db, "bob", "pw", "enc", RegistrationMode::Navidrome)
                .await
                .unwrap(),
        );
        assert_eq!(u.credentials_version, 1);
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
    }

    #[tokio::test]
    async fn a_new_name_in_admin_mode_creates_nothing() {
        let db = test_db::migrated().await;
        let outcome = apply_navidrome_login(&db, "carol", "pw", "enc", RegistrationMode::Admin)
            .await
            .unwrap();
        assert!(matches!(outcome, LoginOutcome::Refused(_)), "{outcome:?}");
        assert!(row(&db, "carol").await.is_none());
    }

    #[tokio::test]
    async fn a_new_name_in_navidrome_mode_is_created_linked() {
        let db = test_db::migrated().await;
        let u = signed_in(
            apply_navidrome_login(&db, "carol", "pw", "enc", RegistrationMode::Navidrome)
                .await
                .unwrap(),
        );
        assert_eq!(u.username, "carol");
        assert_eq!(u.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(u.navidrome_status, "connected");
        assert_eq!(u.role, crate::auth::Role::User);
        assert!(u.password_matches("pw"));
    }

    async fn user_count(db: &sqlx::SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_name_in_other_letter_case_signs_in_to_the_same_account() {
        // Navidrome compares names without case, so "Bob" is Bob's account.
        let db = test_db::migrated().await;
        let bob = linked_bob(&db, "pw").await;
        let before = user_count(&db).await;
        let u = signed_in(
            apply_navidrome_login(&db, "Bob", "pw", "enc", RegistrationMode::Navidrome)
                .await
                .unwrap(),
        );
        assert_eq!(u.id, bob.id);
        assert_eq!(u.username, "bob");
        assert_eq!(user_count(&db).await, before);
    }

    #[tokio::test]
    async fn an_unlinked_row_in_other_letter_case_is_not_taken_over() {
        let db = test_db::migrated().await;
        User::create_in(&db, "bob", "attacker-pw").await.unwrap();
        let outcome =
            apply_navidrome_login(&db, "Bob", "navidrome-pw", "enc", RegistrationMode::Open)
                .await
                .unwrap();
        assert!(matches!(outcome, LoginOutcome::Refused(_)), "{outcome:?}");
        assert!(row(&db, "Bob").await.is_none());
    }

    /// The migrated database plus rows whose names differ only in letter
    /// case, as an install from before the rule can hold.
    async fn colliding(names: &[&str]) -> sqlx::SqlitePool {
        let db = test_db::migrated_before(test_db::USERNAME_CASE).await;
        for name in names {
            sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
                .bind(format!("id-{name}"))
                .bind(name)
                .execute(&db)
                .await
                .unwrap();
        }
        test_db::apply_from(&db, test_db::USERNAME_CASE).await;
        db
    }

    #[tokio::test]
    async fn names_that_collide_without_case_are_refused_everywhere() {
        let db = colliding(&["bob", "Bob"]).await;
        let before = user_count(&db).await;
        for name in ["bob", "Bob", "BOB"] {
            let outcome =
                apply_navidrome_login(&db, name, "pw", "enc", RegistrationMode::Navidrome)
                    .await
                    .unwrap();
            match outcome {
                LoginOutcome::Refused(msg) => {
                    assert!(msg.starts_with("Several Soulbeet accounts"), "{msg}")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(user_count(&db).await, before);

        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            let err = sign_in(
                &db,
                &limiter,
                "bob",
                "pw",
                now,
                NavidromeAuthResult::NotConfigured,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(code(&err), 409);
            assert!(
                err.to_string().contains("Several Soulbeet accounts"),
                "{err}"
            );
        }
        // A refusal is not a wrong password, so nothing was counted.
        assert!(limiter.begin("bob", now).is_ok());
    }

    #[tokio::test]
    async fn local_sign_in_ignores_letter_case() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let ok = sign_in(
            &db,
            &limiter,
            "ANN",
            "right",
            Instant::now(),
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap();
        assert!(matches!(ok, Credentials::Local { user, .. } if user.id == ann.id));
    }

    #[tokio::test]
    async fn navidrome_refusing_a_linked_account_refuses_the_sign_in() {
        let db = test_db::migrated().await;
        linked_bob(&db, "pw").await;
        let limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            // The local password matches, but Navidrome said no.
            let err = sign_in(
                &db,
                &limiter,
                "bob",
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
            limiter.begin("bob", now).err(),
            Some(LoginBlock::Account { .. })
        ));
    }

    #[tokio::test]
    async fn a_linked_account_signs_in_locally_while_navidrome_is_away() {
        let db = test_db::migrated().await;
        let bob = linked_bob(&db, "pw").await;
        let limiter = LoginLimiter::new();
        for (result, expected) in [
            (
                NavidromeAuthResult::Unreachable,
                shared::system::NavidromeStatus::Offline,
            ),
            (
                NavidromeAuthResult::NotConfigured,
                shared::system::NavidromeStatus::Unknown,
            ),
        ] {
            let ok = sign_in(&db, &limiter, "bob", "pw", Instant::now(), result)
                .await
                .unwrap();
            assert!(
                matches!(&ok, Credentials::Local { user, status } if user.id == bob.id && *status == expected)
            );
        }
    }

    #[tokio::test]
    async fn an_unlinked_account_keeps_local_sign_in_when_navidrome_refuses() {
        let db = test_db::migrated().await;
        User::create_in(&db, "ann", "right").await.unwrap();
        let limiter = LoginLimiter::new();
        let ok = sign_in(
            &db,
            &limiter,
            "ann",
            "right",
            Instant::now(),
            NavidromeAuthResult::AuthFailed,
        )
        .await
        .unwrap();
        assert!(matches!(
            ok,
            Credentials::Local {
                status: shared::system::NavidromeStatus::InvalidCredentials,
                ..
            }
        ));
    }

    #[test]
    fn only_navidromes_own_refusal_is_a_failed_login() {
        use soulbeet::NavidromeError as E;
        let verdict = |e: E| ping_verdict(&Err(e));
        let subsonic = |code| E::Subsonic {
            code,
            message: "from Navidrome".to_string(),
        };

        assert!(matches!(
            ping_verdict(&Ok(())),
            NavidromeAuthResult::Success
        ));
        assert!(matches!(
            verdict(subsonic(40)),
            NavidromeAuthResult::AuthFailed
        ));

        let no_verdict = [
            // Missing parameter, generic error, and codes Navidrome never
            // sends for a ping.
            subsonic(10),
            subsonic(0),
            subsonic(41),
            subsonic(50),
            // A proxy in front of a Navidrome that is down.
            E::Http {
                action: "Navidrome request",
                status: 502,
                body: None,
                raw: None,
            },
            E::Http {
                action: "Navidrome request",
                status: 503,
                body: None,
                raw: None,
            },
            E::Http {
                action: "Navidrome request",
                status: 504,
                body: None,
                raw: None,
            },
            E::Http {
                action: "Navidrome request",
                status: 401,
                body: None,
                raw: None,
            },
            E::Parse("expected value at line 1 column 1".to_string()),
            E::Url(url::ParseError::EmptyHost),
            E::Config("base URL missing".to_string()),
            E::Transport("connection refused".to_string()),
            E::CircuitOpen,
        ];
        for e in no_verdict {
            let shown = format!("{e:?}");
            assert!(
                matches!(verdict(e), NavidromeAuthResult::Unreachable),
                "{shown}"
            );
        }
    }

    #[tokio::test]
    async fn a_sign_out_that_cannot_revoke_keeps_the_cookie() {
        use crate::secret_key::SecretKey;
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let key = SecretKey::new("k".repeat(32));
        let token = auth::create_token(&key, &ann.id, 0).unwrap();
        let now = chrono::Utc::now().timestamp();
        let cookies = Cookies::default();
        cookies.add(Cookie::new(AUTH_COOKIE_NAME, token.clone()));

        let closed = test_db::migrated().await;
        closed.close().await;
        assert!(sign_out_in(&cookies, &closed, &key, now).await.is_err());
        assert_eq!(
            cookies.get(AUTH_COOKIE_NAME).map(|c| c.value().to_string()),
            Some(token.clone())
        );

        sign_out_in(&cookies, &db, &key, now).await.unwrap();
        assert!(cookies.get(AUTH_COOKIE_NAME).is_none());
        let ended = crate::server_fns::guard::load_session(Some(&token), &db, &key).await;
        assert_eq!(ended.unwrap_err().0.as_u16(), 401);
    }

    #[tokio::test]
    async fn signing_out_ends_that_token_and_no_other() {
        use crate::secret_key::SecretKey;
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let key = SecretKey::new("k".repeat(32));
        let this_device = auth::create_token(&key, &ann.id, 0).unwrap();
        let other_device = auth::create_token(&key, &ann.id, 0).unwrap();

        end_session_in(&db, &key, &this_device, chrono::Utc::now().timestamp())
            .await
            .unwrap();

        let load = |t: String| {
            let (db, key) = (db.clone(), &key);
            async move { crate::server_fns::guard::load_session(Some(&t), &db, key).await }
        };
        assert_eq!(load(this_device).await.unwrap_err().0.as_u16(), 401);
        assert_eq!(load(other_device).await.unwrap().user_id, ann.id);
        // A token that does not verify has nothing to end.
        end_session_in(&db, &key, "not-a-token", 0).await.unwrap();
    }
}
