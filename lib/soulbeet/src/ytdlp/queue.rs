//! The download queue: one per process, one video at a time. Users take turns, videos start
//! 10 to 20 seconds apart and at most 120 an hour, as yt-dlp's guest limits ask. Each video is
//! fetched by the helper into a staging folder of the backend's partial folder, checked, tagged
//! as the wanted track, and landed in its batch folder, never replacing a file. Transfers live
//! in memory only: a restart loses the ones not finished, and reconcile treats them as lost.

use super::error::{ended, Fault, Kind, YtDlpError};
use super::matching::{item_name, source_of, valid_video_id};
use super::protocol::{Answer, Done, Helper, Limits, Progress, Request};
use super::standing::Standing;
use crate::partial::{land, PartialDir};
use crate::tags::{write_tags, MusicBrainzIds, TagError, WantedTags};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use rand::Rng;
use shared::download::{
    DownloadProgress, DownloadState, DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::ytdlp::{YtTrack, BACKEND_ID};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};
use tracing::{info, warn};

/// An ended transfer's record is forgotten this long after it ended.
const ENDED_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);
/// The lowest Opus bitrate taken, in kbps: 250 and 249 are never taken quietly.
const MIN_ABR: u32 = 96;
/// How far the file's length may be from the one YouTube lists.
const LENGTH_SLACK_S: f64 = 2.0;

/// How hard the queue works YouTube.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// The shortest and the longest wait between two video starts, picked at random between.
    pub gap: (Duration, Duration),
    /// Video starts in any hour, at most: 40% of the guest limit, so the rest of the address's
    /// use of YouTube keeps room.
    pub per_hour: usize,
    /// The waits before the second and the third try of a video whose connection failed.
    pub network_waits: [Duration; 2],
    /// Transfers one user may have waiting.
    pub per_user: usize,
    /// The space between the starts of two helper searches.
    pub search_gap: Duration,
    /// A fetch that prints nothing for this long is killed.
    pub fetch_idle: Duration,
    /// A fetch that runs this long is killed.
    pub fetch_total: Duration,
}

impl Pace {
    pub const YOUTUBE: Pace = Pace {
        gap: (Duration::from_secs(10), Duration::from_secs(20)),
        per_hour: 120,
        network_waits: [Duration::from_secs(30), Duration::from_secs(120)],
        per_user: 200,
        search_gap: Duration::from_secs(3),
        fetch_idle: Duration::from_secs(120),
        fetch_total: Duration::from_secs(600),
    };

    /// A wait between two video starts, picked at random between the shortest and the longest.
    fn gap(&self) -> Duration {
        let (shortest, longest) = self.gap;
        if longest > shortest {
            rand::rng().random_range(shortest..=longest)
        } else {
            shortest
        }
    }
}

/// When videos may start: a gap after the last start, and at most `per_hour` starts in any hour.
#[derive(Debug, Default)]
struct Pacer {
    /// When videos started, in the last hour.
    starts: VecDeque<Instant>,
    /// When the next video may start, at the earliest.
    next_start: Option<Instant>,
}

const HOUR: Duration = Duration::from_secs(3600);

impl Pacer {
    /// How long from `now` until the next video may start.
    fn wait(&self, now: Instant, per_hour: usize) -> Duration {
        let gap = self
            .next_start
            .map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
        let capped = match self.starts.len() >= per_hour {
            true => (self.starts.front()).map_or(Duration::ZERO, |first| {
                (*first + HOUR).saturating_duration_since(now)
            }),
            false => Duration::ZERO,
        };
        gap.max(capped)
    }

