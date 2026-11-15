//! Startup: every job the ledger holds unfinished picks up where Soulful stopped.
//! Each slot resumes by its own state, since a job is only at its earliest
//! slot: downloads slskd still has are watched again, interrupted imports run
//! again, kept imports keep their Retry, queued files are enqueued and
//! auto-downloads still searching search again.

use crate::auth::{UserCtx, UserId};
use crate::config::CONFIG;
use crate::core::acquire::{
    alternates_from, enqueue_and_watch, fail_job, manual_batch_label, run_search, spawn_monitor,
    Acquisition, AutoRun, NewBatch,
};
use crate::core::metadata::lastfm_key;
use crate::core::{backend_unavailable, database_failure, sources};
use crate::globals::{get_or_create_user_channel, send_user_event};
use crate::import_queue::{ImportJob, ImportQueue, IMPORT_QUEUE};
use crate::ledger::store::{self, SlotRecord, StoredSearch, Unfinished, UnfinishedJob};
use crate::ledger::{JobEvent, JobState, Ledger, NewSearch, StoredImport, LEDGER};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::{held_failure, transfer_key};
use crate::server_fns::download::failover::{alternates_for, SourcePool};
use crate::server_fns::download::monitor::{DownloadMonitor, WatchedTransfer};
use crate::server_fns::download::process::{
    album_jobs, find_on_disk, import_job, resolve_on_disk, unresolved_row, ResolvedSlot,
};
use crate::server_fns::download::utils::{FileId, Unresolved};
use crate::services::download_backend;
use crate::transfer_registry::{BatchRef, TransferOwner, TransferRegistry};
use async_trait::async_trait;
use shared::download::{
    DownloadEvent, DownloadProgress, DownloadQuery, DownloadState, DownloadableGroup,
    DownloadableItem, JobSlot,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::{DownloadBackend, ImportMode};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// A transfer Soulful watched before it stopped is gone from slskd.
pub(crate) fn transfer_lost() -> Failure {
    Failure::new(
        FailureCode::ServerRestarted,
        Stage::Transfer,
        RetryAdvice::SameSourceLater,
        "Soulful restarted and slskd no longer has this download",
    )
}

/// An import Soulful ran when it stopped has no files left to import (D10).
pub(crate) fn import_lost() -> Failure {
    Failure::new(
        FailureCode::ServerRestarted,
        Stage::Import,
        RetryAdvice::NotRetryable,
        "Soulful restarted during this import and the downloaded files are gone",
    )
}

/// The row `slot` shows, its `job` set: its last one, or before it had one a
/// queued row of its first source.
fn shown_row(slot: &JobSlot, record: &SlotRecord) -> Option<DownloadProgress> {
    let row = record.row.clone().or_else(|| {
        let item = record.attempts.first()?.as_ref()?;
        Some(DownloadProgress::queued(
            item.id.clone(),
            item.source.clone(),
            item.id.clone(),
            item.size.unwrap_or(0),
        ))
    })?;
    Some(row.with_job(slot.clone()))
}

/// Whether `row` is a transfer of `item`: its file from its source.
fn is_from(row: &DownloadProgress, item: &DownloadableItem) -> bool {
    row.source == item.source && row.item == item.id
}

/// The transfer of `item` that `backend` made in the request `request`: what
/// a send Soulful stopped during left. A record of the same file from the
/// same source made in another request is another download's. None when it
/// holds none or cannot be asked.
async fn sent(
    backend: &Arc<dyn DownloadBackend>,
    item: &DownloadableItem,
    request: &str,
) -> Option<DownloadProgress> {
    let listed = match backend
        .get_downloads_for(std::slice::from_ref(&item.source))
        .await
    {
        Ok(listed) => listed,
        Err(e) => {
            debug!(
                "Could not ask {} for the transfers from {}: {e}",
                backend.name(),
                item.source
            );
            return None;
        }
    };
    // One request sends a file once.
    listed
        .into_iter()
        .find(|t| is_from(t, item) && t.request.as_deref() == Some(request))
}

/// The downloading slots of one monitor batch, checked against the backend.
/// Every transfer, path and row carries the JobSlot its slot was given.
#[derive(Default)]
pub(crate) struct Resumed {
    /// Transfers the backend still has, finished ones included (a monitor hands each off on its
    /// first poll), or could not be asked about (the monitor's own unreadable-list deadline
    /// decides those).
    pub watch: Vec<WatchedTransfer>,
    /// Finished files of the batch on disk, with the path found: the transfers the backend no
    /// longer lists, whatever their last row said, whose file is in the batch folder (or
    /// anywhere in the download folder for an adopted one), and the queued files found in the
    /// batch folder.
    pub finished: Vec<ResolvedSlot>,
    /// transfer_lost rows for a transfer neither the backend nor the disk has, and
    /// unresolved_row for a finished transfer whose file is not found.
    pub failed: Vec<DownloadProgress>,
    /// Rows of the slots whose failover had enqueued their next source when Soulful stopped:
    /// that transfer is the slot's now.
    pub rebound: Vec<DownloadProgress>,
    /// (source, transfer id) of the transfers those slots left, to stop.
    pub abandon: Vec<(String, String)>,
    /// The queued slots whose file is in the batch folder, among `finished`: not to enqueue again.
    pub on_disk: Vec<JobSlot>,
}

/// A transfer the backend no longer lists, to look for on disk: its row and
/// the batch folder it was enqueued into (None for an adopted one).
struct Lookup {
    row: DownloadProgress,
    destination: Option<String>,
}

/// Checks each downloading slot of `batch` against `backend`. A slot's
/// current transfer is its last row's, unless its newest attempt names
/// another source the backend holds a transfer of the file from in the
/// request that attempt was sent in: a failover records its next source and
/// request before it enqueues it. Its last attempt says whether it was
/// adopted. `base` is the download folder.
///
/// `queued` are queued slots of the batch's job whose file the backend does
/// not hold: one slskd finished and cleared while Soulful was down is in the
/// batch folder already. They are looked for there once, without the waits
/// of resolve_on_disk, which give a file the backend just reported finished
/// time to show through a mount: Soulful was down, so nothing just finished.
pub(crate) async fn resume_downloads(
    backend: &Arc<dyn DownloadBackend>,
    base: &Path,
    batch: &BatchRef,
    slots: Vec<(JobSlot, SlotRecord)>,
    queued: Vec<(JobSlot, SlotRecord)>,
) -> Resumed {
    let mut resumed = Resumed::default();
    let mut unlisted = Vec::new();
    for (slot, record) in slots {
        // A slot is downloading only once a row put it there.
        let Some(row) = record.row else {
            continue;
        };
        let mut row = row.with_job(slot.clone());
        if let (Some(Some(next)), Some(request)) = (record.attempts.last(), &record.sent_in) {
            if !is_from(&row, next) {
                if let Some(transfer) = sent(backend, next, request).await {
                    resumed.abandon.push((row.source.clone(), row.id.clone()));
                    row = transfer
                        .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
                        .with_backend(backend.id())
                        .with_job(slot.clone());
                    resumed.rebound.push(row.clone());
                }
            }
        }
        let adopted = matches!(record.attempts.last(), Some(None));
        let watched = WatchedTransfer {
            source: row.source.clone(),
            filename: row.item.clone(),
            transfer_id: row.id.clone(),
            job: slot.clone(),
            adopted,
        };
        let destination = (!adopted).then(|| batch.destination.clone());
        match backend.get_download(&row.source, &row.id).await {
            Ok(Some(_)) => resumed.watch.push(watched),
            Err(e) => {
                debug!(
                    "Could not ask {} about transfer {}: {e}",
                    backend.name(),
                    row.id
                );
                resumed.watch.push(watched);
            }
            // A handoff removed the record before the import was queued, or
            // slskd dropped the record of a transfer that finished while
            // Soulful was down: the file says which.
            Ok(None) => unlisted.push(Lookup { row, destination }),
        }
    }

    let claimed = Mutex::new(HashSet::new());
    let found = look_on_disk(&unlisted, base, &claimed).await;
    for (lookup, path) in unlisted.into_iter().zip(found) {
        match path {
            Ok(path) => {
                resumed.finished.push(ResolvedSlot {
                    row: lookup.row,
                    path,
                    destination: lookup.destination,
                });
            }
            Err(why) if lookup.row.state == DownloadState::Completed => {
                resumed.failed.push(unresolved_row(lookup.row, base, why))
            }
            Err(_) => resumed.failed.push(lookup.row.failed_with(transfer_lost())),
        }
    }
    for (slot, record) in queued {
        let Some(row) = shown_row(&slot, &record) else {
            continue;
        };
        let destination = batch.destination.clone();
        if let Ok(path) = find_on_disk(&row.item, base, Some(&destination), &claimed) {
            resumed.on_disk.push(slot);
            resumed.finished.push(ResolvedSlot {
                row: row
                    .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
                    .with_backend(backend.id()),
                path,
                destination: Some(destination),
            });
        }
    }
    resumed
}

/// Looks for each lookup's file on disk at once, no file found twice.
async fn look_on_disk(
    lookups: &[Lookup],
    base: &Path,
    claimed: &Mutex<HashSet<FileId>>,
) -> Vec<Result<PathBuf, Unresolved>> {
    futures::future::join_all(
        lookups
            .iter()
            .map(|l| resolve_on_disk(&l.row.item, base, l.destination.as_deref(), claimed)),
    )
    .await
}

/// What reconciliation starts.
#[async_trait]
pub(crate) trait Resume: Send + Sync {
    /// The backend the downloads of the unfinished jobs ran on.
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure>;
    /// Watches `transfers` for `batch`, failing slots over from `pool`; an
    /// album batch imports `found`, its files already finished, with them.
    async fn watch(
        &self,
        ctx: &UserCtx,
        batch: BatchRef,
        transfers: Vec<WatchedTransfer>,
        pool: SourcePool,
        found: Vec<ResolvedSlot>,
    );
    /// Enqueues `acquisition`'s files into `fresh` and watches them.
    async fn enqueue(
        &self,
        ctx: &UserCtx,
        backend: Arc<dyn DownloadBackend>,
        fresh: NewBatch,
        acquisition: Acquisition,
    );
    /// Runs the auto-download pipeline of `run`'s job.
    async fn search(&self, ctx: UserCtx, folder: Folder, query: DownloadQuery, run: AutoRun);
}

/// The server's: the default backend, real monitors, enqueues and searches.
struct Live;

#[async_trait]
impl Resume for Live {
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
        download_backend(None)
            .await
            .map_err(|e| backend_unavailable(Stage::Transfer, &e))
    }

    async fn watch(
        &self,
        ctx: &UserCtx,
        batch: BatchRef,
        transfers: Vec<WatchedTransfer>,
        pool: SourcePool,
        found: Vec<ResolvedSlot>,
    ) {
        let tx = get_or_create_user_channel(&ctx.id()).await;
        let target = batch.target.clone();
        let monitor = DownloadMonitor::new(transfers, target, tx, batch, LEDGER.clone());
        spawn_monitor(ctx, monitor.with_failover(pool).with_found(found));
    }

    async fn enqueue(
        &self,
        ctx: &UserCtx,
        backend: Arc<dyn DownloadBackend>,
        fresh: NewBatch,
        acquisition: Acquisition,
    ) {
        let job = acquisition.job.clone();
        // Its failures are recorded on the job's slots.
        if let Err(failure) = enqueue_and_watch(&LEDGER, ctx, backend, fresh, acquisition).await {
            info!("Resumed job {job} could not enqueue: {}", failure.message);
        }
    }

    async fn search(&self, ctx: UserCtx, folder: Folder, query: DownloadQuery, run: AutoRun) {
        let ready = async {
            Ok::<_, Failure>((lastfm_key(&ctx).await?, sources::usable_backends().await?))
        };
        match ready.await {
            Ok((key, backends)) => {
                tokio::spawn(run_search(
                    LEDGER.clone(),
                    ctx,
                    folder,
                    query,
                    key,
                    backends,
                    run,
                ));
            }
            Err(failure) => fail_job(&LEDGER, &ctx, &run, failure).await,
        }
    }
}

