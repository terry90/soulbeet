//! The download engine: one per process. It fetches files over HTTP into the backend's partial
//! folder, resumes a cut transfer with `Range` and `If-Range`, checks the md5 archive.org lists
//! and lands the file in its batch folder, never replacing one. Each transfer is in the transfer
//! book from the moment `download` answers for it, so a restart resumes it.

use super::client::{path_of, Heard, SERVICE};
use super::error::{ended, ArchiveError};
use crate::audio::is_audio_path;
use crate::partial::{land, PartialDir};
use crate::polite::{HostGate, Pace, RemoteError};
use crate::{BookEntry, TransferBook};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE, RETRY_AFTER};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use shared::archive::{ArchiveFile, BACKEND_ID};
use shared::download::{
    DownloadProgress, DownloadState, DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// An ended transfer's record is forgotten this long after it ended, when the engine starts.
const ENDED_KEPT_SECS: i64 = 7 * 24 * 3600;

/// What a speed is averaged over.
const SPEED_WINDOW: Duration = Duration::from_secs(10);

/// How hard the engine works archive.org, and how long it waits on it.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Transfers running from one item at once.
    pub per_item: usize,
    /// Transfers running at once.
    pub total: usize,
    /// Failed connections in a row, each without a byte, before a transfer fails.
    pub tries: u32,
    /// The wait after the first failed connection, doubled after each.
    pub backoff: Duration,
    /// 429 and 503 answers before a transfer fails.
    pub busy_tries: u32,
    /// The wait after the first 429 or 503 without `Retry-After`, doubled after each.
    pub busy_wait: Duration,
    /// A longer `Retry-After` fails the transfer rather than hold its slot.
    pub max_busy_wait: Duration,
    pub connect_timeout: Duration,
    /// How long a body may send nothing.
    pub read_timeout: Duration,
    /// The wait after a transfer book write failed, doubled after each up to a minute.
    pub book_wait: Duration,
    /// The space between two requests to archive.org, kept with its searches (`HostGate`).
    pub interval: Duration,
}

impl Limits {
    pub const ARCHIVE: Limits = Limits {
        per_item: 2,
        total: 4,
        tries: 5,
        backoff: Duration::from_secs(5),
        busy_tries: 5,
        busy_wait: Duration::from_secs(30),
        max_busy_wait: Duration::from_secs(600),
        connect_timeout: Duration::from_secs(15),
        read_timeout: Duration::from_secs(60),
        book_wait: Duration::from_secs(1),
        interval: Pace::ARCHIVE.min_interval,
    };
}

/// The longest wait between two tries of a transfer book write.
const BOOK_WAIT_MAX: Duration = Duration::from_secs(60);

/// What the engine needs of the server it runs in.
#[derive(Clone)]
pub struct EngineSettings {
    pub download_path: PathBuf,
    /// This install's id: its partial files are its own.
    pub instance: String,
    pub book: Arc<dyn TransferBook>,
    /// Where archive.org is reached, `client::base_url_from_env`.
    pub base_url: String,
}

/// Where a transfer stands, as the book keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stored {
    /// Not finished: a restart resumes it.
    Queued,
    /// Its bytes matched their md5 and it is being landed: a restart lands it, or finds it
    /// landed.
    Landing,
    Completed,
    Failed,
    Cancelled,
}

impl Stored {
    fn unfinished(self) -> bool {
        matches!(self, Stored::Queued | Stored::Landing)
    }
}

/// Whether the book entry is of a transfer that ended; one whose record cannot be read did
/// not.
pub(crate) fn finished(entry: &BookEntry) -> bool {
    serde_json::from_value::<Data>(entry.data.clone()).is_ok_and(|d| !d.state.unfinished())
}

/// What the book keeps of a transfer besides its entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Data {
    size: u64,
    md5: String,
    /// The version the partial bytes are of, for `If-Range`.
    etag: Option<String>,
    state: Stored,
    failure: Option<Failure>,
    /// Unix seconds.
    ended_at: Option<i64>,
}

impl Data {
    /// This data, ended now as `stored` with `failure`.
    fn ended(&self, stored: Stored, failure: Option<Failure>) -> Data {
        Data {
            state: stored,
            failure,
            ended_at: Some(now()),
            ..self.clone()
        }
    }

    fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// Where a transfer is in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Going in the book: not answered for, listed or started yet.
    Recording,
    Queued,
    Running,
    /// Being cancelled or stopped: its task is going away.
    Stopping,
    Ended,
}

/// The average speed of the last `SPEED_WINDOW`.
#[derive(Debug, Default)]
struct Speed(VecDeque<(Instant, u64)>);

impl Speed {
    fn record(&mut self, at: Instant, transferred: u64) {
        self.0.push_back((at, transferred));
        while self
            .0
            .front()
            .is_some_and(|(t, _)| at.duration_since(*t) > SPEED_WINDOW)
        {
            self.0.pop_front();
        }
    }

    fn bytes_per_sec(&self) -> f64 {
        match (self.0.front(), self.0.back()) {
            (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => {
                b1.saturating_sub(*b0) as f64 / t1.duration_since(*t0).as_secs_f64()
            }
            _ => 0.0,
        }
    }
}

struct Record {
    id: String,
    source: String,
    item: String,
    /// The file's name in its item.
    name: String,
    request: String,
    destination: String,
    created_at: i64,
    data: Data,
    phase: Phase,
    /// The batch owner, for sharing the slots fairly; kept in memory only, never logged.
    owner: Option<String>,
    transferred: u64,
    speed: Speed,
    attempts: u32,
    /// The order transfers came in.
    seq: u64,
    task: Option<JoinHandle<()>>,
}

impl Record {
    fn entry(&self) -> BookEntry {
        BookEntry {
            backend: BACKEND_ID.to_string(),
            key: self.id.clone(),
            transfer_id: Some(self.id.clone()),
            file_id: None,
            source: self.source.clone(),
            item: self.item.clone(),
            request: self.request.clone(),
            target: self.destination.clone(),
            data: self.data.value(),
            created_at: self.created_at,
        }
    }

    /// The record of a book entry, None for one this engine cannot read.
    fn from_entry(entry: BookEntry, seq: u64) -> Option<Record> {
        let data: Data = serde_json::from_value(entry.data).ok()?;
        let name = entry
            .item
            .strip_prefix(&format!("{}/", entry.source))?
            .to_string();
        Some(Record {
            id: entry.key,
            source: entry.source,
            item: entry.item,
            name,
            request: entry.request,
            destination: entry.target,
            created_at: entry.created_at,
            data,
            phase: Phase::Queued,
            owner: None,
            transferred: 0,
            speed: Speed::default(),
            attempts: 0,
            seq,
            task: None,
        })
    }
}

#[derive(Default)]
struct State {
    records: HashMap<String, Record>,
    next_seq: u64,
    /// Bumped by each stop: a handle built before it is refused.
    generation: u64,
}

impl State {
    fn running_from(&self, source: &str) -> usize {
        (self.records.values())
            .filter(|r| r.phase == Phase::Running && r.source == source)
            .count()
    }

    fn running(&self) -> usize {
        (self.records.values())
            .filter(|r| r.phase == Phase::Running)
            .count()
    }

    /// The queued transfer to start next, when one may: of the owner running the fewest, the
    /// one that came first, from an item running fewer than `per_item`.
    fn next_to_start(&self, per_item: usize) -> Option<String> {
        let mut by_owner: HashMap<Option<&str>, usize> = HashMap::new();
        for r in self.records.values().filter(|r| r.phase == Phase::Running) {
            *by_owner.entry(r.owner.as_deref()).or_default() += 1;
        }
        (self.records.values())
            .filter(|r| r.phase == Phase::Queued && self.running_from(&r.source) < per_item)
            .min_by_key(|r| {
                (
                    by_owner.get(&r.owner.as_deref()).copied().unwrap_or(0),
                    r.seq,
                )
            })
            .map(|r| r.id.clone())
    }

