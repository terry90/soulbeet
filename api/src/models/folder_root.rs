use crate::db::DB;

/// A directory under which non-admins may create folders.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct FolderRoot {
    pub path: String,
    pub created_at: String,
}

impl FolderRoot {
    pub async fn get_all() -> Result<Vec<FolderRoot>, String> {
        sqlx::query_as::<_, FolderRoot>("SELECT * FROM folder_roots ORDER BY path")
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn add(path: &str) -> Result<(), String> {
        sqlx::query("INSERT OR IGNORE INTO folder_roots (path) VALUES (?)")
            .bind(path)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn remove(path: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM folder_roots WHERE path = ?")
            .bind(path)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