/// Shows `rows` (each with its slot) to `user` and records them Failed.
async fn fail_rows(ledger: &Ledger, user: &UserId, rows: Vec<DownloadProgress>) {
    if rows.is_empty() {
        return;
    }
    send_user_event(user, DownloadEvent::Progress(rows.clone())).await;
    ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows,
    });
}

/// One stored import the slots of unfinished jobs share, with their rows.
struct Interrupted {
    user: UserId,
    import: StoredImport,
    rows: Vec<DownloadProgress>,
}

/// Adds `row` to the import its slot shares with other slots, so each
/// import is run or kept once.
fn share(
    imports: &mut Vec<Interrupted>,
    user: &UserId,
    import: StoredImport,
    row: DownloadProgress,
) {
    match imports
        .iter_mut()
        .find(|i| i.import.sources == import.sources)
    {
        Some(shared) => shared.rows.push(row),
        None => imports.push(Interrupted {
            user: user.clone(),
            import,
            rows: vec![row],
        }),
    }
}

/// The downloading slots of one monitor batch, with each slot's pool entry.
struct Downloads {
    ctx: UserCtx,
    batch: BatchRef,
    slots: Vec<(JobSlot, SlotRecord)>,
    /// Queued slots the backend does not hold, to look for in the batch folder.
    queued: Vec<(JobSlot, SlotRecord)>,
    pools: HashMap<JobSlot, (Vec<DownloadableItem>, usize)>,
}

impl Downloads {
    /// Adds `slot`, its pool entry being the alternates of its first source
    /// among `candidates` it has not tried, and the sources it used.
    fn add(&mut self, slot: JobSlot, record: SlotRecord, candidates: &[DownloadableGroup]) {
        let tried: Vec<DownloadableItem> = record.attempts.iter().flatten().cloned().collect();
        let alternates = match record.attempts.first() {
            Some(Some(first)) => alternates_for(first, &alternates_from(candidates, &tried)),
            _ => Vec::new(),
        };
        self.pools
            .insert(slot.clone(), (alternates, record.attempts.len()));
        self.slots.push((slot, record));
    }
}

/// The monitor batch `batch_id` of `job`'s user among `downloads`, added
/// with `label` when it is not there yet.
fn batch_of<'a>(
    downloads: &'a mut Vec<Downloads>,
    job: &UnfinishedJob,
    batch_id: &str,
    label: &str,
) -> &'a mut Downloads {
    let user = job.ctx.id();
    let at = match downloads
        .iter()
        .position(|d| d.batch.owner.user_id == user && d.batch.owner.batch_id == batch_id)
    {
        Some(at) => at,
        None => {
            downloads.push(Downloads {
                ctx: job.ctx.clone(),
                queued: Vec::new(),
                batch: BatchRef::new(
                    TransferOwner {
                        user_id: user,
                        batch_id: batch_id.to_string(),
                    },
                    label.to_string(),
                    PathBuf::from(&job.folder.path),
                    job.import_mode,
                ),
                slots: Vec::new(),
                pools: HashMap::new(),
            });
            downloads.len() - 1
        }
    };
    &mut downloads[at]
}

/// The queued slots of one job, by their first source.
struct Queued {
    job: UnfinishedJob,
    slots: Vec<(JobSlot, SlotRecord)>,
    candidates: Vec<DownloadableGroup>,
}

/// The stored search `id` of `user`, None when it is gone or unreadable.
async fn stored_search(db: &SqlitePool, id: &str, user: &UserId) -> Option<StoredSearch> {
    store::search_of(db, id, user).await.unwrap_or_else(|e| {
        warn!("Could not read search {id} to resume its job: {e}");
        None
    })
}

/// Ends `job` failed: Soulful stopped where nothing can pick it up again.
fn end_interrupted(ledger: &Ledger, job: &str, why: &str) {
    warn!("Ending job {job}, which cannot resume: {why}");
    ledger.record(JobEvent::Ended {
        job: job.to_string(),
        state: JobState::Failed,
    });
}

/// Runs a searching job's pipeline again under a new UI batch id: from the
/// candidates its search stored while that search is live and has them, else
/// searching again, after renewing the search when it is gone or expired.
async fn resume_search(
    ledger: &Ledger,
    resume: &dyn Resume,
    job: UnfinishedJob,
    search_id: String,
    search: Option<StoredSearch>,
    now: i64,
) {
    let live = search.and_then(|s| sources::live_search(Some(s), now).ok());
    let run = AutoRun {
        batch_id: uuid::Uuid::new_v4().to_string(),
        job: job.id,
        search_id: search_id.clone(),
        import_mode: job.import_mode,
        found: live
            .as_ref()
            .and_then(|s| s.candidates.clone())
            .filter(|groups| !groups.is_empty()),
    };
    if live.is_none() {
        let renewed = ledger
            .record_search(NewSearch {
                id: search_id,
                user: job.ctx.id(),
                backend: None,
                query: job.wanted.clone(),
            })
            .await;
        if let Err(e) = renewed {
            let failure = database_failure(Stage::SourceSearch, e);
            return fail_job(ledger, &job.ctx, &run, failure).await;
        }
    }
    resume.search(job.ctx, job.folder, job.wanted, run).await;
}

/// The unfinished jobs, read before Soulful serves a request, so no job a
/// request makes is taken for one left over. None when they cannot be read.
pub(crate) async fn read_unfinished(db: &SqlitePool) -> Option<Unfinished> {
    match store::unfinished(db).await {
        Ok(unfinished) => Some(unfinished),
        Err(e) => {
            warn!("Could not read the unfinished jobs to resume them: {e}");
            None
        }
    }
}

/// `resume_unfinished` of the jobs unfinished now.
#[cfg(test)]
pub(crate) async fn reconcile_in(
    db: &SqlitePool,
    ledger: &Ledger,
    queue: &ImportQueue,
    resume: &dyn Resume,
    download_base: &Path,
) {
    if let Some(unfinished) = read_unfinished(db).await {
        resume_unfinished(db, ledger, queue, resume, download_base, unfinished).await;
    }
}

