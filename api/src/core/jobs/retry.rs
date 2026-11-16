//! What retry_job, cancel_job, dismiss_job and download_source do to a job (design 11.5). Every
//! state change is a transition the ledger's writer checks against the job's state, so of two
//! calls racing on one job only one changes it.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use shared::download::{DownloadableItem, SourcePresence};
use shared::failure::{cut_chars, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use soulbeet::error::PeerAsk;
use soulbeet::ImportMode;

use super::decide::in_download_folder;
use super::runner::{fail_job, spawn_drive, Acquire};
use super::{
    allowed, done_source, folder_for, job_already_running, job_failure, job_not_found, job_view,
    over_limit, source_tried, takes_another_source, untried, CallRefused, JobView, RetryMode,
    Untried, Wanted,
};
use crate::auth::UserCtx;
use crate::core::acquire::{numbered, AUTO_SELECT_SCORE_THRESHOLD};
use crate::core::sources::{
    self, candidate_not_found, owned_search, parse_candidate_id, search_not_found, start_stored,
    view, SearchRun,
};
use crate::core::{database_failure, logged};
use crate::import_queue::FileLeft;
use crate::ledger::store::{self, StoredSearch};
use crate::ledger::{
    Admitted, BatchOrigin, JobKind, JobOptions, JobState, NewJob, NewJobBatch, Pick, SourceFilters,
    StoredSpec, Transition, Transitioned,
};
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;

/// What a call on a job did.
#[derive(Debug)]
pub(crate) enum RetryAnswer {
    /// The change was made: the job as it is now.
    Applied(Box<JobView>),
    /// The change to job `job` was made, but the job could not be read after it: `failure` says
    /// why.
    AppliedUnread { job: String, failure: Failure },
    /// The job does not take the call as it is now (its state, its kind, its candidates, its
    /// recording already in the library), so the same call is refused until the job changes.
    Refused(Failure, Map<String, Value>),
    /// Something the call reached failed or held it back (the backend, the source, MusicBrainz,
    /// the import queue, the user's folders, the database, the user's job limit): the failure's
    /// advice says whether the same call can work later. Nothing was changed.
    Unavailable(Failure, Map<String, Value>),
    /// The job cannot take what an argument says: `field` must be `expected`.
    InvalidInput {
        field: &'static str,
        expected: String,
    },
}

/// What new_search searches with: other words, other filters, or a find_sources search.
#[derive(Debug, Default)]
pub(crate) struct NewSearchArgs {
    pub query_text: Option<String>,
    pub filters: Option<SourceFilters>,
    pub search_id: Option<String>,
}

/// Which job download_source gives the candidate to.
#[derive(Debug)]
pub(crate) enum SourceTarget {
    /// The caller's job of this id.
    Job(String),
    /// A new job for `wanted` in a batch of its own, in folder `folder_id` or the default one.
    New {
        wanted: Wanted,
        folder_id: Option<String>,
    },
}

/// What retry_job, cancel_job and download_source say of a job started in the web page, whose
/// pipeline writes its changes unchecked, so the ledger cannot stop it.
const WEB_PAGE_JOB: &str = "a job started by ensure_in_library or download_source; this one was started in Soulful's web page, where it can be cancelled or retried";

/// What download_source says of a job whose file is in the library already, waiting for
/// Navidrome to list it.
const INDEXED_JOB: &str = "a job whose file is not in the library yet; this one's file is imported and waits for Navidrome to list it, which retry_job index_only waits for again";

/// wrong_job_state: the job is in `state`, and `action` takes a job in one of `allowed`. Facts
/// `state` and `allowed_states`.
pub(crate) fn wrong_job_state(
    state: JobState,
    action: &str,
    allowed: &[JobState],
) -> (Failure, Map<String, Value>) {
    let names: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
    let states = match names.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    };
    let failure = Failure::new(
        FailureCode::WrongJobState,
        Stage::Input,
        RetryAdvice::NotRetryable,
        format!(
            "The job is {}; {action} needs a job that is {states}",
            state.as_str()
        ),
    );
    let facts = Map::from_iter([
        ("state".to_string(), json!(state.as_str())),
        ("allowed_states".to_string(), json!(names)),
    ]);
    (failure, facts)
}

/// next_source found no candidate of the job's search it has not tried.
pub(crate) fn sources_exhausted() -> Failure {
    Failure::new(
        FailureCode::SourcesExhausted,
        Stage::SourceSearch,
        RetryAdvice::OtherQuery,
        "No untried source is left in this job's search",
    )
}

/// A done job's file is not what the user wanted (reject_result).
pub(crate) fn result_rejected() -> Failure {
    Failure::new(
        FailureCode::ResultRejected,
        Stage::Import,
        RetryAdvice::OtherSource,
        "The imported file is not the recording wanted",
    )
}

/// same_source of a source the backend says is offline; the source is in the facts.
pub(crate) fn source_offline() -> Failure {
    Failure::new(
        FailureCode::PeerOffline,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        "The source is offline",
    )
}

/// same_source of a source slskd's blacklist holds, from which nothing downloads.
pub(crate) fn source_blocked() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::OtherSource,
        "slskd's blacklist holds this source",
    )
}

/// `job_view` of a job the caller named: job_not_found when it is no job of `ctx`'s.
async fn named_job(acquire: &Acquire, ctx: &UserCtx, job: &str) -> Result<JobView, Failure> {
    job_view(acquire, ctx, job).await?.ok_or_else(job_not_found)
}

/// The answer to a change made to `ctx`'s job `job`: the job as it is now, or the change made
/// without it when the job cannot be read.
async fn as_changed(acquire: &Acquire, ctx: &UserCtx, job: &str) -> RetryAnswer {
    match named_job(acquire, ctx, job).await {
        Ok(view) => RetryAnswer::Applied(Box::new(view)),
        Err(failure) => RetryAnswer::AppliedUnread {
            job: job.to_string(),
            failure,
        },
    }
}

/// The refusal of a job started in the web page.
fn web_page_job(view: &JobView) -> Option<RetryAnswer> {
    (view.job.origin == BatchOrigin::Ui).then(web_page_refusal)
}

/// What a job started in the web page is refused with: it keeps none of the options an MCP job's
/// changes write.
fn web_page_refusal() -> RetryAnswer {
    RetryAnswer::InvalidInput {
        field: "job_id",
        expected: WEB_PAGE_JOB.to_string(),
    }
}

fn refused((failure, facts): (Failure, Map<String, Value>)) -> RetryAnswer {
    RetryAnswer::Refused(failure, facts)
}

/// `source` as a fact: a peer's name, cut.
fn source_facts(source: &str) -> Map<String, Value> {
    let source = cut_chars(source.to_string(), TEXT_MAX_CHARS);
    Map::from_iter([("source".to_string(), json!(source))])
}

