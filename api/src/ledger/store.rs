//! Reads of the ledger, straight from the pool.

use super::writer::import_mode_named;
use super::{IndexWait, JobState, SlotResult, StoredImport, TERMINAL_SQL};
use crate::auth::{Role, UserCtx, UserId};
use crate::models::folder::Folder;
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadableGroup, DownloadableItem, JobSlot,
};
use shared::failure::{Failure, Stage};
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use std::collections::HashMap;
use tracing::warn;

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

/// The last row of every unfinished slot of `user`'s jobs, oldest job first, each with its `job`
/// set. A finished, failed, cancelled or dismissed slot has no row to show again, and neither has
/// a slot with no row yet. A job in a terminal state has only slots in one. A row that cannot be
/// read is logged and left out.
pub async fn open_rows(
    db: &SqlitePool,
    user: &UserId,
) -> Result<Vec<DownloadProgress>, sqlx::Error> {
    // created_at has one-second resolution: insertion order breaks its ties.
    let rows: Vec<(String, u32, String)> = sqlx::query_as(&format!(
        "SELECT t.job_id, t.slot, t.last_row FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE j.user_id = ? AND t.state NOT IN {TERMINAL_SQL} AND t.last_row IS NOT NULL \
         ORDER BY j.created_at, j.rowid, t.slot"
    ))
    .bind(user.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(job, slot, row)| {
            let slot = JobSlot { job, slot };
            match json::<DownloadProgress>(&row) {
                Ok(row) => Some(row.with_job(slot)),
                Err(e) => {
                    warn!("Could not read the stored row of {slot:?}: {e}");
                    None
                }
            }
        })
        .collect())
}

/// The id, state and `wanted` JSON of every job in one of `folder_ids` whose state is not
/// terminal, a state this version does not know included.
pub async fn open_wanted_in(
    db: &SqlitePool,
    folder_ids: &[&str],
) -> Result<Vec<(String, String, String)>, sqlx::Error> {
    let marks = vec!["?"; folder_ids.len()].join(", ");
    let sql = format!(
        "SELECT id, state, wanted FROM jobs WHERE folder_id IN ({marks}) \
         AND state NOT IN {TERMINAL_SQL} ORDER BY created_at, rowid"
    );
    let mut query = sqlx::query_as(&sql);
    for id in folder_ids {
        query = query.bind(*id);
    }
    query.fetch_all(db).await
}

/// A slot waiting for Navidrome to list its import, with its owner, its last row (its `job` set),
/// the wait and the import's result.
pub struct IndexingSlot {
    pub slot: JobSlot,
    pub user: UserId,
    pub row: DownloadProgress,
    pub wait: IndexWait,
    pub result: SlotResult,
}

/// The slots in `indexing`, and those whose row, wait or result cannot be read.
pub struct Indexing {
    /// Oldest job first.
    pub slots: Vec<IndexingSlot>,
    pub unreadable: Vec<(JobSlot, sqlx::Error)>,
}

