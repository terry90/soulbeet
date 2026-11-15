use crate::models;
use dioxus::prelude::*;
use shared::library::DuplicateReport;

#[cfg(feature = "server")]
use super::{refusal_error, server_error};
#[cfg(feature = "server")]
use crate::services::music_importer;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use std::path::Path;

#[get("/api/folders", auth: AuthSession)]
pub async fn get_user_folders() -> Result<Vec<models::folder::Folder>, ServerFnError> {
    models::folder::Folder::get_all_by_user(&auth.0.user_id)
        .await
        .map_err(server_error)
}

#[post("/api/folders", auth: AuthSession)]
pub async fn create_user_folder(
    name: String,
    path: String,
) -> Result<models::folder::Folder, ServerFnError> {
    if let Err(e) = tokio::fs::create_dir_all(&path).await {
        return Err(server_error(format!("Failed to create directory: {}", e)));
    }

    models::folder::Folder::create(&auth.0.user_id, &name, &path)
        .await
        .map_err(server_error)
}

/// The folder, when `user_id` owns it. Another user's folder is reported as
/// missing, the same as one that does not exist.
#[cfg(feature = "server")]
pub(crate) fn ensure_owner(
    folder: Option<models::folder::Folder>,
    user_id: &str,
) -> Result<models::folder::Folder, ServerFnError> {
    folder
        .filter(|f| f.user_id == user_id)
        .ok_or_else(|| refusal_error("Folder not found"))
}

#[put("/api/folders/update", auth: AuthSession)]
pub async fn update_folder(
    folder_id: String,
    name: String,
    path: String,
) -> Result<(), ServerFnError> {
    let folder = models::folder::Folder::get_by_id(&folder_id)
        .await
        .map_err(server_error)?;
    ensure_owner(folder, &auth.0.user_id)?;
    models::folder::Folder::update(&folder_id, &name, &path)
        .await
        .map_err(server_error)
}

#[delete("/api/folders/delete", auth: AuthSession)]
pub async fn delete_folder(folder_id: String) -> Result<(), ServerFnError> {
    let folder = models::folder::Folder::get_by_id(&folder_id)
        .await
        .map_err(server_error)?;
    ensure_owner(folder, &auth.0.user_id)?;
    models::folder::Folder::delete(&folder_id)
        .await
        .map_err(server_error)
}

/// Find duplicate tracks across all of the user's library folders.
/// Returns tracks that exist in multiple libraries (same artist + title).
#[get("/api/folders/duplicates", auth: AuthSession)]
pub async fn get_folder_duplicates() -> Result<DuplicateReport, ServerFnError> {
    let folders = models::folder::Folder::get_all_by_user(&auth.0.user_id)
        .await
        .map_err(server_error)?;

    if folders.len() < 2 {
        return Ok(DuplicateReport {
            duplicates: vec![],
            total_duplicate_tracks: 0,
            libraries_scanned: folders.iter().map(|f| f.path.clone()).collect(),
        });
    }

    let paths: Vec<&Path> = folders.iter().map(|f| Path::new(&f.path)).collect();

    let importer = music_importer(None).await.map_err(server_error)?;
    importer.find_duplicates(&paths).await.map_err(server_error)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::models::folder::Folder;

    fn folder(id: &str, user_id: &str) -> Folder {
        Folder {
            id: id.to_string(),
            user_id: user_id.to_string(),
            name: "Music".to_string(),
            path: "/music".to_string(),
        }
    }

    #[test]
    fn another_users_folder_is_not_found() {
        let err = ensure_owner(Some(folder("f1", "user-b")), "user-a").unwrap_err();
        assert!(
            matches!(err, ServerFnError::ServerError { code: 409, ref message, .. } if message == "Folder not found")
        );
        assert!(ensure_owner(None, "user-a").is_err());
        assert!(ensure_owner(Some(folder("f1", "user-a")), "user-a").is_ok());
    }
}
