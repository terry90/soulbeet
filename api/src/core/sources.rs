//! Download source search: one backend's search started and polled for a
//! user, the every-backend ranked search an auto-download runs, and the MCP
//! searches, stored in the ledger, run one at a time and read as candidates.

use crate::auth::{UserCtx, UserId};
use crate::core::{backend_unavailable, database_failure, metadata};
use crate::db::DB;
use crate::ledger::store::{self, StoredSearch};
use crate::ledger::{
    Ledger, NewSearch, SearchOutcome, SourceFilters, StoredSpec, LEDGER, SEARCH_KEPT_SECS,
};
use crate::services::{
    available_download_backends, download_backend, BackendLookupError, DEFAULT_DOWNLOAD_BACKEND,
};
use dioxus::logger::tracing::warn;
use serde::Serialize;
use shared::download::{
    DownloadQuery, DownloadableGroup, DownloadableItem, FileFacts, SearchResult, SearchSpec,
    SearchState, SearchStats,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{Album, Track};
use soulbeet::DownloadBackend;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Maximum time to wait for search results from a single backend. A safety
/// net above the slskd client's own deadline, which cancels a search at 120
/// seconds and gives it 15 more to end.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Poll interval when waiting for search results.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A backend an auto-download searches, by its id in the settings.
pub type Searched = (String, Arc<dyn DownloadBackend>);

/// The search does not exist or is another user's.
pub fn search_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Search not found",
    )
}

/// The search was found, and is older than SEARCH_KEPT_SECS.
pub fn search_expired() -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Search expired",
    )
}

/// `stored` when it exists and has not expired at `now`.
pub(crate) fn live_search(stored: Option<StoredSearch>, now: i64) -> Result<StoredSearch, Failure> {
    let stored = stored.ok_or_else(search_not_found)?;
    if stored.expires_at <= now {
        return Err(search_expired());
    }
    Ok(stored)
}

/// `ctx`'s search `id`, while it has not expired. Another user's search
/// reads like a missing one.
pub(crate) async fn owned_search(
    db: &SqlitePool,
    ctx: &UserCtx,
    id: &str,
) -> Result<StoredSearch, Failure> {
    let stored = store::search_of(db, id, &ctx.id())
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    live_search(stored, chrono::Utc::now().timestamp())
}

/// Starts a search on the named backend, or the default one, for `query`
/// with its album's tracklist, and records it in the ledger as `ctx`'s.
pub async fn start(ctx: &UserCtx, query: DownloadQuery) -> Result<String, Failure> {
    let key = metadata::lastfm_key(ctx).await?;
    let mut query = query;
    metadata::hydrate_album_tracks(&mut query, key.as_deref()).await?;

    // Stored by the id the backend is looked up by, so a poll finds it again.
    let backend_id = query
        .backend
        .clone()
        .unwrap_or_else(|| DEFAULT_DOWNLOAD_BACKEND.to_string());
    let backend = download_backend(Some(&backend_id))
        .await
        .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))?;

    let search_id = backend
        .start_search(&SearchSpec::of(query.album.clone(), query.tracks.clone()))
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    LEDGER
        .record_search(NewSearch {
            id: search_id.clone(),
            user: ctx.id(),
            backend: Some(backend_id),
            query,
            spec: None,
        })
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    Ok(search_id)
}

/// The state and groups of `ctx`'s search `search_id`, on the backend it
/// was started on. A search that ended stores its groups as its candidates.
pub async fn poll(ctx: &UserCtx, search_id: &str) -> Result<SearchResult, Failure> {
    let stored = owned_search(&DB, ctx, search_id).await?;
    let backend = download_backend(stored.backend.as_deref())
        .await
        .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))?;

    let result = backend
        .poll_search(search_id)
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    if result.state != SearchState::InProgress {
        LEDGER
            .store_candidates(search_id, result.groups.clone())
            .await
            .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    }
    Ok(result)
}

/// How one backend's search ended.
enum SearchStop {
    /// The backend finished the search, having read what `stats` says.
    Finished { stats: SearchStats },
    /// SEARCH_TIMEOUT passed before the backend finished.
    TimedOut,
    /// Starting or polling the search failed.
    Failed(Failure),
}

/// One backend's search for `spec`, `id` naming it in the logs: the groups it
/// found and how it ended.
async fn search_backend(
    id: &str,
    backend: &Arc<dyn DownloadBackend>,
    spec: &SearchSpec,
) -> (Vec<DownloadableGroup>, SearchStop) {
    match start_backend_search(id, backend, spec).await {
        Ok(search_id) => poll_backend_search(id, backend, &search_id).await,
        Err(stop) => (Vec::new(), stop),
    }
}

/// Starts one backend's search for `spec`: the backend's id of it, or how it
/// ended when the backend refused it.
async fn start_backend_search(
    id: &str,
    backend: &Arc<dyn DownloadBackend>,
    spec: &SearchSpec,
) -> Result<String, SearchStop> {
    backend.start_search(spec).await.map_err(|e| {
        warn!("Backend {} search start failed: {}", id, e);
        SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()))
    })
}

/// Polls the backend's search `search_id` until it ends: the groups it found
/// and how it ended.
async fn poll_backend_search(
    id: &str,
    backend: &Arc<dyn DownloadBackend>,
    search_id: &str,
) -> (Vec<DownloadableGroup>, SearchStop) {
    // Poll until results are ready or timeout. A backend may send
    // groups while the search is in progress, so keep the latest
    // non-empty batch. slskd sends its groups only in the completing
    // poll.
    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
    let mut latest_groups = Vec::<DownloadableGroup>::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            warn!("Backend {} search timed out", id);
            return (latest_groups, SearchStop::TimedOut);
        }
        tokio::time::sleep(SEARCH_POLL_INTERVAL).await;

        match backend.poll_search(search_id).await {
            Ok(result) => match result.state {
                SearchState::Completed | SearchState::TimedOut => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                    let stop = SearchStop::Finished {
                        stats: result.stats,
                    };
                    return (latest_groups, stop);
                }
                SearchState::InProgress => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                }
            },
            Err(e) => {
                warn!("Backend {} poll error: {}", id, e);
                let stop = SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()));
                return (latest_groups, stop);
            }
        }
    }
}

