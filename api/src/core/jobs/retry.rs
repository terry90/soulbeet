//! What retry_job, cancel_job, dismiss_job and download_source do to a job (design 11.5). Every
//! state change is a transition the ledger's writer checks against the job's state, so of two
//! calls racing on one job only one changes it.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use shared::download::{DownloadableItem, SourcePresence};
use shared::failure::{
    cut_chars, Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode, TEXT_MAX_CHARS,
};
use soulbeet::ImportMode;

use super::runner::{spawn_drive, Acquire};
use super::{
    allowed, done_source, folder_for, job_already_running, job_failure, job_not_found, job_view,
    last_tried, over_limit, untried, CallRefused, JobView, RetryMode, Untried, Wanted,
};
use crate::auth::UserCtx;
use crate::core::acquire::{numbered, AUTO_SELECT_SCORE_THRESHOLD};
use crate::core::database_failure;
use crate::core::sources::{
    candidate_not_found, owned_search, parse_candidate_id, search_not_found, start_stored,
    SearchRun,
};
use crate::ledger::store::StoredSearch;
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
    /// Refused with this failure and its facts.
    Refused(Failure, Map<String, Value>),
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

/// Design 11.5: how a person lets this app delete music, so a rejected file can be removed
/// (step 7 D7: the checkbox "Allow deleting music").
pub(crate) fn enable_deletion() -> UserAction {
    UserAction::new(
        UserActionCode::EnableDeletion,
        "Allow deleting music for this app",
        "Settings > Users > Connected apps",
    )
}

/// `job_view` of a job the caller named: job_not_found when it is no job of `ctx`'s.
async fn named_job(acquire: &Acquire, ctx: &UserCtx, job: &str) -> Result<JobView, Failure> {
    job_view(acquire, ctx, job).await?.ok_or_else(job_not_found)
}

