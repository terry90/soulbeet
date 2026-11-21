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

    /// Every track ever suggested to the user, as `seen_key`s.
    pub async fn get_suggested_keys(
        user_id: &str,
    ) -> Result<std::collections::HashSet<String>, String> {
        Self::get_suggested_keys_in(&DB, user_id).await
    }

    pub(crate) async fn get_suggested_keys_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<std::collections::HashSet<String>, String> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT artist, track FROM discovery_history WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.iter().map(|(a, t)| seen_key(a, t)).collect())
    }
}

/// The key a Discovery run dedupes a track by. History, deletions and candidates all
/// go through here, so their keys match. Folded in Rust: SQLite's lower() folds ASCII
/// letters only.
pub fn seen_key(artist: &str, track: &str) -> String {
    format!("{}:{}", artist.to_lowercase(), track.to_lowercase())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    async fn suggest(db: &sqlx::SqlitePool, artist: &str, track: &str) {
        sqlx::query(
            "INSERT INTO discovery_history (id, user_id, artist, track, profile) \
             VALUES (?, 'alice', ?, ?, 'Conservative')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(artist)
        .bind(track)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_suggested_track_is_seen_under_its_candidate_key_in_any_script() {
        let db = test_db::migrated().await;
        let tracks = [
            ("Мумий Тролль", "Контрабанды"),
            ("Till Lindemann", "Übers Meer"),
            ("Marrow Choir", "O Holy Night"),
        ];
        for (artist, track) in tracks {
            suggest(&db, artist, track).await;
        }

        let seen = DiscoveryHistoryRow::get_suggested_keys_in(&db, "alice")
            .await
            .unwrap();

        for (artist, track) in tracks {
            assert!(
                seen.contains(&seen_key(artist, track)),
                "{artist} - {track} not in {seen:?}"
            );
        }
        // The engine may name it in another case.
        assert!(seen.contains(&seen_key("МУМИЙ ТРОЛЛЬ", "КОНТРАБАНДЫ")));
    }

    #[test]
    fn seen_key_folds_letters_of_any_script() {
        assert_eq!(
            seen_key("Мумий Тролль", "Контрабанды"),
            "мумий тролль:контрабанды"
        );
        assert_eq!(
            seen_key("Till Lindemann", "Übers Meer"),
            "till lindemann:übers meer"
        );
        assert_eq!(
            seen_key("Marrow Choir", "O Holy Night"),
            "marrow choir:o holy night"
        );
    }
}
