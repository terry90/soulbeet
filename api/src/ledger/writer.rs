//! The writer task: the one place the ledger is written, applying each write
//! in the order it was sent.

use super::{
    job_state, ClaimedJob, ImportDone, JobEvent, JobState, LedgerError, NewJob, NewSearch, Pruned,
    JOB_KEPT_SECS, SEARCH_KEPT_SECS, TERMINAL_SQL,
};
use crate::auth::UserId;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableGroup, DownloadableItem, IndexState, JobSlot,
};
use shared::failure::FailureCode;
use soulbeet::ImportMode;
use sqlx::{Sqlite, SqliteConnection, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::mem::Discriminant;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

pub(super) type Reply<T> = oneshot::Sender<Result<T, LedgerError>>;

pub(super) enum Write {
    Create {
        job: Box<NewJob>,
        search: Option<Box<NewSearch>>,
        reply: Reply<()>,
    },
    RecordSearch {
        search: Box<NewSearch>,
        reply: Reply<()>,
    },
    StoreCandidates {
        search_id: String,
        groups: Vec<DownloadableGroup>,
        reply: Reply<()>,
    },
    ClaimSourceChoice {
        user: UserId,
        search_id: String,
        folder_id: String,
        picked: Vec<(u32, DownloadableItem)>,
        reply: Reply<Option<ClaimedJob>>,
    },
    Prune {
        now: i64,
        reply: Reply<Pruned>,
    },
    Event(JobEvent),
    #[cfg(test)]
    Settled {
        reply: Reply<()>,
    },
}

impl From<sqlx::Error> for LedgerError {
    fn from(e: sqlx::Error) -> Self {
        LedgerError(e.to_string())
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(e: serde_json::Error) -> Self {
        LedgerError(e.to_string())
    }
}

pub(super) fn spawn(db: SqlitePool, mut inbox: mpsc::UnboundedReceiver<Write>) {
    tokio::spawn(async move {
        let mut writer = Writer {
            db,
            stored: HashMap::new(),
        };
        while let Some(write) = inbox.recv().await {
            writer.apply(write).await;
        }
    });
}

/// What a slot's stored row is, as far as a new row of it is compared. A row
/// of another transfer is always stored: a failover or an adoption rebinds
/// the slot, and a restart must find the transfer it rebound to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowKey {
    state: JobState,
    row: Discriminant<DownloadState>,
    failure: Option<FailureCode>,
    index: Option<Discriminant<IndexState>>,
    /// The transfer's source and id.
    transfer: (String, String),
}

impl RowKey {
    fn of(state: JobState, row: &DownloadProgress) -> RowKey {
        RowKey {
            state,
            row: std::mem::discriminant(&row.state),
            failure: row.failure.as_ref().map(|f| f.code),
            index: row.index.as_ref().map(std::mem::discriminant),
            transfer: (row.source.clone(), row.id.clone()),
        }
    }
}

/// Stores `items` as `job`'s slots, each queued with its candidate as its first attempt.
async fn insert_slots(
    tx: &mut SqliteConnection,
    job: &str,
    items: &[(u32, DownloadableItem)],
) -> Result<(), LedgerError> {
    for (slot, candidate) in items {
        sqlx::query("INSERT INTO job_tracks (job_id, slot, state) VALUES (?, ?, ?)")
            .bind(job)
            .bind(slot)
            .bind(JobState::Queued.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO job_attempts (job_id, slot, n, candidate) VALUES (?, ?, 1, ?)")
            .bind(job)
            .bind(slot)
            .bind(serde_json::to_string(candidate)?)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

struct Writer {
    db: SqlitePool,
    /// The key of the row last stored for each slot of an unfinished job. A
    /// slot missing here stores its next row.
    stored: HashMap<JobSlot, RowKey>,
}

impl Writer {
    async fn apply(&mut self, write: Write) {
        let db = &self.db;
        match write {
            Write::Create { job, search, reply } => answer(
                reply,
                "record a job",
                create(db, *job, search.map(|s| *s)).await,
            ),
            Write::RecordSearch { search, reply } => {
                answer(reply, "record a search", insert_search(db, *search).await)
            }
            Write::StoreCandidates {
                search_id,
                groups,
                reply,
            } => answer(
                reply,
                "store a search's candidates",
                store_candidates(db, &search_id, &groups).await,
            ),
            Write::ClaimSourceChoice {
                user,
                search_id,
                folder_id,
                picked,
                reply,
            } => answer(
                reply,
                "claim a job waiting for a source",
                claim_source_choice(db, &user, &search_id, &folder_id, &picked).await,
            ),
            Write::Prune { now, reply } => {
                answer(reply, "remove expired rows", prune(db, now).await)
            }
            Write::Event(event) => {
                if let Err(e) = self.record(event).await {
                    warn!("Could not record a job's change in the job ledger: {e}");
                    // The keys may name rows the failed transaction did not
                    // store; a slot without one stores its next row.
                    self.stored.clear();
                }
            }
            #[cfg(test)]
            Write::Settled { reply } => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    async fn record(&mut self, event: JobEvent) -> Result<(), LedgerError> {
        if let JobEvent::Slots { state, rows } = &event {
            // Most rows are progress ticks of a stored state: skip them
            // before taking a connection.
            let changed = rows.iter().any(|row| {
                row.job
                    .as_ref()
                    .is_some_and(|slot| self.changes(slot, *state, row))
            });
            if !changed {
                return Ok(());
            }
        }
        let mut tx = self.db.begin().await?;
        let mut jobs = HashSet::new();
        match event {
            JobEvent::NeedsSourceChoice { job } => {
                self.set_job(&mut tx, &job, JobState::NeedsSourceChoice)
                    .await?;
            }
            JobEvent::Picked { job, items } => {
                insert_slots(&mut tx, &job, &items).await?;
                jobs.insert(job);
            }
            JobEvent::Enqueued {
                job,
                monitor_batch_id,
                batch_label,
            } => {
                sqlx::query("UPDATE jobs SET monitor_batch_id = ?, batch_label = ? WHERE id = ?")
                    .bind(monitor_batch_id)
                    .bind(batch_label)
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
            }
            JobEvent::Slots { state, rows } => {
                for row in &rows {
                    if let Some(slot) = &row.job {
                        if self.store_row(&mut tx, slot, state, row).await? {
                            jobs.insert(slot.job.clone());
                        }
                    }
                }
            }
            JobEvent::Attempt { slot, candidate } => {
                // Selected from the slot, so a deleted job's attempt adds nothing.
                sqlx::query(
                    "INSERT INTO job_attempts (job_id, slot, n, candidate) \
                     SELECT job_id, slot, (SELECT COALESCE(MAX(n), 0) + 1 FROM job_attempts \
                                           WHERE job_id = t.job_id AND slot = t.slot), ? \
                     FROM job_tracks t WHERE job_id = ? AND slot = ?",
                )
                .bind(candidate.map(|c| serde_json::to_string(&c)).transpose()?)
                .bind(&slot.job)
                .bind(slot.slot)
                .execute(&mut *tx)
                .await?;
            }
            JobEvent::ImportQueued(import) => {
                let stored = serde_json::to_string(&*import)?;
                for (slot, row) in &import.rows {
                    self.store_row(&mut tx, slot, JobState::Importing, row)
                        .await?;
                    sqlx::query(&format!(
                        "UPDATE job_tracks SET import_job = ? \
                         WHERE job_id = ? AND slot = ? AND state NOT IN {TERMINAL_SQL}"
                    ))
                    .bind(&stored)
                    .bind(&slot.job)
                    .bind(slot.slot)
                    .execute(&mut *tx)
                    .await?;
                    jobs.insert(slot.job.clone());
                }
            }
            JobEvent::ImportDone(done) => {
                let ImportDone {
                    import,
                    rows,
                    next,
                    index,
                } = *done;
                let kept = (next == JobState::NeedsDecision)
                    .then(|| serde_json::to_string(&import))
                    .transpose()?;
                let wait = index.map(|wait| serde_json::to_string(&wait)).transpose()?;
                for (slot, _) in &import.rows {
                    self.set_slot(&mut tx, slot, next).await?;
                    sqlx::query(&format!(
                        "UPDATE job_tracks SET import_job = ?, index_wait = ? \
                         WHERE job_id = ? AND slot = ? AND state NOT IN {TERMINAL_SQL}"
                    ))
                    .bind(&kept)
                    .bind(&wait)
                    .bind(&slot.job)
                    .bind(slot.slot)
                    .execute(&mut *tx)
                    .await?;
                    jobs.insert(slot.job.clone());
                }
                for row in &rows {
                    if let Some(slot) = &row.job {
                        if self.store_row(&mut tx, slot, next, row).await? {
                            jobs.insert(slot.job.clone());
                        }
                    }
                }
            }
            JobEvent::Indexing { slots, wait } => {
                let wait = serde_json::to_string(&*wait)?;
                for slot in &slots {
                    self.set_slot(&mut tx, slot, JobState::Indexing).await?;
                    sqlx::query(&format!(
                        "UPDATE job_tracks SET index_wait = ? \
                         WHERE job_id = ? AND slot = ? AND state NOT IN {TERMINAL_SQL}"
                    ))
                    .bind(&wait)
                    .bind(&slot.job)
                    .bind(slot.slot)
                    .execute(&mut *tx)
                    .await?;
                    jobs.insert(slot.job.clone());
                }
            }
            JobEvent::Dismiss(slots) => {
                for slot in &slots {
                    self.set_slot(&mut tx, slot, JobState::Dismissed).await?;
                    jobs.insert(slot.job.clone());
                }
            }
            JobEvent::Unreadable(slots) => {
                for slot in &slots {
                    self.set_slot(&mut tx, slot, JobState::Failed).await?;
                    jobs.insert(slot.job.clone());
                }
            }
            JobEvent::Ended { job, state } => {
                let ended: Vec<u32> = sqlx::query_scalar(&format!(
                    "UPDATE job_tracks SET state = ? WHERE job_id = ? AND state NOT IN {TERMINAL_SQL} \
                     RETURNING slot"
                ))
                .bind(state.as_str())
                .bind(&job)
                .fetch_all(&mut *tx)
                .await?;
                for slot in ended {
                    self.restate(
                        &JobSlot {
                            job: job.clone(),
                            slot,
                        },
                        state,
                    );
                }
                self.set_job(&mut tx, &job, state).await?;
            }
        }
        for job in jobs {
            let states: Vec<String> =
                sqlx::query_scalar("SELECT state FROM job_tracks WHERE job_id = ?")
                    .bind(&job)
                    .fetch_all(&mut *tx)
                    .await?;
            let states = states
                .iter()
                .map(|s| {
                    JobState::parse(s)
                        .ok_or_else(|| LedgerError(format!("unknown job state {s:?}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // A deleted job (D13) has no slots left to follow.
            if states.is_empty() {
                continue;
            }
            self.set_job(&mut tx, &job, job_state(&states)).await?;
        }
        Ok(tx.commit().await?)
    }

    /// Whether `row` of `slot`, now in `state`, differs from the row stored for it.
    fn changes(&self, slot: &JobSlot, state: JobState, row: &DownloadProgress) -> bool {
        self.stored.get(slot) != Some(&RowKey::of(state, row))
    }

    /// Puts `slot` in `state` with `row` as its last row, unless that row
    /// changes nothing the ledger keeps or the slot already ended: a slot in a
    /// terminal state never moves again. Whether it changed the slot.
    async fn store_row(
        &mut self,
        tx: &mut SqliteConnection,
        slot: &JobSlot,
        state: JobState,
        row: &DownloadProgress,
    ) -> Result<bool, LedgerError> {
        if !self.changes(slot, state, row) {
            return Ok(false);
        }
        let stored = sqlx::query(&format!(
            "UPDATE job_tracks SET state = ?, last_row = ? \
             WHERE job_id = ? AND slot = ? AND state NOT IN {TERMINAL_SQL}"
        ))
        .bind(state.as_str())
        .bind(serde_json::to_string(row)?)
        .bind(&slot.job)
        .bind(slot.slot)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if stored > 0 {
            self.stored.insert(slot.clone(), RowKey::of(state, row));
        }
        Ok(stored > 0)
    }

    /// Puts `slot` in `state`, keeping its last row, unless it already ended.
    async fn set_slot(
        &mut self,
        tx: &mut SqliteConnection,
        slot: &JobSlot,
        state: JobState,
    ) -> Result<(), LedgerError> {
        let moved = sqlx::query(&format!(
            "UPDATE job_tracks SET state = ? \
             WHERE job_id = ? AND slot = ? AND state NOT IN {TERMINAL_SQL}"
        ))
        .bind(state.as_str())
        .bind(&slot.job)
        .bind(slot.slot)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved > 0 {
            self.restate(slot, state);
        }
        Ok(())
    }

    /// `slot` is now in `state` and its stored row is unchanged.
    fn restate(&mut self, slot: &JobSlot, state: JobState) {
        if let Some(key) = self.stored.get_mut(slot) {
            key.state = state;
        }
    }

    /// Puts `job` in `state`. A finished job's slots need no stored keys.
    async fn set_job(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        state: JobState,
    ) -> Result<(), LedgerError> {
        sqlx::query("UPDATE jobs SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(now())
            .bind(job)
            .execute(&mut *tx)
            .await?;
        if state.is_terminal() {
            self.stored.retain(|slot, _| slot.job != job);
        }
        Ok(())
    }
}

/// Logs a failed write and answers its sender, who may have stopped waiting.
fn answer<T>(reply: Reply<T>, what: &str, result: Result<T, LedgerError>) {
    if let Err(e) = &result {
        warn!("Could not {what} in the job ledger: {e}");
    }
    let _ = reply.send(result);
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn import_mode_name(mode: ImportMode) -> &'static str {
    match mode {
        ImportMode::Album => "album",
        ImportMode::Singleton => "singleton",
    }
}

pub(super) fn import_mode_named(name: &str) -> Result<ImportMode, LedgerError> {
    match name {
        "album" => Ok(ImportMode::Album),
        "singleton" => Ok(ImportMode::Singleton),
        other => Err(LedgerError(format!("unknown import mode {other:?}"))),
    }
}

/// Records `search`, renewing a search of the same id: it expires a day from
/// now and has no candidates until it ends again.
async fn insert_search(
    db: impl sqlx::Executor<'_, Database = Sqlite>,
    search: NewSearch,
) -> Result<(), LedgerError> {
    sqlx::query(
        "INSERT INTO searches (id, user_id, backend, expires_at, query) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO UPDATE SET expires_at = excluded.expires_at, candidates = NULL",
    )
    .bind(search.id)
    .bind(search.user.to_string())
    .bind(search.backend)
    .bind(now() + SEARCH_KEPT_SECS)
    .bind(serde_json::to_string(&search.query)?)
    .execute(db)
    .await?;
    Ok(())
}

/// Records `job` with its picked slots, and first `search` when given.
async fn create(
    db: &SqlitePool,
    job: NewJob,
    search: Option<NewSearch>,
) -> Result<(), LedgerError> {
    let mut tx = db.begin().await?;
    if let Some(search) = search {
        insert_search(&mut *tx, search).await?;
    }
    let at = now();
    sqlx::query(
        "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, \
         state, search_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&job.id)
    .bind(job.user.to_string())
    .bind(job.folder_id)
    .bind(at)
    .bind(at)
    .bind(serde_json::to_string(&job.wanted)?)
    .bind(import_mode_name(job.import_mode))
    .bind(job.state.as_str())
    .bind(job.search_id)
    .execute(&mut *tx)
    .await?;
    insert_slots(&mut tx, &job.id, &job.picked).await?;
    Ok(tx.commit().await?)
}

async fn store_candidates(
    db: &SqlitePool,
    search_id: &str,
    groups: &[DownloadableGroup],
) -> Result<(), LedgerError> {
    sqlx::query("UPDATE searches SET candidates = ? WHERE id = ?")
        .bind(serde_json::to_string(groups)?)
        .bind(search_id)
        .execute(db)
        .await?;
    Ok(())
}

async fn claim_source_choice(
    db: &SqlitePool,
    user: &UserId,
    search_id: &str,
    folder_id: &str,
    picked: &[(u32, DownloadableItem)],
) -> Result<Option<ClaimedJob>, LedgerError> {
    let mut tx = db.begin().await?;
    // created_at has one-second resolution: insertion order breaks its ties.
    let claimed: Option<(String, String)> = sqlx::query_as(
        "UPDATE jobs SET state = ?, updated_at = ? WHERE id = (\
             SELECT id FROM jobs \
             WHERE user_id = ? AND search_id = ? AND folder_id = ? AND state = ? \
             ORDER BY created_at, rowid LIMIT 1\
         ) RETURNING id, import_mode",
    )
    .bind(JobState::Queued.as_str())
    .bind(now())
    .bind(user.to_string())
    .bind(search_id)
    .bind(folder_id)
    .bind(JobState::NeedsSourceChoice.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, mode)) = claimed else {
        return Ok(None);
    };
    insert_slots(&mut tx, &id, picked).await?;
    let import_mode = import_mode_named(&mode)?;
    tx.commit().await?;
    Ok(Some(ClaimedJob { id, import_mode }))
}

async fn prune(db: &SqlitePool, now: i64) -> Result<Pruned, LedgerError> {
    let mut tx = db.begin().await?;
    let searches = sqlx::query("DELETE FROM searches WHERE expires_at <= ?")
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // Every search left is unexpired, so a waiting job without one has
    // nothing left to pick from.
    let dismissed = sqlx::query(
        "UPDATE jobs SET state = ?, updated_at = ? \
         WHERE state = ? AND NOT EXISTS (SELECT 1 FROM searches s WHERE s.id = jobs.search_id)",
    )
    .bind(JobState::Dismissed.as_str())
    .bind(now)
    .bind(JobState::NeedsSourceChoice.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let jobs = sqlx::query(&format!(
        "DELETE FROM jobs WHERE state IN {TERMINAL_SQL} AND updated_at < ?"
    ))
    .bind(now - JOB_KEPT_SECS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(Pruned {
        searches,
        dismissed,
        jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{store, JobState, Ledger, NewJob, NewSearch};
    use crate::auth::UserId;
    use shared::download::DownloadQuery;
    use soulbeet::ImportMode;

    #[tokio::test]
    async fn a_claim_continues_the_job_waiting_longest_for_the_source() {
        let db = crate::db::test_db::migrated().await;
        let folder = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        for id in ["first", "second"] {
            ledger
                .create(
                    NewJob {
                        id: id.into(),
                        user: UserId::new("alice"),
                        folder_id: folder.clone(),
                        wanted: DownloadQuery::new(vec![]),
                        import_mode: ImportMode::Singleton,
                        state: JobState::NeedsSourceChoice,
                        search_id: Some("s1".into()),
                        picked: vec![],
                    },
                    Some(NewSearch {
                        id: "s1".into(),
                        user: UserId::new("alice"),
                        backend: None,
                        query: DownloadQuery::new(vec![]),
                    }),
                )
                .await
                .unwrap();
        }
        // Stored second, but made first.
        sqlx::query("UPDATE jobs SET created_at = created_at - 10 WHERE id = 'second'")
            .execute(&db)
            .await
            .unwrap();
        let claimed = ledger
            .claim_source_choice(&UserId::new("alice"), "s1", &folder, vec![])
            .await
            .unwrap();
        assert_eq!(claimed.map(|c| c.id), Some("second".to_string()));
        assert_eq!(
            store::job_state(&db, "first").await,
            Some(JobState::NeedsSourceChoice)
        );
    }
}