/// The refusal of a job started in the web page.
fn web_page_job(view: &JobView) -> Option<RetryAnswer> {
    (view.job.origin == BatchOrigin::Ui).then(|| RetryAnswer::InvalidInput {
        field: "job_id",
        expected: WEB_PAGE_JOB.to_string(),
    })
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
            Ok(RetryAnswer::Applied(Box::new(
                named_job(acquire, ctx, id).await?,
            )))
        }
        Transitioned::Refused { state } => Ok(refused(wrong_job_state(state, action, states))),
        Transitioned::Unknown => Err(job_not_found()),
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
            let Some((slot, item)) = last_tried(&view) else {
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
/// failure when that failure is why (its recording already in the library, a failure that does
/// not say to try that source later, one that was not a wait for Navidrome); sources_exhausted
/// for next_source with nothing untried; else the modes the job allows now.
fn refusal(view: &JobView, untried: &Untried, mode: RetryMode) -> RetryAnswer {
    let job = &view.job;
    if !mode.states().contains(&job.state) {
        let action = format!("retry_job {}", mode.as_str());
        return refused(wrong_job_state(job.state, &action, mode.states()));
    }
    let failure = job_failure(view);
    if let Some(f) = failure.as_ref() {
        if f.code == FailureCode::ImportAlreadyInLibrary {
            return RetryAnswer::Refused(f.clone(), Map::new());
        }
    }
    let track_only = matches!(mode, RetryMode::SameSource | RetryMode::NextSource);
    if track_only && job.kind == JobKind::Album {
        return RetryAnswer::InvalidInput {
            field: "mode",
            expected: "new_search or index_only: an album job is retried whole".to_string(),
        };
    }
    if mode == RetryMode::NextSource {
        return RetryAnswer::Refused(sources_exhausted(), Map::new());
    }
    if let Some(f) = failure {
        return RetryAnswer::Refused(f, Map::new());
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
/// say.
async fn source_ready(acquire: &Acquire, source: &str) -> Result<(), RetryAnswer> {
    let backend = acquire.effects.backend().await.map_err(|f| {
        let f = Failure {
            stage: Stage::Enqueue,
            ..f
        };
        RetryAnswer::Refused(f, Map::new())
    })?;
    let refused = |failure| Err(RetryAnswer::Refused(failure, source_facts(source)));
    match backend.source_status(source).await {
        Ok(SourcePresence::Offline) => refused(source_offline()),
        Ok(SourcePresence::Blocked) => refused(source_blocked()),
        Ok(SourcePresence::Online | SourcePresence::Away) => Ok(()),
        Err(e) => refused(e.failure(Stage::Enqueue, e.to_string())),
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
                Err(f) => return Ok(RetryAnswer::Refused(f, facts)),
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
                if let Err(f) = acquire.effects.hydrate(ctx, &mut query).await {
                    return Ok(RetryAnswer::Refused(f, Map::new()));
                }
            }
            let backend = match acquire.effects.backend().await {
                Ok(backend) => backend,
                Err(f) => return Ok(RetryAnswer::Refused(f, Map::new())),
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
    }
    // Started before the runner, which then waits for it: a cancel between the two leaves a
    // stop mark that start_stored takes (D9).
    if let Some((backend, run)) = start {
        let started = start_stored(&acquire.searches, &acquire.ledger, ctx, backend, run).await;
        if let Err(f) = started {
            tracing::warn!(
                "Could not record search {search_id} of job {}: {}; its runner searches for the job's own query",
                job.id,
                f.message
            );
        }
    }
    spawn_drive(acquire.clone(), ctx.clone(), job.id.clone());
    Ok(RetryAnswer::Applied(Box::new(
        named_job(acquire, ctx, &job.id).await?,
    )))
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
        Transitioned::Applied { from, to, .. } => {
            let view = named_job(acquire, ctx, job).await?;
            // Read after the cancel, which no later search change can follow.
            if (from, to) == (JobState::Searching, JobState::Cancelled) {
                if let Some(id) = &view.job.search_id {
                    acquire.searches.stop(id).await;
                }
            }
            Ok(RetryAnswer::Applied(Box::new(view)))
        }
        Transitioned::Refused { state } => {
            Ok(refused(wrong_job_state(state, "cancel_job", states)))
        }
        Transitioned::Unknown => Err(job_not_found()),
    }
}

/// dismiss_job: `ctx`'s job `job` will not be pursued. A job waiting for an import decision first
/// has the import each of its slots still waiting keeps discarded (each import once), which
/// dismisses those slots; a kept import gone while its slot still waits (another decision took it
/// and has not recorded it yet) refuses with the queue's failure, and the call can be made again.
/// Then the job ends dismissed; one whose slots the discards all dismissed already is answered as
/// it is.
pub(crate) async fn dismiss(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
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
                Ok(_) => discarded = true,
                Err(refusal) => {
                    let now = named_job(acquire, ctx, job).await?;
                    let still_waiting = now.slots.iter().any(|slot| {
                        slot.state == JobState::NeedsDecision
                            && slot.row.as_ref().is_some_and(|row| row.item == item)
                    });
                    if still_waiting {
                        let facts = Map::from_iter([("kept".to_string(), json!("gone"))]);
                        return Ok(RetryAnswer::Refused(refusal.failure, facts));
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
    }
    Ok(RetryAnswer::Applied(Box::new(
        named_job(acquire, ctx, job).await?,
    )))
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
        return Ok(RetryAnswer::Refused(f, Map::new()));
    }
    if let Err(f) = acquire.effects.backend().await {
        return Ok(RetryAnswer::Refused(f, Map::new()));
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
            return Ok(RetryAnswer::Refused(failure, facts));
        }
    };
    let mut query = wanted.query();
    if let Wanted::Album(album) = &wanted {
        if let Err(f) = acquire.effects.hydrate(ctx, &mut query).await {
            let facts = Map::from_iter([("release_mbid".to_string(), json!(album.release_mbid))]);
            return Ok(RetryAnswer::Refused(f, facts));
        }
    }
    let batch = NewJobBatch {
        id: uuid::Uuid::new_v4().to_string(),
        user: ctx.id(),
        label: Some(wanted.label()),
        origin: BatchOrigin::Mcp,
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
            Ok(RetryAnswer::Applied(Box::new(
                named_job(acquire, ctx, &id).await?,
            )))
        }
        Admitted::AlreadyRunning { job, state } => Ok(refused(job_already_running(&job, state))),
        Admitted::OverLimit { unfinished } => Ok(refused(over_limit(unfinished))),
    }
}

