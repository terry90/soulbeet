//! A scripted importer and recording deps for the queue's tests.

use super::{ImportQueue, QueueDeps};
use crate::auth::UserId;
use crate::core::delete_guard::test_support::FakeNavidrome;
use crate::core::delete_guard::Guard;
use crate::ledger::{JobEvent, Ledger};
use async_trait::async_trait;
use shared::download::{
    DownloadEvent, DownloadProgress, DownloadState, MatchCandidate, MatchChoices, MatchKind,
};
use shared::system::{HealthIssue, HealthLevel, ImporterHealth};
use soulbeet::{
    BeetsTrackInfo, DriverChoice, DuplicateRun, ForgetOutcome, ImportMode, ImportOutcome,
    ImportResult, ImportedItem, MusicImporter, PruneOutcome, RegisterOutcome, Relocation,
    SkipReason, TagEdits, TaskCount,
};
use sqlx::SqlitePool;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One file imported, as beets reports it.
pub fn imported() -> ImportResult {
    ImportResult {
        outcome: ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![ImportedItem {
                path: "Zzqx Artist/a.flac".into(),
                recording_mbid: None,
                artist: "Zzqx Artist".into(),
                title: "a".into(),
                album: "Qzz Album".into(),
                beets_id: None,
                release_mbid: None,
                match_per_mille: None,
            }],
            failure: None,
        },
        output_tail: "imported tail".into(),
        tasks: TaskCount {
            imported: 1,
            total: 1,
        },
        choices: None,
        user_action: None,
        removals: vec![],
        files: vec![],
    }
}

/// A skipped single-task run that found `candidates`, with no choices.
pub fn no_match(candidates: u32) -> ImportResult {
    ImportResult {
        outcome: ImportOutcome::Skipped(SkipReason::NoMatch {
            candidates: Some(candidates),
        }),
        output_tail: "skipped tail".into(),
        tasks: TaskCount {
            imported: 0,
            total: 1,
        },
        choices: None,
        user_action: None,
        removals: vec![],
        files: vec![],
    }
}

pub fn candidate(data_source: &str, id: &str) -> MatchCandidate {
    MatchCandidate {
        data_source: data_source.into(),
        id: id.into(),
        artist: "Artist".into(),
        title: "Title".into(),
        album: None,
        year: None,
        country: None,
        disambig: None,
        per_mille: 500,
        differs: vec![],
        length_secs: None,
        tracks: None,
    }
}

pub fn track_choices(candidates: Vec<MatchCandidate>) -> MatchChoices {
    MatchChoices {
        kind: MatchKind::Track,
        candidates,
    }
}

pub fn broken() -> ImporterHealth {
    ImporterHealth {
        level: HealthLevel::Broken,
        issues: vec![HealthIssue {
            severity: HealthLevel::Broken,
            title: "No metadata source plugin loaded".into(),
            detail: String::new(),
        }],
        facts: vec![],
    }
}

pub fn ready() -> ImporterHealth {
    ImporterHealth {
        level: HealthLevel::Ready,
        ..Default::default()
    }
}

/// A finished download row for `item`.
pub fn row(item: &str) -> DownloadProgress {
    DownloadProgress {
        state: DownloadState::Completed,
        ..DownloadProgress::queued(item.into(), "peer".into(), item.into(), 1)
    }
}

