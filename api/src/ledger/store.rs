//! Reads of the ledger, straight from the pool.

use super::{IndexWait, JobState, TERMINAL};
use crate::auth::UserId;
use shared::download::{DownloadProgress, DownloadQuery, DownloadableGroup, JobSlot};
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

/// The last row of every slot, not dismissed, of `user`'s unfinished jobs, oldest job first,
/// each with its `job` set. A slot with no row yet has none to show.
pub async fn open_rows(
    db: &SqlitePool,
    user: &UserId,
) -> Result<Vec<DownloadProgress>, sqlx::Error> {
    let terminal = vec!["?"; TERMINAL.len()].join(", ");
    // created_at has one-second resolution: insertion order breaks its ties.
    let open = format!(
        "SELECT t.job_id, t.slot, t.last_row FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE j.user_id = ? AND j.state NOT IN ({terminal}) AND t.state != ? \
           AND t.last_row IS NOT NULL \
         ORDER BY j.created_at, j.rowid, t.slot"
    );
    let rows: Vec<(String, u32, String)> = TERMINAL
        .iter()
        .fold(
            sqlx::query_as(&open).bind(user.to_string()),
            |query, state| query.bind(state.as_str()),
        )
        .bind(JobState::Dismissed.as_str())
        .fetch_all(db)
        .await?;
    rows.into_iter()
        .map(|(job, slot, row)| Ok(json::<DownloadProgress>(&row)?.with_job(JobSlot { job, slot })))
        .collect()
}

/// A slot waiting for Navidrome to list its import, with its owner, its last row (its `job` set)
/// and the wait.
pub struct IndexingSlot {
    pub slot: JobSlot,
    pub user: UserId,
    pub row: DownloadProgress,
    pub wait: IndexWait,
}

/// Every slot in `indexing`, oldest job first.
pub async fn indexing_slots(db: &SqlitePool) -> Result<Vec<IndexingSlot>, sqlx::Error> {
    let rows: Vec<(String, u32, String, String, String)> = sqlx::query_as(
        "SELECT t.job_id, t.slot, j.user_id, t.last_row, t.index_wait \
         FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE t.state = ? AND t.last_row IS NOT NULL AND t.index_wait IS NOT NULL \
         ORDER BY j.created_at, j.rowid, t.slot",
    )
    .bind(JobState::Indexing.as_str())
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(job, slot, user, row, wait)| {
            let slot = JobSlot { job, slot };
            Ok(IndexingSlot {
                row: json::<DownloadProgress>(&row)?.with_job(slot.clone()),
                slot,
                user: UserId::new(&user),
                wait: json(&wait)?,
            })
        })
        .collect()
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
