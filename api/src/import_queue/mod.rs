//! One import at a time per library. Downloads, discovery and promote all
//! import through here, so a row waiting for another job's import says so
//! instead of reading "Importing".

mod retry;
mod rows;
#[cfg(test)]
pub(crate) mod test_support;

pub use retry::{forget_retries, retry_import_for};

use async_trait::async_trait;
use futures::FutureExt;
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use soulbeet::audio::{count_audio_files, is_audio_path};
use soulbeet::{ImportMode, ImportOutcome, ImportResult, MusicImporter};
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use tokio::sync::watch;
use tracing::{info, warn};

use retry::RetryStore;
use rows::{rows_for_done, rows_with_state};

const WORKER_STOPPED: &str = "import worker stopped";

/// Who a job reports to. The rows are already stamped with their batch.
#[derive(Debug, Clone)]
pub struct JobOwner {
    pub username: String,
    pub rows: Vec<DownloadProgress>,
}

#[derive(Debug, Clone)]
pub struct ImportJob {
    pub sources: Vec<PathBuf>,
    pub target: PathBuf,
    pub mode: ImportMode,
    /// None for discovery and promote, which have no rows.
    pub owner: Option<JobOwner>,
    /// List the target around the run and report the audio files it gained.
    pub snapshot_target: bool,
    /// Prune the emptied source folders up to (not including) this root.
    pub cleanup_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub result: ImportResult,
    /// Audio files the run added, relative to the target. Only filled for
    /// `snapshot_target` jobs.
    pub new_files: Vec<PathBuf>,
}