    /// A video started at `now`; the next waits `gap` at least.
    fn started(&mut self, now: Instant, gap: Duration) {
        while (self.starts.front()).is_some_and(|at| now.duration_since(*at) >= HOUR) {
            self.starts.pop_front();
        }
        self.starts.push_back(now);
        self.next_start = Some(now + gap);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Queued,
    Running,
    /// Being cancelled or stopped: its task is going away.
    Stopping,
    Completed,
    Failed(Failure),
    Cancelled,
}

impl Phase {
    fn ended(&self) -> bool {
        matches!(self, Phase::Completed | Phase::Failed(_) | Phase::Cancelled)
    }
}

struct Record {
    id: String,
    source: String,
    item: String,
    request: String,
    destination: String,
    track: YtTrack,
    /// The batch owner, for taking turns; kept in memory only, never logged.
    owner: String,
    phase: Phase,
    size: u64,
    transferred: u64,
    speed: f64,
    attempts: u32,
    /// The order transfers came in.
    seq: u64,
    ended_at: Option<Instant>,
    task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    records: HashMap<String, Record>,
    next_seq: u64,
    /// Bumped by each stop: a handle built before it is refused.
    generation: u64,
    /// The turn each owner was last served in; an owner never served goes first.
    served: HashMap<String, u64>,
    turn: u64,
    pacer: Pacer,
}

impl State {
    fn queued(&self) -> impl Iterator<Item = &Record> {
        self.records.values().filter(|r| r.phase == Phase::Queued)
    }

    /// The queued transfer to start next: of the owner served longest ago, the one that came
    /// first.
    fn next(&self) -> Option<String> {
        self.queued()
            .min_by_key(|r| (self.served.get(&r.owner).copied().unwrap_or(0), r.seq))
            .map(|r| r.id.clone())
    }

    /// A queued transfer's place in the queue, 1 for the next one.
    fn place(&self, record: &Record) -> u32 {
        let ahead = self.queued().filter(|o| o.seq < record.seq).count();
        ahead as u32 + 1
    }

