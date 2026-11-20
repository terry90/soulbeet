use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db::DB;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct UserProfileRow {
    pub user_id: String,
    pub last_report: Option<String>,
    pub updated_at: String,
}

#[cfg(feature = "server")]
impl UserProfileRow {
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
