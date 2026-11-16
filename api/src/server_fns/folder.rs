use crate::models;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use super::{failure_error, refusal_error, server_error};
#[cfg(feature = "server")]
use crate::auth::UserCtx;
use crate::folder_policy::FolderAdminView;
#[cfg(feature = "server")]
use crate::folder_policy::{
    canonical_folder_path, check_folder_path, check_folder_update, FolderRefusal, PolicyRows,
};
#[cfg(feature = "server")]
use crate::models::folder_root::FolderRoot;
#[cfg(feature = "server")]
use crate::AdminSession;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};

#[get("/api/folders", auth: AuthSession)]
pub async fn get_user_folders() -> Result<Vec<models::folder::Folder>, ServerFnError> {
    crate::core::folders::list(&auth.0)
        .await
        .map_err(failure_error)
}

#[post("/api/folders", auth: AuthSession)]
pub async fn create_user_folder(
    name: String,
    path: String,
) -> Result<models::folder::Folder, ServerFnError> {
    let canonical = checked_folder_path(&auth.0, &path, FolderChange::Create).await?;
    models::folder::Folder::create(&auth.0.user_id, &name, path_str(&canonical))
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
fn path_str(path: &Path) -> &str {
    path.to_str().expect("canonical paths are UTF-8")
}

/// What a folder path is checked for.
#[cfg(feature = "server")]
#[derive(Clone, Copy)]
enum FolderChange<'a> {
    Create,
    /// An existing folder, currently stored at `stored`.
    Update {
        stored: &'a str,
    },
}

/// The canonical form of `path` once the folder rules allow `actor` to keep a
/// folder there. On create, the directory is made only after the checks
/// pass, and its canonical path must not move while it is made.
#[cfg(feature = "server")]
async fn checked_folder_path(
    actor: &UserCtx,
    path: &str,
    change: FolderChange<'_>,
) -> Result<PathBuf, ServerFnError> {
    let candidate = canonical_folder_path(Path::new(path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;
    let rows = PolicyRows::load().await.map_err(server_error)?;
    let (folders, roots) = rows.inputs();
    match change {
        FolderChange::Create => check_folder_path(&candidate, actor, &folders, &roots),
        FolderChange::Update { stored } => {
            check_folder_update(&candidate, stored, actor, &folders, &roots)
        }
    }
    .map_err(refusal_error)?;
    if let FolderChange::Create = change {
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

#[put("/api/folders/update", auth: AuthSession)]
pub async fn update_folder(
    folder_id: String,
    name: String,
    path: String,
) -> Result<(), ServerFnError> {
    let folder = crate::core::folders::owned(&auth.0, &folder_id)
        .await
        .map_err(failure_error)?;
    let canonical = checked_folder_path(
        &auth.0,
        &path,
        FolderChange::Update {
            stored: &folder.path,
        },
    )
    .await?;
    crate::core::folders::update(&auth.0, &folder_id, &name, path_str(&canonical))
        .await
        .map_err(failure_error)
}

#[delete("/api/folders/delete", auth: AuthSession)]
pub async fn delete_folder(folder_id: String) -> Result<(), ServerFnError> {
    crate::core::folders::delete(&auth.0, &folder_id)
        .await
        .map_err(failure_error)
}

#[get("/api/folders/admin", _: AdminSession)]
pub async fn get_folder_admin() -> Result<FolderAdminView, ServerFnError> {
    PolicyRows::load()
        .await
        .map(PolicyRows::admin_view)
        .map_err(server_error)
}

/// Deletes any user's folder, so an admin can clear one that blocks others.
/// Only the folder entry goes; its files stay on disk.
#[delete("/api/folders/admin/delete", _: AdminSession)]
pub async fn delete_any_folder(folder_id: String) -> Result<(), ServerFnError> {
    models::folder::Folder::delete(&folder_id)
        .await
        .map_err(server_error)
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