/// Answers imports from a script (Imported once it runs out), forgets with
/// one answer (Removed unless scripted), and relocates with one answer (that
/// it moved nothing unless scripted).
pub struct StubImporter {
    results: Mutex<VecDeque<ImportResult>>,
    delay: Duration,
    /// When set, each import waits here for a permit.
    gate: Option<tokio::sync::Semaphore>,
    moves_files: bool,
    panic_next: AtomicBool,
    health: Mutex<ImporterHealth>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    seen: Mutex<Vec<(Vec<PathBuf>, PathBuf)>>,
    choices: Mutex<Vec<DriverChoice>>,
    duplicate_runs: Mutex<Vec<DuplicateRun>>,
    forget_answer: Mutex<ForgetOutcome>,
    forgets: Mutex<Vec<(PathBuf, PathBuf)>>,
    prune_answer: Mutex<PruneOutcome>,
    prunes: Mutex<Vec<PathBuf>>,
    relocations: Mutex<Vec<(PathBuf, Vec<u64>)>>,
    relocate_answer: Mutex<Relocation>,
    registers: Mutex<Vec<(PathBuf, PathBuf)>>,
    register_answer: Mutex<RegisterOutcome>,
    inspect_answer: Mutex<Result<Option<BeetsTrackInfo>, String>>,
    tag_edits: Mutex<Vec<(PathBuf, PathBuf, TagEdits)>>,
    tag_edit_answer: Mutex<Option<Result<BeetsTrackInfo, shared::failure::Failure>>>,
    rematches: Mutex<Vec<(PathBuf, Vec<PathBuf>, bool)>>,
    relocate_files: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl StubImporter {
    pub fn new() -> Self {
        StubImporter {
            results: Mutex::new(VecDeque::new()),
            delay: Duration::ZERO,
            gate: None,
            moves_files: false,
            panic_next: AtomicBool::new(false),
            health: Mutex::new(ready()),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            choices: Mutex::new(vec![]),
            duplicate_runs: Mutex::new(vec![]),
            forget_answer: Mutex::new(ForgetOutcome::Removed),
            forgets: Mutex::new(vec![]),
            prune_answer: Mutex::new(PruneOutcome::Pruned {
                removed: 0,
                kept: 0,
            }),
            prunes: Mutex::new(vec![]),
            relocations: Mutex::new(vec![]),
            relocate_answer: Mutex::new(Relocation {
                now: vec![],
                failure: None,
            }),
            registers: Mutex::new(vec![]),
            register_answer: Mutex::new(RegisterOutcome::Registered),
            inspect_answer: Mutex::new(Ok(None)),
            tag_edits: Mutex::new(vec![]),
            tag_edit_answer: Mutex::new(None),
            rematches: Mutex::new(vec![]),
            relocate_files: Mutex::new(vec![]),
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Holds every import inside the importer until `release` lets it go.
    pub fn held(mut self) -> Self {
        self.gate = Some(tokio::sync::Semaphore::new(0));
        self
    }

    pub fn release(&self, imports: usize) {
        self.gate.as_ref().expect("not held").add_permits(imports);
    }

    /// Imports inside the importer right now.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Moves each source file (or each file directly inside a source
    /// directory) into the target, the way beets does with `move: yes`.
    pub fn moving_files(mut self) -> Self {
        self.moves_files = true;
        self
    }

    pub fn script(&self, result: ImportResult) {
        self.results.lock().unwrap().push_back(result);
    }

    pub fn script_forget(&self, outcome: ForgetOutcome) {
        *self.forget_answer.lock().unwrap() = outcome;
    }

    pub fn script_prune(&self, outcome: PruneOutcome) {
        *self.prune_answer.lock().unwrap() = outcome;
    }

    pub fn script_relocate(&self, answer: Relocation) {
        *self.relocate_answer.lock().unwrap() = answer;
    }

    #[allow(dead_code)]
    pub fn script_register(&self, answer: RegisterOutcome) {
        *self.register_answer.lock().unwrap() = answer;
    }

    pub fn script_inspect(&self, answer: Result<Option<BeetsTrackInfo>, String>) {
        *self.inspect_answer.lock().unwrap() = answer;
    }

    #[allow(dead_code)]
    pub fn script_modify_tags(&self, answer: Result<BeetsTrackInfo, shared::failure::Failure>) {
        *self.tag_edit_answer.lock().unwrap() = Some(answer);
    }

    pub fn registers(&self) -> Vec<(PathBuf, PathBuf)> {
        self.registers.lock().unwrap().clone()
    }

    pub fn tag_edits(&self) -> Vec<(PathBuf, PathBuf, TagEdits)> {
        self.tag_edits.lock().unwrap().clone()
    }

    pub fn rematches(&self) -> Vec<(PathBuf, Vec<PathBuf>, bool)> {
        self.rematches.lock().unwrap().clone()
    }

    pub fn relocate_files(&self) -> Vec<(PathBuf, PathBuf)> {
        self.relocate_files.lock().unwrap().clone()
    }

    pub fn panic_next(&self) {
        self.panic_next.store(true, Ordering::SeqCst);
    }

    pub fn set_health(&self, health: ImporterHealth) {
        *self.health.lock().unwrap() = health;
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    /// Sources and target of every import, in call order.
    pub fn seen(&self) -> Vec<(Vec<PathBuf>, PathBuf)> {
        self.seen.lock().unwrap().clone()
    }

    /// Library and file of every forget, in call order.
    pub fn forgets(&self) -> Vec<(PathBuf, PathBuf)> {
        self.forgets.lock().unwrap().clone()
    }

    /// The forgets once there are `n` of them, which a spawned task queues; panics after 10 s.
    pub async fn forgets_once(&self, n: usize) -> Vec<(PathBuf, PathBuf)> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.forgets.lock().unwrap().len() < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the forgets never came");
        self.forgets()
    }

    /// The library of every prune, in call order.
    pub fn prunes(&self) -> Vec<PathBuf> {
        self.prunes.lock().unwrap().clone()
    }

    /// The choice of every import, in call order.
    pub fn choices(&self) -> Vec<DriverChoice> {
        self.choices.lock().unwrap().clone()
    }

    /// The duplicate handling of every import, in call order.
    pub fn duplicate_runs(&self) -> Vec<DuplicateRun> {
        self.duplicate_runs.lock().unwrap().clone()
    }

    /// Library and item ids of every relocate, in call order.
    pub fn relocations(&self) -> Vec<(PathBuf, Vec<u64>)> {
        self.relocations.lock().unwrap().clone()
    }

    fn move_into(sources: &[&Path], target: &Path) {
        for source in sources {
            let files = if source.is_dir() {
                std::fs::read_dir(source)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .filter(|p| p.is_file())
                    .collect()
            } else {
                vec![source.to_path_buf()]
            };
            for file in files {
                std::fs::rename(&file, target.join(file.file_name().unwrap())).unwrap();
            }
        }
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl MusicImporter for StubImporter {
    async fn import(
        &self,
        sources: &[&Path],
        target: &Path,
        _: ImportMode,
        choice: &DriverChoice,
        duplicates: DuplicateRun,
    ) -> ImportResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.choices.lock().unwrap().push(choice.clone());
        self.duplicate_runs.lock().unwrap().push(duplicates);
        self.seen.lock().unwrap().push((
            sources.iter().map(|s| s.to_path_buf()).collect(),
            target.to_path_buf(),
        ));
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveGuard(&self.active);
        self.max_active.fetch_max(now, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.panic_next.swap(false, Ordering::SeqCst) {
            panic!("stub importer panicked");
        }
        if self.moves_files {
            Self::move_into(sources, target);
        }
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(imported)
    }

    async fn relocate(&self, library: &Path, item_ids: &[u64]) -> Relocation {
        self.relocations
            .lock()
            .unwrap()
            .push((library.to_path_buf(), item_ids.to_vec()));
        self.relocate_answer.lock().unwrap().clone()
    }

    async fn relocate_file(&self, library: &Path, file: &Path) -> Relocation {
        self.relocate_files
            .lock()
            .unwrap()
            .push((library.to_path_buf(), file.to_path_buf()));
        self.relocate_answer.lock().unwrap().clone()
    }

    fn refused(&self, issue: String) -> ImportResult {
        ImportResult::refused(issue)
    }

    async fn forget(&self, library: &Path, file: &Path) -> ForgetOutcome {
        self.forgets
            .lock()
            .unwrap()
            .push((library.to_path_buf(), file.to_path_buf()));
        self.forget_answer.lock().unwrap().clone()
    }

    async fn register_in_place(&self, library: &Path, file: &Path) -> RegisterOutcome {
        self.registers
            .lock()
            .unwrap()
            .push((library.to_path_buf(), file.to_path_buf()));
        self.register_answer.lock().unwrap().clone()
    }

    async fn inspect_file(
        &self,
        _library: &Path,
        _file: &Path,
    ) -> Result<Option<BeetsTrackInfo>, String> {
        self.inspect_answer.lock().unwrap().clone()
    }

    async fn modify_tags(
        &self,
        library: &Path,
        file: &Path,
        edits: &TagEdits,
    ) -> Result<BeetsTrackInfo, shared::failure::Failure> {
        self.tag_edits
            .lock()
            .unwrap()
            .push((library.to_path_buf(), file.to_path_buf(), edits.clone()));
        if let Some(answer) = self.tag_edit_answer.lock().unwrap().clone() {
            return answer;
        }
        Ok(BeetsTrackInfo {
            beets_id: 1,
            path: file.to_path_buf(),
            title: edits.title.clone().unwrap_or_else(|| "O Holy Night".into()),
            artist: edits.artist.clone().unwrap_or_else(|| "Marrow Choir".into()),
            album: edits.album.clone().unwrap_or_else(|| "Winter Songs".into()),
            album_artist: edits
                .album_artist
                .clone()
                .unwrap_or_else(|| "Marrow Choir".into()),
            year: edits.year.or(Some(2019)),
            track: edits.track.or(Some(3)),
            disc: edits.disc.or(Some(1)),
            genre: edits.genre.clone().unwrap_or_default(),
            comments: edits.comment.clone().unwrap_or_default(),
            recording_mbid: None,
            release_mbid: None,
        })
    }

    async fn rematch_in_place(
        &self,
        sources: &[&Path],
        library: &Path,
        _mode: ImportMode,
        _choice: &DriverChoice,
        pretend: bool,
    ) -> ImportResult {
        self.rematches.lock().unwrap().push((
            library.to_path_buf(),
            sources.iter().map(|s| s.to_path_buf()).collect(),
            pretend,
        ));
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(imported)
    }

    async fn prune(&self, library: &Path) -> PruneOutcome {
        self.prunes.lock().unwrap().push(library.to_path_buf());
        self.prune_answer.lock().unwrap().clone()
    }

    fn health(&self) -> ImporterHealth {
        self.health.lock().unwrap().clone()
    }

    async fn refresh_health(&self) -> ImporterHealth {
        self.health()
    }

    async fn health_stale(&self) -> bool {
        false
    }
}

/// Hands out one importer and, when given one, a delete guard; records every event per user id
/// and every ledger event, and hands the ledger events to a ledger when given one.
pub struct RecordingDeps {
    importer: Arc<StubImporter>,
    guard: Option<(SqlitePool, Arc<FakeNavidrome>)>,
    events: Mutex<Vec<(UserId, DownloadEvent)>>,
    ledger: Mutex<Vec<JobEvent>>,
    into: Option<Ledger>,
}

impl RecordingDeps {
    /// Deps whose queue never settles a removal: `guard` panics.
    pub fn new(importer: Arc<StubImporter>) -> Arc<Self> {
        Self::build(importer, None)
    }

    /// Deps whose guard checks against `db` and `fake`, with no music path set.
    pub fn with_guard(
        importer: Arc<StubImporter>,
        db: SqlitePool,
        fake: Arc<FakeNavidrome>,
    ) -> Arc<Self> {
        Self::build(importer, Some((db, fake)))
    }

    /// Deps whose ledger events also reach `ledger`, as the server's reach its ledger.
    pub fn into_ledger(importer: Arc<StubImporter>, ledger: Ledger) -> Arc<Self> {
        Arc::new(RecordingDeps {
            into: Some(ledger),
            ..Self::unwrapped(importer, None)
        })
    }

    fn build(
        importer: Arc<StubImporter>,
        guard: Option<(SqlitePool, Arc<FakeNavidrome>)>,
    ) -> Arc<Self> {
        Arc::new(Self::unwrapped(importer, guard))
    }

    fn unwrapped(
        importer: Arc<StubImporter>,
        guard: Option<(SqlitePool, Arc<FakeNavidrome>)>,
    ) -> Self {
        RecordingDeps {
            importer,
            guard,
            events: Mutex::new(vec![]),
            ledger: Mutex::new(vec![]),
            into: None,
        }
    }

    /// What the queue recorded in the ledger, in order.
    pub fn ledger_events(&self) -> Vec<JobEvent> {
        self.ledger.lock().unwrap().clone()
    }

    /// Each Progress event sent to `user`, as (item, state) pairs.
    pub fn progress_for(&self, user: &UserId) -> Vec<Vec<(String, DownloadState)>> {
        self.rows_for(user)
            .into_iter()
            .map(|rows| rows.into_iter().map(|r| (r.item, r.state)).collect())
            .collect()
    }

    pub fn rows_for(&self, user: &UserId) -> Vec<Vec<DownloadProgress>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(owner, _)| owner == user)
            .filter_map(|(_, event)| match event {
                DownloadEvent::Progress(rows) => Some(rows.clone()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl QueueDeps for RecordingDeps {
    async fn importer(&self) -> Result<Arc<dyn MusicImporter>, String> {
        Ok(self.importer.clone())
    }

    async fn emit(&self, user: &UserId, event: DownloadEvent) {
        self.events.lock().unwrap().push((user.clone(), event));
    }

    fn record(&self, event: JobEvent) {
        if let Some(ledger) = &self.into {
            ledger.record(event.clone());
        }
        self.ledger.lock().unwrap().push(event);
    }

    fn guard(&self) -> Guard<'_> {
        let (db, fake) = self
            .guard
            .as_ref()
            .expect("this test gives the import queue no delete guard");
        Guard::new(db, fake.as_ref(), None)
    }
}

/// A queue on a new StubImporter, whose removals no guard settles, and that importer.
pub fn stub_queue() -> (ImportQueue, Arc<StubImporter>) {
    let stub = Arc::new(StubImporter::new());
    (ImportQueue::new(RecordingDeps::new(stub.clone())), stub)
}

/// Two users whose ids differ from any display name they could have.
pub fn alice() -> UserId {
    UserId::new("id-alice")
}

pub fn bob() -> UserId {
    UserId::new("id-bob")
}
