//! What MCP acquisition acts through (the database, the ledger, the one line of MCP searches
//! and the effects outside them), and the job runner that takes an MCP job from its search to
//! its enqueue.

use crate::auth::{UserCtx, UserId};
use crate::core::acquire::{
    alternates_from, backend_of, enqueue_and_watch, manual_batch_label, numbered, prepare_target,
    Acquisition, NewBatch,
};
use crate::core::sources::{
    no_candidate_passed, start_stored, view, Candidate, CandidateView, SearchProgress, SearchRun,
    Searches, RECENT_FAILURES_SECS,
};
use crate::core::{
    backend_unavailable, backend_unavailable_at, database_failure, logged, metadata,
};
use crate::import_queue::{ImportQueue, IMPORT_QUEUE};
use crate::importer_health::blocking_failure;
use crate::ledger::store::{self, JobRecord, StoredSearch};
use crate::ledger::{
    BatchOrigin, DuplicateRule, JobKind, JobOptions, JobState, Ledger, LedgerError, Pick,
    StoredSpec, Transition, Transitioned, LEDGER,
};
use crate::server_fns::download::enqueue::nothing_enqueued;
use crate::services::{Backends, Registry, DEFAULT_DOWNLOAD_BACKEND};
use async_trait::async_trait;
use shared::download::{DownloadQuery, DownloadableGroup, DownloadableItem, QueuedDownload};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::{DownloadBackend, ImportMode};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// What acquisition does outside the ledger, so tests run it on stubs.
#[async_trait]
pub(crate) trait Effects: Send + Sync {
    /// Where download backends are looked up.
    fn backends(&self) -> &dyn Backends;
    /// The failure an import would end with because the importer is broken; None when nothing
    /// blocks imports.
    async fn importer_blocked(&self) -> Option<Failure>;
    /// Fills an album query's empty tracklist from MusicBrainz: an MCP album is asked for by
    /// its release MBID (D1).
    async fn hydrate(&self, query: &mut DownloadQuery) -> Result<(), Failure>;
    /// Enqueues `acquisition`'s files into `fresh` for `ctx` on `backend` and watches them,
    /// recording every row in the ledger (`enqueue_and_watch`). A failure is already on the
    /// slots it failed.
    async fn enqueue(
        &self,
        ctx: &UserCtx,
        backend: Arc<dyn DownloadBackend>,
        fresh: NewBatch,
        acquisition: Acquisition,
    ) -> Result<Vec<QueuedDownload>, Failure>;
}

/// Everything a tool and the job runner act through.
pub(crate) struct Acquire {
    pub db: SqlitePool,
    pub ledger: Ledger,
    pub effects: Arc<dyn Effects>,
    /// One MCP search at a time.
    pub searches: Arc<Searches>,
    /// Where finished downloads are imported and kept imports wait for a decision.
    pub queue: ImportQueue,
    /// The folder downloads land in, which kept imports are shown relative to.
    pub download_base: PathBuf,
    /// The jobs a runner takes on now.
    runners: Runners,
}

/// The server's: its database, LEDGER, the live effects, one line of searches, IMPORT_QUEUE and
/// the configured download folder.
pub(crate) static ACQUIRE: LazyLock<Arc<Acquire>> = LazyLock::new(|| {
    Arc::new(Acquire {
        db: (*crate::db::DB).clone(),
        ledger: LEDGER.clone(),
        effects: Arc::new(LiveEffects),
        searches: Arc::default(),
        queue: IMPORT_QUEUE.clone(),
        download_base: crate::config::CONFIG.download_path().clone(),
        runners: Runners::default(),
    })
});

/// One runner per job at a time: a job taken on again (a retry, a chosen source) while an
/// earlier runner of it still waits for a download place is taken on once that one ends, and
/// that one looks at the job again at once, so a job is never enqueued twice.
#[derive(Default)]
struct Runners {
    /// Each job a runner has, with what tells that runner to look at its job again.
    jobs: Mutex<HashMap<String, Arc<Notify>>>,
    /// Notified when a runner ends.
    ended: Notify,
}

impl Runners {
    /// Tells the runner that has job `id`, when one has, to look at it again.
    fn look_again(&self, id: &str) {
        if let Some(look_again) = self.jobs.lock().unwrap().get(id) {
            look_again.notify_one();
        }
    }

    /// Waits until no runner has job `id`, telling the one that has it to look at it again, and
    /// takes it until the returned guard drops.
    async fn take(&self, id: &str) -> RunnerOf<'_> {
        loop {
            let ended = self.ended.notified();
            tokio::pin!(ended);
            // Registered before the look, so a runner ending after it still wakes this one.
            ended.as_mut().enable();
            {
                let mut jobs = self.jobs.lock().unwrap();
                match jobs.get(id) {
                    Some(look_again) => look_again.notify_one(),
                    None => {
                        let look_again = Arc::new(Notify::new());
                        jobs.insert(id.to_string(), look_again.clone());
                        return RunnerOf {
                            runners: self,
                            id: id.to_string(),
                            look_again,
                        };
                    }
                }
            }
            ended.await;
        }
    }
}

/// A job's runner, given up when dropped.
struct RunnerOf<'a> {
    runners: &'a Runners,
    id: String,
    /// Notified when the job is taken on again while this runner has it.
    look_again: Arc<Notify>,
}

impl Drop for RunnerOf<'_> {
    fn drop(&mut self) {
        self.runners.jobs.lock().unwrap().remove(&self.id);
        self.runners.ended.notify_waiters();
    }
}

impl Acquire {
    /// A call changed job `id` (cancel_job): its runner, waiting for a download place, looks at
    /// it again now rather than when a place is next given up.
    pub(crate) fn job_changed(&self, id: &str) {
        self.runners.look_again(id);
    }
}

/// The services the server is configured with.
struct LiveEffects;

#[async_trait]
impl Effects for LiveEffects {
    fn backends(&self) -> &dyn Backends {
        &Registry
    }

    async fn importer_blocked(&self) -> Option<Failure> {
        blocking_failure().await
    }

    async fn hydrate(&self, query: &mut DownloadQuery) -> Result<(), Failure> {
        metadata::hydrate_album_tracks(query).await
    }

    async fn enqueue(
        &self,
        ctx: &UserCtx,
        backend: Arc<dyn DownloadBackend>,
        fresh: NewBatch,
        acquisition: Acquisition,
    ) -> Result<Vec<QueuedDownload>, Failure> {
        enqueue_and_watch(&LEDGER, ctx, backend, fresh, acquisition).await
    }
}

/// Design 11.5: the MCP jobs of one user that download at once.
pub(crate) const MCP_DOWNLOADS_PER_USER: u32 = 6;

/// What an automatic pick chose (D4).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Picked {
    /// The candidate at this index downloads, the others stand by.
    Auto(usize),
    /// Every candidate waits for the assistant (pick review, or none at min_score a job may take
    /// without the assistant naming it).
    Choose,
}

/// D4, on `view`'s candidates, best first (never empty: drive fails a job whose view is empty
/// first): the best one at or above `min_score` a job may take without the assistant naming it,
/// neither flagged nor below the floor (`Candidate::automatic`).
pub(crate) fn pick(candidates: &[Candidate], options: &JobOptions) -> Picked {
    if options.pick == Pick::Review {
        return Picked::Choose;
    }
    candidates
        .iter()
        .position(|c| c.automatic() && c.group.score >= options.min_score)
        .map_or(Picked::Choose, Picked::Auto)
}