/// Resumes every job of `unfinished`, each slot by its state; indexing
/// slots and jobs waiting for a source choice are left to the index runner
/// and the prune. A job or slot whose stored state cannot be read ends
/// failed, and so does a job stopped before it had a search to resume or a
/// slot to download.
pub(crate) async fn resume_unfinished(
    db: &SqlitePool,
    ledger: &Ledger,
    queue: &ImportQueue,
    resume: &dyn Resume,
    download_base: &Path,
    unfinished: Unfinished,
) {
    let Unfinished {
        jobs,
        unreadable_jobs,
        unreadable_slots,
    } = unfinished;
    for (job, e) in unreadable_jobs {
        end_interrupted(
            ledger,
            &job,
            &format!("its stored state cannot be read: {e}"),
        );
    }
    let partly_unreadable: HashSet<String> = unreadable_slots
        .iter()
        .map(|(slot, _)| slot.job.clone())
        .collect();
    if !unreadable_slots.is_empty() {
        for (slot, e) in &unreadable_slots {
            warn!("Ending {slot:?}, whose stored state cannot be read: {e}");
        }
        let slots = unreadable_slots.into_iter().map(|(slot, _)| slot).collect();
        ledger.record(JobEvent::Unreadable(slots));
    }
    if jobs.is_empty() {
        return;
    }
    info!("Resuming {} unfinished jobs", jobs.len());

    let now = chrono::Utc::now().timestamp();
    let mut kept = Vec::new();
    let mut importing = Vec::new();
    let mut downloads: Vec<Downloads> = Vec::new();
    let mut queued = Vec::new();
    for mut job in jobs {
        let user = job.ctx.id();
        let search = match &job.search_id {
            Some(id) => stored_search(db, id, &user).await,
            None => None,
        };
        if job.state == JobState::Searching {
            // An auto-download's job is made with its search.
            match job.search_id.clone() {
                Some(search_id) => resume_search(ledger, resume, job, search_id, search, now).await,
                None => end_interrupted(ledger, &job.id, "it is searching without a search"),
            }
            continue;
        }
        if job.state == JobState::Queued && job.slots.is_empty() {
            let why = if partly_unreadable.contains(&job.id) {
                "the stored state of its files cannot be read"
            } else {
                "it stopped before it had files to download"
            };
            end_interrupted(ledger, &job.id, why);
            continue;
        }

        let candidates = search.and_then(|s| s.candidates).unwrap_or_default();
        let mut waiting = Vec::new();
        for record in std::mem::take(&mut job.slots) {
            let slot = JobSlot {
                job: job.id.clone(),
                slot: record.slot,
            };
            match record.state {
                JobState::NeedsDecision | JobState::Importing => {
                    let Some(row) = shown_row(&slot, &record) else {
                        continue;
                    };
                    match record.import {
                        Some(import) if record.state == JobState::NeedsDecision => {
                            share(&mut kept, &user, import, row)
                        }
                        Some(import) => share(&mut importing, &user, import, row),
                        // Both states store their import with them.
                        None => {
                            let row = row.failed_with(import_lost());
                            fail_rows(ledger, &user, vec![row]).await
                        }
                    }
                }
                JobState::Downloading => {
                    // Its row names the batch that watched it; a job's later
                    // files may have gone to another batch.
                    let named = record.row.as_ref().and_then(|row| {
                        Some((
                            row.batch_id.clone()?,
                            row.batch_label.clone().unwrap_or_default(),
                        ))
                    });
                    let recorded = job
                        .monitor_batch_id
                        .clone()
                        .map(|id| (id, job.batch_label.clone().unwrap_or_default()));
                    let Some((batch_id, label)) = named.or(recorded) else {
                        let rows = shown_row(&slot, &record).into_iter();
                        let rows = rows.map(|row| row.failed_with(transfer_lost())).collect();
                        fail_rows(ledger, &user, rows).await;
                        continue;
                    };
                    batch_of(&mut downloads, &job, &batch_id, &label).add(
                        slot,
                        record,
                        &candidates,
                    );
                }
                JobState::Queued => waiting.push((slot, record)),
                _ => {}
            }
        }
        if !waiting.is_empty() {
            queued.push(Queued {
                job,
                slots: waiting,
                candidates,
            });
        }
    }

    for Interrupted { user, import, rows } in kept {
        if let Err(refusal) = queue
            .restore(ImportJob::from_stored(import, user.clone()))
            .await
        {
            let rows = rows
                .into_iter()
                .map(|row| row.failed_with(refusal.failure.clone()))
                .collect();
            fail_rows(ledger, &user, rows).await;
        }
    }
    for Interrupted { user, import, rows } in importing {
        let import = ImportJob::from_stored(import, user.clone());
        if import.holds_audio().await {
            queue.submit(import).await;
        } else {
            let rows = rows
                .into_iter()
                .map(|row| row.failed_with(import_lost()))
                .collect();
            fail_rows(ledger, &user, rows).await;
        }
    }

    if downloads.is_empty() && queued.is_empty() {
        return;
    }
    let backend = match resume.backend().await {
        Ok(backend) => backend,
        Err(failure) => {
            let stopped = downloads
                .into_iter()
                .map(|d| (d.ctx, d.slots))
                .chain(queued.into_iter().map(|q| (q.job.ctx, q.slots)));
            for (ctx, slots) in stopped {
                let rows = slots
                    .iter()
                    .filter_map(|(slot, record)| shown_row(slot, record))
                    .map(|row| row.failed_with(failure.clone()))
                    .collect();
                fail_rows(ledger, &ctx.id(), rows).await;
            }
            return;
        }
    };
    let mut again = Vec::new();
    for job in queued {
        if let Some(job) = adopt_sent(ledger, &backend, &mut downloads, job).await {
            again.push(job);
        }
    }
    // One batch whose files are slow to find on disk holds up no other.
    let imported: HashSet<JobSlot> = futures::future::join_all(
        downloads
            .into_iter()
            .map(|batch| resume_batch(ledger, queue, resume, &backend, download_base, batch)),
    )
    .await
    .into_iter()
    .flatten()
    .collect();
    for job in &mut again {
        job.slots.retain(|(slot, _)| !imported.contains(slot));
    }
    futures::future::join_all(
        again
            .into_iter()
            .filter(|job| !job.slots.is_empty())
            .map(|job| enqueue_again(resume, &backend, job)),
    )
    .await;
}

/// Moves the queued slots of `job` whose file the backend already holds into
/// the job's monitor batch, recording each as downloading, and hands that
/// batch the others to look for in its folder. The batch is stored before
/// the backend hears of a file, so a job without one sent nothing. Gives
/// back the slots to enqueue again unless their file is found, None when
/// none is left.
async fn adopt_sent(
    ledger: &Ledger,
    backend: &Arc<dyn DownloadBackend>,
    downloads: &mut Vec<Downloads>,
    mut queued: Queued,
) -> Option<Queued> {
    let Some(batch_id) = queued.job.monitor_batch_id.clone() else {
        return Some(queued);
    };
    let label = queued.job.batch_label.clone().unwrap_or_default();
    let mut rest = Vec::new();
    let mut rows = Vec::new();
    for (slot, mut record) in std::mem::take(&mut queued.slots) {
        let held = match (record.attempts.first(), &queued.job.request) {
            (Some(Some(first)), Some(request)) => sent(backend, first, request).await,
            _ => None,
        };
        let Some(transfer) = held else {
            batch_of(downloads, &queued.job, &batch_id, &label)
                .queued
                .push((slot.clone(), record.clone()));
            rest.push((slot, record));
            continue;
        };
        let row = transfer
            .with_batch(batch_id.clone(), label.clone())
            .with_backend(backend.id())
            .with_job(slot.clone());
        rows.push(row.clone());
        record.row = Some(row);
        record.state = JobState::Downloading;
        batch_of(downloads, &queued.job, &batch_id, &label).add(slot, record, &queued.candidates);
    }
    if !rows.is_empty() {
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows,
        });
    }
    queued.slots = rest;
    (!queued.slots.is_empty()).then_some(queued)
}

