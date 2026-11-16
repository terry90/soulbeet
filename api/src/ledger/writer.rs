//! The writer task: the one place the ledger is written, applying each write
//! in the order it was sent.

use super::{
    job_state, recording_keys, Admitted, BatchOrigin, ClaimedJob, ImportDone, IndexWait, JobEvent,
    JobState, LedgerError, NewJob, NewJobBatch, NewSearch, Pruned, RecordingKeys, Transition,
    Transitioned, JOB_KEPT_SECS, MCP_UNFINISHED_JOBS_PER_USER, SEARCH_KEPT_SECS, TERMINAL_SQL,
};
use crate::auth::UserId;
use crate::transfer_registry::TransferRegistry;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableGroup, DownloadableItem, IndexState, JobSlot,
};
use shared::failure::{Failure, FailureCode, Stage};
use soulbeet::ImportMode;
use sqlx::{Sqlite, SqliteConnection, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::mem::Discriminant;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

pub(super) type Reply<T> = oneshot::Sender<Result<T, LedgerError>>;

/// What the writer reads its writes from and sends its change numbers on.
pub(super) struct Inbox {
    pub(super) writes: mpsc::UnboundedReceiver<Write>,
    pub(super) changed: watch::Sender<i64>,
}

pub(super) enum Write {
    Create {
        job: Box<NewJob>,
        search: Option<Box<NewSearch>>,
        reply: Reply<()>,
    },
    CreateBatch {
        batch: NewJobBatch,
        jobs: Vec<NewJob>,
        reply: Reply<Vec<Admitted>>,
    },
    ClaimDownload {
        job: String,
        limit: u32,
        reply: Reply<bool>,
    },
    Transition {
        user: UserId,
        job: String,
        change: Box<Transition>,
        reply: Reply<Transitioned>,
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
    /// An event whose sender waits until it is stored.
    Stored {
        event: Box<JobEvent>,
        reply: Reply<()>,
    },
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

/// Starts the writer on `db`, counting changes on from the last one stored and marking jobs in
/// `registry`.
pub(super) fn spawn(db: SqlitePool, inbox: Inbox, registry: &'static TransferRegistry) {
    let Inbox {
        mut writes,
        changed,
    } = inbox;
    tokio::spawn(async move {
        let seq = sqlx::query_scalar("SELECT COALESCE(MAX(changed_seq), 0) FROM jobs")
            .fetch_one(&db)
            .await
            .unwrap_or_else(|e| {
                warn!("Could not read the job ledger's last change number, counting from 0: {e}");
                0
            });
        changed.send_if_modified(|last| std::mem::replace(last, seq) != seq);
        let mut writer = Writer {
            db,
            stored: HashMap::new(),
            registry,
            changed,
            seq,
            pending: Pending::default(),
        };
        while let Some(write) = writes.recv().await {
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

/// A job's cancel mark to set or drop in the transfer registry.
enum Mark {
    Cancel(String),
    Reopen(String),
}

/// What the write being made does once it commits.
#[derive(Default)]
struct Pending {
    /// Its change number, taken by its first change of a job.
    seq: Option<i64>,
    marks: Vec<Mark>,
}

/// Which of a job's slots `fail_slots` fails.
enum Failing {
    Unfinished,
    All,
}

struct Writer {
    db: SqlitePool,
    /// The key of the row last stored for each slot of an unfinished job. A
    /// slot missing here stores its next row.
    stored: HashMap<JobSlot, RowKey>,
    registry: &'static TransferRegistry,
    changed: watch::Sender<i64>,
    /// The change number of the last write that changed a job.
    seq: i64,
    pending: Pending,
}

impl Writer {
    async fn apply(&mut self, write: Write) {
        match write {
            Write::Create { job, search, reply } => {
                let created = self.create(*job, search.map(|s| *s)).await;
                self.finish(reply, "record a job", created)
            }
            Write::CreateBatch { batch, jobs, reply } => {
                let created = self.create_batch(batch, jobs).await;
                self.finish(reply, "record a batch of jobs", created)
            }
            Write::ClaimDownload { job, limit, reply } => {
                let claimed = claim_download(&self.db, &job, limit).await;
                self.finish(reply, "claim a download place", claimed)
            }
            Write::Transition {
                user,
                job,
                change,
                reply,
            } => {
                let changed = self.transition(&user, &job, *change).await;
                self.finish(reply, "change a job's state", changed)
            }
            Write::RecordSearch { search, reply } => {
                let recorded = insert_search(&self.db, *search).await;
                self.finish(reply, "record a search", recorded)
            }
            Write::StoreCandidates {
                search_id,
                groups,
                reply,
            } => {
                let stored = store_candidates(&self.db, &search_id, &groups).await;
                self.finish(reply, "store a search's candidates", stored)
            }
            Write::ClaimSourceChoice {
                user,
                search_id,
                folder_id,
                picked,
                reply,
            } => {
                let claimed = self
                    .claim_source_choice(&user, &search_id, &folder_id, &picked)
                    .await;
                self.finish(reply, "claim a job waiting for a source", claimed)
            }
            Write::Prune { now, reply } => {
                let pruned = self.prune(now).await;
                self.finish(reply, "remove expired rows", pruned)
            }
            Write::Event(event) => {
                let recorded = self.event(event).await;
                self.settle(recorded.is_ok());
                if let Err(e) = recorded {
                    warn!("Could not record a job's change in the job ledger: {e}");
                }
            }
            Write::Stored { event, reply } => {
                let stored = self.event(*event).await;
                self.finish(reply, "record a job's change", stored)
            }
            #[cfg(test)]
            Write::Settled { reply } => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Settles the write, then logs a failure and answers its sender.
    fn finish<T>(&mut self, reply: Reply<T>, what: &str, result: Result<T, LedgerError>) {
        self.settle(result.is_ok());
        answer(reply, what, result);
    }

    /// After a write: when it committed, sets and drops its cancel marks and sends its change
    /// number; otherwise forgets both.
    fn settle(&mut self, committed: bool) {
        let Pending { seq, marks } = std::mem::take(&mut self.pending);
        if !committed {
            return;
        }
        for mark in marks {
            match mark {
                Mark::Cancel(job) => self.registry.cancel_job(&job),
                Mark::Reopen(job) => self.registry.reopen_job(&job),
            }
        }
        if let Some(seq) = seq {
            self.seq = seq;
            self.changed.send_replace(seq);
        }
    }

    /// Records `event`, forgetting the stored row keys when it fails.
    async fn event(&mut self, event: JobEvent) -> Result<(), LedgerError> {
        let recorded = self.record(event).await;
        if recorded.is_err() {
            // The keys may name rows the failed transaction did not
            // store; a slot without one stores its next row.
            self.stored.clear();
        }
        recorded
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
                self.pick_slots(&mut tx, &job, &items).await?;
                jobs.insert(job);
            }
            JobEvent::Enqueued {
                job,
                monitor_batch_id,
                batch_label,
                request,
            } => {
                sqlx::query(
                    "UPDATE jobs SET monitor_batch_id = ?, batch_label = ?, request = ? WHERE id = ?",
                )
                .bind(monitor_batch_id)
                .bind(batch_label)
                .bind(request)
                .bind(&job)
                .execute(&mut *tx)
                .await?;
                self.touch(&mut tx, &job, None, now()).await?;
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
            JobEvent::Attempt {
                slot,
                candidate,
                request,
            } => {
                // Selected from the slot, so a deleted job's attempt adds nothing.
                sqlx::query(
                    "INSERT INTO job_attempts (job_id, slot, n, candidate, request) \
                     SELECT job_id, slot, (SELECT COALESCE(MAX(n), 0) + 1 FROM job_attempts \
                                           WHERE job_id = t.job_id AND slot = t.slot), ?, ? \
                     FROM job_tracks t WHERE job_id = ? AND slot = ?",
                )
                .bind(candidate.map(|c| serde_json::to_string(&c)).transpose()?)
                .bind(request)
                .bind(&slot.job)
                .bind(slot.slot)
                .execute(&mut *tx)
                .await?;
                self.touch(&mut tx, &slot.job, None, now()).await?;
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
                self.end_slots(&mut tx, &job, state).await?;
                self.set_job(&mut tx, &job, state).await?;
            }
        }
        for job in jobs {
            // A deleted job (D13) has no slots left to follow.
            if let Some(state) = self.followed_state(&mut tx, &job).await? {
                self.set_job(&mut tx, &job, state).await?;
            }
        }
        Ok(tx.commit().await?)
    }

    /// The state `job`'s slots give it (D6); None when it has none. A job searching or waiting
    /// for a source choice whose slots all ended (a new search set them aside) keeps its state.
    async fn followed_state(
        &self,
        tx: &mut SqliteConnection,
        job: &str,
    ) -> Result<Option<JobState>, LedgerError> {
        let states: Vec<String> =
            sqlx::query_scalar("SELECT state FROM job_tracks WHERE job_id = ?")
                .bind(job)
                .fetch_all(&mut *tx)
                .await?;
        if states.is_empty() {
            return Ok(None);
        }
        let states = states
            .iter()
            .map(|s| state_named(s))
            .collect::<Result<Vec<_>, _>>()?;
        let current: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
            .bind(job)
            .fetch_one(&mut *tx)
            .await?;
        let current = state_named(&current)?;
        let set_aside = matches!(current, JobState::Searching | JobState::NeedsSourceChoice)
            && states.iter().all(|s| s.is_terminal());
        Ok(Some(if set_aside {
            current
        } else {
            job_state(&states)
        }))
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

    /// Forgets the stored row keys of `job`'s slots: their next rows are stored.
    fn forget(&mut self, job: &str) {
        self.stored.retain(|slot, _| slot.job != job);
    }

    /// Puts every unfinished slot of `job` in `state`.
    async fn end_slots(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        state: JobState,
    ) -> Result<(), LedgerError> {
        let ended: Vec<u32> = sqlx::query_scalar(&format!(
            "UPDATE job_tracks SET state = ? WHERE job_id = ? AND state NOT IN {TERMINAL_SQL} \
             RETURNING slot"
        ))
        .bind(state.as_str())
        .bind(job)
        .fetch_all(&mut *tx)
        .await?;
        for slot in ended {
            self.restate(
                &JobSlot {
                    job: job.to_string(),
                    slot,
                },
                state,
            );
        }
        Ok(())
    }

    /// Stores `items` as `job`'s slots, each queued with its candidate as its next attempt. A
    /// slot picked before (set aside by a new search, or failed) is reopened without its row,
    /// import, index wait and result, its current pick starting at that attempt.
    async fn pick_slots(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        items: &[(u32, DownloadableItem)],
    ) -> Result<(), LedgerError> {
        for (slot, candidate) in items {
            sqlx::query(
                "INSERT INTO job_tracks (job_id, slot, state, pick_n) VALUES (?, ?, ?, 1) \
                 ON CONFLICT (job_id, slot) DO UPDATE SET state = excluded.state, \
                     last_row = NULL, import_job = NULL, index_wait = NULL, result = NULL, \
                     pick_n = (SELECT COALESCE(MAX(n), 0) + 1 FROM job_attempts \
                               WHERE job_id = excluded.job_id AND slot = excluded.slot)",
            )
            .bind(job)
            .bind(slot)
            .bind(JobState::Queued.as_str())
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO job_attempts (job_id, slot, n, candidate, source) \
                 VALUES (?, ?, (SELECT COALESCE(MAX(n), 0) + 1 FROM job_attempts \
                                WHERE job_id = ? AND slot = ?), ?, ?)",
            )
            .bind(job)
            .bind(slot)
            .bind(job)
            .bind(slot)
            .bind(serde_json::to_string(candidate)?)
            .bind(&candidate.source)
            .execute(&mut *tx)
            .await?;
            self.stored.remove(&JobSlot {
                job: job.to_string(),
                slot: *slot,
            });
        }
        Ok(())
    }

    /// Puts `slots` of `job` in failed with `failure` on each row; a slot with no row yet gets
    /// one for its newest attempt's candidate. How many slots it failed.
    async fn fail_slots(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        slots: Failing,
        failure: &Failure,
    ) -> Result<usize, LedgerError> {
        let only_unfinished = match slots {
            Failing::Unfinished => format!("AND t.state NOT IN {TERMINAL_SQL}"),
            Failing::All => String::new(),
        };
        let failing: Vec<(u32, Option<String>, Option<String>)> = sqlx::query_as(&format!(
            "SELECT t.slot, t.last_row, \
                    (SELECT a.candidate FROM job_attempts a \
                     WHERE a.job_id = t.job_id AND a.slot = t.slot ORDER BY a.n DESC LIMIT 1) \
             FROM job_tracks t WHERE t.job_id = ? {only_unfinished}"
        ))
        .bind(job)
        .fetch_all(&mut *tx)
        .await?;
        for (slot, row, candidate) in &failing {
            let row = match (row, candidate) {
                (Some(row), _) => Some(serde_json::from_str::<DownloadProgress>(row)?),
                (None, Some(candidate)) => {
                    let item: DownloadableItem = serde_json::from_str(candidate)?;
                    Some(DownloadProgress::queued(
                        item.id.clone(),
                        item.source,
                        item.id,
                        0,
                    ))
                }
                // An adopted transfer that never sent a row.
                (None, None) => None,
            };
            // A stored row never holds its slot: its readers set it.
            let row = row
                .map(|row| serde_json::to_string(&row.failed_with(failure.clone())))
                .transpose()?;
            sqlx::query(
                "UPDATE job_tracks SET state = ?, last_row = COALESCE(?, last_row) \
                 WHERE job_id = ? AND slot = ?",
            )
            .bind(JobState::Failed.as_str())
            .bind(row)
            .bind(job)
            .bind(slot)
            .execute(&mut *tx)
            .await?;
        }
        self.forget(job);
        Ok(failing.len())
    }

    /// Stamps `job` as changed by this write at `at`: its change number and updated_at, its
    /// state when given, and its download place given up unless it is then queued or
    /// downloading (D8). The change number.
    async fn touch(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        state: Option<JobState>,
        at: i64,
    ) -> Result<i64, LedgerError> {
        let seq = *self.pending.seq.get_or_insert(self.seq + 1);
        let state = state.map(JobState::as_str);
        sqlx::query(
            "UPDATE jobs SET state = COALESCE(?, state), changed_seq = ?, updated_at = ?, \
             gate_held = CASE WHEN COALESCE(?, state) IN ('queued', 'downloading') \
                         THEN gate_held ELSE 0 END \
             WHERE id = ?",
        )
        .bind(state)
        .bind(seq)
        .bind(at)
        .bind(state)
        .bind(job)
        .execute(&mut *tx)
        .await?;
        Ok(seq)
    }

    /// Puts `job` in `state` now. The change number.
    async fn set_job(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        state: JobState,
    ) -> Result<i64, LedgerError> {
        self.set_job_at(tx, job, state, now()).await
    }

    /// Puts `job` in `state` at `at`. A finished job's slots need no stored keys and the job no
    /// cancel mark. The change number.
    async fn set_job_at(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        state: JobState,
        at: i64,
    ) -> Result<i64, LedgerError> {
        let seq = self.touch(tx, job, Some(state), at).await?;
        if state.is_terminal() {
            self.forget(job);
            self.pending.marks.push(Mark::Reopen(job.to_string()));
        }
        Ok(seq)
    }

    /// Records `job` in `batch` with its picked slots.
    async fn insert_job(
        &mut self,
        tx: &mut SqliteConnection,
        job: NewJob,
        batch: &str,
        keys: RecordingKeys,
    ) -> Result<(), LedgerError> {
        let at = now();
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, \
             state, search_id, batch_id, kind, options, mbid_key, text_key) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(batch)
        .bind(job.kind.as_str())
        .bind(job.options.map(|o| serde_json::to_string(&o)).transpose()?)
        .bind(keys.mbid)
        .bind(keys.text)
        .execute(&mut *tx)
        .await?;
        self.pick_slots(tx, &job.id, &job.picked).await?;
        self.touch(tx, &job.id, None, at).await?;
        Ok(())
    }

    /// Records the web UI's `job` as a batch of its own, and first `search` when given.
    async fn create(&mut self, job: NewJob, search: Option<NewSearch>) -> Result<(), LedgerError> {
        let mut tx = self.db.begin().await?;
        if let Some(search) = search {
            insert_search(&mut *tx, search).await?;
        }
        let batch = NewJobBatch {
            id: job.id.clone(),
            user: job.user.clone(),
            label: (job.wanted.album.as_ref().map(|a| &a.title))
                .or(job.wanted.tracks.first().map(|t| &t.title))
                .cloned(),
            origin: BatchOrigin::Ui,
        };
        insert_batch(&mut tx, &batch).await?;
        let keys = recording_keys(&job.wanted, job.kind);
        self.insert_job(&mut tx, job, &batch.id, keys).await?;
        Ok(tx.commit().await?)
    }

    async fn create_batch(
        &mut self,
        batch: NewJobBatch,
        jobs: Vec<NewJob>,
    ) -> Result<Vec<Admitted>, LedgerError> {
        let mut tx = self.db.begin().await?;
        let mut written = false;
        let mut admitted = Vec::with_capacity(jobs.len());
        for job in jobs {
            let keys = recording_keys(&job.wanted, job.kind);
            if let Some((running, state)) = same_recording(&mut tx, &job.user, &keys).await? {
                admitted.push(Admitted::AlreadyRunning {
                    job: running,
                    state,
                });
                continue;
            }
            if batch.origin == BatchOrigin::Mcp {
                let unfinished = unfinished_mcp_jobs(&mut tx, &job.user).await?;
                if unfinished >= MCP_UNFINISHED_JOBS_PER_USER {
                    admitted.push(Admitted::OverLimit { unfinished });
                    continue;
                }
            }
            if !written {
                insert_batch(&mut tx, &batch).await?;
                written = true;
            }
            self.insert_job(&mut tx, job, &batch.id, keys).await?;
            admitted.push(Admitted::Created);
        }
        tx.commit().await?;
        Ok(admitted)
    }

    async fn transition(
        &mut self,
        user: &UserId,
        job: &str,
        change: Transition,
    ) -> Result<Transitioned, LedgerError> {
        let mut tx = self.db.begin().await?;
        let found: Option<(String, bool, bool)> = sqlx::query_as(
            "SELECT state, gate_held, cancel_requested FROM jobs WHERE id = ? AND user_id = ?",
        )
        .bind(job)
        .bind(user.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, gate_held, cancel_requested)) = found else {
            return Ok(Transitioned::Unknown);
        };
        let from = state_named(&state)?;
        if !change.allowed_from().contains(&from) {
            return Ok(Transitioned::Refused { state: from });
        }
        let to = match change {
            Transition::Cancel if from == JobState::Downloading || gate_held => {
                // Its transfers may exist: every monitor stops them on its next poll.
                sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = ?")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
                self.pending.marks.push(Mark::Cancel(job.to_string()));
                from
            }
            Transition::Cancel => {
                self.end_slots(&mut tx, job, JobState::Cancelled).await?;
                JobState::Cancelled
            }
            Transition::Dismiss => {
                self.end_slots(&mut tx, job, JobState::Dismissed).await?;
                JobState::Dismissed
            }
            Transition::Pick { items } => {
                self.pick_slots(&mut tx, job, &items).await?;
                forget_send(&mut tx, job).await?;
                JobState::Queued
            }
            Transition::AwaitChoice => JobState::NeedsSourceChoice,
            Transition::Fail { .. } if cancel_requested => {
                // Its user cancelled it before it could fail.
                self.end_slots(&mut tx, job, JobState::Cancelled).await?;
                JobState::Cancelled
            }
            Transition::Fail { failure } => {
                if self
                    .fail_slots(&mut tx, job, Failing::Unfinished, &failure)
                    .await?
                    == 0
                {
                    sqlx::query("UPDATE jobs SET failure = ? WHERE id = ?")
                        .bind(serde_json::to_string(&failure)?)
                        .bind(job)
                        .execute(&mut *tx)
                        .await?;
                }
                JobState::Failed
            }
            Transition::Search { search_id, filters } => {
                // Every slot, ended ones too: the new search's pick reopens those it fills.
                sqlx::query("UPDATE job_tracks SET state = ? WHERE job_id = ?")
                    .bind(JobState::Dismissed.as_str())
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
                let filters = filters.map(|f| serde_json::to_string(&f)).transpose()?;
                sqlx::query(
                    "UPDATE jobs SET search_id = ?, cancel_requested = 0, failure = NULL, \
                     options = CASE WHEN ? IS NULL THEN options \
                                    ELSE json_set(options, '$.filters', json(?)) END \
                     WHERE id = ?",
                )
                .bind(search_id)
                .bind(&filters)
                .bind(&filters)
                .bind(job)
                .execute(&mut *tx)
                .await?;
                forget_send(&mut tx, job).await?;
                self.forget(job);
                self.pending.marks.push(Mark::Reopen(job.to_string()));
                JobState::Searching
            }
            Transition::Queue { items, search_id } => {
                self.pick_slots(&mut tx, job, &items).await?;
                sqlx::query(
                    "UPDATE jobs SET search_id = COALESCE(?, search_id), cancel_requested = 0, \
                     failure = NULL WHERE id = ?",
                )
                .bind(search_id)
                .bind(job)
                .execute(&mut *tx)
                .await?;
                forget_send(&mut tx, job).await?;
                self.pending.marks.push(Mark::Reopen(job.to_string()));
                JobState::Queued
            }
            Transition::Reject { source, failure } => {
                self.fail_slots(&mut tx, job, Failing::All, &failure)
                    .await?;
                sqlx::query(
                    "UPDATE jobs SET excluded_sources = json_insert(excluded_sources, '$[#]', ?) \
                     WHERE id = ?",
                )
                .bind(source)
                .bind(job)
                .execute(&mut *tx)
                .await?;
                JobState::Failed
            }
            Transition::Reindex { now } => {
                self.reindex(&mut tx, job, now).await?;
                self.followed_state(&mut tx, job).await?.unwrap_or(from)
            }
        };
        let seq = self.set_job(&mut tx, job, to).await?;
        tx.commit().await?;
        Ok(Transitioned::Applied { from, to, seq })
    }

    /// Moves each failed slot of `job` whose row failed at the index stage back to indexing,
    /// waiting from `now` as if its import had just ended.
    async fn reindex(
        &mut self,
        tx: &mut SqliteConnection,
        job: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let failed: Vec<(u32, String, String)> = sqlx::query_as(
            "SELECT slot, last_row, index_wait FROM job_tracks \
             WHERE job_id = ? AND state = ? AND last_row IS NOT NULL AND index_wait IS NOT NULL",
        )
        .bind(job)
        .bind(JobState::Failed.as_str())
        .fetch_all(&mut *tx)
        .await?;
        for (slot, row, wait) in failed {
            let row: DownloadProgress = serde_json::from_str(&row)?;
            if row.failure.map(|f| f.stage) != Some(Stage::Index) {
                continue;
            }
            let wait = IndexWait {
                since: now,
                baseline: None,
                checked: None,
                ..serde_json::from_str(&wait)?
            };
            sqlx::query(
                "UPDATE job_tracks SET state = ?, index_wait = ? WHERE job_id = ? AND slot = ?",
            )
            .bind(JobState::Indexing.as_str())
            .bind(serde_json::to_string(&wait)?)
            .bind(job)
            .bind(slot)
            .execute(&mut *tx)
            .await?;
        }
        self.forget(job);
        Ok(())
    }

    async fn claim_source_choice(
        &mut self,
        user: &UserId,
        search_id: &str,
        folder_id: &str,
        picked: &[(u32, DownloadableItem)],
    ) -> Result<Option<ClaimedJob>, LedgerError> {
        let mut tx = self.db.begin().await?;
        // created_at has one-second resolution: insertion order breaks its ties.
        let claimed: Option<(String, String)> = sqlx::query_as(
            "SELECT id, import_mode FROM jobs \
             WHERE user_id = ? AND search_id = ? AND folder_id = ? AND state = ? \
             ORDER BY created_at, rowid LIMIT 1",
        )
        .bind(user.to_string())
        .bind(search_id)
        .bind(folder_id)
        .bind(JobState::NeedsSourceChoice.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, mode)) = claimed else {
            return Ok(None);
        };
        let import_mode = import_mode_named(&mode)?;
        self.pick_slots(&mut tx, &id, picked).await?;
        self.set_job(&mut tx, &id, JobState::Queued).await?;
        tx.commit().await?;
        Ok(Some(ClaimedJob { id, import_mode }))
    }

    async fn prune(&mut self, now: i64) -> Result<Pruned, LedgerError> {
        let mut tx = self.db.begin().await?;
        let searches = sqlx::query("DELETE FROM searches WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // Every search left is unexpired, so a waiting job without one has
        // nothing left to pick from.
        let waiting: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM jobs \
             WHERE state = ? AND NOT EXISTS (SELECT 1 FROM searches s WHERE s.id = jobs.search_id)",
        )
        .bind(JobState::NeedsSourceChoice.as_str())
        .fetch_all(&mut *tx)
        .await?;
        for job in &waiting {
            self.set_job_at(&mut tx, job, JobState::Dismissed, now)
                .await?;
        }
        let jobs = sqlx::query(&format!(
            "DELETE FROM jobs WHERE state IN {TERMINAL_SQL} AND updated_at < ?"
        ))
        .bind(now - JOB_KEPT_SECS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM batches WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE jobs.batch_id = batches.id)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Pruned {
            searches,
            dismissed: waiting.len() as u64,
            jobs,
        })
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

fn state_named(name: &str) -> Result<JobState, LedgerError> {
    JobState::parse(name).ok_or_else(|| LedgerError(format!("unknown job state {name:?}")))
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

/// `job` has no send of its files yet: reconcile must not take its queued slots for ones the
/// last send made. `Enqueued` names the next one.
async fn forget_send(tx: &mut SqliteConnection, job: &str) -> Result<(), LedgerError> {
    sqlx::query(
        "UPDATE jobs SET monitor_batch_id = NULL, batch_label = NULL, request = NULL WHERE id = ?",
    )
    .bind(job)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn insert_batch(tx: &mut SqliteConnection, batch: &NewJobBatch) -> Result<(), LedgerError> {
    sqlx::query(
        "INSERT INTO batches (id, user_id, created_at, label, origin) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&batch.id)
    .bind(batch.user.to_string())
    .bind(now())
    .bind(&batch.label)
    .bind(batch.origin.as_str())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The oldest unfinished job of `user` that is the recording `keys` name (D2): both MBID keys
/// set and equal, or one missing and the text keys equal.
async fn same_recording(
    tx: &mut SqliteConnection,
    user: &UserId,
    keys: &RecordingKeys,
) -> Result<Option<(String, JobState)>, LedgerError> {
    let found: Option<(String, String)> = sqlx::query_as(&format!(
        "SELECT id, state FROM jobs WHERE user_id = ? AND state NOT IN {TERMINAL_SQL} \
         AND (mbid_key = ? OR ((mbid_key IS NULL OR ? IS NULL) AND text_key = ?)) \
         ORDER BY created_at, rowid LIMIT 1"
    ))
    .bind(user.to_string())
    .bind(&keys.mbid)
    .bind(&keys.mbid)
    .bind(&keys.text)
    .fetch_optional(&mut *tx)
    .await?;
    found
        .map(|(id, state)| Ok((id, state_named(&state)?)))
        .transpose()
}

/// How many of `user`'s MCP jobs are unfinished.
async fn unfinished_mcp_jobs(tx: &mut SqliteConnection, user: &UserId) -> Result<u32, LedgerError> {
    let count: u32 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM jobs j JOIN batches b ON b.id = j.batch_id \
         WHERE j.user_id = ? AND b.origin = ? AND j.state NOT IN {TERMINAL_SQL}"
    ))
    .bind(user.to_string())
    .bind(BatchOrigin::Mcp.as_str())
    .fetch_one(&mut *tx)
    .await?;
    Ok(count)
}

/// D8: whether `job`, queued, holds a download place now, taking one when its user's MCP jobs
/// hold fewer than `limit`.
async fn claim_download(db: &SqlitePool, job: &str, limit: u32) -> Result<bool, LedgerError> {
    // A place is only ever held while queued or downloading (`touch`), so one held is still
    // counted in the limit.
    let claimed: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET gate_held = 1 WHERE id = ? AND state = ? AND (gate_held = 1 OR \
             (SELECT COUNT(*) FROM jobs j JOIN batches b ON b.id = j.batch_id \
              WHERE j.user_id = jobs.user_id AND b.origin = ? AND j.gate_held = 1 \
              AND j.state IN ('queued', 'downloading')) < ?) \
         RETURNING id",
    )
    .bind(job)
    .bind(JobState::Queued.as_str())
    .bind(BatchOrigin::Mcp.as_str())
    .bind(limit)
    .fetch_optional(db)
    .await?;
    Ok(claimed.is_some())
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

#[cfg(test)]
mod tests {
    use super::super::{store, JobKind, JobState, Ledger, NewJob, NewSearch};
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
                        kind: JobKind::Track,
                        options: None,
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
