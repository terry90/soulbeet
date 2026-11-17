//! Getting files: batches, enqueueing a request's files and watching them.
//! The manual download, the auto-download pipeline and cancelling a download
//! each have their own module.

mod auto;
mod cancel;
mod manual;
#[cfg(test)]
mod test_support;

pub use auto::auto_download;
pub(crate) use auto::{fail_job, run_search, AutoRun, AUTO_SELECT_SCORE_THRESHOLD};
pub use cancel::cancel;
pub use manual::download;

use crate::auth::UserCtx;
use crate::core::{database_failure, target_unwritable};
use crate::globals::{get_or_create_user_channel, register_user_task, unregister_user_task};
use crate::ledger::{DuplicateRule, JobEvent, JobState, Ledger};
use crate::models::folder::Folder;
use crate::server_fns::download::enqueue::{
    another_folder, claim_into, claim_request, deliver, failed_reason, nothing_enqueued,
    stopped_rows, transfer_key, RequestEnqueue, Watcher,
};
use crate::server_fns::download::failover::{align_pool_tracks, item_index, SourcePool};
use crate::server_fns::download::monitor::{DownloadMonitor, WatchedTransfer};
use crate::transfer_registry::{BatchRef, InboxReservation, TransferOwner};
use dioxus::logger::tracing::info;
use shared::download::{
    DownloadEvent, DownloadProgress, DownloadableGroup, DownloadableItem, EnqueueStatus, JobSlot,
    QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::error::SoulseekError;
use soulbeet::{DownloadBackend, ImportMode};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name for a manual download's batch: the album its items share, or the
/// first item's title when they span albums.
pub(crate) fn manual_batch_label(items: &[DownloadableItem]) -> String {
    match items.split_first() {
        Some((first, rest))
            if !first.album.trim().is_empty() && rest.iter().all(|i| i.album == first.album) =>
        {
            first.album.clone()
        }
        Some((first, _)) => first.title.clone(),
        None => String::new(),
    }
}

/// The backend call that enqueues a request's files failed.
fn queue_failed(e: &SoulseekError) -> Failure {
    e.failure(Stage::Enqueue, format!("Download queue failed: {e}"))
}

/// The task that ran a request's enqueue and watch ended without an answer.
pub(super) fn request_stopped(e: &tokio::task::JoinError) -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        format!("Download request stopped: {e}"),
    )
}

/// The batch a request starts when it joins no running one. It downloads
/// into `batch_folder(label, batch_id)` and imports into `target` in
/// `import_mode`, treating what the library has by `duplicates`; the
/// request's user owns it.
#[derive(Clone)]
pub struct NewBatch {
    pub batch_id: String,
    pub label: String,
    pub target: PathBuf,
    pub import_mode: ImportMode,
    pub duplicates: DuplicateRule,
}

/// The batch a request's files go into.
pub enum RequestBatch {
    /// One of its own, unless the user already downloads some of its files
    /// into the same folder in the same mode: the rest then join that batch.
    New(NewBatch),
    /// A running batch whose inbox the reservation holds open for this
    /// request: a job resumed at startup sends the files slskd never got
    /// into the batch that holds the rest of it, so its monitor imports the
    /// job's album once.
    Running(BatchRef, InboxReservation),
}

impl From<NewBatch> for RequestBatch {
    fn from(fresh: NewBatch) -> Self {
        Self::New(fresh)
    }
}

impl NewBatch {
    fn owned_by(self, ctx: &UserCtx) -> BatchRef {
        let batch = BatchRef::new(
            TransferOwner {
                user_id: ctx.id(),
                batch_id: self.batch_id,
            },
            self.label,
            self.target,
            self.import_mode,
        );
        batch.with_duplicates(self.duplicates)
    }
}

/// What one job asks the backend for: its files by slot, and other sources
/// for them.
#[derive(Clone)]
pub struct Acquisition {
    pub job: String,
    pub items: Vec<(u32, DownloadableItem)>,
    pub alternates: Vec<DownloadableGroup>,
    /// The most sources each of the job's files may use, the first being the
    /// one enqueued.
    pub max_sources: usize,
}

/// `items` as the slots 0..n of a job.
pub(crate) fn numbered(items: Vec<DownloadableItem>) -> Vec<(u32, DownloadableItem)> {
    (0..).zip(items).collect()
}

/// Creates `folder`'s directory, and returns its path as the import target.
pub(crate) async fn prepare_target(folder: &Folder) -> Result<PathBuf, Failure> {
    let target = PathBuf::from(&folder.path);
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|e| target_unwritable(&e, format!("Failed to create target directory: {e}")))?;
    Ok(target)
}

