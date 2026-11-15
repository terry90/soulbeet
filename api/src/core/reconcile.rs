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
use crate::ledger::store::{self, SlotRecord, StoredSearch, UnfinishedJob};
use crate::ledger::{JobEvent, JobState, Ledger, NewSearch, StoredImport, LEDGER};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::transfer_key;
use crate::server_fns::download::failover::{alternates_for, SourcePool};
use crate::server_fns::download::monitor::{DownloadMonitor, WatchedTransfer};
use crate::server_fns::download::process::{
    album_jobs, import_job, resolve_on_disk, unresolved_row, ResolvedSlot,
};
use crate::services::download_backend;
use crate::transfer_registry::{BatchRef, TransferOwner, TransferRegistry};
use async_trait::async_trait;
use shared::download::{
    DownloadEvent, DownloadProgress, DownloadQuery, DownloadState, DownloadableGroup,
    DownloadableItem, JobSlot,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::audio::count_audio_files;
use soulbeet::{DownloadBackend, ImportMode};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// `row` as Failed with `failure`, as `unresolved_row` builds one.
fn failed(row: DownloadProgress, failure: Failure) -> DownloadProgress {
    row.with_state(DownloadState::Failed(String::new()))
        .with_failure(failure)
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

/// The downloading slots of one monitor batch, checked against the backend.
/// Every transfer, path and row carries the JobSlot its slot was given.
#[derive(Default)]
pub(crate) struct Resumed {
    /// Transfers the backend still has, or could not be asked about (the monitor's own
    /// unreadable-list deadline decides those).
    pub watch: Vec<WatchedTransfer>,
    /// Finished transfers whose file is in the batch folder (or anywhere in the download folder
    /// for an adopted one), with the path found: those the backend no longer lists and, in an
    /// album batch that has one, every Completed one it still lists (D15).
    pub finished: Vec<ResolvedSlot>,
    /// (source, transfer id) of the finished transfers the backend still lists, removed once found.
    pub remove: Vec<(String, String)>,
    /// transfer_lost rows, and unresolved_row for a finished transfer whose file is not found.
    pub failed: Vec<DownloadProgress>,
}

/// A finished transfer to look for on disk: its row, the batch folder it
/// was enqueued into (None for an adopted one), and its transfer when the
/// backend still lists it.
struct Finished {
    row: DownloadProgress,
    destination: Option<String>,
    listed: Option<WatchedTransfer>,
}

/// Checks each downloading slot of `batch` against `backend`. A slot's
/// current transfer is its last row's; its last attempt says whether it was
/// adopted. `base` is the download folder.
pub(crate) async fn resume_downloads(
    backend: &Arc<dyn DownloadBackend>,
    base: &Path,
    batch: &BatchRef,
    slots: Vec<(JobSlot, SlotRecord)>,
) -> Resumed {
    let mut resumed = Resumed::default();
    let mut finished = Vec::new();
    for (slot, record) in slots {
        // A slot is downloading only once a row put it there.
        let Some(row) = record.row else {
            continue;
        };
        let row = row.with_job(slot.clone());
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
            Ok(Some(listed)) if listed.state == DownloadState::Completed => {
                finished.push(Finished {
                    row: listed
                        .with_batch(batch.owner.batch_id.clone(), batch.label.clone())
                        .with_job(slot),
                    destination,
                    listed: Some(watched),
                })
            }
            Ok(Some(_)) => resumed.watch.push(watched),
            Err(e) => {
                debug!(
                    "Could not ask {} about transfer {}: {e}",
                    backend.name(),
                    row.id
                );
                resumed.watch.push(watched);
            }
            // The monitor had seen it finish, and a handoff removed the
            // record before the import was queued.
            Ok(None) if row.state == DownloadState::Completed => finished.push(Finished {
                row,
                destination,
                listed: None,
            }),
            Ok(None) => resumed.failed.push(failed(row, transfer_lost())),
        }
    }

    // An album batch's records stay until the whole album is imported, so one
    // already gone means Soulful stopped inside finish_album (D15).
    let whole_album =
        batch.import_mode == ImportMode::Album && finished.iter().any(|f| f.listed.is_none());
    let mut lookups = Vec::new();
    for f in finished {
        match f.listed {
            // A monitor hands it off on its first poll.
            Some(watched) if !whole_album => resumed.watch.push(watched),
            listed => lookups.push((f.row, f.destination, listed.is_some())),
        }
    }
    let claimed = Mutex::new(HashSet::new());
    let paths = futures::future::join_all(lookups.iter().map(|(row, destination, _)| {
        resolve_on_disk(&row.item, base, destination.as_deref(), &claimed)
    }))
    .await;
    for ((row, destination, listed), path) in lookups.into_iter().zip(paths) {
        match path {
            Ok(path) => {
                if listed {
                    resumed.remove.push((row.source.clone(), row.id.clone()));
                }
                resumed.finished.push(ResolvedSlot {
                    row,
                    path,
                    destination,
                });
            }
            Err(why) => resumed.failed.push(unresolved_row(row, base, why)),
        }
    }
    resumed
}

/// What reconciliation starts.
#[async_trait]
pub(crate) trait Resume: Send + Sync {
    /// The backend the downloads of the unfinished jobs ran on.
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure>;
    /// Watches `transfers` for `batch`, failing slots over from `pool`.
    async fn watch(
        &self,
        ctx: &UserCtx,
        batch: BatchRef,
        transfers: Vec<WatchedTransfer>,
        pool: SourcePool,
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
    ) {
        let tx = get_or_create_user_channel(&ctx.id()).await;
        let target = batch.target.clone();
        let monitor = DownloadMonitor::new(transfers, target, tx, batch, LEDGER.clone());
        spawn_monitor(ctx, monitor.with_failover(pool));
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

/// The queued slots of one job, by their first source.
struct Queued {
    ctx: UserCtx,
    job: String,
    folder: Folder,
    import_mode: ImportMode,
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

/// Runs a searching job's pipeline again under a new UI batch id: from the
/// candidates its search stored while that search is live and has them, else
/// searching again, after renewing the search when it is gone or expired.
async fn resume_search(
    ledger: &Ledger,
    resume: &dyn Resume,
    job: UnfinishedJob,
    search: Option<StoredSearch>,
    now: i64,
) {
    // An auto-download's job is made with its search.
    let Some(search_id) = job.search_id else {
        return;
    };
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

/// Resumes every job `unfinished` reads, each slot by its state; indexing
/// slots and jobs waiting for a source choice are left to the index runner
/// and the prune.
pub(crate) async fn reconcile_in(
    db: &SqlitePool,
    ledger: &Ledger,
    queue: &ImportQueue,
    resume: &dyn Resume,
    download_base: &Path,
) {
    let jobs = match store::unfinished(db).await {
        Ok(jobs) => jobs,
        Err(e) => {
            warn!("Could not read the unfinished jobs to resume them: {e}");
            return;
        }
    };
    if jobs.is_empty() {
        return;
    }
    info!("Resuming {} unfinished jobs", jobs.len());

    let now = chrono::Utc::now().timestamp();
    let mut kept = Vec::new();
    let mut importing = Vec::new();
    let mut downloads: Vec<Downloads> = Vec::new();
    let mut queued = Vec::new();
    for job in jobs {
        let user = job.ctx.id();
        let search = match &job.search_id {
            Some(id) => stored_search(db, id, &user).await,
            None => None,
        };
        if job.state == JobState::Searching {
            resume_search(ledger, resume, job, search, now).await;
            continue;
        }

        let candidates = search.and_then(|s| s.candidates).unwrap_or_default();
        let mut waiting = Vec::new();
        for record in job.slots {
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
                        None => fail_rows(ledger, &user, vec![failed(row, import_lost())]).await,
                    }
                }
                JobState::Downloading => {
                    // Its batch is recorded before the row that put it here.
                    let Some(batch_id) = &job.monitor_batch_id else {
                        let rows = shown_row(&slot, &record).into_iter();
                        let rows = rows.map(|row| failed(row, transfer_lost())).collect();
                        fail_rows(ledger, &user, rows).await;
                        continue;
                    };
                    let at = match downloads.iter().position(|d| {
                        d.batch.owner.user_id == user && d.batch.owner.batch_id == *batch_id
                    }) {
                        Some(at) => at,
                        None => {
                            let batch = BatchRef::new(
                                TransferOwner {
                                    user_id: user.clone(),
                                    batch_id: batch_id.clone(),
                                },
                                job.batch_label.clone().unwrap_or_default(),
                                PathBuf::from(&job.folder.path),
                                job.import_mode,
                            );
                            downloads.push(Downloads {
                                ctx: job.ctx.clone(),
                                batch,
                                slots: Vec::new(),
                                pools: HashMap::new(),
                            });
                            downloads.len() - 1
                        }
                    };
                    downloads[at].add(slot, record, &candidates);
                }
                JobState::Queued => waiting.push((slot, record)),
                _ => {}
            }
        }
        if !waiting.is_empty() {
            queued.push(Queued {
                ctx: job.ctx,
                job: job.id,
                folder: job.folder,
                import_mode: job.import_mode,
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
                .map(|row| failed(row, refusal.failure.clone()))
                .collect();
            fail_rows(ledger, &user, rows).await;
        }
    }
    for Interrupted { user, import, rows } in importing {
        let sources: Vec<&Path> = import.sources.iter().map(PathBuf::as_path).collect();
        if count_audio_files(&sources).await > 0 {
            queue.submit(ImportJob::from_stored(import, user)).await;
        } else {
            let rows = rows
                .into_iter()
                .map(|row| failed(row, import_lost()))
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
                .chain(queued.into_iter().map(|q| (q.ctx, q.slots)));
            for (ctx, slots) in stopped {
                let rows = slots
                    .iter()
                    .filter_map(|(slot, record)| shown_row(slot, record))
                    .map(|row| failed(row, failure.clone()))
                    .collect();
                fail_rows(ledger, &ctx.id(), rows).await;
            }
            return;
        }
    };
    for batch in downloads {
        resume_batch(ledger, queue, resume, &backend, download_base, batch).await;
    }
    for job in queued {
        enqueue_again(resume, &backend, job).await;
    }
}

/// Resumes one monitor batch: fails the transfers slskd lost, imports the
/// finished ones and watches the rest.
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
        mut pools,
    } = downloads;
    let user = ctx.id();
    let Resumed {
        watch,
        finished,
        remove,
        failed: lost,
    } = resume_downloads(backend, download_base, &batch, slots).await;
    fail_rows(ledger, &user, lost).await;
    for (source, id) in remove {
        if let Err(e) = backend.cancel_download(&source, &id, true).await {
            warn!("Failed to remove finished transfer {id} from slskd: {e}");
        }
    }
    let imports = match batch.import_mode {
        ImportMode::Album => album_jobs(finished, &batch.target, &user, download_base),
        ImportMode::Singleton => finished
            .into_iter()
            .map(|f| {
                import_job(
                    f.path,
                    ImportMode::Singleton,
                    vec![f.row],
                    &batch.target,
                    &user,
                    download_base,
                )
            })
            .collect(),
    };
    for import in imports {
        queue.submit(import).await;
    }
    if watch.is_empty() {
        return;
    }
    let keys: Vec<_> = watch
        .iter()
        .map(|w| transfer_key(backend.id(), &w.source, &w.filename))
        .collect();
    TransferRegistry::global().claim(&keys, &batch);
    // Every watched transfer is one of the slots given, each with its entry.
    let pool = SourcePool::resumed(
        watch
            .iter()
            .map(|w| pools.remove(&w.job).unwrap_or_default())
            .collect(),
    );
    resume.watch(&ctx, batch, watch, pool).await;
}

/// Enqueues a job's queued slots again by their first sources, into a new
/// batch importing in the job's mode, the search's other candidates standing
/// by as alternates.
async fn enqueue_again(resume: &dyn Resume, backend: &Arc<dyn DownloadBackend>, job: Queued) {
    let items: Vec<(u32, DownloadableItem)> = job
        .slots
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
        job: job.job,
        alternates: alternates_from(&job.candidates, &picked),
        items,
    };
    resume
        .enqueue(&job.ctx, backend.clone(), fresh, acquisition)
        .await;
}

/// Resumes the server's unfinished jobs in the background.
pub fn start() {
    tokio::spawn(reconcile_in(
        &crate::db::DB,
        &LEDGER,
        &IMPORT_QUEUE,
        &Live,
        CONFIG.download_path(),
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
    use soulbeet::ImportedItem;
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
        )
        .await;

        assert!(resumed.watch.is_empty() && resumed.failed.is_empty() && resumed.remove.is_empty());
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
    async fn an_album_stopped_while_importing_is_imported_whole() {
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
        )
        .await;
        assert!(resumed.watch.is_empty() && resumed.failed.is_empty());
        assert_eq!(
            resumed
                .finished
                .iter()
                .map(|f| f.row.job.clone())
                .collect::<Vec<_>>(),
            vec![Some(slot(0)), Some(slot(1))]
        );
        assert_eq!(
            resumed.remove,
            vec![("peer_a".to_string(), "t-02".to_string())]
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
        enqueued: Mutex<Vec<EnqueueStart>>,
        /// job, search id, stored groups
        searched: Mutex<Vec<(String, String, Option<usize>)>>,
    }

    impl Started {
        fn on(backend: Arc<dyn DownloadBackend>) -> Started {
            Started {
                backend,
                watched: Mutex::default(),
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
        ) {
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
        ledger.record(JobEvent::Enqueued {
            job: "downloading".into(),
            monitor_batch_id: "b1".into(),
            batch_label: "Come Over".into(),
        });
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
        async fn watch(&self, _: &UserCtx, _: BatchRef, _: Vec<WatchedTransfer>, _: SourcePool) {
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
                ledger.record(JobEvent::Enqueued {
                    job: id.into(),
                    monitor_batch_id: "b1".into(),
                    batch_label: "Come Over".into(),
                });
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
}
