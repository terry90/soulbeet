//! Startup: every job the ledger holds unfinished picks up where Soulful stopped.
//! Each slot resumes by its own state, since a job is only at its earliest
//! slot: downloads slskd still has are watched again, interrupted imports run
//! again, kept imports keep their Retry, queued files are enqueued again
//! into their job's batch (a new one when it had none) and auto-downloads
//! still searching search again.

use crate::auth::{UserCtx, UserId};
use crate::config::CONFIG;
use crate::core::acquire::{
    alternates_from, enqueue_and_watch, fail_job, manual_batch_label, run_search, spawn_monitor,
    Acquisition, AutoRun, NewBatch, RequestBatch,
};
use crate::core::jobs::runner::{spawn_drive, ACQUIRE};
use crate::core::sources::{server_restarted_search, view, RECENT_FAILURES_SECS};
use crate::core::{backend_unavailable, database_failure, sources};
use crate::globals::{get_or_create_user_channel, send_user_event};
use crate::import_queue::{ImportJob, ImportQueue, IMPORT_QUEUE};
use crate::ledger::store::{self, SlotRecord, StoredSearch, Unfinished, UnfinishedJob};
use crate::ledger::{
    BatchOrigin, DuplicateRule, JobEvent, JobState, Ledger, NewSearch, StoredImport, LEDGER,
};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::{already_running, held_failure, transfer_key};
use crate::server_fns::download::failover::{alternates_for, SourcePool, MAX_SOURCES_PER_TRACK};
use crate::server_fns::download::monitor::{
    cancelled_on_request, DownloadMonitor, WatchedTransfer,
};
use crate::server_fns::download::process::{
    find_on_disk, import_job, resolve_on_disk, unresolved_row, Downloaded, ResolvedSlot,
};
use crate::server_fns::download::utils::{FileId, Unresolved};
use crate::services::download_backend;
use crate::transfer_registry::{BatchRef, InboxReservation, TransferOwner, TransferRegistry};
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
        "Soulbeet restarted and slskd no longer has this download",
    )
}

/// An import Soulful ran when it stopped has no files left to import (D10).
pub(crate) fn import_lost() -> Failure {
    Failure::new(
        FailureCode::ServerRestarted,
        Stage::Import,
        RetryAdvice::NotRetryable,
        "Soulbeet restarted during this import and the downloaded files are gone",
    )
}

/// The candidate `record`'s current pick started at: the attempts before it are an earlier
/// pick's.
fn current_pick(record: &SlotRecord) -> Option<&DownloadableItem> {
    let first = record.pick_n.saturating_sub(1) as usize;
    record.attempts.get(first)?.as_ref()
}

/// The sources each slot of `job` may use: its own for an MCP job (D7), the web UI's otherwise.
fn max_sources(job: &UnfinishedJob) -> usize {
    (job.options.as_ref()).map_or(MAX_SOURCES_PER_TRACK, |o| o.max_sources as usize)
}