/// Why a search of `first` and `rest` found nothing: when any backend
/// finished, an empty search, or one whose files were all scored out when its
/// responses held files; else a timeout when any timed out, else the first
/// backend's own failure.
fn no_results_failure(first: &SearchStop, rest: &[SearchStop]) -> Failure {
    const MESSAGE: &str = "No results found";
    let rank = |stop: &SearchStop| match stop {
        SearchStop::Finished { .. } => 2,
        SearchStop::TimedOut => 1,
        SearchStop::Failed(_) => 0,
    };
    // The earliest of the highest rank.
    let stop = rest
        .iter()
        .fold(first, |best, s| if rank(s) > rank(best) { s } else { best });
    match stop {
        SearchStop::Finished {
            stats: SearchStats {
                file_count: 1.., ..
            },
        } => Failure::new(
            FailureCode::SearchAllFiltered,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::Finished {
            stats: SearchStats { file_count: 0, .. },
        } => Failure::new(
            FailureCode::SearchNoResults,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::TimedOut => Failure::new(
            FailureCode::SearchTimeout,
            Stage::SourceSearch,
            RetryAdvice::SameSourceLater,
            MESSAGE,
        ),
        SearchStop::Failed(f) => f.clone().with_message(MESSAGE),
    }
}

/// No backend was available, `e` saying why the default one was not.
fn no_backends(e: &BackendLookupError) -> Failure {
    e.failure(Stage::SourceSearch, "No download backends available")
}

/// The backends to search, the default one first when it is available, or
/// the default one's lookup error as `no_backends` when none is.
pub async fn usable_backends() -> Result<(Searched, Vec<Searched>), Failure> {
    let mut others = Vec::new();
    for (id, _) in available_download_backends() {
        if id == DEFAULT_DOWNLOAD_BACKEND {
            continue;
        }
        match download_backend(Some(id)).await {
            Ok(b) => others.push((id.to_string(), b)),
            Err(e) => warn!("Backend {} unavailable for auto-download: {}", id, e),
        }
    }
    match download_backend(None).await {
        Ok(b) => Ok(((DEFAULT_DOWNLOAD_BACKEND.to_string(), b), others)),
        Err(e) => {
            warn!(
                "Backend {} unavailable for auto-download: {}",
                DEFAULT_DOWNLOAD_BACKEND, e
            );
            let mut others = others.into_iter();
            match others.next() {
                Some(first) => Ok((first, others.collect())),
                None => Err(no_backends(&e)),
            }
        }
    }
}

/// Searches every backend at once: all groups found, best score first, or
/// the failure of the stops when none was found.
pub async fn search_ranked(
    first: &Searched,
    rest: &[Searched],
    album: Option<&Album>,
    tracks: &[Track],
) -> Result<Vec<DownloadableGroup>, Failure> {
    let spec = SearchSpec::of(album.cloned(), tracks.to_vec());
    let ((first_groups, first_stop), results) = futures::future::join(
        search_backend(&first.0, &first.1, &spec),
        futures::future::join_all(
            rest.iter()
                .map(|(id, backend)| search_backend(id, backend, &spec)),
        ),
    )
    .await;

    let (groups, stops): (Vec<_>, Vec<_>) = results.into_iter().unzip();
    let mut all_groups: Vec<DownloadableGroup> = first_groups
        .into_iter()
        .chain(groups.into_iter().flatten())
        .collect();
    if all_groups.is_empty() {
        return Err(no_results_failure(&first_stop, &stops));
    }
    best_first(&mut all_groups);
    Ok(all_groups)
}

/// Sorts `groups` by score, best first, keeping the order of equal ones.
fn best_first(groups: &mut [DownloadableGroup]) {
    groups.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// D5: a source whose transfers failed this many times in the last 24 hours is flagged.
pub const FLAGGED_FAILURES: u32 = 3;

/// How far back a source's failed transfers count (D5).
pub const RECENT_FAILURES_SECS: i64 = 24 * 60 * 60;

/// How long find_sources waits for its search inside the call (design 11.4).
pub const FIND_WAIT: Duration = Duration::from_secs(45);

/// The most `find_sources` searches one user may have waiting for the turn; `start_find` past
/// it starts none.
pub const FINDS_WAITING_PER_USER: usize = 3;

/// A search with groups none of which `view` keeps. find_sources and the job runner both
/// answer it.
pub fn no_candidate_passed() -> Failure {
    Failure::new(
        FailureCode::SearchAllFiltered,
        Stage::SourceSearch,
        RetryAdvice::OtherQuery,
        "No candidate passed the filters",
    )
}

/// D9: a job's search stopped before it ran, because the job was cancelled.
pub fn search_stopped() -> Failure {
    Failure::new(
        FailureCode::SearchStopped,
        Stage::SourceSearch,
        RetryAdvice::NotRetryable,
        "The search was stopped because its job was cancelled",
    )
}

/// A stored search that no task runs any more: Soulful restarted while it ran.
pub fn server_restarted_search() -> Failure {
    Failure::new(
        FailureCode::ServerRestarted,
        Stage::SourceSearch,
        RetryAdvice::SameSourceLater,
        "Soulful restarted while this search ran",
    )
}

/// Where a stored search is.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchProgress {
    Running,
    /// Its groups, best first, and what it read.
    Done(Vec<DownloadableGroup>, SearchStats),
    /// Why it found nothing, and what it read when the backend finished it.
    Failed(Failure, Option<SearchStats>),
}

impl SearchProgress {
    /// A stored search with neither candidates nor a failure is running. A web UI search stores
    /// its candidates without stats, which read as all zero.
    pub fn of(stored: &StoredSearch) -> SearchProgress {
        match (&stored.candidates, &stored.failure) {
            (Some(groups), _) => {
                SearchProgress::Done(groups.clone(), stored.stats.clone().unwrap_or_default())
            }
            (None, Some(failure)) => SearchProgress::Failed(failure.clone(), stored.stats.clone()),
            (None, None) => SearchProgress::Running,
        }
    }

    fn of_outcome(outcome: &SearchOutcome) -> SearchProgress {
        match outcome {
            Ok((groups, stats)) => SearchProgress::Done(groups.clone(), stats.clone()),
            Err((failure, stats)) => SearchProgress::Failed(failure.clone(), stats.clone()),
        }
    }
}

/// Who a stored search runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFor {
    /// A `find_sources` call.
    Find,
    /// An MCP job's runner.
    Job,
}

/// A stored search a task of this process runs.
struct Run {
    id: String,
    user: UserId,
    purpose: SearchFor,
    holds_turn: bool,
    /// The backend's id of the search and the backend, once the task started it.
    started: Option<(String, Arc<dyn DownloadBackend>)>,
    /// `Searches::stop` asked for it: it never takes the turn, or is stopped at the backend.
    stop: bool,
}

/// The searches in line, and the ids stopped before a task ran them.
#[derive(Default)]
struct Line {
    /// In arrival order: the first holds the turn once its task took it, the rest wait.
    runs: Vec<Run>,
    /// Ids `Searches::stop` named while no task ran them, with when (unix seconds): a task that
    /// starts one of them afterwards stores it stopped without searching.
    stops: HashMap<String, i64>,
}

/// The MCP searches: one at a time, every MCP search holding the turn while it runs (design
/// 11.5). The server has one, in `Acquire`; each test makes its own. Web UI searches do not wait
/// for it.
#[derive(Default)]
pub struct Searches {
    line: Mutex<Line>,
    /// Notified when a search leaves or is stopped, so the next one takes the turn and a stopped
    /// one stops waiting for it.
    left: Notify,
}

impl Searches {
    /// Whether a task of this process runs stored search `id` now (start_stored registers it for
    /// the task's life). A stored search that reads Running and is not running here was cut off
    /// by a restart.
    pub fn runs(&self, id: &str) -> bool {
        self.line
            .lock()
            .unwrap()
            .runs
            .iter()
            .any(|run| run.id == id)
    }

    /// Waits until no task of this process runs stored search `id`: its task stored how it
    /// ended, or could not.
    pub async fn until_left(&self, id: &str) {
        loop {
            let left = self.left.notified();
            tokio::pin!(left);
            // Registered before the look, so a search leaving after it still wakes this one.
            left.as_mut().enable();
            if !self.runs(id) {
                return;
            }
            left.await;
        }
    }

    /// The searches holding or waiting for the turn before search `id`; None once it holds the
    /// turn, and for a search no task runs.
    pub fn ahead_of(&self, id: &str) -> Option<usize> {
        let line = self.line.lock().unwrap();
        let place = line.runs.iter().position(|run| run.id == id)?;
        (!line.runs[place].holds_turn).then_some(place)
    }

    /// The ids of `user`'s `find_sources` searches waiting for the turn, in arrival order.
    pub fn waiting_finds(&self, user: &UserId) -> Vec<String> {
        waiting_finds_in(&self.line.lock().unwrap().runs, user)
    }

    /// The backend's id of stored search `id`, once its task started it at the backend.
    #[cfg(test)]
    pub fn backend_search(&self, id: &str) -> Option<String> {
        let line = self.line.lock().unwrap();
        line.runs
            .iter()
            .find(|run| run.id == id)
            .and_then(|run| run.started.as_ref().map(|(search, _)| search.clone()))
    }

    /// D9: stops search `id` of a cancelled job when it is the job's own. One waiting for the
    /// turn never starts and is stored failed with `search_stopped`; one running is stopped at
    /// the backend and keeps what it found. A find_sources search the job reads (new_search with
    /// its search_id) is its caller's and runs on. For an id no task runs, it leaves a stop mark
    /// only when the search is not `recorded` yet (new_search records it after the job's write):
    /// the mark is taken under the lock `start_stored` registers with, so the task that starts
    /// that id afterwards stores it stopped without searching. A recorded search no task runs has
    /// ended, or a restart cut it off: nothing runs it again for a cancelled job. Marks older than
    /// SEARCH_KEPT_SECS are dropped when one is added. True when it stopped a task's search.
    pub async fn stop_job_search(&self, id: &str, recorded: bool) -> bool {
        let started = {
            let mut line = self.line.lock().unwrap();
            match line.runs.iter_mut().find(|run| run.id == id) {
                Some(run) if run.purpose == SearchFor::Job => {
                    run.stop = true;
                    run.started.clone()
                }
                Some(_) => return false,
                None => {
                    if !recorded {
                        let now = chrono::Utc::now().timestamp();
                        line.stops.retain(|_, at| *at > now - SEARCH_KEPT_SECS);
                        line.stops.insert(id.to_string(), now);
                    }
                    return false;
                }
            }
        };
        // A search waiting for the turn looks again, and leaves.
        self.left.notify_waiters();
        // One still starting is stopped by its task once the backend named it (`started`).
        if let Some((search, backend)) = started {
            stop_at_backend(&backend, &search).await;
        }
        true
    }

    /// Puts a job's search `id` in line; it leaves when the returned place is dropped.
    fn enter(self: &Arc<Self>, id: &str, user: UserId) -> Place {
        self.push(&mut self.line.lock().unwrap(), id, user, SearchFor::Job)
    }

    /// Puts `user`'s find_sources search `id` in line, unless `user` already has
    /// FINDS_WAITING_PER_USER waiting: then their ids. Counted and entered under one lock, so
    /// calls made together never pass the bound together.
    fn enter_find(self: &Arc<Self>, id: &str, user: UserId) -> Result<Place, Vec<String>> {
        let mut line = self.line.lock().unwrap();
        let waiting = waiting_finds_in(&line.runs, &user);
        if waiting.len() >= FINDS_WAITING_PER_USER {
            return Err(waiting);
        }
        Ok(self.push(&mut line, id, user, SearchFor::Find))
    }

    fn push(
        self: &Arc<Self>,
        line: &mut Line,
        id: &str,
        user: UserId,
        purpose: SearchFor,
    ) -> Place {
        let stop = line.stops.remove(id).is_some();
        line.runs.push(Run {
            id: id.to_string(),
            user,
            purpose,
            holds_turn: false,
            started: None,
            stop,
        });
        Place {
            searches: self.clone(),
            id: id.to_string(),
        }
    }

    /// Waits until search `id` is first in line, and takes the turn: true then, false once it is
    /// stopped before.
    async fn take_turn(&self, id: &str) -> bool {
        loop {
            let left = self.left.notified();
            tokio::pin!(left);
            // Registered before the look, so a search leaving after it still wakes this one.
            left.as_mut().enable();
            {
                let mut line = self.line.lock().unwrap();
                if line.runs.iter().any(|run| run.id == id && run.stop) {
                    return false;
                }
                if let Some(first) = line.runs.first_mut() {
                    if first.id == id {
                        first.holds_turn = true;
                        return true;
                    }
                }
            }
            left.await;
        }
    }

    /// The backend started search `id` as `backend_search`: whether it was stopped meanwhile, so
    /// its task stops it at the backend now.
    fn started(&self, id: &str, backend_search: &str, backend: &Arc<dyn DownloadBackend>) -> bool {
        let mut line = self.line.lock().unwrap();
        match line.runs.iter_mut().find(|run| run.id == id) {
            Some(run) => {
                run.started = Some((backend_search.to_string(), backend.clone()));
                run.stop
            }
            None => false,
        }
    }

    /// Takes the turn with a search of its own until the returned place is dropped, so searches
    /// started meanwhile wait for it.
    #[cfg(test)]
    pub(crate) async fn hold_turn_for_test(self: &Arc<Self>) -> Place {
        let id = uuid::Uuid::new_v4().to_string();
        let place = self.enter(&id, UserId::new("tester"));
        self.take_turn(&id).await;
        place
    }
}

/// Asks the backend to stop its search `search`; a refusal is logged, the search then running to
/// its own end.
async fn stop_at_backend(backend: &Arc<dyn DownloadBackend>, search: &str) {
    if let Err(e) = backend.stop_search(search).await {
        warn!("Could not stop search {search} at the backend: {e}");
    }
}

fn waiting_finds_in(runs: &[Run], user: &UserId) -> Vec<String> {
    runs.iter()
        .filter(|run| &run.user == user && run.purpose == SearchFor::Find && !run.holds_turn)
        .map(|run| run.id.clone())
        .collect()
}

/// A search's place in `Searches`, given up when dropped (its task ended, however it ended).
pub(crate) struct Place {
    searches: Arc<Searches>,
    id: String,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut line = self.searches.line.lock().unwrap();
        if let Some(place) = line.runs.iter().position(|run| run.id == self.id) {
            line.runs.remove(place);
        }
        drop(line);
        self.searches.left.notify_waiters();
    }
}

