use shared::failure::Failure;
use shared::navidrome::BlockedDeletion;
use sqlx::SqlitePool;

/// A 1-star song auto-delete kept, and the refusal that kept it.
pub(crate) struct KeptTrack {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub failure: Failure,
}

/// The tracks auto-delete kept at each user's last rating sync, and why (D11): the refusal's
/// code and message.
pub struct DeletionBlockRow;

impl DeletionBlockRow {
    /// Replaces `user_id`'s rows with `kept`, in one transaction. Rows are stored only while
    /// auto-delete is on for the user, so a sync that ends after it was switched off leaves
    /// none. A song listed twice keeps its last reason.
    pub(crate) async fn replace_in(
        db: &SqlitePool,
        user_id: &str,
        kept: &[KeptTrack],
    ) -> Result<(), String> {
        let mut tx = db.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM deletion_blocks WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        for track in kept {
            sqlx::query(
                "INSERT INTO deletion_blocks (user_id, song_id, title, artist, code, message) \
                 SELECT ?, ?, ?, ?, ?, ? FROM user_settings \
                 WHERE user_id = ? AND auto_delete_enabled \
                 ON CONFLICT (user_id, song_id) DO UPDATE SET title = excluded.title, \
                 artist = excluded.artist, code = excluded.code, message = excluded.message",
            )
            .bind(user_id)
            .bind(&track.song_id)
            .bind(&track.title)
            .bind(&track.artist)
            .bind(track.failure.code.as_str())
            .bind(&track.failure.message)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Drops `user_id`'s rows, on `conn`.
    pub(crate) async fn clear_in(
        conn: &mut sqlx::SqliteConnection,
        user_id: &str,
    ) -> Result<(), String> {
        sqlx::query("DELETE FROM deletion_blocks WHERE user_id = ?")
            .bind(user_id)
            .execute(conn)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// `user_id`'s kept tracks, by artist then title.
    pub(crate) async fn for_user_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<BlockedDeletion>, String> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT title, artist, message FROM deletion_blocks WHERE user_id = ? \
             ORDER BY artist, title",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(title, artist, message)| BlockedDeletion {
                title,
                artist,
                message,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};
    use shared::failure::{FailureCode, RetryAdvice, Stage};

    fn kept(song_id: &str) -> KeptTrack {
        KeptTrack {
            song_id: song_id.into(),
            title: "Silent Night".into(),
            artist: "Marrow Choir".into(),
            failure: Failure::new(
                FailureCode::DeleteBlockedByOtherUser,
                Stage::Delete,
                RetryAdvice::NotRetryable,
                "Another listener rated, starred or kept this track in a playlist",
            ),
        }
    }

    async fn with_auto_delete(db: &SqlitePool, on: bool) {
        UserSettings::upsert_in(
            db,
            "alice",
            UpdateUserSettings {
                auto_delete_enabled: Some(on),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    async fn stored(db: &SqlitePool) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT song_id, code, message FROM deletion_blocks WHERE user_id = 'alice'")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_kept_track_is_stored_with_its_code_while_auto_delete_is_on() {
        let db = test_db::migrated().await;
        sqlx::query(
            "INSERT INTO users (id, username, password_hash) VALUES ('alice', 'alice', 'x')",
        )
        .execute(&db)
        .await
        .unwrap();
        with_auto_delete(&db, true).await;
        DeletionBlockRow::replace_in(&db, "alice", &[kept("s2")])
            .await
            .unwrap();
        assert_eq!(
            stored(&db).await,
            vec![(
                "s2".to_string(),
                "delete_blocked_by_other_user".to_string(),
                "Another listener rated, starred or kept this track in a playlist".to_string()
            )]
        );
        // Another setting changed: the rows stay.
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                discovery_promote_threshold: Some(4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(stored(&db).await.len(), 1);
        // Switched off: they go, and a sync that ends afterwards stores none.
        with_auto_delete(&db, false).await;
        assert!(stored(&db).await.is_empty());
        DeletionBlockRow::replace_in(&db, "alice", &[kept("s2")])
            .await
            .unwrap();
        assert!(stored(&db).await.is_empty());
        assert!(DeletionBlockRow::for_user_in(&db, "alice")
            .await
            .unwrap()
            .is_empty());
    }
}
