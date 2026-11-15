#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use shared::navidrome::{DeletionReason, DeletionReview};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

/// A row of the deletion audit.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct DeletionReviewRow {
    pub id: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: Option<String>,
    pub rating: Option<i32>,
    pub reason: String,
    pub created_at: String,
}

/// One deletion, as the guard records it.
#[cfg(feature = "server")]
#[derive(Clone, Debug)]
pub struct DeletionRecord {
    /// The row's id, chosen before the removal so a removal that lost track of itself can
    /// record it once.
    pub id: String,
    pub user_id: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    /// The caller's own rating, None when unrated.
    pub rating: Option<u8>,
    /// What started the removal: "ui" or "auto".
    pub origin: &'static str,
    pub reason: DeletionReason,
}

#[cfg(feature = "server")]
impl TryFrom<DeletionReviewRow> for DeletionReview {
    type Error = String;

    fn try_from(row: DeletionReviewRow) -> Result<Self, String> {
        let reason = serde_json::from_value(serde_json::Value::String(row.reason))
            .map_err(|e| format!("Deletion {} has an unknown reason: {e}", row.id))?;
        Ok(DeletionReview {
            id: row.id,
            song_id: row.song_id,
            title: row.title,
            artist: row.artist,
            album: row.album,
            path: row.path,
            rating: row.rating.map(|r| r as u8),
            reason,
            created_at: row.created_at,
        })
    }
}

#[cfg(feature = "server")]
impl DeletionReviewRow {
    /// Records `record` on `conn` (inside the caller's transaction, when it has one), unless a
    /// row with its id is there already.
    pub(crate) async fn insert_in(
        conn: &mut sqlx::SqliteConnection,
        record: &DeletionRecord,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, path, rating, origin, reason)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&record.id)
        .bind(&record.user_id)
        .bind(&record.song_id)
        .bind(&record.title)
        .bind(&record.artist)
        .bind(&record.album)
        .bind(&record.path)
        .bind(record.rating.map(i32::from))
        .bind(record.origin)
        .bind(record.reason.as_str())
        .execute(conn)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn history(user_id: &str, limit: u32) -> Result<Vec<DeletionReview>, String> {
        Self::history_in(&DB, user_id, limit).await
    }

    /// The user's latest `limit` deletions, newest first.
    pub(crate) async fn history_in(
        db: &SqlitePool,
        user_id: &str,
        limit: u32,
    ) -> Result<Vec<DeletionReview>, String> {
        sqlx::query_as::<_, DeletionReviewRow>(
            "SELECT id, song_id, title, artist, album, path, rating, reason, created_at
             FROM deletion_reviews WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(DeletionReview::try_from)
        .collect()
    }

    /// "artist:title" lowercased, of the user's tracks deleted as disliked.
    pub async fn disliked_keys(user_id: &str) -> Result<Vec<String>, String> {
        Self::disliked_keys_in(&DB, user_id).await
    }

    pub(crate) async fn disliked_keys_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<String>, String> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT artist, title FROM deletion_reviews WHERE user_id = ? AND reason = ?",
        )
        .bind(user_id)
        .bind(DeletionReason::Disliked.as_str())
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(artist, title)| format!("{}:{}", artist.to_lowercase(), title.to_lowercase()))
            .collect())
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn a_deletion_recorded_before_the_upgrade_reads_as_a_disliked_auto_deletion() {
        let db = test_db::migrated_before(test_db::DELETION_AUDIT).await;
        sqlx::query(
            "INSERT INTO deletion_reviews (id, song_id, title, artist, album, path, rating, action, user_id) \
             VALUES ('r1', 's1', 'O Holy Night', 'Marrow Choir', 'Winter Songs', '/music/x.flac', 1, 'Deleted', 'alice')",
        )
        .execute(&db)
        .await
        .unwrap();
        test_db::apply(&db, test_db::DELETION_AUDIT).await;
        let history = DeletionReviewRow::history_in(&db, "alice", 10)
            .await
            .unwrap();
        assert_eq!(
            (history[0].song_id.as_str(), history[0].reason),
            ("s1", DeletionReason::Disliked)
        );
    }

    #[tokio::test]
    async fn discovery_excludes_only_tracks_deleted_as_disliked() {
        let db = test_db::migrated().await;
        let mut conn = db.acquire().await.unwrap();
        for (title, reason) in [
            ("O Holy Night", DeletionReason::Disliked),
            ("Silent Night", DeletionReason::Replaced),
        ] {
            DeletionReviewRow::insert_in(
                &mut conn,
                &DeletionRecord {
                    id: title.to_string(),
                    user_id: "alice".into(),
                    song_id: "s".into(),
                    title: title.into(),
                    artist: "Marrow Choir".into(),
                    album: "Winter Songs".into(),
                    path: "/music/x.flac".into(),
                    rating: Some(1),
                    origin: "auto",
                    reason,
                },
            )
            .await
            .unwrap();
        }
        drop(conn);
        assert_eq!(
            DeletionReviewRow::disliked_keys_in(&db, "alice")
                .await
                .unwrap(),
            vec!["marrow choir:o holy night".to_string()]
        );
        assert_eq!(
            DeletionReviewRow::history_in(&db, "alice", 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
