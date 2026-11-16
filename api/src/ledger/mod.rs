//! The job ledger: every download request as a job with its track slots, and
//! the source searches jobs come from, kept in SQLite so they survive a
//! restart. Every write goes through one writer task, so writes apply in the
//! order they were made; reads go to the pool (`store`).

use crate::auth::UserId;
use crate::transfer_registry::TransferRegistry;
use serde::{Deserialize, Serialize};
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadableGroup, DownloadableItem, JobSlot, MatchChoices,
};
use shared::failure::Failure;
use shared::recommendation::normalize_for_matching;
use soulbeet::{DriverChoice, ImportMode, ImportedItem};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

pub mod store;
mod writer;

use writer::{Inbox, Reply, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    Searching,
    NeedsSourceChoice,
    Queued,
    Downloading,
    Importing,
    Indexing,
    NeedsDecision,
    Done,
    Failed,
    Cancelled,
    Dismissed,
}

/// The states a job never leaves.
const TERMINAL: [JobState; 4] = [
    JobState::Done,
    JobState::Failed,
    JobState::Cancelled,
    JobState::Dismissed,
];

/// TERMINAL as an SQL list, for the `state IN` and `state NOT IN` clauses of the reads and the
/// writer.
const TERMINAL_SQL: &str = "('done', 'failed', 'cancelled', 'dismissed')";

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Searching => "searching",
            JobState::NeedsSourceChoice => "needs_source_choice",
            JobState::Queued => "queued",
            JobState::Downloading => "downloading",
            JobState::Importing => "importing",
            JobState::Indexing => "indexing",
            JobState::NeedsDecision => "needs_decision",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Dismissed => "dismissed",
        }
    }

    pub fn parse(text: &str) -> Option<JobState> {
        Some(match text {
            "searching" => JobState::Searching,
            "needs_source_choice" => JobState::NeedsSourceChoice,
            "queued" => JobState::Queued,
            "downloading" => JobState::Downloading,
            "importing" => JobState::Importing,
            "indexing" => JobState::Indexing,
            "needs_decision" => JobState::NeedsDecision,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            "dismissed" => JobState::Dismissed,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        TERMINAL.contains(&self)
    }
}

/// The state of a job whose slots are in `slots` (D6). `slots` is never empty.
pub fn job_state(slots: &[JobState]) -> JobState {
    use JobState::*;
    // While a slot is unfinished the job is at its earliest one; once all
    // ended, a failure outweighs a done file, which outweighs a cancel.
    [
        Searching,
        NeedsSourceChoice,
        Queued,
        Downloading,
        Importing,
        Indexing,
        NeedsDecision,
        Failed,
        Done,
        Cancelled,
    ]
    .into_iter()
    .find(|state| slots.contains(state))
    .unwrap_or(Dismissed)
}

/// An import as the ledger keeps it, to run it again after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredImport {
    pub sources: Vec<PathBuf>,
    pub target: PathBuf,
    pub album: bool,
    pub cleanup_root: Option<PathBuf>,
    pub choices: Option<MatchChoices>,
    /// How the run decides on a match: a retry the user chose for runs that way again.
    pub choice: DriverChoice,
    pub rows: Vec<(JobSlot, DownloadProgress)>,
}

/// Navidrome's two scan times at the first look with Navidrome idle (D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanBaseline {
    pub last_scan: chrono::DateTime<chrono::Utc>,
    pub started: chrono::DateTime<chrono::Utc>,
}

/// What an index wait knows so far (Task 10 reads and advances it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexWait {
    /// The import's first source, which names it: the slots of one import wait together.
    pub import: String,
    /// The library folder the items are relative to.
    pub library: PathBuf,
    /// Never empty: an import with no items is done at once (D14).
    pub items: Vec<ImportedItem>,
    /// When the import ended, on Soulful's clock.
    pub since: i64,
    /// Set at the first idle look, when the runner asks for a scan (D8).
    pub baseline: Option<ScanBaseline>,
    /// Navidrome's lastScan when the items were last looked up.
    pub checked: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    NeedsSourceChoice {
        job: String,
    },
    /// Slots and their first attempts of a job made without them (an auto-download's pick): the
    /// job is queued.
    Picked {
        job: String,
        items: Vec<(u32, DownloadableItem)>,
    },
    /// The monitor batch the job's files go into and the backend request they are sent in,
    /// recorded before the backend hears of them.
    Enqueued {
        job: String,
        monitor_batch_id: String,
        batch_label: String,
        request: String,
    },
    /// Rows of slots now in `state`, each carrying its slot. Rows without a job are ignored.
    Slots {
        state: JobState,
        rows: Vec<DownloadProgress>,
    },
    /// The slot's next source and the backend request it is sent in, recorded before it is sent;
    /// both None for a transfer it adopted.
    Attempt {
        slot: JobSlot,
        candidate: Option<Box<DownloadableItem>>,
        request: Option<String>,
    },
    ImportQueued(Box<StoredImport>),
    /// `next` is Indexing (with `index`), Done (an import with no items, D14), NeedsDecision
    /// (the import is kept) or Failed.
    ImportDone(Box<ImportDone>),
    Indexing {
        slots: Vec<JobSlot>,
        wait: Box<IndexWait>,
    },
    Dismiss(Vec<JobSlot>),
    /// Slots whose stored state cannot be read: they end failed, their stored row kept.
    Unreadable(Vec<JobSlot>),
    /// A job ended before or without its slots: every unfinished slot takes `state` too.
    Ended {
        job: String,
        state: JobState,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDone {
    pub import: StoredImport,
    pub rows: Vec<DownloadProgress>,
    pub next: JobState,
    pub index: Option<IndexWait>,
}

/// How long a search stays its starter's to poll and pick from.
pub const SEARCH_KEPT_SECS: i64 = 24 * 60 * 60;
/// How long a finished job stays in the ledger after its last change.
pub const JOB_KEPT_SECS: i64 = 30 * 24 * 60 * 60;
const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The most unfinished MCP jobs one user may have; a request past it makes no job.
pub const MCP_UNFINISHED_JOBS_PER_USER: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Track,
    Album,
}

impl JobKind {
    /// The kind of a web UI request: an album when it names one.
    pub fn of(wanted: &DownloadQuery) -> JobKind {
        match wanted.album {
            Some(_) => JobKind::Album,
            None => JobKind::Track,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            JobKind::Track => "track",
            JobKind::Album => "album",
        }
    }
}

/// Where a batch was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchOrigin {
    Ui,
    Mcp,
}

impl BatchOrigin {
    fn as_str(self) -> &'static str {
        match self {
            BatchOrigin::Ui => "ui",
            BatchOrigin::Mcp => "mcp",
        }
    }
}

/// Whether an MCP job takes its best source itself or waits for Claude's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    Auto,
    Review,
}

/// D6. Stored with an MCP search and with its job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceFilters {
    pub formats: Vec<String>,
    pub min_bitrate_kbps: Option<u32>,
    pub lossless_only: bool,
    pub max_queue_length: Option<u64>,
}

/// How an MCP job picks and keeps sources (design 11.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobOptions {
    pub pick: Pick,
    pub min_score: f64,
    pub filters: SourceFilters,
    pub max_sources: u32,
}

/// D2. What makes two jobs the same recording: equal `mbid`s when both have one, else equal
/// `text`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingKeys {
    pub mbid: Option<String>,
    pub text: Option<String>,
}

/// The keys of `wanted` as a job of `kind`: a track's are its recording MBID and its first
/// track's artist and title, an album's its release MBID and its artist and title.
pub fn recording_keys(wanted: &DownloadQuery, kind: JobKind) -> RecordingKeys {
    let (prefix, mbid_prefix, named) = match kind {
        JobKind::Track => (
            "track",
            "recording",
            wanted
                .tracks
                .first()
                .map(|t| (t.mbid.as_deref(), &t.artist, &t.title)),
        ),
        JobKind::Album => (
            "album",
            "release",
            wanted
                .album
                .as_ref()
                .map(|a| (a.mbid.as_deref(), &a.artist, &a.title)),
        ),
    };
    let Some((mbid, artist, title)) = named else {
        return RecordingKeys {
            mbid: None,
            text: None,
        };
    };
    RecordingKeys {
        mbid: mbid.map(|mbid| format!("{mbid_prefix}:{}", mbid.to_lowercase())),
        text: Some(format!(
            "{prefix}:{}\u{1f}{}",
            normalize_for_matching(artist),
            normalize_for_matching(title)
        )),
    }
}