/// Resumes one monitor batch: fails the transfers slskd lost, and watches the
/// rest. The files of the batch found on disk (finished transfers slskd no
/// longer lists, queued files in its folder) are finished slots of it: a
/// singleton batch imports each now, and an album batch hands them to its
/// monitor, which imports them with the watched files once every slot
/// settled, as a live batch does, or imports them now when it watches
/// nothing. A transfer a request made since startup now holds is that
/// request's: its slot is dismissed when the request downloads the file into
/// the same folder for the same user, and fails as that request's claim says
/// otherwise. Gives back the queued slots whose file it imports.
async fn resume_batch(
    ledger: &Ledger,
    queue: &ImportQueue,
    resume: &dyn Resume,
    backend: &Arc<dyn DownloadBackend>,
    download_base: &Path,
    downloads: Downloads,
) -> Vec<JobSlot> {
    let Downloads {
        ctx,
        batch,
        slots,
        queued,
        mut pools,
    } = downloads;
    let user = ctx.id();
    let Resumed {
        watch,
        finished,
        failed: lost,
        rebound,
        abandon,
        on_disk,
    } = resume_downloads(backend, download_base, &batch, slots, queued).await;
    fail_rows(ledger, &user, lost).await;
    if !rebound.is_empty() {
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: rebound,
        });
    }
    // As the failover would have once its next source was enqueued.
    for (source, id) in abandon {
        if let Err(e) = backend.cancel_download(&source, &id, false).await {
            warn!("Failed to cancel abandoned transfer {id} in slskd: {e}");
        }
    }
    let found = match batch.import_mode {
        ImportMode::Album => finished,
        ImportMode::Singleton => {
            for f in finished {
                let import = import_job(
                    f.path,
                    ImportMode::Singleton,
                    vec![f.row],
                    &batch.target,
                    &user,
                    download_base,
                );
                queue.submit(import).await;
            }
            Vec::new()
        }
    };

    let registry = TransferRegistry::global();
    let mut watched = Vec::new();
    let mut dismissed = Vec::new();
    let mut held = Vec::new();
    for w in watch {
        let claim = registry.claim(
            &[transfer_key(backend.id(), &w.source, &w.filename)],
            &batch,
        );
        if !claim.claimed.is_empty() {
            watched.push(w);
            continue;
        }
        let failure = held_failure(&claim, &batch);
        if failure.code == FailureCode::AlreadyRunning {
            dismissed.push(w.job);
        } else {
            held.push(
                DownloadProgress::failed(w.transfer_id, w.source, w.filename, failure)
                    .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
                    .with_job(w.job),
            );
        }
    }
    if !dismissed.is_empty() {
        ledger.record(JobEvent::Dismiss(dismissed));
    }
    fail_rows(ledger, &user, held).await;
    if watched.is_empty() {
        // Every slot of the album settled: Soulful stopped inside
        // finish_album (D15), or slskd finished and cleared the rest.
        for import in album_jobs(found, &batch.target, &user, download_base) {
            queue.submit(import).await;
        }
        return on_disk;
    }
    // Every watched transfer is one of the slots given, each with its entry.
    let pool = SourcePool::resumed(
        watched
            .iter()
            .map(|w| pools.remove(&w.job).unwrap_or_default())
            .collect(),
    );
    resume.watch(&ctx, batch, watched, pool, found).await;
    on_disk
}

/// Enqueues a job's queued slots again by their first sources, into a new
/// batch importing in the job's mode, the search's other candidates standing
/// by as alternates.
async fn enqueue_again(resume: &dyn Resume, backend: &Arc<dyn DownloadBackend>, queued: Queued) {
    let Queued {
        job,
        slots,
        candidates,
    } = queued;
    let items: Vec<(u32, DownloadableItem)> = slots
        .into_iter()
        .filter_map(|(slot, record)| Some((slot.slot, record.attempts.into_iter().next()??)))
        .collect();
    let picked: Vec<DownloadableItem> = items.iter().map(|(_, item)| item.clone()).collect();
    let fresh = NewBatch {
        batch_id: uuid::Uuid::new_v4().to_string(),
        label: manual_batch_label(&picked),
        target: PathBuf::from(&job.folder.path),
        import_mode: job.import_mode,
    };
    let acquisition = Acquisition {
        job: job.id,
        alternates: alternates_from(&candidates, &picked),
        items,
    };
    resume
        .enqueue(&job.ctx, backend.clone(), fresh, acquisition)
        .await;
}