    /// A queued transfer's place in the line it waits in: behind its item's running files, or
    /// in the engine's own queue.
    fn place(&self, r: &Record, per_item: usize) -> (bool, u32) {
        let at_source = self.running_from(&r.source) >= per_item;
        let ahead = (self.records.values())
            .filter(|o| o.phase == Phase::Queued && o.seq < r.seq)
            .filter(|o| !at_source || o.source == r.source)
            .count();
        (at_source, ahead as u32 + 1)
    }
}

/// What a transfer task fetches.
#[derive(Debug, Clone)]
struct Job {
    id: String,
    name: String,
    source: String,
    destination: String,
    size: u64,
    md5: String,
    etag: Option<String>,
    landing: bool,
}

/// How one request of a transfer went.
enum Step {
    /// The bytes are all there and match their md5.
    Verified,
    /// The answer stopped early or never came: ask again from where the bytes end.
    Resume {
        progressed: bool,
        why: String,
    },
    /// archive.org asked to wait: ask again once the hold is over.
    Busy,
    /// The bytes cannot be used: throw them away and ask for the whole file.
    Restart(Mismatch),
    Failed(Failure),
}

/// What a transfer's requests ran into so far.
#[derive(Default)]
struct Tries {
    /// Failed connections in a row, each without progress.
    failed: u32,
    /// 429 and 503 answers.
    busy: u32,
    /// Times the bytes were thrown away and the whole file asked for again.
    restarts: u32,
}

/// Why a transfer's bytes were thrown away.
enum Mismatch {
    /// archive.org sent or names this many bytes, not the size it listed.
    Size(u64),
    /// The bytes' md5.
    Md5(String),
    /// archive.org answered with another range than the one asked, or refused it.
    Range(String),
}

impl Mismatch {
    /// The failure when the bytes came wrong a second time.
    fn failure(self, job: &Job) -> Failure {
        match self {
            Mismatch::Size(got) => ended::size(job.size, got),
            Mismatch::Md5(got) => ended::checksum(&job.md5, &got),
            Mismatch::Range(why) => ended::connection(&why),
        }
    }
}

/// Unix seconds now.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn plain_folder(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// The remote basename `resolve_in_destination` reads from an item id: its last segment.
fn basename(item: &str) -> &str {
    item.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(item)
}

/// The length of the file at `path`, 0 when there is none.
fn length_of(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The md5 of the first `length` bytes of `path`, read off the async runtime.
async fn md5_of(path: PathBuf) -> io::Result<md5::Context> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = md5::Context::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                return Ok(hasher);
            }
            hasher.consume(&buffer[..n]);
        }
    })
    .await
    .map_err(io::Error::other)?
}

