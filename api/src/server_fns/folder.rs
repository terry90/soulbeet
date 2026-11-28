use crate::models;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeLibraryWarningDto {
    pub default_library_path: String,
    pub overlapping_user_paths: Vec<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLibrarySyncEntryDto {
    pub user_id: String,
    pub username: String,
    pub folder_path: String,
    pub navidrome_library_id: Option<i64>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeLibrarySyncReportDto {
    pub has_admin: bool,
    pub entries: Vec<UserLibrarySyncEntryDto>,
    pub warning: Option<NavidromeLibraryWarningDto>,
}

/// Returns the caller's single personal library folder (as a 0- or 1-element list
/// for simple UI consumption).
#[get("/api/folders", auth: AuthSession)]
pub async fn get_user_folders() -> Result<Vec<models::folder::Folder>, ServerFnError> {
    crate::core::folders::list(&auth.0)
        .await
        .map_err(failure_error)
}

/// Returns the caller's single personal library folder, if configured.
#[get("/api/folder", auth: AuthSession)]
pub async fn get_user_folder() -> Result<Option<models::folder::Folder>, ServerFnError> {
    models::folder::Folder::get_for_user(&auth.0.user_id)
        .await
        .map_err(server_error)
}

/// Creates (if none exists) or updates the caller's single personal library folder.
#[post("/api/folders", auth: AuthSession)]
pub async fn create_user_folder(
    name: String,
    path: String,
) -> Result<models::folder::Folder, ServerFnError> {
    if let Some(existing) = models::folder::Folder::get_for_user(&auth.0.user_id)
        .await
        .map_err(server_error)?
    {
        let canonical = checked_folder_path(
            &auth.0,
            &path,
            FolderChange::Update {
                stored: &existing.path,
            },
        )
        .await?;
        let canonical_str = path_str(&canonical);
        crate::core::folders::update(&auth.0, &existing.id, &name, canonical_str)
            .await
            .map_err(failure_error)?;
        let uid = auth.0.user_id.clone();
        tokio::spawn(async move {
            crate::core::navidrome_libraries::sync_single_user_library_best_effort(&uid).await;
        });
        return Ok(models::folder::Folder {
            id: existing.id,
            user_id: existing.user_id,
            name,
            path: canonical_str.to_string(),
        });
    }
    let canonical = checked_folder_path(&auth.0, &path, FolderChange::Create).await?;
    let folder = models::folder::Folder::create(&auth.0.user_id, &name, path_str(&canonical))
        .await
        .map_err(server_error)?;
    let uid = auth.0.user_id.clone();
    tokio::spawn(async move {
        crate::core::navidrome_libraries::sync_single_user_library_best_effort(&uid).await;
    });
    Ok(folder)
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
/// folder there. For both create and update, the directory is made only after
/// the checks pass, and its canonical path must not move while it is made.
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
    tokio::fs::create_dir_all(&candidate)
        .await
        .map_err(|e| server_error(format!("Failed to create directory: {}", e)))?;
    let created =
        canonical_folder_path(&candidate).map_err(|e| refusal_error(FolderRefusal::Path(e)))?;
    if created != candidate {
        return Err(refusal_error(FolderRefusal::Changed));
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
        .map_err(failure_error)?;
    let uid = auth.0.user_id.clone();
    tokio::spawn(async move {
        crate::core::navidrome_libraries::sync_single_user_library_best_effort(&uid).await;
    });
    Ok(())
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

#[post("/api/admin/navidrome/sync-libraries", _: AdminSession)]
pub async fn sync_navidrome_libraries() -> Result<NavidromeLibrarySyncReportDto, ServerFnError> {
    let report = crate::core::navidrome_libraries::sync_all_user_libraries().await;
    Ok(NavidromeLibrarySyncReportDto {
        has_admin: report.has_admin,
        entries: report
            .entries
            .into_iter()
            .map(|e| UserLibrarySyncEntryDto {
                user_id: e.user_id,
                username: e.username,
                folder_path: e.folder_path,
                navidrome_library_id: e.navidrome_library_id,
                status: e.status,
            })
            .collect(),
        warning: report.warning.map(|w| NavidromeLibraryWarningDto {
            default_library_path: w.default_library_path,
            overlapping_user_paths: w.overlapping_user_paths,
            message: w.message,
        }),
    })
}

#[get("/api/admin/navidrome/library-warning", _: AdminSession)]
pub async fn get_navidrome_library_warning(
) -> Result<Option<NavidromeLibraryWarningDto>, ServerFnError> {
    Ok(crate::core::navidrome_libraries::inspect_library_warning()
        .await
        .map(|w| NavidromeLibraryWarningDto {
            default_library_path: w.default_library_path,
            overlapping_user_paths: w.overlapping_user_paths,
            message: w.message,
        }))
}