/// A stored search for `start_stored` to run.
pub struct SearchRun {
    pub id: String,
    /// The album and tracks searched for and scored against.
    pub query: DownloadQuery,
    pub spec: StoredSpec,
}

/// Why `start_find` started no search.
#[derive(Debug)]
pub enum FindRefused {
    /// The user already has FINDS_WAITING_PER_USER find_sources searches waiting for the turn:
    /// their ids, in arrival order. Nothing was written.
    TooManyWaiting(Vec<String>),
    /// Recording the search failed.
    Failed(Failure),
}

/// Records a job's search `run.id` for `ctx` on the default backend and runs it in its own task
/// once it holds the turn, storing how it ended. The task's answer is how it ended.
///
/// Called only for a new id, or for a search stored Running that no task runs (a restart cut it
/// off), which runs again under its id. Recording a search again drops what it found, so the
/// candidate ids handed out from it would name other groups: a search that ended is read as
/// stored and never started again.
pub async fn start_stored(
    searches: &Arc<Searches>,
    ledger: &Ledger,
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    run: SearchRun,
) -> Result<JoinHandle<SearchProgress>, Failure> {
    let place = searches.enter(&run.id, ctx.id());
    run_in_line(place, searches, ledger, ctx, backend, run).await
}

/// `start_stored` for a new find_sources search, refused before anything is written when the
/// user already has FINDS_WAITING_PER_USER find_sources searches waiting for the turn.
pub async fn start_find(
    searches: &Arc<Searches>,
    ledger: &Ledger,
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    run: SearchRun,
) -> Result<JoinHandle<SearchProgress>, FindRefused> {
    let place = searches
        .enter_find(&run.id, ctx.id())
        .map_err(FindRefused::TooManyWaiting)?;
    run_in_line(place, searches, ledger, ctx, backend, run)
        .await
        .map_err(FindRefused::Failed)
}

