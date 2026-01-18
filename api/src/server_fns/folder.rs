use crate::models;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use super::server_error;
#[cfg(feature = "server")]
use crate::AuthSession;

#[get("/api/folders", auth: AuthSession)]
pub async fn get_user_folders() -> Result<Vec<models::folder::Folder>, ServerFnError> {
    let claims = auth.0;

    models::folder::Folder::get_all_by_user(&claims.sub)
        .await
        .map_err(server_error)
}

#[post("/api/folders", auth: AuthSession)]
pub async fn create_user_folder(
    name: String,
    path: String,
) -> Result<models::folder::Folder, ServerFnError> {
    let claims = auth.0;

    if let Err(e) = tokio::fs::create_dir_all(&path).await {
        return Err(server_error(format!("Failed to create directory: {}", e)));
    }

    models::folder::Folder::create(&claims.sub, &name, &path)
        .await
        .map_err(server_error)
}

#[put("/api/folders/update", _: AuthSession)]
pub async fn update_folder(
    folder_id: String,
    name: String,
    path: String,
) -> Result<(), ServerFnError> {
    models::folder::Folder::update(&folder_id, &name, &path)
        .await
        .map_err(server_error)
}

#[delete("/api/folders/delete", _: AuthSession)]
pub async fn delete_folder(folder_id: String) -> Result<(), ServerFnError> {
    models::folder::Folder::delete(&folder_id)
        .await
        .map_err(server_error)
}
