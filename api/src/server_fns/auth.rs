use super::server_error;
use crate::auth::{self, AuthResponse};
use crate::models;
use dioxus::prelude::*;

#[server]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    models::user::User::create(&username, &password)
        .await
        .map_err(server_error)
        .map(|_| ())
}

#[server]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    let user = match models::user::User::verify(&username, &password).await {
        Ok(user) => user,
        Err(e) => return Err(server_error(e)),
    };

    auth::create_tokens(user.id, user.username).map_err(server_error)
}

#[server]
pub async fn refresh_token(token: String) -> Result<AuthResponse, ServerFnError> {
    let claims = match auth::verify_token(&token, "refresh") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    let _ = models::user::User::get_by_id(&claims.sub)
        .await
        .map_err(server_error)?;

    auth::create_tokens(claims.sub, claims.username).map_err(server_error)
}
