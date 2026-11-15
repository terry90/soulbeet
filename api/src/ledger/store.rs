//! Reads of the ledger, straight from the pool.

#[cfg(test)]
use super::JobState;
use crate::auth::UserId;
use shared::download::{DownloadQuery, DownloadableGroup};
use sqlx::SqlitePool;

/// A source search as stored: what it looked for, where, and what it found.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSearch {
    pub id: String,
    pub backend: Option<String>,
    pub query: DownloadQuery,
    /// None until the search ended.
    pub candidates: Option<Vec<DownloadableGroup>>,
    pub expires_at: i64,
}

fn json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, sqlx::Error> {
    serde_json::from_str(text).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

/// `user`'s search `id`; None for an unknown id or another user's.
pub async fn search_of(
    db: &SqlitePool,
    id: &str,
    user: &UserId,
) -> Result<Option<StoredSearch>, sqlx::Error> {
    let row: Option<(Option<String>, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT backend, query, candidates, expires_at FROM searches WHERE id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(user.to_string())
    .fetch_optional(db)
    .await?;
    row.map(|(backend, query, candidates, expires_at)| {
        Ok(StoredSearch {
            id: id.to_string(),
            backend,
            query: json(&query)?,
            candidates: candidates.as_deref().map(json).transpose()?,
            expires_at,
        })
    })
    .transpose()
}

#[cfg(test)]
pub async fn job_state(db: &SqlitePool, job: &str) -> Option<JobState> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
        .bind(job)
        .fetch_optional(db)
        .await
        .unwrap();
    state.map(|s| JobState::parse(&s).expect("a stored job state"))
}