impl JobOutcome {
    fn of(result: ImportResult) -> Self {
        JobOutcome {
            result,
            new_files: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub enum JobState {
    /// `position` jobs are ahead, counting the running one.
    Queued {
        position: u32,
    },
    Running,
    Done(JobOutcome),
}

pub struct ImportHandle {
    rx: watch::Receiver<JobState>,
}

impl ImportHandle {
    pub async fn wait(mut self) -> JobOutcome {
        let done = match self.rx.wait_for(|s| matches!(s, JobState::Done(_))).await {
            Ok(state) => match &*state {
                JobState::Done(outcome) => Some(outcome.clone()),
                _ => None,
            },
            Err(_) => None,
        };
        done.unwrap_or_else(|| JobOutcome::of(ImportResult::failed(WORKER_STOPPED.into())))
    }
}

#[async_trait]
pub trait QueueDeps: Send + Sync {
    async fn importer(&self) -> Result<Arc<dyn MusicImporter>, String>;
    async fn emit(&self, username: &str, event: DownloadEvent);
}

struct Waiting {
    job: ImportJob,
    state: watch::Sender<JobState>,
}

/// A target's worker and the jobs waiting for it. The entry exists exactly
/// as long as its worker runs.
struct TargetEntry {
    generation: u64,
    running: bool,
    queue: VecDeque<Waiting>,
}

/// Rows to send, grouped by username in first-seen order.
type UserRows = Vec<(String, Vec<DownloadProgress>)>;

struct Shared {
    targets: Mutex<HashMap<PathBuf, TargetEntry>>,
    next_generation: AtomicU64,
    deps: Arc<dyn QueueDeps>,
    /// Row events are built and sent under this lock, from the job states
    /// at that moment, so a late event can never show an older state.
    emit_lock: tokio::sync::Mutex<()>,
    retries: RetryStore,
}

pub struct ImportQueue {
    shared: Arc<Shared>,
}

impl ImportQueue {
    pub fn new(deps: Arc<dyn QueueDeps>) -> Self {
        ImportQueue {
            shared: Arc::new(Shared {
                targets: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(0),
                deps,
                emit_lock: tokio::sync::Mutex::new(()),
                retries: RetryStore::default(),
            }),
        }
    }

    pub async fn submit(&self, job: ImportJob) -> ImportHandle {
        let key = match canonical_target(&job.target).await {
            Ok(key) => key,
            Err(e) => {
                let result =
                    ImportResult::failed(format!("Could not create {}: {e}", job.target.display()));
                let (state, rx) = watch::channel(JobState::Running);
                self.shared
                    .finish(&job, JobOutcome::of(result), &state)
                    .await;
                return ImportHandle { rx };
            }
        };

        let owner = job.owner.clone();
        let (state, rx) = watch::channel(JobState::Queued { position: 0 });
        let new_worker = {
            let mut targets = self.shared.targets();
            let spawn = !targets.contains_key(&key);
            let entry = targets.entry(key.clone()).or_insert_with(|| TargetEntry {
                generation: self.shared.next_generation.fetch_add(1, Ordering::Relaxed),
                running: false,
                queue: VecDeque::new(),
            });
            let position = u32::from(entry.running) + entry.queue.len() as u32;
            state.send_replace(JobState::Queued { position });
            entry.queue.push_back(Waiting { job, state });
            spawn.then_some(entry.generation)
        };
        if let Some(generation) = new_worker {
            tokio::spawn(work(self.shared.clone(), key, generation));
        }

        if let Some(owner) = owner {
            let _emit = self.shared.emit_lock.lock().await;
            let position = match *rx.borrow() {
                JobState::Queued { position } => position,
                _ => 0,
            };
            if position > 0 {
                let rows = rows_with_state(&owner.rows, DownloadState::ImportQueued(position));
                self.shared
                    .deps
                    .emit(&owner.username, DownloadEvent::Progress(rows))
                    .await;
            }
        }
        ImportHandle { rx }
    }
}

/// Removes the worker's entry if the worker dies without removing it itself.
struct ExitGuard {
    shared: Arc<Shared>,
    key: PathBuf,
    generation: u64,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let mut targets = self.shared.targets();
        if targets
            .get(&self.key)
            .is_some_and(|e| e.generation == self.generation)
        {
            targets.remove(&self.key);
        }
    }
}

async fn work(shared: Arc<Shared>, key: PathBuf, generation: u64) {
    let _guard = ExitGuard {
        shared: shared.clone(),
        key: key.clone(),
        generation,
    };
    while let Some((job, state, moved)) = shared.next_job(&key, generation) {
        {
            let _emit = shared.emit_lock.lock().await;
            if let Some(owner) = &job.owner {
                let rows = rows_with_state(&owner.rows, DownloadState::Importing);
                shared
                    .deps
                    .emit(&owner.username, DownloadEvent::Progress(rows))
                    .await;
            }
            for (username, rows) in moved {
                shared
                    .deps
                    .emit(&username, DownloadEvent::Progress(rows))
                    .await;
            }
        }
        let outcome = AssertUnwindSafe(shared.execute(&key, &job))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                warn!("Import into {} panicked", key.display());
                JobOutcome::of(ImportResult::failed(WORKER_STOPPED.into()))
            });
        shared.finish(&job, outcome, &state).await;
    }
}

impl Shared {
    fn targets(&self) -> MutexGuard<'_, HashMap<PathBuf, TargetEntry>> {
        self.targets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the next job on `key`, or removes the entry when none is left.
    /// Also returns the waiting rows whose position moved up.
    fn next_job(
        &self,
        key: &Path,
        generation: u64,
    ) -> Option<(ImportJob, watch::Sender<JobState>, UserRows)> {
        let mut targets = self.targets();
        let entry = targets
            .get_mut(key)
            .filter(|e| e.generation == generation)?;
        let Some(next) = entry.queue.pop_front() else {
            targets.remove(key);
            return None;
        };
        // Positions count the running job, so they only move when one ran before.
        let moved_up = std::mem::replace(&mut entry.running, true);
        next.state.send_replace(JobState::Running);
        let mut moved: UserRows = Vec::new();
        for (i, waiting) in entry.queue.iter().enumerate() {
            let position = i as u32 + 1;
            waiting.state.send_replace(JobState::Queued { position });
            if let (true, Some(owner)) = (moved_up, &waiting.job.owner) {
                let rows = rows_with_state(&owner.rows, DownloadState::ImportQueued(position));
                match moved.iter_mut().find(|(user, _)| *user == owner.username) {
                    Some((_, user_rows)) => user_rows.extend(rows),
                    None => moved.push((owner.username.clone(), rows)),
                }
            }
        }
        Some((next.job, next.state, moved))
    }