/// Asks the writer for `change` on `view`'s job, which `action` makes from one of `states`: the
/// job as it is after the change, its runner started when the change queued it. A change refused
/// for the job's state is wrong_job_state.
async fn apply(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    view: &JobView,
    change: Transition,
    action: &str,
    states: &[JobState],
) -> Result<RetryAnswer, Failure> {
    let id = &view.job.id;
    let changed = (acquire.ledger.transition(&ctx.id(), id, change).await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    match changed {
        Transitioned::Applied { to, .. } => {
            if to == JobState::Queued {
                spawn_drive(acquire.clone(), ctx.clone(), id.clone());
            }
            Ok(as_changed(acquire, ctx, id).await)
        }
        Transitioned::Refused { state } => Ok(refused(wrong_job_state(state, action, states))),
        Transitioned::Unknown => Err(job_not_found()),
        Transitioned::NoOptions => Ok(web_page_refusal()),
    }
}

/// One mode of retry_job (design 11.5), on `ctx`'s job `job`, refused as `refusal` says when
/// `allowed` says no. `new_search` holds what new_search searches with.
pub(crate) async fn retry(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    job: &str,
    mode: RetryMode,
    new_search: Option<NewSearchArgs>,
) -> Result<RetryAnswer, Failure> {
    let view = named_job(acquire, ctx, job).await?;
    if let Some(refusal) = web_page_job(&view) {
        return Ok(refusal);
    }
    let now = chrono::Utc::now().timestamp();
    let untried = untried(&acquire.db, &view, now).await?;
    if !allowed(&view, &untried, mode) {
        return Ok(refusal(&view, &untried, mode));
    }
    let action = format!("retry_job {}", mode.as_str());
    let change = match mode {
        RetryMode::SameSource => {
            let Some((slot, item)) = source_tried(&view) else {
                return Ok(refusal(&view, &untried, mode));
            };
            if let Err(refused) = source_ready(acquire, &item.source).await {
                return Ok(refused);
            }
            Transition::Queue {
                items: vec![(slot, item.clone())],
                search_id: None,
            }
        }
        RetryMode::NextSource => {
            let Some(best) = untried.best() else {
                return Ok(refusal(&view, &untried, mode));
            };
            Transition::Queue {
                items: numbered(best.group.items.clone()),
                search_id: None,
            }
        }
        RetryMode::NewSearch => {
            let args = new_search.unwrap_or_default();
            return search_again(acquire, ctx, &view, args, &action).await;
        }
        RetryMode::IndexOnly => Transition::Reindex { now },
        RetryMode::RejectResult => {
            let Some(source) = done_source(&view) else {
                return Ok(refusal(&view, &untried, mode));
            };
            Transition::Reject {
                source: source.to_string(),
                failure: result_rejected(),
            }
        }
    };
    apply(acquire, ctx, &view, change, &action, mode.states()).await
}

/// Why `allowed` refuses `mode` for `view`: wrong_job_state for the job's state; the job's own
/// failure only when its recording is already in the library, which no mode changes;
/// sources_exhausted for next_source when no untried candidate is left; else the modes the job
/// allows now. The job's failure of any other kind is not why a mode is refused, and answering it
/// would read as if the call itself had failed with it.
fn refusal(view: &JobView, untried: &Untried, mode: RetryMode) -> RetryAnswer {
    let job = &view.job;
    if !mode.states().contains(&job.state) {
        let action = format!("retry_job {}", mode.as_str());
        return refused(wrong_job_state(job.state, &action, mode.states()));
    }
    if let Some(f) = job_failure(view) {
        if f.code == FailureCode::ImportAlreadyInLibrary {
            return RetryAnswer::Refused(f, Map::new());
        }
    }
    let track_only = matches!(mode, RetryMode::SameSource | RetryMode::NextSource);
    if track_only && job.kind == JobKind::Album {
        return RetryAnswer::InvalidInput {
            field: "mode",
            expected: "new_search or index_only: an album job is retried whole".to_string(),
        };
    }
    if mode == RetryMode::NextSource && untried.best().is_none() {
        return RetryAnswer::Refused(sources_exhausted(), Map::new());
    }
    let modes: Vec<&str> = (RetryMode::ALL.into_iter())
        .filter(|m| allowed(view, untried, *m))
        .map(RetryMode::as_str)
        .collect();
    let expected = match modes.as_slice() {
        [] => "a mode the job allows; it allows none now".to_string(),
        modes => format!("one of {}, the modes the job allows now", modes.join(", ")),
    };
    RetryAnswer::InvalidInput {
        field: "mode",
        expected,
    }
}

/// Refuses same_source of `source` when the backend says it is offline or blocked, or cannot
/// say: then the failure says whether the same call can work later (slskd or the Soulseek server
/// did not answer) in Soulful's words, slskd's text in its cause.
async fn source_ready(acquire: &Acquire, source: &str) -> Result<(), RetryAnswer> {
    let backend = acquire.effects.backend().await.map_err(|f| {
        let f = Failure {
            stage: Stage::Enqueue,
            ..f
        };
        RetryAnswer::Unavailable(f, Map::new())
    })?;
    let refused = |failure| Err(RetryAnswer::Unavailable(failure, source_facts(source)));
    match backend.source_status(source).await {
        Ok(SourcePresence::Offline) => refused(source_offline()),
        Ok(SourcePresence::Blocked) => refused(source_blocked()),
        Ok(SourcePresence::Online | SourcePresence::Away) => Ok(()),
        Err(e) => refused(e.ask_failure(PeerAsk::Presence, Stage::Enqueue)),
    }
}

/// What a failed read of a search the caller named answers: refused for an id that names no live
/// search of theirs, unavailable when the database could not say.
fn search_refused(failure: Failure, facts: Map<String, Value>) -> RetryAnswer {
    match failure.code {
        FailureCode::UnknownHandle | FailureCode::ExpiredHandle => {
            RetryAnswer::Refused(failure, facts)
        }
        _ => RetryAnswer::Unavailable(failure, facts),
    }
}

/// The kind of job a stored search's candidates serve: an album when its query names one.
fn kind_of(search: &StoredSearch) -> JobKind {
    JobKind::of(&search.query)
}

/// What a search's candidates must be for a job of `kind`.
fn kind_expected(what: &str, kind: JobKind) -> String {
    match kind {
        JobKind::Track => format!("{what} a track search"),
        JobKind::Album => format!("{what} an album search"),
    }
}

/// new_search: the job searches again, with `args.search_id` (the caller's live search of the
/// job's kind, which the runner reads, waiting for it while it runs), else under a new id, the
/// search started here with `args.query_text` and the filters given or the job's. Filters given
/// become the job's.
async fn search_again(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    view: &JobView,
    args: NewSearchArgs,
    action: &str,
) -> Result<RetryAnswer, Failure> {
    let job = &view.job;
    let NewSearchArgs {
        query_text,
        filters,
        search_id,
    } = args;
    let (search_id, start) = match search_id {
        Some(id) => {
            let facts = Map::from_iter([("search_id".to_string(), json!(id))]);
            let stored = match owned_search(&acquire.db, ctx, &id).await {
                Ok(stored) => stored,
                Err(f) => return Ok(search_refused(f, facts)),
            };
            // A web UI search was no tool's.
            if stored.spec.is_none() {
                return Ok(RetryAnswer::Refused(search_not_found(), facts));
            }
            if kind_of(&stored) != job.kind {
                return Ok(RetryAnswer::InvalidInput {
                    field: "search_id",
                    expected: kind_expected("the id of", job.kind),
                });
            }
            (id, None)
        }
        None => {
            let mut query = job.wanted.clone();
            if job.kind == JobKind::Album {
                if let Err(f) = acquire.effects.hydrate(&mut query).await {
                    return Ok(RetryAnswer::Unavailable(f, Map::new()));
                }
            }
            let backend = match acquire.effects.backend().await {
                Ok(backend) => backend,
                Err(f) => return Ok(RetryAnswer::Unavailable(f, Map::new())),
            };
            let job_filters = job.options.as_ref().map(|o| o.filters.clone());
            let spec = StoredSpec {
                text: query_text,
                keep_below_floor: false,
                filters: (filters.clone()).or(job_filters).unwrap_or_default(),
                reference: Value::Null,
            };
            let id = uuid::Uuid::new_v4().to_string();
            let run = SearchRun {
                id: id.clone(),
                query,
                spec,
            };
            (id, Some((backend, run)))
        }
    };
    let change = Transition::Search {
        search_id: search_id.clone(),
        filters,
    };
    let changed = (acquire.ledger.transition(&ctx.id(), &job.id, change).await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    let states = RetryMode::NewSearch.states();
    match changed {
        Transitioned::Applied { .. } => {}
        Transitioned::Refused { state } => {
            return Ok(refused(wrong_job_state(state, action, states)))
        }
        Transitioned::Unknown => return Err(job_not_found()),
        Transitioned::NoOptions => return Ok(web_page_refusal()),
    }
    // Started before the runner, which then waits for it: a cancel between the two leaves a
    // stop mark that start_stored takes (D9).
    if let Some((backend, run)) = start {
        let started = start_stored(&acquire.searches, &acquire.ledger, ctx, backend, run).await;
        if let Err(failure) = started {
            return Ok(search_not_started(acquire, ctx, &job.id, &search_id, failure).await);
        }
    }
    spawn_drive(acquire.clone(), ctx.clone(), job.id.clone());
    Ok(as_changed(acquire, ctx, &job.id).await)
}

/// The search `search_id` that new_search was to start for `ctx`'s searching job `job` could not
/// be recorded, so nothing searches with what the caller gave: the job ends failed with
/// `failure` (cancelled when its user cancelled it meanwhile) and is answered as it then is. A
/// runner would search the job's own query instead, so none is started. When that write fails
/// too, a task writes it again as `fail_job` does; one it gives up is left to the next start.
async fn search_not_started(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    search_id: &str,
    failure: Failure,
) -> RetryAnswer {
    tracing::warn!(
        "Could not record search {search_id} of job {job}: {}",
        logged(&failure)
    );
    let fail = Transition::Fail {
        failure: failure.clone(),
        search_id: Some(search_id.to_string()),
        change: None,
    };
    match acquire.ledger.transition(&ctx.id(), job, fail).await {
        // Failed, or ended another way while the search was being recorded.
        Ok(_) => as_changed(acquire, ctx, job).await,
        Err(e) => {
            tracing::warn!("Could not fail job {job} after its search was not recorded: {e}");
            let (ledger, user, id) = (acquire.ledger.clone(), ctx.id(), job.to_string());
            let (search_id, again) = (search_id.to_string(), failure.clone());
            tokio::spawn(async move {
                fail_job(&ledger, &user, &id, again, Some(search_id), None).await;
            });
            RetryAnswer::AppliedUnread {
                job: job.to_string(),
                failure,
            }
        }
    }
}

/// cancel_job: `ctx`'s job `job` stops (D9). One without transfers ends cancelled at once, its
/// search stopped when it was searching; one that may have transfers is marked for every monitor
/// to stop them (the writer sets the mark), and a transfer that already finished still imports.
pub(crate) async fn cancel(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
) -> Result<RetryAnswer, Failure> {
    let view = named_job(acquire, ctx, job).await?;
    if let Some(refusal) = web_page_job(&view) {
        return Ok(refusal);
    }
    let states = Transition::Cancel.allowed_from();
    let changed = (acquire
        .ledger
        .transition(&ctx.id(), job, Transition::Cancel)
        .await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    match changed {
        Transitioned::Applied { stopped_search, .. } => {
            // The search the job held when it was cancelled, which no later search change can
            // follow. Not recorded yet only while new_search starts it; a read that fails leaves
            // a mark, which a search that never runs keeps only until it expires.
            if let Some(id) = stopped_search {
                let stored = store::search_of(&acquire.db, &id, &ctx.id()).await;
                let recorded = matches!(stored, Ok(Some(_)));
                acquire.searches.stop_job_search(&id, recorded).await;
            }
            acquire.job_changed(job);
            Ok(as_changed(acquire, ctx, job).await)
        }
        Transitioned::Refused { state } => {
            Ok(refused(wrong_job_state(state, "cancel_job", states)))
        }
        Transitioned::Unknown => Err(job_not_found()),
        Transitioned::NoOptions => Ok(web_page_refusal()),
    }
}

/// dismiss_job: `ctx`'s job `job` will not be pursued. A job waiting for an import decision first
/// has the import each of its slots still waiting keeps discarded (each import once), which
/// dismisses those slots; a kept import gone while its slot still waits (another decision took it
/// and has not recorded it yet) refuses with the queue's failure, and the call can be made again.
/// Then the job ends dismissed; one whose slots the discards all dismissed already is answered as
/// it is. The files of the discarded imports that could not be deleted come with any answer:
/// they stay on disk whatever happened after.
pub(crate) async fn dismiss(acquire: &Acquire, ctx: &UserCtx, job: &str) -> Dismissed {
    let mut files_left = Vec::new();
    let answer = dismiss_discarding(acquire, ctx, job, &mut files_left).await;
    Dismissed {
        answer,
        files_left: in_download_folder(acquire, files_left),
    }
}

/// What dismiss_job did: its answer, and the files of the kept imports it discarded that could
/// not be deleted, each path relative to the download folder.
#[derive(Debug)]
pub(crate) struct Dismissed {
    pub answer: Result<RetryAnswer, Failure>,
    pub files_left: Vec<FileLeft>,
}

/// `dismiss`, adding each discard's files left to `files_left`.
async fn dismiss_discarding(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    files_left: &mut Vec<FileLeft>,
) -> Result<RetryAnswer, Failure> {
    let view = named_job(acquire, ctx, job).await?;
    let states = Transition::Dismiss.allowed_from();
    if !states.contains(&view.job.state) {
        return Ok(refused(wrong_job_state(
            view.job.state,
            "dismiss_job",
            states,
        )));
    }
    let mut discarded = false;
    if view.job.state == JobState::NeedsDecision {
        let mut items: Vec<String> = Vec::new();
        for slot in &view.slots {
            if let (JobState::NeedsDecision, Some(row)) = (slot.state, &slot.row) {
                if !items.contains(&row.item) {
                    items.push(row.item.clone());
                }
            }
        }
        for item in items {
            match acquire.queue.discard(&ctx.id(), &item).await {
                Ok(done) => {
                    discarded = true;
                    files_left.extend(done.files_left);
                }
                Err(refusal) => {
                    let now = named_job(acquire, ctx, job).await?;
                    let still_waiting = now.slots.iter().any(|slot| {
                        slot.state == JobState::NeedsDecision
                            && slot.row.as_ref().is_some_and(|row| row.item == item)
                    });
                    if still_waiting {
                        let facts = Map::from_iter([("kept".to_string(), json!("gone"))]);
                        return Ok(RetryAnswer::Unavailable(refusal.failure, facts));
                    }
                }
            }
        }
    }
    let changed = (acquire
        .ledger
        .transition(&ctx.id(), job, Transition::Dismiss)
        .await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    match changed {
        Transitioned::Applied { .. } => {}
        // The discards dismissed every slot.
        Transitioned::Refused {
            state: JobState::Dismissed,
        } if discarded => {}
        Transitioned::Refused { state } => {
            return Ok(refused(wrong_job_state(state, "dismiss_job", states)))
        }
        Transitioned::Unknown => return Err(job_not_found()),
        Transitioned::NoOptions => return Ok(web_page_refusal()),
    }
    Ok(as_changed(acquire, ctx, job).await)
}

/// download_source: candidate `candidate_id` of the caller's live search becomes the source of
/// `target`: the caller's job (an MCP job waiting for a source choice, failed or cancelled, of
/// the candidate's kind), or a new job in a batch of its own, made after the checks
/// ensure_in_library makes (importer, backend, folder), its album's tracklist read. Its runner
/// then enqueues it.
pub(crate) async fn download_source(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    target: SourceTarget,
    candidate_id: &str,
) -> Result<RetryAnswer, Failure> {
    let (search, items) = match candidate(acquire, ctx, candidate_id).await {
        Ok(found) => found,
        Err(refusal) => return Ok(refusal),
    };
    let kind = kind_of(&search);
    let wrong_kind = |job_kind: JobKind| RetryAnswer::InvalidInput {
        field: "candidate_id",
        expected: kind_expected("a candidate of", job_kind),
    };
    let (wanted, folder_id) = match target {
        SourceTarget::Job(id) => {
            let view = named_job(acquire, ctx, &id).await?;
            if let Some(refusal) = web_page_job(&view) {
                return Ok(refusal);
            }
            if view.job.kind != kind {
                return Ok(wrong_kind(view.job.kind));
            }
            if let Some(f) = job_failure(&view) {
                if f.code == FailureCode::ImportAlreadyInLibrary {
                    return Ok(RetryAnswer::Refused(f, Map::new()));
                }
            }
            if !takes_another_source(&view) {
                return Ok(RetryAnswer::InvalidInput {
                    field: "job_id",
                    expected: INDEXED_JOB.to_string(),
                });
            }
            let change = Transition::Queue {
                items: numbered(items),
                search_id: Some(search.id.clone()),
            };
            let states = change.allowed_from();
            if !states.contains(&view.job.state) {
                let state = view.job.state;
                return Ok(refused(wrong_job_state(state, "download_source", states)));
            }
            return apply(acquire, ctx, &view, change, "download_source", states).await;
        }
        SourceTarget::New { wanted, folder_id } => (wanted, folder_id),
    };
    if wanted.kind() != kind {
        return Ok(wrong_kind(wanted.kind()));
    }
    if let Some(f) = acquire.effects.importer_blocked().await {
        return Ok(RetryAnswer::Unavailable(f, Map::new()));
    }
    if let Err(f) = acquire.effects.backend().await {
        return Ok(RetryAnswer::Unavailable(f, Map::new()));
    }
    let folder = match folder_for(&acquire.db, ctx, folder_id.as_deref()).await {
        Ok(folder) => folder,
        Err(CallRefused { failure, folders }) => {
            let listed: Vec<Value> = (folders.iter())
                .map(|f| json!({"folder_id": f.id, "name": f.name}))
                .collect();
            let facts = match listed.is_empty() {
                true => Map::new(),
                false => Map::from_iter([("folders".to_string(), json!(listed))]),
            };
            return Ok(RetryAnswer::Unavailable(failure, facts));
        }
    };
    let mut query = wanted.query();
    if let Wanted::Album(album) = &wanted {
        if let Err(f) = acquire.effects.hydrate(&mut query).await {
            let facts = Map::from_iter([("release_mbid".to_string(), json!(album.release_mbid))]);
            return Ok(RetryAnswer::Unavailable(f, facts));
        }
    }
    let batch = NewJobBatch {
        id: uuid::Uuid::new_v4().to_string(),
        user: ctx.id(),
        // Named by the writer after its one job.
        label: None,
        origin: BatchOrigin::Mcp,
        for_playlist: false,
    };
    let job = NewJob {
        id: uuid::Uuid::new_v4().to_string(),
        user: ctx.id(),
        folder_id: folder.id.clone(),
        wanted: query,
        import_mode: match kind {
            JobKind::Track => ImportMode::Singleton,
            JobKind::Album => ImportMode::Album,
        },
        state: JobState::Queued,
        search_id: Some(search.id.clone()),
        picked: numbered(items),
        kind,
        // Failover keeps to what the search was filtered by.
        options: Some(JobOptions {
            pick: Pick::Auto,
            min_score: AUTO_SELECT_SCORE_THRESHOLD,
            filters: search.spec.map(|s| s.filters).unwrap_or_default(),
            max_sources: MAX_SOURCES_PER_TRACK as u32,
        }),
    };
    let id = job.id.clone();
    let admitted = (acquire.ledger.create_batch(batch, vec![job]).await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    let Some(admitted) = admitted.into_iter().next() else {
        unreachable!("create_batch answers each job it is given");
    };
    match admitted {
        Admitted::Created => {
            spawn_drive(acquire.clone(), ctx.clone(), id.clone());
            Ok(as_changed(acquire, ctx, &id).await)
        }
        Admitted::AlreadyRunning { job, state } => Ok(refused(job_already_running(&job, state))),
        Admitted::OverLimit { unfinished } => {
            let (failure, facts) = over_limit(unfinished);
            Ok(RetryAnswer::Unavailable(failure, facts))
        }
    }
}

/// The search and files of `ctx`'s candidate `candidate_id` (D3): one of a live search a tool
/// started that ended with candidates, its files those the search's filters keep, as get_sources
/// shows them (D6). Any other id, another user's and one get_sources never shows included, reads
/// like an unknown one; an expired search is expired_handle.
async fn candidate(
    acquire: &Acquire,
    ctx: &UserCtx,
    candidate_id: &str,
) -> Result<(StoredSearch, Vec<DownloadableItem>), RetryAnswer> {
    let facts = Map::from_iter([("candidate_id".to_string(), json!(candidate_id))]);
    let unknown = || RetryAnswer::Refused(candidate_not_found(), facts.clone());
    let (search_id, index) = parse_candidate_id(candidate_id).ok_or_else(unknown)?;
    let stored = owned_search(&acquire.db, ctx, &search_id)
        .await
        .map_err(|f| match f.code {
            FailureCode::UnknownHandle => unknown(),
            _ => search_refused(f, facts.clone()),
        })?;
    let Some(spec) = &stored.spec else {
        return Err(unknown());
    };
    let groups = stored.candidates.as_deref().unwrap_or_default();
    let shown = view(&search_id, groups, &spec.filters, &HashMap::new(), &[]);
    let id = sources::candidate_id(&search_id, index);
    let items = (shown.candidates.into_iter())
        .find(|c| c.id == id)
        .map(|c| c.group.items)
        .ok_or_else(unknown)?;
    Ok((stored, items))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::core::jobs::runner::{drive, TestEffects};
    use crate::core::jobs::test_support::{
        auto_options, done_track_job, downloading_track_job, downloading_track_job_with,
        failed_track_job, failed_track_job_with, failed_web_ui_job, file_of, importing_track_job,
        in_progress, index_failed_track_job, kept_album_job, mcp_track_job, queued_track_jobs,
        stop_slots, view_of,
    };
    use crate::core::sources::{search_stopped, server_restarted_search, SearchProgress, Searches};
    use crate::core::test_ctx;
    use crate::ledger::{store, JobEvent};
    use crate::server_fns::download::test_support::{item, search_result, track, transfer};
    use crate::transfer_registry::TransferRegistry;
    use shared::download::{DownloadQuery, DownloadState, JobSlot, SearchState};
    use soulbeet::DownloadBackend;

    // On the real clock: the database answers from its own thread (a paused clock times its pool
    // out), and each search polls once, 2 s after it starts.

    fn alice() -> UserCtx {
        test_ctx("alice")
    }

    /// The enqueues once there are `n`, the job runner sending them in its own task.
    async fn enqueued(
        effects: &TestEffects,
        n: usize,
    ) -> Vec<(
        crate::core::acquire::NewBatch,
        crate::core::acquire::Acquisition,
    )> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while effects.enqueued().len() < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the runner enqueued");
        effects.enqueued()
    }

    #[tokio::test]
    async fn next_source_takes_the_best_untried_source_and_refuses_when_none_is_left() {
        let (acq, effects) = Acquire::for_alice().await;
        // Tried peer_a and carol_shares; its search also found peer_b.
        let job = failed_track_job(&acq).await;
        let RetryAnswer::Applied(view) = retry(&acq, &alice(), &job, RetryMode::NextSource, None)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(view.job.state, JobState::Queued);
        let sent = enqueued(&effects, 1).await;
        let from = &sent[0].1.items[0].1;
        assert_eq!(from.source, "peer_b");
        // Its monitor's row, then the peer is gone (Soulseek.NET src/SoulseekClient.cs:3790).
        let row = transfer(
            &from.source,
            &from.id,
            &format!("t-{}", from.id),
            DownloadState::InProgress,
        )
        .with_job(JobSlot {
            job: job.clone(),
            slot: 0,
        });
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![row.clone()],
        });
        let offline = Failure::new(
            FailureCode::PeerOffline,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            "User peer_b appears to be offline",
        );
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![row.failed_with(offline)],
        });
        acq.ledger.settled().await;
        let RetryAnswer::Refused(f, _) = retry(&acq, &alice(), &job, RetryMode::NextSource, None)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(f, sources_exhausted());
    }

    #[tokio::test]
    async fn a_cancelled_job_retried_downloads_again() {
        let (acq, effects) = Acquire::for_alice().await;
        // peer_a downloading; its search also found peer_b.
        let job = downloading_track_job(&acq).await;
        let cancelled = cancel(&acq, &alice(), &job).await.unwrap();
        assert!(
            matches!(cancelled, RetryAnswer::Applied(v) if v.job.state == JobState::Downloading)
        );
        assert!(TransferRegistry::global().job_cancelled(&job));
        stop_slots(&acq, &job).await;
        assert_eq!(view_of(&acq, &job).await.job.state, JobState::Cancelled);
        let again = retry(&acq, &alice(), &job, RetryMode::NextSource, None)
            .await
            .unwrap();
        assert!(matches!(again, RetryAnswer::Applied(_)));
        assert!(
            !TransferRegistry::global().job_cancelled(&job),
            "the new transfers are not stopped at their first poll"
        );
        assert_eq!(enqueued(&effects, 1).await[0].1.items[0].1.source, "peer_b");
    }

    #[tokio::test]
    async fn cancelling_a_searching_job_stops_its_search() {
        let (acq, effects) = Acquire::for_alice().await;
        let (reached, release) = effects.stub().hold_searches();
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![],
        ))]);
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto_options()).await;
        let running = tokio::spawn({
            let (acq, job) = (acq.clone(), job.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        reached.notified().await;
        let cancelled = cancel(&acq, &alice(), &job).await.unwrap();
        assert!(matches!(cancelled, RetryAnswer::Applied(v) if v.job.state == JobState::Cancelled));
        // slskd answers the start it was asked for; the search's task then stops it there.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            effects.stub().stopped_searches(),
            vec!["search".to_string()]
        );
        assert!(effects.enqueued().is_empty());

        // A search still waiting for the turn never runs: the stored search fails search_stopped.
        let searches = Arc::new(Searches::default());
        let _turn = searches.hold_turn_for_test().await;
        let backend: Arc<dyn DownloadBackend> = acq.effects.backend().await.unwrap();
        let run = SearchRun {
            id: "s9".into(),
            query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
            spec: StoredSpec::default(),
        };
        let waiting = start_stored(&searches, &acq.ledger, &alice(), backend, run)
            .await
            .unwrap();
        assert!(searches.stop_job_search("s9", true).await);
        assert_eq!(
            waiting.await.unwrap(),
            SearchProgress::Failed(search_stopped(), None)
        );
    }

    #[tokio::test]
    async fn a_job_searched_again_after_a_cancel_takes_nothing_from_the_cancelled_search() {
        let (acq, effects) = Acquire::for_alice().await;
        let (reached, release) = effects.stub().hold_searches();
        let found = |source: &str, file: &str| {
            Ok(search_result(
                "search",
                SearchState::Completed,
                vec![crate::server_fns::download::test_support::group(
                    source,
                    vec![item(source, "O Holy Night", file)],
                    0.9,
                )],
            ))
        };
        // The cancelled search's answer, then the new one's.
        effects.stub().script_polls(vec![
            found("peer_a", "a/03.flac"),
            found("peer_b", "b/03.flac"),
        ]);
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto_options()).await;
        let first = tokio::spawn({
            let (acq, job) = (acq.clone(), job.clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        reached.notified().await;
        let cancelled = cancel(&acq, &alice(), &job).await.unwrap();
        assert!(matches!(cancelled, RetryAnswer::Applied(v) if v.job.state == JobState::Cancelled));
        // Searched again while the cancelled search still runs.
        let words = NewSearchArgs {
            query_text: Some("Marrow Choir O Holy Night choir".into()),
            ..NewSearchArgs::default()
        };
        let again = retry(&acq, &alice(), &job, RetryMode::NewSearch, Some(words))
            .await
            .unwrap();
        assert!(matches!(again, RetryAnswer::Applied(v) if v.job.state == JobState::Searching));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(20), first)
            .await
            .unwrap()
            .unwrap();
        let sent = enqueued(&effects, 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(effects.enqueued().len(), 1, "enqueued once");
        assert_eq!(sent[0].1.items[0].1.source, "peer_b");
        assert_eq!(
            effects.stub().specs()[1].text.as_deref(),
            Some("Marrow Choir O Holy Night choir")
        );
    }

    #[tokio::test]
    async fn download_source_takes_only_the_files_the_searchs_filters_keep() {
        let (acq, effects) = Acquire::for_alice().await;
        let album = crate::core::jobs::AlbumRef {
            artist: "Kowloon".into(),
            title: "Come Over".into(),
            release_mbid: "6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
        };
        let wanted = Wanted::Album(album);
        effects.set_tracks(vec![
            track("Kowloon", "Wake Up"),
            track("Kowloon", "Come Over"),
            track("Kowloon", "Lights Out"),
        ]);
        let at = |title: &str, file: &str, kbps: u32| DownloadableItem {
            facts: shared::download::FileFacts {
                extension: Some("mp3".into()),
                bitrate_kbps: Some(kbps),
                ..Default::default()
            },
            ..item("peer_a", title, file)
        };
        let folder = vec![
            at("Wake Up", "a/07.mp3", 320),
            at("Come Over", "a/08.mp3", 128),
            at("Lights Out", "a/09.mp3", 320),
        ];
        let search_id = uuid::Uuid::new_v4().to_string();
        let spec = StoredSpec {
            filters: SourceFilters {
                min_bitrate_kbps: Some(256),
                ..SourceFilters::default()
            },
            ..StoredSpec::default()
        };
        acq.ledger
            .record_search(crate::ledger::NewSearch {
                id: search_id.clone(),
                user: alice().id(),
                backend: Some("slskd".into()),
                query: wanted.query(),
                spec: Some(spec),
            })
            .await
            .unwrap();
        let groups = vec![crate::server_fns::download::test_support::group(
            "peer_a", folder, 0.9,
        )];
        acq.ledger
            .search_ended(&search_id, Ok((groups, Some(Default::default()))))
            .await
            .unwrap();
        let target = SourceTarget::New {
            wanted,
            folder_id: Some("alice-folder".into()),
        };
        let answer = download_source(&acq, &alice(), target, &format!("{search_id}:0"))
            .await
            .unwrap();
        assert!(matches!(answer, RetryAnswer::Applied(_)), "{answer:?}");
        let sent = enqueued(&effects, 1).await;
        let files: Vec<&str> = (sent[0].1.items.iter())
            .map(|(_, item)| item.id.as_str())
            .collect();
        assert_eq!(files, vec!["a/07.mp3", "a/09.mp3"]);
    }

    /// slskd's text for a peer whose upload queue is full, which says to retry the same peer later
    /// (Application.cs:763-766).
    fn peer_busy() -> Failure {
        Failure::new(
            FailureCode::TransferRejectedPeerBusy,
            Stage::Transfer,
            RetryAdvice::SameSourceLater,
            "Transfer rejected: Too many files",
        )
    }

    #[tokio::test]
    async fn same_source_is_refused_while_the_source_is_offline() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = failed_track_job_with(&acq, "Silent Night", peer_busy()).await;
        effects.stub().set_presence(SourcePresence::Offline);
        let RetryAnswer::Unavailable(f, facts) =
            retry(&acq, &alice(), &job, RetryMode::SameSource, None)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailureCode::PeerOffline, "The source is offline")
        );
        assert_eq!(facts["source"], "peer_a");
        effects.stub().set_presence(SourcePresence::Blocked);
        let RetryAnswer::Unavailable(f, _) =
            retry(&acq, &alice(), &job, RetryMode::SameSource, None)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(f, source_blocked());
        effects.stub().set_presence(SourcePresence::Online);
        let again = retry(&acq, &alice(), &job, RetryMode::SameSource, None)
            .await
            .unwrap();
        assert!(matches!(again, RetryAnswer::Applied(v) if v.job.state == JobState::Queued));
        assert_eq!(enqueued(&effects, 1).await[0].1.items[0].1.source, "peer_a");
    }

    #[tokio::test]
    async fn same_source_takes_the_source_that_failed_not_an_alternate_failover_was_refused() {
        let (acq, effects) = Acquire::for_alice().await;
        let title = "Silent Night";
        let job = downloading_track_job_with(&acq, title, auto_options()).await;
        // peer_a's queue is full; failover's peer_b is refused at its enqueue (the backend, as
        // slskd answers a peer it cannot reach: Soulseek.NET src/SoulseekClient.cs:3790), and no
        // source is left, so the slot fails with peer_a's row.
        let left = in_progress(&job, title).failed_with(peer_busy());
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![left.clone()],
        });
        let slot = JobSlot {
            job: job.clone(),
            slot: 0,
        };
        let alternate = item("peer_b", title, &file_of("b", title));
        acq.ledger.record(JobEvent::Attempt {
            slot: slot.clone(),
            candidate: Some(Box::new(alternate.clone())),
            request: Some(uuid::Uuid::new_v4().to_string()),
        });
        let offline = Failure::new(
            FailureCode::PeerOffline,
            Stage::Enqueue,
            RetryAdvice::OtherSource,
            "User peer_b appears to be offline",
        );
        acq.ledger.record(JobEvent::Refused {
            slot,
            candidate: Box::new(alternate),
            failure: offline,
        });
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![left],
        });
        acq.ledger.settled().await;
        let view = view_of(&acq, &job).await;
        let left_out = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        assert!(allowed(&view, &left_out, RetryMode::SameSource));
        let again = retry(&acq, &alice(), &job, RetryMode::SameSource, None)
            .await
            .unwrap();
        assert!(matches!(again, RetryAnswer::Applied(v) if v.job.state == JobState::Queued));
        assert_eq!(effects.stub().presence_asked(), vec!["peer_a".to_string()]);
        assert_eq!(enqueued(&effects, 1).await[0].1.items[0].1.source, "peer_a");
    }

    #[tokio::test]
    async fn same_source_waits_for_a_presence_check_slskd_could_not_finish() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = failed_track_job_with(&acq, "Silent Night", peer_busy()).await;
        // The Soulseek server did not answer GetStatus in time (Soulseek.NET
        // SoulseekClient.cs:3866), as slskd's global handler answers it (Program.cs:1051-1053).
        let timed_out = "The wait timed out after 5000 milliseconds";
        effects.stub().fail_presence(500, timed_out);
        let RetryAnswer::Unavailable(f, facts) =
            retry(&acq, &alice(), &job, RetryMode::SameSource, None)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (f.code, f.retry, f.message.as_str()),
            (
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
                "slskd could not say whether the source is online"
            )
        );
        assert_eq!(f.cause.unwrap().raw, timed_out);
        assert_eq!(facts["source"], "peer_a");
        assert_eq!(view_of(&acq, &job).await.job.state, JobState::Failed);
    }

    /// The `expected` of an invalid_input refusal of `mode`, or a panic naming what came instead.
    fn modes_allowed(answer: RetryAnswer) -> String {
        match answer {
            RetryAnswer::InvalidInput {
                field: "mode",
                expected,
            } => expected,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_mode_the_failure_does_not_take_names_the_modes_allowed_not_the_failure() {
        let (acq, _) = Acquire::for_alice().await;
        let busy = failed_track_job_with(&acq, "Silent Night", peer_busy()).await;
        let answer = retry(&acq, &alice(), &busy, RetryMode::IndexOnly, None)
            .await
            .unwrap();
        assert_eq!(
            modes_allowed(answer),
            "one of same_source, next_source, new_search, the modes the job allows now"
        );
        // A search Soulful's restart cut off, before the job had a source.
        let cut_off = mcp_track_job(&acq, "Marrow Choir", "Coventry Carol", auto_options()).await;
        let fail = Transition::Fail {
            failure: server_restarted_search(),
            search_id: None,
            change: None,
        };
        let failed = acq.ledger.transition(&alice().id(), &cut_off, fail).await;
        assert!(matches!(failed, Ok(Transitioned::Applied { .. })));
        let answer = retry(&acq, &alice(), &cut_off, RetryMode::SameSource, None)
            .await
            .unwrap();
        assert_eq!(
            modes_allowed(answer),
            "one of new_search, the modes the job allows now"
        );
        // Its file is in the library: its search's other sources are not tried, though one is.
        let indexed = index_failed_track_job(&acq).await;
        let view = view_of(&acq, &indexed).await;
        let left = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        assert!(left.best().is_some());
        let answer = retry(&acq, &alice(), &indexed, RetryMode::NextSource, None)
            .await
            .unwrap();
        assert_eq!(
            modes_allowed(answer),
            "one of new_search, index_only, the modes the job allows now"
        );
    }

    #[tokio::test]
    async fn a_new_search_that_cannot_be_recorded_fails_the_job_and_searches_nothing() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = failed_track_job(&acq).await;
        // The database refuses the search's record (a full disk, a lock held too long).
        sqlx::query(
            "CREATE TRIGGER no_search BEFORE INSERT ON searches \
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&acq.db)
        .await
        .unwrap();
        let words = NewSearchArgs {
            query_text: Some("Marrow Choir O Holy Night live".into()),
            ..NewSearchArgs::default()
        };
        let RetryAnswer::Applied(view) =
            retry(&acq, &alice(), &job, RetryMode::NewSearch, Some(words))
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(view.job.state, JobState::Failed);
        let failure = job_failure(&view).unwrap();
        assert_eq!(
            (failure.code, failure.stage, failure.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::SourceSearch,
                "Soulful's database is unavailable"
            )
        );
        // No runner searches the job's own query in its place.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(effects.stub().specs().is_empty());
        assert!(effects.enqueued().is_empty());
    }

    #[tokio::test]
    async fn a_change_whose_job_cannot_be_read_after_it_is_still_answered_as_made() {
        let (acq, _) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        // Every read of the job fails once the change is written (an unreadable row).
        sqlx::query(&format!(
            "CREATE TRIGGER unreadable AFTER UPDATE OF state ON jobs WHEN NEW.id = '{job}' \
             BEGIN UPDATE jobs SET wanted = 'unreadable' WHERE id = NEW.id; END"
        ))
        .execute(&acq.db)
        .await
        .unwrap();
        let RetryAnswer::AppliedUnread { job: id, failure } =
            cancel(&acq, &alice(), &job).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (id.as_str(), failure.code),
            (job.as_str(), FailureCode::SettingsUnreadable)
        );
        assert_eq!(
            store::job_state(&acq.db, &job).await,
            Some(JobState::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_job_cancelled_while_its_runner_waits_for_a_place_lets_the_runner_go_at_once() {
        let (acq, effects) = Acquire::for_alice().await;
        let jobs = queued_track_jobs(&acq, 7).await;
        for job in &jobs[..6] {
            drive(&acq, &alice(), job).await;
        }
        let seventh = tokio::spawn({
            let (acq, job) = (acq.clone(), jobs[6].clone());
            async move { drive(&acq, &alice(), &job).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!seventh.is_finished(), "it waits for a place");
        let cancelled = cancel(&acq, &alice(), &jobs[6]).await.unwrap();
        assert!(matches!(cancelled, RetryAnswer::Applied(v) if v.job.state == JobState::Cancelled));
        // No place was given up.
        tokio::time::timeout(Duration::from_secs(2), seventh)
            .await
            .expect("the cancel wakes it")
            .unwrap();
        assert_eq!(effects.enqueued().len(), 6);
    }

    #[tokio::test]
    async fn download_source_refuses_a_job_whose_file_waits_for_navidrome() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = index_failed_track_job(&acq).await;
        let search = view_of(&acq, &job).await.search.unwrap().id;
        let answer = download_source(
            &acq,
            &alice(),
            SourceTarget::Job(job.clone()),
            &format!("{search}:1"),
        )
        .await
        .unwrap();
        assert!(
            matches!(&answer, RetryAnswer::InvalidInput { field: "job_id", expected }
                if expected == INDEXED_JOB),
            "{answer:?}"
        );
        assert_eq!(view_of(&acq, &job).await.job.state, JobState::Failed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(effects.enqueued().is_empty());
    }

    #[tokio::test]
    async fn cancel_ends_a_waiting_job_and_refuses_one_that_is_importing() {
        let (acq, _) = Acquire::for_alice().await;
        let waiting = queued_track_jobs(&acq, 1).await.remove(0);
        let cancelled = cancel(&acq, &alice(), &waiting).await.unwrap();
        assert!(matches!(cancelled, RetryAnswer::Applied(v) if v.job.state == JobState::Cancelled));
        assert!(
            !TransferRegistry::global().job_cancelled(&waiting),
            "a job with no transfer needs no mark"
        );
        let source = Path::new("/downloads/Come Over [1f2e3d4c]/03 - Coventry Carol.flac");
        let importing = importing_track_job(&acq, "Coventry Carol", source).await;
        let RetryAnswer::Refused(f, facts) = cancel(&acq, &alice(), &importing).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            f.message,
            "The job is importing; cancel_job needs a job that is searching, needs_source_choice, queued or downloading"
        );
        assert_eq!(
            (&facts["state"], f.code),
            (&json!("importing"), FailureCode::WrongJobState)
        );
    }

    #[tokio::test]
    async fn reject_result_reopens_a_done_job_without_that_source() {
        let (acq, _) = Acquire::for_alice().await;
        let job = done_track_job(&acq).await;
        let RetryAnswer::Applied(view) = retry(&acq, &alice(), &job, RetryMode::RejectResult, None)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (view.job.state, view.job.excluded_sources.clone()),
            (JobState::Failed, vec!["peer_a".to_string()])
        );
        let untried = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        assert_eq!(
            untried
                .candidates
                .iter()
                .map(|c| c.group.source.as_str())
                .collect::<Vec<_>>(),
            vec!["peer_b"]
        );
        assert_eq!(job_failure(&view), Some(result_rejected()));
    }

    #[tokio::test]
    async fn a_web_ui_job_cannot_be_retried_or_cancelled_here() {
        let (acq, _) = Acquire::for_alice().await;
        let job = failed_web_ui_job(&acq).await;
        let web_page = |answer: RetryAnswer| matches!(answer, RetryAnswer::InvalidInput { field: "job_id", expected } if expected == WEB_PAGE_JOB);
        assert!(web_page(
            retry(&acq, &alice(), &job, RetryMode::NewSearch, None)
                .await
                .unwrap()
        ));
        assert!(web_page(cancel(&acq, &alice(), &job).await.unwrap()));
        let dismissed = dismiss(&acq, &alice(), &job).await.answer.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
    }

    #[tokio::test]
    async fn a_needs_user_failure_can_be_retried_after_the_user_acted() {
        let (acq, effects) = Acquire::for_alice().await;
        // Soulful's text for a broken importer: a person fixes it.
        let broken = Failure::new(
            FailureCode::ImporterBroken,
            Stage::Import,
            RetryAdvice::NeedsUser,
            "No metadata source plugin loaded",
        );
        let job = failed_track_job_with(&acq, "Silent Night", broken).await;
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![],
        ))]);
        let words = NewSearchArgs {
            query_text: Some("Marrow Choir Silent Night choir".into()),
            ..NewSearchArgs::default()
        };
        let RetryAnswer::Applied(view) =
            retry(&acq, &alice(), &job, RetryMode::NewSearch, Some(words))
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(view.job.state, JobState::Searching);
        tokio::time::timeout(Duration::from_secs(10), async {
            while effects.stub().specs().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            effects.stub().specs()[0].text.as_deref(),
            Some("Marrow Choir Silent Night choir")
        );
    }

    #[tokio::test]
    async fn dismiss_can_be_repeated_after_a_partial_discard() {
        let (acq, _) = Acquire::for_alice().await;
        let dir = tempfile::tempdir().unwrap();
        let files = ["07.flac", "08.flac"].map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, b"flac").unwrap();
            file
        });
        // The second slot's kept import is not in the queue: another decision took it.
        let job = kept_album_job(&acq, [&files[0], &files[1]], [true, false]).await;
        let RetryAnswer::Unavailable(f, facts) =
            dismiss(&acq, &alice(), &job).await.answer.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (f.code, &facts["kept"]),
            (FailureCode::UnknownHandle, &json!("gone"))
        );
        acq.ledger.settled().await;
        let states: Vec<JobState> = view_of(&acq, &job)
            .await
            .slots
            .iter()
            .map(|s| s.state)
            .collect();
        assert_eq!(states, vec![JobState::Dismissed, JobState::NeedsDecision]);
        // That decision's run then ended: the kept files were gone (Soulful's own text).
        let gone = Failure::new(
            FailureCode::ImportFilesGone,
            Stage::Import,
            RetryAdvice::NotRetryable,
            "The kept files are gone",
        );
        let slot = JobSlot {
            job: job.clone(),
            slot: 1,
        };
        let row = transfer(
            "peer_a",
            "a/08.flac",
            "t-a/08.flac",
            DownloadState::ImportSkipped,
        )
        .with_job(slot)
        .failed_with(gone);
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![row],
        });
        acq.ledger.settled().await;
        let dismissed = dismiss(&acq, &alice(), &job).await.answer.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
    }

    #[tokio::test]
    async fn dismissing_a_job_waiting_for_a_decision_discards_its_kept_import() {
        let (acq, _) = Acquire::for_alice().await;
        let dir = tempfile::tempdir().unwrap();
        let files = ["07.flac", "08.flac"].map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, b"flac").unwrap();
            file
        });
        let job = kept_album_job(&acq, [&files[0], &files[1]], [true, true]).await;
        let dismissed = dismiss(&acq, &alice(), &job).await.answer.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
        for item in ["a/07.flac", "a/08.flac"] {
            let kept = acq.queue.import_choices(&alice().id(), item);
            assert!(kept.is_err(), "{item}'s kept import is discarded");
        }
    }
}