/// A ledger batch to write (`core::acquire::NewBatch` is the monitor's batch, another thing).
pub struct NewJobBatch {
    pub id: String,
    pub user: UserId,
    pub label: Option<String>,
    pub origin: BatchOrigin,
}

pub struct NewJob {
    pub id: String,
    pub user: UserId,
    pub folder_id: String,
    pub wanted: DownloadQuery,
    pub import_mode: ImportMode,
    pub state: JobState,
    pub search_id: Option<String>,
    /// The slots and their first attempts, stored with the job: a request dropped while it waits
    /// for the job cannot leave it without them.
    pub picked: Vec<(u32, DownloadableItem)>,
    pub kind: JobKind,
    /// None for a web UI job.
    pub options: Option<JobOptions>,
}

/// What `create_batch` did with one job.
#[derive(Debug, Clone, PartialEq)]
pub enum Admitted {
    Created,
    /// The user's unfinished job `job` is the same recording.
    AlreadyRunning {
        job: String,
        state: JobState,
    },
    /// The user has `unfinished` MCP jobs, MCP_UNFINISHED_JOBS_PER_USER or more.
    OverLimit {
        unfinished: u32,
    },
}

/// The state change a tool or the job runner asks for, checked against the job's state in the
/// writer.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// D9: a job with no transfer ends cancelled; one that may have transfers is marked
    /// cancel_requested and keeps its state.
    Cancel,
    /// A job that will not be pursued ends dismissed.
    Dismiss,
    /// D15, the runner after a search: the job is queued with `items` as its slots.
    Pick { items: Vec<(u32, DownloadableItem)> },
    /// D15: the job waits for Claude's choice.
    AwaitChoice,
    /// D15: the job ends failed: a job without unfinished slots with `failure` in `jobs.failure`,
    /// one with them (picked, then the backend or its folder failed) with `failure` on every
    /// unfinished slot's row. A job whose user asked to cancel it ends cancelled instead.
    Fail { failure: Failure },
    /// Searching again under `search_id`; the job's slots are set aside (dismissed), their
    /// attempts kept. `filters`, when given, become the job's filters.
    Search {
        search_id: String,
        filters: Option<SourceFilters>,
    },
    /// Queued with these items as its slots' next attempts, under `search_id` when given
    /// (a candidate of another search, download_source).
    Queue {
        items: Vec<(u32, DownloadableItem)>,
        search_id: Option<String>,
    },
    /// A done job's result is rejected: its slots fail with `failure` and `source` is excluded.
    Reject { source: String, failure: Failure },
    /// The slots that failed at the index stage wait for Navidrome again from `now`.
    Reindex { now: i64 },
}

impl Transition {
    /// The states the job may be in (D11, the one list `next` reads too).
    pub fn allowed_from(&self) -> &'static [JobState] {
        use JobState::*;
        match self {
            Transition::Cancel => &[Searching, NeedsSourceChoice, Queued, Downloading],
            Transition::Dismiss => &[Failed, Cancelled, NeedsSourceChoice, NeedsDecision],
            Transition::Pick { .. } | Transition::AwaitChoice => &[Searching],
            Transition::Fail { .. } => &[Searching, Queued],
            Transition::Search { .. } | Transition::Queue { .. } => {
                &[Failed, Cancelled, NeedsSourceChoice]
            }
            Transition::Reject { .. } => &[Done],
            Transition::Reindex { .. } => &[Failed],
        }
    }
}

/// What `transition` did.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Transitioned {
    /// Made under change number `seq`.
    Applied {
        from: JobState,
        to: JobState,
        seq: i64,
    },
    /// The job is in `state`, which the change is not allowed from.
    Refused { state: JobState },
    /// No job of the user has that id.
    #[default]
    Unknown,
}

pub struct NewSearch {
    pub id: String,
    pub user: UserId,
    pub backend: Option<String>,
    pub query: DownloadQuery,
}

/// What one prune removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    pub searches: u64,
    pub dismissed: u64,
    pub jobs: u64,
}

/// A job a source pick continues: now queued, importing in the mode it was
/// recorded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub id: String,
    pub import_mode: ImportMode,
}

/// A ledger write that could not be made: the database's error, or the writer stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerError(pub String);

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone)]
pub struct Ledger {
    writes: mpsc::UnboundedSender<Write>,
    changes: watch::Receiver<i64>,
    /// No writer reads `writes`: requests are answered at once, events wait for `Recorded`.
    #[cfg(test)]
    recording: bool,
}

impl Ledger {
    fn channel() -> (Ledger, Inbox) {
        let (writes, inbox) = mpsc::unbounded_channel();
        let (changed, changes) = watch::channel(0);
        let ledger = Ledger {
            writes,
            changes,
            #[cfg(test)]
            recording: false,
        };
        (
            ledger,
            Inbox {
                writes: inbox,
                changed,
            },
        )
    }

    /// A ledger with no writer whose events a test reads. Requests that wait for a reply
    /// (create, record_search, store_candidates, claim_source_choice, prune, settled) are
    /// answered at once with Ok and the empty answer (no claimed job, nothing pruned) and
    /// are not recorded.
    #[cfg(test)]
    pub fn recording() -> (Ledger, Recorded) {
        let (ledger, inbox) = Ledger::channel();
        let ledger = Ledger {
            recording: true,
            ..ledger
        };
        (
            ledger,
            Recorded {
                inbox: inbox.writes,
            },
        )
    }

    /// Records `event`. The writer logs one the database refused.
    pub fn record(&self, event: JobEvent) {
        // The writer stops only with the runtime.
        let _ = self.writes.send(Write::Event(event));
    }

    /// Records `event` and waits until the writer has stored it, after every write sent before
    /// it. The writer logs one the database refused. A recording ledger records it and answers
    /// at once.
    pub async fn record_stored(&self, event: JobEvent) -> Result<(), LedgerError> {
        #[cfg(test)]
        if self.recording {
            self.record(event);
            return Ok(());
        }
        self.write(|reply| Write::Stored {
            event: Box::new(event),
            reply,
        })
        .await
    }

    /// A ledger whose writer runs on `db` (tests, and start_server_ledger).
    pub fn start(db: SqlitePool) -> Ledger {
        let (ledger, inbox) = Ledger::channel();
        writer::spawn(db, inbox, TransferRegistry::global());
        ledger
    }

    /// Sends the write `make` builds around its reply and waits for the answer. A writer that
    /// stopped is logged here; the writer logs what the database refused.
    async fn write<T: Default>(
        &self,
        make: impl FnOnce(Reply<T>) -> Write,
    ) -> Result<T, LedgerError> {
        #[cfg(test)]
        if self.recording {
            return Ok(T::default());
        }
        let stopped = || {
            let e = LedgerError("the job ledger's writer has stopped".into());
            warn!("{e}");
            e
        };
        let (reply, answer) = oneshot::channel();
        self.writes.send(make(reply)).map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }

    /// Records the web UI's `job` as a batch of its own (its id the job's, its label the wanted
    /// album's title, else the first track's), with the job's kind and recording keys, and
    /// first the search it came from when given.
    pub async fn create(&self, job: NewJob, search: Option<NewSearch>) -> Result<(), LedgerError> {
        self.write(|reply| Write::Create {
            job: Box::new(job),
            search: search.map(Box::new),
            reply,
        })
        .await
    }

    /// Records `batch` and its `jobs`, in order, each with its recording keys. A job that is the
    /// same recording as an unfinished job of its user (one earlier in this call included) is
    /// not made, nor one past MCP_UNFINISHED_JOBS_PER_USER in an MCP batch. The batch is
    /// written only when one of its jobs is made.
    pub async fn create_batch(
        &self,
        batch: NewJobBatch,
        jobs: Vec<NewJob>,
    ) -> Result<Vec<Admitted>, LedgerError> {
        self.write(|reply| Write::CreateBatch { batch, jobs, reply })
            .await
    }

    /// The last change number; it moves on with every write that changes a job.
    pub fn changes(&self) -> watch::Receiver<i64> {
        self.changes.clone()
    }

