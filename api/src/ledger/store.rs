//! Reads of the ledger, straight from the pool.

use super::writer::import_mode_named;
use super::{
    BatchOrigin, IndexWait, JobKind, JobOptions, JobState, SlotResult, StoredImport, StoredSpec,
    TERMINAL_SQL,
};
use crate::auth::{Role, UserCtx, UserId};
use crate::models::folder::Folder;
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadableGroup, DownloadableItem, JobSlot, SearchStats,
};
use shared::failure::{Failure, Stage};
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
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
    /// What an MCP search was asked for; None for a web UI search.
    pub spec: Option<StoredSpec>,
    /// What an MCP search read, once it ended; None when it failed before the backend finished.
    pub stats: Option<SearchStats>,
    /// Why an MCP search found nothing.
    pub failure: Option<Failure>,
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
    type Row = (
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT backend, query, candidates, expires_at, spec, stats, failure FROM searches \
         WHERE id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(user.to_string())
    .fetch_optional(db)
    .await?;
    row.map(
        |(backend, query, candidates, expires_at, spec, stats, failure)| {
            Ok(StoredSearch {
                id: id.to_string(),
                backend,
                query: json(&query)?,
                candidates: candidates.as_deref().map(json).transpose()?,
                expires_at,
                spec: spec.as_deref().map(json).transpose()?,
                stats: stats.as_deref().map(json).transpose()?,
                failure: failure.as_deref().map(json).transpose()?,
            })
        },
    )
    .transpose()
}

/// A job as stored, with the user and folder it runs for.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub ctx: UserCtx,
    pub folder: Folder,
    pub batch_id: String,
    pub origin: BatchOrigin,
    pub kind: JobKind,
    pub wanted: DownloadQuery,
    /// None for a web UI job.
    pub options: Option<JobOptions>,
    pub state: JobState,
    pub search_id: Option<String>,
    /// Sources whose result the user rejected, never offered to the job again.
    pub excluded_sources: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub changed_seq: i64,
    /// The failure of a job that failed before it had a slot (its tracklist or its search).
    pub failure: Option<Failure>,
    /// Its current pick's send is recorded (a monitor batch or a backend request): the files are
    /// a monitor's, or reconciliation's after a restart, never the job runner's to send again.
    pub sent: bool,
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: String,
    user_id: String,
    username: String,
    role: Role,
    folder_id: String,
    folder_owner: String,
    folder_name: String,
    folder_path: String,
    folder_shared: bool,
    batch_id: String,
    origin: String,
    kind: String,
    wanted: String,
    options: Option<String>,
    state: String,
    search_id: Option<String>,
    excluded_sources: String,
    created_at: i64,
    updated_at: i64,
    changed_seq: i64,
    failure: Option<String>,
    sent: bool,
}

/// The query of a JobRow, from `jobs j` joined with its user `u`, folder `f` and batch `b`.
const JOB_SELECT: &str = "SELECT j.id, j.user_id, u.username, u.role, j.folder_id, \
            f.user_id AS folder_owner, f.name AS folder_name, f.path AS folder_path, \
            f.shared AS folder_shared, j.batch_id, b.origin, j.kind, j.wanted, j.options, \
            j.state, j.search_id, j.excluded_sources, j.created_at, j.updated_at, \
            j.changed_seq, j.failure, \
            j.monitor_batch_id IS NOT NULL OR j.request IS NOT NULL AS sent \
     FROM jobs j JOIN users u ON u.id = j.user_id JOIN folders f ON f.id = j.folder_id \
     JOIN batches b ON b.id = j.batch_id";

/// `user`'s job `id`, or None for an unknown id or another user's.
pub async fn job_record(
    db: &SqlitePool,
    id: &str,
    user: &UserId,
) -> Result<Option<JobRecord>, sqlx::Error> {
    let row: Option<JobRow> =
        sqlx::query_as(&format!("{JOB_SELECT} WHERE j.id = ? AND j.user_id = ?"))
            .bind(id)
            .bind(user.to_string())
            .fetch_optional(db)
            .await?;
    row.map(job_of).transpose()
}

