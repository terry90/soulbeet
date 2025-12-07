#[cfg(feature = "server")]
use super::server_error;
use crate::models;
#[cfg(feature = "server")]
use crate::AuthSession;
use dioxus::prelude::*;

#[get("/api/users", auth: AuthSession)]
pub async fn get_users() -> Result<Vec<models::user::User>, ServerFnError> {
    models::user::User::get_all().await.map_err(server_error)
}

#[post("/api/users/password", auth: AuthSession)]
pub async fn update_user_password(user_id: String, password: String) -> Result<(), ServerFnError> {
    models::user::User::update_password(&user_id, &password)
        .await
        .map_err(server_error)
}

#[delete("/api/users/delete", auth: AuthSession)]
pub async fn delete_user(user_id: String) -> Result<(), ServerFnError> {
    models::user::User::delete(&user_id)
        .await
        .map_err(server_error)
}
