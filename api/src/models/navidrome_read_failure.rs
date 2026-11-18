use shared::navidrome::UnreadableListener;
use sqlx::SqlitePool;

/// The accounts whose Navidrome ratings the delete guard could not read (D10).
pub struct NavidromeReadFailureRow;

impl NavidromeReadFailureRow {
    /// Records that reading `user_id` failed with `error`, keeping when the failures began.
    pub(crate) async fn record_in(
        db: &SqlitePool,
        user_id: &str,
        error: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO navidrome_read_failures (user_id, error) VALUES (?, ?) \
             ON CONFLICT (user_id) DO UPDATE SET error = excluded.error",
        )
        .bind(user_id)
        .bind(error)
        .execute(db)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    /// `user_id` was read.
    pub(crate) async fn clear_in(db: &SqlitePool, user_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM navidrome_read_failures WHERE user_id = ?")
            .bind(user_id)
            .execute(db)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Every account that cannot be read, the longest failing first.
    pub(crate) async fn all_in(db: &SqlitePool) -> Result<Vec<UnreadableListener>, String> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT u.username, f.error, f.since FROM navidrome_read_failures f \
             JOIN users u ON u.id = f.user_id ORDER BY f.since, u.username",
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(username, error, since)| UnreadableListener {
                username,
                error,
                since,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failure_keeps_when_it_began_until_a_read_clears_it() {
        let db = crate::db::test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('bob', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        NavidromeReadFailureRow::record_in(&db, "bob", "Navidrome request failed: timed out")
            .await
            .unwrap();
        sqlx::query("UPDATE navidrome_read_failures SET since = '2026-11-14 08:00:00'")
            .execute(&db)
            .await
            .unwrap();
        NavidromeReadFailureRow::record_in(
            &db,
            "bob",
            "Navidrome appears unavailable (circuit breaker open)",
        )
        .await
        .unwrap();
        assert_eq!(
            NavidromeReadFailureRow::all_in(&db).await.unwrap(),
            vec![UnreadableListener {
                username: "bob".into(),
                error: "Navidrome appears unavailable (circuit breaker open)".into(),
                since: "2026-11-14 08:00:00".into(),
            }]
        );
        NavidromeReadFailureRow::clear_in(&db, "bob").await.unwrap();
        assert!(NavidromeReadFailureRow::all_in(&db)
            .await
            .unwrap()
            .is_empty());
    }
}
