use crate::models;
use dioxus::prelude::*;

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
    let candidate = canonical_folder_path(Path::new(&path))
        .map_err(|e| refusal_error(FolderRefusal::Path(e)))?;

    let mut tx = crate::db::DB
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(server_error)?;

    let rows = PolicyRows::load_tx(&mut tx).await.map_err(server_error)?;
    let (folders, roots) = rows.inputs();
    check_folder_path(&candidate, &auth.0, &folders, &roots).map_err(refusal_error)?;

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

    let id = uuid::Uuid::new_v4().to_string();
    let folder = match sqlx::query_as::<_, models::folder::Folder>(
        "INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?) RETURNING *",
    )
    .bind(&id)
    .bind(&auth.0.user_id)
    .bind(&name)
    .bind(path_str(&created))
    .fetch_one(&mut *tx)
    .await
    {
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

    sqlx::query("UPDATE folders SET name = ?, path = ? WHERE id = ? AND user_id = ?")
        .bind(&name)
        .bind(path_str(&candidate))
        .bind(&folder_id)
        .bind(&auth.0.user_id)
        .execute(&mut *tx)
        .await
        .map_err(server_error)?;

    tx.commit().await.map_err(server_error)?;
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

#[post("/api/folders/shared", _: AdminSession)]
pub async fn set_folder_shared(folder_id: String, shared: bool) -> Result<(), ServerFnError> {
    models::folder::Folder::set_shared(&folder_id, shared)
        .await
        .map_err(server_error)
}