/// Claims and enqueues the files of `acquisition`'s job for `ctx`, shows
/// their rows on its user's channel, and hands the enqueued ones to a
/// monitor: the running batch's when the request joined one of the user's
/// or `batch` names one, else one of the new batch. Either fails a slot over
/// to its track's alternates.
///
/// Every row it sends carries its slot and is recorded in `ledger`: failed
/// files Failed, queued ones Downloading. The batch a file goes into and the
/// backend request it is sent in are stored before the backend hears of it,
/// so a restart knows where a file slskd took may be; when the ledger cannot
/// store them nothing is sent and the request fails with the database's
/// failure. A file one of the user's batches
/// already downloads into this folder dismisses its slot, as that batch
/// pursues it. A slot the request got no answer for is recorded Failed but
/// not sent, the request's own answer showing why: the same user downloads
/// it into another folder, the ledger could not store its batch, the backend
/// call failed, or the backend answered nothing for it.
///
/// Runs as its own task. Registry keys are claimed from its start, and the
/// client may disconnect at any await and drop the caller: dropping the
/// task's handle does not stop it, so the request always gets as far as a
/// monitor or a release.
pub(crate) async fn enqueue_and_watch(
    ledger: &Ledger,
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    batch: impl Into<RequestBatch>,
    acquisition: Acquisition,
) -> Result<Vec<QueuedDownload>, Failure> {
    tokio::spawn(queue_and_watch(
        ledger.clone(),
        ctx.clone(),
        backend,
        batch.into(),
        acquisition,
    ))
    .await
    .map_err(|e| request_stopped(&e))?
}