/// Every slot in `indexing`. One whose row, wait or result cannot be read is in `unreadable`, so
/// the others still wait. Every import that waits stored its result with its wait.
pub async fn indexing_slots(db: &SqlitePool) -> Result<Indexing, sqlx::Error> {
    let rows: Vec<(String, u32, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT t.job_id, t.slot, j.user_id, t.last_row, t.index_wait, t.result \
         FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE t.state = ? AND t.last_row IS NOT NULL AND t.index_wait IS NOT NULL \
         ORDER BY j.created_at, j.rowid, t.slot",
    )
    .bind(JobState::Indexing.as_str())
    .fetch_all(db)
    .await?;
    let mut indexing = Indexing {
        slots: Vec::new(),
        unreadable: Vec::new(),
    };
    for (job, slot, user, row, wait, result) in rows {
        let slot = JobSlot { job, slot };
        let read = || -> Result<_, sqlx::Error> {
            let result = result.ok_or_else(|| sqlx::Error::Decode("no result stored".into()))?;
            Ok((
                json::<DownloadProgress>(&row)?,
                json(&wait)?,
                json(&result)?,
            ))
        };
        match read() {
            Ok((row, wait, result)) => indexing.slots.push(IndexingSlot {
                row: row.with_job(slot.clone()),
                slot,
                user: UserId::new(&user),
                wait,
                result,
            }),
            Err(e) => indexing.unreadable.push((slot, e)),
        }
    }
    Ok(indexing)
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
    /// The backend request a failover sent its newest attempt in; None when that attempt is its
    /// first source, which went in the job's request, or a transfer it adopted.
    pub sent_in: Option<String>,
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
    /// The backend request of the job's latest enqueue.
    pub request: Option<String>,
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
    request: Option<String>,
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

/// The unfinished jobs, and the jobs and slots among them whose stored state cannot be read.
#[derive(Debug, Default)]
pub struct Unfinished {
    /// Oldest first, each job's readable slots in order.
    pub jobs: Vec<UnfinishedJob>,
    /// Jobs whose own row cannot be read, with why; none of them is in `jobs`.
    pub unreadable_jobs: Vec<(String, sqlx::Error)>,
    /// Slots of the jobs in `jobs` whose row, import or attempts cannot be read, with why.
    pub unreadable_slots: Vec<(JobSlot, sqlx::Error)>,
}

/// Every unfinished job, oldest first, its slots in order. `ctx` comes from
/// the job's user row (id, username, role), `folder` from its folder row. A
/// job or slot whose stored state cannot be read is set apart, so the others
/// still resume.
pub async fn unfinished(db: &SqlitePool) -> Result<Unfinished, sqlx::Error> {
    // created_at has one-second resolution: insertion order breaks its ties.
    let jobs: Vec<UnfinishedRow> = sqlx::query_as(&format!(
        "SELECT j.id, j.user_id, u.username, u.role, j.folder_id, f.user_id AS folder_owner, \
                f.name AS folder_name, f.path AS folder_path, f.shared AS folder_shared, \
                j.wanted, j.import_mode, j.state, j.search_id, j.monitor_batch_id, j.batch_label, \
                j.request \
         FROM jobs j JOIN users u ON u.id = j.user_id JOIN folders f ON f.id = j.folder_id \
         WHERE j.state NOT IN {TERMINAL_SQL} ORDER BY j.created_at, j.rowid"
    ))
    .fetch_all(db)
    .await?;
    let slots: Vec<SlotRow> = sqlx::query_as(&format!(
        "SELECT t.job_id, t.slot, t.state, t.last_row, t.import_job \
         FROM job_tracks t JOIN jobs j ON j.id = t.job_id \
         WHERE j.state NOT IN {TERMINAL_SQL} ORDER BY t.job_id, t.slot"
    ))
    .fetch_all(db)
    .await?;
    let attempts: Vec<(String, u32, Option<String>, Option<String>)> = sqlx::query_as(&format!(
        "SELECT a.job_id, a.slot, a.candidate, a.request \
         FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE j.state NOT IN {TERMINAL_SQL} ORDER BY a.job_id, a.slot, a.n"
    ))
    .fetch_all(db)
    .await?;

    let mut unfinished = Unfinished::default();
    // Each slot's attempts, and the request of the newest.
    type Tried = (Vec<Option<DownloadableItem>>, Option<String>);
    let mut tried: HashMap<JobSlot, Result<Tried, sqlx::Error>> = HashMap::new();
    for (job, slot, candidate, request) in attempts {
        let candidate = candidate.as_deref().map(json).transpose();
        let entry = tried
            .entry(JobSlot { job, slot })
            .or_insert(Ok(Default::default()));
        match (entry, candidate) {
            (Ok((attempts, newest)), Ok(candidate)) => {
                attempts.push(candidate);
                *newest = request;
            }
            (entry @ Ok(_), Err(e)) => *entry = Err(e),
            (Err(_), _) => {}
        }
    }
    let mut by_job: HashMap<String, Vec<SlotRecord>> = HashMap::new();
    for row in slots {
        let slot = JobSlot {
            job: row.job_id,
            slot: row.slot,
        };
        let record = (|| {
            let (attempts, sent_in) = tried.remove(&slot).unwrap_or(Ok(Default::default()))?;
            Ok::<_, sqlx::Error>(SlotRecord {
                slot: row.slot,
                state: state_named(&row.state)?,
                row: row.last_row.as_deref().map(json).transpose()?,
                import: row.import_job.as_deref().map(json).transpose()?,
                attempts,
                sent_in,
            })
        })();
        match record {
            Ok(record) => by_job.entry(slot.job).or_default().push(record),
            Err(e) => unfinished.unreadable_slots.push((slot, e)),
        }
    }
    for row in jobs {
        let job = (|| {
            Ok::<_, sqlx::Error>(UnfinishedJob {
                wanted: json(&row.wanted)?,
                import_mode: import_mode_named(&row.import_mode)
                    .map_err(|e| sqlx::Error::Decode(e.0.into()))?,
                state: state_named(&row.state)?,
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
                search_id: row.search_id,
                monitor_batch_id: row.monitor_batch_id,
                batch_label: row.batch_label,
                request: row.request,
                slots: by_job.remove(&row.id).unwrap_or_default(),
                id: row.id.clone(),
            })
        })();
        match job {
            Ok(job) => unfinished.jobs.push(job),
            Err(e) => {
                // Its slots end with the job.
                unfinished
                    .unreadable_slots
                    .retain(|(slot, _)| slot.job != row.id);
                unfinished.unreadable_jobs.push((row.id, e));
            }
        }
    }
    Ok(unfinished)
}

/// One source a slot tried, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    pub n: u32,
    /// None for a transfer the slot adopted.
    pub candidate: Option<DownloadableItem>,
    pub source: Option<String>,
    /// At its transfer's first InProgress row.
    pub started_at: Option<i64>,
    /// When its transfer finished, failed or was cancelled, it was refused, or the slot moved
    /// on to its next attempt.
    pub ended_at: Option<i64>,
    /// The last row of its transfer the ledger stored.
    pub last_row: Option<DownloadProgress>,
    pub failure: Option<Failure>,
}

