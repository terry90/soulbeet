//! The job ledger: every download request as a job with its track slots, and
//! the source searches jobs come from, kept in SQLite so they survive a
//! restart. Every write goes through one writer task, so writes apply in the
//! order they were made; reads go to the pool (`store`).

use crate::auth::UserId;
use serde::{Deserialize, Serialize};
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadableGroup, DownloadableItem, JobSlot, MatchChoices,
};
use soulbeet::{DriverChoice, ImportMode, ImportedItem};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

pub mod store;
mod writer;

use writer::{Reply, Write};

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
        candidate: Option<DownloadableItem>,
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
    /// No writer reads `writes`: requests are answered at once, events wait for `Recorded`.
    #[cfg(test)]
    recording: bool,
}

impl Ledger {
    fn channel() -> (Ledger, mpsc::UnboundedReceiver<Write>) {
        let (writes, inbox) = mpsc::unbounded_channel();
        let ledger = Ledger {
            writes,
            #[cfg(test)]
            recording: false,
        };
        (ledger, inbox)
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
        (ledger, Recorded { inbox })
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
        writer::spawn(db, inbox);
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

    /// Records `job`, and first the search it came from when given.
    pub async fn create(&self, job: NewJob, search: Option<NewSearch>) -> Result<(), LedgerError> {
        self.write(|reply| Write::Create {
            job: Box::new(job),
            search: search.map(Box::new),
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
    /// updated_at < now - JOB_KEPT_SECS.
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
static SERVER_INBOX: Mutex<Option<mpsc::UnboundedReceiver<Write>>> = Mutex::new(None);

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
    writer::spawn((*crate::db::DB).clone(), inbox);
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
}
