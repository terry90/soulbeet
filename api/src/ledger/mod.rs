//! The job ledger: every download request as a job with its track slots, and
//! the source searches jobs come from, kept in SQLite so they survive a
//! restart. Every write goes through one writer task, so writes apply in the
//! order they were made; reads go to the pool (`store`).

use crate::auth::UserId;
use shared::download::{DownloadQuery, DownloadableGroup};
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use std::sync::{LazyLock, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

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
}

pub struct NewSearch {
    pub id: String,
    pub user: UserId,
    pub backend: Option<String>,
    pub query: DownloadQuery,
}

/// What one prune removed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Ledger {
    fn channel() -> (Ledger, mpsc::UnboundedReceiver<Write>) {
        let (writes, inbox) = mpsc::unbounded_channel();
        (Ledger { writes }, inbox)
    }

    /// A ledger whose writer runs on `db` (tests, and start_server_ledger).
    pub fn start(db: SqlitePool) -> Ledger {
        let (ledger, inbox) = Ledger::channel();
        writer::spawn(db, inbox);
        ledger
    }

    /// Sends the write `make` builds around its reply and waits for the answer.
    async fn write<T>(&self, make: impl FnOnce(Reply<T>) -> Write) -> Result<T, LedgerError> {
        let stopped = || LedgerError("the job ledger's writer has stopped".into());
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

    /// The job of `user` waiting for a source from `search_id` into
    /// `folder_id`, now queued; None when there is none. One conditional
    /// write in the writer, so of two claims only one gets the job.
    pub async fn claim_source_choice(
        &self,
        user: &UserId,
        search_id: &str,
        folder_id: &str,
    ) -> Result<Option<ClaimedJob>, LedgerError> {
        let (user, search_id, folder_id) =
            (user.clone(), search_id.to_string(), folder_id.to_string());
        self.write(|reply| Write::ClaimSourceChoice {
            user,
            search_id,
            folder_id,
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
        loop {
            every.tick().await;
            // The writer logs a prune the database refused.
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
        }
    }

    fn search(id: &str, user: &str) -> NewSearch {
        NewSearch {
            id: id.into(),
            user: UserId::new(user),
            backend: Some("soulseek".into()),
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
        let (a, b) = tokio::join!(
            ledger.claim_source_choice(&user, "s1", &alice),
            ledger.claim_source_choice(&user, "s1", &alice)
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
    }

    #[tokio::test]
    async fn a_pick_continues_only_the_users_own_job_into_its_folder() {
        let (ledger, _, alice, bob) = ledger().await;
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
                .claim_source_choice(&UserId::new("bob"), "s1", &bob)
                .await,
            Ok(None)
        );
        assert_eq!(
            ledger
                .claim_source_choice(&UserId::new("alice"), "s1", &bob)
                .await,
            Ok(None)
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
    }
}