/// `user`'s jobs in creation order: those of `ids` when given (an id that is not one of the
/// user's is left out), else those of batch `batch_id` when given, else all of them; with
/// `states` not empty, only those in one of `states`.
pub async fn jobs_of(
    db: &SqlitePool,
    user: &UserId,
    ids: Option<&[String]>,
    batch_id: Option<&str>,
    states: &[JobState],
) -> Result<Vec<JobRecord>, sqlx::Error> {
    let marks = |n: usize| vec!["?"; n].join(", ");
    let mut sql = format!("{JOB_SELECT} WHERE j.user_id = ?");
    if let Some(ids) = ids {
        sql += &format!(" AND j.id IN ({})", marks(ids.len()));
    }
    if batch_id.is_some() {
        sql += " AND j.batch_id = ?";
    }
    if !states.is_empty() {
        sql += &format!(" AND j.state IN ({})", marks(states.len()));
    }
    // created_at has one-second resolution: insertion order breaks its ties.
    sql += " ORDER BY j.created_at, j.rowid";
    let mut query = sqlx::query_as::<_, JobRow>(&sql).bind(user.to_string());
    for id in ids.unwrap_or_default() {
        query = query.bind(id);
    }
    if let Some(batch_id) = batch_id {
        query = query.bind(batch_id);
    }
    for state in states {
        query = query.bind(state.as_str());
    }
    query.fetch_all(db).await?.into_iter().map(job_of).collect()
}

fn job_of(row: JobRow) -> Result<JobRecord, sqlx::Error> {
    Ok(JobRecord {
        id: row.id,
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
        batch_id: row.batch_id,
        origin: origin_named(Some(&row.origin))?,
        kind: kind_named(&row.kind)?,
        wanted: json(&row.wanted)?,
        options: row.options.as_deref().map(json).transpose()?,
        state: state_named(&row.state)?,
        search_id: row.search_id,
        excluded_sources: json(&row.excluded_sources)?,
        created_at: row.created_at,
        updated_at: row.updated_at,
        changed_seq: row.changed_seq,
        failure: row.failure.as_deref().map(json).transpose()?,
        sent: row.sent,
    })
}

fn kind_named(name: &str) -> Result<JobKind, sqlx::Error> {
    match name {
        "track" => Ok(JobKind::Track),
        "album" => Ok(JobKind::Album),
        other => Err(sqlx::Error::Decode(
            format!("unknown job kind {other:?}").into(),
        )),
    }
}

/// The queued slots of `job`, in order, each with the candidate its current pick starts at.
pub async fn queued_picks(
    db: &SqlitePool,
    job: &str,
) -> Result<Vec<(u32, DownloadableItem)>, sqlx::Error> {
    let rows: Vec<(u32, String)> = sqlx::query_as(
        "SELECT t.slot, a.candidate FROM job_tracks t \
         JOIN job_attempts a ON a.job_id = t.job_id AND a.slot = t.slot AND a.n = t.pick_n \
         WHERE t.job_id = ? AND t.state = ? AND a.candidate IS NOT NULL ORDER BY t.slot",
    )
    .bind(job)
    .bind(JobState::Queued.as_str())
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(slot, candidate)| Ok((slot, json(&candidate)?)))
        .collect()
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
    /// The number of the attempt its current pick started at (1 for the first): the attempts
    /// before it are of an earlier pick.
    pub pick_n: u32,
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
    pub origin: BatchOrigin,
    /// None for a web UI job.
    pub options: Option<JobOptions>,
    /// Sources whose result the user rejected, never offered to the job again.
    pub excluded_sources: Vec<String>,
    /// Its user asked to cancel it while it may have had transfers (D9).
    pub cancel_requested: bool,
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
    origin: Option<String>,
    options: Option<String>,
    excluded_sources: String,
    cancel_requested: bool,
}