/// Takes job `id` of `ctx` on from its state: searching (hydrate an album, search or read its
/// search, pick), then queued (wait for a download place, then enqueue). Returns when the job is
/// enqueued, waits for a choice, ends, or a transition it asked for was refused (another call
/// moved the job, D15). A web UI job has its own pipeline and is left to it. One runner takes a
/// job at a time: a second waits for the first to end, then reads the job as it is then. A job
/// it cannot read once it decided on it fails, as at any read the runner cannot make; one it
/// cannot read at all is stranded (`Ledger::strand`).
pub(crate) async fn drive(acquire: &Arc<Acquire>, ctx: &UserCtx, id: &str) {
    let taken = acquire.runners.take(id).await;
    let user = ctx.id();
    // Unread, the job is not known to be this runner's to fail (an earlier runner may have sent
    // its files): it is read again, and stranded once the database keeps refusing.
    let read = retried(
        || store::job_record(&acquire.db, id, &user),
        |e: &sqlx::Error, wait: Duration| {
            warn!("Could not read job {id} to take it on, trying again in {wait:?}: {e}")
        },
    )
    .await;
    let job = match read {
        Ok(Some(job)) => job,
        Ok(None) => return,
        Err(e) => {
            error!("Could not read job {id} in {FAIL_ATTEMPTS} reads, leaving it to the next start: {e}");
            acquire.ledger.strand(id);
            return;
        }
    };
    let Some(options) = job.options.clone() else {
        return;
    };
    let runner = Runner {
        acquire,
        ctx,
        job: &job,
        options: &options,
    };
    let decided = match job.state {
        JobState::Searching => match runner.search_and_pick().await {
            Some(decided) => decided,
            None => return,
        },
        // A send already recorded is its monitor's.
        JobState::Queued if !job.sent => Decided {
            search_id: job.search_id.clone(),
            change: job.changed_seq,
        },
        _ => return,
    };
    if !runner.take_place(&taken.look_again, &decided).await {
        return;
    }
    // Read again: the job may have been cancelled and taken on again with another source while
    // this runner waited for its place.
    let job = match store::job_record(&acquire.db, id, &user).await {
        Ok(Some(job)) if job.state == JobState::Queued && !job.sent => job,
        Ok(_) => return,
        Err(e) => {
            // Failing gives up the place it holds now. The failure names only the search, as
            // its download's do: a cancel asked for meanwhile then ends it cancelled, where a
            // failure refused for it would leave the place held with no runner.
            let failure = database_failure(Stage::Enqueue, e);
            runner.fail(failure, decided.search_id.clone()).await;
            return;
        }
    };
    let Some(options) = job.options.clone() else {
        return;
    };
    let runner = Runner {
        acquire,
        ctx,
        job: &job,
        options: &options,
    };
    runner.download().await;
}

/// `drive` in a task of its own.
pub(crate) fn spawn_drive(acquire: Arc<Acquire>, ctx: UserCtx, id: String) {
    tokio::spawn(async move { drive(&acquire, &ctx, &id).await });
}

/// One job the runner takes on.
struct Runner<'a> {
    acquire: &'a Acquire,
    ctx: &'a UserCtx,
    job: &'a JobRecord,
    options: &'a JobOptions,
}

/// Why the runner stops before its job is picked.
enum Stop {
    /// The job fails with this.
    Fail(Failure),
    /// Another call moved the job.
    Quietly,
}

/// A job's search gave its runner no answer: its task stopped (a panic) before it said how the
/// search went, or the job had no search to read. Searching again is the remedy.
pub(crate) fn search_unanswered() -> Failure {
    Failure::new(
        FailureCode::SearchLost,
        Stage::SourceSearch,
        RetryAdvice::SameSourceLater,
        "The job's search ended without an answer",
    )
}

/// The longest a failure that could not be written, or a job that could not be read, waits
/// before it is tried again.
const FAIL_RETRY_MAX: Duration = Duration::from_secs(30);

/// How many times a failure is written, or a job read, before it is given up: about a minute of
/// waits.
const FAIL_ATTEMPTS: u32 = 7;

/// Fails `user`'s job `job` with `failure` (`search_id` the search it was decided on, when there
/// was one; `change` the job's change number, when it was decided on one), writing it again,
/// waiting longer each time up to FAIL_RETRY_MAX, until the writer applies or refuses it: a job
/// whose failure was not written would wait with no runner. A refusal means the job moved on
/// meanwhile. After FAIL_ATTEMPTS writes the database still refused, it logs an error, marks the
/// job stranded (`Ledger::strand`: no wait is offered on it) and returns, so a runner lets its
/// job go: the job then waits, still searching or queued, until the next start of the server,
/// whose reconcile resumes or ends it.
pub(crate) async fn fail_job(
    ledger: &Ledger,
    user: &UserId,
    job: &str,
    failure: Failure,
    search_id: Option<String>,
    change: Option<i64>,
) {
    info!("Job {job} failed: {}", logged(&failure));
    let written = retried(
        move || {
            let fail = Transition::Fail {
                failure: failure.clone(),
                search_id: search_id.clone(),
                change,
            };
            ledger.transition(user, job, fail)
        },
        |e: &LedgerError, wait: Duration| {
            warn!("Could not fail job {job}, trying again in {wait:?}: {e}")
        },
    )
    .await;
    if let Err(e) = written {
        error!(
            "Could not fail job {job} in {FAIL_ATTEMPTS} writes, leaving it to the next start: {e}"
        );
        ledger.strand(job);
    }
}

/// Runs `attempt` until it succeeds, FAIL_ATTEMPTS times at most, waiting 1 s after the first
/// try and twice as long after each next one, up to FAIL_RETRY_MAX; `retrying` logs each error
/// it waits after. The last try's error when every try failed.
async fn retried<T, E, F>(
    mut attempt: impl FnMut() -> F,
    retrying: impl Fn(&E, Duration),
) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let mut wait = Duration::from_secs(1);
    for _ in 1..FAIL_ATTEMPTS {
        match attempt().await {
            Ok(done) => return Ok(done),
            Err(e) => retrying(&e, wait),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(FAIL_RETRY_MAX);
    }
    attempt().await
}

/// What a queued job's runner decided on before it waits for a download place: the search it
/// picked from and the job's change number after its own last change.
struct Decided {
    search_id: Option<String>,
    change: i64,
}

