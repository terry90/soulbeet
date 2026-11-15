//! Reads of the ledger, straight from the pool.

use super::writer::import_mode_named;
use super::{IndexWait, JobState, StoredImport, TERMINAL};
use crate::auth::{Role, UserCtx, UserId};
use crate::models::folder::Folder;
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadableGroup, DownloadableItem, JobSlot,
};
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use std::collections::HashMap;

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

/// One slot of an unfinished job as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotRecord {
    pub slot: u32,
    pub state: JobState,
    pub row: Option<DownloadProgress>,
    pub import: Option<StoredImport>,
    /// In order; None for an adopted transfer.
    pub attempts: Vec<Option<DownloadableItem>>,
}

/// A job not in a terminal state, with the user and folder it runs for (a job whose user or folder
/// is gone was deleted with them, D13).
#[derive(Debug)]
pub struct UnfinishedJob {
    pub id: String,
    pub ctx: UserCtx,
    pub folder: Folder,
    pub wanted: DownloadQuery,
    pub import_mode: ImportMode,
    pub state: JobState,
    pub search_id: Option<String>,
    pub monitor_batch_id: Option<String>,
    pub batch_label: Option<String>,
    pub slots: Vec<SlotRecord>,
}

#[derive(sqlx::FromRow)]
struct UnfinishedRow {
    id: String,
    user_id: String,
    username: String,
    role: Role,
    folder_id: String,
    folder_owner: String,
    folder_name: String,
    folder_path: String,
    folder_shared: bool,
    wanted: String,
    import_mode: String,
    state: String,
    search_id: Option<String>,
    monitor_batch_id: Option<String>,
    batch_label: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SlotRow {
    job_id: String,
    slot: u32,
    state: String,
    last_row: Option<String>,
    import_job: Option<String>,
}

fn state_named(name: &str) -> Result<JobState, sqlx::Error> {
    JobState::parse(name)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown job state {name:?}").into()))
}

/// Every unfinished job, oldest first, its slots in order. `ctx` comes from
/// the job's user row (id, username, role), `folder` from its folder row.
pub async fn unfinished(db: &SqlitePool) -> Result<Vec<UnfinishedJob>, sqlx::Error> {
    let terminal = vec!["?"; TERMINAL.len()].join(", ");
    // created_at has one-second resolution: insertion order breaks its ties.
    let jobs = format!(
        "SELECT j.id, j.user_id, u.username, u.role, j.folder_id, f.user_id AS folder_owner, \
                f.name AS folder_name, f.path AS folder_path, f.shared AS folder_shared, \
                j.wanted, j.import_mode, j.state, j.search_id, j.monitor_batch_id, j.batch_label \
         FROM jobs j JOIN users u ON u.id = j.user_id JOIN folders f ON f.id = j.folder_id \
         WHERE j.state NOT IN ({terminal}) ORDER BY j.created_at, j.rowid"
    );
    let jobs: Vec<UnfinishedRow> = TERMINAL
        .iter()
        .fold(sqlx::query_as(&jobs), |query, state| {
            query.bind(state.as_str())
        })
        .fetch_all(db)
        .await?;
    let slots = format!(
        "SELECT t.job_id, t.slot, t.state, t.last_row, t.import_job \
         FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE j.state NOT IN ({terminal}) ORDER BY t.job_id, t.slot"
    );
    let slots: Vec<SlotRow> = TERMINAL
        .iter()
        .fold(sqlx::query_as(&slots), |query, state| {
            query.bind(state.as_str())
        })
        .fetch_all(db)
        .await?;
    let attempts = format!(
        "SELECT a.job_id, a.slot, a.candidate FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE j.state NOT IN ({terminal}) ORDER BY a.job_id, a.slot, a.n"
    );
    let attempts: Vec<(String, u32, Option<String>)> = TERMINAL
        .iter()
        .fold(sqlx::query_as(&attempts), |query, state| {
            query.bind(state.as_str())
        })
        .fetch_all(db)
        .await?;

    let mut tried: HashMap<(String, u32), Vec<Option<DownloadableItem>>> = HashMap::new();
    for (job, slot, candidate) in attempts {
        tried
            .entry((job, slot))
            .or_default()
            .push(candidate.as_deref().map(json).transpose()?);
    }
    let mut by_job: HashMap<String, Vec<SlotRecord>> = HashMap::new();
    for row in slots {
        let record = SlotRecord {
            slot: row.slot,
            state: state_named(&row.state)?,
            row: row.last_row.as_deref().map(json).transpose()?,
            import: row.import_job.as_deref().map(json).transpose()?,
            attempts: tried
                .remove(&(row.job_id.clone(), row.slot))
                .unwrap_or_default(),
        };
        by_job.entry(row.job_id).or_default().push(record);
    }
    jobs.into_iter()
        .map(|row| {
            Ok(UnfinishedJob {
                ctx: UserCtx {
                    user_id: row.user_id,
                    username: row.username,
                    role: row.role,
                },
                folder: Folder {
                    id: row.folder_id,
                    user_id: row.folder_owner,
                    name: row.folder_name,
                    path: row.folder_path,
                    shared: row.folder_shared,
                },
                wanted: json(&row.wanted)?,
                import_mode: import_mode_named(&row.import_mode)
                    .map_err(|e| sqlx::Error::Decode(e.0.into()))?,
                state: state_named(&row.state)?,
                search_id: row.search_id,
                monitor_batch_id: row.monitor_batch_id,
                batch_label: row.batch_label,
                slots: by_job.remove(&row.id).unwrap_or_default(),
                id: row.id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{JobEvent, Ledger, NewJob};
    use crate::server_fns::download::test_support::item;
    use shared::download::DownloadState;

    #[tokio::test]
    async fn unfinished_jobs_come_back_with_their_slots_imports_and_attempts() {
        let db = crate::db::test_db::migrated().await;
        let folder = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id: folder.clone(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                },
                None,
            )
            .await
            .unwrap();
        let one = item("peer_a", "One", "01.flac");
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, one.clone()), (1, item("peer_a", "Two", "02.flac"))],
        });
        let slot = |slot| JobSlot {
            job: "j1".into(),
            slot,
        };
        ledger.record(JobEvent::Attempt {
            slot: slot(0),
            candidate: None,
        });
        let import = StoredImport {
            sources: vec!["/downloads/b/02.flac".into()],
            target: "/music".into(),
            album: false,
            cleanup_root: None,
            choices: None,
            rows: vec![(
                slot(1),
                DownloadProgress::queued("t-02".into(), "peer_a".into(), "02.flac".into(), 100)
                    .with_state(DownloadState::ImportQueued(1)),
            )],
        };
        ledger.record(JobEvent::ImportQueued(Box::new(import.clone())));
        ledger.settled().await;

        let jobs = unfinished(&db).await.unwrap();
        assert_eq!(
            jobs.iter()
                .map(|j| (
                    j.id.as_str(),
                    j.ctx.username.as_str(),
                    j.folder.id.as_str(),
                    j.state
                ))
                .collect::<Vec<_>>(),
            vec![("j1", "alice", folder.as_str(), JobState::Queued)]
        );
        let slots = &jobs[0].slots;
        assert_eq!(
            slots
                .iter()
                .map(|s| (s.slot, s.state, s.attempts.clone()))
                .collect::<Vec<_>>(),
            vec![
                (0, JobState::Queued, vec![Some(one), None]),
                (
                    1,
                    JobState::Importing,
                    vec![Some(item("peer_a", "Two", "02.flac"))]
                ),
            ]
        );
        assert_eq!(slots[1].import, Some(import));

        ledger.record(JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        });
        ledger.settled().await;
        assert!(unfinished(&db).await.unwrap().is_empty());
    }
}