/// A redirect policy that follows archive.org to its data nodes: the same host or one of its
/// subdomains, and never from https to http.
fn redirects(base: &str) -> reqwest::redirect::Policy {
    let base = url::Url::parse(base).ok();
    reqwest::redirect::Policy::custom(move |attempt| {
        let Some(base) = &base else {
            return attempt.stop();
        };
        let (Some(host), Some(base_host)) = (attempt.url().host_str(), base.host_str()) else {
            return attempt.stop();
        };
        let same = host == base_host || host.ends_with(&format!(".{base_host}"));
        let downgrade = base.scheme() == "https" && attempt.url().scheme() != "https";
        if attempt.previous().len() > 5 {
            attempt.error("too many redirects")
        } else if same && !downgrade {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// The Retry-After seconds of an answer, when it gives whole seconds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()
}

/// The start and total of a 206 answer's `Content-Range: bytes <start>-<end>/<total>`.
fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

pub(crate) struct Transfers {
    dir: PartialDir,
    download_path: PathBuf,
    book: Arc<dyn TransferBook>,
    http: reqwest::Client,
    base: String,
    limits: Limits,
    state: Mutex<State>,
    /// archive.org's pace and hold, which its searches keep too.
    gate: Arc<HostGate>,
    /// What archive.org last answered, which the health check reads.
    heard: Heard,
}

impl Transfers {
    /// The engine of `settings`: its partial folder opened first, then the book read. Every
    /// unfinished transfer is resumed, the ended ones older than 7 days are forgotten and every
    /// partial file no unfinished transfer has is deleted. When the folder cannot be opened
    /// (another server holds it, or it cannot be written) nothing is touched.
    pub(crate) async fn start(
        settings: EngineSettings,
        limits: Limits,
        heard: Heard,
    ) -> Result<Arc<Transfers>, ArchiveError> {
        let storage = |error| ArchiveError::Storage {
            download_path: settings.download_path.clone(),
            error,
        };
        let dir = PartialDir::open(&settings.download_path, &settings.instance, BACKEND_ID)
            .map_err(storage)?;
        let unavailable = |detail| {
            ArchiveError::Remote(RemoteError::Unavailable {
                service: SERVICE,
                detail,
            })
        };
        let gate = HostGate::of(&settings.base_url).map_err(unavailable)?;
        let http = reqwest::Client::builder()
            .user_agent(crate::polite::USER_AGENT)
            .connect_timeout(limits.connect_timeout)
            .read_timeout(limits.read_timeout)
            .redirect(redirects(&settings.base_url))
            .build()
            .map_err(|e| unavailable(e.without_url().to_string()))?;
        let entries = settings.book.all(BACKEND_ID).await?;

        let mut state = State::default();
        let mut keep = HashSet::new();
        for entry in entries {
            let key = entry.key.clone();
            let seq = state.next_seq;
            let Some(mut record) = Record::from_entry(entry, seq) else {
                warn!("Forgetting archive transfer {key}, whose record cannot be read");
                Self::forget_in(&*settings.book, &key).await;
                continue;
            };
            state.next_seq += 1;
            if record.data.state.unfinished() {
                record.transferred = length_of(&dir.part(&record.id));
                keep.insert(record.id.clone());
            } else if record
                .data
                .ended_at
                .is_none_or(|at| now() - at > ENDED_KEPT_SECS)
            {
                Self::forget_in(&*settings.book, &key).await;
                continue;
            } else {
                record.phase = Phase::Ended;
            }
            state.records.insert(record.id.clone(), record);
        }
        let clearing = dir.clone();
        let cleared = tokio::task::spawn_blocking(move || clearing.clear_except(&keep))
            .await
            .map_err(io::Error::other)
            .and_then(|r| r);
        cleared.map_err(storage)?;
        if !state.records.is_empty() {
            info!(
                "Internet Archive: {} transfers recorded, {} to resume",
                state.records.len(),
                (state.records.values())
                    .filter(|r| r.phase == Phase::Queued)
                    .count()
            );
        }

        let transfers = Arc::new(Transfers {
            dir,
            download_path: settings.download_path,
            book: settings.book,
            http,
            base: settings.base_url.trim_end_matches('/').to_string(),
            limits,
            state: Mutex::new(state),
            gate,
            heard,
        });
        transfers.pump();
        Ok(transfers)
    }

    async fn forget_in(book: &dyn TransferBook, key: &str) {
        if let Err(e) = book.forget(BACKEND_ID, key).await {
            warn!("Could not forget archive transfer {key}: {e}");
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    /// Runs `f` while `generation` is the current one, so no stop can come between the check
    /// and what `f` starts.
    pub(crate) fn while_current<T>(
        &self,
        generation: u64,
        f: impl FnOnce() -> Result<T, ArchiveError>,
    ) -> Result<T, ArchiveError> {
        let state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err(ArchiveError::TurnedOff);
        }
        f()
    }

    /// Starts the queued transfers that may start now.
    fn pump(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        while state.running() < self.limits.total {
            let Some(id) = state.next_to_start(self.limits.per_item) else {
                break;
            };
            let task = tokio::spawn(self.clone().run(id.clone()));
            if let Some(record) = state.records.get_mut(&id) {
                record.phase = Phase::Running;
                record.task = Some(task);
            }
        }
    }

    /// Enqueues `items` into `destination` for the handle of `generation`. Each one is in the
    /// book before it is answered for.
    pub(crate) async fn download(
        self: &Arc<Self>,
        generation: u64,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        owner: &str,
    ) -> Result<Vec<QueuedDownload>, ArchiveError> {
        if self.generation() != generation {
            return Err(ArchiveError::TurnedOff);
        }
        if !plain_folder(destination) {
            return Ok(vec![]);
        }
        let mut answers = Vec::new();
        for item in items.into_iter().filter(|i| i.backend == BACKEND_ID) {
            // The item comes back from the browser: only an audio file is ever fetched.
            let Some(file) = item
                .backend_data
                .as_deref()
                .and_then(|d| serde_json::from_str::<ArchiveFile>(d).ok())
                .filter(|f| {
                    f.identifier == item.source
                        && item.id == format!("{}/{}", f.identifier, f.name)
                        && is_audio_path(Path::new(&f.name))
                })
            else {
                continue;
            };
            let id = uuid::Uuid::new_v4().to_string();
            let record = Record {
                id: id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                name: file.name,
                request: request.to_string(),
                destination: destination.to_string(),
                created_at: now(),
                data: Data {
                    size: file.size,
                    md5: file.md5.to_ascii_lowercase(),
                    etag: None,
                    state: Stored::Queued,
                    failure: None,
                    ended_at: None,
                },
                phase: Phase::Recording,
                owner: Some(owner.to_string()),
                transferred: 0,
                speed: Speed::default(),
                attempts: 0,
                seq: 0,
                task: None,
            };
            let entry = record.entry();
            let answer = |status| QueuedDownload {
                id: item.id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                size: file.size,
                status,
            };
            // Held in memory before it goes in the book, so a call at the same time sees it
            // and nothing goes in the book that is already fetched or was turned off.
            {
                let mut state = self.state.lock().unwrap();
                if state.generation != generation {
                    return Err(ArchiveError::TurnedOff);
                }
                if state.records.values().any(|r| {
                    r.phase != Phase::Ended
                        && r.source == record.source
                        && r.item == record.item
                        && r.destination == record.destination
                }) {
                    answers.push(answer(EnqueueStatus::Failed(already_fetching())));
                    continue;
                }
                state.records.insert(id.clone(), record);
            }
            if let Err(e) = self.book.put(&entry).await {
                self.state.lock().unwrap().records.remove(&id);
                let failure = ArchiveError::Book(e).failure(
                    Stage::Enqueue,
                    "The Internet Archive transfer could not be recorded",
                );
                answers.push(answer(EnqueueStatus::Failed(failure)));
                continue;
            }
            let admitted = {
                let mut guard = self.state.lock().unwrap();
                let state = &mut *guard;
                let current = state.generation == generation;
                if let Some(record) = state.records.get_mut(&id).filter(|_| current) {
                    record.phase = Phase::Queued;
                    record.seq = state.next_seq;
                    state.next_seq += 1;
                    true
                } else {
                    state.records.remove(&id);
                    false
                }
            };
            // Turned off while it went in the book.
            if !admitted {
                Self::forget_in(&*self.book, &id).await;
                return Err(ArchiveError::TurnedOff);
            }
            answers.push(QueuedDownload {
                id,
                ..answer(EnqueueStatus::Enqueued)
            });
        }
        self.pump();
        Ok(answers)
    }

    /// The transfers from any of `sources`.
    pub(crate) fn listed(&self, sources: &[String]) -> Vec<DownloadProgress> {
        let state = self.state.lock().unwrap();
        (state.records.values())
            .filter(|r| r.phase != Phase::Recording && sources.contains(&r.source))
            .map(|r| self.progress(&state, r))
            .collect()
    }

    /// The record `id` of `source`, once `download` answered for it.
    fn answered<'a>(state: &'a State, source: &str, id: &str) -> Option<&'a Record> {
        (state.records.get(id)).filter(|r| r.phase != Phase::Recording && r.source == source)
    }

    pub(crate) fn one(&self, source: &str, id: &str) -> Option<DownloadProgress> {
        let state = self.state.lock().unwrap();
        Self::answered(&state, source, id).map(|r| self.progress(&state, r))
    }

    pub(crate) fn queue_position(&self, source: &str, id: &str) -> Option<u32> {
        let state = self.state.lock().unwrap();
        let record = Self::answered(&state, source, id)?;
        Some(match record.phase {
            Phase::Queued => state.place(record, self.limits.per_item).1,
            _ => 0,
        })
    }

    fn progress(&self, state: &State, r: &Record) -> DownloadProgress {
        let size = r.data.size;
        let mut row =
            DownloadProgress::queued(r.id.clone(), r.source.clone(), r.item.clone(), size)
                .with_backend(BACKEND_ID);
        row.request = Some(r.request.clone());
        row.attempts = r.attempts;
        row.transferred = r.transferred.min(size);
        row.percent = if size == 0 {
            0.0
        } else {
            row.transferred as f64 * 100.0 / size as f64
        };
        match r.phase {
            Phase::Recording | Phase::Queued => {
                let (at_source, place) = state.place(r, self.limits.per_item);
                row.queued_at_source = at_source;
                row.queue_position = Some(place);
            }
            Phase::Running => {
                row.state = DownloadState::InProgress;
                row.speed = r.speed.bytes_per_sec();
            }
            Phase::Stopping => row.state = DownloadState::Cancelled,
            Phase::Ended => match (r.data.state, &r.data.failure) {
                (Stored::Completed, _) => {
                    row.state = DownloadState::Completed;
                    row.transferred = size;
                    row.percent = 100.0;
                }
                (_, Some(failure)) => row = row.failed_with(failure.clone()),
                // Ended without finishing or failing: cancelled.
                (_, None) => row.state = DownloadState::Cancelled,
            },
        }
        row
    }

    /// Stops the transfer `id` of `source`, deletes its partial file and keeps it as
    /// cancelled, or with `remove` forgets it: an ended one too. A landed file is never touched.
    /// The book is written first: when it cannot be, the error is answered and the record stays
    /// as the book has it, for a later cancel or stop.
    pub(crate) async fn cancel(
        self: &Arc<Self>,
        source: &str,
        id: &str,
        remove: bool,
    ) -> Result<(), ArchiveError> {
        let task = {
            let mut state = self.state.lock().unwrap();
            let Some(record) = (state.records.get_mut(id))
                .filter(|r| r.phase != Phase::Recording && r.source == source)
            else {
                return Ok(());
            };
            if record.phase != Phase::Ended {
                record.phase = Phase::Stopping;
            }
            record.task.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        let cancelled = self.cancelled(id, remove).await;
        self.pump();
        cancelled
    }

    /// The rest of `cancel`, once the record's task is gone: its data is the book's.
    async fn cancelled(&self, id: &str, remove: bool) -> Result<(), ArchiveError> {
        let Some(data) = self.data_of(id) else {
            return Ok(());
        };
        if remove {
            self.book.forget(BACKEND_ID, id).await?;
            self.state.lock().unwrap().records.remove(id);
        } else if data.state.unfinished() {
            let data = data.ended(Stored::Cancelled, None);
            self.book.set_data(BACKEND_ID, id, &data.value()).await?;
            if let Some(record) = self.state.lock().unwrap().records.get_mut(id) {
                record.data = data;
                record.phase = Phase::Ended;
            }
        }
        if data.state.unfinished() {
            self.remove_part(id);
        }
        Ok(())
    }

    /// Stops every unfinished transfer: refuses older handles from now on, deletes the partial
    /// files and forgets the book entries. Ended transfers stay. An entry the book cannot forget
    /// keeps its record and its part, so the next stop forgets it.
    pub(crate) async fn stop(&self) {
        let tasks: Vec<(String, Option<JoinHandle<()>>)> = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            (state.records.values_mut())
                .filter(|r| r.phase != Phase::Ended)
                .map(|r| {
                    r.phase = Phase::Stopping;
                    (r.id.clone(), r.task.take())
                })
                .collect()
        };
        for (id, task) in tasks {
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
            }
            // One that ended, in the book, just before it was stopped stays.
            if !self.data_of(&id).is_some_and(|d| d.state.unfinished()) {
                continue;
            }
            match self.book.forget(BACKEND_ID, &id).await {
                Ok(()) => {
                    self.state.lock().unwrap().records.remove(&id);
                    self.remove_part(&id);
                }
                Err(e) => warn!("Could not forget archive transfer {id} on turning off: {e}"),
            }
        }
    }

    fn data_of(&self, id: &str) -> Option<Data> {
        Some(self.state.lock().unwrap().records.get(id)?.data.clone())
    }

    fn remove_part(&self, id: &str) {
        match std::fs::remove_file(self.dir.part(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!("Could not delete the partial file of archive transfer {id}: {e}"),
        }
    }

    /// Writes `data` to the book as the transfer `id`'s, trying again after a failed write until
    /// it is written or the task is stopped. A restart reads the book, so a state goes there
    /// before the record shows it.
    async fn write(&self, id: &str, data: &Data) {
        let value = data.value();
        let mut wait = self.limits.book_wait;
        while let Err(e) = self.book.set_data(BACKEND_ID, id, &value).await {
            warn!(
                "Could not record archive transfer {id}, trying again in {} s: {e}",
                wait.as_secs_f32()
            );
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(BOOK_WAIT_MAX);
        }
    }

    /// Ends the transfer `id` as `stored`, with `failure`: in the book, then in its record.
    async fn end(&self, id: &str, stored: Stored, failure: Option<Failure>) {
        let Some(data) = self.data_of(id) else {
            return;
        };
        let data = data.ended(stored, failure);
        self.write(id, &data).await;
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.records.get_mut(id) {
            record.data = data;
            record.phase = Phase::Ended;
            record.task = None;
            if stored == Stored::Completed {
                record.transferred = record.data.size;
            }
        }
    }

    fn job(&self, id: &str) -> Option<Job> {
        let state = self.state.lock().unwrap();
        let r = state.records.get(id)?;
        Some(Job {
            id: r.id.clone(),
            name: r.name.clone(),
            source: r.source.clone(),
            destination: r.destination.clone(),
            size: r.data.size,
            md5: r.data.md5.clone(),
            etag: r.data.etag.clone(),
            landing: r.data.state == Stored::Landing,
        })
    }

    /// One transfer's task: fetch, then end it as it went.
    async fn run(self: Arc<Self>, id: String) {
        let Some(job) = self.job(&id) else {
            return;
        };
        match self.fetch(&job).await {
            Ok(landed) => {
                info!(
                    "Internet Archive: {} landed at {}",
                    job.name,
                    landed.display()
                );
                self.end(&id, Stored::Completed, None).await;
            }
            Err(failure) => {
                warn!(
                    "Internet Archive transfer {id} of {} failed: {}",
                    job.name, failure.message
                );
                self.end(&id, Stored::Failed, Some(failure)).await;
                // Only once the book says failed, so a restart never fetches it from nothing.
                self.remove_part(&id);
            }
        }
        self.pump();
    }

    fn set_progress(&self, id: &str, transferred: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.records.get_mut(id) {
            record.transferred = transferred;
            record.speed.record(Instant::now(), transferred);
        }
    }

    fn count_attempt(&self, id: &str) {
        if let Some(record) = self.state.lock().unwrap().records.get_mut(id) {
            record.attempts += 1;
        }
    }

    /// Records the ETag the bytes from now on are of. A failed write is only logged: a restart
    /// then sends an older ETag, which brings the whole file again, or none, and the md5 catches
    /// a file that changed.
    async fn set_etag(&self, job: &mut Job, etag: Option<String>) {
        if job.etag == etag {
            return;
        }
        job.etag.clone_from(&etag);
        let data = {
            let mut state = self.state.lock().unwrap();
            let Some(record) = state.records.get_mut(&job.id) else {
                return;
            };
            record.data.etag = etag;
            record.data.value()
        };
        if let Err(e) = self.book.set_data(BACKEND_ID, &job.id, &data).await {
            warn!(
                "Could not record the ETag of archive transfer {}: {e}",
                job.id
            );
        }
    }

    /// Fetches the job's file and lands it: where it landed.
    async fn fetch(&self, job: &Job) -> Result<PathBuf, Failure> {
        let mut job = job.clone();
        // The part goes only once it is linked: the file landed, and is never fetched again.
        if job.landing && !self.dir.part(&job.id).exists() {
            return self
                .landed_before(&job)
                .await
                .ok_or_else(ended::landed_gone);
        }
        let mut tries = Tries::default();
        loop {
            match self.attempt(&mut job, &mut tries).await {
                Step::Verified => return self.land(&job).await,
                // A connection that brought bytes is asked again at once.
                Step::Resume {
                    progressed: true, ..
                } => tries.failed = 0,
                Step::Resume { why, .. } => {
                    tries.failed += 1;
                    if tries.failed >= self.limits.tries {
                        return Err(ended::connection(&why));
                    }
                    tokio::time::sleep(self.limits.backoff * 2u32.pow(tries.failed - 1)).await;
                }
                Step::Busy => {}
                Step::Restart(mismatch) => {
                    tries.restarts += 1;
                    if tries.restarts >= 2 {
                        return Err(mismatch.failure(&job));
                    }
                    self.remove_part(&job.id);
                    self.set_etag(&mut job, None).await;
                }
                Step::Failed(failure) => return Err(failure),
            }
        }
    }

    /// One request of the transfer, from where its partial bytes end.
    async fn attempt(&self, job: &mut Job, tries: &mut Tries) -> Step {
        let part = self.dir.part(&job.id);
        let mut have = length_of(&part);
        if have > job.size {
            return Step::Restart(Mismatch::Size(have));
        }
        let file = match self.dir.open_part(&job.id, have > 0) {
            Ok(file) => file,
            // A landing cut between its link and its removal: land again.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Step::Verified,
            Err(e) => return Step::Failed(ended::storage(&e, &self.download_path)),
        };
        let mut hasher = if have > 0 {
            match md5_of(part).await {
                Ok(hasher) => hasher,
                Err(e) => return Step::Failed(ended::storage(&e, &self.download_path)),
            }
        } else {
            md5::Context::new()
        };
        let mut file = tokio::fs::File::from_std(file);
        self.set_progress(&job.id, have);
        let before = have;
        let received = if have < job.size {
            self.receive(job, &mut file, &mut hasher, &mut have, tries)
                .await
        } else {
            None
        };
        let written = async {
            file.flush().await?;
            file.sync_all().await
        };
        if let Err(e) = written.await {
            return Step::Failed(ended::storage(&e, &self.download_path));
        }
        drop(file);
        if let Some(step) = received {
            return step;
        }
        if have < job.size {
            return Step::Resume {
                progressed: have > before,
                why: format!("the answer ended after {have} of {} bytes", job.size),
            };
        }
        let got = format!("{:x}", hasher.compute());
        if got != job.md5 {
            return Step::Restart(Mismatch::Md5(got));
        }
        Step::Verified
    }

    /// Asks for the bytes from `have` on and writes them to `file`: None when the answer was
    /// read to its end, else the step it calls for. `have` and `hasher` follow every byte
    /// written. A cut made progress when the part ends past where it ended before the request.
    async fn receive(
        &self,
        job: &mut Job,
        file: &mut tokio::fs::File,
        hasher: &mut md5::Context,
        have: &mut u64,
        tries: &mut Tries,
    ) -> Option<Step> {
        let before = *have;
        self.gate.wait_for_turn(self.limits.interval).await;
        let url = format!(
            "{}/download/{}/{}",
            self.base,
            path_of(&job.source),
            path_of(&job.name)
        );
        let mut request = self.http.get(&url);
        if *have > 0 {
            request = request.header(RANGE, format!("bytes={have}-"));
            // A weak ETag may not go in If-Range; the md5 then catches a file that changed.
            if let Some(etag) = job.etag.as_deref().filter(|e| !e.starts_with("W/")) {
                request = request.header(IF_RANGE, etag);
            }
        }
        self.count_attempt(&job.id);
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                let why = e.without_url().to_string();
                self.heard.note::<()>(&Err(RemoteError::Unavailable {
                    service: SERVICE,
                    detail: why.clone(),
                }));
                return Some(Step::Resume {
                    progressed: false,
                    why,
                });
            }
        };
        let status = response.status();
        self.heard.note(&match status {
            StatusCode::TOO_MANY_REQUESTS => Err(RemoteError::RateLimited { service: SERVICE }),
            StatusCode::SERVICE_UNAVAILABLE => Err(RemoteError::Unavailable {
                service: SERVICE,
                detail: "HTTP 503".into(),
            }),
            _ => Ok(()),
        });
        let headers = response.headers().clone();
        match status {
            StatusCode::PARTIAL_CONTENT => match content_range(&headers) {
                Some((_, total)) if total != job.size => {
                    return Some(Step::Restart(Mismatch::Size(total)))
                }
                Some((start, _)) if start == *have => {}
                _ => {
                    return Some(Step::Restart(Mismatch::Range(
                        "archive.org answered another range than the one asked".into(),
                    )))
                }
            },
            StatusCode::OK => {
                let length = headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
                if let Some(length) = length.filter(|l| *l != job.size) {
                    return Some(Step::Restart(Mismatch::Size(length)));
                }
                if *have > 0 {
                    // The file changed since its bytes were fetched, or the range was ignored:
                    // the whole file comes again, which counts as a restart.
                    tries.restarts += 1;
                    if tries.restarts >= 2 {
                        let why = "archive.org ignored the range asked".to_string();
                        return Some(Step::Failed(Mismatch::Range(why).failure(job)));
                    }
                    match self.dir.open_part(&job.id, false) {
                        Ok(fresh) => *file = tokio::fs::File::from_std(fresh),
                        Err(e) => {
                            return Some(Step::Failed(ended::storage(&e, &self.download_path)))
                        }
                    }
                    *hasher = md5::Context::new();
                    *have = 0;
                    self.set_progress(&job.id, 0);
                }
            }
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                tries.busy += 1;
                let asked = retry_after(&headers);
                let wait = asked.map_or_else(
                    || self.limits.busy_wait * 2u32.pow((tries.busy - 1).min(16)),
                    Duration::from_secs,
                );
                if tries.busy > self.limits.busy_tries || wait > self.limits.max_busy_wait {
                    return Some(Step::Failed(ended::busy(status.as_u16(), asked)));
                }
                // Every transfer and search waits it out, not only this one.
                self.gate.hold(wait);
                info!(
                    "archive.org answered {} for {}; waiting {} s",
                    status.as_u16(),
                    job.name,
                    wait.as_secs()
                );
                return Some(Step::Busy);
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Some(Step::Restart(Mismatch::Range(
                    "archive.org refused the range asked".into(),
                )))
            }
            s if s.is_server_error() => {
                return Some(Step::Resume {
                    progressed: false,
                    why: format!("HTTP {}", s.as_u16()),
                })
            }
            s => return Some(Step::Failed(ended::refused(s.as_u16()))),
        }
        let etag = headers
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.set_etag(job, etag).await;

        let mut response = response;
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let after = *have + bytes.len() as u64;
                    if after > job.size {
                        return Some(Step::Restart(Mismatch::Size(after)));
                    }
                    if let Err(e) = file.write_all(&bytes).await {
                        return Some(Step::Failed(ended::storage(&e, &self.download_path)));
                    }
                    hasher.consume(&bytes);
                    *have = after;
                    self.set_progress(&job.id, after);
                }
                Ok(None) => return None,
                Err(e) => {
                    let why = e.without_url().to_string();
                    warn!("archive.org cut the transfer of {}: {why}", job.name);
                    return Some(Step::Resume {
                        progressed: *have > before,
                        why,
                    });
                }
            }
        }
    }

    /// Lands the verified part of `job` in its batch folder, once the book says so.
    async fn land(&self, job: &Job) -> Result<PathBuf, Failure> {
        if let Some(mut data) = self.data_of(&job.id) {
            data.state = Stored::Landing;
            self.write(&job.id, &data).await;
            if let Some(record) = self.state.lock().unwrap().records.get_mut(&job.id) {
                record.data = data;
            }
        }
        land(
            &self.dir.part(&job.id),
            &self.download_path,
            &job.destination,
            basename(&format!("{}/{}", job.source, job.name)),
        )
        .map_err(|e| ended::storage(&e, &self.download_path))
    }

    /// The file a landing whose part is gone put in the batch folder: one under one of the
    /// landing names with the job's size and md5.
    async fn landed_before(&self, job: &Job) -> Option<PathBuf> {
        let folder = self.download_path.join(&job.destination);
        let remote = basename(&format!("{}/{}", job.source, job.name)).to_string();
        let names = shared::download::landing_names(&remote);
        let entries = std::fs::read_dir(&folder).ok()?;
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let named = names.contains(&name)
                || names
                    .iter()
                    .any(|n| shared::download::is_ticks_copy(&name, n));
            if !named || entry.metadata().map_or(true, |m| m.len() != job.size) {
                continue;
            }
            let Ok(hasher) = md5_of(entry.path()).await else {
                continue;
            };
            if format!("{:x}", hasher.compute()) == job.md5 {
                return Some(entry.path());
            }
        }
        None
    }
}