    fn prune(&mut self) {
        self.records
            .retain(|_, r| r.ended_at.is_none_or(|at| at.elapsed() < ENDED_KEPT));
    }
}

/// What a transfer task fetches.
#[derive(Debug, Clone)]
struct Job {
    id: String,
    item: String,
    destination: String,
    track: YtTrack,
}

pub(crate) struct Queue {
    dir: PartialDir,
    download_path: PathBuf,
    helper: Helper,
    standing: Arc<Standing>,
    pace: Pace,
    state: Mutex<State>,
    /// Notified when a transfer is queued.
    queued: Notify,
    /// Notified when the running transfer is gone.
    ended: Notify,
}

fn plain_folder(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

impl Queue {
    /// The queue, its worker started.
    pub(crate) fn start(
        dir: PartialDir,
        download_path: PathBuf,
        helper: Helper,
        standing: Arc<Standing>,
        pace: Pace,
    ) -> Arc<Queue> {
        let queue = Arc::new(Queue {
            dir,
            download_path,
            helper,
            standing,
            pace,
            state: Mutex::new(State::default()),
            queued: Notify::new(),
            ended: Notify::new(),
        });
        tokio::spawn(queue.clone().work());
        queue
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    /// Runs `f` while `generation` is the current one, so no stop comes between the check and
    /// what `f` starts.
    pub(crate) fn while_current<T>(
        &self,
        generation: u64,
        f: impl FnOnce() -> Result<T, YtDlpError>,
    ) -> Result<T, YtDlpError> {
        let state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err(YtDlpError::TurnedOff);
        }
        f()
    }

    /// How many transfers wait now.
    pub(crate) fn waiting(&self) -> u64 {
        self.state.lock().unwrap().queued().count() as u64
    }

    /// The worker: starts one queued transfer at a time, when the pace allows, and fails what
    /// waits when YouTube paused this server.
    async fn work(self: Arc<Self>) {
        loop {
            if let Some(pause) = self.standing.paused() {
                self.fail_queued(&ended::paused(pause));
            }
            let wait = {
                let state = self.state.lock().unwrap();
                let waiting = state.queued().next().is_some();
                waiting.then(|| state.pacer.wait(Instant::now(), self.pace.per_hour))
            };
            match wait {
                None => {
                    tokio::select! {
                        () = self.queued.notified() => {}
                        () = self.standing.pause_started() => {}
                    }
                    continue;
                }
                Some(wait) if !wait.is_zero() => {
                    tokio::select! {
                        () = sleep(wait) => {}
                        () = self.standing.pause_started() => {}
                    }
                    continue;
                }
                Some(_) => {}
            }
            if self.start_next() {
                while self.running() {
                    self.ended.notified().await;
                }
            }
        }
    }

    fn running(&self) -> bool {
        let state = self.state.lock().unwrap();
        state
            .records
            .values()
            .any(|r| matches!(r.phase, Phase::Running | Phase::Stopping))
    }

    /// Starts the next queued transfer: whether one started.
    fn start_next(self: &Arc<Self>) -> bool {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let Some(id) = state.next() else {
            return false;
        };
        state.pacer.started(Instant::now(), self.pace.gap());
        state.turn += 1;
        let turn = state.turn;
        let Some(record) = state.records.get_mut(&id) else {
            return false;
        };
        state.served.insert(record.owner.clone(), turn);
        record.phase = Phase::Running;
        record.task = Some(tokio::spawn(self.clone().run(id)));
        true
    }

    /// Fails every queued transfer with `failure`.
    fn fail_queued(&self, failure: &Failure) {
        let mut state = self.state.lock().unwrap();
        for record in state.records.values_mut() {
            if record.phase == Phase::Queued {
                record.phase = Phase::Failed(failure.clone());
                record.ended_at = Some(Instant::now());
            }
        }
    }

    /// Enqueues `items` into `destination` for the handle of `generation`, on behalf of
    /// `owner`. An item whose data is not a YouTube track this backend offered gets no answer.
    pub(crate) fn download(
        &self,
        generation: u64,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        owner: &str,
    ) -> Result<Vec<QueuedDownload>, YtDlpError> {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err(YtDlpError::TurnedOff);
        }
        if !plain_folder(destination) {
            return Ok(vec![]);
        }
        state.prune();
        let paused = self.standing.paused();
        let mut answers = Vec::new();
        for item in items.into_iter().filter(|i| i.backend == BACKEND_ID) {
            // The item comes back from the browser: only what a search offered is fetched.
            let Some(track) = item
                .backend_data
                .as_deref()
                .and_then(|d| serde_json::from_str::<YtTrack>(d).ok())
                .filter(|t| {
                    valid_video_id(&t.video_id)
                        && item.source == source_of(t.browse_id.as_deref(), &t.video_id)
                        && item.id == item_name(&t.wanted.artist, &t.wanted.title, &t.video_id)
                })
            else {
                continue;
            };
            let answer = |status| QueuedDownload {
                id: item.id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                size: 0,
                status,
            };
            if let Some(pause) = paused {
                let failure = YtDlpError::Paused(pause).failure(Stage::Enqueue, pause.to_string());
                answers.push(answer(EnqueueStatus::Failed(failure)));
                continue;
            }
            let fetching = state.records.values().any(|r| {
                !r.phase.ended()
                    && r.source == item.source
                    && r.item == item.id
                    && r.destination == destination
            });
            if fetching {
                answers.push(answer(EnqueueStatus::Failed(already_fetching())));
                continue;
            }
            let waiting = state.queued().filter(|r| r.owner == owner).count();
            if waiting >= self.pace.per_user {
                answers.push(answer(EnqueueStatus::Failed(too_many(self.pace.per_user))));
                continue;
            }
            let id = uuid::Uuid::new_v4().to_string();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.records.insert(
                id.clone(),
                Record {
                    id: id.clone(),
                    source: item.source.clone(),
                    item: item.id.clone(),
                    request: request.to_string(),
                    destination: destination.to_string(),
                    track,
                    owner: owner.to_string(),
                    phase: Phase::Queued,
                    size: 0,
                    transferred: 0,
                    speed: 0.0,
                    attempts: 0,
                    seq,
                    ended_at: None,
                    task: None,
                },
            );
            answers.push(QueuedDownload {
                id,
                ..answer(EnqueueStatus::Enqueued)
            });
        }
        drop(state);
        self.queued.notify_one();
        Ok(answers)
    }

    /// The transfers from any of `sources`.
    pub(crate) fn listed(&self, sources: &[String]) -> Vec<DownloadProgress> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        (state.records.values())
            .filter(|r| sources.contains(&r.source))
            .map(|r| progress(&state, r))
            .collect()
    }

