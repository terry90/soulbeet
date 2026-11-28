//! The download engine: one per process, its transfers in memory. It fetches each file from
//! NetEase's servers into the backend's partial folder, resumes a cut answer with `Range`,
//! checks the bytes against the size GD Studio gave and the MD5 the file is named by, tags the
//! file with the wanted track and lands it in its batch folder, never replacing one.

use super::api::{Api, Located, Refusal, Resolved};
use super::audio_header::{self, Header, HEAD_BYTES};
use super::error::{ended, GdError};
use super::Limits;
use crate::partial::{land, PartialDir};
use crate::tags::{write_tags, MusicBrainzIds, TagError, WantedTags};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::gdstudio::{GdTrack, BACKEND_ID, SOURCE};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// An ended transfer's record is forgotten this long after it ended.
const ENDED_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// What a speed is averaged over.
const SPEED_WINDOW: Duration = Duration::from_secs(5);

/// How a transfer ended.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Completed,
    Failed(Failure),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Waiting for a worker.
    Queued,
    /// A worker has it: it waits for its URL, then moves bytes.
    Running,
    /// Being cancelled or stopped: its task is going away.
    Stopping,
    Ended(Outcome, Instant),
}

/// The average speed of the last `SPEED_WINDOW`.
#[derive(Debug, Default)]
struct Speed(VecDeque<(Instant, u64)>);

impl Speed {
    fn record(&mut self, at: Instant, transferred: u64) {
        self.0.push_back((at, transferred));
        while (self.0.front()).is_some_and(|(t, _)| at.duration_since(*t) > SPEED_WINDOW) {
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
    item: String,
    request: String,
    destination: String,
    /// The batch owner, for sharing the workers fairly; kept in memory only, never logged.
    owner: String,
    track: GdTrack,
    phase: Phase,
    /// Bytes are moving: before that a running transfer still waits for its URL.
    fetching: bool,
    transferred: u64,
    speed: Speed,
    attempts: u32,
    /// The order transfers came in.
    seq: u64,
    task: Option<JoinHandle<()>>,
}

impl Record {
    fn unfinished(&self) -> bool {
        !matches!(self.phase, Phase::Ended(..))
    }

    fn part(&self) -> String {
        part_name(&self.id, &self.track.ext)
    }
}

/// The partial file of a transfer: its id, and its format's extension, which the tag writer
/// reads the format by.
fn part_name(id: &str, ext: &str) -> String {
    format!("{id}.{ext}")
}

#[derive(Default)]
struct State {
    records: HashMap<String, Record>,
    next_seq: u64,
    /// Bumped by each stop: a handle built before it is refused.
    generation: u64,
}

impl State {
    fn running(&self) -> usize {
        (self.records.values())
            .filter(|r| r.phase == Phase::Running)
            .count()
    }

    /// The queued transfer to start next: of the owner running the fewest, the one that came
    /// first.
    fn next_to_start(&self) -> Option<String> {
        let mut by_owner: HashMap<&str, usize> = HashMap::new();
        for r in self.records.values().filter(|r| r.phase == Phase::Running) {
            *by_owner.entry(r.owner.as_str()).or_default() += 1;
        }
        (self.records.values())
            .filter(|r| r.phase == Phase::Queued)
            .min_by_key(|r| (by_owner.get(r.owner.as_str()).copied().unwrap_or(0), r.seq))
            .map(|r| r.id.clone())
    }

    /// A queued transfer's place in the install's line.
    fn place(&self, r: &Record) -> u32 {
        let ahead = (self.records.values())
            .filter(|o| o.phase == Phase::Queued && o.seq < r.seq)
            .count();
        ahead as u32 + 1
    }
}

/// What a transfer task fetches.
#[derive(Debug, Clone)]
struct Job {
    id: String,
    item: String,
    destination: String,
    track: GdTrack,
}

impl Job {
    fn part(&self) -> String {
        part_name(&self.id, &self.track.ext)
    }
}

/// How one request of a transfer went.
enum Step {
    /// Every byte is in the part.
    Done,
    /// The part already landed: a landing cut between its link and its removal.
    Landed,
    /// The server refused the URL at the start of the request.
    Forbidden(u16),
    /// The answer stopped early or never came: ask again from where the bytes end.
    Cut {
        stalled: bool,
        why: String,
    },
    Failed(Failure),
}

/// The transfers a stop marked: each one's id, partial file and task.
pub(crate) struct Halted(Vec<(String, String, Option<JoinHandle<()>>)>);

fn plain_folder(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// The remote basename `resolve_in_destination` reads from an item id: its last segment.
fn basename(item: &str) -> &str {
    item.rsplit('/').next().unwrap_or(item)
}

/// The item's track, when the item is one this backend's search made: its id names the track
/// and a file of the track's format.
fn track_of(item: &DownloadableItem) -> Option<GdTrack> {
    if item.backend != BACKEND_ID || item.source != SOURCE {
        return None;
    }
    let track: GdTrack = serde_json::from_str(item.backend_data.as_deref()?).ok()?;
    let hex = |s: &str| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let (id, name) = item.id.split_once('/')?;
    let named = id == track.track_id
        && plain_folder(name)
        && name.ends_with(&format!(" [{}].{}", track.track_id, track.ext));
    let sound = matches!(track.ext.as_str(), "flac" | "mp3")
        && hex(&track.md5)
        && track.md5 == track.md5.to_ascii_lowercase()
        && track.size > 0
        && !track.track_id.is_empty();
    (named && sound).then_some(track)
}

fn length_of(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The MD5 of the file at `path`, read off the async runtime.
async fn md5_of(path: PathBuf) -> io::Result<String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = md5::Context::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                return Ok(format!("{:x}", hasher.compute()));
            }
            hasher.consume(&buffer[..n]);
        }
    })
    .await
    .map_err(io::Error::other)?
}

/// The first bytes of the file at `path`.
fn head_of(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path)?
        .take(HEAD_BYTES)
        .read_to_end(&mut head)?;
    Ok(head)
}

/// The start and total of a 206 answer's `Content-Range: bytes <start>-<end>/<total>`.
fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn wanted_tags(track: &GdTrack) -> WantedTags {
    let tags = &track.tags;
    WantedTags {
        title: tags.title.clone(),
        artist: tags.artist.clone(),
        album: tags.album.clone(),
        album_artist: tags.album_artist.clone(),
        track_no: tags.track,
        track_total: tags.track_total,
        date: tags.date.clone(),
        musicbrainz: tags.musicbrainz.as_ref().map(|m| MusicBrainzIds {
            recording: m.recording.clone(),
            release: m.release.clone(),
            title_score: m.title_score,
            album_score: m.album_score,
            length_diff_s: m.length_diff_s,
        }),
        comment: None,
    }
}