/// The same file is already fetched into the same folder.
fn already_fetching() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        "The Internet Archive backend is already fetching this file into this folder",
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_http::{serve_file, serve_with_headers, FileAnswer, Reply, ServedFile};
    use crate::BookError;
    use async_trait::async_trait;
    use shared::download::FileFacts;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A transfer book in memory, as the SQLite one behaves. While `.1` is set it refuses
    /// every write but `put`, as a database that cannot be written.
    #[derive(Default)]
    pub(crate) struct MemoryBook(pub Mutex<Vec<BookEntry>>, pub AtomicBool);

    impl MemoryBook {
        fn writable(&self) -> Result<(), BookError> {
            match self.1.load(Ordering::SeqCst) {
                true => Err(BookError("the database is locked".into())),
                false => Ok(()),
            }
        }
    }

    #[async_trait]
    impl TransferBook for MemoryBook {
        async fn put(&self, entry: &BookEntry) -> Result<(), BookError> {
            let mut entries = self.0.lock().unwrap();
            entries.retain(|e| !(e.backend == entry.backend && e.key == entry.key));
            entries.push(entry.clone());
            Ok(())
        }
        async fn bind(
            &self,
            backend: &str,
            key: &str,
            transfer_id: &str,
            file_id: Option<&str>,
        ) -> Result<(), BookError> {
            self.writable()?;
            let mut entries = self.0.lock().unwrap();
            let entry = (entries.iter_mut())
                .find(|e| e.backend == backend && e.key == key)
                .ok_or_else(|| BookError(format!("no entry {backend}/{key}")))?;
            entry.transfer_id = Some(transfer_id.to_string());
            entry.file_id = file_id.map(str::to_string);
            Ok(())
        }
        async fn set_data(
            &self,
            backend: &str,
            key: &str,
            data: &serde_json::Value,
        ) -> Result<(), BookError> {
            self.writable()?;
            let mut entries = self.0.lock().unwrap();
            let entry = (entries.iter_mut())
                .find(|e| e.backend == backend && e.key == key)
                .ok_or_else(|| BookError(format!("no entry {backend}/{key}")))?;
            entry.data = data.clone();
            Ok(())
        }
        async fn by_sources(
            &self,
            backend: &str,
            sources: &[String],
        ) -> Result<Vec<BookEntry>, BookError> {
            Ok(self
                .all(backend)
                .await?
                .into_iter()
                .filter(|e| sources.contains(&e.source))
                .collect())
        }
        async fn by_transfer(
            &self,
            backend: &str,
            transfer_id: &str,
        ) -> Result<Option<BookEntry>, BookError> {
            Ok(self
                .all(backend)
                .await?
                .into_iter()
                .find(|e| e.transfer_id.as_deref() == Some(transfer_id)))
        }
        async fn all(&self, backend: &str) -> Result<Vec<BookEntry>, BookError> {
            let mut entries: Vec<BookEntry> = (self.0.lock().unwrap().iter())
                .filter(|e| e.backend == backend)
                .cloned()
                .collect();
            entries.sort_by(|a, b| (a.created_at, &a.key).cmp(&(b.created_at, &b.key)));
            Ok(entries)
        }
        async fn forget(&self, backend: &str, key: &str) -> Result<(), BookError> {
            self.writable()?;
            (self.0.lock().unwrap()).retain(|e| !(e.backend == backend && e.key == key));
            Ok(())
        }
    }

    impl MemoryBook {
        pub(crate) fn states(&self) -> Vec<(String, String)> {
            (self.0.lock().unwrap().iter())
                .map(|e| {
                    (
                        e.key.clone(),
                        e.data["state"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        }
    }

    pub(crate) fn fast() -> Limits {
        Limits {
            backoff: Duration::from_millis(10),
            busy_wait: Duration::from_millis(10),
            read_timeout: Duration::from_secs(30),
            book_wait: Duration::from_millis(10),
            interval: Duration::ZERO,
            ..Limits::ARCHIVE
        }
    }

    pub(crate) fn body(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    pub(crate) fn md5_hex(bytes: &[u8]) -> String {
        format!("{:x}", md5::compute(bytes))
    }

    /// An item of a search for the file `name` of `identifier`, whose listed size and md5 are
    /// those of `listed`.
    pub(crate) fn item(identifier: &str, name: &str, listed: &[u8]) -> DownloadableItem {
        let data = ArchiveFile {
            identifier: identifier.into(),
            name: name.into(),
            size: listed.len() as u64,
            md5: md5_hex(listed),
        };
        DownloadableItem {
            id: format!("{identifier}/{name}"),
            backend: BACKEND_ID.into(),
            source: identifier.into(),
            title: name.into(),
            artist: "Duster".into(),
            album: "Stratosphere".into(),
            size: Some(listed.len() as u64),
            duration: None,
            quality: "flac".into(),
            quality_score: 1.0,
            backend_data: Some(serde_json::to_string(&data).unwrap()),
            match_score: 1.0,
            below_floor: false,
            facts: FileFacts::default(),
        }
    }

    pub(crate) struct Rig {
        pub root: tempfile::TempDir,
        pub instance: String,
        pub book: Arc<MemoryBook>,
    }

    impl Rig {
        pub(crate) fn new() -> Rig {
            Rig {
                root: tempfile::tempdir().unwrap(),
                instance: uuid::Uuid::new_v4().to_string(),
                book: Arc::new(MemoryBook::default()),
            }
        }

        pub(crate) fn settings(&self, base: &str) -> EngineSettings {
            EngineSettings {
                download_path: self.root.path().to_path_buf(),
                instance: self.instance.clone(),
                book: self.book.clone(),
                base_url: base.to_string(),
            }
        }

        async fn start(&self, base: &str, limits: Limits) -> Arc<Transfers> {
            Transfers::start(self.settings(base), limits, Heard::default())
                .await
                .unwrap()
        }

        pub(crate) fn part_dir(&self) -> PathBuf {
            self.root
                .path()
                .join(crate::partial::PARTIAL_FOLDER)
                .join(&self.instance)
                .join(BACKEND_ID)
        }

        pub(crate) fn parts(&self) -> Vec<String> {
            std::fs::read_dir(self.part_dir())
                .map(|dir| {
                    dir.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn folder(&self, destination: &str) -> PathBuf {
            self.root.path().join(destination)
        }

        /// An unfinished transfer a previous run recorded, with `part` as its bytes so far.
        fn recorded(&self, id: &str, listed: &[u8], etag: Option<&str>, state: &str, part: &[u8]) {
            let entry = BookEntry {
                backend: BACKEND_ID.into(),
                key: id.into(),
                transfer_id: Some(id.into()),
                file_id: None,
                source: "duster".into(),
                item: "duster/01 Moon Age.flac".into(),
                request: "r-before".into(),
                target: "Stratosphere [aaaa1111]".into(),
                data: serde_json::json!({
                    "size": listed.len(), "md5": md5_hex(listed), "etag": etag,
                    "state": state, "failure": null, "ended_at": null
                }),
                created_at: now() - 60,
            };
            self.book.0.lock().unwrap().push(entry);
            std::fs::create_dir_all(self.part_dir()).unwrap();
            if !part.is_empty() {
                std::fs::write(self.part_dir().join(id), part).unwrap();
            }
        }
    }

    /// The transfer `id` of `source` once `until` holds of it.
    pub(crate) async fn wait_for(
        transfers: &Transfers,
        source: &str,
        id: &str,
        until: impl Fn(&DownloadProgress) -> bool,
    ) -> DownloadProgress {
        for _ in 0..1000 {
            if let Some(row) = transfers.one(source, id).filter(|r| until(r)) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "transfer {id} never got there: {:?}",
            transfers.one(source, id)
        );
    }

    fn ended(row: &DownloadProgress) -> bool {
        matches!(
            row.state,
            DownloadState::Completed | DownloadState::Failed(_) | DownloadState::Cancelled
        )
    }

    async fn sent(
        transfers: &Arc<Transfers>,
        item: DownloadableItem,
        destination: &str,
        owner: &str,
    ) -> String {
        let generation = transfers.generation();
        let queued = transfers
            .download(generation, vec![item], destination, "r1", owner)
            .await
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, EnqueueStatus::Enqueued, "{queued:?}");
        queued[0].id.clone()
    }

    const DESTINATION: &str = "Stratosphere [aaaa1111]";

    #[tokio::test]
    async fn a_file_lands_checked_and_stays_listed_until_removed() {
        let rig = Rig::new();
        let file = body(5000, 1);
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"v1\"".into()),
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "[1998] Stratosphere/01 Moon Age.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;

        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(
            (
                row.backend.as_deref(),
                row.request.as_deref(),
                row.transferred,
                row.attempts
            ),
            (Some("archive"), Some("r1"), 5000, 1)
        );
        assert_eq!(row.item, "duster/[1998] Stratosphere/01 Moon Age.flac");
        let landed = rig.folder(DESTINATION).join("01 Moon Age.flac");
        assert_eq!(std::fs::read(&landed).unwrap(), file);
        assert!(rig.parts().is_empty(), "{:?}", rig.parts());
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/download/duster/%5B1998%5D%20Stratosphere/01%20Moon%20Age.flac"
        );
        assert_eq!(rig.book.states(), [(id.clone(), "completed".to_string())]);
        assert_eq!(transfers.queue_position("duster", &id), Some(0));

        transfers.cancel("duster", &id, true).await.unwrap();
        assert!(transfers.one("duster", &id).is_none());
        assert!(rig.book.states().is_empty());
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            file,
            "remove never touches a landed file"
        );
        assert_eq!(transfers.queue_position("duster", &id), None);
    }

    #[tokio::test]
    async fn a_cut_transfer_resumes_from_where_its_bytes_end() {
        let rig = Rig::new();
        let file = body(6000, 2);
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"v1\"".into()),
            },
            vec![FileAnswer::Cut(1500), FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01 Moon Age.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].range.as_deref(), Some("bytes=1500-"));
        assert_eq!(seen[1].if_range.as_deref(), Some("\"v1\""));
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            file
        );
    }

    #[tokio::test]
    async fn a_restart_resumes_an_unfinished_transfer_and_clears_what_no_transfer_has() {
        let rig = Rig::new();
        let file = body(4000, 3);
        rig.recorded("t-before", &file, Some("\"v1\""), "queued", &file[..1000]);
        std::fs::write(rig.part_dir().join("stray"), b"left by a crash").unwrap();
        // An ended transfer older than seven days.
        rig.book.0.lock().unwrap().push(BookEntry {
            backend: BACKEND_ID.into(),
            key: "t-old".into(),
            transfer_id: Some("t-old".into()),
            file_id: None,
            source: "duster".into(),
            item: "duster/02 Heading for the Door.flac".into(),
            request: "r-old".into(),
            target: DESTINATION.into(),
            data: serde_json::json!({"size": 1, "md5": md5_hex(b"x"), "etag": null,
                "state": "completed", "failure": null, "ended_at": now() - 8 * 24 * 3600}),
            created_at: now() - 9 * 24 * 3600,
        });
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"v1\"".into()),
            },
            vec![FileAnswer::Whole],
        );

        let transfers = rig.start(&base, fast()).await;

        assert!(!rig.parts().contains(&"stray".to_string()));
        let row = wait_for(&transfers, "duster", "t-before", ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(row.request.as_deref(), Some("r-before"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].range.as_deref(), Some("bytes=1000-"));
        assert_eq!(seen[0].if_range.as_deref(), Some("\"v1\""));
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            file
        );
        assert_eq!(
            rig.book.states(),
            [("t-before".to_string(), "completed".to_string())]
        );
    }

    #[tokio::test]
    async fn a_file_that_changed_since_its_bytes_came_is_fetched_again_whole() {
        let rig = Rig::new();
        let file = body(3000, 4);
        let old = body(3000, 99);
        rig.recorded("t-changed", &file, Some("\"v0\""), "queued", &old[..1200]);
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"v1\"".into()),
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let row = wait_for(&transfers, "duster", "t-changed", ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(seen.lock().unwrap()[0].if_range.as_deref(), Some("\"v0\""));
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            file
        );
    }

    #[tokio::test]
    async fn bytes_that_do_not_match_their_md5_twice_never_land() {
        let rig = Rig::new();
        let listed = body(2000, 5);
        let sent_bytes = body(2000, 6);
        let (base, seen) = serve_file(
            ServedFile {
                body: sent_bytes,
                etag: None,
            },
            vec![FileAnswer::Whole, FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01 Moon Age.flac", &listed),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        let failure = row.failure.unwrap();
        assert_eq!(
            (failure.code, failure.retry),
            (
                FailureCode::TransferChecksumMismatch,
                RetryAdvice::OtherSource
            )
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "asked once more from the start"
        );
        assert!(seen.lock().unwrap()[1].range.is_none());
        assert!(!rig.folder(DESTINATION).exists());
        assert!(rig.parts().is_empty());
        assert_eq!(rig.book.states(), [(id, "failed".to_string())]);
    }

    #[tokio::test]
    async fn a_file_longer_than_its_size_never_lands() {
        let rig = Rig::new();
        let listed = body(1000, 7);
        let (base, _) = serve_file(
            ServedFile {
                body: body(1200, 7),
                etag: None,
            },
            vec![FileAnswer::Whole, FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01 Moon Age.flac", &listed),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(row.failure.unwrap().code, FailureCode::TransferSizeMismatch);
        assert!(!rig.folder(DESTINATION).exists());
    }

    #[tokio::test]
    async fn a_refused_or_missing_file_fails_with_archive_orgs_answer() {
        for (status, message) in [
            (403, "archive.org refused this file (restricted)"),
            (404, "archive.org no longer has this file"),
        ] {
            let rig = Rig::new();
            let file = body(100, 8);
            let (base, _) = serve_file(
                ServedFile {
                    body: file.clone(),
                    etag: None,
                },
                vec![FileAnswer::Status(status)],
            );
            let transfers = rig.start(&base, fast()).await;
            let id = sent(
                &transfers,
                item("duster", "01 Moon Age.flac", &file),
                DESTINATION,
                "u1",
            )
            .await;
            let row = wait_for(&transfers, "duster", &id, ended).await;
            let failure = row.failure.unwrap();
            assert_eq!(
                (failure.code, failure.message.as_str()),
                (FailureCode::TransferRejectedNotShared, message)
            );
            assert_eq!(row.error.as_deref(), Some(message));
        }
    }

    #[tokio::test]
    async fn a_busy_archive_is_waited_out_as_it_asks_then_followed_to_its_data_node() {
        let rig = Rig::new();
        let file = body(2500, 9);
        let (node, node_seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"n1\"".into()),
            },
            vec![FileAnswer::Whole],
        );
        let empty = |status| Reply {
            status,
            body: String::new(),
        };
        let (base, seen) = serve_with_headers(vec![
            (empty(429), vec![("Retry-After", "1".into())]),
            (empty(503), vec![]),
            (
                empty(302),
                vec![("Location", format!("{node}/0/items/duster/01.flac"))],
            ),
        ]);
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01 Moon Age.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[1].at.duration_since(seen[0].at) >= Duration::from_secs(1),
            "Retry-After was waited out"
        );
        assert_eq!(
            node_seen.lock().unwrap()[0].target,
            "/0/items/duster/01.flac"
        );
        assert_eq!(row.attempts, 3);
    }

    #[tokio::test]
    async fn an_archive_busy_too_long_fails_the_transfer_with_its_wait() {
        let rig = Rig::new();
        let file = body(100, 10);
        let empty = |status| Reply {
            status,
            body: String::new(),
        };
        let (base, _) =
            serve_with_headers(vec![(empty(429), vec![("Retry-After", "7200".into())])]);
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let failure = wait_for(&transfers, "duster", &id, ended)
            .await
            .failure
            .unwrap();
        assert_eq!(
            (failure.code, failure.retry, failure.retry_after_seconds),
            (
                FailureCode::TransferRejectedPeerBusy,
                RetryAdvice::SameSourceLater,
                Some(7200)
            )
        );

        let (base, seen) = serve_with_headers((0..3).map(|_| (empty(503), vec![])).collect());
        let limits = Limits {
            busy_tries: 2,
            ..fast()
        };
        let rig = Rig::new();
        let transfers = rig.start(&base, limits).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let failure = wait_for(&transfers, "duster", &id, ended)
            .await
            .failure
            .unwrap();
        assert_eq!(failure.code, FailureCode::TransferRejectedPeerBusy);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_connection_that_keeps_failing_ends_the_transfer() {
        let rig = Rig::new();
        let file = body(100, 11);
        // Nothing listens on port 1 of loopback.
        let transfers = rig.start("http://127.0.0.1:1", fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(
            row.failure.unwrap().code,
            FailureCode::TransferConnectionFailed
        );
        assert_eq!(row.attempts, fast().tries);
    }

    #[tokio::test]
    async fn a_name_taken_in_the_folder_lands_as_a_copy_and_the_file_there_stays() {
        let rig = Rig::new();
        let file = body(800, 12);
        std::fs::create_dir_all(rig.folder(DESTINATION)).unwrap();
        std::fs::write(
            rig.folder(DESTINATION).join("01 Moon Age.flac"),
            b"someone's",
        )
        .unwrap();
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01 Moon Age.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        assert_eq!(
            wait_for(&transfers, "duster", &id, ended).await.state,
            DownloadState::Completed
        );
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            b"someone's"
        );
        let copies: Vec<PathBuf> = std::fs::read_dir(rig.folder(DESTINATION))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                shared::download::is_ticks_copy(
                    p.file_name().unwrap().to_str().unwrap(),
                    "01 Moon Age.flac",
                )
            })
            .collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(std::fs::read(&copies[0]).unwrap(), file);
    }

    #[tokio::test]
    async fn a_folder_that_cannot_be_written_fails_the_transfer_and_keeps_no_part() {
        let rig = Rig::new();
        let file = body(300, 13);
        // A file where the batch folder would be.
        std::fs::write(rig.folder(DESTINATION), b"in the way").unwrap();
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let failure = wait_for(&transfers, "duster", &id, ended)
            .await
            .failure
            .unwrap();
        assert_eq!(
            (failure.code, failure.user_action.map(|a| a.code)),
            (
                FailureCode::TransferAborted,
                Some(shared::failure::UserActionCode::FixDownloadPath)
            )
        );
        assert!(rig.parts().is_empty());
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION)).unwrap(),
            b"in the way"
        );
    }

    #[tokio::test]
    async fn a_landing_cut_short_is_finished_at_restart_without_asking_again() {
        let rig = Rig::new();
        let file = body(900, 14);
        // Landed, its book entry not yet completed.
        rig.recorded("t-landed", &file, None, "landing", &[]);
        std::fs::create_dir_all(rig.folder(DESTINATION)).unwrap();
        std::fs::write(rig.folder(DESTINATION).join("01 Moon Age.flac"), &file).unwrap();
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![],
        );
        let transfers = rig.start(&base, fast()).await;
        let row = wait_for(&transfers, "duster", "t-landed", ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read_dir(rig.folder(DESTINATION)).unwrap().count(),
            1
        );

        // Linked, its part not yet removed.
        let rig = Rig::new();
        rig.recorded("t-linked", &file, None, "landing", &file);
        std::fs::create_dir_all(rig.folder(DESTINATION)).unwrap();
        std::fs::hard_link(
            rig.part_dir().join("t-linked"),
            rig.folder(DESTINATION).join("01 Moon Age.flac"),
        )
        .unwrap();
        let transfers = rig.start(&base, fast()).await;
        let row = wait_for(&transfers, "duster", "t-linked", ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert!(rig.parts().is_empty());
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            file
        );

        // Landed, its file taken away since, by an import or by hand.
        let rig = Rig::new();
        rig.recorded("t-gone", &file, None, "landing", &[]);
        let transfers = rig.start(&base, fast()).await;
        let row = wait_for(&transfers, "duster", "t-gone", ended).await;
        let failure = row.failure.unwrap();
        assert_eq!(
            (failure.code, failure.retry),
            (
                FailureCode::FileNotFoundAfterDownload,
                RetryAdvice::NotRetryable
            )
        );
        assert!(seen.lock().unwrap().is_empty(), "never fetched again");
        assert!(!rig.folder(DESTINATION).exists());
        assert_eq!(
            rig.book.states(),
            [("t-gone".to_string(), "failed".to_string())]
        );
    }

    #[tokio::test]
    async fn a_cancel_deletes_only_the_part_and_a_remove_forgets_the_record() {
        let rig = Rig::new();
        let file = body(4000, 15);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Stall(1000)],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        wait_for(&transfers, "duster", &id, |r| r.transferred >= 1000).await;
        assert_eq!(rig.parts(), [id.as_str()]);

        transfers.cancel("another-item", &id, false).await.unwrap();
        assert_eq!(
            rig.parts(),
            [id.as_str()],
            "a cancel names the transfer's own source"
        );
        transfers.cancel("duster", &id, false).await.unwrap();
        assert_eq!(
            transfers.one("duster", &id).unwrap().state,
            DownloadState::Cancelled
        );
        assert!(rig.parts().is_empty());
        assert_eq!(rig.book.states(), [(id.clone(), "cancelled".to_string())]);

        transfers.cancel("duster", &id, true).await.unwrap();
        assert!(transfers.one("duster", &id).is_none());
        assert!(rig.book.states().is_empty());
        transfers
            .cancel("duster", "never-sent", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_third_file_of_one_item_waits_behind_the_two_it_sends() {
        let rig = Rig::new();
        let file = body(4000, 16);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Stall(10), FileAnswer::Stall(10)],
        );
        let transfers = rig.start(&base, fast()).await;
        let mut ids = Vec::new();
        for name in ["01.flac", "02.flac", "03.flac"] {
            ids.push(sent(&transfers, item("duster", name, &file), DESTINATION, "u1").await);
        }
        for id in &ids[..2] {
            wait_for(&transfers, "duster", id, |r| {
                r.state == DownloadState::InProgress
            })
            .await;
        }
        let third = transfers.one("duster", &ids[2]).unwrap();
        assert_eq!(
            (third.state, third.queued_at_source, third.queue_position),
            (DownloadState::Queued, true, Some(1))
        );
        assert_eq!(transfers.queue_position("duster", &ids[2]), Some(1));
        assert_eq!(transfers.queue_position("duster", &ids[0]), Some(0));
        let listed = transfers.listed(&["duster".to_string()]);
        assert_eq!(listed.len(), 3);
        assert!(transfers.listed(&["other".to_string()]).is_empty());
        transfers.stop().await;
    }

    #[tokio::test]
    async fn turning_off_mid_transfer_leaves_no_part_and_no_unfinished_entry() {
        let rig = Rig::new();
        let file = body(4000, 17);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole, FileAnswer::Stall(1500)],
        );
        let limits = Limits {
            per_item: 1,
            ..fast()
        };
        let transfers = rig.start(&base, limits).await;
        let finished = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        wait_for(&transfers, "duster", &finished, ended).await;
        let running = sent(
            &transfers,
            item("duster", "02.flac", &file),
            DESTINATION,
            "u2",
        )
        .await;
        wait_for(&transfers, "duster", &running, |r| r.transferred >= 1500).await;
        let waiting = sent(
            &transfers,
            item("duster", "03.flac", &file),
            DESTINATION,
            "u2",
        )
        .await;
        assert_eq!(
            transfers.one("duster", &waiting).unwrap().state,
            DownloadState::Queued
        );
        let before = transfers.generation();

        transfers.stop().await;

        assert!(rig.parts().is_empty(), "{:?}", rig.parts());
        assert_eq!(
            rig.book.states(),
            [(finished.clone(), "completed".to_string())]
        );
        assert!(transfers.one("duster", &running).is_none());
        assert!(transfers.one("duster", &waiting).is_none());
        assert_eq!(
            transfers.one("duster", &finished).unwrap().state,
            DownloadState::Completed
        );
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01.flac")).unwrap(),
            file
        );
        // A handle built before the stop is refused from now on.
        let refused = transfers
            .download(
                before,
                vec![item("duster", "04.flac", &file)],
                DESTINATION,
                "r3",
                "u1",
            )
            .await;
        assert!(
            matches!(refused, Err(ArchiveError::TurnedOff)),
            "{refused:?}"
        );
        assert!(matches!(
            transfers.while_current(before, || Ok(())),
            Err(ArchiveError::TurnedOff)
        ));
        assert!(transfers
            .while_current(transfers.generation(), || Ok(()))
            .is_ok());
        assert_eq!(rig.book.states().len(), 1);
    }

    #[tokio::test]
    async fn two_users_downloads_land_each_in_its_own_folder_under_its_own_request() {
        let rig = Rig::new();
        let file = body(1500, 19);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole, FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let generation = transfers.generation();
        let mut ids = Vec::new();
        for (owner, destination, request, name) in [
            ("u1", "Theirs [aaaa1111]", "r-u1", "01.flac"),
            ("u2", "Mine [bbbb2222]", "r-u2", "02.flac"),
        ] {
            let queued = transfers
                .download(
                    generation,
                    vec![item("duster", name, &file)],
                    destination,
                    request,
                    owner,
                )
                .await
                .unwrap();
            ids.push((queued[0].id.clone(), destination, request, name));
        }
        for (id, destination, request, name) in &ids {
            let row = wait_for(&transfers, "duster", id, ended).await;
            assert_eq!(row.state, DownloadState::Completed);
            assert_eq!(row.request.as_deref(), Some(*request));
            let names: Vec<String> = std::fs::read_dir(rig.folder(destination))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, [name.to_string()]);
        }
    }

    #[tokio::test]
    async fn a_state_the_book_cannot_take_is_not_shown_until_it_does() {
        let rig = Rig::new();
        let file = body(3000, 20);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        rig.book.1.store(true, Ordering::SeqCst);
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        wait_for(&transfers, "duster", &id, |r| r.transferred == 3000).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row = transfers.one("duster", &id).unwrap();
        assert_eq!(row.state, DownloadState::InProgress, "{row:?}");
        assert!(
            !rig.folder(DESTINATION).exists(),
            "nothing lands before the book says it is landing"
        );
        assert_eq!(rig.book.states(), [(id.clone(), "queued".to_string())]);

        rig.book.1.store(false, Ordering::SeqCst);
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(rig.book.states(), [(id, "completed".to_string())]);
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01.flac")).unwrap(),
            file
        );
    }

    #[tokio::test]
    async fn a_cancel_or_stop_the_book_refuses_keeps_the_transfer_for_the_next_one() {
        let rig = Rig::new();
        let file = body(4000, 21);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Stall(1000)],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        wait_for(&transfers, "duster", &id, |r| r.transferred >= 1000).await;
        rig.book.1.store(true, Ordering::SeqCst);

        assert!(transfers.cancel("duster", &id, true).await.is_err());
        assert!(transfers.cancel("duster", &id, false).await.is_err());
        transfers.stop().await;
        assert_eq!(
            transfers.one("duster", &id).unwrap().state,
            DownloadState::Cancelled
        );
        assert_eq!(rig.parts(), [id.as_str()]);
        assert_eq!(rig.book.states(), [(id.clone(), "queued".to_string())]);

        rig.book.1.store(false, Ordering::SeqCst);
        transfers.stop().await;
        assert!(transfers.one("duster", &id).is_none());
        assert!(rig.parts().is_empty());
        assert!(rig.book.states().is_empty());
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_fails_the_transfer_after_one_restart() {
        let rig = Rig::new();
        let file = body(6000, 22);
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("\"v1\"".into()),
            },
            vec![FileAnswer::NoRange(1500); 4],
        );
        let transfers = rig.start(&base, fast()).await;
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let row = wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(
            row.failure.unwrap().code,
            FailureCode::TransferConnectionFailed
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[1].range.as_deref(), Some("bytes=1500-"));
        assert!(!rig.folder(DESTINATION).exists());
        assert!(rig.parts().is_empty());
    }

    #[tokio::test]
    async fn a_weak_etag_is_never_sent_in_if_range() {
        let rig = Rig::new();
        let file = body(3000, 23);
        rig.recorded("t-weak", &file, Some("W/\"v1\""), "queued", &file[..1000]);
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: Some("W/\"v1\"".into()),
            },
            vec![FileAnswer::Whole],
        );
        let transfers = rig.start(&base, fast()).await;
        let row = wait_for(&transfers, "duster", "t-weak", ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(
            (seen[0].range.as_deref(), seen[0].if_range.as_deref()),
            (Some("bytes=1000-"), None)
        );
        assert_eq!(
            std::fs::read(rig.folder(DESTINATION).join("01 Moon Age.flac")).unwrap(),
            file
        );
    }

    #[tokio::test]
    async fn a_file_already_fetched_into_its_folder_or_not_audio_is_never_recorded() {
        let rig = Rig::new();
        let file = body(2000, 24);
        let (base, _) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Stall(500)],
        );
        let transfers = rig.start(&base, fast()).await;
        let first = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        let again = transfers
            .download(
                transfers.generation(),
                vec![
                    item("duster", "01.flac", &file),
                    item("duster", "disc image.iso", &file),
                ],
                DESTINATION,
                "r2",
                "u1",
            )
            .await
            .unwrap();
        assert_eq!(again.len(), 1, "the iso gets no answer: {again:?}");
        let EnqueueStatus::Failed(failure) = &again[0].status else {
            panic!("{again:?}");
        };
        assert_eq!(failure.code, FailureCode::EnqueueFailed);
        assert_eq!(rig.book.states(), [(first, "queued".to_string())]);
        transfers.stop().await;
    }

    #[tokio::test]
    async fn what_archive_org_answers_a_download_is_heard() {
        let rig = Rig::new();
        let empty = |status| Reply {
            status,
            body: String::new(),
        };
        let (base, _) =
            serve_with_headers(vec![(empty(429), vec![("Retry-After", "7200".into())])]);
        let heard = Heard::default();
        let transfers = Transfers::start(rig.settings(&base), fast(), heard.clone())
            .await
            .unwrap();
        let file = body(100, 25);
        let id = sent(
            &transfers,
            item("duster", "01.flac", &file),
            DESTINATION,
            "u1",
        )
        .await;
        wait_for(&transfers, "duster", &id, ended).await;
        assert_eq!(
            heard.within(Duration::from_secs(60)),
            Some(Err(RemoteError::RateLimited { service: SERVICE }))
        );
    }

    #[test]
    fn a_free_slot_goes_to_the_owner_running_the_fewest() {
        let record = |id: &str, owner: &str, source: &str, phase, seq| Record {
            id: id.into(),
            source: source.into(),
            item: format!("{source}/{id}.flac"),
            name: format!("{id}.flac"),
            request: "r".into(),
            destination: "D".into(),
            created_at: 0,
            data: Data {
                size: 1,
                md5: md5_hex(b"x"),
                etag: None,
                state: Stored::Queued,
                failure: None,
                ended_at: None,
            },
            phase,
            owner: Some(owner.into()),
            transferred: 0,
            speed: Speed::default(),
            attempts: 0,
            seq,
            task: None,
        };
        let mut state = State::default();
        for r in [
            record("a1", "alice", "x", Phase::Running, 0),
            record("a2", "alice", "x", Phase::Queued, 1),
            record("a3", "alice", "y", Phase::Queued, 2),
            record("b1", "bob", "y", Phase::Queued, 3),
        ] {
            state.records.insert(r.id.clone(), r);
        }
        assert_eq!(state.next_to_start(2).as_deref(), Some("b1"));
        state.records.get_mut("b1").unwrap().phase = Phase::Running;
        // Even again, the earliest goes first.
        assert_eq!(state.next_to_start(2).as_deref(), Some("a2"));
        // Items x and y each send their one file already.
        assert_eq!(state.next_to_start(1), None);
        state.records.get_mut("b1").unwrap().phase = Phase::Queued;
        state.records.get_mut("a1").unwrap().phase = Phase::Ended;
        assert_eq!(state.next_to_start(1).as_deref(), Some("a2"));
    }

    #[test]
    fn a_book_entry_of_an_ended_transfer_is_finished() {
        let entry = |data: serde_json::Value| BookEntry {
            backend: BACKEND_ID.into(),
            key: "k".into(),
            transfer_id: None,
            file_id: None,
            source: "s".into(),
            item: "s/f".into(),
            request: "r".into(),
            target: "t".into(),
            data,
            created_at: 0,
        };
        let data = |state: &str| {
            serde_json::json!({"size": 1, "md5": "m", "etag": null, "state": state,
                "failure": null, "ended_at": null})
        };
        for (state, done) in [
            ("queued", false),
            ("landing", false),
            ("completed", true),
            ("failed", true),
            ("cancelled", true),
        ] {
            assert_eq!(finished(&entry(data(state))), done, "{state}");
        }
        assert!(!finished(&entry(serde_json::json!({}))));
    }
}