    pub(crate) fn one(&self, source: &str, id: &str) -> Option<DownloadProgress> {
        let state = self.state.lock().unwrap();
        let record = state.records.get(id).filter(|r| r.source == source)?;
        Some(progress(&state, record))
    }

    pub(crate) fn queue_position(&self, source: &str, id: &str) -> Option<u32> {
        let state = self.state.lock().unwrap();
        let record = state.records.get(id).filter(|r| r.source == source)?;
        Some(match record.phase {
            Phase::Queued => state.place(record),
            _ => 0,
        })
    }

    /// Stops the transfer `id` of `source`, killing its helper, deletes its staging folder and
    /// keeps it as cancelled, or with `remove` forgets it: an ended one too. A landed file is
    /// never touched.
    pub(crate) async fn cancel(&self, source: &str, id: &str, remove: bool) {
        let task = {
            let mut state = self.state.lock().unwrap();
            let Some(record) = state.records.get_mut(id).filter(|r| r.source == source) else {
                return;
            };
            if !record.phase.ended() {
                record.phase = Phase::Stopping;
            }
            record.task.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        {
            let mut state = self.state.lock().unwrap();
            if remove {
                state.records.remove(id);
            } else if let Some(record) = state.records.get_mut(id) {
                if record.phase == Phase::Stopping {
                    record.phase = Phase::Cancelled;
                    record.ended_at = Some(Instant::now());
                }
            }
        }
        self.remove_staging(id);
        self.ended.notify_one();
    }

    /// Stops every unfinished transfer, killing its helper: refuses older handles from now on,
    /// deletes the staging folders and forgets the transfers. Ended transfers stay.
    pub(crate) async fn stop(&self) {
        let tasks: Vec<(String, Option<JoinHandle<()>>)> = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            (state.records.values_mut())
                .filter(|r| !r.phase.ended())
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
            {
                let mut state = self.state.lock().unwrap();
                // One that ended just before it was stopped stays.
                if state.records.get(&id).is_some_and(|r| !r.phase.ended()) {
                    state.records.remove(&id);
                }
            }
            self.remove_staging(&id);
        }
        self.ended.notify_one();
    }

