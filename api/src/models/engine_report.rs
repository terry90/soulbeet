use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct EngineReportRow {
    pub id: String,
    pub user_id: String,
    pub profile: String,
    pub report_json: String,
    pub candidate_count: i32,
    pub created_at: String,
}

#[cfg(feature = "server")]
impl EngineReportRow {
    pub(crate) async fn insert_in(
        conn: &mut sqlx::SqliteConnection,
        user_id: &str,
        profile: &str,
        report_json: &str,
        candidate_count: u32,
    ) -> Result<(), String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO engine_reports (id, user_id, profile, report_json, candidate_count) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(profile)
        .bind(report_json)
        .bind(candidate_count as i32)
        .execute(conn)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The newest first. created_at has whole seconds, so rowid orders reports stored in one.
    pub async fn get_history(user_id: &str, limit: u32) -> Result<Vec<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT * FROM engine_reports WHERE user_id = ? ORDER BY created_at DESC, rowid DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&*DB)
        .await
        .map_err(|e| e.to_string())
    }

    /// Each profile's newest report, the newest first: what describes its candidate pool now.
    pub(crate) async fn latest_per_profile_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<Vec<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT id, user_id, profile, report_json, candidate_count, created_at FROM (
               SELECT *, rowid AS seq, ROW_NUMBER() OVER (
                 PARTITION BY profile ORDER BY created_at DESC, rowid DESC
               ) AS n FROM engine_reports WHERE user_id = ?
             ) WHERE n = 1 ORDER BY created_at DESC, seq DESC",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    /// Keep only the last N reports per user, delete older ones
    pub(crate) async fn prune_in(
        conn: &mut sqlx::SqliteConnection,
        user_id: &str,
        keep: u32,
    ) -> Result<(), String> {
        sqlx::query(
            "DELETE FROM engine_reports WHERE user_id = ? AND id NOT IN (SELECT id FROM engine_reports WHERE user_id = ? ORDER BY created_at DESC, rowid DESC LIMIT ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(keep)
        .execute(conn)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}