    async fn execute(&self, target: &Path, job: &ImportJob) -> JobOutcome {
        let sources: Vec<&Path> = job.sources.iter().map(PathBuf::as_path).collect();
        if count_audio_files(&sources).await == 0 {
            return JobOutcome::of(ImportResult::nothing_to_import());
        }
        let importer = match self.deps.importer().await {
            Ok(importer) => importer,
            Err(e) => return JobOutcome::of(ImportResult::failed(e)),
        };
        if let Some(issue) = crate::importer_health::refusal(importer.as_ref()).await {
            return JobOutcome::of(ImportResult::refused(issue));
        }
        let before = match job.snapshot_target {
            true => Some(audio_files_under(target).await),
            false => None,
        };
        let result = importer.import(&sources, target, job.mode).await;
        let new_files = match before {
            Some(before) => {
                let mut new: Vec<PathBuf> = audio_files_under(target)
                    .await
                    .difference(&before)
                    .filter_map(|p| p.strip_prefix(target).ok().map(Path::to_path_buf))
                    .collect();
                new.sort();
                new
            }
            None => vec![],
        };
        JobOutcome { result, new_files }
    }

    async fn finish(&self, job: &ImportJob, outcome: JobOutcome, state: &watch::Sender<JobState>) {
        match &outcome.result.outcome {
            ImportOutcome::Imported { .. } => {
                info!(
                    "Imported {:?} into {}: {}",
                    job.sources,
                    job.target.display(),
                    outcome.result
                )
            }
            _ => warn!(
                "Import of {:?} into {}: {}",
                job.sources,
                job.target.display(),
                outcome.result
            ),
        }
        if let Some(root) = &job.cleanup_root {
            // beets moves the files out but leaves their folders behind.
            for source in &job.sources {
                let dir = match job.mode {
                    ImportMode::Album => Some(source.as_path()),
                    ImportMode::Singleton => source.parent(),
                };
                if let Some(dir) = dir {
                    let _ = crate::server_fns::cleanup_empty_ancestors(dir, root).await;
                }
            }
        }
        // Stored before the rows go out, so their Retry button always finds it.
        self.retries.record(job, &outcome.result);
        let _emit = self.emit_lock.lock().await;
        if let Some(owner) = &job.owner {
            let rows = rows_for_done(&owner.rows, job, &outcome);
            self.deps
                .emit(&owner.username, DownloadEvent::Progress(rows))
                .await;
        }
        state.send_replace(JobState::Done(outcome));
    }
}

/// Audio files under `dir`, recursively, without following symlinked folders.
async fn audio_files_under(dir: &Path) -> HashSet<PathBuf> {
    let mut files = HashSet::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(_) if is_audio_path(&path) => {
                    files.insert(path);
                }
                _ => {}
            }
        }
    }
    files
}

/// The key a library is queued under: `..`, relative paths and symlinks
/// would otherwise give one library several queues.
pub async fn canonical_target(target: &Path) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(target).await?;
    tokio::fs::canonicalize(target).await
}

/// Imports one file into a library folder and waits for it (promote).
pub async fn import_to_library(src: &Path, target: &Path) -> Result<(), String> {
    let outcome = IMPORT_QUEUE
        .submit(ImportJob {
            sources: vec![src.to_path_buf()],
            target: target.to_path_buf(),
            mode: ImportMode::Singleton,
            owner: None,
            snapshot_target: false,
            cleanup_root: None,
        })
        .await
        .wait()
        .await;
    match outcome.result.outcome {
        ImportOutcome::Imported { .. } => Ok(()),
        _ => Err(outcome.result.to_string()),
    }
}

struct LiveDeps;

#[async_trait]
impl QueueDeps for LiveDeps {
    async fn importer(&self) -> Result<Arc<dyn MusicImporter>, String> {
        crate::services::music_importer(None).await
    }

    async fn emit(&self, username: &str, event: DownloadEvent) {
        crate::globals::send_user_event(username, event).await;
    }
}

