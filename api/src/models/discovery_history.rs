use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct DiscoveryHistoryRow {
    pub id: String,
    pub user_id: String,
    pub artist: String,
    pub track: String,
    pub profile: String,
    pub outcome: String,
    pub created_at: String,
}

#[cfg(feature = "server")]
impl DiscoveryHistoryRow {
    /// Record a track as suggested to a user for a specific profile.
    pub async fn record(
        user_id: &str,
        artist: &str,
        track: &str,
        profile: &str,
    ) -> Result<(), String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO discovery_history (id, user_id, artist, track, profile) VALUES (?, ?, ?, ?, ?)"
        )
        .bind(&id).bind(user_id).bind(artist).bind(track).bind(profile)
        .execute(&*DB).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Update the outcome for a track (e.g., promoted, removed, expired).
    pub async fn update_outcome(
        user_id: &str,
        artist: &str,
        track: &str,
        outcome: &str,
    ) -> Result<(), String> {
        Self::update_outcome_in(&DB, user_id, artist, track, outcome).await
    }

    pub(crate) async fn update_outcome_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
        artist: &str,
        track: &str,
        outcome: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE discovery_history SET outcome = ? WHERE user_id = ? AND lower(artist) = lower(?) AND lower(track) = lower(?)"
        )
        .bind(outcome).bind(user_id).bind(artist).bind(track)
        .execute(db).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Check if a track was ever suggested to this user (in any profile, any batch).
    pub async fn was_suggested(user_id: &str, artist: &str, track: &str) -> Result<bool, String> {
        let count = sqlx::query_scalar::<_, i32>(
            "SELECT COUNT(*) FROM discovery_history WHERE user_id = ? AND lower(artist) = lower(?) AND lower(track) = lower(?)"
        )
        .bind(user_id).bind(artist).bind(track)
        .fetch_one(&*DB).await.map_err(|e| e.to_string())?;
        Ok(count > 0)
    }

    /// Build a set of all previously suggested track keys (lowercased "artist:track") for fast lookup.
    pub async fn get_suggested_keys(
        user_id: &str,
    ) -> Result<std::collections::HashSet<String>, String> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT lower(artist), lower(track) FROM discovery_history WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_all(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(a, t)| format!("{}:{}", a, t))
            .collect())
    }
}
