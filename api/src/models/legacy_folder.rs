#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

/// A retired secondary or formerly `shared = true` folder recorded during the
/// single-folder-per-user migration so its audio files on disk can be safely
/// hardlinked/imported into each user's personal library folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct LegacyFolder {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub path: String,
    pub was_shared: bool,
    pub target_folder_id: String,
    pub migrated_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct LegacyFolderWithOwner {
    #[cfg_attr(feature = "server", sqlx(flatten))]
    pub legacy: LegacyFolder,
    pub owner: String,
    pub target_path: String,
}

#[cfg(feature = "server")]
impl LegacyFolder {
    pub async fn list_unmigrated() -> Result<Vec<LegacyFolderWithOwner>, String> {
        Self::list_unmigrated_in(&DB).await
    }

    pub(crate) async fn list_unmigrated_in(
        db: &SqlitePool,
    ) -> Result<Vec<LegacyFolderWithOwner>, String> {
        sqlx::query_as::<_, LegacyFolderWithOwner>(
            "SELECT lf.*, u.username AS owner, f.path AS target_path \
             FROM legacy_folders lf \
             JOIN users u ON u.id = lf.user_id \
             JOIN folders f ON f.id = lf.target_folder_id \
             WHERE lf.migrated_at IS NULL \
             ORDER BY lf.path, u.username",
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub(crate) async fn list_unmigrated_for_path_in(
        db: &SqlitePool,
        path: &str,
    ) -> Result<Vec<LegacyFolderWithOwner>, String> {
        sqlx::query_as::<_, LegacyFolderWithOwner>(
            "SELECT lf.*, u.username AS owner, f.path AS target_path \
             FROM legacy_folders lf \
             JOIN users u ON u.id = lf.user_id \
             JOIN folders f ON f.id = lf.target_folder_id \
             WHERE lf.migrated_at IS NULL AND lf.path = ? \
             ORDER BY u.username",
        )
        .bind(path)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub(crate) async fn mark_migrated_in(db: &SqlitePool, id: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE legacy_folders SET migrated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) async fn mark_path_migrated_in(db: &SqlitePool, path: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE legacy_folders SET migrated_at = ? WHERE path = ? AND migrated_at IS NULL")
            .bind(&now)
            .bind(path)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn dismiss(id: &str) -> Result<(), String> {
        Self::mark_migrated_in(&DB, id).await
    }
}