    /// D8: true when `job` (queued) already holds a download place or takes one now, its user's
    /// MCP jobs holding fewer than `limit`.
    pub async fn claim_download(&self, job: &str, limit: u32) -> Result<bool, LedgerError> {
        let job = job.to_string();
        self.write(|reply| Write::ClaimDownload { job, limit, reply })
            .await
    }

    /// `user`'s job `job` changed as `change` says when its state is one `allowed_from` lists.
    pub async fn transition(
        &self,
        user: &UserId,
        job: &str,
        change: Transition,
    ) -> Result<Transitioned, LedgerError> {
        let (user, job) = (user.clone(), job.to_string());
        self.write(|reply| Write::Transition {
            user,
            job,
            change: Box::new(change),
            reply,
        })
        .await
    }

    /// Records `search` as its user's for SEARCH_KEPT_SECS. A search of the same
    /// id is renewed instead: kept SEARCH_KEPT_SECS from now, its candidates
    /// dropped until it ends again.
    pub async fn record_search(&self, search: NewSearch) -> Result<(), LedgerError> {
        self.write(|reply| Write::RecordSearch {
            search: Box::new(search),
            reply,
        })
        .await
    }

    pub async fn store_candidates(
        &self,
        search_id: &str,
        groups: Vec<DownloadableGroup>,
    ) -> Result<(), LedgerError> {
        let search_id = search_id.to_string();
        self.write(|reply| Write::StoreCandidates {
            search_id,
            groups,
            reply,
        })
        .await
    }

    /// The oldest job of `user` waiting for a source from `search_id` into
    /// `folder_id`, now queued with `picked` as its slots; None when there is
    /// none. One conditional write in the writer, so of two claims only one
    /// gets the job.
    pub async fn claim_source_choice(
        &self,
        user: &UserId,
        search_id: &str,
        folder_id: &str,
        picked: Vec<(u32, DownloadableItem)>,
    ) -> Result<Option<ClaimedJob>, LedgerError> {
        let (user, search_id, folder_id) =
            (user.clone(), search_id.to_string(), folder_id.to_string());
        self.write(|reply| Write::ClaimSourceChoice {
            user,
            search_id,
            folder_id,
            picked,
            reply,
        })
        .await
    }

    /// At `now`: deletes searches with expires_at <= now, dismisses jobs
    /// waiting for a source whose search is gone or expired (D11) and stamps
    /// their updated_at with `now`, deletes terminal jobs with
    /// updated_at < now - JOB_KEPT_SECS, then batches with no job left.
    pub async fn prune(&self, now: i64) -> Result<Pruned, LedgerError> {
        self.write(|reply| Write::Prune { now, reply }).await
    }

    /// Waits until every write sent before it is applied.
    #[cfg(test)]
    pub async fn settled(&self) {
        self.write(|reply| Write::Settled { reply })
            .await
            .expect("the ledger writer runs");
    }
}

/// The events a `Ledger::recording` was given.
#[cfg(test)]
pub struct Recorded {
    inbox: mpsc::UnboundedReceiver<Write>,
}

#[cfg(test)]
impl Recorded {
    /// Every event recorded since the last call.
    pub fn events(&mut self) -> Vec<JobEvent> {
        std::iter::from_fn(|| self.inbox.try_recv().ok())
            .map(|write| match write {
                Write::Event(event) => event,
                _ => unreachable!("a recording ledger sends only its events"),
            })
            .collect()
    }
}

/// LEDGER's writer inbox, until start_server_ledger takes it.
static SERVER_INBOX: Mutex<Option<Inbox>> = Mutex::new(None);

/// The server's ledger. Writes wait in its channel until start_server_ledger starts the writer.
pub static LEDGER: LazyLock<Ledger> = LazyLock::new(|| {
    let (ledger, inbox) = Ledger::channel();
    *SERVER_INBOX.lock().unwrap() = Some(inbox);
    ledger
});

