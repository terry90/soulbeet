use crate::models;
use dioxus::prelude::*;
use shared::library::DuplicateReport;

#[cfg(feature = "server")]
use super::{refusal_error, server_error};
#[cfg(feature = "server")]
use crate::auth::UserCtx;
use crate::folder_policy::FolderAdminView;
#[cfg(feature = "server")]
use crate::folder_policy::{
    canonical_folder_path, check_folder_path, folder_conflicts, load_policy_inputs,
    FolderAdminEntry, FolderRefusal,
};
#[cfg(feature = "server")]
use crate::models::{folder_root::FolderRoot, user::User};
#[cfg(feature = "server")]
use crate::services::music_importer;
#[cfg(feature = "server")]
use crate::AdminSession;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};

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
    let canonical = checked_folder_path(&auth.0, &path, true).await?;
    models::folder::Folder::create(&auth.0.user_id, &name, path_str(&canonical))
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
fn path_str(path: &Path) -> &str {
    path.to_str().expect("canonical paths are UTF-8")
}

/// The canonical form of `path` once the folder rules allow `actor` to keep a
/// folder there. With `create`, the directory is made only after the checks
/// pass, and its canonical path must not move while it is made.
#[cfg(feature = "server")]
async fn checked_folder_path(
    actor: &UserCtx,
    path: &str,
    create: bool,
) -> Result<PathBuf, ServerFnError> {
    let candidate = canonical_folder_path(Path::new(path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;
    let (folders, roots) = load_policy_inputs().await.map_err(server_error)?;
    check_folder_path(&candidate, actor, &folders, &roots).map_err(refusal_error)?;
    if create {
        tokio::fs::create_dir_all(&candidate)
            .await
            .map_err(|e| server_error(format!("Failed to create directory: {}", e)))?;
        let created =
            canonical_folder_path(&candidate).map_err(|e| refusal_error(FolderRefusal::Path(e)))?;
        if created != candidate {
            return Err(refusal_error(FolderRefusal::Changed));
        }
    }
    Ok(candidate)
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
    let canonical = checked_folder_path(&auth.0, &path, false).await?;
    models::folder::Folder::update(&folder_id, &name, path_str(&canonical))
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

#[get("/api/folders/admin", _: AdminSession)]
pub async fn get_folder_admin() -> Result<FolderAdminView, ServerFnError> {
    let users = User::get_all().await.map_err(server_error)?;
    let (policy_folders, policy_roots) = load_policy_inputs().await.map_err(server_error)?;
    let folders = models::folder::Folder::get_all()
        .await
        .map_err(server_error)?
        .into_iter()
        .map(|folder| {
            let owner = users.iter().find(|u| u.id == folder.user_id);
            FolderAdminEntry {
                owner: owner.map_or_else(|| folder.user_id.clone(), |u| u.username.clone()),
                owner_is_admin: owner.is_some_and(|u| u.role.is_admin()),
                folder,
            }
        })
        .collect();
    let roots = FolderRoot::get_all()
        .await
        .map_err(server_error)?
        .into_iter()
        .map(|r| r.path)
        .collect();
    Ok(FolderAdminView {
        roots,
        folders,
        conflicts: folder_conflicts(&policy_folders, &policy_roots),
    })
}

#[post("/api/folders/roots", _: AdminSession)]
pub async fn add_folder_root(path: String) -> Result<(), ServerFnError> {
    let canonical = canonical_folder_path(Path::new(&path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;
    FolderRoot::add(path_str(&canonical))
        .await
        .map_err(server_error)
}

#[delete("/api/folders/roots", _: AdminSession)]
pub async fn remove_folder_root(path: String) -> Result<(), ServerFnError> {
    FolderRoot::remove(&path).await.map_err(server_error)
}

#[post("/api/folders/shared", _: AdminSession)]
pub async fn set_folder_shared(folder_id: String, shared: bool) -> Result<(), ServerFnError> {
    models::folder::Folder::set_shared(&folder_id, shared)
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
            shared: false,
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
