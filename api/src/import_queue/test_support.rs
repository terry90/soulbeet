//! A scripted importer and recording deps for the queue's tests.

use super::QueueDeps;
use async_trait::async_trait;
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use shared::library::DuplicateReport;
use shared::system::{HealthIssue, HealthLevel, ImporterHealth};
use soulbeet::{ImportMode, ImportOutcome, ImportResult, MusicImporter, TaskCount};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn imported() -> ImportResult {
    ImportResult {
        outcome: ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
        },
        output_tail: "imported tail".into(),
        tasks: TaskCount {
            imported: 1,
            total: 1,
        },
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

/// Answers imports from a script (Imported once it runs out).
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
    fn id(&self) -> &'static str {
        "stub"
    }

    fn name(&self) -> &'static str {
        "Stub"
    }

    async fn import(&self, sources: &[&Path], target: &Path, _: ImportMode) -> ImportResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
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

    async fn find_duplicates(&self, _: &[&Path]) -> Result<DuplicateReport, String> {
        unreachable!()
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

/// Hands out one importer and records every event per username.
pub struct RecordingDeps {
    importer: Arc<StubImporter>,
    events: Mutex<Vec<(String, DownloadEvent)>>,
}

impl RecordingDeps {
    pub fn new(importer: Arc<StubImporter>) -> Arc<Self> {
        Arc::new(RecordingDeps {
            importer,
            events: Mutex::new(vec![]),
        })
    }

    /// Each Progress event sent to `username`, as (item, state) pairs.
    pub fn progress_for(&self, username: &str) -> Vec<Vec<(String, DownloadState)>> {
        self.rows_for(username)
            .into_iter()
            .map(|rows| rows.into_iter().map(|r| (r.item, r.state)).collect())
            .collect()
    }

    pub fn rows_for(&self, username: &str) -> Vec<Vec<DownloadProgress>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(user, _)| user == username)
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

    async fn emit(&self, username: &str, event: DownloadEvent) {
        self.events
            .lock()
            .unwrap()
            .push((username.to_string(), event));
    }
}
