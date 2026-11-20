use std::time::Duration;

use async_trait::async_trait;
use soulbeet::knowledge::CacheError;
use soulbeet::traits::KnowledgeCache;
use sqlx::SqlitePool;

/// Facts fetched from music services, kept in SQLite for every user of the install.
pub struct SqliteKnowledgeCache {
    db: SqlitePool,
}

impl SqliteKnowledgeCache {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn cache_error(e: sqlx::Error) -> CacheError {
    CacheError(e.to_string())
}

#[async_trait]
impl KnowledgeCache for SqliteKnowledgeCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        sqlx::query_scalar("SELECT value FROM knowledge_cache WHERE key = ? AND expires_at > ?")
            .bind(key)
            .bind(now())
            .fetch_optional(&self.db)
            .await
            .map_err(cache_error)
    }

    async fn put(&self, key: &str, value: &str, ttl: Duration) -> Result<(), CacheError> {
        sqlx::query(
            "INSERT INTO knowledge_cache (key, value, expires_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
        )
        .bind(key)
        .bind(value)
        .bind(now() + ttl.as_secs() as i64)
        .execute(&self.db)
        .await
        .map_err(cache_error)?;
        Ok(())
    }
}

/// Deletes expired facts and returns how many went.
pub async fn prune_expired_in(db: &SqlitePool) -> Result<u64, String> {
    sqlx::query("DELETE FROM knowledge_cache WHERE expires_at <= ?")
        .bind(now())
        .execute(db)
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn a_fact_reads_back_until_it_expires_and_a_new_value_replaces_it() {
        let db = test_db::migrated().await;
        let cache = SqliteKnowledgeCache::new(db.clone());
        cache
            .put("kc1:a", "[1]", Duration::from_secs(86_400))
            .await
            .unwrap();
        cache
            .put("kc1:a", "[2]", Duration::from_secs(86_400))
            .await
            .unwrap();
        assert_eq!(cache.get("kc1:a").await.unwrap().as_deref(), Some("[2]"));
        cache.put("kc1:b", "[3]", Duration::ZERO).await.unwrap();
        assert_eq!(cache.get("kc1:b").await.unwrap(), None);
    }

    #[tokio::test]
    async fn pruning_deletes_only_expired_facts() {
        let db = test_db::migrated().await;
        let cache = SqliteKnowledgeCache::new(db.clone());
        cache.put("kc1:old", "1", Duration::ZERO).await.unwrap();
        cache
            .put("kc1:new", "1", Duration::from_secs(86_400))
            .await
            .unwrap();
        assert_eq!(prune_expired_in(&db).await.unwrap(), 1);
        assert!(cache.get("kc1:new").await.unwrap().is_some());
    }
}
