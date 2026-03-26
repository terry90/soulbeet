use crate::auth::AuthResponse;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::{auth, models, server_fns::server_error, AuthSession};

#[cfg(feature = "server")]
use tower_cookies::{
    cookie::{time, SameSite},
    Cookie, Cookies,
};

pub const AUTH_COOKIE_NAME: &str = "auth_token";

/// Helper to configure the auth cookie consistently
#[cfg(feature = "server")]
fn build_auth_cookie(token: String) -> Cookie<'static> {
    use crate::auth::EXPIRATION_DAYS;

    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_expires(time::OffsetDateTime::now_utc() + time::Duration::days(EXPIRATION_DAYS));
    cookie
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
        _ => return NavidromeAuthResult::Unreachable,
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

#[post("/api/auth/login", cookies: Cookies)]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    use crate::crypto;
    use crate::services::evict_navidrome_client;
    use models::user::User;
    use shared::system::NavidromeStatus;

    match try_navidrome_auth(&username, &password).await {
        NavidromeAuthResult::Success => {
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

            let token =
                auth::create_token(user.id.clone(), user.username.clone()).map_err(server_error)?;
            cookies.add(build_auth_cookie(token));

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: NavidromeStatus::Connected,
            })
        }
        NavidromeAuthResult::AuthFailed => {
            // Navidrome rejected the credentials. Fall back to local auth.
            let user = User::get_by_username(&username)
                .await
                .map_err(server_error)?
                .ok_or_else(|| server_error("Invalid username or password"))?;

            // Verify local password
            let _ = User::verify(&username, &password)
                .await
                .map_err(|_| server_error("Invalid username or password"))?;

            // Mark Navidrome status as invalid_credentials
            User::update_navidrome_token(
                &user.id,
                user.navidrome_token.as_deref(),
                NavidromeStatus::InvalidCredentials.as_str(),
            )
            .await
            .map_err(server_error)?;

            // Evict cached client with stale credentials
            evict_navidrome_client(&user.id).await;

            let token =
                auth::create_token(user.id.clone(), user.username.clone()).map_err(server_error)?;
            cookies.add(build_auth_cookie(token));

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: NavidromeStatus::InvalidCredentials,
            })
        }
        NavidromeAuthResult::Unreachable => {
            // Navidrome is down. Fall back to local password verification.
            let user = User::verify(&username, &password)
                .await
                .map_err(|_| server_error("Invalid username or password"))?;

            // Mark Navidrome status as offline (keep existing token)
            User::update_navidrome_token(
                &user.id,
                user.navidrome_token.as_deref(),
                NavidromeStatus::Offline.as_str(),
            )
            .await
            .map_err(server_error)?;

            let token =
                auth::create_token(user.id.clone(), user.username.clone()).map_err(server_error)?;
            cookies.add(build_auth_cookie(token));

            Ok(AuthResponse {
                username: user.username,
                user_id: user.id,
                navidrome_status: NavidromeStatus::Offline,
            })
        }
    }
}

#[post("/api/auth/refresh", auth: AuthSession, cookies: Cookies)]
pub async fn refresh_token() -> Result<(), ServerFnError> {
    let claims = auth.0;

    let _ = models::user::User::get_by_id(&claims.sub)
        .await
        .map_err(server_error)?;

    let token = auth::create_token(claims.sub, claims.username).map_err(server_error)?;

    cookies.add(build_auth_cookie(token));

    Ok(())
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
    let claims = auth.0;

    let status = models::user::User::get_by_id(&claims.sub)
        .await
        .map(|u| shared::system::NavidromeStatus::from(u.navidrome_status))
        .unwrap_or_default();

    Ok(Some(AuthResponse {
        username: claims.username,
        user_id: claims.sub,
        navidrome_status: status,
    }))
}
