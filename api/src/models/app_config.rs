use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use sqlx::SqlitePool;

pub mod keys {
    pub const SLSKD_API_KEY: &str = "slskd_api_key";
    pub const SLSKD_URL: &str = "slskd_url";
    /// This install's id, made on first use: what tells two installs sharing a downloads
    /// folder apart.
    pub const INSTANCE_ID: &str = "instance_id";
    /// The AirDC++ sidecar's Web API address, its web user, and that user's password,
    /// encrypted under the secret key.
    pub const AIRDCPP_URL: &str = "airdcpp_url";
    pub const AIRDCPP_USERNAME: &str = "airdcpp_username";
    pub const AIRDCPP_PASSWORD: &str = "airdcpp_password";

    /// The key that holds `"true"` while an admin has the backend `id` turned on.
    pub fn backend_enabled(id: &str) -> String {
        format!("{id}_enabled")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct AppConfig {
    pub key: String,
    pub value: String,
}

#[cfg(feature = "server")]
impl AppConfig {
    pub async fn get(key: &str) -> Result<Option<String>, String> {
        Self::get_in(&DB, key).await
    }

    pub async fn get_in(db: &SqlitePool, key: &str) -> Result<Option<String>, String> {
        let row = sqlx::query_as::<_, Self>("SELECT * FROM app_config WHERE key = ?")
            .bind(key)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;

        Ok(row.map(|r| r.value))
    }

    pub async fn set(key: &str, value: &str) -> Result<(), String> {
        Self::set_in(&DB, key, value).await
    }

    pub async fn set_in(db: &SqlitePool, key: &str, value: &str) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO app_config (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        )
        .bind(key)
        .bind(value)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub async fn delete(key: &str) -> Result<(), String> {
        Self::delete_in(&DB, key).await
    }

    pub async fn delete_in(db: &SqlitePool, key: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM app_config WHERE key = ?")
            .bind(key)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub async fn get_all() -> Result<Vec<Self>, String> {
        sqlx::query_as::<_, Self>("SELECT * FROM app_config ORDER BY key")
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }
}
