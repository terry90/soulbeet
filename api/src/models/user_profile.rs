use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use shared::recommendation::UserMusicProfile;

#[cfg(feature = "server")]
use crate::db::DB;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct UserProfileRow {
    pub user_id: String,
    pub profile_json: String,
    pub last_report: Option<String>,
    pub updated_at: String,
}

#[cfg(feature = "server")]
impl UserProfileRow {
    pub(crate) async fn upsert_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
        profile: &UserMusicProfile,
    ) -> Result<(), String> {
        let json = serde_json::to_string(profile).map_err(|e| e.to_string())?;
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_profiles (user_id, profile_json, updated_at)
             VALUES (?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
               profile_json = excluded.profile_json,
               updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&json)
        .bind(&now)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn get_report(user_id: &str) -> Result<Option<String>, String> {
        Self::get_report_in(&DB, user_id).await
    }

    pub(crate) async fn get_report_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<Option<String>, String> {
        let row = sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_report FROM user_profiles WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.flatten())
    }
}