pub static IMPORT_QUEUE: LazyLock<ImportQueue> =
    LazyLock::new(|| ImportQueue::new(Arc::new(LiveDeps)));

#[cfg(test)]
mod tests {
    use super::test_support::{broken, row, RecordingDeps, StubImporter};
    use super::*;
    use shared::download::DownloadState;
    use soulbeet::{ImportOutcome, SkipReason};
    use std::time::Duration;

    pub(super) fn queue(
        stub: StubImporter,
    ) -> (ImportQueue, Arc<StubImporter>, Arc<RecordingDeps>) {
        let stub = Arc::new(stub);
        let deps = RecordingDeps::new(stub.clone());
        (ImportQueue::new(deps.clone()), stub, deps)
    }

    /// An empty file at `dir/name`, parents created.
    pub(super) fn file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        path
    }

    pub(super) fn job(source: PathBuf, target: &Path) -> ImportJob {
        ImportJob {
            sources: vec![source],
            target: target.to_path_buf(),
            mode: ImportMode::Singleton,
            owner: None,
            snapshot_target: false,
            cleanup_root: None,
        }
    }

    pub(super) fn owned(mut job: ImportJob, username: &str, items: &[&str]) -> ImportJob {
        job.owner = Some(JobOwner {
            username: username.into(),
            rows: items.iter().map(|i| row(i)).collect(),
        });
        job
    }

    pub(super) async fn finish(handle: ImportHandle) -> JobOutcome {
        tokio::time::timeout(Duration::from_secs(10), handle.wait())
            .await
            .expect("job never finished")
    }

    fn is_imported(outcome: &JobOutcome) -> bool {
        matches!(outcome.result.outcome, ImportOutcome::Imported { .. })
    }

    #[tokio::test]
    async fn jobs_on_one_target_run_one_at_a_time_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, _) = queue(StubImporter::new().with_delay(Duration::from_millis(30)));
        let sources: Vec<PathBuf> = (0..3)
            .map(|i| file(tmp.path(), &format!("dl/{i}.flac")))
            .collect();

        let mut handles = vec![];
        for source in &sources {
            handles.push(queue.submit(job(source.clone(), &lib)).await);
        }
        for handle in handles {
            assert!(is_imported(&finish(handle).await));
        }

        let order: Vec<PathBuf> = stub.seen().into_iter().map(|(s, _)| s[0].clone()).collect();
        assert_eq!(order, sources);
        assert_eq!(stub.max_active(), 1);
    }

    #[tokio::test]
    async fn jobs_on_two_targets_run_in_parallel() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _) = queue(StubImporter::new().held());

        let a = queue
            .submit(job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib1")))
            .await;
        let b = queue
            .submit(job(file(tmp.path(), "dl/b.flac"), &tmp.path().join("lib2")))
            .await;
        // Both imports are held inside the importer, so they can only meet
        // there if the two targets run at once.
        tokio::time::timeout(Duration::from_secs(10), async {
            while stub.active() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the second target never started while the first ran");
        stub.release(2);
        finish(a).await;
        finish(b).await;

        assert_eq!(stub.max_active(), 2);
    }

    #[tokio::test]
    async fn positions_are_sent_as_one_batched_progress_per_user() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, _, deps) = queue(StubImporter::new().with_delay(Duration::from_millis(100)));

        let slow = queue
            .submit(job(file(tmp.path(), "dl/slow.flac"), &lib))
            .await;
        let mut handles = vec![slow];
        for name in ["a", "b", "c"] {
            let source = file(tmp.path(), &format!("dl/{name}.flac"));
            handles.push(queue.submit(owned(job(source, &lib), "u", &[name])).await);
        }
        for handle in handles {
            finish(handle).await;
        }

        use DownloadState::{ImportQueued, Imported, Importing};
        let s = |item: &str, state: DownloadState| (item.to_string(), state);
        assert_eq!(
            deps.progress_for("u"),
            vec![
                vec![s("a", ImportQueued(1))],
                vec![s("b", ImportQueued(2))],
                vec![s("c", ImportQueued(3))],
                vec![s("a", Importing)],
                vec![s("b", ImportQueued(1)), s("c", ImportQueued(2))],
                vec![s("a", Imported)],
                vec![s("b", Importing)],
                vec![s("c", ImportQueued(1))],
                vec![s("b", Imported)],
                vec![s("c", Importing)],
                vec![s("c", Imported)],
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_submit_racing_the_idle_exit_is_not_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());

        for _ in 0..200 {
            let handle = queue.submit(job(source.clone(), &lib)).await;
            assert!(is_imported(&finish(handle).await));
        }
        assert_eq!(stub.calls(), 200);
    }

    #[tokio::test]
    async fn a_panicking_job_does_not_strand_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());
        stub.panic_next();

        let first = finish(queue.submit(job(source.clone(), &lib)).await).await;
        assert_eq!(
            first.result,
            ImportResult::failed("import worker stopped".into())
        );

        let second = finish(queue.submit(job(source, &lib)).await).await;
        assert!(is_imported(&second));
    }

    #[tokio::test]
    async fn a_source_without_audio_is_nothing_to_import() {
        let tmp = tempfile::tempdir().unwrap();
        let album = tmp.path().join("dl/X");
        file(&album, "cover.jpg");
        let (queue, stub, _) = queue(StubImporter::new());

        let mut album_job = job(album, &tmp.path().join("lib"));
        album_job.mode = ImportMode::Album;
        let outcome = finish(queue.submit(album_job).await).await;

        assert_eq!(
            outcome.result.outcome,
            ImportOutcome::Skipped(SkipReason::NothingToImport)
        );
        assert_eq!(stub.calls(), 0);
    }

    #[tokio::test]
    async fn a_broken_importer_is_refused_after_a_recheck() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _) = queue(StubImporter::new());
        stub.set_health(broken());

        let outcome = finish(
            queue
                .submit(job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")))
                .await,
        )
        .await;

        assert_eq!(
            outcome.result.outcome,
            ImportOutcome::Refused {
                issue: "No metadata source plugin loaded".into()
            }
        );
        assert_eq!(stub.calls(), 0);
    }

    #[tokio::test]
    async fn a_missing_target_is_created_then_canonicalized() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _) = queue(StubImporter::new());

        let target = tmp.path().join("a/../b");
        finish(
            queue
                .submit(job(file(tmp.path(), "dl/a.flac"), &target))
                .await,
        )
        .await;

        let canonical = std::fs::canonicalize(tmp.path().join("b")).unwrap();
        assert_eq!(stub.seen()[0].1, canonical);
        assert_eq!(canonical_target(&target).await.unwrap(), canonical);
    }

    #[tokio::test]
    async fn snapshot_returns_only_this_jobs_files_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        file(&lib, "old.flac");
        let (queue, _, _) = queue(
            StubImporter::new()
                .moving_files()
                .with_delay(Duration::from_millis(50)),
        );

        let other = queue.submit(job(file(tmp.path(), "dl/a.flac"), &lib)).await;
        let mut mine = job(file(tmp.path(), "dl/b.flac"), &lib);
        mine.snapshot_target = true;
        let mine = queue.submit(mine).await;
        finish(other).await;

        assert_eq!(finish(mine).await.new_files, vec![PathBuf::from("b.flac")]);
    }

    #[tokio::test]
    async fn cleanup_root_prunes_empty_source_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("dl");
        let (queue, _, _) = queue(StubImporter::new().moving_files());

        let mut pruned = job(file(&base, "X/a.flac"), &tmp.path().join("lib"));
        pruned.cleanup_root = Some(base.clone());
        finish(queue.submit(pruned).await).await;

        assert!(!base.join("X").exists());
        assert!(base.exists());
    }

    #[tokio::test]
    async fn a_closed_handle_reads_as_failed() {
        let (tx, rx) = watch::channel(JobState::Running);
        drop(tx);

        let outcome = ImportHandle { rx }.wait().await;

        assert_eq!(
            outcome.result,
            ImportResult::failed("import worker stopped".into())
        );
        assert!(outcome.new_files.is_empty());
    }
}
