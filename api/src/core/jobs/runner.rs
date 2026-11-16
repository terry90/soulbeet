//! What MCP acquisition acts through (the database, the ledger, the one line of MCP searches
//! and the effects outside them), and the job runner that takes an MCP job from its search to
//! its enqueue.

use crate::auth::UserCtx;
use crate::core::acquire::{
    alternates_from, enqueue_and_watch, manual_batch_label, numbered, prepare_target, Acquisition,
    NewBatch,
};
use crate::core::sources::{
    no_candidate_passed, start_stored, view, Candidate, CandidateView, SearchProgress, SearchRun,
    Searches, RECENT_FAILURES_SECS,
};
use crate::core::{backend_unavailable, database_failure, metadata};
use crate::ledger::store::{self, JobRecord, StoredSearch};
use crate::ledger::{
    DuplicateRule, JobKind, JobOptions, JobState, Ledger, Pick, StoredSpec, Transition,
    Transitioned, LEDGER,
};
use crate::services::download_backend;
use async_trait::async_trait;
use shared::download::{DownloadQuery, DownloadableGroup, DownloadableItem, QueuedDownload};
use shared::failure::{Failure, Stage};
use soulbeet::{DownloadBackend, ImportMode};
use sqlx::SqlitePool;
use std::sync::{Arc, LazyLock};
use tracing::{info, warn};