/// `enqueue_and_watch`'s work, in the task it runs as.
pub(super) async fn queue_and_watch(
    ledger: Ledger,
    ctx: UserCtx,
    backend: Arc<dyn DownloadBackend>,
    batch: RequestBatch,
    acquisition: Acquisition,
) -> Result<Vec<QueuedDownload>, Failure> {
    let Acquisition {
        job,
        items: picked,
        alternates,
        max_sources,
    } = &acquisition;
    let items: Vec<DownloadableItem> = picked.iter().map(|(_, item)| item.clone()).collect();
    let claimed = match batch {
        RequestBatch::New(fresh) => claim_request(&backend, items.clone(), fresh.owned_by(&ctx)),
        RequestBatch::Running(batch, reservation) => {
            claim_into(&backend, items.clone(), batch, reservation)
        }
    };
    let request = backend.new_request();
    // Stored before the backend hears of a file, so a restart after it took one finds the batch
    // and tells the transfers of this send by its request. Nothing is sent when the ledger
    // cannot store it.
    let unstored = match claimed.sends() {
        true => ledger
            .record_stored(JobEvent::Enqueued {
                job: job.clone(),
                monitor_batch_id: claimed.batch().owner.batch_id.clone(),
                batch_label: claimed.batch().label.clone(),
                request: request.clone(),
            })
            .await
            .err()
            .map(|e| database_failure(Stage::Enqueue, e)),
        false => None,
    };
    let enq = match unstored {
        None => claimed.enqueue(&backend, &request).await,
        Some(_) => claimed.withdraw(&backend),
    };
    let refusal = enq.refusal();
    let RequestEnqueue {
        batch,
        results,
        watcher,
        error,
        elsewhere,
    } = enq;

    // The answer for each slot, by its index in `picked`. Every answer is
    // for a file the request asked for: the enqueue drops the others.
    let mut answers: Vec<Option<&QueuedDownload>> = vec![None; picked.len()];
    for q in &results {
        if let Some(at) = item_index(q, &items) {
            answers[at] = Some(q);
        }
    }
    let slot = |n: u32| JobSlot {
        job: job.clone(),
        slot: n,
    };
    let (enqueued_slots, enqueued): (Vec<u32>, Vec<QueuedDownload>) = picked
        .iter()
        .zip(&answers)
        .filter_map(|((n, _), answer)| {
            answer
                .filter(|q| q.status == EnqueueStatus::Enqueued)
                .map(|q| (*n, q.clone()))
        })
        .unzip();

    // A request only ever joins a batch of its own user, so every row of it
    // goes to that user's channel.
    let user = ctx.id();
    let tx = get_or_create_user_channel(&user).await;
    let row = |p: DownloadProgress, n: u32| {
        p.with_batch(batch.owner.batch_id.clone(), batch.label.clone())
            .with_backend(backend.id())
            .with_job(slot(n))
    };

    let mut failed = Vec::new();
    let mut unanswered = Vec::new();
    let mut queued = Vec::new();
    let mut dismissed = Vec::new();
    for ((n, item), answer) in picked.iter().zip(answers) {
        let Some(q) = answer else {
            let failure = if elsewhere.contains(&transfer_key(backend.id(), &item.source, &item.id))
            {
                another_folder(RetryAdvice::OtherSource)
            } else if let Some(failure) = &unstored {
                failure.clone()
            } else if let Some(e) = &error {
                queue_failed(e)
            } else {
                nothing_enqueued()
            };
            let p = DownloadProgress::failed(
                item.id.clone(),
                item.source.clone(),
                item.id.clone(),
                failure,
            );
            unanswered.push(row(p, *n));
            continue;
        };
        match failed_reason(&q.status) {
            Some(reason) => failed.push(row(
                DownloadProgress::failed(q.id.clone(), q.source.clone(), q.item.clone(), reason),
                *n,
            )),
            None if q.status == EnqueueStatus::Enqueued => queued.push(row(
                DownloadProgress::queued(q.id.clone(), q.source.clone(), q.item.clone(), q.size),
                *n,
            )),
            None => dismissed.push(slot(*n)),
        }
    }

    if !failed.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(failed.clone()));
    }
    failed.extend(unanswered);
    if !failed.is_empty() {
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: failed,
        });
    }
    // Send initial "Queued" state immediately so UI shows the downloads right away
    if !queued.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(queued.clone()));
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: queued,
        });
    }
    if !dismissed.is_empty() {
        ledger.record(JobEvent::Dismiss(dismissed));
    }

    match watcher {
        Watcher::Joined(reservation) => {
            info!(
                "Added {} downloads to running batch {}",
                enqueued.len(),
                batch.owner.batch_id
            );
            let lost = deliver(reservation, &enqueued, &acquisition, &backend, &batch).await;
            if !lost.is_empty() {
                let rows = stopped_rows(&lost, &batch);
                let _ = tx.send(DownloadEvent::Progress(rows.clone()));
                ledger.record(JobEvent::Slots {
                    state: JobState::Failed,
                    rows,
                });
            }
        }
        Watcher::Own if enqueued.is_empty() && batch.inbox.close_if_idle() => {}
        // Starts empty when nothing was enqueued but another request joined
        // the batch while this one was enqueueing.
        Watcher::Own => {
            info!(
                "Started monitoring downloads: {:?}",
                enqueued.iter().map(|d| &d.item).collect::<Vec<_>>()
            );
            let transfers = enqueued
                .iter()
                .zip(enqueued_slots)
                .map(|(q, n)| WatchedTransfer::enqueued(q, slot(n)))
                .collect();
            // Pool slots must line up with the monitor's slots, which follow
            // `enqueued`, not `items`.
            let pool = SourcePool::from_tracks(
                &align_pool_tracks(&enqueued, &items),
                alternates,
                *max_sources,
            );
            let target = batch.target.clone();
            spawn_monitor(
                &ctx,
                DownloadMonitor::new(transfers, target, tx, batch, ledger).with_failover(pool),
            );
        }
    }

    if let Some(failure) = unstored {
        return Err(failure);
    }
    if let Some(e) = error {
        return Err(queue_failed(&e));
    }
    if let Some(failure) = refusal {
        return Err(failure);
    }
    Ok(results)
}

/// Registers `ctx`'s user task and runs `monitor` in it.
pub(crate) fn spawn_monitor(ctx: &UserCtx, mut monitor: DownloadMonitor) {
    let user = ctx.id();
    tokio::spawn(async move {
        register_user_task(&user).await;
        monitor.run().await;
        unregister_user_task(&user).await;
    });
}