#[derive(sqlx::FromRow)]
struct SlotRow {
    job_id: String,
    slot: u32,
    state: String,
    last_row: Option<String>,
    import_job: Option<String>,
    pick_n: u32,
}

fn state_named(name: &str) -> Result<JobState, sqlx::Error> {
    JobState::parse(name)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown job state {name:?}").into()))
}

/// A batch's stored origin; a job without a batch is the web UI's.
fn origin_named(name: Option<&str>) -> Result<BatchOrigin, sqlx::Error> {
    match name {
        Some("mcp") => Ok(BatchOrigin::Mcp),
        Some("ui") | None => Ok(BatchOrigin::Ui),
        Some(other) => Err(sqlx::Error::Decode(
            format!("unknown batch origin {other:?}").into(),
        )),
    }
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
    /// The MCP searches stored running that no unfinished job searching resumes: the restart
    /// cut them off.
    pub cut_searches: Vec<String>,
}

/// Every unfinished job, oldest first, its slots in order, and the MCP searches a restart cut
/// off. `ctx` comes from the job's user row (id, username, role), `folder` from its folder row.
/// A job or slot whose stored state cannot be read is set apart, so the others still resume.
pub async fn unfinished(db: &SqlitePool) -> Result<Unfinished, sqlx::Error> {
    // created_at has one-second resolution: insertion order breaks its ties.
    let jobs: Vec<UnfinishedRow> = sqlx::query_as(&format!(
        "SELECT j.id, j.user_id, u.username, u.role, j.folder_id, f.user_id AS folder_owner, \
                f.name AS folder_name, f.path AS folder_path, f.shared AS folder_shared, \
                j.wanted, j.import_mode, j.state, j.search_id, j.monitor_batch_id, j.batch_label, \
                j.request, b.origin, j.options, j.excluded_sources, j.cancel_requested \
         FROM jobs j JOIN users u ON u.id = j.user_id JOIN folders f ON f.id = j.folder_id \
         LEFT JOIN batches b ON b.id = j.batch_id \
         WHERE j.state NOT IN {TERMINAL_SQL} ORDER BY j.created_at, j.rowid"
    ))
    .fetch_all(db)
    .await?;
    let cut_searches: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM searches \
         WHERE spec IS NOT NULL AND candidates IS NULL AND failure IS NULL \
         AND id NOT IN (SELECT j.search_id FROM jobs j JOIN batches b ON b.id = j.batch_id \
                        WHERE b.origin = ? AND j.state = ? AND j.search_id IS NOT NULL)",
    )
    .bind(BatchOrigin::Mcp.as_str())
    .bind(JobState::Searching.as_str())
    .fetch_all(db)
    .await?;
    let slots: Vec<SlotRow> = sqlx::query_as(&format!(
        "SELECT t.job_id, t.slot, t.state, t.last_row, t.import_job, t.pick_n \
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

    let mut unfinished = Unfinished {
        cut_searches,
        ..Unfinished::default()
    };
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
                pick_n: row.pick_n,
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
                origin: origin_named(row.origin.as_deref())?,
                options: row.options.as_deref().map(json).transpose()?,
                excluded_sources: json(&row.excluded_sources)?,
                cancel_requested: row.cancel_requested,
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

/// One slot of a job as stored, whatever its state.
#[derive(Debug, Clone, PartialEq)]
pub struct JobTrack {
    pub slot: u32,
    pub state: JobState,
    /// Its `job` set.
    pub row: Option<DownloadProgress>,
    /// Kept after the wait ends, so a slot failed at the index stage still has it.
    pub index_wait: Option<IndexWait>,
    pub result: Option<SlotResult>,
    /// The import a slot waiting for a decision keeps; None in any other state.
    pub kept: Option<StoredImport>,
}

/// `job`'s slots, in order.
pub async fn tracks_of(db: &SqlitePool, job: &str) -> Result<Vec<JobTrack>, sqlx::Error> {
    type Row = (
        u32,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT slot, state, last_row, index_wait, result, import_job FROM job_tracks \
         WHERE job_id = ? ORDER BY slot",
    )
    .bind(job)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(slot, state, row, index_wait, result, import)| {
            let row: Option<DownloadProgress> = row.as_deref().map(json).transpose()?;
            let state = state_named(&state)?;
            // Only a kept import is read here; a queued or running one is the import queue's.
            let kept = match state {
                JobState::NeedsDecision => import.as_deref().map(json).transpose()?,
                _ => None,
            };
            Ok(JobTrack {
                slot,
                state,
                kept,
                row: row.map(|row| {
                    row.with_job(JobSlot {
                        job: job.to_string(),
                        slot,
                    })
                }),
                index_wait: index_wait.as_deref().map(json).transpose()?,
                result: result.as_deref().map(json).transpose()?,
            })
        })
        .collect()
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

/// A batch as stored, with how many of its jobs are in each state.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchRecord {
    pub id: String,
    pub created_at: i64,
    pub label: Option<String>,
    pub origin: BatchOrigin,
    /// By state name; a state no job is in is left out.
    pub counts: BTreeMap<String, u32>,
}

/// `user`'s batches, newest first: only `id` when given, only those created at or after `since`
/// when given. Each has its jobs counted by state.
async fn batches_where(
    db: &SqlitePool,
    user: &UserId,
    id: Option<&str>,
    since: Option<i64>,
) -> Result<Vec<BatchRecord>, sqlx::Error> {
    let mut sql = "SELECT b.id, b.created_at, b.label, b.origin, j.state, COUNT(j.id) \
                   FROM batches b LEFT JOIN jobs j ON j.batch_id = b.id WHERE b.user_id = ?"
        .to_string();
    if id.is_some() {
        sql += " AND b.id = ?";
    }
    if since.is_some() {
        sql += " AND b.created_at >= ?";
    }
    // created_at has one-second resolution: insertion order breaks its ties.
    sql += " GROUP BY b.id, j.state ORDER BY b.created_at DESC, b.rowid DESC";
    type Row = (String, i64, Option<String>, String, Option<String>, u32);
    let mut query = sqlx::query_as::<_, Row>(&sql).bind(user.to_string());
    if let Some(id) = id {
        query = query.bind(id);
    }
    if let Some(since) = since {
        query = query.bind(since);
    }
    let mut batches: Vec<BatchRecord> = Vec::new();
    for (id, created_at, label, origin, state, count) in query.fetch_all(db).await? {
        if batches.last().is_none_or(|b| b.id != id) {
            batches.push(BatchRecord {
                id,
                created_at,
                label,
                origin: origin_named(Some(&origin))?,
                counts: BTreeMap::new(),
            });
        }
        if let (Some(state), Some(batch)) = (state, batches.last_mut()) {
            batch
                .counts
                .insert(state_named(&state)?.as_str().to_string(), count);
        }
    }
    Ok(batches)
}

/// `user`'s batch `id`; None for an unknown id or another user's.
pub async fn batch_of(
    db: &SqlitePool,
    id: &str,
    user: &UserId,
) -> Result<Option<BatchRecord>, sqlx::Error> {
    Ok(batches_where(db, user, Some(id), None).await?.pop())
}

/// `user`'s batches, newest first; `label` matches case-insensitively as a substring, `since` on
/// created_at.
pub async fn batches_of(
    db: &SqlitePool,
    user: &UserId,
    label: Option<&str>,
    since: Option<i64>,
) -> Result<Vec<BatchRecord>, sqlx::Error> {
    let mut batches = batches_where(db, user, None, since).await?;
    // Folded here: SQLite's lower() folds ASCII letters only.
    if let Some(label) = label.map(str::to_lowercase) {
        batches.retain(|b| (b.label.as_deref()).is_some_and(|l| l.to_lowercase().contains(&label)));
    }
    Ok(batches)
}

/// A batch with a job not yet in a terminal state.
#[derive(Clone, Debug, PartialEq)]
pub struct RunningBatch {
    pub batch_id: String,
    pub label: Option<String>,
    pub unfinished_jobs: u32,
}

/// D13: `user`'s batches with an unfinished job, newest first.
pub async fn running_batches(
    db: &SqlitePool,
    user: &UserId,
) -> Result<Vec<RunningBatch>, sqlx::Error> {
    // created_at has one-second resolution: insertion order breaks its ties.
    let rows: Vec<(String, Option<String>, u32)> = sqlx::query_as(&format!(
        "SELECT b.id, b.label, COUNT(*) FROM batches b JOIN jobs j ON j.batch_id = b.id \
         WHERE b.user_id = ? AND j.state NOT IN {TERMINAL_SQL} \
         GROUP BY b.id ORDER BY b.created_at DESC, b.rowid DESC"
    ))
    .bind(user.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(batch_id, label, unfinished_jobs)| RunningBatch {
            batch_id,
            label,
            unfinished_jobs,
        })
        .collect())
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
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{
        batch_of_downloads, named_batch, test_db_with_alice, track_job_of,
    };
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
            last_run: None,
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
            failure: None,
        });
        ledger.settled().await;
        assert!(unfinished(&db).await.unwrap().jobs.is_empty());
    }

    #[tokio::test]
    async fn running_batches_are_the_batches_with_unfinished_jobs() {
        let db = test_db_with_alice().await;
        crate::core::folder_of(&db, "bob").await;
        let (acq, _) = Acquire::for_test(db.clone());
        let (running, jobs) = batch_of_downloads(&acq, 2).await;
        let end = |job: &str, state| JobEvent::Ended {
            job: job.to_string(),
            state,
            failure: None,
        };
        acq.ledger.record(end(&jobs[0], JobState::Done));
        let ended = named_batch(&acq, "Advent").await;
        let advent = jobs_of(&db, &UserId::new("alice"), None, Some(&ended), &[])
            .await
            .unwrap();
        acq.ledger.record(end(&advent[0].id, JobState::Failed));
        track_job_of(&acq, "bob").await;
        acq.ledger.settled().await;
        assert_eq!(
            running_batches(&db, &UserId::new("alice")).await.unwrap(),
            vec![RunningBatch {
                batch_id: running,
                label: Some("Winter Songs".into()),
                unfinished_jobs: 1,
            }]
        );
    }

    #[tokio::test]
    async fn batches_are_read_newest_first_with_their_counts_and_found_by_label() {
        let db = test_db_with_alice().await;
        let (acq, _) = Acquire::for_test(db.clone());
        let (downloads, _) = batch_of_downloads(&acq, 2).await;
        let advent = named_batch(&acq, "Été d'Advent").await;
        let alice = UserId::new("alice");
        let all = batches_of(&db, &alice, None, None).await.unwrap();
        assert_eq!(
            all.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            vec![advent.as_str(), downloads.as_str()]
        );
        assert_eq!(
            (all[1].label.as_deref(), all[1].origin, &all[1].counts),
            (
                Some("Winter Songs"),
                BatchOrigin::Mcp,
                &BTreeMap::from([("downloading".to_string(), 2)])
            )
        );
        // Unicode letters fold too.
        let found = batches_of(&db, &alice, Some("ÉTÉ"), None).await.unwrap();
        assert_eq!(found.len(), 1);
        let later = all[0].created_at + 1;
        assert!(batches_of(&db, &alice, None, Some(later))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            batch_of(&db, &downloads, &alice).await.unwrap().as_ref(),
            Some(&all[1])
        );
        assert_eq!(
            batch_of(&db, &downloads, &UserId::new("bob"))
                .await
                .unwrap(),
            None
        );
    }
}