/// One entry of a job's timeline. `event` is a snake_case name; `facts` holds `slot`, `source`,
/// `item`, `code` and `message` as they apply, text from peers only there.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    pub seq: u32,
    pub at: i64,
    pub stage: Stage,
    pub event: String,
    pub facts: serde_json::Map<String, serde_json::Value>,
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    slot: u32,
    n: u32,
    candidate: Option<String>,
    source: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    last_row: Option<String>,
    failure: Option<String>,
}

/// Each slot of `job` with its attempts, oldest first.
pub async fn attempts_of(
    db: &SqlitePool,
    job: &str,
) -> Result<Vec<(u32, Vec<AttemptRecord>)>, sqlx::Error> {
    let rows: Vec<AttemptRow> = sqlx::query_as(
        "SELECT slot, n, candidate, source, started_at, ended_at, last_row, failure \
         FROM job_attempts WHERE job_id = ? ORDER BY slot, n",
    )
    .bind(job)
    .fetch_all(db)
    .await?;
    let mut slots: Vec<(u32, Vec<AttemptRecord>)> = Vec::new();
    for row in rows {
        let attempt = AttemptRecord {
            n: row.n,
            candidate: row.candidate.as_deref().map(json).transpose()?,
            source: row.source,
            started_at: row.started_at,
            ended_at: row.ended_at,
            last_row: row.last_row.as_deref().map(json).transpose()?,
            failure: row.failure.as_deref().map(json).transpose()?,
        };
        match slots.last_mut() {
            Some((slot, attempts)) if *slot == row.slot => attempts.push(attempt),
            _ => slots.push((row.slot, vec![attempt])),
        }
    }
    Ok(slots)
}