/// Starts LEDGER's writer on crate::db::DB, and a task that prunes it now and
/// every PRUNE_EVERY, logging what it removed.
pub fn start_server_ledger() {
    // Making LEDGER first puts its inbox in SERVER_INBOX.
    let ledger = LEDGER.clone();
    let Some(inbox) = SERVER_INBOX.lock().unwrap().take() else {
        return;
    };
    writer::spawn((*crate::db::DB).clone(), inbox, TransferRegistry::global());
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            // A refused prune and a stopped writer are both logged on their way back.
            if let Ok(Pruned {
                searches,
                dismissed,
                jobs,
            }) = ledger.prune(chrono::Utc::now().timestamp()).await
            {
                if searches + dismissed + jobs > 0 {
                    info!(
                        "Pruned the job ledger: {searches} expired searches removed, \
                         {dismissed} source choices dismissed, {jobs} finished jobs removed"
                    );
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_fns::download::test_support::{group, item};
    use shared::download::{DownloadProgress, DownloadState, IndexState};
    use shared::failure::{FailureCode, RetryAdvice, Stage};

    async fn ledger() -> (Ledger, SqlitePool, String, String) {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob) = (
            crate::core::folder_of(&db, "alice").await,
            crate::core::folder_of(&db, "bob").await,
        );
        (Ledger::start(db.clone()), db, alice, bob)
    }

    fn job(id: &str, user: &str, folder: &str, state: JobState, search: Option<&str>) -> NewJob {
        NewJob {
            id: id.into(),
            user: UserId::new(user),
            folder_id: folder.into(),
            wanted: DownloadQuery::new(vec![]),
            import_mode: ImportMode::Singleton,
            state,
            search_id: search.map(str::to_string),
            picked: vec![],
            kind: JobKind::Track,
            options: None,
        }
    }

    fn search(id: &str, user: &str) -> NewSearch {
        NewSearch {
            id: id.into(),
            user: UserId::new(user),
            backend: Some("slskd".into()),
            query: DownloadQuery::new(vec![]),
        }
    }

    #[tokio::test]
    async fn a_search_is_its_starters_for_a_day_and_keeps_its_candidates() {
        let (ledger, db, _, _) = ledger().await;
        ledger.record_search(search("s1", "alice")).await.unwrap();
        let found = store::search_of(&db, "s1", &UserId::new("alice"))
            .await
            .unwrap()
            .expect("alice's");
        let left = found.expires_at - chrono::Utc::now().timestamp();
        assert!(
            (SEARCH_KEPT_SECS - 5..=SEARCH_KEPT_SECS).contains(&left),
            "{left}"
        );
        assert_eq!(found.candidates, None);
        assert!(store::search_of(&db, "s1", &UserId::new("bob"))
            .await
            .unwrap()
            .is_none());
        let groups = vec![group(
            "peer_a",
            vec![item("peer_a", "One", "a/01.flac")],
            0.9,
        )];
        ledger.store_candidates("s1", groups.clone()).await.unwrap();
        assert_eq!(
            store::search_of(&db, "s1", &UserId::new("alice"))
                .await
                .unwrap()
                .unwrap()
                .candidates,
            Some(groups)
        );
    }

    #[tokio::test]
    async fn a_search_recorded_again_is_renewed_without_its_candidates() {
        let (ledger, db, _, _) = ledger().await;
        ledger.record_search(search("s1", "alice")).await.unwrap();
        ledger
            .store_candidates(
                "s1",
                vec![group(
                    "peer_a",
                    vec![item("peer_a", "One", "a/01.flac")],
                    0.9,
                )],
            )
            .await
            .unwrap();
        ledger.record_search(search("s1", "alice")).await.unwrap();
        let renewed = store::search_of(&db, "s1", &UserId::new("alice"))
            .await
            .unwrap()
            .expect("alice's");
        assert_eq!(renewed.candidates, None);
        let left = renewed.expires_at - chrono::Utc::now().timestamp();
        assert!(
            (SEARCH_KEPT_SECS - 5..=SEARCH_KEPT_SECS).contains(&left),
            "{left}"
        );
    }

    #[tokio::test]
    async fn only_one_pick_continues_a_job_waiting_for_a_source() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(
                job(
                    "j1",
                    "alice",
                    &alice,
                    JobState::NeedsSourceChoice,
                    Some("s1"),
                ),
                Some(search("s1", "alice")),
            )
            .await
            .unwrap();
        let user = UserId::new("alice");
        let picked = || vec![(0, item("peer_a", "One", "a/01.flac"))];
        let (a, b) = tokio::join!(
            ledger.claim_source_choice(&user, "s1", &alice, picked()),
            ledger.claim_source_choice(&user, "s1", &alice, picked())
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let mode = a
            .iter()
            .chain(b.iter())
            .map(|c| c.import_mode)
            .collect::<Vec<_>>();
        assert_eq!(mode, vec![ImportMode::Singleton]);
        let mut got = vec![a.map(|c| c.id), b.map(|c| c.id)];
        got.sort();
        assert_eq!(got, vec![None, Some("j1".to_string())]);
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Queued));
        // Only the claim that got the job stored its picks.
        let slots: Vec<u32> = sqlx::query_scalar("SELECT slot FROM job_tracks WHERE job_id = 'j1'")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(slots, vec![0]);
    }

    #[tokio::test]
    async fn a_job_is_stored_with_its_picked_slots_and_their_first_attempts() {
        let (ledger, db, alice, _) = ledger().await;
        let one = item("peer_a", "One", "a/01.flac");
        ledger
            .create(
                NewJob {
                    picked: vec![(0, one.clone())],
                    ..job("j1", "alice", &alice, JobState::Queued, None)
                },
                None,
            )
            .await
            .unwrap();
        let jobs = store::unfinished(&db).await.unwrap().jobs;
        assert_eq!(
            jobs[0]
                .slots
                .iter()
                .map(|s| (s.slot, s.state, s.attempts.clone()))
                .collect::<Vec<_>>(),
            vec![(0, JobState::Queued, vec![Some(one)])]
        );
    }

    #[tokio::test]
    async fn a_pick_continues_only_the_users_own_job_into_its_folder() {
        let (ledger, db, alice, bob) = ledger().await;
        ledger
            .create(
                job(
                    "j1",
                    "alice",
                    &alice,
                    JobState::NeedsSourceChoice,
                    Some("s1"),
                ),
                Some(search("s1", "alice")),
            )
            .await
            .unwrap();
        assert_eq!(
            ledger
                .claim_source_choice(&UserId::new("bob"), "s1", &bob, vec![])
                .await,
            Ok(None)
        );
        assert_eq!(
            ledger
                .claim_source_choice(&UserId::new("alice"), "s1", &bob, vec![])
                .await,
            Ok(None)
        );
        assert_eq!(
            store::job_state(&db, "j1").await,
            Some(JobState::NeedsSourceChoice)
        );
    }

    #[tokio::test]
    async fn pruning_drops_old_searches_dead_choices_and_old_finished_jobs() {
        let (ledger, db, alice, _) = ledger().await;
        let now = chrono::Utc::now().timestamp();
        ledger
            .create(
                job(
                    "waiting",
                    "alice",
                    &alice,
                    JobState::NeedsSourceChoice,
                    Some("s1"),
                ),
                Some(search("s1", "alice")),
            )
            .await
            .unwrap();
        ledger
            .create(job("done", "alice", &alice, JobState::Done, None), None)
            .await
            .unwrap();
        ledger
            .create(
                job("running", "alice", &alice, JobState::Downloading, None),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            ledger.prune(now + SEARCH_KEPT_SECS - 60).await,
            Ok(Pruned {
                searches: 0,
                dismissed: 0,
                jobs: 0
            })
        );
        let dismissed_at = now + SEARCH_KEPT_SECS + 60;
        assert_eq!(
            ledger.prune(dismissed_at).await,
            Ok(Pruned {
                searches: 1,
                dismissed: 1,
                jobs: 0
            })
        );
        assert_eq!(
            store::job_state(&db, "waiting").await,
            Some(JobState::Dismissed)
        );
        // "waiting" was stamped at dismissed_at, so only "done" is a month old here.
        assert_eq!(
            ledger.prune(now + JOB_KEPT_SECS + 60).await,
            Ok(Pruned {
                searches: 0,
                dismissed: 0,
                jobs: 1
            })
        );
        assert_eq!(store::job_state(&db, "done").await, None);
        assert_eq!(
            store::job_state(&db, "waiting").await,
            Some(JobState::Dismissed)
        );
        assert_eq!(
            ledger.prune(dismissed_at + JOB_KEPT_SECS + 1).await,
            Ok(Pruned {
                searches: 0,
                dismissed: 0,
                jobs: 1
            })
        );
        assert_eq!(store::job_state(&db, "waiting").await, None);
        assert_eq!(
            store::job_state(&db, "running").await,
            Some(JobState::Downloading)
        );
    }

    #[tokio::test]
    async fn a_search_expires_at_its_expiry_and_not_a_second_before() {
        let (ledger, db, _, _) = ledger().await;
        ledger.record_search(search("s1", "alice")).await.unwrap();
        let expires_at = store::search_of(&db, "s1", &UserId::new("alice"))
            .await
            .unwrap()
            .expect("alice's")
            .expires_at;
        assert_eq!(ledger.prune(expires_at - 1).await.unwrap().searches, 0);
        assert_eq!(ledger.prune(expires_at).await.unwrap().searches, 1);
    }

    #[test]
    fn a_job_follows_its_earliest_unfinished_slot_and_ends_with_the_last() {
        use JobState::*;
        for (slots, job) in [
            (vec![Downloading, NeedsDecision, Done], Downloading),
            (vec![Indexing, Importing], Importing),
            (vec![NeedsDecision, Failed], NeedsDecision),
            (vec![Done, Failed, Cancelled], Failed),
            (vec![Done, Dismissed, Cancelled], Done),
            (vec![Cancelled, Dismissed], Cancelled),
            (vec![Dismissed, Dismissed], Dismissed),
        ] {
            assert_eq!(job_state(&slots), job, "{slots:?}");
        }
    }

    fn row_of(
        job: &str,
        slot: u32,
        item: &str,
        state: DownloadState,
        transferred: u64,
    ) -> DownloadProgress {
        DownloadProgress {
            transferred,
            ..DownloadProgress::queued(format!("t-{item}"), "peer_a".into(), item.into(), 100)
                .with_state(state)
                .with_job(JobSlot {
                    job: job.into(),
                    slot,
                })
        }
    }

    #[tokio::test]
    async fn slot_rows_move_the_slot_and_its_job() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![
                (0, item("peer_a", "One", "01.flac")),
                (1, item("peer_a", "Two", "02.flac")),
            ],
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![row_of("j1", 0, "01.flac", DownloadState::InProgress, 10)],
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![row_of(
                "j1",
                1,
                "02.flac",
                DownloadState::Failed("Per-track timeout".into()),
                0,
            )],
        });
        ledger.settled().await;
        assert_eq!(
            store::job_state(&db, "j1").await,
            Some(JobState::Downloading)
        );
        ledger.record(JobEvent::Slots {
            state: JobState::Cancelled,
            rows: vec![row_of("j1", 0, "01.flac", DownloadState::Cancelled, 10)],
        });
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
    }

    #[tokio::test]
    async fn a_slot_that_ended_never_moves_again() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![
                (0, item("peer_a", "One", "01.flac")),
                (1, item("peer_a", "Two", "02.flac")),
            ],
        });
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        ledger.record(JobEvent::Dismiss(vec![slot.clone()]));
        // A later row, a retry's import and an index wait leave the dismissed slot alone.
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![row_of("j1", 0, "01.flac", DownloadState::InProgress, 10)],
        });
        let import = StoredImport {
            sources: vec!["/downloads/b/01.flac".into()],
            target: "/music".into(),
            album: false,
            cleanup_root: None,
            choices: None,
            choice: DriverChoice::Auto,
            rows: vec![(
                slot.clone(),
                row_of("j1", 0, "01.flac", DownloadState::ImportQueued(1), 100),
            )],
        };
        ledger.record(JobEvent::ImportQueued(Box::new(import)));
        ledger.settled().await;
        let states: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT state, last_row, import_job FROM job_tracks WHERE job_id = 'j1' ORDER BY slot",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            states,
            vec![
                ("dismissed".to_string(), None, None),
                ("queued".to_string(), None, None)
            ]
        );
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Queued));
    }

    #[tokio::test]
    async fn a_row_of_another_transfer_is_stored_in_the_same_state() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_a", "One", "01.flac"))],
        });
        let first = row_of("j1", 0, "01.flac", DownloadState::Queued, 0);
        // The slot failed over to peer_b's transfer, which is queued too.
        let next = DownloadProgress {
            id: "t-b01".into(),
            source: "peer_b".into(),
            item: "b/01.flac".into(),
            ..first.clone()
        };
        for row in [first, next.clone()] {
            ledger.record(JobEvent::Slots {
                state: JobState::Downloading,
                rows: vec![row],
            });
        }
        ledger.settled().await;
        assert_eq!(
            store::open_rows(&db, &UserId::new("alice")).await.unwrap(),
            vec![next]
        );
    }

    #[tokio::test]
    async fn progress_ticks_reach_the_disk_only_when_the_state_changes() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_a", "One", "01.flac"))],
        });
        for transferred in [10, 20, 30] {
            ledger.record(JobEvent::Slots {
                state: JobState::Downloading,
                rows: vec![row_of(
                    "j1",
                    0,
                    "01.flac",
                    DownloadState::InProgress,
                    transferred,
                )],
            });
        }
        ledger.settled().await;
        let rows = store::open_rows(&db, &UserId::new("alice")).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| (r.state.clone(), r.transferred))
                .collect::<Vec<_>>(),
            vec![(DownloadState::InProgress, 10)]
        );
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![row_of("j1", 0, "01.flac", DownloadState::Completed, 100)],
        });
        ledger.settled().await;
        let rows = store::open_rows(&db, &UserId::new("alice")).await.unwrap();
        assert_eq!(
            (rows[0].state.clone(), rows[0].transferred),
            (DownloadState::Completed, 100)
        );
    }

    #[tokio::test]
    async fn an_index_change_reaches_the_disk() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_a", "One", "01.flac"))],
        });
        let imported = row_of("j1", 0, "01.flac", DownloadState::Imported, 100);
        let waiting = DownloadProgress {
            index: Some(IndexState::Waiting),
            ..imported.clone()
        };
        for row in [imported, waiting.clone()] {
            ledger.record(JobEvent::Slots {
                state: JobState::Indexing,
                rows: vec![row],
            });
        }
        ledger.settled().await;
        assert_eq!(
            store::open_rows(&db, &UserId::new("alice")).await.unwrap(),
            vec![waiting]
        );
    }

    #[tokio::test]
    async fn open_rows_are_the_rows_of_unfinished_slots() {
        let (ledger, db, alice, _) = ledger().await;
        for id in ["open", "finished"] {
            ledger
                .create(job(id, "alice", &alice, JobState::Queued, None), None)
                .await
                .unwrap();
            ledger.record(JobEvent::Picked {
                job: id.into(),
                items: vec![
                    (0, item("peer_a", "One", "01.flac")),
                    (1, item("peer_a", "Two", "02.flac")),
                    (2, item("peer_a", "Three", "03.flac")),
                ],
            });
            // Every slot has a row, so only the state keeps one out.
            ledger.record(JobEvent::Slots {
                state: JobState::Downloading,
                rows: ["01.flac", "02.flac", "03.flac"]
                    .into_iter()
                    .zip(0..)
                    .map(|(file, slot)| row_of(id, slot, file, DownloadState::InProgress, 10))
                    .collect(),
            });
        }
        ledger.record(JobEvent::Dismiss(vec![JobSlot {
            job: "open".into(),
            slot: 1,
        }]));
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![row_of(
                "open",
                2,
                "03.flac",
                DownloadState::Failed("Per-track timeout".into()),
                10,
            )],
        });
        ledger.record(JobEvent::Ended {
            job: "finished".into(),
            state: JobState::Failed,
        });
        ledger.settled().await;
        let rows = store::open_rows(&db, &UserId::new("alice")).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| (r.item.as_str(), r.job.clone()))
                .collect::<Vec<_>>(),
            vec![(
                "01.flac",
                Some(JobSlot {
                    job: "open".into(),
                    slot: 0
                })
            )]
        );
        assert!(store::open_rows(&db, &UserId::new("bob"))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_import_is_kept_only_for_a_decision_and_an_indexed_one_keeps_its_wait() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(job("j1", "alice", &alice, JobState::Queued, None), None)
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![
                (0, item("peer_a", "One", "01.flac")),
                (1, item("peer_a", "Two", "02.flac")),
            ],
        });
        let import = |slot: u32, file: &str| StoredImport {
            sources: vec![PathBuf::from(format!("/downloads/b/{file}"))],
            target: "/music".into(),
            album: false,
            cleanup_root: None,
            choices: None,
            choice: DriverChoice::Auto,
            rows: vec![(
                JobSlot {
                    job: "j1".into(),
                    slot,
                },
                DownloadProgress::queued(format!("t-{file}"), "peer_a".into(), file.into(), 100)
                    .with_state(DownloadState::ImportQueued(1)),
            )],
        };
        for (slot, file) in [(0, "01.flac"), (1, "02.flac")] {
            ledger.record(JobEvent::ImportQueued(Box::new(import(slot, file))));
        }
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Importing));
        let wait = IndexWait {
            import: "/downloads/b/01.flac".into(),
            library: "/music".into(),
            items: vec![ImportedItem {
                path: "Zzqx Artist/Qzz Album/01 One.flac".into(),
                recording_mbid: None,
                artist: "Zzqx Artist".into(),
                title: "One".into(),
                album: "Qzz Album".into(),
                beets_id: None,
                release_mbid: None,
                match_per_mille: None,
            }],
            since: 0,
            baseline: None,
            checked: None,
        };
        ledger.record(JobEvent::ImportDone(Box::new(ImportDone {
            import: import(0, "01.flac"),
            rows: vec![row_of("j1", 0, "01.flac", DownloadState::Imported, 100)],
            next: JobState::Indexing,
            index: Some(wait.clone()),
        })));
        ledger.record(JobEvent::ImportDone(Box::new(ImportDone {
            import: import(1, "02.flac"),
            rows: vec![row_of(
                "j1",
                1,
                "02.flac",
                DownloadState::ImportSkipped,
                100,
            )],
            next: JobState::NeedsDecision,
            index: None,
        })));
        ledger.settled().await;
        let slots: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT state, import_job, index_wait FROM job_tracks WHERE job_id = 'j1' ORDER BY slot",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        let parsed = slots
            .iter()
            .map(|(state, import, wait)| {
                (
                    state.as_str(),
                    import.as_deref().map(|j| serde_json::from_str(j).unwrap()),
                    wait.as_deref().map(|j| serde_json::from_str(j).unwrap()),
                )
            })
            .collect::<Vec<(&str, Option<StoredImport>, Option<IndexWait>)>>();
        assert_eq!(
            parsed,
            vec![
                ("indexing", None, Some(wait)),
                ("needs_decision", Some(import(1, "02.flac")), None),
            ]
        );
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Indexing));
        let rows = store::open_rows(&db, &UserId::new("alice")).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.state.clone()).collect::<Vec<_>>(),
            vec![DownloadState::Imported, DownloadState::ImportSkipped]
        );
    }

    #[tokio::test]
    async fn an_unreadable_slot_ends_failed() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(
                NewJob {
                    picked: vec![
                        (0, item("peer_a", "One", "01.flac")),
                        (1, item("peer_a", "Two", "02.flac")),
                    ],
                    ..job("j1", "alice", &alice, JobState::Queued, None)
                },
                None,
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Unreadable(vec![JobSlot {
            job: "j1".into(),
            slot: 0,
        }]));
        ledger.settled().await;
        let states: Vec<String> =
            sqlx::query_scalar("SELECT state FROM job_tracks WHERE job_id = 'j1' ORDER BY slot")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(states, vec!["failed", "queued"]);
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Queued));
    }

    #[tokio::test]
    async fn a_recording_ledger_answers_requests_at_once_and_keeps_only_events() {
        let (ledger, mut recorded) = Ledger::recording();
        ledger
            .create(job("j1", "alice", "f1", JobState::Queued, None), None)
            .await
            .unwrap();
        assert_eq!(
            ledger
                .claim_source_choice(&UserId::new("alice"), "s1", "f1", vec![])
                .await,
            Ok(None)
        );
        assert_eq!(ledger.prune(0).await, Ok(Pruned::default()));
        ledger.settled().await;
        let ended = JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        };
        ledger.record(ended.clone());
        assert_eq!(recorded.events(), vec![ended]);
        assert!(recorded.events().is_empty());
    }

    #[test]
    fn every_state_has_its_design_name() {
        use JobState::*;
        for (state, name, terminal) in [
            (Searching, "searching", false),
            (NeedsSourceChoice, "needs_source_choice", false),
            (Queued, "queued", false),
            (Downloading, "downloading", false),
            (Importing, "importing", false),
            (Indexing, "indexing", false),
            (NeedsDecision, "needs_decision", false),
            (Done, "done", true),
            (Failed, "failed", true),
            (Cancelled, "cancelled", true),
            (Dismissed, "dismissed", true),
        ] {
            assert_eq!(
                (state.as_str(), JobState::parse(name), state.is_terminal()),
                (name, Some(state), terminal)
            );
        }
        let names: Vec<String> = TERMINAL
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect();
        assert_eq!(TERMINAL_SQL, format!("({})", names.join(", ")));
    }

    fn wanted_track(artist: &str, title: &str, mbid: Option<&str>) -> DownloadQuery {
        DownloadQuery::new(vec![shared::metadata::Track {
            id: String::new(),
            title: title.into(),
            artist: artist.into(),
            album_id: None,
            album_title: None,
            release_date: None,
            duration: Some("4:23".into()),
            mbid: mbid.map(str::to_string),
            release_mbid: None,
        }])
    }

    fn mcp_job(id: &str, folder: &str, wanted: DownloadQuery) -> NewJob {
        NewJob {
            wanted,
            kind: JobKind::Track,
            options: Some(JobOptions {
                pick: Pick::Auto,
                min_score: 0.7,
                filters: SourceFilters::default(),
                max_sources: 4,
            }),
            ..job(id, "alice", folder, JobState::Searching, None)
        }
    }

    fn batch_of(id: &str) -> NewJobBatch {
        NewJobBatch {
            id: id.into(),
            user: UserId::new("alice"),
            label: Some("Christmas".into()),
            origin: BatchOrigin::Mcp,
        }
    }

    /// `n` queued MCP jobs of alice, each for its own carol, under new uuids.
    fn queued_carols(n: usize, folder: &str) -> Vec<NewJob> {
        (0..n)
            .map(|n| NewJob {
                state: JobState::Queued,
                ..mcp_job(
                    &uuid::Uuid::new_v4().to_string(),
                    folder,
                    wanted_track("Marrow Choir", &format!("Carol {n}"), None),
                )
            })
            .collect()
    }

    #[test]
    fn a_recording_is_keyed_by_its_mbid_and_its_normalized_names() {
        let keys = recording_keys(
            &wanted_track(
                "Marrow Choir",
                "O Holy Night!",
                Some("0A2F7E4B-6C1D-4E8A-9B3F-5D7C1E2A4B6C"),
            ),
            JobKind::Track,
        );
        assert_eq!(
            keys.mbid.as_deref(),
            Some("recording:0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c")
        );
        assert_eq!(
            keys,
            recording_keys(
                &wanted_track(
                    "marrow choir",
                    "O Holy Night",
                    Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c")
                ),
                JobKind::Track
            )
        );
        assert_eq!(
            recording_keys(
                &wanted_track("Marrow Choir", "O Holy Night", None),
                JobKind::Track
            )
            .mbid,
            None
        );
    }

    #[test]
    fn an_album_is_keyed_by_its_release() {
        let album = shared::metadata::Album {
            id: "r1".into(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: Some("5D7C1E2A-4B6C-4E8A-9B3F-0A2F7E4B6C1D".into()),
            cover_url: None,
            release_types: None,
        };
        let keys = recording_keys(&DownloadQuery::new(vec![]).album(album), JobKind::Album);
        assert_eq!(
            keys,
            RecordingKeys {
                mbid: Some("release:5d7c1e2a-4b6c-4e8a-9b3f-0a2f7e4b6c1d".into()),
                text: Some("album:marrow choir\u{1f}winter songs".into()),
            }
        );
    }

    #[tokio::test]
    async fn two_requests_for_one_recording_make_one_job() {
        let (ledger, db, alice, _) = ledger().await;
        let night = || wanted_track("Marrow Choir", "O Holy Night", None);
        let (a, b) = tokio::join!(
            ledger.create_batch(batch_of("b1"), vec![mcp_job("j1", &alice, night())]),
            ledger.create_batch(batch_of("b2"), vec![mcp_job("j2", &alice, night())])
        );
        let mut answers = vec![a.unwrap().remove(0), b.unwrap().remove(0)];
        answers.sort_by_key(|a| matches!(a, Admitted::AlreadyRunning { .. }));
        assert_eq!(answers[0], Admitted::Created);
        let Admitted::AlreadyRunning { job, state } = &answers[1] else {
            panic!("{answers:?}")
        };
        assert_eq!(state, &JobState::Searching);
        assert_eq!(store::job_state(&db, job).await, Some(JobState::Searching));
        // In one call, the second of two equal items is the first one's.
        let both = ledger
            .create_batch(
                batch_of("b3"),
                vec![
                    mcp_job(
                        "j3",
                        &alice,
                        wanted_track("Marrow Choir", "Silent Night", None),
                    ),
                    mcp_job(
                        "j4",
                        &alice,
                        wanted_track("Marrow Choir", "Silent Night", None),
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            both[1],
            Admitted::AlreadyRunning {
                job: "j3".into(),
                state: JobState::Searching
            }
        );
    }

    #[tokio::test]
    async fn two_takes_with_their_own_mbids_are_two_recordings() {
        let (ledger, _, alice, _) = ledger().await;
        let take = |mbid| wanted_track("Marrow Choir", "O Holy Night", Some(mbid));
        let made = ledger
            .create_batch(
                batch_of("b1"),
                vec![
                    mcp_job(
                        "studio",
                        &alice,
                        take("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"),
                    ),
                    mcp_job("live", &alice, take("9c1d2e3f-4a5b-4c6d-8e7f-0a1b2c3d4e5f")),
                    // No MBID: the names decide.
                    mcp_job(
                        "unknown",
                        &alice,
                        wanted_track("Marrow Choir", "O Holy Night", None),
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            made,
            vec![
                Admitted::Created,
                Admitted::Created,
                Admitted::AlreadyRunning {
                    job: "studio".into(),
                    state: JobState::Searching
                }
            ]
        );
    }

    #[tokio::test]
    async fn a_batch_whose_every_item_runs_already_is_not_written() {
        let (ledger, db, alice, _) = ledger().await;
        let night = || wanted_track("Marrow Choir", "O Holy Night", None);
        ledger
            .create_batch(batch_of("b1"), vec![mcp_job("j1", &alice, night())])
            .await
            .unwrap();
        ledger
            .create_batch(batch_of("b2"), vec![mcp_job("j2", &alice, night())])
            .await
            .unwrap();
        let batches: Vec<(String, Option<String>, String)> =
            sqlx::query_as("SELECT id, label, origin FROM batches ORDER BY id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            batches,
            vec![("b1".into(), Some("Christmas".into()), "mcp".into())]
        );
    }

    #[tokio::test]
    async fn a_web_ui_job_blocks_an_mcp_request_until_it_ends() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create(
                NewJob {
                    wanted: wanted_track("Marrow Choir", "O Holy Night", None),
                    ..job("ui", "alice", &alice, JobState::Queued, None)
                },
                None,
            )
            .await
            .unwrap();
        let first = ledger
            .create_batch(
                batch_of("b1"),
                vec![mcp_job(
                    "j1",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        assert_eq!(
            first,
            vec![Admitted::AlreadyRunning {
                job: "ui".into(),
                state: JobState::Queued
            }]
        );
        ledger.record(JobEvent::Ended {
            job: "ui".into(),
            state: JobState::Failed,
        });
        let again = ledger
            .create_batch(
                batch_of("b2"),
                vec![mcp_job(
                    "j2",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        assert_eq!(again, vec![Admitted::Created]);
        // The web UI job is a batch of its own, named by what it asked for.
        let batch: (String, Option<String>, String, String) = sqlx::query_as(
            "SELECT b.id, b.label, b.origin, j.kind FROM jobs j JOIN batches b ON b.id = j.batch_id \
             WHERE j.id = 'ui'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            batch,
            (
                "ui".into(),
                Some("O Holy Night".into()),
                "ui".into(),
                "track".into()
            )
        );
    }

    #[tokio::test]
    async fn a_user_at_the_bound_of_unfinished_jobs_gets_no_more() {
        let (ledger, _, alice, _) = ledger().await;
        let jobs = queued_carols(MCP_UNFINISHED_JOBS_PER_USER as usize, &alice);
        let first = jobs[0].id.clone();
        let at_bound = ledger.create_batch(batch_of("b1"), jobs).await.unwrap();
        assert!(at_bound.iter().all(|a| a == &Admitted::Created));
        let next = || {
            vec![mcp_job(
                &uuid::Uuid::new_v4().to_string(),
                &alice,
                wanted_track("Marrow Choir", "O Holy Night", None),
            )]
        };
        assert_eq!(
            ledger.create_batch(batch_of("b2"), next()).await.unwrap(),
            vec![Admitted::OverLimit {
                unfinished: MCP_UNFINISHED_JOBS_PER_USER
            }]
        );
        ledger.record(JobEvent::Ended {
            job: first,
            state: JobState::Failed,
        });
        assert_eq!(
            ledger.create_batch(batch_of("b3"), next()).await.unwrap(),
            vec![Admitted::Created]
        );
    }

    #[tokio::test]
    async fn only_one_transition_wins_a_race() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![mcp_job(
                    "j1",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        });
        let user = UserId::new("alice");
        let (a, b) = tokio::join!(
            ledger.transition(
                &user,
                "j1",
                Transition::Search {
                    search_id: "s2".into(),
                    filters: None
                }
            ),
            ledger.transition(&user, "j1", Transition::Dismiss)
        );
        let mut got = vec![a.unwrap(), b.unwrap()];
        got.sort_by_key(|t| matches!(t, Transitioned::Refused { .. }));
        assert!(
            matches!(
                got[0],
                Transitioned::Applied {
                    from: JobState::Failed,
                    to: JobState::Searching | JobState::Dismissed,
                    ..
                }
            ),
            "{got:?}"
        );
        assert!(
            matches!(
                got[1],
                Transitioned::Refused {
                    state: JobState::Searching | JobState::Dismissed
                }
            ),
            "{got:?}"
        );
        assert_eq!(
            ledger
                .transition(&UserId::new("bob"), "j1", Transition::Dismiss)
                .await
                .unwrap(),
            Transitioned::Unknown
        );
        assert!(matches!(
            store::job_state(&db, "j1").await,
            Some(JobState::Searching | JobState::Dismissed)
        ));
    }

    #[tokio::test]
    async fn a_new_search_sets_the_slots_aside_and_the_next_pick_reopens_them() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![mcp_job(
                    "j1",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
        });
        ledger.record(JobEvent::Enqueued {
            job: "j1".into(),
            monitor_batch_id: "m1".into(),
            batch_label: "O Holy Night".into(),
            request: "r1".into(),
        });
        ledger.record(JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        });
        let user = UserId::new("alice");
        let filters = SourceFilters {
            formats: vec!["flac".into()],
            ..SourceFilters::default()
        };
        ledger
            .transition(
                &user,
                "j1",
                Transition::Search {
                    search_id: "s2".into(),
                    filters: Some(filters.clone()),
                },
            )
            .await
            .unwrap();
        let (search, sent, options): (Option<String>, Option<String>, String) =
            sqlx::query_as("SELECT search_id, monitor_batch_id, options FROM jobs WHERE id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((search.as_deref(), sent), (Some("s2"), None));
        assert_eq!(
            serde_json::from_str::<JobOptions>(&options)
                .unwrap()
                .filters,
            filters
        );
        // A late event of the set-aside slot leaves the job searching.
        ledger.record(JobEvent::Dismiss(vec![JobSlot {
            job: "j1".into(),
            slot: 0,
        }]));
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Searching));
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_b", "O Holy Night", "b/03.flac"))],
        });
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Queued));
        let attempts: Vec<(u32, Option<String>)> =
            sqlx::query_as("SELECT n, source FROM job_attempts WHERE job_id = 'j1' ORDER BY n")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            attempts,
            vec![(1, Some("peer_a".into())), (2, Some("peer_b".into()))]
        );
        let pick_n: u32 = sqlx::query_scalar("SELECT pick_n FROM job_tracks WHERE job_id = 'j1'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(pick_n, 2);
    }

    #[tokio::test]
    async fn a_user_holds_at_most_the_limit_of_download_places() {
        let (ledger, _, alice, _) = ledger().await;
        let jobs = queued_carols(3, &alice);
        let ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
        ledger.create_batch(batch_of("b1"), jobs).await.unwrap();
        assert!(ledger.claim_download(&ids[0], 2).await.unwrap());
        assert!(
            ledger.claim_download(&ids[0], 2).await.unwrap(),
            "a held place is held"
        );
        assert!(ledger.claim_download(&ids[1], 2).await.unwrap());
        assert!(!ledger.claim_download(&ids[2], 2).await.unwrap());
        ledger.record(JobEvent::Ended {
            job: ids[0].clone(),
            state: JobState::Failed,
        });
        assert!(
            ledger.claim_download(&ids[2], 2).await.unwrap(),
            "a place frees when its job moves on"
        );
    }

    #[tokio::test]
    async fn a_job_that_failed_holding_a_place_queues_again_without_it() {
        let (ledger, db, alice, _) = ledger().await;
        let jobs = queued_carols(3, &alice);
        let ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
        ledger.create_batch(batch_of("b1"), jobs).await.unwrap();
        assert!(ledger.claim_download(&ids[0], 2).await.unwrap());
        ledger.record(JobEvent::Ended {
            job: ids[0].clone(),
            state: JobState::Failed,
        });
        let user = UserId::new("alice");
        ledger
            .transition(
                &user,
                &ids[0],
                Transition::Search {
                    search_id: "s2".into(),
                    filters: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            ledger
                .transition(
                    &user,
                    &ids[0],
                    Transition::Pick {
                        items: vec![(0, item("peer_b", "Carol 0", "b/01.flac"))]
                    }
                )
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Searching,
                to: JobState::Queued,
                ..
            }
        ));
        assert!(ledger.claim_download(&ids[1], 2).await.unwrap());
        assert!(ledger.claim_download(&ids[2], 2).await.unwrap());
        assert!(
            !ledger.claim_download(&ids[0], 2).await.unwrap(),
            "the limit is reached and the job holds no stale place"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT gate_held FROM jobs WHERE id = ?")
                .bind(&ids[0])
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_cancel_ends_a_job_without_transfers_and_marks_one_that_holds_a_place() {
        let (ledger, db, alice, _) = ledger().await;
        let jobs = queued_carols(2, &alice);
        let (idle, placed) = (jobs[0].id.clone(), jobs[1].id.clone());
        ledger.create_batch(batch_of("b1"), jobs).await.unwrap();
        let user = UserId::new("alice");
        let marks = TransferRegistry::global();
        assert!(matches!(
            ledger
                .transition(&user, &idle, Transition::Cancel)
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Queued,
                to: JobState::Cancelled,
                ..
            }
        ));
        assert!(!marks.job_cancelled(&idle));
        assert!(ledger.claim_download(&placed, 6).await.unwrap());
        assert!(matches!(
            ledger
                .transition(&user, &placed, Transition::Cancel)
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Queued,
                to: JobState::Queued,
                ..
            }
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT cancel_requested FROM jobs WHERE id = ?")
                .bind(&placed)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        assert!(marks.job_cancelled(&placed));
        // Its enqueue then failed: the user still cancelled it.
        let failed_enqueue = crate::server_fns::download::enqueue::nothing_enqueued();
        assert!(matches!(
            ledger
                .transition(
                    &user,
                    &placed,
                    Transition::Fail {
                        failure: failed_enqueue
                    }
                )
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Queued,
                to: JobState::Cancelled,
                ..
            }
        ));
        assert!(!marks.job_cancelled(&placed), "an ended job keeps no mark");
    }

    #[tokio::test]
    async fn the_runner_cannot_bring_back_a_job_cancelled_while_it_searched() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![mcp_job(
                    "j1",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        let user = UserId::new("alice");
        ledger
            .transition(&user, "j1", Transition::Cancel)
            .await
            .unwrap();
        for change in [
            Transition::Pick {
                items: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
            },
            Transition::AwaitChoice,
            // Soulful's own text for an empty search (core/sources.rs).
            Transition::Fail {
                failure: Failure::new(
                    FailureCode::SearchNoResults,
                    Stage::SourceSearch,
                    RetryAdvice::OtherQuery,
                    "No results found",
                ),
            },
        ] {
            assert_eq!(
                ledger.transition(&user, "j1", change).await.unwrap(),
                Transitioned::Refused {
                    state: JobState::Cancelled
                }
            );
        }
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Cancelled));
    }

    #[tokio::test]
    async fn every_job_change_moves_the_change_number_on() {
        let (ledger, _, alice, _) = ledger().await;
        let mut changes = ledger.changes();
        let before = *changes.borrow_and_update();
        ledger
            .create_batch(
                batch_of("b1"),
                vec![mcp_job(
                    "j1",
                    &alice,
                    wanted_track("Marrow Choir", "O Holy Night", None),
                )],
            )
            .await
            .unwrap();
        changes.changed().await.unwrap();
        let created = *changes.borrow_and_update();
        assert!(created > before);
        let applied = ledger
            .transition(&UserId::new("alice"), "j1", Transition::AwaitChoice)
            .await
            .unwrap();
        let Transitioned::Applied { seq, .. } = applied else {
            panic!("{applied:?}")
        };
        assert!(seq > created);
        assert_eq!(*changes.borrow(), seq);
    }

    #[tokio::test]
    async fn a_failed_search_is_the_jobs_and_a_failed_send_is_its_slots() {
        let (ledger, db, alice, _) = ledger().await;
        let jobs = vec![
            mcp_job(
                "searched",
                &alice,
                wanted_track("Marrow Choir", "Carol 0", None),
            ),
            NewJob {
                state: JobState::Queued,
                picked: vec![(0, item("peer_a", "Carol 1", "a/01.flac"))],
                ..mcp_job(
                    "sent",
                    &alice,
                    wanted_track("Marrow Choir", "Carol 1", None),
                )
            },
        ];
        ledger.create_batch(batch_of("b1"), jobs).await.unwrap();
        let user = UserId::new("alice");
        let none_found = Failure::new(
            FailureCode::SearchNoResults,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            "No results found",
        );
        let unsent = crate::server_fns::download::enqueue::nothing_enqueued();
        for (job, failure) in [("searched", &none_found), ("sent", &unsent)] {
            let failure = failure.clone();
            assert!(matches!(
                ledger
                    .transition(&user, job, Transition::Fail { failure })
                    .await
                    .unwrap(),
                Transitioned::Applied {
                    to: JobState::Failed,
                    ..
                }
            ));
        }
        let failure: Option<String> =
            sqlx::query_scalar("SELECT failure FROM jobs WHERE id = 'searched'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Failure>(&failure.unwrap()).unwrap(),
            none_found
        );
        let (state, row): (String, String) =
            sqlx::query_as("SELECT state, last_row FROM job_tracks WHERE job_id = 'sent'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(
            serde_json::from_str::<DownloadProgress>(&row).unwrap(),
            DownloadProgress::failed(
                "a/01.flac".into(),
                "peer_a".into(),
                "a/01.flac".into(),
                unsent
            )
        );
        let failure: Option<String> =
            sqlx::query_scalar("SELECT failure FROM jobs WHERE id = 'sent'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(failure, None);
    }

    #[tokio::test]
    async fn a_failed_job_queued_again_takes_its_next_attempt_and_a_new_send() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![NewJob {
                    state: JobState::Queued,
                    picked: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
                    ..mcp_job(
                        "j1",
                        &alice,
                        wanted_track("Marrow Choir", "O Holy Night", None),
                    )
                }],
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Enqueued {
            job: "j1".into(),
            monitor_batch_id: "m1".into(),
            batch_label: "O Holy Night".into(),
            request: "r1".into(),
        });
        ledger.record(JobEvent::Ended {
            job: "j1".into(),
            state: JobState::Failed,
        });
        let applied = ledger
            .transition(
                &UserId::new("alice"),
                "j1",
                Transition::Queue {
                    items: vec![(0, item("peer_b", "O Holy Night", "b/03.flac"))],
                    search_id: Some("s2".into()),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            applied,
            Transitioned::Applied {
                from: JobState::Failed,
                to: JobState::Queued,
                ..
            }
        ));
        let job: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT state, search_id, request FROM jobs WHERE id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(job, ("queued".into(), Some("s2".into()), None));
        let slots = store::unfinished(&db).await.unwrap().jobs.remove(0).slots;
        assert_eq!(
            slots
                .iter()
                .map(|s| (s.state, s.attempts.len()))
                .collect::<Vec<_>>(),
            vec![(JobState::Queued, 2)]
        );
    }

    #[tokio::test]
    async fn a_rejected_result_fails_every_slot_and_excludes_its_source() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![NewJob {
                    state: JobState::Queued,
                    picked: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
                    ..mcp_job(
                        "j1",
                        &alice,
                        wanted_track("Marrow Choir", "O Holy Night", None),
                    )
                }],
            )
            .await
            .unwrap();
        ledger.record(JobEvent::Slots {
            state: JobState::Done,
            rows: vec![row_of("j1", 0, "a/03.flac", DownloadState::Imported, 100)],
        });
        let rejected = Failure::new(
            FailureCode::ResultRejected,
            Stage::Import,
            RetryAdvice::OtherSource,
            "Rejected by the user",
        );
        assert!(matches!(
            ledger
                .transition(
                    &UserId::new("alice"),
                    "j1",
                    Transition::Reject {
                        source: "peer_a".into(),
                        failure: rejected.clone(),
                    },
                )
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Done,
                to: JobState::Failed,
                ..
            }
        ));
        let excluded: String =
            sqlx::query_scalar("SELECT excluded_sources FROM jobs WHERE id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&excluded).unwrap(),
            vec!["peer_a".to_string()]
        );
        let (state, row): (String, String) =
            sqlx::query_as("SELECT state, last_row FROM job_tracks WHERE job_id = 'j1'")
                .fetch_one(&db)
                .await
                .unwrap();
        let row: DownloadProgress = serde_json::from_str(&row).unwrap();
        assert_eq!((state.as_str(), row.failure), ("failed", Some(rejected)));
    }

    #[tokio::test]
    async fn a_reindex_sends_the_slots_lost_at_the_index_back_to_wait_from_now() {
        let (ledger, db, alice, _) = ledger().await;
        ledger
            .create_batch(
                batch_of("b1"),
                vec![NewJob {
                    state: JobState::Queued,
                    picked: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
                    ..mcp_job(
                        "j1",
                        &alice,
                        wanted_track("Marrow Choir", "O Holy Night", None),
                    )
                }],
            )
            .await
            .unwrap();
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        let imported = row_of("j1", 0, "a/03.flac", DownloadState::Imported, 100);
        let wait = IndexWait {
            import: "/downloads/a/03.flac".into(),
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
            since: 10,
            baseline: Some(ScanBaseline {
                last_scan: chrono::DateTime::from_timestamp(20, 0).unwrap(),
                started: chrono::DateTime::from_timestamp(15, 0).unwrap(),
            }),
            checked: chrono::DateTime::from_timestamp(30, 0),
        };
        ledger.record(JobEvent::Indexing {
            slots: vec![slot.clone()],
            wait: Box::new(wait.clone()),
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![imported.failed_with(crate::core::index::not_found_after_scan())],
        });
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
        assert!(matches!(
            ledger
                .transition(&UserId::new("alice"), "j1", Transition::Reindex { now: 77 })
                .await
                .unwrap(),
            Transitioned::Applied {
                from: JobState::Failed,
                to: JobState::Indexing,
                ..
            }
        ));
        let waiting = store::indexing_slots(&db).await.unwrap().slots;
        assert_eq!(
            waiting
                .iter()
                .map(|s| (&s.slot, &s.wait))
                .collect::<Vec<_>>(),
            vec![(
                &slot,
                &IndexWait {
                    since: 77,
                    baseline: None,
                    checked: None,
                    ..wait
                }
            )]
        );
    }
}