/// Records search `run` and spawns its task, which keeps `place` for its life. The place is
/// taken before the search is recorded, so a stored Running search is never read as cut off.
async fn run_in_line(
    place: Place,
    searches: &Arc<Searches>,
    ledger: &Ledger,
    ctx: &UserCtx,
    backend: Arc<dyn DownloadBackend>,
    run: SearchRun,
) -> Result<JoinHandle<SearchProgress>, Failure> {
    let SearchRun { id, query, spec } = run;
    ledger
        .record_search(NewSearch {
            id: id.clone(),
            user: ctx.id(),
            backend: Some(DEFAULT_DOWNLOAD_BACKEND.into()),
            query: query.clone(),
            spec: Some(spec.clone()),
        })
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    let (searches, ledger) = (searches.clone(), ledger.clone());
    Ok(tokio::spawn(async move {
        let _place = place;
        if !searches.take_turn(&id).await {
            let outcome = Err((search_stopped(), None));
            let progress = SearchProgress::of_outcome(&outcome);
            // The writer logs a write the database refused.
            let _ = ledger.search_ended(&id, outcome).await;
            return progress;
        }
        let search = SearchSpec {
            album: query.album,
            tracks: query.tracks,
            text: spec.text,
            keep_below_floor: spec.keep_below_floor,
        };
        let name = DEFAULT_DOWNLOAD_BACKEND;
        let (mut groups, stop) = match start_backend_search(name, &backend, &search).await {
            Ok(backend_search) => {
                if searches.started(&id, &backend_search, &backend) {
                    stop_at_backend(&backend, &backend_search).await;
                }
                poll_backend_search(name, &backend, &backend_search).await
            }
            Err(stop) => (Vec::new(), stop),
        };
        let stats = match &stop {
            SearchStop::Finished { stats } => Some(stats.clone()),
            SearchStop::TimedOut | SearchStop::Failed(_) => None,
        };
        let outcome = if groups.is_empty() {
            Err((no_results_failure(&stop, &[]), stats))
        } else {
            // Groups found before a timeout or a failed poll come with no stats: the backend
            // reported none.
            best_first(&mut groups);
            Ok((groups, stats.unwrap_or_default()))
        };
        let progress = SearchProgress::of_outcome(&outcome);
        // The writer logs a write the database refused.
        let _ = ledger.search_ended(&id, outcome).await;
        progress
    }))
}

/// A stored group offered as a source, with its source's recent failed transfers (D5).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// `{search_id}:{index}` (D3).
    pub id: String,
    /// Only its files that passed the filters.
    pub group: DownloadableGroup,
    pub recent_failures: u32,
}

impl Candidate {
    /// D5: never picked automatically.
    pub fn flagged(&self) -> bool {
        self.recent_failures >= FLAGGED_FAILURES
    }

    /// A file of it was scored below the floor, a match of another recording kept because the
    /// search asked for those: never taken unless Claude names it (download_source).
    pub fn below_floor(&self) -> bool {
        self.group.items.iter().any(|item| item.below_floor)
    }

    /// Whether a job may take it without Claude naming it: an automatic pick, next_source and
    /// failover take only a candidate neither flagged (D5) nor below the floor.
    pub fn automatic(&self) -> bool {
        !self.flagged() && !self.below_floor()
    }
}

/// Files the filters dropped, each under the first filter it failed (D6), and groups of an
/// excluded source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DroppedByFilter {
    pub format: u32,
    pub bitrate: u32,
    pub not_lossless: u32,
    pub queue_length: u32,
    /// Groups of a source the job excludes (a rejected result).
    pub excluded: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CandidateView {
    pub candidates: Vec<Candidate>,
    pub dropped_by_filter: DroppedByFilter,
}

impl CandidateView {
    /// The groups of its candidates a job may take without Claude naming them (`automatic`):
    /// what its failover may use (D4, D5).
    pub fn automatic_groups(&self) -> Vec<DownloadableGroup> {
        (self.candidates.iter())
            .filter(|c| c.automatic())
            .map(|c| c.group.clone())
            .collect()
    }
}

/// D6: extensions that are always lossless. `m4a` holds AAC or ALAC alike, so it is not one.
const LOSSLESS: [&str; 6] = ["flac", "wav", "aiff", "aif", "ape", "wv"];