/// The same file is already fetched into the same folder.
fn already_fetching() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        "The GD Studio backend is already fetching this file into this folder",
    )
}

pub(crate) struct Transfers {
    dir: PartialDir,
    download_path: PathBuf,
    api: Arc<Api>,
    /// Fetches files, never following a redirect off an allowed host.
    http: reqwest::Client,
    limits: Limits,
    state: Mutex<State>,
}

impl Transfers {
    /// The engine's transfers: its partial folder opened first, then emptied, since the
    /// transfers of an earlier run are gone with it. When the folder cannot be opened (another
    /// server holds it, or it cannot be written) nothing is touched.
    pub(crate) async fn start(
        download_path: &Path,
        instance: &str,
        api: Arc<Api>,
        limits: Limits,
    ) -> Result<Arc<Transfers>, GdError> {
        let dir = open_dir(download_path, instance)?;
        let clearing = dir.clone();
        tokio::task::spawn_blocking(move || clearing.clear_except(&HashSet::new()))
            .await
            .map_err(io::Error::other)
            .and_then(|r| r)
            .map_err(|e| storage(download_path, e))?;
        let http = reqwest::Client::builder()
            .user_agent(crate::polite::USER_AGENT)
            .connect_timeout(limits.connect_timeout)
            .read_timeout(limits.read_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                GdError::Remote(crate::polite::RemoteError::Unavailable {
                    service: super::api::SERVICE,
                    detail: e.without_url().to_string(),
                })
            })?;
        Ok(Arc::new(Transfers {
            dir,
            download_path: download_path.to_path_buf(),
            api,
            http,
            limits,
            state: Mutex::new(State::default()),
        }))
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).generation
    }

    /// Runs `f` while `generation` is the current one, so no stop can come between the check
    /// and what `f` starts.
    pub(crate) fn while_current<T>(
        &self,
        generation: u64,
        f: impl FnOnce() -> Result<T, GdError>,
    ) -> Result<T, GdError> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation != generation {
            return Err(GdError::TurnedOff);
        }
        f()
    }

    /// Whether the partial folder can be written now: a file made and removed through it.
    pub(crate) fn probe_storage(&self) -> Result<(), GdError> {
        const PROBE: &str = ".health";
        let made = self.dir.open_part(PROBE, false);
        let removed = made.and_then(|_| std::fs::remove_file(self.dir.part(PROBE)));
        removed.map_err(|e| storage(&self.download_path, e))
    }

    /// Starts the queued transfers that may start now.
    fn pump(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.running() < self.limits.workers {
            let Some(id) = state.next_to_start() else {
                break;
            };
            let task = tokio::spawn(self.clone().run(id.clone()));
            if let Some(record) = state.records.get_mut(&id) {
                record.phase = Phase::Running;
                record.task = Some(task);
            }
        }
    }

    /// Enqueues `items` into `destination` for the handle of `generation`. An item this
    /// backend's search did not make gets no answer.
    pub(crate) fn download(
        self: &Arc<Self>,
        generation: u64,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        owner: &str,
    ) -> Result<Vec<QueuedDownload>, GdError> {
        let mut answers = Vec::new();
        {
            let mut guard = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = &mut *guard;
            if state.generation != generation {
                return Err(GdError::TurnedOff);
            }
            if !plain_folder(destination) {
                return Ok(vec![]);
            }
            state.records.retain(|_, r| match r.phase {
                Phase::Ended(_, at) => at.elapsed() < ENDED_KEPT,
                _ => true,
            });
            for item in items {
                let Some(track) = track_of(&item) else {
                    continue;
                };
                let answer = |id: String, status| QueuedDownload {
                    id,
                    source: SOURCE.to_string(),
                    item: item.id.clone(),
                    size: track.size,
                    status,
                };
                let fetching = state
                    .records
                    .values()
                    .any(|r| r.unfinished() && r.item == item.id && r.destination == destination);
                if fetching {
                    answers.push(answer(
                        item.id.clone(),
                        EnqueueStatus::Failed(already_fetching()),
                    ));
                    continue;
                }
                let id = uuid::Uuid::new_v4().to_string();
                state.records.insert(
                    id.clone(),
                    Record {
                        id: id.clone(),
                        item: item.id.clone(),
                        request: request.to_string(),
                        destination: destination.to_string(),
                        owner: owner.to_string(),
                        track: track.clone(),
                        phase: Phase::Queued,
                        fetching: false,
                        transferred: 0,
                        speed: Speed::default(),
                        attempts: 0,
                        seq: state.next_seq,
                        task: None,
                    },
                );
                state.next_seq += 1;
                answers.push(answer(id, EnqueueStatus::Enqueued));
            }
        }
        self.pump();
        Ok(answers)
    }

    /// The transfers from any of `sources`.
    pub(crate) fn listed(&self, sources: &[String]) -> Vec<DownloadProgress> {
        if !sources.iter().any(|s| s == SOURCE) {
            return vec![];
        }
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.records.values())
            .map(|r| Self::progress(&state, r))
            .collect()
    }

    pub(crate) fn one(&self, source: &str, id: &str) -> Option<DownloadProgress> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.records.get(id))
            .filter(|_| source == SOURCE)
            .map(|r| Self::progress(&state, r))
    }

    pub(crate) fn queue_position(&self, source: &str, id: &str) -> Option<u32> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = state.records.get(id).filter(|_| source == SOURCE)?;
        Some(match record.phase {
            Phase::Queued => state.place(record),
            _ => 0,
        })
    }

    fn progress(state: &State, r: &Record) -> DownloadProgress {
        let size = r.track.size;
        let mut row =
            DownloadProgress::queued(r.id.clone(), SOURCE.to_string(), r.item.clone(), size)
                .with_backend(BACKEND_ID);
        row.request = Some(r.request.clone());
        row.attempts = r.attempts;
        row.transferred = r.transferred.min(size);
        row.percent = if size == 0 {
            0.0
        } else {
            row.transferred as f64 * 100.0 / size as f64
        };
        match &r.phase {
            Phase::Queued => row.queue_position = Some(state.place(r)),
            Phase::Running if !r.fetching => row.queue_position = Some(0),
            Phase::Running => {
                row.state = DownloadState::InProgress;
                row.speed = r.speed.bytes_per_sec();
                row.queue_position = Some(0);
            }
            Phase::Stopping | Phase::Ended(Outcome::Cancelled, _) => {
                row.state = DownloadState::Cancelled
            }
            Phase::Ended(Outcome::Completed, _) => {
                row.state = DownloadState::Completed;
                row.transferred = size;
                row.percent = 100.0;
            }
            Phase::Ended(Outcome::Failed(failure), _) => row = row.failed_with(failure.clone()),
        }
        row
    }

    /// Stops the transfer `id`, deletes its partial file and keeps it as cancelled, or with
    /// `remove` forgets it: an ended one too. A transfer that ends while it is being cancelled
    /// (its task was past its last wait, landing its file) keeps how it ended. A landed file is
    /// never touched. One this backend does not have is no error.
    pub(crate) async fn cancel(self: &Arc<Self>, source: &str, id: &str, remove: bool) {
        let (task, part) = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(record) = state.records.get_mut(id).filter(|_| source == SOURCE) else {
                return;
            };
            if record.unfinished() {
                record.phase = Phase::Stopping;
            }
            (record.task.take(), record.part())
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        let cancelled = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let cancelled = (state.records.get(id)).is_some_and(|r| r.phase == Phase::Stopping);
            if remove {
                state.records.remove(id);
            } else if let Some(record) = state.records.get_mut(id).filter(|_| cancelled) {
                record.phase = Phase::Ended(Outcome::Cancelled, Instant::now());
            }
            cancelled
        };
        if cancelled {
            self.remove_part(&part);
        }
        self.pump();
    }

    /// Refuses older handles from now on and marks every unfinished transfer stopping: what
    /// `clear` then stops.
    pub(crate) fn halt(&self) -> Halted {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation += 1;
        let halted = (state.records.values_mut())
            .filter(|r| r.unfinished())
            .map(|r| {
                r.phase = Phase::Stopping;
                (r.id.clone(), r.part(), r.task.take())
            })
            .collect();
        Halted(halted)
    }

    /// Stops every transfer `halt` marked, forgets it and deletes its partial file. Ended
    /// transfers stay.
    pub(crate) async fn clear(&self, Halted(halted): Halted) {
        for (id, part, task) in halted {
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
            }
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // One that ended just before it was stopped stays.
            if state.records.get(&id).is_some_and(Record::unfinished) {
                state.records.remove(&id);
                drop(state);
                self.remove_part(&part);
            }
        }
    }

    fn remove_part(&self, part: &str) {
        match std::fs::remove_file(self.dir.part(part)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!("Could not delete the GD Studio partial file {part}: {e}"),
        }
    }

    fn end(&self, id: &str, outcome: Outcome) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = state.records.get_mut(id) {
            if outcome == Outcome::Completed {
                record.transferred = record.track.size;
            }
            record.phase = Phase::Ended(outcome, Instant::now());
            record.task = None;
            record.fetching = false;
        }
    }

    fn job(&self, id: &str) -> Option<Job> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let r = state.records.get(id)?;
        Some(Job {
            id: r.id.clone(),
            item: r.item.clone(),
            destination: r.destination.clone(),
            track: r.track.clone(),
        })
    }

    /// One transfer's task: fetch, then end it as it went.
    /// Uses an RAII drop guard so worker slots are always released and `pump()`
    /// runs even on early return or panic.
    async fn run(self: Arc<Self>, id: String) {
        struct RunGuard {
            transfers: Arc<Transfers>,
            id: String,
            finished: bool,
        }

        impl Drop for RunGuard {
            fn drop(&mut self) {
                if !self.finished {
                    let part = {
                        let mut state = self
                            .transfers
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(record) = state.records.get_mut(&self.id) {
                            if record.phase == Phase::Running {
                                let part = record.part();
                                record.phase = Phase::Ended(
                                    Outcome::Failed(ended::connection(
                                        "GD Studio transfer task exited unexpectedly",
                                    )),
                                    Instant::now(),
                                );
                                record.task = None;
                                record.fetching = false;
                                Some(part)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(part) = part {
                        self.transfers.remove_part(&part);
                    }
                    if tokio::runtime::Handle::try_current().is_ok() {
                        self.transfers.pump();
                    }
                }
            }
        }

        let mut guard = RunGuard {
            transfers: Arc::clone(&self),
            id: id.clone(),
            finished: false,
        };
        let Some(job) = self.job(&id) else {
            return;
        };
        let track = &job.track;
        match self.fetch(&job).await {
            Ok(landed) => {
                info!(
                    "GD Studio: NetEase track {} ({}) landed at {}",
                    track.track_id,
                    track.md5,
                    landed.display()
                );
                self.end(&id, Outcome::Completed);
            }
            Err(failure) => {
                warn!(
                    "GD Studio transfer {id} of NetEase track {} ({}) failed: {}",
                    track.track_id, track.md5, failure.message
                );
                self.remove_part(&job.part());
                self.end(&id, Outcome::Failed(failure));
            }
        }
        guard.finished = true;
        self.pump();
    }

    fn set_progress(&self, id: &str, transferred: u64, fetching: bool) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = state.records.get_mut(id) {
            record.transferred = transferred;
            record.fetching = fetching;
            record.speed.record(Instant::now(), transferred);
        }
    }

    fn count_attempt(&self, id: &str) {
        if let Some(record) = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).records.get_mut(id) {
            record.attempts += 1;
        }
    }

    /// Where the job's file is now: the URL a search was given while it may still be used,
    /// unless `again`, else a fresh one. It must name the very file the search found.
    async fn locate(&self, job: &Job, again: bool) -> Result<Located, Failure> {
        let track = &job.track;
        let named = match self.api.fresh(&track.track_id).filter(|_| !again) {
            Some(located) => located,
            None => match self.api.resolve(&track.track_id).await {
                Err(e) => {
                    return Err(e.failure(
                        Stage::Transfer,
                        "GD Studio could not say where NetEase serves this track now",
                    ))
                }
                Ok(Resolved::NotServed) => {
                    return Err(ended::not_served("GD Studio answered an empty URL"))
                }
                Ok(Resolved::Refused(Refusal::Host(host))) => {
                    return Err(ended::foreign_host(&host))
                }
                Ok(Resolved::Refused(Refusal::File(why))) => return Err(ended::changed(why)),
                Ok(Resolved::Located(located)) => located,
            },
        };
        if !named.same_object(&track.ext, track.size, &track.md5) {
            return Err(ended::changed(format!(
                "now {}.{} of {} bytes, the search found {}.{} of {} bytes",
                named.md5, named.ext, named.size, track.md5, track.ext, track.size
            )));
        }
        Ok(named)
    }

    /// Fetches the job's file, checks, tags and lands it: where it landed.
    async fn fetch(&self, job: &Job) -> Result<PathBuf, Failure> {
        let mut located = self.locate(job, false).await?;
        let mut refreshed = false;
        let mut cuts = 0;
        loop {
            // A request started on a URL past its 25 minutes is refused.
            if located.issued.elapsed() >= self.limits.reuse {
                located = self.locate(job, true).await?;
            }
            self.count_attempt(&job.id);
            match self.attempt(job, &located).await {
                Step::Done => break,
                Step::Landed => return self.land(job),
                Step::Forbidden(status) if !refreshed => {
                    info!(
                        "{} answered {status} for NetEase track {}; asking GD Studio again",
                        located.host, job.track.track_id
                    );
                    refreshed = true;
                    located = self.locate(job, true).await?;
                }
                Step::Forbidden(status) => return Err(ended::refused(status)),
                Step::Cut { stalled, why } => {
                    cuts += 1;
                    if cuts >= self.limits.attempts {
                        return Err(match stalled {
                            true => ended::timed_out(&why),
                            false => ended::connection(&why),
                        });
                    }
                    tokio::time::sleep(self.limits.retry_wait).await;
                }
                Step::Failed(failure) => return Err(failure),
            }
        }
        self.check(job).await?;
        self.tag(job).await?;
        self.land(job)
    }

    /// One request of the transfer, from where its partial bytes end.
    async fn attempt(&self, job: &Job, located: &Located) -> Step {
        let size = job.track.size;
        let part = self.dir.part(&job.part());
        let mut have = length_of(&part);
        if have > size {
            return Step::Failed(ended::size(size, have));
        }
        let file = match self.dir.open_part(&job.part(), have > 0) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Step::Landed,
            Err(e) => return Step::Failed(ended::storage(&e, &self.download_path)),
        };
        let mut file = tokio::fs::File::from_std(file);
        if have == size {
            return Step::Done;
        }
        let mut request = self.http.get(&located.url);
        if have > 0 {
            request = request.header(RANGE, format!("bytes={have}-"));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                let stalled = e.is_timeout();
                return Step::Cut {
                    stalled,
                    why: e.without_url().to_string(),
                };
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        match status {
            StatusCode::PARTIAL_CONTENT => match content_range(&headers) {
                Some((_, total)) if total != size => return Step::Failed(ended::size(size, total)),
                Some((start, _)) if start == have => {}
                _ => {
                    return Step::Failed(ended::connection(&format!(
                        "{} answered another range than bytes={have}-",
                        located.host
                    )))
                }
            },
            StatusCode::OK => {
                let length = headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
                if length != Some(size) {
                    return Step::Failed(ended::size(size, length.unwrap_or(0)));
                }
                if have > 0 {
                    // The range was not served: the whole file comes again.
                    match self.dir.open_part(&job.part(), false) {
                        Ok(fresh) => file = tokio::fs::File::from_std(fresh),
                        Err(e) => return Step::Failed(ended::storage(&e, &self.download_path)),
                    }
                    have = 0;
                }
            }
            StatusCode::FORBIDDEN => return Step::Forbidden(403),
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                return Step::Failed(ended::not_served(&format!(
                    "{} answered HTTP {}",
                    located.host,
                    status.as_u16()
                )))
            }
            s if s.is_server_error() => {
                return Step::Cut {
                    stalled: false,
                    why: format!("{} answered HTTP {}", located.host, s.as_u16()),
                }
            }
            s => {
                return Step::Failed(ended::connection(&format!(
                    "{} answered HTTP {}",
                    located.host,
                    s.as_u16()
                )))
            }
        }
        self.set_progress(&job.id, have, true);
        let mut response = response;
        let cut = loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let after = have + bytes.len() as u64;
                    if after > size {
                        return Step::Failed(ended::size(size, after));
                    }
                    if let Err(e) = file.write_all(&bytes).await {
                        return Step::Failed(ended::storage(&e, &self.download_path));
                    }
                    have = after;
                    self.set_progress(&job.id, have, true);
                }
                Ok(None) => break None,
                Err(e) => {
                    let stalled = e.is_timeout();
                    let why = e.without_url().to_string();
                    warn!(
                        "{} cut NetEase track {} at {have} of {size} bytes: {why}",
                        located.host, job.track.track_id
                    );
                    break Some(Step::Cut { stalled, why });
                }
            }
        };
        let written = async {
            file.flush().await?;
            file.sync_all().await
        };
        if let Err(e) = written.await {
            return Step::Failed(ended::storage(&e, &self.download_path));
        }
        if let Some(cut) = cut {
            return cut;
        }
        if have < size {
            return Step::Cut {
                stalled: false,
                why: format!("the answer ended after {have} of {size} bytes"),
            };
        }
        Step::Done
    }

    /// The part's bytes are the file the search found: its MD5, and audio of its format.
    async fn check(&self, job: &Job) -> Result<(), Failure> {
        let part = self.dir.part(&job.part());
        let storage = |e: io::Error| ended::storage(&e, &self.download_path);
        let md5 = md5_of(part.clone()).await.map_err(storage)?;
        if md5 != job.track.md5 {
            return Err(ended::checksum(&job.track.md5, &md5));
        }
        let header = audio_header::read(&head_of(&part).map_err(storage)?)
            .map_err(|why| ended::not_audio(&job.track.ext, &why))?;
        if header.ext() != job.track.ext {
            return Err(ended::not_audio(
                &job.track.ext,
                &format!("it starts as {}", header.ext()),
            ));
        }
        if let Header::Flac { samples: 0, .. } = header {
            return Err(ended::not_audio("flac", "its STREAMINFO holds no samples"));
        }
        Ok(())
    }

    /// Writes the wanted track's tags into the part and puts them on disk.
    async fn tag(&self, job: &Job) -> Result<(), Failure> {
        let part = self.dir.part(&job.part());
        let tags = wanted_tags(&job.track);
        let written = tokio::task::spawn_blocking(move || {
            write_tags(&part, &tags)?;
            std::fs::File::open(&part)
                .and_then(|f| f.sync_all())
                .map_err(|source| TagError::Stat {
                    path: part.display().to_string(),
                    source,
                })
        })
        .await
        .map_err(|e| ended::storage(&io::Error::other(e), &self.download_path))?;
        written.map_err(|e| tag_failure(e, &self.download_path))
    }

    fn land(&self, job: &Job) -> Result<PathBuf, Failure> {
        land(
            &self.dir.part(&job.part()),
            &self.download_path,
            &job.destination,
            basename(&job.item),
        )
        .map_err(|e| ended::storage(&e, &self.download_path))
    }
}

