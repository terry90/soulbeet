use super::server_error;
use crate::auth::{self, AuthResponse};
use crate::models;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use tower_cookies::{
    cookie::{time, SameSite},
    Cookie, Cookies,
};

pub const AUTH_COOKIE_NAME: &str = "auth_token";

/// Helper to configure the auth cookie consistently
#[cfg(feature = "server")]
fn build_auth_cookie(token: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_expires(time::OffsetDateTime::now_utc() + time::Duration::days(30));
    cookie
}

#[post("/api/auth/register")]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    models::user::User::create(&username, &password)
        .await
        .map_err(server_error)
        .map(|_| ())
}

#[post("/api/auth/login", cookies: Cookies)]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    let user = match models::user::User::verify(&username, &password).await {
        Ok(user) => user,
        Err(e) => return Err(server_error(e)),
    };

    let response = auth::create_token(user.id, user.username).map_err(server_error)?;

    cookies.add(build_auth_cookie(response.token.clone()));

    Ok(response)
}

#[post("/api/auth/refresh", auth: AuthSession, cookies: Cookies)]
pub async fn refresh_token() -> Result<AuthResponse, ServerFnError> {
    let claims = auth.0;

    let _ = models::user::User::get_by_id(&claims.sub)
        .await
        .map_err(server_error)?;

    let response = auth::create_token(claims.sub, claims.username).map_err(server_error)?;

    cookies.add(build_auth_cookie(response.token.clone()));

    Ok(response)
}

#[post("/api/auth/logout", cookies: Cookies)]
pub async fn logout() -> Result<(), ServerFnError> {
    let mut cookie = Cookie::new(AUTH_COOKIE_NAME, "");
    cookie.set_path("/");

    cookies.remove(cookie);

    Ok(())
}

#[get("/api/auth/me", auth: AuthSession, cookies: Cookies)]
pub async fn get_current_user() -> Result<Option<AuthResponse>, ServerFnError> {
    let claims = auth.0;

    let token = cookies
        .get(AUTH_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    Ok(Some(AuthResponse {
        token,
        username: claims.username,
        user_id: claims.sub,
        expires_at: claims.exp as i64,
    }))
}