/// Whether a file with `facts` passes `filters`; one that does not is counted in `dropped`
/// under the first filter it fails. What the source did not report fails the filter that asks
/// for it.
fn passes(facts: &FileFacts, filters: &SourceFilters, dropped: &mut DroppedByFilter) -> bool {
    let extension = facts.extension.as_deref();
    let failed = if !filters.formats.is_empty()
        && !extension.is_some_and(|e| filters.formats.iter().any(|f| f == e))
    {
        &mut dropped.format
    } else if filters
        .min_bitrate_kbps
        .is_some_and(|min| !facts.bitrate_kbps.is_some_and(|kbps| kbps >= min))
    {
        &mut dropped.bitrate
    } else if filters.lossless_only && !extension.is_some_and(|e| LOSSLESS.contains(&e)) {
        &mut dropped.not_lossless
    } else if filters
        .max_queue_length
        .is_some_and(|max| !facts.source_queue_length.is_some_and(|queue| queue <= max))
    {
        &mut dropped.queue_length
    } else {
        return true;
    };
    *failed += 1;
    false
}

/// The stored groups of search `search_id` that pass `filters` (D6) and whose source is not in
/// `excluded`, each keeping only the files that pass and its stored index in its id (D3), in
/// stored order, empty groups dropped. `failures` holds each source's recent failures.
pub fn view(
    search_id: &str,
    groups: &[DownloadableGroup],
    filters: &SourceFilters,
    failures: &HashMap<String, u32>,
    excluded: &[String],
) -> CandidateView {
    let mut dropped = DroppedByFilter::default();
    let mut candidates = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if excluded.contains(&group.source) {
            dropped.excluded += 1;
            continue;
        }
        let items: Vec<DownloadableItem> = group
            .items
            .iter()
            .filter(|item| passes(&item.facts, filters, &mut dropped))
            .cloned()
            .collect();
        if items.is_empty() {
            continue;
        }
        let group = if items.len() == group.items.len() {
            group.clone()
        } else {
            DownloadableGroup {
                item_count: items.len(),
                total_size: items.iter().filter_map(|item| item.size).sum(),
                items,
                ..group.clone()
            }
        };
        candidates.push(Candidate {
            id: candidate_id(search_id, index),
            recent_failures: failures.get(&group.source).copied().unwrap_or(0),
            group,
        });
    }
    CandidateView {
        candidates,
        dropped_by_filter: dropped,
    }
}

/// D3: the id of the group at `index` of search `search_id`'s stored ranking.
pub fn candidate_id(search_id: &str, index: usize) -> String {
    format!("{search_id}:{index}")
}

/// The search id and index a candidate id names, split at its last ':'; None for text that is
/// not one.
pub fn parse_candidate_id(text: &str) -> Option<(String, usize)> {
    let (search_id, index) = text.rsplit_once(':')?;
    if search_id.is_empty() {
        return None;
    }
    Some((search_id.to_string(), index.parse().ok()?))
}