/// Deletes `files`, a job's own downloaded files, and prunes the folders
/// they leave empty up to `base`; a file already gone still has its folder
/// pruned, or, when that folder is gone too, the nearest folder above it that
/// is left. Nothing but these files is deleted: a folder may hold other
/// downloads. A file whose folder (or nearest folder left) is not inside
/// `base`, symlinks resolved, is never touched: it is not a download. The
/// paths it could not delete, with the OS error: a file gone already is one,
/// with `NotFound`.
pub(crate) async fn discard_files(
    files: &[PathBuf],
    base: &Path,
) -> Vec<(PathBuf, std::io::Error)> {
    let mut failed = Vec::new();
    let real_base = match tokio::fs::canonicalize(base).await {
        Ok(real) => real,
        Err(e) => {
            let kind = e.kind();
            return files
                .iter()
                .map(|f| (f.clone(), std::io::Error::new(kind, e.to_string())))
                .collect();
        }
    };
    for file in files {
        let folder = match download_in(file, &real_base).await {
            Ok(Download::At { folder, name }) => {
                if let Err(e) = tokio::fs::remove_file(folder.join(&name)).await {
                    let gone = e.kind() == std::io::ErrorKind::NotFound;
                    failed.push((file.clone(), e));
                    if !gone {
                        continue;
                    }
                }
                folder
            }
            Ok(Download::FolderGone { gone, nearest }) => {
                failed.push((file.clone(), gone));
                nearest
            }
            Err(e) => {
                failed.push((file.clone(), e));
                continue;
            }
        };
        if let Err(e) = crate::server_fns::cleanup_empty_ancestors(&folder, &real_base).await {
            failed.push((folder, e));
        }
    }
    failed
}

/// Where a download is on disk.
enum Download {
    /// The real path of its folder, and its name.
    At {
        folder: PathBuf,
        name: std::ffi::OsString,
    },
    /// Its folder is gone (`gone`, `NotFound`); `nearest` is the real path of the nearest
    /// folder above it that is left.
    FolderGone {
        gone: std::io::Error,
        nearest: PathBuf,
    },
}