    fn remove_staging(&self, id: &str) {
        match std::fs::remove_dir_all(self.dir.part(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!("Could not delete the staging folder of YouTube transfer {id}: {e}"),
        }
    }

    fn job(&self, id: &str) -> Option<Job> {
        let state = self.state.lock().unwrap();
        let r = state.records.get(id)?;
        Some(Job {
            id: r.id.clone(),
            item: r.item.clone(),
            destination: r.destination.clone(),
            track: r.track.clone(),
        })
    }

    fn set_progress(&self, id: &str, progress: Progress) {
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.records.get_mut(id) {
            record.transferred = progress.downloaded;
            if let Some(total) = progress.total {
                record.size = total;
            }
            record.speed = progress.speed.unwrap_or(0.0);
        }
    }

    fn count_attempt(&self, id: &str) {
        if let Some(record) = self.state.lock().unwrap().records.get_mut(id) {
            record.attempts += 1;
        }
    }

    fn end(&self, id: &str, outcome: Result<PathBuf, Failure>) {
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.records.get_mut(id) {
            record.phase = match outcome {
                Ok(_) => {
                    record.transferred = record.size;
                    Phase::Completed
                }
                Err(failure) => Phase::Failed(failure),
            };
            record.ended_at = Some(Instant::now());
            record.task = None;
        }
    }

    /// One transfer's task: fetch, land, then end it as it went.
    async fn run(self: Arc<Self>, id: String) {
        let Some(job) = self.job(&id) else {
            return;
        };
        let staging = self.dir.part(&id);
        let outcome = match std::fs::create_dir_all(&staging) {
            Ok(()) => self.fetch(&job, &staging).await,
            Err(e) => Err(ended::storage(&e, &self.download_path)),
        };
        match &outcome {
            Ok(landed) => info!("YouTube Music: {} landed at {}", job.item, landed.display()),
            Err(failure) => warn!(
                "YouTube Music transfer {id} of {} failed: {}",
                job.item, failure.message
            ),
        }
        self.end(&id, outcome);
        self.remove_staging(&id);
        self.ended.notify_one();
    }

    /// Fetches the job's video into `staging` and lands it: where it landed. A failed
    /// connection is tried twice more, after the waits of the pace.
    async fn fetch(&self, job: &Job, staging: &Path) -> Result<PathBuf, Failure> {
        let mut network_failures = 0;
        loop {
            self.count_attempt(&job.id);
            let request = Request::Fetch {
                video_id: job.track.video_id.clone(),
                staging_dir: staging.to_path_buf(),
                min_abr: MIN_ABR,
            };
            let limits = Limits {
                idle: self.pace.fetch_idle,
                total: self.pace.fetch_total,
            };
            let answer = self
                .helper
                .call(&request, limits, &mut |p| self.set_progress(&job.id, p))
                .await;
            let outcome = match answer {
                Ok(Answer::Done(done)) => Ok(done),
                Ok(other) => {
                    let shown: String = format!("{other:?}").chars().take(120).collect();
                    Err(Fault::new(
                        Kind::Unreadable,
                        format!("The helper answered a fetch with {shown}"),
                    ))
                }
                Err(fault) => Err(fault),
            };
            if let Some(pause) = self.standing.heard(outcome.as_ref().map(|_| ())) {
                self.fail_queued(&ended::paused(pause));
            }
            match outcome {
                Ok(done) => return self.land(job, staging, &done),
                Err(f) if f.kind == Kind::Network && network_failures < 2 => {
                    let wait = self.pace.network_waits[network_failures];
                    network_failures += 1;
                    info!(
                        "YouTube Music transfer {}: {}; trying again in {} s",
                        job.id,
                        f.message,
                        wait.as_secs()
                    );
                    sleep(wait).await;
                }
                Err(f) if matches!(f.kind, Kind::RateLimited | Kind::BotCheck) => {
                    return Err(match self.standing.paused() {
                        Some(pause) => ended::paused(pause),
                        None => ended::helper(&f),
                    })
                }
                Err(f) => return Err(ended::helper(&f)),
            }
        }
    }

    /// Checks the fetched file, tags it as the wanted track and lands it in its batch folder.
    fn land(&self, job: &Job, staging: &Path, done: &Done) -> Result<PathBuf, Failure> {
        let file = &done.path;
        // The helper names a file of its own staging folder, never another.
        let inside = file.parent() == Some(staging)
            && std::fs::symlink_metadata(file).is_ok_and(|m| m.is_file());
        if !inside {
            return Err(ended::helper(&Fault::new(
                Kind::Unreadable,
                format!("The helper named {} as its file", file.display()),
            )));
        }
        let abr = done.abr.unwrap_or(0.0);
        if done.acodec != "opus" || abr < f64::from(MIN_ABR) {
            return Err(ended::helper(&Fault::new(
                Kind::FormatMissing,
                format!("YouTube sent {} at {abr:.0} kbps", done.acodec),
            )));
        }
        let length = opus_length(file)?;
        if let Some(listed) = done.duration {
            if (length - listed).abs() > LENGTH_SLACK_S {
                return Err(ended::helper(&Fault::new(
                    Kind::Truncated,
                    format!("The file lasts {length:.1} s and YouTube lists {listed:.0} s"),
                )));
            }
        }
        let wanted = &job.track.wanted;
        let musicbrainz = match (&wanted.recording_mbid, &wanted.release_mbid) {
            (Some(recording), Some(release)) => Some(MusicBrainzIds {
                recording: recording.clone(),
                release: release.clone(),
                title_score: job.track.title_score,
                album_score: job.track.album_score,
                length_diff_s: wanted
                    .duration_s
                    .map(|w| (length - f64::from(w)).abs().round() as u32),
            }),
            _ => None,
        };
        let tags = WantedTags {
            title: wanted.title.clone(),
            artist: wanted.artist.clone(),
            album: wanted.album.clone(),
            album_artist: wanted.album_artist.clone(),
            track_no: wanted.track_no,
            track_total: wanted.track_total,
            date: wanted.date.clone(),
            musicbrainz,
            comment: Some(format!(
                "Lossy: YouTube Music, Opus {abr:.0} kbps, video {}",
                job.track.video_id
            )),
        };
        write_tags(file, &tags).map_err(|e| self.tag_failure(e))?;
        let synced = std::fs::File::open(file).and_then(|f| f.sync_all());
        synced.map_err(|e| ended::storage(&e, &self.download_path))?;
        land(file, &self.download_path, &job.destination, &job.item)
            .map_err(|e| ended::storage(&e, &self.download_path))
    }

    fn tag_failure(&self, e: TagError) -> Failure {
        let io_error = match &e {
            TagError::Stat { source, .. } => Some(source),
            TagError::Write { source, .. } => {
                std::error::Error::source(source).and_then(|s| s.downcast_ref::<io::Error>())
            }
            TagError::Read { .. } | TagError::Linked { .. } => None,
        };
        match io_error {
            Some(io_error) => ended::storage(io_error, &self.download_path),
            None => ended::helper(&Fault::new(Kind::Internal, e.to_string())),
        }
    }
}

/// The length of the Ogg Opus file at `path`, in seconds; a file that is not one fails.
fn opus_length(path: &Path) -> Result<f64, Failure> {
    let not_opus = |why: String| ended::helper(&Fault::new(Kind::FormatMissing, why));
    let file = lofty::read_from_path(path).map_err(|e| not_opus(format!("{e}")))?;
    if file.file_type() != FileType::Opus {
        return Err(not_opus(format!(
            "The file is {:?}, not Ogg Opus",
            file.file_type()
        )));
    }
    Ok(file.properties().duration().as_secs_f64())
}

fn progress(state: &State, r: &Record) -> DownloadProgress {
    let mut row = DownloadProgress::queued(r.id.clone(), r.source.clone(), r.item.clone(), r.size)
        .with_backend(BACKEND_ID);
    row.request = Some(r.request.clone());
    row.attempts = r.attempts;
    row.transferred = r.transferred.min(r.size);
    row.percent = if r.size == 0 {
        0.0
    } else {
        row.transferred as f64 * 100.0 / r.size as f64
    };
    match &r.phase {
        Phase::Queued => row.queue_position = Some(state.place(r)),
        Phase::Running => {
            row.state = DownloadState::InProgress;
            row.speed = r.speed;
            row.queue_position = Some(0);
        }
        Phase::Stopping | Phase::Cancelled => row.state = DownloadState::Cancelled,
        Phase::Completed => {
            row.state = DownloadState::Completed;
            row.transferred = r.size;
            row.percent = 100.0;
        }
        Phase::Failed(failure) => row = row.failed_with(failure.clone()),
    }
    row
}

/// The same video is already fetched into the same folder.
fn already_fetching() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        "The YouTube Music backend is already fetching this track into this folder",
    )
}

/// The user has `cap` transfers waiting already.
fn too_many(cap: usize) -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        format!("You have {cap} YouTube Music downloads waiting already"),
    )
}

#[cfg(test)]
pub(crate) mod tests;