/// `job`'s timeline, in order.
pub async fn timeline_of(db: &SqlitePool, job: &str) -> Result<Vec<TimelineEntry>, sqlx::Error> {
    let rows: Vec<(u32, i64, String, String, String)> = sqlx::query_as(
        "SELECT seq, at, stage, event, facts FROM job_events WHERE job_id = ? ORDER BY seq",
    )
    .bind(job)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(seq, at, stage, event, facts)| {
            Ok(TimelineEntry {
                seq,
                at,
                stage: serde_json::from_value(serde_json::Value::String(stage))
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                event,
                facts: json(&facts)?,
            })
        })
        .collect()
}

/// D5: per source, the attempts of any user that ended at or after `since` with the peer
/// failing the transfer. One slskd account serves every user, and a peer's daily limit of
/// failed transfers is on that account. A slow peer (stuck in its queue), a cancel, a refused
/// enqueue and Soulful's own refusals do not count.
pub async fn recent_failures(
    db: &SqlitePool,
    since: i64,
) -> Result<HashMap<String, u32>, sqlx::Error> {
    let rows: Vec<(String, u32)> = sqlx::query_as(
        "SELECT a.source, COUNT(*) FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE a.ended_at >= ? AND a.source IS NOT NULL \
         AND json_extract(a.failure, '$.stage') = 'transfer' \
         AND json_extract(a.failure, '$.retry') = 'other_source' \
         AND json_extract(a.failure, '$.code') \
             NOT IN ('transfer_stuck_in_remote_queue', 'transfer_cancelled') \
         GROUP BY a.source",
    )
    .bind(since)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
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
    use crate::ledger::{DuplicateRule, JobEvent, JobKind, Ledger, NewJob};
    use crate::server_fns::download::test_support::item;
    use shared::download::DownloadState;
    use soulbeet::DriverChoice;

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
                    picked: vec![],
                    kind: JobKind::Track,
                    options: None,
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
        ledger.record(JobEvent::Enqueued {
            job: "j1".into(),
            monitor_batch_id: "b1".into(),
            batch_label: "Come Over".into(),
            request: "r1".into(),
        });
        ledger.record(JobEvent::Attempt {
            slot: slot(0),
            candidate: None,
            request: None,
        });
        let two_elsewhere = item("peer_b", "Two", "b/02.flac");
        ledger.record(JobEvent::Attempt {
            slot: slot(1),
            candidate: Some(Box::new(two_elsewhere.clone())),
            request: Some("r2".into()),
        });
        let import = StoredImport {
            sources: vec!["/downloads/b/02.flac".into()],
            target: "/music".into(),
            album: false,
            cleanup_root: None,
            choices: None,
            choice: DriverChoice::Auto,
            duplicates: DuplicateRule::Guarded,
            rows: vec![(
                slot(1),
                DownloadProgress::queued("t-02".into(), "peer_a".into(), "02.flac".into(), 100)
                    .with_state(DownloadState::ImportQueued(1)),
            )],
        };
        ledger.record(JobEvent::ImportQueued(Box::new(import.clone())));
        ledger.settled().await;

        let jobs = unfinished(&db).await.unwrap().jobs;
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
        assert_eq!(jobs[0].request.as_deref(), Some("r1"));
        let slots = &jobs[0].slots;
        assert_eq!(
            slots
                .iter()
                .map(|s| (s.slot, s.state, s.attempts.clone(), s.sent_in.clone()))
                .collect::<Vec<_>>(),
            vec![
                (0, JobState::Queued, vec![Some(one), None], None),
                (
                    1,
                    JobState::Importing,
                    vec![Some(item("peer_a", "Two", "02.flac")), Some(two_elsewhere)],
                    Some("r2".into())
                ),
            ]
        );
        assert_eq!(slots[1].import, Some(import));

        ledger.record(JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        });
        ledger.settled().await;
        assert!(unfinished(&db).await.unwrap().jobs.is_empty());
    }
}
