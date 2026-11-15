use crate::server_fns::navidrome::KeptTrack;
use shared::navidrome::BlockedDeletion;
use sqlx::SqlitePool;

/// The tracks auto-delete kept at each user's last rating sync, and why (D11).
pub struct DeletionBlockRow;

impl DeletionBlockRow {
    /// Replaces `user_id`'s rows with `kept`, in one transaction. A song listed twice keeps
    /// its last reason.
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
                "INSERT INTO deletion_blocks (user_id, song_id, title, artist, message) \
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT (user_id, song_id) DO UPDATE SET \
                 title = excluded.title, artist = excluded.artist, message = excluded.message",
            )
            .bind(user_id)
            .bind(&track.song_id)
            .bind(&track.title)
            .bind(&track.artist)
            .bind(&track.failure.message)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
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