/// Where `file`, a download, is: refused unless the real path of its folder,
/// or of the nearest folder above it left when that one is gone, is
/// `real_base` or inside it, and the name is a plain one.
async fn download_in(file: &Path, real_base: &Path) -> std::io::Result<Download> {
    let outside = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("not inside the download folder {}", real_base.display()),
        )
    };
    let (Some(parent), Some(name)) = (file.parent(), file.file_name()) else {
        return Err(outside());
    };
    let inside = |real: PathBuf| match real.starts_with(real_base) {
        true => Ok(real),
        false => Err(outside()),
    };
    let gone = match tokio::fs::canonicalize(parent).await {
        Ok(folder) => {
            return Ok(Download::At {
                folder: inside(folder)?,
                name: name.to_os_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => e,
        Err(e) => return Err(e),
    };
    for above in parent.ancestors().skip(1) {
        match tokio::fs::canonicalize(above).await {
            Ok(nearest) => {
                return Ok(Download::FolderGone {
                    gone,
                    nearest: inside(nearest)?,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Err(gone)
}

/// The search's groups without the picked files, best score first, empty
/// groups dropped.
pub(crate) fn alternates_from(
    groups: &[DownloadableGroup],
    picked: &[DownloadableItem],
) -> Vec<DownloadableGroup> {
    let mut left: Vec<DownloadableGroup> = groups
        .iter()
        .map(|group| DownloadableGroup {
            items: group
                .items
                .iter()
                .filter(|item| {
                    !picked
                        .iter()
                        .any(|p| p.id == item.id && p.source == item.source)
                })
                .cloned()
                .collect(),
            ..group.clone()
        })
        .filter(|group| !group.items.is_empty())
        .collect();
    left.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    left
}

#[cfg(test)]
mod tests {
    use super::test_support::{ctx_of, own_ctx};
    use super::*;
    use crate::auth::UserId;
    use crate::ledger::{DuplicateRule, JobKind, NewJob};
    use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
    use crate::server_fns::download::test_support::{
        batch, group, item, key, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferRegistry;
    use shared::download::DownloadQuery;
    use shared::failure::{Cause, Service};

    /// A new batch importing into `target` in `running`'s import mode.
    fn new_batch_beside(running: &BatchRef, target: PathBuf) -> NewBatch {
        NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: "Come Over".to_string(),
            target,
            import_mode: running.import_mode,
            duplicates: DuplicateRule::Guarded,
        }
    }

    /// Job `j1` asking for `items` as slots 0..n, with `alternates`.
    fn acquisition(
        items: Vec<DownloadableItem>,
        alternates: Vec<DownloadableGroup>,
    ) -> Acquisition {
        Acquisition {
            job: "j1".into(),
            items: numbered(items),
            alternates,
            max_sources: MAX_SOURCES_PER_TRACK,
        }
    }

    fn new_batch(label: &str) -> NewBatch {
        NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: label.to_string(),
            target: PathBuf::from("/music"),
            import_mode: ImportMode::Singleton,
            duplicates: DuplicateRule::Guarded,
        }
    }

    #[test]
    fn a_fresh_batch_carries_its_duplicates_rule() {
        let running = batch("Come Over");
        let ctx = ctx_of(&running);
        for rule in [DuplicateRule::Guarded, DuplicateRule::Skip] {
            let fresh = NewBatch {
                duplicates: rule.clone(),
                ..new_batch("Come Over")
            };
            assert_eq!(fresh.owned_by(&ctx).duplicates, rule);
        }
    }

    #[tokio::test]
    async fn discarding_deletes_only_the_jobs_files_and_names_what_it_could_not_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("dl");
        let file = base.join("X [1f2e3d4c]/01.flac");
        // An album folder slskd's default layout shares with another download of the same
        // remote folder name: that download's file and the album's cover stay.
        let album = base.join("Come Over");
        let others = album.join("05 - Other Song.flac");
        let gone = base.join("Z [3d4c5b6a]/01.flac");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();
        std::fs::create_dir_all(&album).unwrap();
        for name in ["07.flac", "08.flac", "cover.jpg"] {
            std::fs::write(album.join(name), b"x").unwrap();
        }
        std::fs::write(&others, b"x").unwrap();
        std::fs::create_dir_all(gone.parent().unwrap()).unwrap();

        let files = [
            file.clone(),
            album.join("07.flac"),
            album.join("08.flac"),
            gone.clone(),
        ];
        let failed: Vec<(PathBuf, String)> = (discard_files(&files, &base).await.into_iter())
            .map(|(path, e)| (path, e.to_string()))
            .collect();

        assert_eq!(
            failed,
            vec![(
                gone.clone(),
                "No such file or directory (os error 2)".to_string()
            )]
        );
        assert!(!file.exists() && !file.parent().unwrap().exists());
        assert!(!album.join("07.flac").exists() && !album.join("08.flac").exists());
        assert!(others.exists() && album.join("cover.jpg").exists());
        assert!(
            !gone.parent().unwrap().exists(),
            "a gone file's empty folder is pruned"
        );
        assert!(base.exists());
    }

    #[tokio::test]
    async fn discarding_a_file_whose_folder_is_gone_prunes_the_nearest_folder_left() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("dl");
        // The album's disc folder is gone; the album folder is left, empty.
        let album = base.join("Y [2e3d4c5b]");
        let file = album.join("CD1/01.flac");
        std::fs::create_dir_all(&album).unwrap();
        // Outside the download folder, an empty folder above a gone one stays.
        let library = tmp.path().join("music/bob");
        let outside = library.join("Gone/01.flac");
        std::fs::create_dir_all(&library).unwrap();

        let failed: Vec<(PathBuf, std::io::ErrorKind)> =
            (discard_files(&[file.clone(), outside.clone()], &base)
                .await
                .into_iter())
            .map(|(path, e)| (path, e.kind()))
            .collect();

        assert_eq!(
            failed,
            vec![
                (file, std::io::ErrorKind::NotFound),
                (outside, std::io::ErrorKind::PermissionDenied)
            ]
        );
        assert!(!album.exists(), "the empty album folder is pruned");
        assert!(base.exists() && library.exists());
    }

    #[tokio::test]
    async fn discarding_never_deletes_a_file_outside_the_download_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("dl");
        let library = tmp.path().join("music/bob/Album");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(library.join("01.flac"), b"bob's").unwrap();
        std::os::unix::fs::symlink(&library, base.join("Linked")).unwrap();

        let files = [
            library.join("01.flac"),
            base.join("../music/bob/Album/01.flac"),
            base.join("Linked/01.flac"),
        ];
        let failed = discard_files(&files, &base).await;

        assert_eq!(
            failed
                .iter()
                .map(|(p, e)| (p.clone(), e.kind()))
                .collect::<Vec<_>>(),
            files
                .iter()
                .map(|f| (f.clone(), std::io::ErrorKind::PermissionDenied))
                .collect::<Vec<_>>()
        );
        assert!(library.join("01.flac").exists());
    }

    #[test]
    fn manual_label_is_the_shared_album_else_the_first_title() {
        let same_album = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];
        assert_eq!(manual_batch_label(&same_album), "Come Over");

        let mut mixed = same_album.clone();
        mixed[1].album = "Other".to_string();
        assert_eq!(manual_batch_label(&mixed), "One");

        let mut no_album = same_album;
        for i in &mut no_album {
            i.album = " ".to_string();
        }
        assert_eq!(manual_batch_label(&no_album), "One");
    }

    #[tokio::test]
    async fn a_request_dropped_mid_enqueue_still_hands_its_transfers_over() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "p", "1.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        let items = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];

        // The handler is dropped while slskd is still answering the enqueue,
        // as when the client disconnects.
        let ctx = ctx_of(&running);
        let handler = tokio::spawn(async move {
            enqueue_and_watch(
                &Ledger::recording().0,
                &ctx,
                backend,
                fresh,
                acquisition(items, Vec::new()),
            )
            .await
        });
        reached.notified().await;
        handler.abort();
        assert!(handler.await.unwrap_err().is_cancelled());
        release.notify_one();

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let slots = running.inbox.take();
                if !slots.is_empty() {
                    break slots;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the enqueued transfer never reached the running batch");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].filename, "2.flac");
        assert_eq!(delivered[0].transfer_id, "t-2.flac");
    }

    #[tokio::test]
    async fn a_request_that_joins_a_running_batch_hands_over_its_alternates() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Two", "b/02.flac")],
            0.8,
        )];

        enqueue_and_watch(
            &Ledger::recording().0,
            &ctx_of(&running),
            backend,
            fresh,
            acquisition(
                vec![
                    item("peer_a", "One", "01.flac"),
                    item("peer_a", "Two", "02.flac"),
                ],
                rest,
            ),
        )
        .await
        .unwrap();

        let slots = running.inbox.take();
        assert_eq!(slots.len(), 1);
        assert_eq!(
            (slots[0].filename.as_str(), slots[0].alternates.clone()),
            ("02.flac", vec![item("peer_b", "Two", "b/02.flac")])
        );
    }

    #[tokio::test]
    async fn a_request_only_for_files_downloading_elsewhere_fails_as_another_folder() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, PathBuf::from("/other-library"));
        let got = enqueue_and_watch(
            &Ledger::recording().0,
            &ctx_of(&running),
            backend,
            fresh,
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await;
        assert_eq!(got.unwrap_err(), another_folder(RetryAdvice::NotRetryable));
        assert!(stub.destinations().is_empty());
    }

    #[tokio::test]
    async fn a_failed_backend_call_is_the_requests_failure() {
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        let f = enqueue_and_watch(
            &Ledger::recording().0,
            &own_ctx(),
            backend,
            new_batch("X"),
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await
        .unwrap_err();
        let e = SoulseekError::InvalidResponse(
            "batch enqueue response: expected value at line 1 column 1 (body 20 bytes)".into(),
        );
        assert_eq!(
            (f.code, f.stage, f.message),
            (
                FailureCode::BackendResponseUnreadable,
                Stage::Enqueue,
                format!("Download queue failed: {e}")
            )
        );
    }

    #[tokio::test]
    async fn failed_rows_reach_the_users_channel_stamped_with_batch_and_backend() {
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        let ctx = own_ctx();
        let fresh = new_batch("Come Over");
        let batch_id = fresh.batch_id.clone();
        let mut rx = crate::globals::get_or_create_user_channel(&ctx.id())
            .await
            .subscribe();
        let results = enqueue_and_watch(
            &Ledger::recording().0,
            &ctx,
            backend.clone(),
            fresh,
            acquisition(vec![item("peer_a", "One", "01.flac")], Vec::new()),
        )
        .await
        .unwrap();
        assert!(matches!(results[0].status, EnqueueStatus::Failed(_)));
        let Ok(DownloadEvent::Progress(rows)) = rx.try_recv() else {
            panic!("no row sent")
        };
        assert_eq!(
            (rows[0].batch_id.as_deref(), rows[0].backend.as_deref()),
            (Some(batch_id.as_str()), Some(backend.id()))
        );
        assert_eq!(
            rows[0].failure.as_ref().map(|f| f.code),
            Some(FailureCode::PeerOffline)
        );
    }

    #[test]
    fn a_hand_picked_download_keeps_the_other_sources_of_its_search() {
        let groups = vec![
            group(
                "peer_a",
                vec![
                    item("peer_a", "One", "a/01.flac"),
                    item("peer_a", "Two", "a/02.flac"),
                ],
                0.6,
            ),
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "Two", "c/02.flac")], 0.4),
        ];
        let picked = vec![
            item("peer_a", "One", "a/01.flac"),
            item("peer_a", "Two", "a/02.flac"),
            item("peer_c", "Two", "c/02.flac"),
        ];
        let left = alternates_from(&groups, &picked);
        assert_eq!(
            left.iter()
                .map(|g| (g.source.as_str(), g.items.len()))
                .collect::<Vec<_>>(),
            vec![("peer_b", 1)]
        );
    }

    /// Each row's slot and failure.
    type SlotRows = Vec<(Option<JobSlot>, Option<Failure>)>;

    /// The Slots events of `events`: their state, and each row's slot and failure.
    fn slot_events(events: Vec<JobEvent>) -> Vec<(JobState, SlotRows)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                JobEvent::Slots { state, rows } => Some((
                    state,
                    rows.into_iter().map(|r| (r.job, r.failure)).collect(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_file_of_a_request_is_recorded_on_its_slot() {
        let (ledger, mut recorded) = Ledger::recording();
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap();
        let failed: Vec<(JobState, Vec<Option<FailureCode>>)> = slot_events(recorded.events())
            .into_iter()
            .map(|(state, rows)| {
                (
                    state,
                    rows.into_iter().map(|(_, f)| f.map(|f| f.code)).collect(),
                )
            })
            .collect();
        assert_eq!(
            failed,
            vec![(JobState::Failed, vec![Some(FailureCode::PeerOffline)])]
        );
    }

    #[tokio::test]
    async fn files_a_request_got_no_answer_for_fail_their_slots() {
        // The backend call fails: both slots fail with the request's own failure.
        let (ledger, mut recorded) = Ledger::recording();
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        let returned = enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items: vec![
                    (0, item("peer_a", "One", "01.flac")),
                    (1, item("peer_a", "Two", "02.flac")),
                ],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap_err();
        let slot = |n| {
            Some(JobSlot {
                job: "j1".into(),
                slot: n,
            })
        };
        assert_eq!(
            slot_events(recorded.events()),
            vec![(
                JobState::Failed,
                vec![(slot(0), Some(returned.clone())), (slot(1), Some(returned))]
            )]
        );

        // The same user downloads the only file into another folder.
        let (ledger, mut recorded) = Ledger::recording();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, PathBuf::from("/elsewhere"));
        let refused = enqueue_and_watch(
            &ledger,
            &ctx_of(&running),
            backend,
            fresh,
            Acquisition {
                job: "j2".into(),
                items: vec![(0, item("peer_a", "One", "01.flac"))],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(refused, another_folder(RetryAdvice::NotRetryable));
        assert_eq!(
            slot_events(recorded.events()),
            vec![(
                JobState::Failed,
                vec![(
                    Some(JobSlot {
                        job: "j2".into(),
                        slot: 0
                    }),
                    Some(another_folder(RetryAdvice::OtherSource))
                )]
            )]
        );
        assert!(stub.destinations().is_empty());
    }

    #[tokio::test]
    async fn a_request_whose_enqueue_fails_ends_its_job() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
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
        let items = vec![
            (0, item("peer_a", "One", "01.flac")),
            (1, item("peer_a", "Two", "02.flac")),
        ];
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: items.clone(),
        });
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::TransportError]);
        enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend,
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items,
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap_err();
        ledger.settled().await;
        // Terminal, so Task 12's `unfinished` never returns it.
        assert_eq!(
            crate::ledger::store::job_state(&db, "j1").await,
            Some(JobState::Failed)
        );
    }

    #[tokio::test]
    async fn a_file_another_batch_already_downloads_dismisses_its_slot_and_names_that_batch() {
        let (ledger, mut recorded) = Ledger::recording();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        enqueue_and_watch(
            &ledger,
            &ctx_of(&running),
            backend,
            fresh,
            Acquisition {
                job: "j2".into(),
                items: vec![
                    (0, item("peer_a", "One", "01.flac")),
                    (1, item("peer_a", "Two", "02.flac")),
                ],
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap();
        let events = recorded.events();
        assert!(events.contains(&JobEvent::Dismiss(vec![JobSlot {
            job: "j2".into(),
            slot: 0
        }])));
        assert!(events.contains(&JobEvent::Enqueued {
            job: "j2".into(),
            monitor_batch_id: running.owner.batch_id.clone(),
            batch_label: "Come Over".into(),
            request: stub.requests()[0].clone(),
        }));
        assert_eq!(
            running
                .inbox
                .take()
                .iter()
                .map(|s| s.job.clone())
                .collect::<Vec<_>>(),
            vec![JobSlot {
                job: "j2".into(),
                slot: 1
            }]
        );
    }

    #[tokio::test]
    async fn a_folder_that_cannot_be_created_is_an_unwritable_target() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("music");
        std::fs::write(&file, b"").unwrap();
        let folder = Folder {
            id: "f1".into(),
            user_id: "user-a".into(),
            name: "Music".into(),
            path: file.join("Come Over").to_string_lossy().into_owned(),
            shared: false,
        };
        let f = prepare_target(&folder).await.unwrap_err();
        let os = std::io::Error::from_raw_os_error(20); // ENOTDIR: "Not a directory (os error 20)"
        assert_eq!(
            (f.code, f.stage, f.retry, f.message),
            (
                FailureCode::ImportTargetUnwritable,
                Stage::Import,
                RetryAdvice::NotRetryable,
                format!("Failed to create target directory: {os}")
            )
        );
        assert_eq!(
            f.cause,
            Some(Cause::new(Service::Filesystem, None, os.to_string()))
        );
    }

    #[tokio::test]
    async fn the_batch_is_recorded_before_the_backend_hears_of_a_file() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        // Joining a running batch, so no monitor starts.
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "p", "1.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        let (ledger, mut recorded) = Ledger::recording();
        let ctx = ctx_of(&running);
        let request = tokio::spawn(async move {
            let items = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];
            enqueue_and_watch(&ledger, &ctx, backend, fresh, acquisition(items, vec![])).await
        });

        reached.notified().await;
        let enqueued = JobEvent::Enqueued {
            job: "j1".into(),
            monitor_batch_id: running.owner.batch_id.clone(),
            batch_label: running.label.clone(),
            request: stub.requests()[0].clone(),
        };
        assert_eq!(recorded.events(), vec![enqueued]);
        release.notify_one();
        request.await.unwrap().unwrap();
        // 2.flac is queued; 1.flac, which the running batch downloads, is dismissed.
        let after: Vec<Option<JobState>> = recorded
            .events()
            .into_iter()
            .map(|e| match e {
                JobEvent::Slots { state, .. } => Some(state),
                JobEvent::Dismiss(_) => None,
                other => panic!("only rows follow the send: {other:?}"),
            })
            .collect();
        assert_eq!(after, vec![Some(JobState::Downloading), None]);
    }

    #[tokio::test]
    async fn a_request_whose_batch_the_ledger_cannot_store_sends_nothing_and_fails() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let picked = numbered(vec![item("peer_a", "One", "01.flac")]);
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                    picked: picked.clone(),
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_batches BEFORE UPDATE OF monitor_batch_id ON jobs \
             BEGIN SELECT RAISE(ABORT, 'this test refuses the write'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();

        let f = enqueue_and_watch(
            &ledger,
            &own_ctx(),
            backend.clone(),
            new_batch("Come Over"),
            Acquisition {
                job: "j1".into(),
                items: picked,
                alternates: vec![],
                max_sources: MAX_SOURCES_PER_TRACK,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::Enqueue,
                "Soulbeet's database is unavailable"
            )
        );
        assert!(stub.destinations().is_empty(), "a file was sent unstored");
        assert!(TransferRegistry::global()
            .may_adopt(&key(&backend, "peer_a", "01.flac"), &batch("Y").owner));
    }

    #[tokio::test]
    async fn the_backend_hears_of_a_file_only_once_its_batch_is_stored() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                    picked: numbered(vec![
                        item("p", "Zero", "slow/0.flac"),
                        item("p", "One", "slow/1.flac"),
                    ]),
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        // Joining a running batch, so no monitor starts.
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "p", "slow/0.flac")], &running);
        let fresh = new_batch_beside(&running, running.target.clone());
        // A slow writer: the pool's one connection is taken until the test lets it go.
        let busy = db.acquire().await.unwrap();
        let ctx = ctx_of(&running);
        let request = tokio::spawn({
            let ledger = ledger.clone();
            async move {
                let items = vec![
                    item("p", "Zero", "slow/0.flac"),
                    item("p", "One", "slow/1.flac"),
                ];
                enqueue_and_watch(&ledger, &ctx, backend, fresh, acquisition(items, vec![])).await
            }
        });

        let early = tokio::time::timeout(std::time::Duration::from_millis(300), reached.notified());
        assert!(
            early.await.is_err(),
            "the backend heard of the file before its batch was stored"
        );
        drop(busy);
        reached.notified().await;
        let stored: Option<String> =
            sqlx::query_scalar("SELECT monitor_batch_id FROM jobs WHERE id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored, Some(running.owner.batch_id.clone()));
        release.notify_one();
        request.await.unwrap().unwrap();
    }
}