/// The search and files of `ctx`'s candidate `candidate_id` (D3): one of a live search a tool
/// started that ended with candidates. Any other id, another user's included, reads like an
/// unknown one; an expired search is expired_handle.
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
            _ => RetryAnswer::Refused(f, facts.clone()),
        })?;
    if stored.spec.is_none() {
        return Err(unknown());
    }
    let items = (stored.candidates.as_ref())
        .and_then(|groups| groups.get(index))
        .map(|group| group.items.clone())
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
        auto_options, done_track_job, downloading_track_job, failed_track_job,
        failed_track_job_with, failed_web_ui_job, importing_track_job, kept_album_job,
        mcp_track_job, queued_track_jobs, stop_slots, test_db_with_alice, view_of,
    };
    use crate::core::sources::{search_stopped, SearchProgress, Searches};
    use crate::core::test_ctx;
    use crate::ledger::JobEvent;
    use crate::server_fns::download::test_support::{search_result, track, transfer};
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        assert!(searches.stop("s9").await);
        assert_eq!(
            waiting.await.unwrap(),
            SearchProgress::Failed(search_stopped(), None)
        );
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
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
        let job = failed_track_job_with(&acq, "Silent Night", peer_busy()).await;
        effects.stub().set_presence(SourcePresence::Offline);
        let RetryAnswer::Refused(f, facts) =
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
        let RetryAnswer::Refused(f, _) = retry(&acq, &alice(), &job, RetryMode::SameSource, None)
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
    async fn cancel_ends_a_waiting_job_and_refuses_one_that_is_importing() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let job = failed_web_ui_job(&acq).await;
        let web_page = |answer: RetryAnswer| matches!(answer, RetryAnswer::InvalidInput { field: "job_id", expected } if expected == WEB_PAGE_JOB);
        assert!(web_page(
            retry(&acq, &alice(), &job, RetryMode::NewSearch, None)
                .await
                .unwrap()
        ));
        assert!(web_page(cancel(&acq, &alice(), &job).await.unwrap()));
        let dismissed = dismiss(&acq, &alice(), &job).await.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
    }

    #[tokio::test]
    async fn a_needs_user_failure_can_be_retried_after_the_user_acted() {
        let (acq, effects) = Acquire::for_test(test_db_with_alice().await);
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
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let dir = tempfile::tempdir().unwrap();
        let files = ["07.flac", "08.flac"].map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, b"flac").unwrap();
            file
        });
        // The second slot's kept import is not in the queue: another decision took it.
        let job = kept_album_job(&acq, [&files[0], &files[1]], [true, false]).await;
        let RetryAnswer::Refused(f, facts) = dismiss(&acq, &alice(), &job).await.unwrap() else {
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
        let dismissed = dismiss(&acq, &alice(), &job).await.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
    }

    #[tokio::test]
    async fn dismissing_a_job_waiting_for_a_decision_discards_its_kept_import() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let dir = tempfile::tempdir().unwrap();
        let files = ["07.flac", "08.flac"].map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, b"flac").unwrap();
            file
        });
        let job = kept_album_job(&acq, [&files[0], &files[1]], [true, true]).await;
        let dismissed = dismiss(&acq, &alice(), &job).await.unwrap();
        assert!(matches!(dismissed, RetryAnswer::Applied(v) if v.job.state == JobState::Dismissed));
        for item in ["a/07.flac", "a/08.flac"] {
            let kept = acq.queue.import_choices(&alice().id(), item);
            assert!(kept.is_err(), "{item}'s kept import is discarded");
        }
    }
}