/// What acquisition does outside the ledger, so tests run it on stubs.
#[async_trait]
pub(crate) trait Effects: Send + Sync {
    /// The default download backend.
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure>;
    /// Fills an album query's empty tracklist from its metadata provider.
    async fn hydrate(&self, ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure>;
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
}

/// The server's: its database, LEDGER, the live effects and one line of searches.
pub(crate) static ACQUIRE: LazyLock<Arc<Acquire>> = LazyLock::new(|| {
    Arc::new(Acquire {
        db: (*crate::db::DB).clone(),
        ledger: LEDGER.clone(),
        effects: Arc::new(LiveEffects),
        searches: Arc::default(),
    })
});

/// The services the server is configured with.
struct LiveEffects;

#[async_trait]
impl Effects for LiveEffects {
    async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
        download_backend(None)
            .await
            .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))
    }

    async fn hydrate(&self, ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure> {
        let key = metadata::lastfm_key(ctx).await?;
        metadata::hydrate_album_tracks(query, key.as_deref()).await
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
    /// Every candidate waits for Claude (pick review, or none at min_score from an unflagged
    /// source).
    Choose,
}

/// D4, on `view`'s candidates, best first (never empty: drive fails a job whose view is empty
/// first): the best one at or above `min_score` whose source is not flagged.
pub(crate) fn pick(candidates: &[Candidate], options: &JobOptions) -> Picked {
    if options.pick == Pick::Review {
        return Picked::Choose;
    }
    candidates
        .iter()
        .position(|c| !c.flagged() && c.group.score >= options.min_score)
        .map_or(Picked::Choose, Picked::Auto)
}

/// Takes job `id` of `ctx` on from its state: searching (hydrate an album, search or read its
/// search, pick), then queued (wait for a download place, then enqueue). Returns when the job is
/// enqueued, waits for a choice, ends, or a transition it asked for was refused (another call
/// moved the job, D15). A web UI job has its own pipeline and is left to it.
pub(crate) async fn drive(acquire: &Arc<Acquire>, ctx: &UserCtx, id: &str) {
    let job = match store::job_record(&acquire.db, id, &ctx.id()).await {
        Ok(Some(job)) => job,
        Ok(None) => return,
        Err(e) => {
            warn!("Could not read job {id} to take it on: {e}");
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
    let search_id = match job.state {
        JobState::Searching => match runner.search_and_pick().await {
            Some(search_id) => Some(search_id),
            None => return,
        },
        JobState::Queued => job.search_id.clone(),
        _ => return,
    };
    runner.download(search_id.as_deref()).await;
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
    /// Another call moved the job, or the search's task stopped without an answer.
    Quietly,
}

impl Runner<'_> {
    /// Asks the writer for `change`: whether it applied. Refused, the job moved on without the
    /// runner (D15); the writer logs one it could not make.
    async fn transition(&self, change: Transition) -> bool {
        let applied = self
            .acquire
            .ledger
            .transition(&self.ctx.id(), &self.job.id, change)
            .await;
        matches!(applied, Ok(Transitioned::Applied { .. }))
    }

    async fn fail(&self, failure: Failure) {
        info!("Job {} failed: {}", self.job.id, failure.message);
        self.transition(Transition::Fail { failure }).await;
    }

    /// The searching job's search, then its pick: the id of the search it was queued from, or
    /// None when it waits for a choice, failed or moved on.
    async fn search_and_pick(&self) -> Option<String> {
        let (search_id, groups) = match self.searched().await {
            Ok(found) => found,
            Err(Stop::Fail(failure)) => {
                self.fail(failure).await;
                return None;
            }
            Err(Stop::Quietly) => return None,
        };
        let view = match self.view(&search_id, &groups, Stage::SourceSearch).await {
            Ok(view) => view,
            Err(failure) => {
                self.fail(failure).await;
                return None;
            }
        };
        if view.candidates.is_empty() {
            self.fail(no_candidate_passed()).await;
            return None;
        }
        match pick(&view.candidates, self.options) {
            Picked::Choose => {
                self.transition(Transition::AwaitChoice).await;
                None
            }
            Picked::Auto(at) => {
                let items = numbered(view.candidates[at].group.items.clone());
                let picked = self.transition(Transition::Pick { items }).await;
                picked.then_some(search_id)
            }
        }
    }

    /// The groups of the job's search and its id: a stored search that ended is read as it is,
    /// one this process runs is waited for, one a restart cut off runs again under its id, one
    /// not stored yet runs for the job's query, and one that expired runs again under a new id
    /// (the old one's candidate ids keep naming what they named).
    async fn searched(&self) -> Result<(String, Vec<DownloadableGroup>), Stop> {
        let Some(mut id) = self.job.search_id.clone() else {
            warn!("Job {} is searching without a search", self.job.id);
            return Err(Stop::Quietly);
        };
        let searches = &self.acquire.searches;
        let progress = loop {
            let stored = store::search_of(&self.acquire.db, &id, &self.ctx.id())
                .await
                .map_err(|e| Stop::Fail(database_failure(Stage::SourceSearch, e)))?;
            let Some(stored) = stored else {
                break self.run(&id, None).await?;
            };
            match SearchProgress::of(&stored) {
                SearchProgress::Running if searches.runs(&id) => searches.until_left(&id).await,
                SearchProgress::Running => break self.run(&id, Some(&stored)).await?,
                _ if stored.expires_at <= chrono::Utc::now().timestamp() => {
                    let renewed = uuid::Uuid::new_v4().to_string();
                    let change = Transition::SearchAgain {
                        search_id: renewed.clone(),
                    };
                    if !self.transition(change).await {
                        return Err(Stop::Quietly);
                    }
                    id = renewed;
                    break self.run(&id, Some(&stored)).await?;
                }
                ended => break ended,
            }
        };
        match progress {
            SearchProgress::Done(groups, _) => Ok((id, groups)),
            SearchProgress::Failed(failure, _) => Err(Stop::Fail(failure)),
            // A search task answers how its search ended.
            SearchProgress::Running => Err(Stop::Quietly),
        }
    }

    /// Runs the job's search `id` and waits for how it ended: for the query and spec of
    /// `stored` (a search run again), else for the job's query, an album's tracklist read
    /// first, with the job's filters.
    async fn run(&self, id: &str, stored: Option<&StoredSearch>) -> Result<SearchProgress, Stop> {
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
                    (self.acquire.effects.hydrate(self.ctx, &mut query).await)
                        .map_err(Stop::Fail)?;
                }
                (query, spec())
            }
        };
        let backend = self.acquire.effects.backend().await.map_err(Stop::Fail)?;
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
        .await
        .map_err(Stop::Fail)?;
        task.await.map_err(|e| {
            warn!("The search of job {} stopped: {e}", self.job.id);
            Stop::Quietly
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

    /// The queued job, once it holds a download place: its slots' current picks enqueued into a
    /// batch of its own, the unflagged candidates of its search `search_id` standing by.
    async fn download(&self, search_id: Option<&str>) {
        if !self.take_place().await {
            return;
        }
        let (backend, fresh, acquisition) = match self.request(search_id).await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(failure) => return self.fail(failure).await,
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
    /// one is given up. False once the job left queued.
    async fn take_place(&self) -> bool {
        let mut places = self.acquire.ledger.places();
        loop {
            // Seen before the claim, so a place given up after it wakes this one.
            places.borrow_and_update();
            match (self.acquire.ledger)
                .claim_download(&self.job.id, MCP_DOWNLOADS_PER_USER)
                .await
            {
                Ok(true) => return true,
                Ok(false) => {}
                Err(_) => return false,
            }
            match store::job_record(&self.acquire.db, &self.job.id, &self.ctx.id()).await {
                Ok(Some(job)) if job.state == JobState::Queued => {}
                Ok(_) => return false,
                Err(e) => {
                    warn!(
                        "Could not read job {} waiting to download: {e}",
                        self.job.id
                    );
                    return false;
                }
            }
            if places.changed().await.is_err() {
                return false;
            }
        }
    }

    /// What the job's enqueue asks for: the backend, a batch of its own importing tracks as
    /// singletons and an album whole (step 5 D1), skipping what the library has, and its slots'
    /// current picks with the unflagged candidates of search `search_id` as alternates. None
    /// when it has no queued slot left.
    async fn request(
        &self,
        search_id: Option<&str>,
    ) -> Result<Option<(Arc<dyn DownloadBackend>, NewBatch, Acquisition)>, Failure> {
        let backend = (self.acquire.effects.backend().await).map_err(|f| Failure {
            stage: Stage::Enqueue,
            ..f
        })?;
        let target = prepare_target(&self.job.folder).await?;
        let items = store::queued_picks(&self.acquire.db, &self.job.id)
            .await
            .map_err(|e| database_failure(Stage::Enqueue, e))?;
        if items.is_empty() {
            return Ok(None);
        }
        let picked: Vec<DownloadableItem> = items.iter().map(|(_, item)| item.clone()).collect();
        let fresh = NewBatch {
            batch_id: uuid::Uuid::new_v4().to_string(),
            label: manual_batch_label(&picked),
            target,
            import_mode: match self.job.kind {
                JobKind::Track => ImportMode::Singleton,
                JobKind::Album => ImportMode::Album,
            },
            duplicates: DuplicateRule::Skip,
        };
        let acquisition = Acquisition {
            job: self.job.id.clone(),
            alternates: self.alternates(search_id, &picked).await?,
            items,
            max_sources: self.options.max_sources as usize,
        };
        Ok(Some((backend, fresh, acquisition)))
    }

    /// The unflagged candidates of search `search_id` without `picked`, best first; none when
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
        Ok(alternates_from(&view.unflagged_groups(), picked))
    }
}

#[cfg(test)]
pub(crate) use test_effects::TestEffects;

#[cfg(test)]
mod test_effects {
    use super::*;
    use crate::server_fns::download::test_support::StubBackend;
    use shared::download::EnqueueStatus;
    use shared::metadata::{AlbumWithTracks, Provider, SearchResult, Track};
    use soulbeet::{MetadataError, MetadataProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::watch;

    impl Acquire {
        /// An `Acquire` on `db` with its own ledger writer and line of searches, acting through
        /// the `TestEffects` it returns.
        pub(crate) fn for_test(db: SqlitePool) -> (Arc<Acquire>, Arc<TestEffects>) {
            let effects = Arc::new(TestEffects {
                stub: StubBackend::new(vec![]),
                tracks: Mutex::default(),
                hydrate_error: Mutex::default(),
                backend_held: watch::Sender::new(false),
                backend_calls_held: AtomicUsize::new(0),
                enqueued: Mutex::default(),
            });
            let acquire = Acquire {
                ledger: Ledger::start(db.clone()),
                db,
                effects: effects.clone(),
                searches: Arc::default(),
            };
            (Arc::new(acquire), effects)
        }
    }

    /// Answers the `StubBackend` it holds, and hydrates through step 3's `hydrate_in` on a
    /// provider whose album has the tracks `set_tracks` gave (none until then), or whose
    /// `get_album` answers the error `fail_hydrate` gave. Keeps every enqueue and answers it
    /// with each item enqueued, recording nothing.
    pub(crate) struct TestEffects {
        stub: Arc<StubBackend>,
        tracks: Mutex<Vec<Track>>,
        hydrate_error: Mutex<Option<MetadataError>>,
        /// While true, `backend` calls wait for it to turn false.
        backend_held: watch::Sender<bool>,
        backend_calls_held: AtomicUsize,
        enqueued: Mutex<Vec<(NewBatch, Acquisition)>>,
    }

    impl TestEffects {
        pub(crate) fn stub(&self) -> &StubBackend {
            &self.stub
        }

        pub(crate) fn set_tracks(&self, tracks: Vec<Track>) {
            *self.tracks.lock().unwrap() = tracks;
        }

        pub(crate) fn fail_hydrate(&self, e: MetadataError) {
            *self.hydrate_error.lock().unwrap() = Some(e);
        }

        /// Holds every `backend` call from now on until `release_backend`.
        pub(crate) fn hold_backend(&self) {
            self.backend_held.send_replace(true);
        }

        pub(crate) fn release_backend(&self) {
            self.backend_held.send_replace(false);
        }

        /// The `backend` calls waiting for `release_backend`.
        pub(crate) fn backend_calls_held(&self) -> usize {
            self.backend_calls_held.load(Ordering::SeqCst)
        }

        /// Every enqueue, in order.
        pub(crate) fn enqueued(&self) -> Vec<(NewBatch, Acquisition)> {
            self.enqueued.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Effects for TestEffects {
        async fn backend(&self) -> Result<Arc<dyn DownloadBackend>, Failure> {
            let mut held = self.backend_held.subscribe();
            if *held.borrow_and_update() {
                self.backend_calls_held.fetch_add(1, Ordering::SeqCst);
                let _ = held.wait_for(|held| !held).await;
                self.backend_calls_held.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(self.stub.clone())
        }

        async fn hydrate(&self, _ctx: &UserCtx, query: &mut DownloadQuery) -> Result<(), Failure> {
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
            self.enqueued.lock().unwrap().push((fresh, acquisition));
            Ok(answers)
        }
    }

    /// A provider whose every album has these tracks, or which answers this error.
    struct Tracklist(Result<Vec<Track>, MetadataError>);

    #[async_trait]
    impl MetadataProvider for Tracklist {
        fn id(&self) -> Provider {
            Provider::MusicBrainz
        }
        fn name(&self) -> &'static str {
            "MusicBrainz"
        }
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
        mcp_album_job, mcp_track_job, queued_track_jobs, record_failures, test_db_with_alice,
    };
    use crate::core::sources::no_candidate_passed;
    use crate::core::test_ctx;
    use crate::ledger::{JobEvent, SourceFilters};
    use crate::server_fns::download::test_support::{group, item, search_result, track};
    use shared::download::{FileFacts, SearchResult, SearchState};
    use shared::failure::{FailureCode, RetryAdvice, Service};
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
    }

    #[tokio::test]
    async fn a_flagged_source_is_never_picked_or_kept_as_an_alternate() {
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
            (fresh.import_mode, fresh.duplicates, acquisition.max_sources),
            (ImportMode::Singleton, DuplicateRule::Skip, 4)
        );
        assert_eq!(acquisition.job, job);
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Queued)
        );
    }

    #[tokio::test]
    async fn a_review_job_waits_for_a_choice_and_downloads_nothing() {
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
    async fn an_album_job_downloads_its_folder_as_one_album() {
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
            (fresh.import_mode, fresh.duplicates),
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let backend = acq.effects.backend().await.unwrap();
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
            .search_ended(&old, Ok((stored.clone(), Default::default())))
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