/// How a transfer whose tags could not be written ends: a file lofty cannot read is no audio,
/// any other error is the disk's, with the OS error lofty met kept, so a full disk reads so.
fn tag_failure(e: TagError, download_path: &Path) -> Failure {
    match e {
        TagError::Read { .. } => ended::untaggable(e.to_string()),
        TagError::Stat { source, .. } => ended::storage(&source, download_path),
        other => match io_cause(&other) {
            Some(io) => ended::storage(io, download_path),
            None => ended::storage(&io::Error::other(other.to_string()), download_path),
        },
    }
}

/// The first `io::Error` among the sources of `e`.
fn io_cause<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a io::Error> {
    let mut next = e.source();
    while let Some(source) = next {
        if let Some(io) = source.downcast_ref::<io::Error>() {
            return Some(io);
        }
        next = source.source();
    }
    None
}

/// The backend's partial folder of this install.
pub(crate) fn open_dir(download_path: &Path, instance: &str) -> Result<PartialDir, GdError> {
    PartialDir::open(download_path, instance, BACKEND_ID).map_err(|e| storage(download_path, e))
}

fn storage(download_path: &Path, error: io::Error) -> GdError {
    GdError::Storage {
        download_path: download_path.to_path_buf(),
        error: Arc::new(error),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gdstudio::api::tests::{api_at, ok};
    use crate::gdstudio::matching::file_name;
    use crate::gdstudio::tests::{fast, Rig};
    use crate::test_http::{serve, serve_file, FileAnswer, Reply, Seen, ServedFile};
    use lofty::file::TaggedFileExt;
    use lofty::tag::ItemKey;
    use shared::gdstudio::{GdMusicBrainz, GdTags};

    type Log = Arc<Mutex<Vec<Seen>>>;

    pub(crate) fn md5_hex(bytes: &[u8]) -> String {
        format!("{:x}", md5::compute(bytes))
    }

    /// Silent audio the tag writer and the header reader take: a mono 16-bit 44.1 kHz FLAC of
    /// `frames` frames (2, 3 or 40), or 40 frames of a 128 kbit/s MP3.
    fn silence(frames: u32, ext: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/src/gdstudio/fixtures/silence-{frames}.{ext}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn flac_of_silence(frames: u32) -> Vec<u8> {
        silence(frames, "flac")
    }

    /// The item of the NetEase track `track_id`, the wanted `title` at position 1, whose file
    /// GD Studio says is `listed`.
    pub(crate) fn item(track_id: &str, title: &str, listed: &[u8], ext: &str) -> DownloadableItem {
        let data = GdTrack {
            track_id: track_id.into(),
            ext: ext.into(),
            size: listed.len() as u64,
            md5: md5_hex(listed),
            tags: GdTags {
                title: title.into(),
                artist: "Daft Punk".into(),
                album: "Discovery".into(),
                album_artist: "Daft Punk".into(),
                track: Some(1),
                track_total: Some(14),
                date: Some("2001-03-12".into()),
                musicbrainz: Some(GdMusicBrainz {
                    recording: "5c0e0f5c-8c35-4b1d-9d0a-1f6f2a8b9c01".into(),
                    release: "2b4e4a77-0e57-4d3e-8b4f-5d2f7c9e3a10".into(),
                    title_score: 1.0,
                    album_score: 1.0,
                    length_diff_s: Some(0),
                }),
            },
        };
        DownloadableItem {
            id: format!("{track_id}/{}", file_name(Some(1), title, track_id, ext)),
            backend: BACKEND_ID.into(),
            source: SOURCE.into(),
            title: title.into(),
            artist: "Daft Punk".into(),
            album: "Discovery".into(),
            size: Some(listed.len() as u64),
            duration: Some(1),
            quality: ext.into(),
            quality_score: 1.0,
            backend_data: Some(serde_json::to_string(&data).unwrap()),
            match_score: 1.0,
            below_floor: false,
            facts: Default::default(),
        }
    }

    /// GD Studio's answer naming the file of `md5` and `size` on the server at `cdn`.
    fn named(cdn: &str, md5: &str, ext: &str, size: usize) -> Reply {
        ok(&serde_json::json!({
            "url": format!("{cdn}/cdn/{md5}.{ext}"), "br": 900, "size": size,
            "from": "music.gdstudio.xyz"
        })
        .to_string())
    }

    /// NetEase's server, serving `body` as `answers` say.
    fn cdn(body: &[u8], answers: Vec<FileAnswer>) -> (String, Log) {
        serve_file(
            ServedFile {
                body: body.to_vec(),
                etag: None,
            },
            answers,
        )
    }

    async fn started(rig: &Rig, api: &str, limits: Limits) -> Arc<Transfers> {
        let api = Arc::new(api_at(api, &limits));
        Transfers::start(rig.root.path(), &rig.instance, api, limits)
            .await
            .unwrap()
    }

    async fn sent(transfers: &Arc<Transfers>, item: DownloadableItem, owner: &str) -> String {
        let queued = transfers
            .download(transfers.generation(), vec![item], DESTINATION, "r1", owner)
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, EnqueueStatus::Enqueued, "{queued:?}");
        queued[0].id.clone()
    }

    pub(crate) async fn wait_for(
        transfers: &Transfers,
        id: &str,
        until: impl Fn(&DownloadProgress) -> bool,
    ) -> DownloadProgress {
        for _ in 0..1000 {
            if let Some(row) = transfers.one(SOURCE, id).filter(|r| until(r)) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "transfer {id} never got there: {:?}",
            transfers.one(SOURCE, id)
        );
    }

    fn ended(row: &DownloadProgress) -> bool {
        matches!(
            row.state,
            DownloadState::Completed | DownloadState::Failed(_) | DownloadState::Cancelled
        )
    }

    fn folder(rig: &Rig) -> PathBuf {
        rig.root.path().join(DESTINATION)
    }

    const DESTINATION: &str = "Discovery [aaaa1111]";

    async fn failure_of(transfers: &Transfers, id: &str) -> Failure {
        let row = wait_for(transfers, id, ended).await;
        match row.failure {
            Some(failure) => failure,
            None => panic!("transfer {id} ended without a failure: {:?}", row.state),
        }
    }

    #[tokio::test]
    async fn a_file_lands_checked_and_tagged_under_its_item_name_and_stays_listed() {
        for (ext, file) in [("flac", flac_of_silence(2)), ("mp3", silence(40, "mp3"))] {
            let rig = Rig::new();
            let (files, fetched) = cdn(&file, vec![FileAnswer::Whole]);
            let (api, calls) = serve(vec![named(&files, &md5_hex(&file), ext, file.len())]);
            let transfers = started(&rig, &api, fast()).await;
            let item = item("17455800", "One More Time", &file, ext);
            let id = sent(&transfers, item.clone(), "u1").await;

            let row = wait_for(&transfers, &id, ended).await;
            assert_eq!(row.state, DownloadState::Completed, "{row:?}");
            assert_eq!(
                (
                    row.backend.as_deref(),
                    row.request.as_deref(),
                    row.item.as_str()
                ),
                (Some("gdstudio"), Some("r1"), item.id.as_str())
            );
            assert_eq!((row.transferred, row.percent), (file.len() as u64, 100.0));
            let landed = folder(&rig).join(format!("01 - One More Time [17455800].{ext}"));
            let tagged = lofty::read_from_path(&landed).unwrap();
            let tag = tagged.primary_tag().unwrap();
            assert_eq!(
                tag.get_string(ItemKey::TrackTitle),
                Some("One More Time"),
                "{ext}"
            );
            assert_eq!(tag.get_string(ItemKey::TrackNumber), Some("1"), "{ext}");
            assert_eq!(
                tag.get_string(ItemKey::MusicBrainzRecordingId),
                Some("5c0e0f5c-8c35-4b1d-9d0a-1f6f2a8b9c01"),
                "{ext}"
            );
            assert!(rig.parts().is_empty(), "{:?}", rig.parts());
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(
                fetched.lock().unwrap()[0].target,
                format!("/cdn/{}.{ext}", md5_hex(&file))
            );
            assert_eq!(
                fetched.lock().unwrap()[0].user_agent.as_deref(),
                Some(crate::polite::USER_AGENT)
            );
            assert_eq!(transfers.queue_position(SOURCE, &id), Some(0));

            transfers.cancel(SOURCE, &id, true).await;
            assert!(transfers.one(SOURCE, &id).is_none());
            assert!(landed.exists(), "remove never touches a landed file");
            assert_eq!(transfers.queue_position(SOURCE, &id), None);
        }
    }

    #[tokio::test]
    async fn a_url_a_search_was_given_is_used_while_it_is_fresh() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let (files, _) = cdn(&file, vec![FileAnswer::Whole]);
        let (api, calls) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let limits = fast();
        let shared_api = Arc::new(api_at(&api, &limits));
        // The search's resolve.
        assert!(matches!(
            shared_api.resolve("17455800").await.unwrap(),
            Resolved::Located(_)
        ));
        let transfers = Transfers::start(rig.root.path(), &rig.instance, shared_api, limits)
            .await
            .unwrap();
        let id = sent(
            &transfers,
            item("17455800", "One More Time", &file, "flac"),
            "u1",
        )
        .await;
        assert_eq!(
            wait_for(&transfers, &id, ended).await.state,
            DownloadState::Completed
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "no call of the download's own"
        );
    }

    #[tokio::test]
    async fn a_cut_answer_resumes_from_where_its_bytes_end() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, fetched) = cdn(&file, vec![FileAnswer::Cut(150), FileAnswer::Whole]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let row = wait_for(&transfers, &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(row.attempts, 2);
        let fetched = fetched.lock().unwrap();
        assert_eq!(fetched[0].range, None);
        assert_eq!(fetched[1].range.as_deref(), Some("bytes=150-"));
        assert!(folder(&rig).join("01 - Aerodynamic [1].flac").exists());
    }

    #[tokio::test]
    async fn a_refused_url_is_asked_for_again_once() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let (files, _) = cdn(&file, vec![FileAnswer::Status(403), FileAnswer::Whole]);
        let md5 = md5_hex(&file);
        let (api, calls) = serve(vec![
            named(&files, &md5, "flac", file.len()),
            named(&files, &md5, "flac", file.len()),
        ]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let row = wait_for(&transfers, &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        assert_eq!(calls.lock().unwrap().len(), 2);

        let rig = Rig::new();
        let (files, _) = cdn(
            &file,
            vec![FileAnswer::Status(403), FileAnswer::Status(403)],
        );
        let (api, _) = serve(vec![
            named(&files, &md5, "flac", file.len()),
            named(&files, &md5, "flac", file.len()),
        ]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let failure = failure_of(&transfers, &id).await;
        assert_eq!(failure.code, FailureCode::TransferConnectionFailed);
        assert!(rig.parts().is_empty());
    }

    #[tokio::test]
    async fn a_track_that_now_names_another_file_is_not_fetched() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let other = flac_of_silence(3);
        let (files, fetched) = cdn(&file, vec![]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&other), "flac", other.len())]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let failure = failure_of(&transfers, &id).await;
        assert_eq!(
            (failure.code, failure.retry),
            (FailureCode::SourceFileChanged, RetryAdvice::OtherQuery)
        );
        assert!(fetched.lock().unwrap().is_empty());

        // Asked again after a refusal, it names another file.
        let rig = Rig::new();
        let (files, fetched) = cdn(&file, vec![FileAnswer::Status(403)]);
        let (api, _) = serve(vec![
            named(&files, &md5_hex(&file), "flac", file.len()),
            named(&files, &md5_hex(&other), "flac", other.len()),
        ]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        assert_eq!(
            failure_of(&transfers, &id).await.code,
            FailureCode::SourceFileChanged
        );
        assert_eq!(fetched.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_track_no_longer_served_or_named_off_netease_fails_with_its_cause() {
        let file = flac_of_silence(2);
        for (answer, code) in [
            (
                ok(r#"{"url":"","br":0,"size":0,"from":"music.gdstudio.xyz"}"#),
                FailureCode::TransferRejectedNotShared,
            ),
            (
                ok(&serde_json::json!({
                    "url": format!("https://evil.example/cdn/{}.flac", md5_hex(&file)),
                    "br": 900, "size": file.len()
                })
                .to_string()),
                FailureCode::BackendResponseUnreadable,
            ),
        ] {
            let rig = Rig::new();
            let (api, _) = serve(vec![answer]);
            let transfers = started(&rig, &api, fast()).await;
            let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
            assert_eq!(failure_of(&transfers, &id).await.code, code);
        }
    }

    #[tokio::test]
    async fn bytes_of_another_size_md5_or_format_never_land() {
        let file = flac_of_silence(3);
        let mut same_length = file.clone();
        *same_length.last_mut().unwrap() ^= 0xFF;
        let mp3 = silence(40, "mp3");
        // (what the item lists, what the server sends, the failure)
        let cases: [(&[u8], &[u8], &str, FailureCode); 3] = [
            (
                &file[..file.len() - 10],
                &file,
                "flac",
                FailureCode::TransferSizeMismatch,
            ),
            (
                &file,
                &same_length,
                "flac",
                FailureCode::TransferChecksumMismatch,
            ),
            (&mp3, &mp3, "flac", FailureCode::TransferRejectedOther),
        ];
        for (listed, sent_bytes, ext, code) in cases {
            let rig = Rig::new();
            let (files, _) = cdn(sent_bytes, vec![FileAnswer::Whole]);
            let (api, _) = serve(vec![named(&files, &md5_hex(listed), ext, listed.len())]);
            let transfers = started(&rig, &api, fast()).await;
            let id = sent(&transfers, item("1", "Aerodynamic", listed, ext), "u1").await;
            assert_eq!(failure_of(&transfers, &id).await.code, code, "{code:?}");
            assert!(rig.parts().is_empty(), "{code:?}: {:?}", rig.parts());
            assert!(!folder(&rig).exists(), "{code:?}");
        }
    }

    #[tokio::test]
    async fn a_server_that_stops_sending_fails_the_transfer_after_its_attempts() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, fetched) = cdn(&file, vec![FileAnswer::Stall(100); 3]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let limits = Limits {
            read_timeout: Duration::from_millis(300),
            ..fast()
        };
        let transfers = started(&rig, &api, limits).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let failure = failure_of(&transfers, &id).await;
        assert_eq!(failure.code, FailureCode::TransferTimedOut);
        let fetched = fetched.lock().unwrap();
        assert_eq!(fetched.len(), 3);
        assert_eq!(fetched[1].range.as_deref(), Some("bytes=100-"));
    }

    #[tokio::test]
    async fn a_url_older_than_its_reuse_is_asked_for_again_before_the_request() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let (files, _) = cdn(&file, vec![FileAnswer::Whole]);
        let md5 = md5_hex(&file);
        let (api, calls) = serve(vec![
            named(&files, &md5, "flac", file.len()),
            named(&files, &md5, "flac", file.len()),
        ]);
        let limits = Limits {
            reuse: Duration::from_millis(50),
            ..fast()
        };
        let api_handle = Arc::new(api_at(&api, &limits));
        assert!(matches!(
            api_handle.resolve("1").await.unwrap(),
            Resolved::Located(_)
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let transfers = Transfers::start(rig.root.path(), &rig.instance, api_handle, limits)
            .await
            .unwrap();
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        assert_eq!(
            wait_for(&transfers, &id, ended).await.state,
            DownloadState::Completed
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_name_taken_in_the_folder_lands_as_a_copy_and_the_file_there_stays() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        std::fs::create_dir_all(folder(&rig)).unwrap();
        let name = "01 - Aerodynamic [1].flac";
        std::fs::write(folder(&rig).join(name), b"someone's").unwrap();
        let (files, _) = cdn(&file, vec![FileAnswer::Whole]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        assert_eq!(
            wait_for(&transfers, &id, ended).await.state,
            DownloadState::Completed
        );
        assert_eq!(
            std::fs::read(folder(&rig).join(name)).unwrap(),
            b"someone's"
        );
        let copies = std::fs::read_dir(folder(&rig))
            .unwrap()
            .flatten()
            .filter(|e| shared::download::is_ticks_copy(e.file_name().to_str().unwrap(), name))
            .count();
        assert_eq!(copies, 1);
    }

    #[tokio::test]
    async fn a_destination_that_is_not_one_plain_folder_gets_no_answer_and_nothing_written() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let (api, calls) = serve(vec![]);
        let transfers = started(&rig, &api, fast()).await;
        for destination in ["../x", "a/b", ".", "", "a\\b"] {
            let queued = transfers
                .download(
                    transfers.generation(),
                    vec![item("1", "Aerodynamic", &file, "flac")],
                    destination,
                    "r1",
                    "u1",
                )
                .unwrap();
            assert!(queued.is_empty(), "{destination:?}");
        }
        assert!(calls.lock().unwrap().is_empty());
        let written: Vec<_> = std::fs::read_dir(rig.root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(written, [std::ffi::OsString::from(".soulful-partial")]);
    }

    #[tokio::test]
    async fn an_item_this_search_did_not_make_gets_no_answer() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let (api, _) = serve(vec![]);
        let transfers = started(&rig, &api, fast()).await;
        let good = item("1", "Aerodynamic", &file, "flac");
        let renamed = DownloadableItem {
            id: "1/evil.sh".into(),
            ..good.clone()
        };
        let other_backend = DownloadableItem {
            backend: "slskd".into(),
            ..good.clone()
        };
        let mut wrong_ext = good.clone();
        wrong_ext.backend_data = Some(
            serde_json::to_string(&GdTrack {
                ext: "exe".into(),
                ..serde_json::from_str::<GdTrack>(good.backend_data.as_deref().unwrap()).unwrap()
            })
            .unwrap(),
        );
        let queued = transfers
            .download(
                transfers.generation(),
                vec![renamed, other_backend, wrong_ext],
                DESTINATION,
                "r1",
                "u1",
            )
            .unwrap();
        assert!(queued.is_empty(), "{queued:?}");
    }

    #[tokio::test]
    async fn a_cancel_deletes_only_the_part_and_a_remove_forgets_the_record() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, _) = cdn(&file, vec![FileAnswer::Stall(100)]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let running = wait_for(&transfers, &id, |r| r.transferred >= 100).await;
        assert_eq!(running.state, DownloadState::InProgress);
        let part = format!("{id}.flac");
        assert_eq!(rig.parts(), [part.as_str()]);

        transfers.cancel("another source", &id, false).await;
        assert_eq!(
            rig.parts(),
            [part.as_str()],
            "a cancel names the transfer's source"
        );
        transfers.cancel(SOURCE, &id, false).await;
        assert_eq!(
            transfers.one(SOURCE, &id).unwrap().state,
            DownloadState::Cancelled
        );
        assert!(rig.parts().is_empty());
        transfers.cancel(SOURCE, &id, true).await;
        assert!(transfers.one(SOURCE, &id).is_none());
        transfers.cancel(SOURCE, "never-sent", true).await;
    }

    #[tokio::test]
    async fn a_third_file_waits_for_one_of_the_two_workers() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, _) = cdn(&file, vec![FileAnswer::Stall(10), FileAnswer::Stall(10)]);
        let md5 = md5_hex(&file);
        let (api, _) = serve(vec![]);
        let transfers = started(&rig, &api, fast()).await;
        // The URLs come from the cache, as a search left them.
        let mut ids = Vec::new();
        for track in ["1", "2", "3"] {
            transfers.api.remember(track, &files, &md5, file.len());
            ids.push(sent(&transfers, item(track, "Aerodynamic", &file, "flac"), "u1").await);
        }
        for id in &ids[..2] {
            wait_for(&transfers, id, |r| r.state == DownloadState::InProgress).await;
        }
        let third = transfers.one(SOURCE, &ids[2]).unwrap();
        assert_eq!(
            (third.state, third.queued_at_source, third.queue_position),
            (DownloadState::Queued, false, Some(1))
        );
        assert_eq!(transfers.queue_position(SOURCE, &ids[0]), Some(0));
        assert_eq!(transfers.listed(&[SOURCE.to_string()]).len(), 3);
        assert!(transfers.listed(&["peer".to_string()]).is_empty());
        transfers.clear(transfers.halt()).await;
    }

    #[tokio::test]
    async fn turning_off_mid_transfer_leaves_no_part_and_refuses_older_handles() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, _) = cdn(&file, vec![FileAnswer::Whole, FileAnswer::Stall(100)]);
        let md5 = md5_hex(&file);
        let (api, _) = serve(vec![]);
        let transfers = started(&rig, &api, fast()).await;
        transfers.api.remember("1", &files, &md5, file.len());
        transfers.api.remember("2", &files, &md5, file.len());
        let finished = sent(&transfers, item("1", "One More Time", &file, "flac"), "u1").await;
        wait_for(&transfers, &finished, ended).await;
        let running = sent(&transfers, item("2", "Aerodynamic", &file, "flac"), "u2").await;
        wait_for(&transfers, &running, |r| r.transferred >= 100).await;
        let before = transfers.generation();

        transfers.clear(transfers.halt()).await;

        assert!(rig.parts().is_empty(), "{:?}", rig.parts());
        assert!(transfers.one(SOURCE, &running).is_none());
        assert_eq!(
            transfers.one(SOURCE, &finished).unwrap().state,
            DownloadState::Completed
        );
        assert!(folder(&rig).join("01 - One More Time [1].flac").exists());
        assert!(matches!(
            transfers.download(before, vec![], DESTINATION, "r", "u1"),
            Err(GdError::TurnedOff)
        ));
        assert!(matches!(
            transfers.while_current(before, || Ok(())),
            Err(GdError::TurnedOff)
        ));
        assert!(transfers
            .while_current(transfers.generation(), || Ok(()))
            .is_ok());
    }

    #[tokio::test]
    async fn the_same_file_into_the_same_folder_is_sent_once() {
        let rig = Rig::new();
        let file = flac_of_silence(40);
        let (files, _) = cdn(&file, vec![FileAnswer::Stall(10)]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let transfers = started(&rig, &api, fast()).await;
        sent(&transfers, item("1", "Aerodynamic", &file, "flac"), "u1").await;
        let again = transfers
            .download(
                transfers.generation(),
                vec![item("1", "Aerodynamic", &file, "flac")],
                DESTINATION,
                "r2",
                "u1",
            )
            .unwrap();
        let EnqueueStatus::Failed(failure) = &again[0].status else {
            panic!("{again:?}");
        };
        assert_eq!(failure.code, FailureCode::EnqueueFailed);
        transfers.clear(transfers.halt()).await;
    }

    /// A transfer `id` of `owner` in `phase`, the `seq`-th to come.
    fn record(id: &str, owner: &str, phase: Phase, seq: u64) -> Record {
        let file = flac_of_silence(2);
        Record {
            id: id.into(),
            item: format!("{id}/x"),
            request: "r".into(),
            destination: "D".into(),
            owner: owner.into(),
            track: track_of(&item("1", "x", &file, "flac")).unwrap(),
            phase,
            fetching: false,
            transferred: 0,
            speed: Speed::default(),
            attempts: 0,
            seq,
            task: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transfer_that_lands_while_it_is_cancelled_stays_completed() {
        let rig = Rig::new();
        let (api, _) = serve(vec![]);
        let transfers = started(&rig, &api, fast()).await;
        let (began, landing) = tokio::sync::oneshot::channel();
        let (land, landed) = std::sync::mpsc::channel::<()>();
        // A task past its last wait, as one inside `land` is: an abort does not stop it.
        let task = tokio::task::spawn_blocking({
            let transfers = transfers.clone();
            move || {
                began.send(()).unwrap();
                landed.recv().unwrap();
                transfers.end("t1", Outcome::Completed);
            }
        });
        landing.await.unwrap();
        let mut running = record("t1", "u1", Phase::Running, 0);
        running.task = Some(task);
        transfers
            .state
            .lock()
            .unwrap()
            .records
            .insert("t1".into(), running);

        let cancelling = tokio::spawn({
            let transfers = transfers.clone();
            async move { transfers.cancel(SOURCE, "t1", false).await }
        });
        wait_for(&transfers, "t1", |r| r.state == DownloadState::Cancelled).await;
        land.send(()).unwrap();
        cancelling.await.unwrap();
        assert_eq!(
            transfers.one(SOURCE, "t1").unwrap().state,
            DownloadState::Completed
        );
    }

    #[tokio::test]
    async fn a_long_title_lands_under_a_name_the_disk_takes_and_so_does_its_copy() {
        let rig = Rig::new();
        let file = flac_of_silence(2);
        let title = "夜".repeat(120);
        let item = item("1306373588", &title, &file, "flac");
        let name = basename(&item.id).to_string();
        // The name is taken, so the file lands as a ticks copy, 19 bytes longer.
        std::fs::create_dir_all(folder(&rig)).unwrap();
        std::fs::write(folder(&rig).join(&name), b"someone's").unwrap();
        let (files, _) = cdn(&file, vec![FileAnswer::Whole]);
        let (api, _) = serve(vec![named(&files, &md5_hex(&file), "flac", file.len())]);
        let transfers = started(&rig, &api, fast()).await;
        let id = sent(&transfers, item, "u1").await;
        let row = wait_for(&transfers, &id, ended).await;
        assert_eq!(row.state, DownloadState::Completed, "{row:?}");
        let copy = std::fs::read_dir(folder(&rig))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .find(|n| shared::download::is_ticks_copy(n, &name))
            .expect("a copy landed");
        assert_eq!(copy.len(), 255);
    }

    #[test]
    fn a_full_disk_while_tags_are_written_reads_as_one() {
        let path = Path::new("/downloads");
        let full = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let write = |source: std::io::Error| TagError::Write {
            path: "x.flac".into(),
            source: source.into(),
        };
        assert_eq!(
            tag_failure(write(full), path).code,
            FailureCode::DownloadDiskFull
        );
        let denied = std::io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(
            tag_failure(write(denied), path).code,
            FailureCode::TransferAborted
        );
        let linked = TagError::Linked {
            path: "x.flac".into(),
        };
        assert_eq!(tag_failure(linked, path).code, FailureCode::TransferAborted);
    }

    #[test]
    fn a_free_worker_goes_to_the_owner_running_the_fewest() {
        let mut state = State::default();
        for r in [
            record("a1", "alice", Phase::Running, 0),
            record("a2", "alice", Phase::Queued, 1),
            record("a3", "alice", Phase::Queued, 2),
            record("b1", "bob", Phase::Queued, 3),
        ] {
            state.records.insert(r.id.clone(), r);
        }
        assert_eq!(state.next_to_start().as_deref(), Some("b1"));
        state.records.get_mut("b1").unwrap().phase = Phase::Running;
        // Even again, the earliest goes first.
        assert_eq!(state.next_to_start().as_deref(), Some("a2"));
        assert_eq!(state.place(&state.records["a3"]), 2);
    }

    #[test]
    fn progress_with_zero_size_does_not_produce_nan() {
        let state = State::default();
        let mut r = record("z1", "alice", Phase::Running, 0);
        r.track.size = 0;
        r.transferred = 0;
        r.fetching = true;
        let row = Transfers::progress(&state, &r);
        assert!(!row.percent.is_nan(), "percent must not be NaN");
        assert_eq!(row.percent, 0.0);
        let json = serde_json::to_string(&row).unwrap();
        let round_trip: DownloadProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip.percent, 0.0);
    }
}