impl Runner<'_> {
    /// Asks the writer for `change`: whether it applied. Refused, the job moved on without the
    /// runner (D15); the writer logs one it could not make.
    async fn transition(&self, change: Transition) -> bool {
        self.transition_at(change).await.is_some()
    }

    /// `transition`, with the change number it was applied under.
    async fn transition_at(&self, change: Transition) -> Option<i64> {
        let applied = self
            .acquire
            .ledger
            .transition(&self.ctx.id(), &self.job.id, change)
            .await;
        match applied {
            Ok(Transitioned::Applied { seq, .. }) => Some(seq),
            _ => None,
        }
    }

    /// The search the runner read its job with: a change it decides from that read names it, so
    /// a job cancelled and searched again meanwhile is left to its new search's runner (D15).
    fn read_on(&self) -> Option<String> {
        self.job.search_id.clone()
    }

    /// `fail_job` of the runner's job, decided on `search_id`.
    async fn fail(&self, failure: Failure, search_id: Option<String>) {
        self.fail_decided(failure, search_id, None).await;
    }

    /// `fail_job` of the runner's job, decided on `search_id` and, when given, the job's change
    /// number `change`.
    async fn fail_decided(&self, failure: Failure, search_id: Option<String>, change: Option<i64>) {
        let user = self.ctx.id();
        fail_job(
            &self.acquire.ledger,
            &user,
            &self.job.id,
            failure,
            search_id,
            change,
        )
        .await;
    }

    /// The searching job's search, then its pick: what the job was queued on, or None when it
    /// waits for a choice, failed or moved on. Every write after the search names it, so a job
    /// cancelled and searched again meanwhile is left to its new search's runner (D15); the
    /// search it ended under (renewed, for one that expired) is the one the job is queued on.
    async fn search_and_pick(&self) -> Option<Decided> {
        let Some(first) = self.job.search_id.clone() else {
            // The writer gives every searching job its search: one without cannot be answered.
            warn!("Job {} is searching without a search", self.job.id);
            self.fail(search_unanswered(), None).await;
            return None;
        };
        let (search_id, searched) = self.searched(first).await;
        let groups = match searched {
            Ok(groups) => groups,
            Err(Stop::Fail(failure)) => {
                self.fail(failure, Some(search_id)).await;
                return None;
            }
            Err(Stop::Quietly) => return None,
        };
        let view = match self.view(&search_id, &groups, Stage::SourceSearch).await {
            Ok(view) => view,
            Err(failure) => {
                self.fail(failure, Some(search_id)).await;
                return None;
            }
        };
        if view.candidates.is_empty() {
            self.fail(no_candidate_passed(), Some(search_id)).await;
            return None;
        }
        match pick(&view.candidates, self.options) {
            Picked::Choose => {
                self.transition(Transition::AwaitChoice { search_id }).await;
                None
            }
            Picked::Auto(at) => {
                let items = numbered(view.candidates[at].group.items.clone());
                let picked = Transition::Pick {
                    items,
                    search_id: search_id.clone(),
                };
                let change = self.transition_at(picked).await?;
                Some(Decided {
                    search_id: Some(search_id),
                    change,
                })
            }
        }
    }

    /// The groups of the job's search `id` and the id it ended under: a stored search that ended
    /// is read as it is, one this process runs is waited for, one a restart cut off runs again
    /// under its id, one not stored yet runs for the job's query, and one that expired runs
    /// again under a new id (the old one's candidate ids keep naming what they named).
    async fn searched(&self, mut id: String) -> (String, Result<Vec<DownloadableGroup>, Stop>) {
        let searches = &self.acquire.searches;
        let progress = loop {
            let stored = match store::search_of(&self.acquire.db, &id, &self.ctx.id()).await {
                Ok(stored) => stored,
                Err(e) => {
                    let failure = database_failure(Stage::SourceSearch, e);
                    return (id, Err(Stop::Fail(failure)));
                }
            };
            let Some(stored) = stored else {
                break self.run(&id, None).await;
            };
            match SearchProgress::of(&stored) {
                SearchProgress::Running if searches.runs(&id) => searches.until_left(&id).await,
                SearchProgress::Running => break self.run(&id, Some(&stored)).await,
                _ if stored.expires_at <= chrono::Utc::now().timestamp() => {
                    let renewed = uuid::Uuid::new_v4().to_string();
                    let change = Transition::SearchAgain {
                        from: id.clone(),
                        search_id: renewed.clone(),
                    };
                    if !self.transition(change).await {
                        return (id, Err(Stop::Quietly));
                    }
                    id = renewed;
                    break self.run(&id, Some(&stored)).await;
                }
                ended => break Ok(ended),
            }
        };
        let groups = match progress {
            Ok(SearchProgress::Done(groups, _)) => Ok(groups),
            Ok(SearchProgress::Failed(failure, _)) => Err(Stop::Fail(failure)),
            // A search task answers how its search ended.
            Ok(SearchProgress::Running) => Err(Stop::Fail(search_unanswered())),
            Err(failure) => Err(Stop::Fail(failure)),
        };
        (id, groups)
    }

    /// Runs the job's search `id` and waits for how it ended: for the query and spec of
    /// `stored` (a search run again), else for the job's query, an album's tracklist read
    /// first, with the job's filters.
    async fn run(
        &self,
        id: &str,
        stored: Option<&StoredSearch>,
    ) -> Result<SearchProgress, Failure> {
        let spec = || StoredSpec {
            filters: self.options.filters.clone(),
            ..StoredSpec::default()
        };
        let (query, spec) = match stored {
            Some(stored) => (
                stored.query.clone(),
                stored.spec.clone().unwrap_or_else(spec),
            ),
            None => {
                let mut query = self.job.wanted.clone();
                if self.job.kind == JobKind::Album {
                    self.acquire.effects.hydrate(&mut query).await?;
                }
                (query, spec())
            }
        };
        let backend = (self.acquire.effects.backends())
            .get(DEFAULT_DOWNLOAD_BACKEND)
            .await
            .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))?;
        let run = SearchRun {
            id: id.to_string(),
            query,
            spec,
        };
        let task = start_stored(
            &self.acquire.searches,
            &self.acquire.ledger,
            self.ctx,
            backend,
            run,
        )
        .await?;
        task.await.map_err(|e| {
            warn!("The search of job {} stopped: {e}", self.job.id);
            search_unanswered()
        })
    }

    /// `groups` of search `search_id` as the job sees them: its filters, the sources it
    /// excludes, and each source's recent failures. A failed read is a failure at `stage`.
    async fn view(
        &self,
        search_id: &str,
        groups: &[DownloadableGroup],
        stage: Stage,
    ) -> Result<CandidateView, Failure> {
        let since = chrono::Utc::now().timestamp() - RECENT_FAILURES_SECS;
        let failures = store::recent_failures(&self.acquire.db, since)
            .await
            .map_err(|e| database_failure(stage, e))?;
        Ok(view(
            search_id,
            groups,
            &self.options.filters,
            &failures,
            &self.job.excluded_sources,
        ))
    }

    /// The queued job holding a download place: its slots' current picks enqueued into a batch
    /// of its own, the automatic candidates of its search standing by.
    async fn download(&self) {
        let (backend, fresh, acquisition) = match self.request().await {
            Ok(Some(request)) => request,
            // Queued with nothing to send: it gives up the place it holds by failing.
            Ok(None) => return self.fail(nothing_enqueued(), self.read_on()).await,
            Err(failure) => return self.fail(failure, self.read_on()).await,
        };
        let enqueued = self
            .acquire
            .effects
            .enqueue(self.ctx, backend, fresh, acquisition);
        if let Err(failure) = enqueued.await {
            info!("Job {} could not enqueue: {}", self.job.id, failure.message);
        }
    }

    /// D8: waits until the job holds one of its user's download places, trying again each time
    /// one is given up or the job is taken on again or changed by a call (`look_again`). False
    /// once the job left queued, or failed because the database refused the claim of a place or
    /// the read of the job: that failure names what the runner `decided` on, so it is refused for
    /// a job that moved on meanwhile (a search renewed, or cancelled and queued again on the same
    /// search).
    async fn take_place(&self, look_again: &Notify, decided: &Decided) -> bool {
        let mut places = self.acquire.ledger.places();
        let failure = loop {
            // Seen before the claim, so a place given up after it wakes this one.
            places.borrow_and_update();
            match (self.acquire.ledger)
                .claim_download(&self.job.id, MCP_DOWNLOADS_PER_USER)
                .await
            {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => break database_failure(Stage::Enqueue, e),
            }
            match store::job_record(&self.acquire.db, &self.job.id, &self.ctx.id()).await {
                Ok(Some(job)) if job.state == JobState::Queued => {}
                Ok(_) => return false,
                Err(e) => break database_failure(Stage::Enqueue, e),
            }
            tokio::select! {
                changed = places.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                }
                () = look_again.notified() => {}
            }
        };
        let (search_id, change) = (decided.search_id.clone(), Some(decided.change));
        self.fail_decided(failure, search_id, change).await;
        false
    }

    /// What the job's enqueue asks for: the backend its picks name, a batch of its own
    /// importing tracks as singletons and an album whole, skipping what the library has, and its
    /// slots' current picks with the automatic candidates of its search as alternates. None when
    /// it has no queued slot left.
    async fn request(
        &self,
    ) -> Result<Option<(Arc<dyn DownloadBackend>, NewBatch, Acquisition)>, Failure> {
        let items = store::queued_picks(&self.acquire.db, &self.job.id)
            .await
            .map_err(|e| database_failure(Stage::Enqueue, e))?;
        if items.is_empty() {
            return Ok(None);
        }
        let picked: Vec<DownloadableItem> = items.iter().map(|(_, item)| item.clone()).collect();
        let routed = backend_of(&picked)?.to_string();
        let backend = (self.acquire.effects.backends())
            .get(&routed)
            .await
            .map_err(|e| backend_unavailable_at(Stage::Enqueue, &e))?;
        let target = prepare_target(&self.job.folder).await?;
        let fresh = NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: manual_batch_label(&picked),
            backend: routed,
            target,
            import_mode: match self.job.kind {
                JobKind::Track => ImportMode::Singleton,
                JobKind::Album => ImportMode::Album,
            },
            duplicates: DuplicateRule::for_job(BatchOrigin::Mcp, Some(self.options)),
        };
        let acquisition = Acquisition {
            job: self.job.id.clone(),
            alternates: self
                .alternates(self.job.search_id.as_deref(), &picked)
                .await?,
            items,
            max_sources: self.options.max_sources as usize,
        };
        Ok(Some((backend, fresh, acquisition)))
    }

    /// The automatic candidates of search `search_id` without `picked`, best first; none when
    /// the search is gone or found nothing.
    async fn alternates(
        &self,
        search_id: Option<&str>,
        picked: &[DownloadableItem],
    ) -> Result<Vec<DownloadableGroup>, Failure> {
        let Some(search_id) = search_id else {
            return Ok(Vec::new());
        };
        let stored = store::search_of(&self.acquire.db, search_id, &self.ctx.id())
            .await
            .map_err(|e| database_failure(Stage::Enqueue, e))?;
        let groups = stored.and_then(|s| s.candidates).unwrap_or_default();
        let view = self.view(search_id, &groups, Stage::Enqueue).await?;
        Ok(alternates_from(&view.automatic_groups(), picked))
    }
}

