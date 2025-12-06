use super::server_error;
use crate::auth;
use crate::models;
use dioxus::prelude::*;

#[server]
pub async fn get_users(token: String) -> Result<Vec<models::user::User>, ServerFnError> {
    let _claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::user::User::get_all().await.map_err(server_error)
}

#[server]
pub async fn update_user_password(
    token: String,
    user_id: String,
    password: String,
) -> Result<(), ServerFnError> {
    let _claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::user::User::update_password(&user_id, &password)
        .await
        .map_err(server_error)
}

#[server]
pub async fn delete_user(token: String, user_id: String) -> Result<(), ServerFnError> {
    let _claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::user::User::delete(&user_id)
        .await
        .map_err(server_error)
}