/// The candidate does not exist, or its search is another user's.
pub fn candidate_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Candidate not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::core::test_ctx;
    use crate::ledger::{Ledger, NewSearch};
    use crate::server_fns::download::test_support::{group, item, search_result, StubBackend};
    use shared::download::{DownloadableItem, FileFacts};
    use shared::failure::UserActionCode;
    use soulbeet::error::{MissingSetting, SoulseekError};

    /// The groups `stub`'s search found and the failure an empty one gives.
    async fn searched(stub: Arc<StubBackend>) -> (Vec<DownloadableGroup>, Failure) {
        let backend: Arc<dyn DownloadBackend> = stub;
        let (groups, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;
        (groups, no_results_failure(&stop, &[]))
    }

    /// A search the backend finished with no file in its responses.
    fn finished_empty() -> SearchStop {
        SearchStop::Finished {
            stats: SearchStats::default(),
        }
    }

    fn advice(f: &Failure) -> (FailureCode, Stage, RetryAdvice, &str) {
        (f.code, f.stage, f.retry, f.message.as_str())
    }

    // Paused: every poll waits SEARCH_POLL_INTERVAL first.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_refuses_to_start_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        let refused = "The server connection must be connected and logged in to perform a search (currently: Disconnected)";
        stub.fail_search_start(SoulseekError::Api {
            status: 409,
            message: refused.into(),
        });

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        let cause = f.cause.expect("slskd's answer");
        assert_eq!((cause.status, cause.raw.as_str()), (Some(409), refused));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_whose_poll_fails_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".into(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(503));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_that_finishes_empty_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            Vec::new(),
        ))]);

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchNoResults,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(f.cause, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_still_running_at_the_deadline_is_a_timeout() {
        let stub = StubBackend::new(vec![]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchTimeout,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
    }

    /// A backend may send groups while the search runs: those an earlier
    /// poll found are the result when the completing poll is empty.
    #[tokio::test(start_paused = true)]
    async fn a_search_keeps_the_groups_found_before_it_completed() {
        let stub = StubBackend::new(vec![]);
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "Wake Up", "a.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(search_result(
                "search",
                SearchState::InProgress,
                found.clone(),
            )),
            Ok(search_result("search", SearchState::Completed, Vec::new())),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub;

        let (groups, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;

        assert_eq!(groups, found);
        assert!(matches!(stop, SearchStop::Finished { .. }));
    }

    #[test]
    fn a_search_whose_files_were_all_scored_out_is_all_filtered() {
        let f = no_results_failure(
            &SearchStop::Finished {
                stats: SearchStats {
                    file_count: 7,
                    ..Default::default()
                },
            },
            &[SearchStop::TimedOut],
        );
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SearchAllFiltered,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(
            no_results_failure(&finished_empty(), &[]).code,
            FailureCode::SearchNoResults
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_search_reports_the_files_it_saw() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(SearchResult {
            stats: SearchStats {
                file_count: 7,
                ..Default::default()
            },
            ..search_result("search", SearchState::Completed, Vec::new())
        })]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let (_, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;
        assert!(matches!(
            stop,
            SearchStop::Finished {
                stats: SearchStats { file_count: 7, .. }
            }
        ));
    }

    #[test]
    fn no_results_failure_tells_an_empty_search_from_a_dead_backend() {
        let dead = |message: &str| {
            SearchStop::Failed(
                SoulseekError::Api {
                    status: 503,
                    message: message.into(),
                }
                .failure(Stage::SourceSearch, "x"),
            )
        };
        let advice = |first: SearchStop, rest: Vec<SearchStop>| {
            let f = no_results_failure(&first, &rest);
            (f.code, f.retry)
        };
        use FailureCode::*;
        use RetryAdvice::*;
        assert_eq!(
            advice(finished_empty(), vec![]),
            (SearchNoResults, OtherQuery)
        );
        assert_eq!(
            advice(SearchStop::TimedOut, vec![]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![]),
            (BackendUnreachable, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![finished_empty()]),
            (SearchNoResults, OtherQuery)
        );
        // A timeout says more than a dead backend, an empty search more than both.
        assert_eq!(
            advice(dead("down"), vec![SearchStop::TimedOut]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(SearchStop::TimedOut, vec![finished_empty()]),
            (SearchNoResults, OtherQuery)
        );
        let f = no_results_failure(&dead("first"), &[dead("second")]);
        assert_eq!(f.message, "No results found");
        assert_eq!(f.stage, Stage::SourceSearch);
        assert_eq!(f.cause.unwrap().raw, "first");
    }

    /// slskd lost the search: its 404 on the responses is a failed search
    /// to run again, not an empty one.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_no_longer_has_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::SearchGone {
            body: String::new(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchLost,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(404));
    }

    /// slskd refused the query itself: another query, not a wait.
    #[tokio::test(start_paused = true)]
    async fn a_query_slskd_refuses_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        let body = "Search query must contain at least one non-exclusion term (Parameter 'query')";
        stub.fail_search_start(SoulseekError::SearchQueryRejected { body: body.into() });

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchQueryRejected,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        let cause = f.cause.unwrap();
        assert_eq!((cause.status, cause.raw.as_str()), (Some(400), body));
    }

    #[test]
    fn a_search_is_readable_until_it_expires() {
        let stored = |expires_at| {
            Some(StoredSearch {
                id: "s1".into(),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![]),
                candidates: None,
                expires_at,
                spec: None,
                stats: None,
                failure: None,
            })
        };
        assert_eq!(live_search(None, 1_000).unwrap_err(), search_not_found());
        assert_eq!(
            live_search(stored(1_000), 1_000).unwrap_err(),
            search_expired()
        );
        assert_eq!(
            live_search(stored(1_001), 1_000).map(|s| s.expires_at),
            Ok(1_001)
        );
        let f = search_expired();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::ExpiredHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Search expired"
            )
        );
    }

    #[tokio::test]
    async fn another_users_search_reads_like_a_missing_one() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        crate::core::folder_of(&db, "bob").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .record_search(NewSearch {
                id: "s1".into(),
                user: UserId::new("alice"),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![]),
                spec: None,
            })
            .await
            .unwrap();
        assert_eq!(
            owned_search(&db, &test_ctx("bob"), "s1").await.unwrap_err(),
            search_not_found()
        );
        assert_eq!(
            owned_search(&db, &test_ctx("alice"), "s1")
                .await
                .map(|s| s.backend),
            Ok(Some("slskd".into()))
        );
    }

    // Paused: every poll waits SEARCH_POLL_INTERVAL first.
    #[tokio::test(start_paused = true)]
    async fn search_ranked_puts_the_best_group_of_every_backend_first() {
        let found =
            |peer: &str, score| vec![group(peer, vec![item(peer, "Wake Up", "a.flac")], score)];
        let (a, b) = (StubBackend::new(vec![]), StubBackend::new(vec![]));
        a.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("peer_a", 0.5),
        ))]);
        b.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("peer_b", 0.9),
        ))]);
        let first: Searched = ("a".into(), a as Arc<dyn DownloadBackend>);
        let rest: Vec<Searched> = vec![("b".into(), b as Arc<dyn DownloadBackend>)];
        let groups = search_ranked(&first, &rest, None, &[]).await.unwrap();
        let ranked: Vec<(&str, f64)> = groups
            .iter()
            .map(|g| (g.source.as_str(), g.score))
            .collect();
        assert_eq!(ranked, vec![("peer_b", 0.9), ("peer_a", 0.5)]);
    }

    #[tokio::test(start_paused = true)]
    async fn search_ranked_with_nothing_found_fails_as_no_results() {
        let a = StubBackend::new(vec![]);
        a.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            Vec::new(),
        ))]);
        let first: Searched = ("a".into(), a as Arc<dyn DownloadBackend>);
        let f = search_ranked(&first, &[], None, &[]).await.unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailureCode::SearchNoResults, "No results found")
        );
    }

    #[test]
    fn an_unknown_search_is_not_found() {
        let f = search_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Search not found"
            )
        );
    }

    fn facts(ext: &str, kbps: Option<u32>, queue: u64) -> FileFacts {
        FileFacts {
            extension: Some(ext.into()),
            bitrate_kbps: kbps,
            source_queue_length: Some(queue),
            ..FileFacts::default()
        }
    }

    fn file(source: &str, id: &str, facts: FileFacts) -> DownloadableItem {
        DownloadableItem {
            facts,
            ..item(source, "One", id)
        }
    }

    fn run(id: &str, spec: StoredSpec) -> SearchRun {
        SearchRun {
            id: id.into(),
            query: DownloadQuery::new(vec![]),
            spec,
        }
    }

    fn ids(view: &CandidateView) -> Vec<&str> {
        view.candidates.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn filters_drop_files_by_the_first_rule_they_fail_and_ids_keep_their_place() {
        let groups = vec![
            group(
                "peer_a",
                vec![file("peer_a", "a/01.flac", facts("flac", None, 0))],
                0.9,
            ),
            group(
                "peer_b",
                vec![file("peer_b", "b/01.mp3", facts("mp3", Some(320), 0))],
                0.8,
            ),
            group(
                "peer_c",
                vec![file("peer_c", "c/01.flac", facts("flac", Some(900), 40))],
                0.7,
            ),
        ];
        let lossless = SourceFilters {
            lossless_only: true,
            max_queue_length: Some(10),
            ..Default::default()
        };
        let shown = view("s1", &groups, &lossless, &HashMap::new(), &[]);
        assert_eq!(ids(&shown), vec!["s1:0"]);
        assert_eq!(
            shown.dropped_by_filter,
            DroppedByFilter {
                not_lossless: 1,
                queue_length: 1,
                ..Default::default()
            }
        );
        let bitrate = SourceFilters {
            min_bitrate_kbps: Some(256),
            ..Default::default()
        };
        assert_eq!(
            ids(&view("s1", &groups, &bitrate, &HashMap::new(), &[])),
            vec!["s1:1", "s1:2"],
            "a file with no bitrate fails a bitrate filter"
        );
        let without_a = view(
            "s1",
            &groups,
            &SourceFilters::default(),
            &HashMap::new(),
            &["peer_a".to_string()],
        );
        assert_eq!(
            (
                without_a.candidates[0].id.as_str(),
                without_a.dropped_by_filter.excluded
            ),
            ("s1:1", 1)
        );
        assert_eq!(parse_candidate_id("s1:2"), Some(("s1".into(), 2)));
        assert_eq!(parse_candidate_id("s1"), None);
    }

    #[test]
    fn a_group_keeps_only_its_files_that_pass_and_a_format_filter_comes_first() {
        let groups = vec![group(
            "peer_a",
            vec![
                file("peer_a", "a/01.flac", facts("flac", Some(900), 0)),
                file("peer_a", "a/02.mp3", facts("mp3", Some(128), 0)),
                file("peer_a", "a/cover.m4a", facts("m4a", Some(256), 0)),
            ],
            0.9,
        )];
        let filters = SourceFilters {
            formats: vec!["flac".into(), "mp3".into()],
            min_bitrate_kbps: Some(192),
            lossless_only: true,
            max_queue_length: None,
        };
        let shown = view("s1", &groups, &filters, &HashMap::new(), &[]);
        let kept = &shown.candidates[0].group;
        assert_eq!(
            (kept.items.len(), kept.item_count, kept.total_size),
            (1, 1, 27_213_250)
        );
        assert_eq!(kept.items[0].id, "a/01.flac");
        assert_eq!(
            shown.dropped_by_filter,
            DroppedByFilter {
                format: 1,
                bitrate: 1,
                ..Default::default()
            }
        );
        let wav_only = SourceFilters {
            formats: vec!["wav".into()],
            ..Default::default()
        };
        let none = view("s1", &groups, &wav_only, &HashMap::new(), &[]);
        assert!(
            none.candidates.is_empty(),
            "a group left with no file is dropped"
        );
        assert_eq!(none.dropped_by_filter.format, 3);
    }

    #[test]
    fn candidates_show_recent_failures_and_flag_a_failing_source() {
        let groups = vec![
            group("peer_a", vec![item("peer_a", "One", "a/01.flac")], 0.9),
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.8),
        ];
        let shown = view(
            "s1",
            &groups,
            &SourceFilters::default(),
            &HashMap::from([("peer_a".to_string(), 3)]),
            &[],
        );
        assert_eq!(
            shown
                .candidates
                .iter()
                .map(|c| (c.recent_failures, c.flagged()))
                .collect::<Vec<_>>(),
            vec![(3, true), (0, false)]
        );
    }

    #[test]
    fn a_candidate_id_names_its_search_and_place() {
        assert_eq!(candidate_id("s1", 4), "s1:4");
        assert_eq!(parse_candidate_id("a:b:7"), Some(("a:b".into(), 7)));
        assert_eq!(parse_candidate_id("s1:x"), None);
        assert_eq!(parse_candidate_id(":3"), None);
        let f = candidate_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Candidate not found"
            )
        );
    }

    // Real time, in the tests of stored searches: the in-memory database answers from its own
    // thread, and a paused clock runs the pool's acquire timeout out while it waits. Each search
    // here polls once, SEARCH_POLL_INTERVAL after it starts.
    #[tokio::test]
    async fn a_stored_search_waits_for_the_one_before_it_and_keeps_its_outcome() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let stub = StubBackend::new(vec![]);
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "One", "a/01.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(SearchResult {
                stats: SearchStats {
                    query_text: "Kowloon One".into(),
                    response_count: 1,
                    file_count: 1,
                    ..Default::default()
                },
                ..search_result("search", SearchState::Completed, found.clone())
            }),
            Ok(SearchResult {
                stats: SearchStats {
                    query_text: "Kowloon Two".into(),
                    response_count: 2,
                    file_count: 7,
                    below_floor: 7,
                    ..Default::default()
                },
                ..search_result("search", SearchState::Completed, vec![])
            }),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let ctx = test_ctx("alice");
        let searches: Arc<Searches> = Arc::default();
        let spec = StoredSpec {
            text: Some("Kowloon One".into()),
            ..Default::default()
        };
        let first = start_find(
            &searches,
            &ledger,
            &ctx,
            backend.clone(),
            run("s1", spec.clone()),
        )
        .await
        .unwrap();
        let second = start_stored(
            &searches,
            &ledger,
            &ctx,
            backend,
            run("s2", StoredSpec::default()),
        )
        .await
        .unwrap();
        assert!(matches!(first.await.unwrap(), SearchProgress::Done(g, _) if g == found));
        let SearchProgress::Failed(f, Some(stats)) = second.await.unwrap() else {
            panic!()
        };
        assert_eq!(
            (f.code, stats.below_floor),
            (FailureCode::SearchAllFiltered, 7)
        );
        assert_eq!(
            stub.specs()
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>(),
            vec![Some("Kowloon One".into()), None]
        );
        let stored = store::search_of(&db, "s2", &ctx.id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            SearchProgress::of(&stored),
            SearchProgress::Failed(..)
        ));
        let done = store::search_of(&db, "s1", &ctx.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (done.spec.clone(), SearchProgress::of(&done)),
            (
                Some(spec),
                SearchProgress::Done(
                    found,
                    SearchStats {
                        query_text: "Kowloon One".into(),
                        response_count: 1,
                        file_count: 1,
                        ..Default::default()
                    }
                )
            )
        );
        assert!(!searches.runs("s1") && !searches.runs("s2"));
    }

    #[tokio::test]
    async fn a_stopped_search_never_runs_or_stops_at_the_backend_keeping_what_it_found() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let stub = StubBackend::new(vec![]);
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "One", "a/01.flac")],
            0.9,
        )];
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found.clone(),
        ))]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let ctx = test_ctx("alice");
        let searches: Arc<Searches> = Arc::default();
        // A cancel that came before the runner recorded and started its job's search.
        assert!(
            !searches.stop_job_search("s1", false).await,
            "no task runs s1 yet"
        );
        let stopped = start_stored(
            &searches,
            &ledger,
            &ctx,
            backend.clone(),
            run("s1", StoredSpec::default()),
        )
        .await
        .unwrap();
        assert_eq!(
            stopped.await.unwrap(),
            SearchProgress::Failed(search_stopped(), None)
        );
        assert!(stub.specs().is_empty(), "it never searched");
        let stored = store::search_of(&db, "s1", &ctx.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.failure, Some(search_stopped()));

        let running = start_stored(
            &searches,
            &ledger,
            &ctx,
            backend,
            run("s2", StoredSpec::default()),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while searches.backend_search("s2").is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the backend started it");
        assert!(searches.stop_job_search("s2", true).await);
        assert_eq!(stub.stopped_searches(), vec!["search".to_string()]);
        assert_eq!(
            running.await.unwrap(),
            SearchProgress::Done(found, SearchStats::default())
        );
    }

    #[tokio::test]
    async fn a_job_stops_only_its_own_search_and_marks_only_one_not_recorded_yet() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let stub = StubBackend::new(vec![]);
        let (reached, release) = stub.hold_searches();
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "One", "a/01.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(search_result(
                "search",
                SearchState::Completed,
                found.clone(),
            )),
            Ok(search_result("search", SearchState::Completed, found)),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let ctx = test_ctx("alice");
        let searches: Arc<Searches> = Arc::default();
        // A job read a find_sources search, and was cancelled while it ran: it runs on.
        let find = start_find(
            &searches,
            &ledger,
            &ctx,
            backend.clone(),
            run("f1", StoredSpec::default()),
        )
        .await
        .unwrap();
        reached.notified().await;
        assert!(!searches.stop_job_search("f1", true).await);
        release.notify_one();
        assert!(matches!(find.await.unwrap(), SearchProgress::Done(..)));
        assert!(stub.stopped_searches().is_empty());
        // A recorded search no task runs leaves no mark: a later run of its id searches.
        assert!(!searches.stop_job_search("s1", true).await);
        let later = start_stored(
            &searches,
            &ledger,
            &ctx,
            backend,
            run("s1", StoredSpec::default()),
        )
        .await
        .unwrap();
        release.notify_one();
        assert!(matches!(later.await.unwrap(), SearchProgress::Done(..)));
    }

    #[tokio::test]
    async fn the_second_stored_search_starts_after_the_first_ended() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let stub = StubBackend::new(vec![]);
        let (reached, release) = stub.hold_searches();
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "One", "a/01.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(search_result(
                "search",
                SearchState::Completed,
                found.clone(),
            )),
            Ok(search_result("search", SearchState::Completed, found)),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let ctx = test_ctx("alice");
        let searches: Arc<Searches> = Arc::default();
        let text = |text: &str| StoredSpec {
            text: Some(text.into()),
            ..Default::default()
        };
        let texts = || {
            stub.specs()
                .into_iter()
                .map(|s| s.text.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        let first = start_stored(
            &searches,
            &ledger,
            &ctx,
            backend.clone(),
            run("s1", text("One")),
        )
        .await
        .unwrap();
        let second = start_find(
            &searches,
            &ledger,
            &ctx,
            backend.clone(),
            run("s2", text("Two")),
        )
        .await
        .unwrap();

        reached.notified().await;
        assert_eq!(texts(), vec!["One"]);
        assert_eq!(
            (searches.ahead_of("s1"), searches.ahead_of("s2")),
            (None, Some(1))
        );
        assert_eq!(searches.waiting_finds(&ctx.id()), vec!["s2".to_string()]);
        let waiting = store::search_of(&db, "s2", &ctx.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(SearchProgress::of(&waiting), SearchProgress::Running);

        release.notify_one();
        reached.notified().await;
        assert_eq!(texts(), vec!["One", "Two"]);
        let ended = store::search_of(&db, "s1", &ctx.id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            SearchProgress::of(&ended),
            SearchProgress::Done(..)
        ));
        assert!(!searches.runs("s1"));
        assert_eq!(searches.ahead_of("s2"), None);
        assert!(searches.waiting_finds(&ctx.id()).is_empty());

        // slskd ends a search it was told to stop.
        backend.stop_search("search").await.unwrap();
        assert_eq!(stub.stopped_searches(), vec!["search".to_string()]);
        assert!(matches!(first.await.unwrap(), SearchProgress::Done(..)));
        assert!(matches!(second.await.unwrap(), SearchProgress::Done(..)));
        assert!(!searches.runs("s2"));
    }

    // Paused: a search waiting for the turn is seen not to get it.
    #[tokio::test(start_paused = true)]
    async fn a_search_takes_the_turn_when_every_one_before_it_left() {
        let searches: Arc<Searches> = Arc::default();
        let alice = UserId::new("alice");
        let first = searches.enter("s1", alice.clone());
        let second = searches.enter_find("s2", alice.clone()).unwrap();
        let third = searches.enter_find("s3", UserId::new("bob")).unwrap();
        let ahead = || ["s1", "s2", "s3"].map(|id| searches.ahead_of(id));
        assert_eq!(ahead(), [Some(0), Some(1), Some(2)]);

        searches.take_turn("s1").await;
        let stub: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        assert!(!searches.started("s1", "backend-1", &stub));
        assert_eq!(ahead(), [None, Some(1), Some(2)]);
        assert_eq!(
            (searches.backend_search("s1"), searches.backend_search("s2")),
            (Some("backend-1".to_string()), None)
        );
        assert_eq!(searches.waiting_finds(&alice), vec!["s2".to_string()]);
        let turn = tokio::time::timeout(Duration::from_secs(60), searches.take_turn("s3"));
        assert!(turn.await.is_err(), "s3 waits behind s1 and s2");

        // A search that leaves while it waits gives up its place.
        drop(second);
        assert_eq!(ahead(), [None, None, Some(1)]);
        let waiting = tokio::spawn({
            let searches = searches.clone();
            async move { searches.take_turn("s3").await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!waiting.is_finished());
        drop(first);
        waiting.await.unwrap();
        assert_eq!(ahead(), [None, None, None]);
        assert!(!searches.runs("s1") && !searches.runs("s2") && searches.runs("s3"));
        drop(third);
        assert!(!searches.runs("s3"));
    }

    #[test]
    fn a_user_with_finds_waiting_up_to_the_bound_enters_no_more_and_writes_nothing() {
        let searches: Arc<Searches> = Arc::default();
        let (alice, bob) = (UserId::new("alice"), UserId::new("bob"));
        let mut places = vec![searches.enter("j1", alice.clone())];
        for id in ["f1", "f2", "f3"] {
            places.push(searches.enter_find(id, alice.clone()).unwrap());
        }
        let refused = searches.enter_find("f4", alice.clone()).err();
        assert_eq!(
            refused,
            Some(vec!["f1".to_string(), "f2".into(), "f3".into()])
        );
        assert!(!searches.runs("f4"), "a refused search is not in line");
        // Another user's finds and a job's searches have no bound of their own here.
        places.push(searches.enter_find("b1", bob).unwrap());
        places.push(searches.enter("j2", alice.clone()));
        // A find that holds the turn waits no more.
        places.remove(0);
        searches.line.lock().unwrap().runs[0].holds_turn = true;
        assert_eq!(
            searches.waiting_finds(&alice),
            vec!["f2".to_string(), "f3".into()]
        );
        places.push(searches.enter_find("f4", alice.clone()).unwrap());
        assert!(searches.enter_find("f5", alice).is_err());
    }

    #[tokio::test]
    async fn a_refused_find_records_nothing() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let ctx = test_ctx("alice");
        let searches: Arc<Searches> = Arc::default();
        let _held: Vec<Place> = ["f1", "f2", "f3"]
            .map(|id| searches.enter_find(id, ctx.id()).unwrap())
            .into();
        let refused = start_find(
            &searches,
            &ledger,
            &ctx,
            backend,
            run("f4", StoredSpec::default()),
        )
        .await
        .err();
        assert!(matches!(refused, Some(FindRefused::TooManyWaiting(ids)) if ids.len() == 3));
        ledger.settled().await;
        assert!(store::search_of(&db, "f4", &ctx.id())
            .await
            .unwrap()
            .is_none());
        assert!(!searches.runs("f4"));
    }

    #[test]
    fn no_backends_keeps_why_the_default_backend_was_unavailable() {
        let unset = BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url));
        let f = no_backends(&unset);
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::BackendNotConfigured,
                Stage::SourceSearch,
                RetryAdvice::NeedsUser,
                "No download backends available"
            )
        );
        assert_eq!(
            f.user_action.unwrap().code,
            UserActionCode::ConfigureBackend
        );
        assert_eq!(f.cause.unwrap().raw, "slskd URL not configured");
    }
}