#[cfg(test)]
pub(crate) use test_effects::TestEffects;

#[cfg(test)]
mod test_effects {
    use super::*;
    use crate::core::jobs::test_support::add_alice;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::importer_health::blocking_failure_of;
    use crate::ledger::JobEvent;
    use crate::server_fns::download::test_support::{StubBackend, StubBackends};
    use crate::services::BackendLookupError;
    use shared::download::EnqueueStatus;
    use shared::metadata::{AlbumWithTracks, SearchResult, Track};
    use soulbeet::{MetadataError, MetadataProvider};
    use std::sync::Mutex;

    impl Acquire {
        /// An `Acquire` on `db` with its own ledger writer, line of searches and import queue
        /// (importing through the `TestEffects`' importer, its ledger events reaching that
        /// ledger), and a download folder of its own, acting through the `TestEffects` it
        /// returns, which holds that folder until both are dropped.
        pub(crate) fn for_test(db: SqlitePool) -> (Arc<Acquire>, Arc<TestEffects>) {
            let ledger = Ledger::start(db.clone());
            let importer = Arc::new(StubImporter::new());
            let stub = StubBackend::new(vec![]);
            let effects = Arc::new(TestEffects {
                ledger: ledger.clone(),
                backends: StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub.clone()),
                stub,
                importer: importer.clone(),
                downloads: tempfile::tempdir().expect("a test download folder"),
                library: tempfile::tempdir().expect("a test library folder"),
                tracks: Mutex::default(),
                hydrate_error: Mutex::default(),
                enqueued: Mutex::default(),
            });
            let deps = RecordingDeps::into_ledger(importer, ledger.clone());
            let acquire = Acquire {
                ledger,
                db,
                effects: effects.clone(),
                searches: Arc::default(),
                queue: ImportQueue::new(deps),
                download_base: effects.downloads.path().to_path_buf(),
                runners: Runners::default(),
            };
            (Arc::new(acquire), effects)
        }

