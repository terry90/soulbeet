#[cfg(feature = "server")]
use super::{
    auth::{start_session, SessionCookie},
    refusal_error, server_error,
};
use crate::models;
#[cfg(feature = "server")]
use crate::{config::CONFIG, AdminSession, AuthSession};
#[cfg(feature = "server")]
use axum::http::HeaderMap;
use dioxus::prelude::*;
#[cfg(feature = "server")]
use tower_cookies::Cookies;

#[get("/api/users", _: AuthSession)]
pub async fn get_users() -> Result<Vec<models::user::User>, ServerFnError> {
    models::user::User::get_all().await.map_err(server_error)
}

/// Creates an account. Allowed in every registration mode: the mode limits
/// strangers, not admins.
#[post("/api/users/create", _: AdminSession)]
pub async fn create_user(
    username: String,
    password: String,
) -> Result<models::user::User, ServerFnError> {
    use models::user::User;

    if username.is_empty() || password.is_empty() {
        return Err(refusal_error("Username and password are required"));
    }
    if User::get_by_username(&username)
        .await
        .map_err(server_error)?
        .is_some()
    {
        return Err(refusal_error("That username is taken"));
    }
    User::create(&username, &password)
        .await
        .map_err(server_error)
}

#[post("/api/users/username", auth: AuthSession)]
pub async fn update_username(new_username: String) -> Result<(), ServerFnError> {
    if CONFIG.navidrome_url().is_some() {
        return Err(refusal_error(
            "Usernames follow Navidrome while NAVIDROME_URL is set, so they cannot be changed here.",
        ));
    }
    if new_username.trim().is_empty() {
        return Err(server_error("Username cannot be empty"));
    }
    models::user::User::update_username(&auth.0.user_id, new_username.trim())
        .await
        .map_err(server_error)
}

#[post("/api/users/password", auth: AuthSession, cookies: Cookies, headers: HeaderMap)]
pub async fn update_user_password(user_id: String, password: String) -> Result<(), ServerFnError> {
    // The bump ends every session of this user; the caller's own is renewed.
    let own_session = if user_id == auth.0.user_id {
        Some(SessionCookie::for_request(&headers)?)
    } else {
        None
    };
    models::user::User::update_password(&user_id, &password)
        .await
        .map_err(server_error)?;
    if let Some(session) = own_session {
        start_session(&cookies, &session, &user_id).await?;
    }
    Ok(())
}

#[delete("/api/users/delete", _: AuthSession)]
pub async fn delete_user(user_id: String) -> Result<(), ServerFnError> {
    models::user::User::delete(&user_id)
        .await
        .map_err(server_error)
}
