use crate::models;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use super::{failure_error, refusal_error, server_error};
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
use std::path::Path;

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
    let candidate = canonical_folder_path(Path::new(&path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;

    let mut tx = crate::db::DB
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(server_error)?;

    let existing = sqlx::query_as::<_, models::folder::Folder>(
        "SELECT * FROM folders WHERE user_id = ? LIMIT 1",
    )
    .bind(&auth.0.user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(server_error)?;

    let rows = PolicyRows::load_tx(&mut tx).await.map_err(server_error)?;
    let (folders, roots) = rows.inputs();
    match existing.as_ref() {
        Some(existing) => {
            check_folder_update(&candidate, &existing.path, &auth.0, &folders, &roots)
        }
        None => check_folder_path(&candidate, &auth.0, &folders, &roots),
    }
    .map_err(refusal_error)?;

    let dir_existed = tokio::fs::try_exists(&candidate).await.unwrap_or(true);
    if !dir_existed {
        tokio::fs::create_dir_all(&candidate)
            .await
            .map_err(|e| server_error(format!("Failed to create directory: {}", e)))?;
    }
    let created = match canonical_folder_path(&candidate) {
        Ok(c) if c == candidate => c,
        Ok(_) => {
            if !dir_existed {
                let _ = tokio::fs::remove_dir(&candidate).await;
            }
            return Err(refusal_error(FolderRefusal::Changed));
        }
        Err(e) => {
            if !dir_existed {
                let _ = tokio::fs::remove_dir(&candidate).await;
            }
            return Err(refusal_error(FolderRefusal::Path(e)));
        }
    };

    let folder_res = if let Some(existing) = existing {
        sqlx::query_as::<_, models::folder::Folder>(
            "UPDATE folders SET name = ?, path = ? WHERE id = ? AND user_id = ? RETURNING *",
        )
        .bind(&name)
        .bind(path_str(&created))
        .bind(&existing.id)
        .bind(&auth.0.user_id)
        .fetch_one(&mut *tx)
        .await
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query_as::<_, models::folder::Folder>(
            "INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(&auth.0.user_id)
        .bind(&name)
        .bind(path_str(&created))
        .fetch_one(&mut *tx)
        .await
    };

    let folder = match folder_res {
        Ok(folder) => folder,
        Err(e) => {
            let _ = tx.rollback().await;
            if !dir_existed {
                let _ = tokio::fs::remove_dir(&candidate).await;
            }
            return Err(server_error(e));
        }
    };

    if let Err(e) = tx.commit().await {
        if !dir_existed {
            let _ = tokio::fs::remove_dir(&candidate).await;
        }
        return Err(server_error(e));
    }

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

#[put("/api/folders/update", auth: AuthSession)]
pub async fn update_folder(
    folder_id: String,
    name: String,
    path: String,
) -> Result<(), ServerFnError> {
    let candidate = canonical_folder_path(Path::new(&path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;

    let mut tx = crate::db::DB
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(server_error)?;

    let existing = sqlx::query_as::<_, models::folder::Folder>(
        "SELECT * FROM folders WHERE id = ? AND user_id = ?",
    )
    .bind(&folder_id)
    .bind(&auth.0.user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(server_error)?
    .ok_or_else(|| {
        failure_error(shared::failure::Failure::new(
            shared::failure::FailureCode::UnknownHandle,
            shared::failure::Stage::Input,
            shared::failure::RetryAdvice::NotRetryable,
            "Folder not found",
        ))
    })?;

    let rows = PolicyRows::load_tx(&mut tx).await.map_err(server_error)?;
    let (folders, roots) = rows.inputs();
    check_folder_update(&candidate, &existing.path, &auth.0, &folders, &roots)
        .map_err(refusal_error)?;

    let dir_existed = tokio::fs::try_exists(&candidate).await.unwrap_or(true);
    if !dir_existed {
        tokio::fs::create_dir_all(&candidate)
            .await
            .map_err(|e| server_error(format!("Failed to create directory: {}", e)))?;
    }
    let created = match canonical_folder_path(&candidate) {
        Ok(c) if c == candidate => c,
        Ok(_) => {
            if !dir_existed {
                let _ = tokio::fs::remove_dir(&candidate).await;
            }
            return Err(refusal_error(FolderRefusal::Changed));
        }
        Err(e) => {
            if !dir_existed {
                let _ = tokio::fs::remove_dir(&candidate).await;
            }
            return Err(refusal_error(FolderRefusal::Path(e)));
        }
    };

    if let Err(e) = sqlx::query("UPDATE folders SET name = ?, path = ? WHERE id = ? AND user_id = ?")
        .bind(&name)
        .bind(path_str(&created))
        .bind(&folder_id)
        .bind(&auth.0.user_id)
        .execute(&mut *tx)
        .await
    {
        let _ = tx.rollback().await;
        if !dir_existed {
            let _ = tokio::fs::remove_dir(&candidate).await;
        }
        return Err(server_error(e));
    }

    if let Err(e) = tx.commit().await {
        if !dir_existed {
            let _ = tokio::fs::remove_dir(&candidate).await;
        }
        return Err(server_error(e));
    }

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