        /// `for_test` of a new database holding alice and her one folder, `alice-folder`, at
        /// the `TestEffects`' library folder, which lives as long as they do.
        pub(crate) async fn for_alice() -> (Arc<Acquire>, Arc<TestEffects>) {
            let (acquire, effects) = Acquire::for_test(crate::db::test_db::migrated().await);
            add_alice(&acquire.db, effects.library()).await;
            (acquire, effects)
        }
    }

    /// Looks up the `StubBackend` it holds under the default backend id, blocks imports as its
    /// `StubImporter`'s health says (ready until `importer().set_health`), the importer the queue
    /// imports through, and hydrates through `hydrate_in` on a provider whose album has the tracks
    /// `set_tracks` gave (none until then), or whose `get_album` answers the error `fail_hydrate`
    /// gave. Keeps every enqueue and answers it with each item enqueued, recording only its send
    /// (Enqueued), as enqueue_and_watch records it before the backend hears of it.
    pub(crate) struct TestEffects {
        ledger: Ledger,
        stub: Arc<StubBackend>,
        backends: StubBackends,
        importer: Arc<StubImporter>,
        /// The download folder of the `Acquire` made with it.
        downloads: tempfile::TempDir,
        /// A library folder of its own, for a user's folder to be at.
        library: tempfile::TempDir,
        tracks: Mutex<Vec<Track>>,
        hydrate_error: Mutex<Option<MetadataError>>,
        enqueued: Mutex<Vec<(NewBatch, Acquisition)>>,
    }

    impl TestEffects {
        pub(crate) fn stub(&self) -> &StubBackend {
            &self.stub
        }

        pub(crate) fn stub_backends(&self) -> &StubBackends {
            &self.backends
        }

        pub(crate) fn importer(&self) -> &StubImporter {
            &self.importer
        }

        pub(crate) fn set_tracks(&self, tracks: Vec<Track>) {
            *self.tracks.lock().unwrap() = tracks;
        }

        pub(crate) fn fail_hydrate(&self, e: MetadataError) {
            *self.hydrate_error.lock().unwrap() = Some(e);
        }

        /// Its library folder, on disk while it lives.
        pub(crate) fn library(&self) -> &std::path::Path {
            self.library.path()
        }

        /// Makes every lookup of the default backend answer `e`.
        pub(crate) fn fail_backend(&self, e: BackendLookupError) {
            self.backends.fail(DEFAULT_DOWNLOAD_BACKEND, e);
        }

        /// Every enqueue, in order.
        pub(crate) fn enqueued(&self) -> Vec<(NewBatch, Acquisition)> {
            self.enqueued.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Effects for TestEffects {
        fn backends(&self) -> &dyn Backends {
            &self.backends
        }

        async fn importer_blocked(&self) -> Option<Failure> {
            blocking_failure_of(self.importer.as_ref())
        }

        async fn hydrate(&self, query: &mut DownloadQuery) -> Result<(), Failure> {
            let album = match self.hydrate_error.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(self.tracks.lock().unwrap().clone()),
            };
            metadata::hydrate_in(&Tracklist(album), query).await
        }

        async fn enqueue(
            &self,
            _ctx: &UserCtx,
            _backend: Arc<dyn DownloadBackend>,
            fresh: NewBatch,
            acquisition: Acquisition,
        ) -> Result<Vec<QueuedDownload>, Failure> {
            let answers = (acquisition.items.iter())
                .map(|(_, item)| QueuedDownload {
                    id: format!("t-{}", item.id),
                    source: item.source.clone(),
                    item: item.id.clone(),
                    size: item.size.unwrap_or(0),
                    status: EnqueueStatus::Enqueued,
                })
                .collect();
            let sent = JobEvent::Enqueued {
                job: acquisition.job.clone(),
                monitor_batch_id: fresh.batch_id.clone(),
                batch_label: fresh.label.clone(),
                request: uuid::Uuid::new_v4().to_string(),
            };
            (self.ledger.record_stored(sent).await)
                .map_err(|e| database_failure(Stage::Enqueue, e))?;
            self.enqueued.lock().unwrap().push((fresh, acquisition));
            Ok(answers)
        }
    }

    /// A provider whose every album has these tracks, or which answers this error.
    struct Tracklist(Result<Vec<Track>, MetadataError>);

    #[async_trait]
    impl MetadataProvider for Tracklist {
        async fn search_albums(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(vec![])
        }
        async fn search_tracks(
            &self,
            _: Option<&str>,
            _: &str,
            _: usize,
        ) -> Result<Vec<SearchResult>, MetadataError> {
            Ok(vec![])
        }
        async fn get_album(&self, id: &str) -> Result<AlbumWithTracks, MetadataError> {
            let tracks = self.0.clone()?;
            Ok(AlbumWithTracks {
                album: shared::metadata::Album {
                    id: id.to_string(),
                    title: String::new(),
                    artist: String::new(),
                    release_date: None,
                    mbid: Some(id.to_string()),
                    cover_url: None,
                    release_types: None,
                },
                tracks,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::test_support::{
        mcp_album_job, mcp_track_job, queued_track_jobs, record_failures,
    };
    use crate::core::sources::no_candidate_passed;
    use crate::core::test_ctx;
    use crate::ledger::{JobEvent, SourceFilters};
    use crate::server_fns::download::test_support::{group, item, search_result, track};
    use crate::services::BackendLookupError;
    use shared::download::{FileFacts, SearchResult, SearchState};
    use shared::failure::{FailureCode, RetryAdvice, Service};
    use soulbeet::error::{MissingSetting, SoulseekError};
    use soulbeet::MetadataError;
    use std::time::Duration;

    // The tests that search run on the real clock: a paused one jumps ahead while SQLite answers
    // on its own thread, timing out the pool. Each search polls once, after 2 s.

    fn candidate(source: &str, score: f64, failures: u32) -> Candidate {
        Candidate {
            id: format!("s1:{source}"),
            group: group(
                source,
                vec![item(source, "O Holy Night", &format!("{source}/03.flac"))],
                score,
            ),
            recent_failures: failures,
        }
    }

    fn auto(min_score: f64) -> JobOptions {
        JobOptions {
            pick: Pick::Auto,
            min_score,
            filters: SourceFilters::default(),
            max_sources: 4,
            replace: None,
        }
    }

    fn alice() -> UserCtx {
        test_ctx("alice")
    }

    /// The groups of one completed search poll.
    fn found(groups: Vec<DownloadableGroup>) -> Vec<soulbeet::error::Result<SearchResult>> {
        vec![Ok(search_result("search", SearchState::Completed, groups))]
    }

    #[test]
    fn a_pick_takes_the_best_unflagged_source_at_min_score() {
        let candidates = vec![
            candidate("peer_a", 0.95, 3),
            candidate("peer_b", 0.8, 0),
            candidate("peer_c", 0.6, 0),
        ];
        assert_eq!(pick(&candidates, &auto(0.7)), Picked::Auto(1));
        assert_eq!(
            pick(&candidates, &auto(0.9)),
            Picked::Choose,
            "only a flagged source is above it"
        );
        let review = JobOptions {
            pick: Pick::Review,
            ..auto(0.1)
        };
        assert_eq!(pick(&candidates, &review), Picked::Choose);
        // A file scored below the floor matches another recording: only the assistant takes it.
        let mut weak = candidate("peer_d", 0.79, 0);
        weak.group.items[0].below_floor = true;
        let candidates = vec![weak, candidate("peer_c", 0.6, 0)];
        assert_eq!(pick(&candidates, &auto(0.7)), Picked::Choose);
        let view = CandidateView {
            candidates,
            dropped_by_filter: Default::default(),
        };
        let standing_by: Vec<String> = (view.automatic_groups().into_iter())
            .map(|g| g.source)
            .collect();
        assert_eq!(standing_by, vec!["peer_c".to_string()]);
    }

    #[tokio::test]
    async fn a_flagged_source_is_never_picked_or_kept_as_an_alternate() {
        let (acq, effects) = Acquire::for_alice().await;
        record_failures(&acq, "peer_a", 3).await;
        effects.stub().script_polls(found(vec![
            group(
                "peer_a",
                vec![item("peer_a", "O Holy Night", "a/03.flac")],
                0.95,
            ),
            group(
                "peer_b",
                vec![item("peer_b", "O Holy Night", "b/03.flac")],
                0.85,
            ),
            group(
                "peer_c",
                vec![item("peer_c", "O Holy Night", "c/03.flac")],
                0.75,
            ),
        ]));
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto(0.7)).await;
        drive(&acq, &alice(), &job).await;
        let enqueued = effects.enqueued();
        let [(fresh, acquisition)] = &enqueued[..] else {
            panic!("{} enqueues", enqueued.len())
        };
        assert_eq!(
            (acquisition.items.iter())
                .map(|(slot, i)| (*slot, i.source.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "peer_b")]
        );
        assert_eq!(
            (acquisition.alternates.iter())
                .map(|g| g.source.as_str())
                .collect::<Vec<_>>(),
            vec!["peer_c"]
        );
        assert_eq!(
            (
                fresh.import_mode,
                fresh.duplicates.clone(),
                acquisition.max_sources
            ),
            (ImportMode::Singleton, DuplicateRule::Skip, 4)
        );
        assert_eq!(acquisition.job, job);
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Queued)
        );
    }

    #[tokio::test]
    async fn a_replacing_job_downloads_into_a_replacing_batch() {
        let (acq, effects) = Acquire::for_alice().await;
        effects.stub().script_polls(found(vec![group(
            "peer_a",
            vec![item("peer_a", "O Holy Night", "a/03.flac")],
            0.95,
        )]));
        let earlier = effects.library().join("x.flac");
        std::fs::write(&earlier, b"audio").unwrap();
        let set = crate::core::delete_guard::ReplaceSet {
            grant_id: "g1".into(),
            files: vec![crate::core::delete_guard::test_support::previewed_file(
                "s1", &earlier,
            )],
            expires_at: 1_800_000_000,
        };
        let replacing = JobOptions {
            replace: Some(set.clone()),
            ..auto(0.7)
        };
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", replacing).await;
        drive(&acq, &alice(), &job).await;
        let enqueued = effects.enqueued();
        let [(fresh, _)] = &enqueued[..] else {
            panic!("{} enqueues", enqueued.len())
        };
        assert_eq!(fresh.duplicates, DuplicateRule::Replace(set));
    }

    #[tokio::test]
    async fn a_review_job_waits_for_a_choice_and_downloads_nothing() {
        let (acq, effects) = Acquire::for_alice().await;
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![item("peer_b", "O Holy Night", "b/03.flac")],
            0.85,
        )]));
        let review = JobOptions {
            pick: Pick::Review,
            ..auto(0.7)
        };
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", review).await;
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        assert!(effects.enqueued().is_empty());
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::NeedsSourceChoice)
        );
    }

    #[tokio::test]
    async fn candidates_that_all_fail_the_jobs_filters_fail_it_instead_of_waiting() {
        let (acq, effects) = Acquire::for_alice().await;
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![DownloadableItem {
                facts: FileFacts {
                    extension: Some("mp3".into()),
                    bitrate_kbps: Some(320),
                    ..FileFacts::default()
                },
                ..item("peer_b", "O Holy Night", "b/03.mp3")
            }],
            0.85,
        )]));
        let lossless = JobOptions {
            filters: SourceFilters {
                lossless_only: true,
                ..Default::default()
            },
            ..auto(0.7)
        };
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", lossless).await;
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Failed)
        );
        assert_eq!(stored_failure(&acq, &job).await, no_candidate_passed());
        assert!(effects.enqueued().is_empty());
        // The search was stored with the job's filters.
        let search_id = (store::job_record(&acq.db, &job, &alice().id()).await)
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        let search = (store::search_of(&acq.db, &search_id, &alice().id()).await)
            .unwrap()
            .unwrap();
        assert!(search.spec.unwrap().filters.lossless_only);
    }

    /// The failure stored as `job`'s own.
    async fn stored_failure(acq: &Acquire, job: &str) -> Failure {
        let stored: String = sqlx::query_scalar("SELECT failure FROM jobs WHERE id = ?")
            .bind(job)
            .fetch_one(&acq.db)
            .await
            .unwrap();
        serde_json::from_str(&stored).unwrap()
    }

    #[tokio::test]
    async fn a_job_cancelled_while_it_searches_stays_cancelled() {
        let (acq, effects) = Acquire::for_alice().await;
        let (reached, release) = effects.stub().hold_searches();
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![item("peer_b", "O Holy Night", "b/03.flac")],
            0.85,
        )]));
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto(0.7)).await;
        let running = tokio::spawn({
            let (acq, job) = (acq.clone(), job.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        reached.notified().await;
        let cancelled = acq
            .ledger
            .transition(&alice().id(), &job, Transition::Cancel)
            .await
            .unwrap();
        assert!(matches!(
            cancelled,
            Transitioned::Applied {
                from: JobState::Searching,
                to: JobState::Cancelled,
                ..
            }
        ));
        release.notify_one();
        running.await.unwrap();
        acq.ledger.settled().await;
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Cancelled)
        );
        assert!(effects.enqueued().is_empty());
    }

    #[tokio::test]
    async fn the_seventh_download_waits_for_a_place() {
        let (acq, effects) = Acquire::for_alice().await;
        let jobs = queued_track_jobs(&acq, 7).await;
        for job in &jobs[..6] {
            drive(&acq, &alice(), job).await;
        }
        assert_eq!(effects.enqueued().len(), 6);
        let seventh = tokio::spawn({
            let (acq, job) = (acq.clone(), jobs[6].clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(effects.enqueued().len(), 6);
        assert!(!seventh.is_finished(), "it waits for a place");
        acq.ledger.record(JobEvent::Ended {
            job: jobs[0].clone(),
            state: JobState::Failed,
            failure: None,
        });
        tokio::time::timeout(Duration::from_secs(10), seventh)
            .await
            .expect("a freed place wakes it")
            .unwrap();
        let enqueued = effects.enqueued();
        assert_eq!(enqueued.len(), 7);
        assert_eq!(enqueued[6].1.job, jobs[6]);
        // Each picked through the source choice, with no search to take alternates from.
        assert_eq!(
            (enqueued[6].1.items.iter())
                .map(|(slot, i)| (*slot, i.id.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "a/07.flac")]
        );
        assert!(enqueued[6].1.alternates.is_empty());
    }

    #[tokio::test]
    async fn a_job_cancelled_while_it_waits_for_a_place_is_left_to_its_cancel() {
        let (acq, effects) = Acquire::for_alice().await;
        let jobs = queued_track_jobs(&acq, 7).await;
        for job in &jobs[..6] {
            drive(&acq, &alice(), job).await;
        }
        let seventh = tokio::spawn({
            let (acq, job) = (acq.clone(), jobs[6].clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        acq.ledger
            .transition(&alice().id(), &jobs[6], Transition::Cancel)
            .await
            .unwrap();
        acq.ledger.record(JobEvent::Ended {
            job: jobs[0].clone(),
            state: JobState::Failed,
            failure: None,
        });
        tokio::time::timeout(Duration::from_secs(10), seventh)
            .await
            .expect("it stops once its job left queued")
            .unwrap();
        assert_eq!(effects.enqueued().len(), 6);
        assert_eq!(
            store::job_state(&acq.db, &jobs[6]).await,
            Some(JobState::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_runner_that_cannot_enqueue_leaves_a_job_cancelled_and_searched_again_meanwhile() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // The job as its runner read it, queued under its first search.
        let read = (store::job_record(&acq.db, &job, &alice().id()).await)
            .unwrap()
            .unwrap();
        let first = read.search_id.clone();
        // cancel_job, then retry_job new_search, land before the runner's enqueue fails: the
        // writes those calls make.
        let user = alice().id();
        let renewed = uuid::Uuid::new_v4().to_string();
        for change in [
            Transition::Cancel,
            Transition::Search {
                search_id: renewed.clone(),
                filters: None,
            },
        ] {
            let done = acq.ledger.transition(&user, &job, change).await.unwrap();
            assert!(matches!(done, Transitioned::Applied { .. }), "{done:?}");
        }
        effects.fail_backend(BackendLookupError::Backend(
            SoulseekError::NotConfigured(MissingSetting::Url).into(),
        ));
        let (ctx, options) = (alice(), read.options.clone().unwrap());
        let runner = Runner {
            acquire: &acq,
            ctx: &ctx,
            job: &read,
            options: &options,
        };
        runner.download().await;
        acq.ledger.settled().await;
        let now = store::job_record(&acq.db, &job, &user)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first, Some(renewed.clone()));
        assert_eq!(
            (now.state, now.search_id),
            (JobState::Searching, Some(renewed))
        );
        assert!(effects.enqueued().is_empty());
    }

    #[tokio::test]
    async fn a_failure_the_database_keeps_refusing_is_given_up_after_its_attempts() {
        let (acq, _) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // Every write is refused from now on.
        acq.db.close().await;
        tokio::time::pause();
        let (user, start) = (alice().id(), tokio::time::Instant::now());
        let failing = fail_job(&acq.ledger, &user, &job, nothing_enqueued(), None, None);
        let given_up = tokio::time::timeout(Duration::from_secs(3600), failing).await;
        let waited = start.elapsed();
        tokio::time::resume();
        assert!(given_up.is_ok(), "fail_job never gave up");
        // FAIL_ATTEMPTS writes with the waits between them: 1, 2, 4, 8, 16 and 30 seconds.
        assert!(
            (Duration::from_secs(61)..Duration::from_secs(62)).contains(&waited),
            "{waited:?}"
        );
        assert!(acq.ledger.stranded(&job), "no wait is offered on it");
    }

    /// From now on the database refuses every claim of a download place.
    async fn refuse_places(acq: &Acquire) {
        sqlx::query(
            "CREATE TRIGGER refuse_places BEFORE UPDATE OF gate_held ON jobs \
             WHEN NEW.gate_held = 1 BEGIN SELECT RAISE(FAIL, 'disk I/O error'); END",
        )
        .execute(&acq.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_place_that_cannot_be_claimed_fails_a_job_queued_on_a_renewed_search() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto(0.7)).await;
        let user = alice().id();
        let old = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        // Its search ended and expired: the runner searches again under a new id and picks.
        acq.ledger
            .record_search(crate::ledger::NewSearch {
                id: old.clone(),
                user: user.clone(),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
                spec: Some(StoredSpec::default()),
            })
            .await
            .unwrap();
        acq.ledger
            .search_ended(&old, Ok((vec![], Some(Default::default()))))
            .await
            .unwrap();
        sqlx::query("UPDATE searches SET expires_at = 0 WHERE id = ?")
            .bind(&old)
            .execute(&acq.db)
            .await
            .unwrap();
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![item("peer_b", "O Holy Night", "b/03.flac")],
            0.85,
        )]));
        refuse_places(&acq).await;
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        let now = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap();
        assert_ne!(now.search_id, Some(old));
        assert_eq!(
            now.state,
            JobState::Failed,
            "the failure names the renewed search, so it is not refused"
        );
    }

    #[tokio::test]
    async fn a_runner_that_cannot_claim_a_place_leaves_a_job_cancelled_and_queued_again_meanwhile()
    {
        let (acq, _) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        let user = alice().id();
        // The job as its runner read it, queued and holding no place.
        let read = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap();
        let picks = store::queued_picks(&acq.db, &job).await.unwrap();
        // cancel_job, then retry_job same_source, land before the runner's claim fails: the job
        // is queued again on the same search.
        for change in [
            Transition::Cancel,
            Transition::Queue {
                items: picks,
                search_id: None,
            },
        ] {
            let done = acq.ledger.transition(&user, &job, change).await.unwrap();
            assert!(matches!(done, Transitioned::Applied { .. }), "{done:?}");
        }
        refuse_places(&acq).await;
        let (ctx, options) = (alice(), read.options.clone().unwrap());
        let runner = Runner {
            acquire: &acq,
            ctx: &ctx,
            job: &read,
            options: &options,
        };
        let decided = Decided {
            search_id: read.search_id.clone(),
            change: read.changed_seq,
        };
        assert!(!runner.take_place(&Notify::new(), &decided).await);
        acq.ledger.settled().await;
        let now = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap();
        assert_eq!(
            (now.state, now.search_id),
            (JobState::Queued, read.search_id),
            "left to the runner of the job as it is now"
        );
    }

    /// Whether `job` holds a download place.
    async fn holds_place(acq: &Acquire, job: &str) -> bool {
        sqlx::query_scalar("SELECT gate_held FROM jobs WHERE id = ?")
            .bind(job)
            .fetch_one(&acq.db)
            .await
            .unwrap()
    }

    /// The failure on the row of `job`'s first slot.
    async fn first_slot_failure(acq: &Acquire, job: &str) -> Failure {
        let tracks = store::tracks_of(&acq.db, job).await.unwrap();
        (tracks[0].row.clone())
            .and_then(|row| row.failure)
            .expect("a failed slot")
    }

    #[tokio::test]
    async fn a_job_its_runner_cannot_read_is_read_again_then_stranded() {
        let (acq, _) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // Every read is refused from now on.
        acq.db.close().await;
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let given_up =
            tokio::time::timeout(Duration::from_secs(3600), drive(&acq, &alice(), &job)).await;
        let waited = start.elapsed();
        tokio::time::resume();
        assert!(given_up.is_ok(), "the runner never gave up");
        // FAIL_ATTEMPTS reads with the waits between them: 1, 2, 4, 8, 16 and 30 seconds.
        assert!(
            (Duration::from_secs(61)..Duration::from_secs(62)).contains(&waited),
            "{waited:?}"
        );
        assert!(acq.ledger.stranded(&job), "no wait is offered on it");
    }

    #[tokio::test]
    async fn a_job_that_cannot_be_read_once_it_holds_a_place_fails_and_gives_the_place_up() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // The claim of its place leaves its row unreadable, so the read after it fails.
        sqlx::query(&format!(
            "CREATE TRIGGER unreadable AFTER UPDATE OF gate_held ON jobs \
             WHEN NEW.id = '{job}' AND NEW.gate_held = 1 \
             BEGIN UPDATE jobs SET wanted = 'unreadable' WHERE id = NEW.id; END"
        ))
        .execute(&acq.db)
        .await
        .unwrap();
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        assert!(effects.enqueued().is_empty());
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Failed)
        );
        assert!(!holds_place(&acq, &job).await);
        let failure = first_slot_failure(&acq, &job).await;
        assert_eq!(
            (failure.code, failure.stage),
            (FailureCode::SettingsUnreadable, Stage::Enqueue)
        );
    }

    #[tokio::test]
    async fn a_job_cancelled_once_it_holds_a_place_and_then_unread_ends_cancelled_without_it() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // A cancel lands with the claim of its place (it asks for the cancel and moves the job's
        // change number on), and the read after them fails.
        sqlx::query(&format!(
            "CREATE TRIGGER cancelled AFTER UPDATE OF gate_held ON jobs \
             WHEN NEW.id = '{job}' AND NEW.gate_held = 1 \
             BEGIN UPDATE jobs SET wanted = 'unreadable', cancel_requested = 1, \
             changed_seq = changed_seq + 1 WHERE id = NEW.id; END"
        ))
        .execute(&acq.db)
        .await
        .unwrap();
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        assert!(effects.enqueued().is_empty());
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Cancelled)
        );
        assert!(!holds_place(&acq, &job).await);
    }

    #[tokio::test]
    async fn a_job_that_cannot_be_read_while_it_waits_for_a_place_fails() {
        let (acq, effects) = Acquire::for_alice().await;
        let jobs = queued_track_jobs(&acq, 7).await;
        for job in &jobs[..6] {
            drive(&acq, &alice(), job).await;
        }
        let (waiting, user) = (&jobs[6], alice().id());
        // The job as its runner read it, queued and holding no place.
        let read = (store::job_record(&acq.db, waiting, &user).await)
            .unwrap()
            .unwrap();
        // Every place is taken, and every read of the job fails from now on.
        sqlx::query("UPDATE jobs SET wanted = 'unreadable' WHERE id = ?")
            .bind(waiting)
            .execute(&acq.db)
            .await
            .unwrap();
        let (ctx, options) = (alice(), read.options.clone().unwrap());
        let runner = Runner {
            acquire: &acq,
            ctx: &ctx,
            job: &read,
            options: &options,
        };
        let decided = Decided {
            search_id: read.search_id.clone(),
            change: read.changed_seq,
        };
        assert!(!runner.take_place(&Notify::new(), &decided).await);
        acq.ledger.settled().await;
        assert_eq!(effects.enqueued().len(), 6);
        assert_eq!(
            store::job_state(&acq.db, waiting).await,
            Some(JobState::Failed)
        );
        let failure = first_slot_failure(&acq, waiting).await;
        assert_eq!(
            (failure.code, failure.stage),
            (FailureCode::SettingsUnreadable, Stage::Enqueue)
        );
    }

    #[tokio::test]
    async fn a_job_taken_on_again_while_its_runner_waits_for_a_place_is_enqueued_once() {
        let (acq, effects) = Acquire::for_alice().await;
        let jobs = queued_track_jobs(&acq, 7).await;
        for job in &jobs[..6] {
            drive(&acq, &alice(), job).await;
        }
        let waiting = &jobs[6];
        let first = tokio::spawn({
            let (acq, job) = (acq.clone(), waiting.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Cancelled without a place, then picked again: a second runner for it.
        let user = alice().id();
        acq.ledger
            .transition(&user, waiting, Transition::Cancel)
            .await
            .unwrap();
        let again = Transition::Queue {
            items: vec![(0, item("peer_b", "Song Number 7", "b/07.flac"))],
            search_id: None,
        };
        acq.ledger.transition(&user, waiting, again).await.unwrap();
        let second = tokio::spawn({
            let (acq, job) = (acq.clone(), waiting.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(effects.enqueued().len(), 6, "no place is free yet");
        acq.ledger.record(JobEvent::Ended {
            job: jobs[0].clone(),
            state: JobState::Failed,
            failure: None,
        });
        for runner in [first, second] {
            tokio::time::timeout(Duration::from_secs(10), runner)
                .await
                .expect("both runners end")
                .unwrap();
        }
        let enqueued = effects.enqueued();
        assert_eq!(enqueued.len(), 7, "the job was enqueued once");
        assert_eq!(enqueued[6].1.items[0].1.source, "peer_b");
    }

    #[tokio::test]
    async fn an_album_job_downloads_its_folder_as_one_album() {
        let (acq, effects) = Acquire::for_alice().await;
        effects.set_tracks(vec![
            track("Kowloon", "Wake Up"),
            track("Kowloon", "Come Over"),
        ]);
        effects.stub().script_polls(found(vec![group(
            "peer_a",
            vec![
                item("peer_a", "Wake Up", "a/07.flac"),
                item("peer_a", "Come Over", "a/08.flac"),
            ],
            0.9,
        )]));
        let job = mcp_album_job(
            &acq,
            "Kowloon",
            "Come Over",
            "6a09041b-0f79-3278-88d9-0552dd28d37f",
        )
        .await;
        drive(&acq, &alice(), &job).await;
        let enqueued = effects.enqueued();
        let [(fresh, acquisition)] = &enqueued[..] else {
            panic!("{} enqueues", enqueued.len())
        };
        assert_eq!(
            (fresh.import_mode, fresh.duplicates.clone()),
            (ImportMode::Album, DuplicateRule::Skip)
        );
        assert_eq!(
            (acquisition.items.iter())
                .map(|(slot, i)| (*slot, i.id.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "a/07.flac"), (1, "a/08.flac")]
        );
        assert_eq!(fresh.label, "Come Over");
        assert_eq!(
            (effects.stub().specs()[0].tracks.iter())
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Wake Up", "Come Over"]
        );
    }

    #[tokio::test]
    async fn a_tracklist_that_cannot_be_read_fails_the_job_with_its_failure() {
        let (acq, effects) = Acquire::for_alice().await;
        let mbid = "6a09041b-0f79-3278-88d9-0552dd28d37f";
        // What musicbrainz_rs says of a release MusicBrainz does not have (src/error.rs:17), on
        // the URL find_album asks (lib/soulbeet/src/musicbrainz.rs:351-362, musicbrainz_rs
        // src/api/query.rs:29-46), as From maps it (lib/soulbeet/src/musicbrainz.rs:95-99).
        let raw = format!("Musicbrainz returned \"Not found\" for query \"http://musicbrainz.org/ws/2/release/{mbid}?fmt=json&inc=recordings+artist-credits+release-groups\"");
        effects.fail_hydrate(MetadataError::NotFound {
            service: Service::Musicbrainz,
            status: None,
            raw: raw.clone(),
        });
        let job = mcp_album_job(&acq, "Kowloon", "Come Over", mbid).await;
        drive(&acq, &alice(), &job).await;
        acq.ledger.settled().await;
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Failed)
        );
        let f = stored_failure(&acq, &job).await;
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::MetadataNotFound,
                Stage::Metadata,
                RetryAdvice::OtherQuery
            )
        );
        assert_eq!(f.cause.unwrap().raw, raw);
        assert!(effects.stub().specs().is_empty(), "nothing was searched");
    }

    #[tokio::test]
    async fn a_job_reads_the_search_handed_to_it_and_waits_for_one_still_running() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto(0.7)).await;
        let search_id = (store::job_record(&acq.db, &job, &alice().id()).await)
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        // A search another task started under the job's search id, still running.
        let (reached, release) = effects.stub().hold_searches();
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![item("peer_b", "O Holy Night", "b/03.flac")],
            0.85,
        )]));
        let run = SearchRun {
            id: search_id.clone(),
            query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
            spec: StoredSpec::default(),
        };
        let backend = (acq.effects.backends())
            .get(DEFAULT_DOWNLOAD_BACKEND)
            .await
            .unwrap();
        let search = start_stored(&acq.searches, &acq.ledger, &alice(), backend, run)
            .await
            .unwrap();
        reached.notified().await;
        let running = tokio::spawn({
            let (acq, job) = (acq.clone(), job.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(effects.enqueued().is_empty() && !running.is_finished());
        release.notify_one();
        search.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the search's end wakes it")
            .unwrap();
        assert_eq!(effects.stub().specs().len(), 1, "it searched once");
        let enqueued = effects.enqueued();
        assert_eq!(enqueued.len(), 1);
        assert_eq!(enqueued[0].1.items[0].1.source, "peer_b");
    }

    #[tokio::test]
    async fn a_job_whose_search_expired_searches_again_under_a_new_id() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto(0.7)).await;
        let user = alice().id();
        let old = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        let stored = vec![group(
            "peer_a",
            vec![item("peer_a", "O Holy Night", "a/03.flac")],
            0.9,
        )];
        acq.ledger
            .record_search(crate::ledger::NewSearch {
                id: old.clone(),
                user: user.clone(),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
                spec: Some(StoredSpec::default()),
            })
            .await
            .unwrap();
        acq.ledger
            .search_ended(&old, Ok((stored.clone(), Some(Default::default()))))
            .await
            .unwrap();
        sqlx::query("UPDATE searches SET expires_at = 0 WHERE id = ?")
            .bind(&old)
            .execute(&acq.db)
            .await
            .unwrap();
        effects.stub().script_polls(found(vec![group(
            "peer_b",
            vec![item("peer_b", "O Holy Night", "b/03.flac")],
            0.85,
        )]));
        drive(&acq, &alice(), &job).await;
        let renewed = (store::job_record(&acq.db, &job, &user).await)
            .unwrap()
            .unwrap()
            .search_id
            .unwrap();
        assert_ne!(renewed, old);
        let kept = store::search_of(&acq.db, &old, &user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            kept.candidates,
            Some(stored),
            "the old candidate ids name what they named"
        );
        let enqueued = effects.enqueued();
        assert_eq!(enqueued[0].1.items[0].1.source, "peer_b");
    }
}
