use sqlx::SqlitePool;

/// Session tokens ended by signing out. A row is kept until the token would
/// have expired anyway, so the table holds only tokens that still verify.
pub struct RevokedSession;

impl RevokedSession {
    /// Ends the token `jti`, valid until `expires_at` (Unix seconds), and
    /// forgets the tokens that expired before `now`.
    pub(crate) async fn revoke_in(
        db: &SqlitePool,
        jti: &str,
        expires_at: i64,
        now: i64,
    ) -> Result<(), String> {
        let mut tx = db.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM revoked_sessions WHERE expires_at < ?")
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("INSERT OR IGNORE INTO revoked_sessions (jti, expires_at) VALUES (?, ?)")
            .bind(jti)
            .bind(expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    pub(crate) async fn is_revoked_in(db: &SqlitePool, jti: &str) -> Result<bool, String> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM revoked_sessions WHERE jti = ?)")
            .bind(jti)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn a_revoked_token_stays_revoked_until_it_expires() {
        let db = test_db::migrated().await;
        RevokedSession::revoke_in(&db, "a", 100, 50).await.unwrap();
        assert!(RevokedSession::is_revoked_in(&db, "a").await.unwrap());
        assert!(!RevokedSession::is_revoked_in(&db, "b").await.unwrap());

        // The next sign-out after `a` expired forgets it.
        RevokedSession::revoke_in(&db, "b", 300, 200).await.unwrap();
        assert!(!RevokedSession::is_revoked_in(&db, "a").await.unwrap());
        assert!(RevokedSession::is_revoked_in(&db, "b").await.unwrap());
    }
}