/// The row `slot` shows, its `job` set: its last one, or before it had one a
/// queued row of its current pick.
fn shown_row(slot: &JobSlot, record: &SlotRecord) -> Option<DownloadProgress> {
    let row = record.row.clone().or_else(|| {
        let item = current_pick(record)?;
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
    /// The queued slots whose file is in the batch folder, among `finished`: not to send again.
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
/// `queued` are queued slots of the batch's jobs whose file the backend does
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
    /// Watches `transfers` for `batch`, failing slots over from `pool`, and
    /// the files requests deliver to its inbox; an album batch imports
    /// `found`, its files already finished, with them once every slot has
    /// settled.
    async fn watch(
        &self,
        ctx: &UserCtx,
        batch: BatchRef,
        transfers: Vec<WatchedTransfer>,
        pool: SourcePool,
        found: Vec<ResolvedSlot>,
    );
    /// Enqueues `acquisition`'s files into `batch` and watches them.
    async fn enqueue(
        &self,
        ctx: &UserCtx,
        backend: Arc<dyn DownloadBackend>,
        batch: RequestBatch,
        acquisition: Acquisition,
    );
    /// Runs the auto-download pipeline of `run`'s job.
    async fn search(&self, ctx: UserCtx, folder: Folder, query: DownloadQuery, run: AutoRun);
    /// Takes `ctx`'s MCP job `job` on with the job runner.
    async fn drive(&self, ctx: &UserCtx, job: String);
    /// Marks job `job` cancelled in the transfer registry (D9), so every monitor stops its slots.
    fn cancel_job(&self, job: &str);
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
        batch: RequestBatch,
        acquisition: Acquisition,
    ) {
        let job = acquisition.job.clone();
        // Its failures are recorded on the job's slots.
        if let Err(failure) = enqueue_and_watch(&LEDGER, ctx, backend, batch, acquisition).await {
            info!("Resumed job {job} could not enqueue: {}", failure.message);
        }
    }

    async fn search(&self, ctx: UserCtx, folder: Folder, query: DownloadQuery, run: AutoRun) {
        match sources::usable_backends().await {
            Ok(backends) => {
                tokio::spawn(run_search(
                    LEDGER.clone(),
                    ctx,
                    folder,
                    query,
                    backends,
                    run,
                ));
            }
            Err(failure) => fail_job(&LEDGER, &ctx, &run, failure).await,
        }
    }

    async fn drive(&self, ctx: &UserCtx, job: String) {
        spawn_drive(ACQUIRE.clone(), ctx.clone(), job);
    }

    fn cancel_job(&self, job: &str) {
        TransferRegistry::global().cancel_job(job);
    }
}

/// Shows `rows` (each with its slot) to `user` and records them Failed.
async fn fail_rows(ledger: &Ledger, user: &UserId, rows: Vec<DownloadProgress>) {
    end_rows(ledger, user, JobState::Failed, rows).await;
}

/// Shows `rows` (each with its slot) to `user` and records them in `state`.
async fn end_rows(ledger: &Ledger, user: &UserId, state: JobState, rows: Vec<DownloadProgress>) {
    if rows.is_empty() {
        return;
    }
    send_user_event(user, DownloadEvent::Progress(rows.clone())).await;
    ledger.record(JobEvent::Slots { state, rows });
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
    /// The queued slots of jobs whose files went into this batch that the
    /// backend does not hold, by job: each is looked for in the batch folder,
    /// and the others are sent again into this batch.
    queued: Vec<Queued>,
    pools: HashMap<JobSlot, (Vec<DownloadableItem>, usize, usize)>,
}

impl Downloads {
    /// Adds `slot`, its pool entry being the alternates of its current pick
    /// among `candidates` it has not tried, the sources that pick used, and
    /// the `max_sources` it may use.
    fn add(
        &mut self,
        slot: JobSlot,
        record: SlotRecord,
        candidates: &[DownloadableGroup],
        max_sources: usize,
    ) {
        let tried: Vec<DownloadableItem> = record.attempts.iter().flatten().cloned().collect();
        let alternates = match current_pick(&record) {
            Some(first) => alternates_for(first, &alternates_from(candidates, &tried)),
            None => Vec::new(),
        };
        let earlier = record.pick_n.saturating_sub(1) as usize;
        let used = record.attempts.len().saturating_sub(earlier);
        self.pools
            .insert(slot.clone(), (alternates, used, max_sources));
        self.slots.push((slot, record));
    }
}

/// The monitor batch `batch_id` of `job`'s user among `downloads`, added
/// with `label` when it is not there yet: an MCP job's skips what the library
/// has.
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
            let batch = BatchRef::new(
                TransferOwner {
                    user_id: user,
                    batch_id: batch_id.to_string(),
                },
                label.to_string(),
                PathBuf::from(&job.folder.path),
                job.import_mode,
            );
            downloads.push(Downloads {
                ctx: job.ctx.clone(),
                queued: Vec::new(),
                batch: batch
                    .with_duplicates(DuplicateRule::for_job(job.origin, job.options.as_ref())),
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

/// The jobs some of whose slots' stored state cannot be read.
fn partly_unreadable(slots: &[(JobSlot, sqlx::Error)]) -> HashSet<String> {
    slots.iter().map(|(slot, _)| slot.job.clone()).collect()
}

/// Why `job` ends without resuming when it is queued with no slot to resume: the stored state
/// of its files cannot be read (it is one of `partly_unreadable`), or it stopped before it had
/// files. None for any other job.
fn queued_without_files(
    job: &UnfinishedJob,
    partly_unreadable: &HashSet<String>,
) -> Option<&'static str> {
    if job.state != JobState::Queued || !job.slots.is_empty() {
        return None;
    }
    Some(match partly_unreadable.contains(&job.id) {
        true => "the stored state of its files cannot be read",
        false => "it stopped before it had files to download",
    })
}

/// Ends `job` failed at `stage`: Soulful stopped where nothing can pick it up again, `why`.
fn end_interrupted(ledger: &Ledger, job: &str, stage: Stage, why: &str) {
    warn!("Ending job {job}, which cannot resume: {why}");
    ledger.record(JobEvent::Ended {
        job: job.to_string(),
        state: JobState::Failed,
        failure: Some(Failure::new(
            FailureCode::ServerRestarted,
            stage,
            RetryAdvice::SameSourceLater,
            format!("Soulbeet restarted and could not resume this job: {why}"),
        )),
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
                spec: None,
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
/// and the prune. An MCP job searching, or queued with nothing sent, goes to
/// the job runner. A job or slot whose stored state cannot be read ends
/// failed, and so does a job stopped before it had a search to resume or a
/// slot to download. First the MCP searches the restart cut off end, failed.
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
        cut_searches,
    } = unfinished;
    // D9: the marks a cancel set lived in memory; set again before any job resumes, so every
    // monitor started below stops the slots of a job cancelled before the restart.
    for job in jobs.iter().filter(|job| job.cancel_requested) {
        resume.cancel_job(&job.id);
    }
    for id in cut_searches {
        // The writer logs a write the database refused.
        let _ = (ledger.search_ended(&id, Err((server_restarted_search(), None)))).await;
    }
    for (job, e) in unreadable_jobs {
        end_interrupted(
            ledger,
            &job,
            Stage::Input,
            &format!("its stored state cannot be read: {e}"),
        );
    }
    let partly_unreadable = partly_unreadable(&unreadable_slots);
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
    // Read once: an MCP job's failover passes flagged sources over (D5). Unread, no source can be
    // told flagged, so an MCP job resumes with its current transfers and no other source.
    let failures = match store::recent_failures(db, now - RECENT_FAILURES_SECS).await {
        Ok(failures) => Some(failures),
        Err(e) => {
            warn!("Could not read the sources' recent failures to resume jobs: {e}");
            None
        }
    };
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
        let mcp = job.origin == BatchOrigin::Mcp;
        if job.state == JobState::Searching {
            // Every job searching is made with its search.
            match job.search_id.clone() {
                Some(_) if mcp => resume.drive(&job.ctx, job.id).await,
                Some(search_id) => resume_search(ledger, resume, job, search_id, search, now).await,
                None => end_interrupted(
                    ledger,
                    &job.id,
                    Stage::SourceSearch,
                    "it is searching without a search",
                ),
            }
            continue;
        }
        if let Some(why) = queued_without_files(&job, &partly_unreadable) {
            end_interrupted(ledger, &job.id, Stage::Enqueue, why);
            continue;
        }
        // Nothing was sent: the batch is stored before a send and dropped at every pick.
        if mcp && job.state == JobState::Queued && job.monitor_batch_id.is_none() {
            match job.cancel_requested {
                true => ledger.record(JobEvent::Ended {
                    job: job.id,
                    state: JobState::Cancelled,
                    failure: None,
                }),
                false => resume.drive(&job.ctx, job.id).await,
            }
            continue;
        }

        // An MCP job's alternates are its search's candidates as its own view keeps them.
        let candidates = match (search, &job.options, &failures) {
            (Some(search), Some(options), Some(failures)) => {
                let groups = search.candidates.unwrap_or_default();
                let filters = &options.filters;
                view(
                    &search.id,
                    &groups,
                    filters,
                    failures,
                    &job.excluded_sources,
                )
                .automatic_groups()
            }
            (_, Some(_), None) => Vec::new(),
            (search, None, _) => search.and_then(|s| s.candidates).unwrap_or_default(),
            (None, Some(_), Some(_)) => Vec::new(),
        };
        let cap = max_sources(&job);
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
                        cap,
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
            .restore(ImportJob::from_stored(import, user.clone()), &rows)
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
    let mut fresh = Vec::new();
    for job in queued {
        let Some(job) = adopt_sent(ledger, &backend, &mut downloads, job).await else {
            continue;
        };
        // Cancelled before it sent anything: nothing is sent now.
        if job.job.cancel_requested {
            ledger.record(JobEvent::Ended {
                job: job.job.id,
                state: JobState::Cancelled,
                failure: None,
            });
        } else {
            fresh.push(job);
        }
    }
    // One batch whose files are slow to find on disk holds up no other.
    futures::future::join_all(
        downloads
            .into_iter()
            .map(|batch| resume_batch(ledger, queue, resume, &backend, download_base, batch)),
    )
    .await;
    futures::future::join_all(
        fresh
            .into_iter()
            .map(|job| enqueue_anew(resume, &backend, job)),
    )
    .await;
}

/// Moves the queued slots of `queued`'s job whose file the backend already
/// holds into the job's monitor batch, recording each as downloading, and
/// hands that batch the others: it imports the ones found in its folder with
/// the rest of it and sends the others again, unless the job's user cancelled
/// it, which ends them cancelled. The batch is stored before the backend
/// hears of a file, so a job without one sent nothing: it is given back, to
/// enqueue into a new batch.
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
    let cap = max_sources(&queued.job);
    for (slot, mut record) in std::mem::take(&mut queued.slots) {
        let held = match (current_pick(&record), &queued.job.request) {
            (Some(first), Some(request)) => sent(backend, first, request).await,
            _ => None,
        };
        let Some(transfer) = held else {
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
        batch_of(downloads, &queued.job, &batch_id, &label).add(
            slot,
            record,
            &queued.candidates,
            cap,
        );
    }
    if !rows.is_empty() {
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows,
        });
    }
    if rest.is_empty() {
        return None;
    }
    if queued.job.cancel_requested {
        // Its user cancelled it: what the backend does not hold is not sent again.
        let rows = (rest.iter())
            .filter_map(|(slot, record)| shown_row(slot, record))
            .map(|row| {
                (row.with_state(DownloadState::Cancelled)).with_failure(cancelled_on_request())
            })
            .collect();
        end_rows(ledger, &queued.job.ctx.id(), JobState::Cancelled, rows).await;
        return None;
    }
    queued.slots = rest;
    batch_of(downloads, &queued.job, &batch_id, &label)
        .queued
        .push(queued);
    None
}

/// Resumes one monitor batch: fails the transfers slskd lost, watches the
/// rest, and sends again into it the queued files of its jobs that neither
/// slskd nor its folder has. The files of the batch found on disk (finished
/// transfers slskd no longer lists, queued files in its folder) are finished
/// slots of it: a singleton batch imports each now, and an album batch hands
/// them to its monitor. That monitor is the one place the batch is found
/// finished, for its watched slots and the ones sent again alike: it imports
/// the album once every slot settled, or at once when none is left to watch.
/// A transfer a request made since startup now holds is that request's: its
/// slot is dismissed when the request downloads the file into the same
/// folder for the same user, and fails as that request's claim says
/// otherwise.
async fn resume_batch(
    ledger: &Ledger,
    queue: &ImportQueue,
    resume: &dyn Resume,
    backend: &Arc<dyn DownloadBackend>,
    download_base: &Path,
    downloads: Downloads,
) {
    let Downloads {
        ctx,
        batch,
        slots,
        queued,
        mut pools,
    } = downloads;
    let user = ctx.id();
    let waiting = queued
        .iter()
        .flat_map(|q| q.slots.iter().cloned())
        .collect();
    let Resumed {
        watch,
        finished,
        failed: lost,
        rebound,
        abandon,
        on_disk,
    } = resume_downloads(backend, download_base, &batch, slots, waiting).await;
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
                    Downloaded::File(f.path),
                    vec![f.row],
                    &batch.target,
                    &user,
                    &batch.duplicates,
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
        // Another of the user's batches pursues it, unless this one skips duplicates.
        if failure == already_running() {
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

    let resend: Vec<Queued> = queued
        .into_iter()
        .filter_map(|mut q| {
            q.slots.retain(|(slot, _)| !on_disk.contains(slot));
            (!q.slots.is_empty()).then_some(q)
        })
        .collect();
    if watched.is_empty() && found.is_empty() && resend.is_empty() {
        return;
    }
    // Held open before the monitor runs, so it cannot finish before the files
    // sent again reach it.
    let joining: Vec<(Queued, InboxReservation)> = resend
        .into_iter()
        .map(|q| {
            let reservation = batch
                .inbox
                .reserve()
                .expect("a batch no monitor watches yet takes files");
            (q, reservation)
        })
        .collect();
    // Every watched transfer is one of the slots given, each with its entry.
    let pool = SourcePool::resumed(
        watched
            .iter()
            .map(|w| {
                pools
                    .remove(&w.job)
                    .unwrap_or_else(|| (Vec::new(), 0, MAX_SOURCES_PER_TRACK))
            })
            .collect(),
    );
    resume
        .watch(&ctx, batch.clone(), watched, pool, found)
        .await;
    futures::future::join_all(joining.into_iter().map(|(q, reservation)| {
        let (job, acquisition) = q.acquisition();
        let into = RequestBatch::Running(batch.clone(), reservation);
        async move {
            resume
                .enqueue(&job.ctx, backend.clone(), into, acquisition)
                .await
        }
    }))
    .await;
}

impl Queued {
    /// Its job, and what sending its slots again asks for: each by its current
    /// pick, the search's other candidates standing by as alternates.
    fn acquisition(self) -> (UnfinishedJob, Acquisition) {
        let Queued {
            job,
            slots,
            candidates,
        } = self;
        let items: Vec<(u32, DownloadableItem)> = slots
            .iter()
            .filter_map(|(slot, record)| Some((slot.slot, current_pick(record)?.clone())))
            .collect();
        let picked: Vec<DownloadableItem> = items.iter().map(|(_, item)| item.clone()).collect();
        let acquisition = Acquisition {
            job: job.id.clone(),
            alternates: alternates_from(&candidates, &picked),
            items,
            max_sources: max_sources(&job),
        };
        (job, acquisition)
    }
}

/// Enqueues the queued slots of a job that never had a batch into a new one
/// importing in the job's mode.
async fn enqueue_anew(resume: &dyn Resume, backend: &Arc<dyn DownloadBackend>, queued: Queued) {
    let (job, acquisition) = queued.acquisition();
    let picked: Vec<DownloadableItem> = acquisition
        .items
        .iter()
        .map(|(_, item)| item.clone())
        .collect();
    let fresh = NewBatch {
        batch_id: uuid::Uuid::new_v4().to_string(),
        label: manual_batch_label(&picked),
        target: PathBuf::from(&job.folder.path),
        import_mode: job.import_mode,
        duplicates: DuplicateRule::for_job(job.origin, job.options.as_ref()),
    };
    resume
        .enqueue(&job.ctx, backend.clone(), fresh.into(), acquisition)
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
    use crate::core::jobs::runner::{Acquire, MCP_DOWNLOADS_PER_USER};
    use crate::core::jobs::test_support::{
        auto_options, downloading_track_job, downloading_track_job_with, file_of,
        importing_track_job, in_progress, kept_track_job, mcp_track_job, queued_track_jobs,
    };
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::ledger::{
        store, ImportDone, IndexWait, JobKind, JobOptions, NewJob, NewSearch, StoredImport,
        StoredSpec, Transition, SEARCH_KEPT_SECS,
    };
    use crate::server_fns::download::enqueue::file_taken_by_another_download;
    use crate::server_fns::download::test_support::{batch, group, item, transfer, StubBackend};
    use shared::download::{DownloadQuery, DownloadState, DownloadableItem, JobSlot};
    use shared::failure::{FailureCode, RetryAdvice, Stage};
    use soulbeet::{DriverChoice, DuplicateRun, ImportedItem};
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
            pick_n: 1,
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
                "Soulbeet restarted and slskd no longer has this download",
            ),
            (
                import_lost(),
                Stage::Import,
                RetryAdvice::NotRetryable,
                "Soulbeet restarted during this import and the downloaded files are gone",
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
    /// job, (slot, item id), alternate sources, mode, the running batch joined (None: a new one)
    type EnqueueStart = (
        String,
        Vec<(u32, String)>,
        Vec<String>,
        ImportMode,
        Option<String>,
    );

    /// job, (slot, item id), the running batch joined (None: a new one)
    type Sent = (String, Vec<(u32, String)>, Option<String>);

    /// Records what reconciliation started; its backend is `backend`.
    struct Started {
        backend: Arc<dyn DownloadBackend>,
        watched: Mutex<Vec<WatchStart>>,
        /// The path and slot of each file a watched batch was handed found.
        found: Mutex<Vec<(PathBuf, Option<JobSlot>)>>,
        enqueued: Mutex<Vec<EnqueueStart>>,
        /// job, search id, stored groups
        searched: Mutex<Vec<(String, String, Option<usize>)>>,
        /// Each watched batch's id, duplicates rule and the cap of each slot of its pool.
        rules: Mutex<Vec<(String, DuplicateRule, Vec<usize>)>>,
        /// Each watched batch's slots, whether each has another source standing by.
        standing_by: Mutex<Vec<Vec<bool>>>,
        /// The jobs handed to the job runner.
        driven: Mutex<Vec<String>>,
        /// Each job marked cancelled ("cancel_job {job}") and each transfer watched ("watch
        /// {job}"), in the order they came.
        steps: Mutex<Vec<String>>,
    }

    impl Started {
        fn on(backend: Arc<dyn DownloadBackend>) -> Started {
            Started {
                backend,
                watched: Mutex::default(),
                found: Mutex::default(),
                enqueued: Mutex::default(),
                searched: Mutex::default(),
                rules: Mutex::default(),
                standing_by: Mutex::default(),
                driven: Mutex::default(),
                steps: Mutex::default(),
            }
        }

        /// Each enqueue: its job, (slot, item id) and the running batch it joined.
        fn sent(&self) -> Vec<Sent> {
            self.enqueued
                .lock()
                .unwrap()
                .iter()
                .map(|(job, items, .., batch)| (job.clone(), items.clone(), batch.clone()))
                .collect()
        }

        fn nothing(&self) -> bool {
            self.watched.lock().unwrap().is_empty()
                && self.enqueued.lock().unwrap().is_empty()
                && self.searched.lock().unwrap().is_empty()
                && self.driven.lock().unwrap().is_empty()
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
            pool: SourcePool,
            found: Vec<ResolvedSlot>,
        ) {
            self.found
                .lock()
                .unwrap()
                .extend(found.into_iter().map(|f| (f.path, f.row.job)));
            let caps = (0..pool.slot_count())
                .map(|s| pool.max_sources(s))
                .collect();
            let standing_by = (0..pool.slot_count()).map(|s| pool.has_next(s)).collect();
            self.standing_by.lock().unwrap().push(standing_by);
            (self.rules.lock().unwrap()).push((
                batch.owner.batch_id.clone(),
                batch.duplicates,
                caps,
            ));
            (self.steps.lock().unwrap())
                .extend(transfers.iter().map(|t| format!("watch {}", t.job.job)));
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
            batch: RequestBatch,
            a: Acquisition,
        ) {
            let (mode, running) = match batch {
                RequestBatch::New(fresh) => (fresh.import_mode, None),
                RequestBatch::Running(batch, _) => (batch.import_mode, Some(batch.owner.batch_id)),
            };
            self.enqueued.lock().unwrap().push((
                a.job,
                a.items.into_iter().map(|(s, i)| (s, i.id)).collect(),
                a.alternates.into_iter().map(|g| g.source).collect(),
                mode,
                running,
            ));
        }
        async fn search(&self, _: UserCtx, _: Folder, _: DownloadQuery, run: AutoRun) {
            self.searched.lock().unwrap().push((
                run.job,
                run.search_id,
                run.found.map(|g| g.len()),
            ));
        }
        async fn drive(&self, _: &UserCtx, job: String) {
            self.driven.lock().unwrap().push(job);
        }
        fn cancel_job(&self, job: &str) {
            self.steps.lock().unwrap().push(format!("cancel_job {job}"));
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
                    kind: JobKind::Track,
                    options: None,
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
            spec: None,
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
                sources: vec![source.clone()],
                files: vec![source],
                target: tmp.path().join("lib"),
                album: false,
                cleanup_root: None,
                choices: None,
                choice: DriverChoice::Auto,
                duplicates: DuplicateRule::Guarded,
                last_run: None,
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
                ImportMode::Singleton,
                None
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
                sources: vec![source.clone()],
                files: vec![source],
                target: tmp.path().join("lib"),
                album: false,
                cleanup_root: None,
                choices: None,
                choice: DriverChoice::Auto,
                duplicates: DuplicateRule::Guarded,
                last_run: None,
                rows: vec![(slot, row.clone().with_state(DownloadState::ImportQueued(1)))],
            };
            // As the import queue's finish records a kept import.
            ledger.record(JobEvent::ImportQueued(Box::new(import.clone())));
            ledger.record(JobEvent::ImportDone(Box::new(ImportDone {
                import,
                rows: vec![row.with_state(DownloadState::ImportSkipped)],
                next: JobState::NeedsDecision,
                index: None,
                results: vec![],
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
                    beets_id: None,
                    release_mbid: None,
                    match_per_mille: None,
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
                soulbeet::error::SoulseekError::NotConfigured(soulbeet::error::MissingSetting::Url)
                    .into(),
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
            _: RequestBatch,
            _: Acquisition,
        ) {
            unreachable!("nothing is enqueued without a backend")
        }
        async fn search(&self, _: UserCtx, _: Folder, _: DownloadQuery, _: AutoRun) {
            unreachable!("no job here searches")
        }
        async fn drive(&self, _: &UserCtx, _: String) {
            unreachable!("no job here is an MCP job")
        }
        fn cancel_job(&self, _: &str) {
            unreachable!("no job here was cancelled")
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
        let enqueued = started.sent();
        // 02 goes into the batch that holds 01; the job that sent nothing gets a new one.
        assert_eq!(
            enqueued,
            vec![
                (
                    "sent".to_string(),
                    vec![(1, "q/02.flac".to_string())],
                    Some("b1".to_string())
                ),
                (
                    "unsent".to_string(),
                    vec![(0, "q/01.flac".to_string()), (1, "q/02.flac".to_string())],
                    None
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

    #[tokio::test]
    async fn a_queued_job_whose_files_cannot_be_read_ends_saying_so() {
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

        // j2 stopped before it had files at all.
        job_of(&ledger, &db, "j2", JobState::Queued, None, vec![]).await;
        ledger.settled().await;

        // Each job is read with no slot left to resume, and ends for its own reason.
        let unfinished = read_unfinished(&db).await.unwrap();
        let partly = partly_unreadable(&unfinished.unreadable_slots);
        let why = |id: &str| {
            let job = unfinished.jobs.iter().find(|job| job.id == id).unwrap();
            queued_without_files(job, &partly)
        };
        assert_eq!(
            (why("j1"), why("j2")),
            (
                Some("the stored state of its files cannot be read"),
                Some("it stopped before it had files to download")
            )
        );
        resume_unfinished(&db, &ledger, &queue(), &started, tmp.path(), unfinished).await;
        ledger.settled().await;

        for job in ["j1", "j2"] {
            assert_eq!(store::job_state(&db, job).await, Some(JobState::Failed));
        }
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
        let enqueued = started.sent();
        assert_eq!(
            enqueued,
            vec![(
                "j1".to_string(),
                vec![(1, "q\\02.flac".to_string())],
                Some("b1".to_string())
            )]
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
                    kind: JobKind::Album,
                    options: None,
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
    async fn an_album_file_found_on_disk_waits_for_the_files_its_job_sends_again() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let library = tmp.path().join("music");
        // One request sent 01 to peer_a and 02 to peer_b, as two slskd batches; Soulful stopped
        // after the first was sent.
        let b1 = album_in_b1(
            &ledger,
            &db,
            &library,
            vec![
                (0, item("peer_a", "One", "q\\01.flac")),
                (1, item("peer_b", "Two", "q\\02.flac")),
            ],
        )
        .await;
        ledger.settled().await;
        // 01 finished and slskd cleared its record while Soulful was down; slskd never had 02.
        let folder = tmp.path().join(&b1.destination);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"flac").unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let importer = Arc::new(StubImporter::new());
        let deps = RecordingDeps::new(importer.clone());
        let queue = ImportQueue::new(deps.clone());

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(
            *started.watched.lock().unwrap(),
            vec![("b1".into(), "Come Over".into(), vec![])],
            "b1's monitor holds the job's album"
        );
        assert_eq!(
            *started.found.lock().unwrap(),
            vec![(folder.join("01.flac"), Some(slot_of_j1(0)))]
        );
        assert!(
            !deps
                .ledger_events()
                .iter()
                .any(|e| matches!(e, JobEvent::ImportQueued(_))),
            "01 was imported before 02 was sent again"
        );
        assert_eq!(importer.calls(), 0);
        assert_eq!(
            *started.enqueued.lock().unwrap(),
            vec![(
                "j1".to_string(),
                vec![(1, "q\\02.flac".to_string())],
                vec![],
                ImportMode::Album,
                Some("b1".to_string())
            )],
            "02 is sent again into b1, whose monitor holds 01"
        );
    }

    #[tokio::test]
    async fn an_album_whose_every_file_is_on_disk_hands_them_all_to_its_monitor() {
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
        let deps = RecordingDeps::new(Arc::new(StubImporter::new()));
        let queue = ImportQueue::new(deps.clone());

        reconcile_in(&db, &ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        // It has nothing to watch, so it imports them at once, as one album.
        assert_eq!(
            *started.watched.lock().unwrap(),
            vec![("b1".into(), "Come Over".into(), vec![])]
        );
        assert_eq!(
            *started.found.lock().unwrap(),
            vec![
                (folder.join("01.flac"), Some(slot_of_j1(0))),
                (folder.join("02.flac"), Some(slot_of_j1(1)))
            ]
        );
        assert!(started.enqueued.lock().unwrap().is_empty());
        assert!(!deps
            .ledger_events()
            .iter()
            .any(|e| matches!(e, JobEvent::ImportQueued(_))));
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
        // Its own record cannot be read: a failure of reading what was asked, at input.
        let failure: String = sqlx::query_scalar("SELECT failure FROM jobs WHERE id = 'bad-job'")
            .fetch_one(&db)
            .await
            .unwrap();
        let failure: Failure = serde_json::from_str(&failure).unwrap();
        assert_eq!(
            (failure.code, failure.stage),
            (FailureCode::ServerRestarted, Stage::Input)
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

    #[tokio::test]
    async fn an_mcp_job_resumes_in_every_state() {
        let (acq, _) = Acquire::for_alice().await;
        let (ledger, db) = (&acq.ledger, &acq.db);
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        let started = Started::on(stub.clone());
        let importer = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(importer.clone()));
        let file = |name: &str| {
            let source = tmp.path().join(name).join("03.flac");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, b"flac").unwrap();
            source
        };

        let searching =
            mcp_track_job(&acq, "Marrow Choir", "Ding Dong Merrily", auto_options()).await;
        let [queued, cancel_requested] =
            <[String; 2]>::try_from(queued_track_jobs(&acq, 2).await).expect("two jobs");
        // Cancelled while it held its place, before it sent anything.
        assert!(ledger
            .claim_download(&cancel_requested, MCP_DOWNLOADS_PER_USER)
            .await
            .unwrap());
        ledger
            .transition(&UserId::new("alice"), &cancel_requested, Transition::Cancel)
            .await
            .unwrap();
        let two_sources = JobOptions {
            max_sources: 2,
            ..auto_options()
        };
        let downloading = downloading_track_job_with(&acq, "Silent Night", two_sources).await;
        // slskd still has its transfer.
        let listed = in_progress(&downloading, "Silent Night");
        stub.set_listing(vec![transfer(
            "peer_a",
            &listed.item,
            &listed.id,
            DownloadState::InProgress,
        )]);
        let importing = importing_track_job(&acq, "Coventry Carol", &file("importing")).await;
        let kept = kept_track_job(&acq, "The Holly and the Ivy", &file("kept")).await;
        let indexing = downloading_track_job(&acq).await;
        let slot = JobSlot {
            job: indexing.clone(),
            slot: 0,
        };
        ledger.record(JobEvent::Slots {
            state: JobState::Indexing,
            rows: vec![in_progress(&indexing, "O Holy Night").with_state(DownloadState::Imported)],
        });
        ledger.record(JobEvent::Indexing {
            slots: vec![slot],
            wait: Box::new(IndexWait {
                import: "/downloads/Come Over [1f2e3d4c]/03.flac".into(),
                library: "/music".into(),
                items: vec![ImportedItem {
                    path: "Marrow Choir/Winter Songs/03 O Holy Night.flac".into(),
                    recording_mbid: None,
                    artist: "Marrow Choir".into(),
                    title: "O Holy Night".into(),
                    album: "Winter Songs".into(),
                    beets_id: None,
                    release_mbid: None,
                    match_per_mille: None,
                }],
                since: 0,
                baseline: None,
                checked: None,
            }),
        });
        ledger.settled().await;

        reconcile_in(db, ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        assert_eq!(*started.driven.lock().unwrap(), vec![searching, queued]);
        assert!(started.enqueued.lock().unwrap().is_empty());
        assert!(started.searched.lock().unwrap().is_empty());
        assert_eq!(
            store::job_state(db, &cancel_requested).await,
            Some(JobState::Cancelled)
        );
        let watched = started.watched.lock().unwrap().clone();
        assert_eq!(
            (watched.iter())
                .map(|(_, _, transfers)| transfers.clone())
                .collect::<Vec<_>>(),
            vec![vec![(
                listed.id.clone(),
                JobSlot {
                    job: downloading.clone(),
                    slot: 0
                }
            )]]
        );
        assert_eq!(
            *started.rules.lock().unwrap(),
            vec![(watched[0].0.clone(), DuplicateRule::Skip, vec![2])]
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while importer.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the interrupted import ran again");
        assert_eq!(importer.duplicate_runs(), vec![DuplicateRun::Skip]);
        let alice = UserId::new("alice");
        let kept_file = file_of("a", "The Holly and the Ivy");
        assert!(queue.import_choices(&alice, &kept_file).is_ok());
        let restored = queue.discard(&alice, &kept_file).await.unwrap().job;
        assert_eq!(
            restored.stored().map(|s| (s.sources, s.duplicates)),
            Some((vec![file("kept")], DuplicateRule::Skip))
        );
        assert_eq!(
            store::job_state(db, &importing).await,
            Some(JobState::Importing)
        );
        assert_eq!(
            store::job_state(db, &kept).await,
            Some(JobState::NeedsDecision)
        );
        assert_eq!(
            store::job_state(db, &indexing).await,
            Some(JobState::Indexing)
        );
    }

    #[tokio::test]
    async fn a_cancel_requested_before_a_restart_is_marked_again() {
        let (acq, _) = Acquire::for_alice().await;
        let (ledger, db) = (&acq.ledger, &acq.db);
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        let started = Started::on(stub.clone());
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let alice = UserId::new("alice");
        let downloading = downloading_track_job_with(&acq, "Gaudete", auto_options()).await;
        let listed = in_progress(&downloading, "Gaudete");
        stub.set_listing(vec![transfer(
            "peer_a",
            &listed.item,
            &listed.id,
            DownloadState::InProgress,
        )]);
        // Queued, its file recorded as sent, which slskd no longer holds.
        let sent = queued_track_jobs(&acq, 1).await.remove(0);
        assert!(ledger
            .claim_download(&sent, MCP_DOWNLOADS_PER_USER)
            .await
            .unwrap());
        ledger.record(JobEvent::Enqueued {
            job: sent.clone(),
            monitor_batch_id: uuid::Uuid::new_v4().to_string(),
            batch_label: "Song Number 1".into(),
            request: uuid::Uuid::new_v4().to_string(),
        });
        for job in [&downloading, &sent] {
            ledger
                .transition(&alice, job, Transition::Cancel)
                .await
                .unwrap();
            // The restart lost the mark the cancel set.
            TransferRegistry::global().reopen_job(job);
        }
        ledger.settled().await;

        reconcile_in(db, ledger, &queue, &started, tmp.path()).await;
        ledger.settled().await;

        let steps = started.steps.lock().unwrap().clone();
        assert_eq!(
            steps,
            vec![
                format!("cancel_job {downloading}"),
                format!("cancel_job {sent}"),
                format!("watch {downloading}"),
            ]
        );
        assert!(
            started.enqueued.lock().unwrap().is_empty(),
            "nothing sent again"
        );
        assert_eq!(store::job_state(db, &sent).await, Some(JobState::Cancelled));
    }

    #[tokio::test]
    async fn an_mcp_job_whose_transfer_another_batch_holds_fails_and_is_not_dismissed() {
        let (acq, _) = Acquire::for_alice().await;
        let (ledger, db) = (&acq.ledger, &acq.db);
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let job = downloading_track_job_with(&acq, "In the Bleak Midwinter", auto_options()).await;
        let listed = in_progress(&job, "In the Bleak Midwinter");
        stub.set_listing(vec![transfer(
            "peer_a",
            &listed.item,
            &listed.id,
            DownloadState::InProgress,
        )]);
        // A request since startup: another of alice's batches downloads the file into her folder.
        let record = store::job_record(db, &job, &UserId::new("alice"))
            .await
            .unwrap()
            .unwrap();
        TransferRegistry::global().claim(
            &[transfer_key(backend.id(), "peer_a", &listed.item)],
            &BatchRef::new(
                TransferOwner {
                    user_id: UserId::new("alice"),
                    batch_id: uuid::Uuid::new_v4().to_string(),
                },
                "Winter Songs".into(),
                PathBuf::from(&record.folder.path),
                ImportMode::Singleton,
            ),
        );
        let started = Started::on(backend);

        reconcile_in(db, ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        assert!(started.watched.lock().unwrap().is_empty());
        let slot: String =
            sqlx::query_scalar("SELECT state FROM job_tracks WHERE job_id = ? AND slot = 0")
                .bind(&job)
                .fetch_one(db)
                .await
                .unwrap();
        assert_eq!(slot, "failed");
        assert_eq!(store::job_state(db, &job).await, Some(JobState::Failed));
        assert_eq!(
            stored_row(db, &job, 0).await.failure,
            Some(file_taken_by_another_download())
        );
    }

    #[tokio::test]
    async fn an_mcp_job_resumes_without_other_sources_when_flagged_ones_cannot_be_told() {
        let (acq, _) = Acquire::for_alice().await;
        let (ledger, db) = (&acq.ledger, &acq.db);
        let tmp = tempfile::tempdir().unwrap();
        let stub = StubBackend::new(vec![]);
        // peer_a downloads; its search also found peer_b.
        let job = downloading_track_job_with(&acq, "Coventry Carol", auto_options()).await;
        let listed = in_progress(&job, "Coventry Carol");
        stub.set_listing(vec![transfer(
            "peer_a",
            &listed.item,
            &listed.id,
            DownloadState::InProgress,
        )]);
        // A finished attempt whose failure is not JSON: no source's recent failures can be read.
        sqlx::query(
            "INSERT INTO job_attempts (job_id, slot, n, source, ended_at, failure) \
             VALUES (?, 0, 99, 'peer_x', strftime('%s', 'now'), 'not json')",
        )
        .bind(&job)
        .execute(db)
        .await
        .unwrap();
        let started = Started::on(stub);

        reconcile_in(db, ledger, &queue(), &started, tmp.path()).await;

        assert_eq!(started.watched.lock().unwrap().len(), 1);
        assert_eq!(*started.standing_by.lock().unwrap(), vec![vec![false]]);
    }

    #[tokio::test]
    async fn a_find_sources_search_cut_off_by_a_restart_ends_and_a_jobs_search_is_left_to_it() {
        let (acq, _) = Acquire::for_alice().await;
        let (ledger, db) = (&acq.ledger, &acq.db);
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        let alice = UserId::new("alice");
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto_options()).await;
        let job_search = store::job_record(db, &job, &alice)
            .await
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        let running = |id: &str, spec: Option<StoredSpec>| NewSearch {
            id: id.into(),
            user: alice.clone(),
            backend: Some("slskd".into()),
            query: DownloadQuery::new(vec![]),
            spec,
        };
        for search in [
            running(&job_search, Some(StoredSpec::default())),
            running("find", Some(StoredSpec::default())),
            running("ui", None),
        ] {
            ledger.record_search(search).await.unwrap();
        }

        reconcile_in(db, ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        let failure_of = |id: &'static str| async move {
            let stored = store::search_of(db, id, &UserId::new("alice")).await;
            stored.unwrap().unwrap().failure
        };
        assert_eq!(failure_of("find").await, Some(server_restarted_search()));
        assert_eq!(
            failure_of("ui").await,
            None,
            "a web UI search is polled again"
        );
        let job_search = store::search_of(db, &job_search, &alice).await.unwrap();
        assert_eq!(job_search.unwrap().failure, None);
        assert_eq!(*started.driven.lock().unwrap(), vec![job]);
    }

    #[tokio::test]
    async fn a_job_ended_at_startup_keeps_why_as_its_failure() {
        let (ledger, db) = ledger().await;
        let tmp = tempfile::tempdir().unwrap();
        let started = Started::on(StubBackend::new(vec![]));
        job_of(&ledger, &db, "no-search", JobState::Searching, None, vec![]).await;
        job_of(&ledger, &db, "no-files", JobState::Queued, None, vec![]).await;
        ledger.settled().await;

        reconcile_in(&db, &ledger, &queue(), &started, tmp.path()).await;
        ledger.settled().await;

        for (id, stage, why) in [
            (
                "no-search",
                Stage::SourceSearch,
                "it is searching without a search",
            ),
            (
                "no-files",
                Stage::Enqueue,
                "it stopped before it had files to download",
            ),
        ] {
            let stored: String = sqlx::query_scalar("SELECT failure FROM jobs WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
            let f: Failure = serde_json::from_str(&stored).unwrap();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message),
                (
                    FailureCode::ServerRestarted,
                    stage,
                    RetryAdvice::SameSourceLater,
                    format!("Soulbeet restarted and could not resume this job: {why}")
                )
            );
        }
    }

    #[test]
    fn a_slot_picked_again_resumes_from_its_current_pick() {
        let b = batch("Come Over");
        let mut downloads = Downloads {
            ctx: crate::core::test_ctx("alice"),
            batch: b,
            slots: Vec::new(),
            queued: Vec::new(),
            pools: HashMap::new(),
        };
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        // An earlier search's pick, then this search's peer_a and its failover to peer_b.
        let record = SlotRecord {
            pick_n: 2,
            ..record(
                0,
                JobState::Downloading,
                transfer("peer_b", "b/01.flac", "t-b01", DownloadState::InProgress),
                vec![
                    Some(item("peer_x", "One", "x/01.flac")),
                    Some(item("peer_a", "One", "a/01.flac")),
                    Some(item("peer_b", "One", "b/01.flac")),
                ],
            )
        };
        let candidates = [
            group("peer_a", vec![item("peer_a", "One", "a/01.flac")], 0.9),
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.8),
            group("peer_c", vec![item("peer_c", "One", "c/01.flac")], 0.7),
        ];
        downloads.add(slot.clone(), record, &candidates, 3);
        assert_eq!(
            downloads.pools[&slot],
            (vec![item("peer_c", "One", "c/01.flac")], 2, 3),
            "the earlier pick's source counts against no cap of this one"
        );
    }
}