/// Reads the server's unfinished jobs, then resumes them in the background.
/// Awaited before the server takes its first request; a second call (a hot
/// reload of the serve closure) resumes nothing.
pub async fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(unfinished) = read_unfinished(&crate::db::DB).await else {
        return;
    };
    tokio::spawn(resume_unfinished(
        &crate::db::DB,
        &LEDGER,
        &IMPORT_QUEUE,
        &Live,
        CONFIG.download_path(),
        unfinished,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::ledger::{
        store, ImportDone, IndexWait, NewJob, NewSearch, StoredImport, SEARCH_KEPT_SECS,
    };
    use crate::server_fns::download::test_support::{batch, group, item, transfer, StubBackend};
    use shared::download::{DownloadQuery, DownloadState, DownloadableItem, JobSlot};
    use shared::failure::{FailureCode, RetryAdvice, Stage};
    use soulbeet::{DriverChoice, ImportedItem};
    use std::sync::Mutex;
    use std::time::Duration;

    fn record(
        slot: u32,
        state: JobState,
        row: DownloadProgress,
        attempts: Vec<Option<DownloadableItem>>,
    ) -> SlotRecord {
        SlotRecord {
            slot,
            state,
            row: Some(row),
            import: None,
            attempts,
            sent_in: None,
        }
    }

    // Paused: resolve_on_disk backs off with tokio::time::sleep.
    #[tokio::test(start_paused = true)]
    async fn a_finished_download_slskd_no_longer_lists_is_imported_from_its_folder() {
        let base = tempfile::tempdir().unwrap();
        let b = batch("Come Over");
        std::fs::create_dir_all(base.path().join(&b.destination)).unwrap();
        std::fs::write(
            base.path().join(&b.destination).join("07 - Wake Up.flac"),
            b"flac",
        )
        .unwrap();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let row = transfer(
            "peer_a",
            "music\\Kowloon\\Come Over\\07 - Wake Up.flac",
            "t-07",
            DownloadState::Completed,
        );
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };

        let resumed = resume_downloads(
            &backend,
            base.path(),
            &b,
            vec![(
                slot.clone(),
                record(
                    0,
                    JobState::Downloading,
                    row,
                    vec![Some(item("peer_a", "Wake Up", "07.flac"))],
                ),
            )],
            vec![],
        )
        .await;

        assert!(resumed.watch.is_empty() && resumed.failed.is_empty());
        assert_eq!(
            resumed
                .finished
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            vec![base.path().join(&b.destination).join("07 - Wake Up.flac")]
        );
        assert_eq!(resumed.finished[0].row.job, Some(slot));
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_slskd_still_has_is_watched_and_one_it_lost_fails() {
        let base = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        stub.set_listing(vec![transfer(
            "peer_a",
            "a/01.flac",
            "t-01",
            DownloadState::InProgress,
        )]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let slot = |n| JobSlot {
            job: "j1".into(),
            slot: n,
        };
        let resumed = resume_downloads(
            &backend,
            base.path(),
            &batch("X"),
            vec![
                (
                    slot(0),
                    record(
                        0,
                        JobState::Downloading,
                        transfer("peer_a", "a/01.flac", "t-01", DownloadState::InProgress),
                        vec![Some(item("peer_a", "One", "a/01.flac"))],
                    ),
                ),
                (
                    slot(1),
                    record(
                        1,
                        JobState::Downloading,
                        transfer("peer_a", "a/02.flac", "t-02", DownloadState::Queued),
                        vec![None],
                    ),
                ),
            ],
            vec![],
        )
        .await;
        assert_eq!(
            resumed
                .watch
                .iter()
                .map(|w| (w.transfer_id.as_str(), w.job.clone(), w.adopted))
                .collect::<Vec<_>>(),
            vec![("t-01", slot(0), false)]
        );
        assert_eq!(
            resumed
                .failed
                .iter()
                .map(|r| (r.item.as_str(), r.failure.clone()))
                .collect::<Vec<_>>(),
            vec![("a/02.flac", Some(transfer_lost()))]
        );
        assert_eq!(resumed.failed[0].job, Some(slot(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn an_album_stopped_while_importing_watches_its_listed_files_beside_the_gone_ones() {
        let base = tempfile::tempdir().unwrap();
        let b = BatchRef {
            import_mode: ImportMode::Album,
            ..batch("Come Over")
        };
        let folder = base.path().join(&b.destination);
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["01.flac", "02.flac"] {
            std::fs::write(folder.join(name), b"flac").unwrap();
        }
        let stub = StubBackend::new(vec![]);
        stub.set_listing(vec![transfer(
            "peer_a",
            "a\\02.flac",
            "t-02",
            DownloadState::Completed,
        )]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let slot = |n| JobSlot {
            job: "j1".into(),
            slot: n,
        };
        let resumed = resume_downloads(
            &backend,
            base.path(),
            &b,
            vec![
                (
                    slot(0),
                    record(
                        0,
                        JobState::Downloading,
                        transfer("peer_a", "a\\01.flac", "t-01", DownloadState::Completed),
                        vec![Some(item("peer_a", "One", "a\\01.flac"))],
                    ),
                ),
                (
                    slot(1),
                    record(
                        1,
                        JobState::Downloading,
                        transfer("peer_a", "a\\02.flac", "t-02", DownloadState::Completed),
                        vec![Some(item("peer_a", "Two", "a\\02.flac"))],
                    ),
                ),
            ],
            vec![],
        )
        .await;
        assert!(resumed.failed.is_empty());
        // The monitor imports both on its first poll, 02 being Completed.
        assert_eq!(
            resumed
                .watch
                .iter()
                .map(|w| (w.transfer_id.as_str(), w.job.clone()))
                .collect::<Vec<_>>(),
            vec![("t-02", slot(1))]
        );
        assert_eq!(
            resumed
                .finished
                .iter()
                .map(|f| (f.path.clone(), f.row.job.clone()))
                .collect::<Vec<_>>(),
            vec![(folder.join("01.flac"), Some(slot(0)))]
        );
    }

    #[test]
    fn restart_failures_say_where_the_work_stopped() {
        for (f, stage, retry, message) in [
            (
                transfer_lost(),
                Stage::Transfer,
                RetryAdvice::SameSourceLater,
                "Soulful restarted and slskd no longer has this download",
            ),
            (
                import_lost(),
                Stage::Import,
                RetryAdvice::NotRetryable,
                "Soulful restarted during this import and the downloaded files are gone",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (FailureCode::ServerRestarted, stage, retry, message)
            );
        }
    }

    /// batch id, label, (transfer id, slot)
    type WatchStart = (String, String, Vec<(String, JobSlot)>);
    /// job, (slot, item id), alternate sources, mode
    type EnqueueStart = (String, Vec<(u32, String)>, Vec<String>, ImportMode);

    /// Records what reconciliation started; its backend is `backend`.
    struct Started {
        backend: Arc<dyn DownloadBackend>,
        watched: Mutex<Vec<WatchStart>>,
        /// The path and slot of each file a watched batch was handed found.
        found: Mutex<Vec<(PathBuf, Option<JobSlot>)>>,
        enqueued: Mutex<Vec<EnqueueStart>>,
        /// job, search id, stored groups
        searched: Mutex<Vec<(String, String, Option<usize>)>>,
    }

    impl Started {
        fn on(backend: Arc<dyn DownloadBackend>) -> Started {
            Started {
                backend,
                watched: Mutex::default(),
                found: Mutex::default(),
                enqueued: Mutex::default(),
                searched: Mutex::default(),
            }
        }

        fn nothing(&self) -> bool {
            self.watched.lock().unwrap().is_empty()
                && self.enqueued.lock().unwrap().is_empty()
                && self.searched.lock().unwrap().is_empty()
        }
    }

    #[async_trait]
    impl Resume for Started {
        async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
            Ok(self.backend.clone())
        }
        async fn watch(
            &self,
            _: &UserCtx,
            batch: BatchRef,
            transfers: Vec<WatchedTransfer>,
            _: SourcePool,
            found: Vec<ResolvedSlot>,
        ) {
            self.found
                .lock()
                .unwrap()
                .extend(found.into_iter().map(|f| (f.path, f.row.job)));
            self.watched.lock().unwrap().push((
                batch.owner.batch_id,
                batch.label,
                transfers
                    .into_iter()
                    .map(|t| (t.transfer_id, t.job))
                    .collect(),
            ));
        }
        async fn enqueue(
            &self,
            _: &UserCtx,
            _: Arc<dyn DownloadBackend>,
            fresh: NewBatch,
            a: Acquisition,
        ) {
            self.enqueued.lock().unwrap().push((
                a.job,
                a.items.into_iter().map(|(s, i)| (s, i.id)).collect(),
                a.alternates.into_iter().map(|g| g.source).collect(),
                fresh.import_mode,
            ));
        }
        async fn search(&self, _: UserCtx, _: Folder, _: DownloadQuery, run: AutoRun) {
            self.searched.lock().unwrap().push((
                run.job,
                run.search_id,
                run.found.map(|g| g.len()),
            ));
        }
    }

    /// alice's singleton job `id` in her folder in `state`, its slots picked from `items` when there are any.
    async fn job_of(
        ledger: &Ledger,
        db: &SqlitePool,
        id: &str,
        state: JobState,
        search: Option<&str>,
        items: Vec<(u32, DownloadableItem)>,
    ) {
        let folder_id = Folder::get_by_id_in(db, "alice-folder")
            .await
            .unwrap()
            .unwrap()
            .id;
        ledger
            .create(
                NewJob {
                    id: id.into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state,
                    search_id: search.map(str::to_string),
                    picked: vec![],
                },
                None,
            )
            .await
            .unwrap();
        if !items.is_empty() {
            ledger.record(JobEvent::Picked {
                job: id.into(),
                items,
            });
        }
    }

    fn search(id: &str) -> NewSearch {
        NewSearch {
            id: id.into(),
            user: UserId::new("alice"),
            backend: None,
            query: DownloadQuery::new(vec![]),
        }
    }

    /// A test database with alice and her folder, and a ledger writing to it.
    async fn ledger() -> (Ledger, SqlitePool) {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        (Ledger::start(db.clone()), db)
    }

    fn queue() -> ImportQueue {
        ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())))
    }

    #[tokio::test]
    async fn each_unfinished_state_resumes_its_own_way() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        stub.set_listing(vec![transfer(
            "peer_a",
            "d/01.flac",
            "t-d01",
            DownloadState::InProgress,
        )]);
        let started = Started::on(stub.clone());
        let importer = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(importer.clone()));

        // queued, its search holding another source
        ledger.record_search(search("s-q")).await.unwrap();
        ledger
            .store_candidates(
                "s-q",
                vec![
                    group("peer_a", vec![item("peer_a", "One", "q/01.flac")], 0.9),
                    group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.8),
                ],
            )
            .await
            .unwrap();
        job_of(
            &ledger,
            &db,
            "queued",
            JobState::Queued,
            Some("s-q"),
            vec![(0, item("peer_a", "One", "q/01.flac"))],
        )
        .await;
        // searching, one search with candidates and one without
        ledger.record_search(search("s-found")).await.unwrap();
        ledger
            .store_candidates(
                "s-found",
                vec![group(
                    "peer_a",
                    vec![item("peer_a", "One", "f/01.flac")],
                    0.9,
                )],
            )
            .await
            .unwrap();
        job_of(
            &ledger,
            &db,
            "found",
            JobState::Searching,
            Some("s-found"),
            vec![],
        )
        .await;
        ledger.record_search(search("s-again")).await.unwrap();
        job_of(
            &ledger,
            &db,
            "again",
            JobState::Searching,
            Some("s-again"),
            vec![],
        )
        .await;
        // downloading in monitor batch b1
        job_of(
            &ledger,
            &db,
            "downloading",
            JobState::Queued,
            None,
            vec![(0, item("peer_a", "One", "d/01.flac"))],
        )
        .await;
        enqueued_in_b1(&ledger, "downloading");
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![
                transfer("peer_a", "d/01.flac", "t-d01", DownloadState::InProgress).with_job(
                    JobSlot {
                        job: "downloading".into(),
                        slot: 0,
                    },
                ),
            ],
        });
        // importing: one whose files are there, one whose files are gone
        for (id, present) in [("importing", true), ("import-gone", false)] {
            job_of(
                &ledger,
                &db,
                id,
                JobState::Queued,
                None,
                vec![(0, item("peer_a", "One", "i/01.flac"))],
            )
            .await;
            let source = tmp.path().join(id).join("01.flac");
            if present {
                std::fs::create_dir_all(source.parent().unwrap()).unwrap();
                std::fs::write(&source, b"flac").unwrap();
            }
            let slot = JobSlot {
                job: id.into(),
                slot: 0,
            };
            let row = DownloadProgress::queued(
                "t-i01".into(),
                "peer_a".into(),
                format!("{id}/01.flac"),
                4,
            )
            .with_state(DownloadState::ImportQueued(1))
            .with_job(slot.clone());
            ledger.record(JobEvent::ImportQueued(Box::new(StoredImport {
                sources: vec![source],
                target: tmp.path().join("lib"),
                album: false,
                cleanup_root: None,
                choices: None,
                choice: DriverChoice::Auto,
                rows: vec![(slot, row)],
            })));
        }
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(
            *started.enqueued.lock().unwrap(),
            vec![(
                "queued".to_string(),
                vec![(0, "q/01.flac".to_string())],
                vec!["peer_b".to_string()],
                ImportMode::Singleton
            )]
        );
        assert_eq!(
            *started.searched.lock().unwrap(),
            vec![
                ("found".into(), "s-found".into(), Some(1)),
                ("again".into(), "s-again".into(), None)
            ]
        );
        assert_eq!(
            *started.watched.lock().unwrap(),
            vec![(
                "b1".into(),
                "Come Over".into(),
                vec![(
                    "t-d01".into(),
                    JobSlot {
                        job: "downloading".into(),
                        slot: 0
                    }
                )]
            )]
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while importer.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the interrupted import ran again");
        assert_eq!(
            store::job_state(&db, "import-gone").await,
            Some(JobState::Failed)
        );
    }

    #[tokio::test]
    async fn a_kept_import_is_kept_again_and_one_whose_files_are_gone_fails() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let queue = queue();
        for (id, present) in [("kept", true), ("kept-gone", false)] {
            job_of(
                &ledger,
                &db,
                id,
                JobState::Queued,
                None,
                vec![(0, item("peer_a", "One", "k/01.flac"))],
            )
            .await;
            let source = tmp.path().join(id).join("01.flac");
            if present {
                std::fs::create_dir_all(source.parent().unwrap()).unwrap();
                std::fs::write(&source, b"flac").unwrap();
            }
            let slot = JobSlot {
                job: id.into(),
                slot: 0,
            };
            let row = DownloadProgress::queued(
                "t-k01".into(),
                "peer_a".into(),
                format!("{id}/01.flac"),
                4,
            )
            .with_job(slot.clone());
            let import = StoredImport {
                sources: vec![source],
                target: tmp.path().join("lib"),
                album: false,
                cleanup_root: None,
                choices: None,
                choice: DriverChoice::Auto,
                rows: vec![(slot, row.clone().with_state(DownloadState::ImportQueued(1)))],
            };
            // As the import queue's finish records a kept import.
            ledger.record(JobEvent::ImportQueued(Box::new(import.clone())));
            ledger.record(JobEvent::ImportDone(Box::new(ImportDone {
                import,
                rows: vec![row.with_state(DownloadState::ImportSkipped)],
                next: JobState::NeedsDecision,
                index: None,
            })));
        }
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        assert!(queue
            .import_choices(&UserId::new("alice"), "kept/01.flac")
            .is_ok());
        assert_eq!(
            store::job_state(&db, "kept").await,
            Some(JobState::NeedsDecision)
        );
        assert_eq!(
            store::job_state(&db, "kept-gone").await,
            Some(JobState::Failed)
        );
        let row: String =
            sqlx::query_scalar("SELECT last_row FROM job_tracks WHERE job_id = 'kept-gone'")
                .fetch_one(&db)
                .await
                .unwrap();
        let row: DownloadProgress = serde_json::from_str(&row).unwrap();
        assert_eq!(
            row.failure.map(|f| f.code),
            Some(FailureCode::ImportFilesGone)
        );
        assert!(started.nothing());
    }

    #[tokio::test]
    async fn an_indexing_job_is_left_to_the_index_runner() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            vec![(0, item("peer_a", "One", "01.flac"))],
        )
        .await;
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        let row = DownloadProgress::queued("t-01".into(), "peer_a".into(), "01.flac".into(), 4)
            .with_state(DownloadState::Imported)
            .with_job(slot.clone());
        ledger.record(JobEvent::Slots {
            state: JobState::Indexing,
            rows: vec![row],
        });
        ledger.record(JobEvent::Indexing {
            slots: vec![slot],
            wait: Box::new(IndexWait {
                import: "/downloads/Come Over [1f2e3d4c]/01.flac".into(),
                library: "/music".into(),
                items: vec![ImportedItem {
                    path: "Kowloon/Come Over/01 One.flac".into(),
                    recording_mbid: None,
                    artist: "Kowloon".into(),
                    title: "One".into(),
                    album: "Come Over".into(),
                }],
                since: 0,
                baseline: None,
                checked: None,
            }),
        });
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Indexing));
        assert!(started.nothing());
    }

    /// slskd's URL is not set, so no backend is available; nothing may start.
    struct Unavailable;

    fn unset() -> Failure {
        backend_unavailable(
            Stage::Transfer,
            &crate::services::BackendLookupError::Backend(
                soulbeet::error::SoulseekError::NotConfigured(soulbeet::error::MissingSetting::Url),
            ),
        )
    }

    #[async_trait]
    impl Resume for Unavailable {
        async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
            Err(unset())
        }
        async fn watch(
            &self,
            _: &UserCtx,
            _: BatchRef,
            _: Vec<WatchedTransfer>,
            _: SourcePool,
            _: Vec<ResolvedSlot>,
        ) {
            unreachable!("nothing is watched without a backend")
        }
        async fn enqueue(
            &self,
            _: &UserCtx,
            _: Arc<dyn DownloadBackend>,
            _: NewBatch,
            _: Acquisition,
        ) {
            unreachable!("nothing is enqueued without a backend")
        }
        async fn search(&self, _: UserCtx, _: Folder, _: DownloadQuery, _: AutoRun) {
            unreachable!("no job here searches")
        }
    }

    #[tokio::test]
    async fn without_a_backend_queued_and_downloading_slots_fail_with_its_failure() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        for (id, state) in [
            ("queued", None),
            ("downloading", Some(JobState::Downloading)),
        ] {
            job_of(
                &ledger,
                &db,
                id,
                JobState::Queued,
                None,
                vec![(0, item("peer_a", "One", "01.flac"))],
            )
            .await;
            if let Some(state) = state {
                enqueued_in_b1(&ledger, id);
                ledger.record(JobEvent::Slots {
                    state,
                    rows: vec![
                        transfer("peer_a", "01.flac", "t-01", DownloadState::InProgress).with_job(
                            JobSlot {
                                job: id.into(),
                                slot: 0,
                            },
                        ),
                    ],
                });
            }
        }
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &Unavailable, tmp.path()).await;
        ledger.settled().await;

        for id in ["queued", "downloading"] {
            assert_eq!(
                store::job_state(&db, id).await,
                Some(JobState::Failed),
                "{id}"
            );
            let row: String =
                sqlx::query_scalar("SELECT last_row FROM job_tracks WHERE job_id = ?")
                    .bind(id)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            let row: DownloadProgress = serde_json::from_str(&row).unwrap();
            assert_eq!(
                (row.item.as_str(), row.failure),
                ("01.flac", Some(unset())),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn a_searching_job_whose_search_was_pruned_searches_again_under_a_renewed_search() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let alice = UserId::new("alice");
        ledger.record_search(search("s-old")).await.unwrap();
        ledger
            .store_candidates(
                "s-old",
                vec![group(
                    "peer_a",
                    vec![item("peer_a", "One", "a/01.flac")],
                    0.9,
                )],
            )
            .await
            .unwrap();
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Searching,
            Some("s-old"),
            vec![],
        )
        .await;
        let later = chrono::Utc::now().timestamp() + SEARCH_KEPT_SECS + 1;
        assert_eq!(ledger.prune(later).await.unwrap().searches, 1);
        assert_eq!(store::search_of(&db, "s-old", &alice).await.unwrap(), None);

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;

        assert_eq!(
            *started.searched.lock().unwrap(),
            vec![("j1".into(), "s-old".into(), None)]
        );
        let renewed = store::search_of(&db, "s-old", &alice)
            .await
            .unwrap()
            .expect("the search is stored again");
        assert!(renewed.expires_at > chrono::Utc::now().timestamp());
        assert_eq!(renewed.candidates, None);
    }

    /// `id`'s slot `slot` row as stored.
    async fn stored_row(db: &SqlitePool, id: &str, slot: u32) -> DownloadProgress {
        let row: String =
            sqlx::query_scalar("SELECT last_row FROM job_tracks WHERE job_id = ? AND slot = ?")
                .bind(id)
                .bind(slot)
                .fetch_one(db)
                .await
                .unwrap();
        serde_json::from_str(&row).unwrap()
    }

    /// The backend request `enqueued_in_b1` records.
    const B1_REQUEST: &str = "r-b1";

    /// Records `job` as the batch `b1` ("Come Over") took its files in B1_REQUEST.
    fn enqueued_in_b1(ledger: &Ledger, job: &str) {
        ledger.record(JobEvent::Enqueued {
            job: job.into(),
            monitor_batch_id: "b1".into(),
            batch_label: "Come Over".into(),
            request: B1_REQUEST.into(),
        });
    }

    #[tokio::test]
    async fn a_job_made_after_the_startup_read_is_not_resumed() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let one = || vec![(0, item("peer_a", "One", "01.flac"))];
        job_of(&ledger, &db, "before", JobState::Queued, None, one()).await;
        ledger.settled().await;
        let unfinished = read_unfinished(&db).await.unwrap();
        // A request served once the read was done.
        job_of(&ledger, &db, "after", JobState::Queued, None, one()).await;
        ledger.settled().await;

        resume_unfinished(&db, &ledger, &queue(), &started, tmp.path(), unfinished).await;

        let jobs: Vec<String> = started
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|(job, ..)| job.clone())
            .collect();
        assert_eq!(jobs, vec!["before".to_string()]);
    }

    #[tokio::test]
    async fn a_transfer_a_request_since_startup_holds_is_left_to_it() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::core::folder_of(&db, "bob").await;
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        stub.set_listing(vec![
            transfer("peer_a", "a/01.flac", "t-01", DownloadState::InProgress),
            transfer("peer_a", "a/02.flac", "t-02", DownloadState::InProgress),
            transfer("peer_a", "a/03.flac", "t-03", DownloadState::InProgress),
        ]);
        let files = ["a/01.flac", "a/02.flac", "a/03.flac"];
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            (0..)
                .zip(files)
                .map(|(n, f)| (n, item("peer_a", "One", f)))
                .collect(),
        )
        .await;
        enqueued_in_b1(&ledger, "j1");
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: (0..)
                .zip(files)
                .map(|(n, f)| {
                    transfer(
                        "peer_a",
                        f,
                        &format!("t-0{}", n + 1),
                        DownloadState::InProgress,
                    )
                    .with_job(JobSlot {
                        job: "j1".into(),
                        slot: n,
                    })
                })
                .collect(),
        });
        ledger.settled().await;
        // Requests served since startup: alice's into her folder holds 01, bob's holds 02.
        let holder = |user: &str, target: &str| {
            BatchRef::new(
                TransferOwner {
                    user_id: UserId::new(user),
                    batch_id: uuid::Uuid::new_v4().to_string(),
                },
                "Come Over".into(),
                PathBuf::from(target),
                ImportMode::Singleton,
            )
        };
        let registry = TransferRegistry::global();
        registry.claim(
            &[transfer_key(backend.id(), "peer_a", "a/01.flac")],
            &holder("alice", "/music/alice"),
        );
        registry.claim(
            &[transfer_key(backend.id(), "peer_a", "a/02.flac")],
            &holder("bob", "/music/bob"),
        );
        let started = Started::on(backend);

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        let watched: Vec<String> = started.watched.lock().unwrap()[0]
            .2
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(watched, vec!["t-03".to_string()]);
        let states: Vec<String> =
            sqlx::query_scalar("SELECT state FROM job_tracks WHERE job_id = 'j1' ORDER BY slot")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(states, vec!["dismissed", "failed", "downloading"]);
        assert_eq!(
            stored_row(&db, "j1", 1).await.failure.map(|f| f.code),
            Some(FailureCode::AlreadyInFlightOtherUser)
        );
    }

    #[tokio::test]
    async fn jobs_stopped_before_they_could_resume_end_failed() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        job_of(&ledger, &db, "no-search", JobState::Searching, None, vec![]).await;
        job_of(&ledger, &db, "no-files", JobState::Queued, None, vec![]).await;
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        for id in ["no-search", "no-files"] {
            assert_eq!(
                store::job_state(&db, id).await,
                Some(JobState::Failed),
                "{id}"
            );
        }
        assert!(started.nothing());
    }

    #[tokio::test]
    async fn a_queued_file_slskd_already_took_is_watched_and_the_rest_enqueued_again() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        let started = Started::on(stub.clone());
        let picked = || {
            vec![
                (0, item("peer_a", "One", "q/01.flac")),
                (1, item("peer_b", "Two", "q/02.flac")),
            ]
        };
        job_of(&ledger, &db, "sent", JobState::Queued, None, picked()).await;
        enqueued_in_b1(&ledger, "sent");
        // Stopped before its batch was recorded: slskd heard of nothing it holds.
        job_of(&ledger, &db, "unsent", JobState::Queued, None, picked()).await;
        ledger.settled().await;
        // Soulful stopped while slskd took the batch: it holds 01 but not 02.
        stub.set_listing(vec![in_request(
            transfer("peer_a", "q/01.flac", "t-q01", DownloadState::Queued),
            B1_REQUEST,
        )]);

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(
            *started.watched.lock().unwrap(),
            vec![(
                "b1".into(),
                "Come Over".into(),
                vec![(
                    "t-q01".into(),
                    JobSlot {
                        job: "sent".into(),
                        slot: 0
                    }
                )]
            )]
        );
        let enqueued: Vec<(String, Vec<(u32, String)>)> = started
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|(job, items, ..)| (job.clone(), items.clone()))
            .collect();
        assert_eq!(
            enqueued,
            vec![
                ("sent".to_string(), vec![(1, "q/02.flac".to_string())]),
                (
                    "unsent".to_string(),
                    vec![(0, "q/01.flac".to_string()), (1, "q/02.flac".to_string())]
                ),
            ]
        );
        let row = stored_row(&db, "sent", 0).await;
        assert_eq!(
            (row.id.as_str(), row.batch_id.as_deref()),
            ("t-q01", Some("b1"))
        );
    }

    /// `row` as the backend lists a transfer it made in `request`.
    fn in_request(row: DownloadProgress, request: &str) -> DownloadProgress {
        DownloadProgress {
            request: Some(request.into()),
            ..row
        }
    }

    #[tokio::test]
    async fn a_record_of_the_file_another_request_made_is_not_the_jobs() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        let started = Started::on(stub.clone());
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            vec![
                (0, item("peer_a", "One", "q/01.flac")),
                (1, item("peer_a", "Two", "q/02.flac")),
            ],
        )
        .await;
        enqueued_in_b1(&ledger, "j1");
        ledger.settled().await;
        // Earlier downloads of peer_a's 01 finished once, failed once and ran once from a request
        // that was no Soulful's; slskd took 02 in the job's request.
        stub.set_listing(vec![
            in_request(
                transfer("peer_a", "q/01.flac", "t-old1", DownloadState::Completed),
                "r-old",
            ),
            in_request(
                transfer(
                    "peer_a",
                    "q/01.flac",
                    "t-old2",
                    DownloadState::Failed("Transfer failed".into()),
                ),
                "r-older",
            ),
            transfer("peer_a", "q/01.flac", "t-old3", DownloadState::InProgress),
            in_request(
                transfer("peer_a", "q/02.flac", "t-q02", DownloadState::Queued),
                B1_REQUEST,
            ),
        ]);

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        let watched: Vec<Vec<(String, JobSlot)>> = started
            .watched
            .lock()
            .unwrap()
            .iter()
            .map(|(.., transfers)| transfers.clone())
            .collect();
        assert_eq!(
            watched,
            vec![vec![(
                "t-q02".to_string(),
                JobSlot {
                    job: "j1".into(),
                    slot: 1
                }
            )]]
        );
        let enqueued: Vec<(String, Vec<(u32, String)>)> = started
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|(job, items, ..)| (job.clone(), items.clone()))
            .collect();
        assert_eq!(
            enqueued,
            vec![("j1".to_string(), vec![(0, "q/01.flac".to_string())])]
        );
    }

    /// Keeps the message of every event logged while it is this thread's subscriber.
    struct Logged(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Logged {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Message<'a>(&'a mut String);
            impl tracing::field::Visit for Message<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        *self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = String::new();
            event.record(&mut Message(&mut message));
            self.0.lock().unwrap().push(message);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn a_queued_job_whose_files_cannot_be_read_ends_saying_so() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        // A current-thread runtime: everything the test runs logs here.
        let _logging = tracing::subscriber::set_default(Logged(lines.clone()));
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            vec![(0, item("peer_a", "One", "01.flac"))],
        )
        .await;
        ledger.settled().await;
        sqlx::query("UPDATE job_tracks SET last_row = '{' WHERE job_id = 'j1'")
            .execute(&db)
            .await
            .unwrap();
        // A callsite another test registered while this subscriber was being set may have
        // cached that nothing listens: ask every callsite again.
        tracing::callsite::rebuild_interest_cache();

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
        let ended: Vec<String> = lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("Ending job j1"))
            .cloned()
            .collect();
        assert_eq!(
            ended,
            vec![
                "Ending job j1, which cannot resume: the stored state of its files cannot be read"
            ]
        );
        assert!(started.nothing());
    }

    #[tokio::test]
    async fn a_queued_file_slskd_finished_and_cleared_is_imported_and_not_enqueued_again() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let b1 = BatchRef::new(
            TransferOwner {
                user_id: UserId::new("alice"),
                batch_id: "b1".into(),
            },
            "Come Over".into(),
            tmp.path().join("music"),
            ImportMode::Singleton,
        );
        // slskd took both files of b1 and finished 01 while Soulful was down, and its record
        // was cleared; it holds nothing now.
        let folder = tmp.path().join(&b1.destination);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"flac").unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let importer = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(importer.clone()));
        let library = tmp.path().join("music");
        sqlx::query("UPDATE folders SET path = ? WHERE id = 'alice-folder'")
            .bind(library.to_string_lossy())
            .execute(&db)
            .await
            .unwrap();
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            vec![
                (0, item("peer_a", "One", "q\\01.flac")),
                (1, item("peer_a", "Two", "q\\02.flac")),
            ],
        )
        .await;
        enqueued_in_b1(&ledger, "j1");
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while importer.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the finished file was imported");
        assert_eq!(
            importer.seen().into_iter().collect::<Vec<_>>(),
            vec![(vec![folder.join("01.flac")], library)]
        );
        let enqueued: Vec<(String, Vec<(u32, String)>)> = started
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|(job, items, ..)| (job.clone(), items.clone()))
            .collect();
        assert_eq!(
            enqueued,
            vec![("j1".to_string(), vec![(1, "q\\02.flac".to_string())])]
        );
    }

    /// alice's album job `j1` with `slots` picked, its files sent into batch b1 in B1_REQUEST,
    /// her folder at `library`; gives b1 as its monitor resumes it.
    async fn album_in_b1(
        ledger: &Ledger,
        db: &SqlitePool,
        library: &Path,
        slots: Vec<(u32, DownloadableItem)>,
    ) -> BatchRef {
        sqlx::query("UPDATE folders SET path = ? WHERE id = 'alice-folder'")
            .bind(library.to_string_lossy())
            .execute(db)
            .await
            .unwrap();
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id: "alice-folder".into(),
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Album,
                    state: JobState::Queued,
                    search_id: None,
                    picked: slots,
                },
                None,
            )
            .await
            .unwrap();
        enqueued_in_b1(ledger, "j1");
        BatchRef::new(
            TransferOwner {
                user_id: UserId::new("alice"),
                batch_id: "b1".into(),
            },
            "Come Over".into(),
            library.to_path_buf(),
            ImportMode::Album,
        )
    }

    fn slot_of_j1(slot: u32) -> JobSlot {
        JobSlot {
            job: "j1".into(),
            slot,
        }
    }

    #[tokio::test]
    async fn an_album_file_found_on_disk_waits_for_the_transfer_its_batch_still_runs() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let library = tmp.path().join("music");
        let b1 = album_in_b1(
            &ledger,
            &db,
            &library,
            vec![
                (0, item("peer_a", "One", "q\\01.flac")),
                (1, item("peer_a", "Two", "q\\02.flac")),
            ],
        )
        .await;
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![
                transfer("peer_a", "q\\02.flac", "t-02", DownloadState::InProgress)
                    .with_batch("b1".into(), "Come Over".into())
                    .with_job(slot_of_j1(1)),
            ],
        });
        ledger.settled().await;
        // slskd finished 01 and cleared its record while Soulful was down; 02 still downloads.
        let folder = tmp.path().join(&b1.destination);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"flac").unwrap();
        let stub = StubBackend::new(vec![]);
        stub.set_listing(vec![in_request(
            transfer("peer_a", "q\\02.flac", "t-02", DownloadState::InProgress),
            B1_REQUEST,
        )]);
        let started = Started::on(stub.clone());
        let importer = Arc::new(StubImporter::new());
        let deps = RecordingDeps::new(importer.clone());
        let queue = ImportQueue::new(deps.clone());

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(
            *started.watched.lock().unwrap(),
            vec![(
                "b1".into(),
                "Come Over".into(),
                vec![("t-02".into(), slot_of_j1(1))]
            )]
        );
        assert_eq!(
            *started.found.lock().unwrap(),
            vec![(folder.join("01.flac"), Some(slot_of_j1(0)))],
            "01 is a finished slot of the batch its monitor imports"
        );
        assert!(started.enqueued.lock().unwrap().is_empty());
        assert!(
            !deps
                .ledger_events()
                .iter()
                .any(|e| matches!(e, JobEvent::ImportQueued(_))),
            "an import was queued while 02 still downloads"
        );
        assert_eq!(importer.calls(), 0);
    }

    #[tokio::test]
    async fn an_album_whose_every_file_is_on_disk_is_imported_once_with_all_of_them() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let library = tmp.path().join("music");
        let b1 = album_in_b1(
            &ledger,
            &db,
            &library,
            vec![
                (0, item("peer_a", "One", "q\\01.flac")),
                (1, item("peer_a", "Two", "q\\02.flac")),
            ],
        )
        .await;
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![
                transfer("peer_a", "q\\01.flac", "t-01", DownloadState::Completed)
                    .with_batch("b1".into(), "Come Over".into())
                    .with_job(slot_of_j1(0)),
            ],
        });
        ledger.settled().await;
        // Both finished while Soulful was down, and slskd cleared their records.
        let folder = tmp.path().join(&b1.destination);
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["01.flac", "02.flac"] {
            std::fs::write(folder.join(name), b"flac").unwrap();
        }
        let started = Started::on(StubBackend::new(vec![]));
        let importer = Arc::new(StubImporter::new());
        let deps = RecordingDeps::new(importer.clone());
        let queue = ImportQueue::new(deps.clone());

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while importer.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the album was imported");
        assert_eq!(importer.seen(), vec![(vec![folder], library)]);
        let queued: Vec<Vec<JobSlot>> = deps
            .ledger_events()
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::ImportQueued(import) => {
                    Some(import.rows.into_iter().map(|(slot, _)| slot).collect())
                }
                _ => None,
            })
            .collect();
        assert_eq!(queued, vec![vec![slot_of_j1(0), slot_of_j1(1)]]);
        assert!(started.nothing());
    }

    #[tokio::test]
    async fn a_downloading_slot_resumes_in_the_batch_its_row_names() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        stub.set_listing(vec![transfer(
            "peer_a",
            "d/01.flac",
            "t-d01",
            DownloadState::InProgress,
        )]);
        let started = Started::on(stub.clone());
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::Queued,
            None,
            vec![(0, item("peer_a", "One", "d/01.flac"))],
        )
        .await;
        // A later enqueue of the job's other files went to batch b2.
        ledger.record(JobEvent::Enqueued {
            job: "j1".into(),
            monitor_batch_id: "b2".into(),
            batch_label: "Other".into(),
            request: "r-b2".into(),
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![
                transfer("peer_a", "d/01.flac", "t-d01", DownloadState::InProgress)
                    .with_batch("b1".into(), "Come Over".into())
                    .with_job(JobSlot {
                        job: "j1".into(),
                        slot: 0,
                    }),
            ],
        });
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;

        let watched: Vec<(String, String)> = started
            .watched
            .lock()
            .unwrap()
            .iter()
            .map(|(batch, label, _)| (batch.clone(), label.clone()))
            .collect();
        assert_eq!(watched, vec![("b1".to_string(), "Come Over".to_string())]);
    }

    #[tokio::test]
    async fn an_unreadable_job_or_slot_ends_failed_and_the_rest_resume() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let one = || vec![(0, item("peer_a", "One", "01.flac"))];
        job_of(&ledger, &db, "bad-job", JobState::Queued, None, one()).await;
        job_of(
            &ledger,
            &db,
            "bad-slot",
            JobState::Queued,
            None,
            vec![
                (0, item("peer_a", "One", "01.flac")),
                (1, item("peer_a", "Two", "02.flac")),
            ],
        )
        .await;
        ledger.settled().await;
        for (sql, id) in [
            ("UPDATE jobs SET wanted = '{' WHERE id = ?", "bad-job"),
            (
                "UPDATE job_tracks SET last_row = '{' WHERE job_id = ? AND slot = 0",
                "bad-slot",
            ),
        ] {
            sqlx::query(sql).bind(id).execute(&db).await.unwrap();
        }

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(
            store::job_state(&db, "bad-job").await,
            Some(JobState::Failed)
        );
        let states: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM job_tracks WHERE job_id = 'bad-slot' ORDER BY slot",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(states, vec!["failed", "queued"]);
        let enqueued: Vec<(String, Vec<(u32, String)>)> = started
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|(job, items, ..)| (job.clone(), items.clone()))
            .collect();
        assert_eq!(
            enqueued,
            vec![("bad-slot".to_string(), vec![(1, "02.flac".to_string())])]
        );
    }

    #[tokio::test]
    async fn a_job_waiting_for_a_source_is_left_waiting_and_can_still_be_picked() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        ledger.record_search(search("s1")).await.unwrap();
        job_of(
            &ledger,
            &db,
            "j1",
            JobState::NeedsSourceChoice,
            Some("s1"),
            vec![],
        )
        .await;
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert!(started.nothing());
        assert_eq!(
            store::job_state(&db, "j1").await,
            Some(JobState::NeedsSourceChoice)
        );
        let claimed = ledger
            .claim_source_choice(&UserId::new("alice"), "s1", "alice-folder", vec![])
            .await
            .unwrap();
        assert_eq!(claimed.map(|c| c.id), Some("j1".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failover_cut_short_resumes_the_transfer_it_sent() {
        let base = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        // The failover enqueued peer_b's copy and Soulful stopped before it
        // recorded the row; peer_a's transfer was not cancelled yet. peer_b
        // also holds an earlier download of the file.
        stub.set_listing(vec![
            transfer("peer_a", "a/01.flac", "t-a01", DownloadState::InProgress),
            in_request(
                transfer("peer_b", "b/01.flac", "t-b00", DownloadState::Completed),
                "r-old",
            ),
            in_request(
                transfer("peer_b", "b/01.flac", "t-b01", DownloadState::Queued),
                "r-failover",
            ),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        let resumed = resume_downloads(
            &backend,
            base.path(),
            &batch("X"),
            vec![(
                slot.clone(),
                SlotRecord {
                    sent_in: Some("r-failover".into()),
                    ..record(
                        0,
                        JobState::Downloading,
                        transfer("peer_a", "a/01.flac", "t-a01", DownloadState::InProgress),
                        vec![
                            Some(item("peer_a", "One", "a/01.flac")),
                            Some(item("peer_b", "One", "b/01.flac")),
                        ],
                    )
                },
            )],
            vec![],
        )
        .await;
        assert_eq!(
            resumed
                .watch
                .iter()
                .map(|w| (w.transfer_id.as_str(), w.job.clone()))
                .collect::<Vec<_>>(),
            vec![("t-b01", slot.clone())]
        );
        assert_eq!(
            resumed
                .rebound
                .iter()
                .map(|r| (r.id.as_str(), r.job.clone()))
                .collect::<Vec<_>>(),
            vec![("t-b01", Some(slot))]
        );
        assert_eq!(
            resumed.abandon,
            vec![("peer_a".to_string(), "t-a01".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_slskd_dropped_is_imported_when_its_file_is_there() {
        let base = tempfile::tempdir().unwrap();
        let b = batch("Come Over");
        std::fs::create_dir_all(base.path().join(&b.destination)).unwrap();
        std::fs::write(base.path().join(&b.destination).join("01.flac"), b"flac").unwrap();
        // slskd finished it while Soulful was down, and its record was cleared.
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        let resumed = resume_downloads(
            &backend,
            base.path(),
            &b,
            vec![(
                slot.clone(),
                record(
                    0,
                    JobState::Downloading,
                    transfer("peer_a", "a\\01.flac", "t-01", DownloadState::InProgress),
                    vec![Some(item("peer_a", "One", "a\\01.flac"))],
                ),
            )],
            vec![],
        )
        .await;
        assert!(resumed.watch.is_empty() && resumed.failed.is_empty());
        assert_eq!(
            resumed
                .finished
                .iter()
                .map(|f| (f.path.clone(), f.row.job.clone()))
                .collect::<Vec<_>>(),
            vec![(base.path().join(&b.destination).join("01.flac"), Some(slot))]
        );
    }
}
