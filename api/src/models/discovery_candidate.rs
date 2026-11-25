use serde::{Deserialize, Serialize};
use shared::recommendation::Candidate;

#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct DiscoveryCandidateRow {
    pub id: String,
    pub user_id: String,
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub score: f64,
    pub signals: String,
    pub source: String,
    pub profile: String,
    pub used: bool,
    pub created_at: String,
}

impl From<DiscoveryCandidateRow> for Candidate {
    fn from(row: DiscoveryCandidateRow) -> Self {
        Candidate {
            artist: row.artist,
            track: row.track,
            album: row.album,
            mbid: None,
            score: row.score,
            signals: serde_json::from_str(&row.signals).unwrap_or_default(),
            source: row.source,
            artist_listener_count: None,
            primary_genre: None,
            release_year: None,
            artist_mbid: None,
            reason_label: None,
            culture_bucket: None,
        }
    }
}

#[cfg(feature = "server")]
impl DiscoveryCandidateRow {
    pub(crate) async fn upsert_batch_in(
        conn: &mut sqlx::SqliteConnection,
        user_id: &str,
        profile: &str,
        candidates: &[Candidate],
    ) -> Result<(), String> {
        for c in candidates {
            let id = Uuid::new_v4().to_string();
            let signals = serde_json::to_string(&c.signals).unwrap_or_default();
            sqlx::query(
                "INSERT INTO discovery_candidates (id, user_id, artist, track, album, score, signals, source, profile)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(user_id, profile, artist, track) DO UPDATE SET
                   score = excluded.score, signals = excluded.signals, source = excluded.source,
                   created_at = datetime('now')",
            )
            .bind(&id)
            .bind(user_id)
            .bind(&c.artist)
            .bind(&c.track)
            .bind(&c.album)
            .bind(c.score)
            .bind(&signals)
            .bind(&c.source)
            .bind(profile)
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub async fn get_unused(
        user_id: &str,
        profile: &str,
        limit: u32,
    ) -> Result<Vec<Candidate>, String> {
        Self::get_unused_in(&DB, user_id, profile, limit).await
    }

    pub(crate) async fn get_unused_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
        profile: &str,
        limit: u32,
    ) -> Result<Vec<Candidate>, String> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT * FROM discovery_candidates WHERE user_id = ? AND profile = ? AND used = 0 ORDER BY score DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(profile)
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn mark_used(
        user_id: &str,
        profile: &str,
        artist: &str,
        track: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE discovery_candidates SET used = 1 WHERE user_id = ? AND profile = ? AND lower(artist) = lower(?) AND lower(track) = lower(?)",
        )
        .bind(user_id)
        .bind(profile)
        .bind(artist)
        .bind(track)
        .execute(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn clear_for_user(user_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM discovery_candidates WHERE user_id = ?")
            .bind(user_id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) async fn clear_for_user_profile_in(
        conn: &mut sqlx::SqliteConnection,
        user_id: &str,
        profile: &str,
    ) -> Result<(), String> {
        sqlx::query("DELETE FROM discovery_candidates WHERE user_id = ? AND profile = ?")
            .bind(user_id)
            .bind(profile)
            .execute(conn)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
