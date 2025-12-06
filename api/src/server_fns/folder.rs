use super::server_error;
use crate::auth;
use crate::models;
use dioxus::prelude::*;

#[server]
pub async fn get_user_folders(token: String) -> Result<Vec<models::folder::Folder>, ServerFnError> {
    let claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::folder::Folder::get_all_by_user(&claims.sub)
        .await
        .map_err(server_error)
}

#[server]
pub async fn create_user_folder(
    token: String,
    name: String,
    path: String,
) -> Result<models::folder::Folder, ServerFnError> {
    let claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    if let Err(e) = tokio::fs::create_dir_all(&path).await {
        return Err(server_error(format!("Failed to create directory: {}", e)));
    }

    models::folder::Folder::create(&claims.sub, &name, &path)
        .await
        .map_err(server_error)
}

#[server]
pub async fn update_folder(
    token: String,
    folder_id: String,
    name: String,
    path: String,
) -> Result<(), ServerFnError> {
    let _claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::folder::Folder::update(&folder_id, &name, &path)
        .await
        .map_err(server_error)
}

#[server]
pub async fn delete_folder(token: String, folder_id: String) -> Result<(), ServerFnError> {
    let _claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    models::folder::Folder::delete(&folder_id)
        .await
        .map_err(server_error)
}
