//! The job tools: `get_jobs` shows the caller's jobs, by id or by batch and state, waiting for
//! them to change when asked; `retry_job`, `cancel_job` and `dismiss_job` act on one.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::download::DownloadProgress;
use shared::failure::{Failure, RetryAdvice, Stage};
use soulbeet::navidrome::NavidromeClient;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use tracing::warn;

use super::args::{repeatable, Args};
use super::contract::{
    answer, failed, invalid_value, success, too_large_to_repeat, upstream_error, NextCall,
    ToolError,
};
use super::facts::{
    asks_backend, job_document, live_facts, results_of, unanswered, Detail, LiveFacts, ReadNow,
};
use super::keys::{keyed, request_key, Keyed};
use super::next::next_calls;
use super::paging::{fit, Page};
use super::refs::{filters, filters_schema, page_schema, request_key_schema, wait_schema};
use super::server::{common_codes, ToolSpec};
use super::{wait, McpCaller};
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::index::library_songs;
use crate::core::index::NavidromeOf;
use crate::core::jobs::retry::{cancel, dismiss, retry, NewSearchArgs, RetryAnswer};
use crate::core::jobs::runner::Acquire;
use crate::core::jobs::{
    job_not_found, job_records, job_view, recent_failures, untried, untried_given, views_of,
    JobSelect, JobView, RetryMode, Untried,
};
use crate::core::library::{navidrome_failure, navidrome_for, SongFacts};
use crate::ledger::store::JobRecord;
use crate::ledger::{JobState, SlotResult};

const GET_JOBS_ARGS: &[&str] = &[
    "job_ids",
    "batch_id",
    "states",
    "detail",
    "since",
    "wait_seconds",
    "wait_for",
    "limit",
    "cursor",
];

/// Every job state by name, in the order `states` lists them.
const STATES: [JobState; 11] = [
    JobState::Searching,
    JobState::Queued,
    JobState::Downloading,
    JobState::Importing,
    JobState::Indexing,
    JobState::Done,
    JobState::NeedsSourceChoice,
    JobState::NeedsDecision,
    JobState::Failed,
    JobState::Cancelled,
    JobState::Dismissed,
];

/// The most job ids a get_jobs call takes.
pub(crate) const MAX_JOB_IDS: usize = 100;

pub(crate) const GET_JOBS: ToolSpec = ToolSpec {
    name: "get_jobs",
    title: "Show jobs",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows jobs by job_ids, or by batch_id and states. Summary detail gives each job's state and failure code; full detail gives the current download with live bytes, speed, place in the source's queue and whether the source is online, earlier attempts with their failures (tried), the timeline, what beets applied and Navidrome lists (or, while Navidrome has not listed it yet, when the import ended and whether a scan was asked for; get_status says whether Navidrome is scanning), and next: the calls that can be made for the job now. wait_seconds (0 to 180) waits on the server until wait_for holds: any_change (a job changed after since, or without since after the call came), needs_action (a job needs action: needs_source_choice, needs_decision or failed; with since, one that changed after it) or all_terminal (every job is done, failed, cancelled or dismissed); needs_action also ends when every job is. Each answer has change_cursor: pass it as since, so a wait does not end again for what was already seen; while a job is unfinished, next offers this call again with since set to it. Job ids stay valid 30 days after the job ends. Failure codes: {common}, unknown_handle (a job id this user has no job for; facts.unknown_job_ids names them).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"job_ids": {"type": "array", "minItems": 1, "maxItems": 100, "items": {"type": "string", "minLength": 1}}, "batch_id": {"type": "string", "minLength": 1}, "states": {"type": "array", "items": {"enum": ["searching", "queued", "downloading", "importing", "indexing", "done", "needs_source_choice", "needs_decision", "failed", "cancelled", "dismissed"]}}, "detail": {"enum": ["summary", "full"]}, "since": {"type": "integer", "minimum": 0}, "#,
        wait_schema!(),
        ", ",
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_jobs(state, caller, args)),
};

/// The conditions get_jobs waits for, through `wait::until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitFor {
    AnyChange,
    NeedsAction,
    AllTerminal,
}

impl WaitFor {
    /// `wait_seconds` (0 to 180, default 0) and `wait_for` (default any_change).
    pub(crate) fn read(args: &Args) -> Result<(Duration, WaitFor), ToolError> {
        let wait = args.opt_u32("wait_seconds", 0, 180)?.unwrap_or(0);
        let wait_for =
            match args.opt_choice("wait_for", &["any_change", "needs_action", "all_terminal"])? {
                Some("needs_action") => WaitFor::NeedsAction,
                Some("all_terminal") => WaitFor::AllTerminal,
                _ => WaitFor::AnyChange,
            };
        Ok((Duration::from_secs(wait.into()), wait_for))
    }

    /// Over the selected jobs: AnyChange when one's changed_seq is above `since`; NeedsAction
    /// when one needs action, past a given `since` only one whose changed_seq is above it, or
    /// when all are terminal; AllTerminal when all are.
    pub(crate) fn holds(self, jobs: &[JobRecord], since: Since) -> bool {
        let all_terminal = || jobs.iter().all(|job| job.state.is_terminal());
        match self {
            WaitFor::AnyChange => jobs.iter().any(|job| job.changed_seq > since.seq()),
            WaitFor::NeedsAction => {
                let unseen = |job: &JobRecord| match since {
                    Since::Given(seq) => job.changed_seq > seq,
                    Since::Now(_) => true,
                };
                jobs.iter()
                    .any(|job| needs_action(job.state) && unseen(job))
                    || all_terminal()
            }
            WaitFor::AllTerminal => all_terminal(),
        }
    }
}

/// The change number a wait compares the jobs' changed_seq with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Since {
    /// The caller's: the jobs as they were then are seen.
    Given(i64),
    /// The ledger's when the call came, the caller naming none: no job is seen.
    Now(i64),
}

impl Since {
    pub(crate) fn seq(self) -> i64 {
        match self {
            Since::Given(seq) | Since::Now(seq) => seq,
        }
    }
}

/// Whether a job in `state` waits for the user: needs_source_choice, needs_decision or failed.
pub(crate) fn needs_action(state: JobState) -> bool {
    matches!(
        state,
        JobState::NeedsSourceChoice | JobState::NeedsDecision | JobState::Failed
    )
}

/// A get_jobs call as read.
struct Call {
    select: JobSelect,
    detail: Detail,
    /// A change_cursor of an earlier answer.
    since: Option<i64>,
    wait: Duration,
    wait_for: WaitFor,
    page: Page,
}

fn read_call(args: &Args) -> Result<Call, ToolError> {
    let job_ids = args.opt_strings("job_ids", MAX_JOB_IDS)?;
    if job_ids.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid_value(
            "get_jobs",
            "job_ids",
            "a list of 1 to 100 job ids",
        ));
    }
    let batch_id = args.opt_string("batch_id")?;
    let states = read_states(args)?;
    let detail_name = args
        .opt_choice("detail", &["summary", "full"])?
        .unwrap_or("summary");
    let detail = match detail_name {
        "full" => Detail::Full,
        _ => Detail::Summary,
    };
    // A change number, compared with the jobs' changed_seq.
    let since = args.opt_u32("since", 0, u32::MAX)?.map(i64::from);
    let (wait, wait_for) = WaitFor::read(args)?;
    let names: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
    let listing = json!({"job_ids": job_ids, "batch_id": batch_id, "states": names,
        "detail": detail_name});
    let page = Page::read(args, listing)?;
    let select = match job_ids {
        Some(ids) => {
            let absent = "absent when job_ids is given";
            args.absent("batch_id", absent)?;
            args.absent("states", absent)?;
            JobSelect::Ids(ids)
        }
        None => JobSelect::Filter { batch_id, states },
    };
    args.finish()?;
    Ok(Call {
        select,
        detail,
        since,
        wait,
        wait_for,
        page,
    })
}

/// `states`, each a JobState name.
fn read_states(args: &Args) -> Result<Vec<JobState>, ToolError> {
    let refused = || {
        let names: Vec<&str> = STATES.iter().map(|s| s.as_str()).collect();
        invalid_value(
            "get_jobs",
            "states",
            &format!("a list of job states, each one of {}", names.join(", ")),
        )
    };
    let names = args
        .opt_strings("states", STATES.len())
        .map_err(|_| refused())?
        .unwrap_or_default();
    names
        .iter()
        .map(|name| JobState::parse(name).ok_or_else(refused))
        .collect()
}

/// `get_jobs`: the caller's jobs as asked, once `wait_for` holds for them or `wait_seconds`
/// passed, a page of job documents at a time.
pub(crate) async fn get_jobs(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_jobs", args, GET_JOBS_ARGS);
    answer(
        async {
            let call = read_call(&args)?;
            let acquire = &state.acquire;
            let since = match call.since {
                Some(seq) => Since::Given(seq),
                None => Since::Now(*acquire.ledger.changes().borrow()),
            };
            let failed = |f| upstream_error(f, Map::new(), args.same_call());
            // The wait and the page read records; only the page's jobs are read in full.
            let records = wait::until(
                &acquire.ledger,
                call.wait,
                || job_records(&acquire.db, &caller.ctx, &call.select),
                // An id that is no job of the caller's fails the call at once.
                |records| {
                    !unknown_ids(&call.select, records).is_empty()
                        || call.wait_for.holds(records, since)
                },
            )
            .await
            .map_err(failed)?;
            let unknown = unknown_ids(&call.select, &records);
            if !unknown.is_empty() {
                return Err(unknown_jobs(&args, &call.select, unknown));
            }
            let more = records.len() > call.page.offset + call.page.limit;
            let cursor = records.iter().map(|job| job.changed_seq).max().unwrap_or(0);
            let unfinished = records.iter().any(|job| !job.state.is_terminal());
            let mut fields = Map::new();
            fields.insert("change_cursor".into(), json!(cursor));
            let mut again = args.as_value();
            if let Value::Object(given) = &mut again {
                // A page cursor names a place in the listing as this answer read it.
                given.remove("cursor");
            }
            let follow = unfinished.then(|| since_call("get_jobs", again, cursor));
            fields.insert("next".into(), json!(follow.iter().collect::<Vec<_>>()));
            let page = (records.into_iter())
                .skip(call.page.offset)
                .take(call.page.limit)
                .collect();
            let shown = views_of(&acquire.db, &acquire.ledger, &caller.ctx, page)
                .await
                .map_err(failed)?;
            let untried = untried_of(&acquire.db, &shown).await.map_err(failed)?;
            let reads = match call.detail {
                Detail::Summary => shown.iter().map(|_| ReadNow::default()).collect(),
                Detail::Full => read_now(acquire, &*state.navidrome, &caller.ctx, &shown).await,
            };
            let documents = (shown.iter().zip(&untried).zip(&reads))
                .map(|((view, untried), now)| {
                    job_document(view, untried, call.detail, now, caller.allow_delete)
                })
                .collect();
            Ok(fit(fields, "jobs", documents, &call.page, more))
        }
        .await,
    )
}

/// `tool` called again with `args` (the arguments given), `since` set to `cursor`: the call that
/// follows unfinished jobs past what an answer showed.
pub(crate) fn since_call(tool: &'static str, mut args: Value, cursor: i64) -> NextCall {
    if let Value::Object(given) = &mut args {
        given.insert("since".into(), json!(cursor));
    }
    NextCall::new(tool, args)
}

/// Each of `views`' untried candidates, the sources' recent failures read once.
pub(crate) async fn untried_of(
    db: &SqlitePool,
    views: &[JobView],
) -> Result<Vec<Untried>, Failure> {
    let now = chrono::Utc::now().timestamp();
    let failures = recent_failures(db, now).await?;
    Ok((views.iter())
        .map(|view| untried_given(view, &failures, now))
        .collect())
}

/// The ids of `select` that are none of `jobs`, each once, in the order given.
fn unknown_ids(select: &JobSelect, jobs: &[JobRecord]) -> Vec<String> {
    let JobSelect::Ids(ids) = select else {
        return vec![];
    };
    let mut unknown: Vec<String> = Vec::new();
    for id in ids {
        if !jobs.iter().any(|job| &job.id == id) && !unknown.contains(id) {
            unknown.push(id.clone());
        }
    }
    unknown
}

/// Ids that are no job of the caller's (unknown, or another user's, which reads the same): the
/// call fails naming them, offering the same call with the other ids when there are any.
fn unknown_jobs(args: &Args, select: &JobSelect, unknown: Vec<String>) -> ToolError {
    let mut known: Vec<&String> = Vec::new();
    if let JobSelect::Ids(ids) = select {
        for id in ids {
            if !unknown.contains(id) && !known.contains(&id) {
                known.push(id);
            }
        }
    }
    let mut failure = job_not_found();
    let mut next = vec![];
    if !known.is_empty() {
        let mut call = args.as_value();
        if let Value::Object(fields) = &mut call {
            // A cursor names the listing of the ids given.
            fields.remove("cursor");
            fields.insert("job_ids".into(), json!(known));
        }
        match repeatable("get_jobs", call) {
            Some(call) => next.push(call),
            None => failure.message = too_large_to_repeat(&failure.message),
        }
    }
    ToolError {
        failure,
        facts: Map::from_iter([("unknown_job_ids".to_string(), json!(unknown))]),
        tried: vec![],
        next,
    }
}

/// What the full documents of `views` read now: the backend's answers about each running
/// transfer, all asked at once, and the songs `ctx`'s Navidrome lists for each import beets found
/// in the library.
pub(crate) async fn read_now(
    acquire: &Acquire,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    views: &[JobView],
) -> Vec<ReadNow> {
    let mut reads: Vec<ReadNow> = views.iter().map(|_| ReadNow::default()).collect();
    let running: Vec<(usize, u32, &DownloadProgress)> = (views.iter().enumerate())
        .flat_map(|(i, view)| {
            (view.slots.iter())
                .filter(|slot| !slot.state.is_terminal())
                .filter_map(move |slot| {
                    let row = slot.live.as_ref().or(slot.row.as_ref())?;
                    asks_backend(row).map(|_| (i, slot.slot, row))
                })
        })
        .collect();
    if !running.is_empty() {
        let answers: Vec<Option<LiveFacts>> = match acquire.effects.backend().await {
            Ok(backend) => {
                join_all(running.iter().map(|(_, _, row)| live_facts(&backend, row))).await
            }
            Err(f) => {
                let f = Failure {
                    stage: Stage::Transfer,
                    ..f
                };
                running
                    .iter()
                    .map(|(_, _, row)| unanswered(row, &f))
                    .collect()
            }
        };
        for ((i, slot, _), facts) in running.iter().zip(answers) {
            if let Some(facts) = facts {
                reads[*i].live.insert(*slot, facts);
            }
        }
    }
    let found: Vec<(usize, u32, &[ImportedItem])> = (views.iter().enumerate())
        .flat_map(|(i, view)| {
            results_of(view)
                .into_iter()
                .filter_map(move |(result, slots)| match result {
                    SlotResult::AlreadyInLibrary { existing } => {
                        Some((i, slots[0], existing.as_slice()))
                    }
                    SlotResult::Imported { .. } => None,
                })
        })
        .collect();
    if found.is_empty() {
        return reads;
    }
    let songs: Vec<Result<Vec<SongFacts>, Failure>> =
        match navidrome_for(navidrome, ctx, Stage::LibraryCheck).await {
            Ok(navi) => {
                join_all(found.iter().map(|(i, _, existing)| {
                    listed_songs(&navi, Path::new(&views[*i].job.folder.path), existing)
                }))
                .await
            }
            Err(f) => found.iter().map(|_| Err(f.clone())).collect(),
        };
    for ((i, slot, _), songs) in found.iter().zip(songs) {
        reads[*i].library.insert(*slot, songs);
    }
    reads
}

/// The songs Navidrome lists for each of `existing`, items in the library folder `library`.
async fn listed_songs(
    navi: &NavidromeClient,
    library: &Path,
    existing: &[ImportedItem],
) -> Result<Vec<SongFacts>, Failure> {
    let mut songs = Vec::new();
    for item in existing {
        let found = library_songs(navi, library, item)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;
        songs.extend(found.iter().map(SongFacts::from));
    }
    Ok(songs)
}

const RETRY_JOB_ARGS: &[&str] = &[
    "job_id",
    "mode",
    "query_text",
    "formats",
    "min_bitrate_kbps",
    "lossless_only",
    "max_queue_length",
    "search_id",
    "request_key",
];

const JOB_ARGS: &[&str] = &["job_id", "request_key"];

/// retry_job's modes, as the schema lists them.
const MODES: [&str; 5] = [
    "same_source",
    "next_source",
    "new_search",
    "index_only",
    "reject_result",
];

/// The filters of a new_search.
const FILTERS: [&str; 4] = [
    "formats",
    "min_bitrate_kbps",
    "lossless_only",
    "max_queue_length",
];

pub(crate) const RETRY_JOB: ToolSpec = ToolSpec {
    name: "retry_job",
    title: "Retry a job",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Tries a job again in one of five modes. same_source: the source that failed, for a failure that says to retry it later, or a cancelled job (track jobs). next_source: the best source of the job's search not tried yet (track jobs). new_search: a new search, with optional query_text and filters, or the search_id of a find_sources search. index_only: wait for Navidrome again, for a job that failed at indexing. reject_result: a done job's file is not the recording wanted; the job reopens without that source (the file stays in the library until deleted). A mode the job's state does not allow is refused with wrong_job_state; next in every job document lists only the modes allowed now. A failure that needs a person lists none: after the person did what its user_action says, retry_job runs again. A mode refused because of the job's own failure answers that failure. Album jobs retry by new_search or index_only. Jobs started in Soulful's web page are retried there. Answers the job's full document, as get_jobs gives it. Failure codes: {common}, unknown_handle, expired_handle (search_id), wrong_job_state, sources_exhausted, peer_offline (same_source while the source is offline), enqueue_failed (same_source of a source slskd's blacklist holds), backend_not_configured, backend_unreachable, metadata_not_found, metadata_unavailable (album tracklist), already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["job_id", "mode"], "properties": {"job_id": {"type": "string", "minLength": 1}, "mode": {"enum": ["same_source", "next_source", "new_search", "index_only", "reject_result"]}, "query_text": {"type": "string", "minLength": 1}, "#,
        filters_schema!(),
        r#", "search_id": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(retry_job(state, caller, args)),
};

pub(crate) const CANCEL_JOB: ToolSpec = ToolSpec {
    name: "cancel_job",
    title: "Cancel a job",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Cancels a job that is searching, waiting for a source choice, queued or downloading: its search or transfers stop. A transfer that already finished still imports. Jobs started in Soulful's web page are cancelled there. Answers the job's full document. Failure codes: {common}, unknown_handle, wrong_job_state, already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["job_id"], "properties": {"job_id": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(cancel_job(state, caller, args)),
};

pub(crate) const DISMISS_JOB: ToolSpec = ToolSpec {
    name: "dismiss_job",
    title: "Dismiss a job",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Closes a job that will not be pursued (failed, cancelled, waiting for a source choice or an import decision). For one waiting for an import decision, deletes the downloaded copy Soulful kept for it (never a library file). Answers the job's full document. Failure codes: {common}, unknown_handle (also: another decision already took the kept import; dismiss_job can be called again once it ended), wrong_job_state, already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["job_id"], "properties": {"job_id": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(dismiss_job(state, caller, args)),
};

/// What a job tool's work acts and reads through, owned, so the work runs in a task of its own.
#[derive(Clone)]
pub(crate) struct JobTool {
    tool: &'static str,
    pub acquire: Arc<Acquire>,
    navidrome: Arc<dyn NavidromeOf>,
    pub caller: McpCaller,
}

impl JobTool {
    pub(crate) fn of(tool: &'static str, state: &ConnectionState, caller: &McpCaller) -> JobTool {
        JobTool {
            tool,
            acquire: state.acquire.clone(),
            navidrome: state.navidrome.clone(),
            caller: caller.clone(),
        }
    }

    /// `view`'s full document, as get_jobs gives it.
    async fn document(&self, view: &JobView) -> Result<Map<String, Value>, Failure> {
        let now = chrono::Utc::now().timestamp();
        let untried = untried(&self.acquire.db, view, now).await?;
        let ctx = &self.caller.ctx;
        let reads = read_now(
            &self.acquire,
            &*self.navidrome,
            ctx,
            std::slice::from_ref(view),
        );
        let read = reads.await.pop().unwrap_or_default();
        let deletion_enabled = self.caller.allow_delete;
        let Value::Object(fields) =
            job_document(view, &untried, Detail::Full, &read, deletion_enabled)
        else {
            unreachable!("a job document is an object");
        };
        Ok(fields)
    }

    /// The answer to `answered`, an action on job `job` (None for a job download_source was to
    /// make). A change made answers the job's full document, kept for the request key. A refusal
    /// carries the job's `state` and `next` as read again after it, after this call again when
    /// the failure says the same source can answer later (`again`); an argument the job cannot
    /// take is invalid_input; a failure that kept the action from running offers `again` as
    /// upstream errors do. None of these changed anything, so the key is let go.
    pub(crate) async fn answer(
        &self,
        job: Option<&str>,
        answered: Result<RetryAnswer, Failure>,
        again: Option<NextCall>,
    ) -> Keyed {
        match answered {
            Ok(RetryAnswer::Applied(view)) => Keyed::Kept(match self.document(&view).await {
                Ok(fields) => success(fields),
                Err(f) => failed(upstream_error(f, Map::new(), None)),
            }),
            Ok(RetryAnswer::Refused(failure, facts)) => {
                Keyed::Released(failed(self.refusal(job, failure, facts, again).await))
            }
            Ok(RetryAnswer::InvalidInput { field, expected }) => {
                Keyed::Released(failed(invalid_value(self.tool, field, &expected)))
            }
            Err(f) => Keyed::Released(failed(upstream_error(f, Map::new(), again))),
        }
    }

    /// `failure` with `facts`, the state of job `job` and its next as read now.
    async fn refusal(
        &self,
        job: Option<&str>,
        failure: Failure,
        mut facts: Map<String, Value>,
        again: Option<NextCall>,
    ) -> ToolError {
        let mut next = Vec::new();
        if failure.retry == RetryAdvice::SameSourceLater {
            next.extend(again);
        }
        if let Some(job) = job {
            match self.read(job).await {
                Ok(Some((view, untried))) => {
                    facts.insert("state".into(), json!(view.job.state.as_str()));
                    next.extend(next_calls(&view, &untried));
                }
                Ok(None) => {}
                Err(f) => warn!(
                    "Could not read job {job} again after a refusal: {}",
                    f.message
                ),
            }
        }
        ToolError {
            failure,
            facts,
            tried: vec![],
            next,
        }
    }

    /// The caller's job `job` and its untried candidates.
    async fn read(&self, job: &str) -> Result<Option<(JobView, Untried)>, Failure> {
        let Some(view) = job_view(&self.acquire, &self.caller.ctx, job).await? else {
            return Ok(None);
        };
        let now = chrono::Utc::now().timestamp();
        let untried = untried(&self.acquire.db, &view, now).await?;
        Ok(Some((view, untried)))
    }
}

/// `mode`, required, one of MODES.
fn read_mode(args: &Args) -> Result<RetryMode, ToolError> {
    let refused = || invalid_value("retry_job", "mode", &format!("one of {}", MODES.join(", ")));
    let name = args.opt_choice("mode", &MODES)?.ok_or_else(refused)?;
    (RetryMode::ALL.into_iter())
        .find(|mode| mode.as_str() == name)
        .ok_or_else(refused)
}

/// A retry_job call as read: the job, the mode, what a new_search searches with, the key.
fn read_retry(
    args: &Args,
) -> Result<(String, RetryMode, NewSearchArgs, Option<String>), ToolError> {
    let job_id = args.string("job_id")?;
    let mode = read_mode(args)?;
    let mut search = NewSearchArgs::default();
    if mode == RetryMode::NewSearch {
        search.query_text = args.opt_string("query_text")?;
        search.search_id = args.opt_string("search_id")?;
        if search.search_id.is_some() {
            args.absent("query_text", "absent when search_id is given")?;
        }
        let read = filters(args)?;
        if FILTERS.iter().any(|name| args.has(name)) {
            search.filters = Some(read);
        }
    } else {
        let names = ["query_text"]
            .into_iter()
            .chain(FILTERS)
            .chain(["search_id"]);
        for name in names {
            args.absent(name, "absent unless mode is new_search")?;
        }
    }
    let key = request_key(args)?;
    args.finish()?;
    Ok((job_id, mode, search, key))
}

/// `retry_job`: one mode of retry on the caller's job, once per request key.
pub(crate) async fn retry_job(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("retry_job", args, RETRY_JOB_ARGS);
    let (job_id, mode, search, key) = match read_retry(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = json!({"job_id": job_id, "mode": mode.as_str(), "query_text": search.query_text,
        "filters": search.filters, "search_id": search.search_id});
    let on = JobTool::of("retry_job", state, caller);
    let again = args.same_call();
    let work = async move {
        let ctx = &on.caller.ctx;
        let answered = retry(&on.acquire, ctx, &job_id, mode, Some(search)).await;
        on.answer(Some(&job_id), answered, again).await
    };
    keyed(&state.acquire, caller, "retry_job", key, &call, work).await
}

/// The job id and key of a cancel_job or dismiss_job call.
fn read_job(args: &Args) -> Result<(String, Option<String>), ToolError> {
    let job_id = args.string("job_id")?;
    let key = request_key(args)?;
    args.finish()?;
    Ok((job_id, key))
}

/// `cancel_job`: the caller's job stops, once per request key.
pub(crate) async fn cancel_job(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("cancel_job", args, JOB_ARGS);
    let (job_id, key) = match read_job(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = json!({"job_id": job_id});
    let on = JobTool::of("cancel_job", state, caller);
    let again = args.same_call();
    let work = async move {
        let answered = cancel(&on.acquire, &on.caller.ctx, &job_id).await;
        on.answer(Some(&job_id), answered, again).await
    };
    keyed(&state.acquire, caller, "cancel_job", key, &call, work).await
}

/// `dismiss_job`: the caller's job is closed, once per request key.
pub(crate) async fn dismiss_job(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("dismiss_job", args, JOB_ARGS);
    let (job_id, key) = match read_job(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = json!({"job_id": job_id});
    let on = JobTool::of("dismiss_job", state, caller);
    let again = args.same_call();
    let work = async move {
        let answered = dismiss(&on.acquire, &on.caller.ctx, &job_id).await;
        on.answer(Some(&job_id), answered, again).await
    };
    keyed(&state.acquire, caller, "dismiss_job", key, &call, work).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};
    use shared::download::{
        DownloadProgress, DownloadState, JobSlot, SearchResult, SearchState, SearchStats,
    };
    use soulbeet::ImportedItem;

    use crate::connection::test_util::{app_of, call_tool, document, jobs_app};
    use crate::core::folder_of;
    use crate::core::jobs::runner::drive;
    use crate::core::jobs::test_support::{
        auto_options, downloading_track_job, downloading_track_job_with, duplicate_track_job,
        fail_slot, failed_track_job, failed_web_ui_job, mcp_track_job, track_job_of,
    };
    use crate::core::test_ctx;
    use crate::ledger::{JobEvent, JobState};
    use crate::server_fns::download::test_support::{search_result, transfer};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

    #[tokio::test]
    async fn full_detail_shows_the_live_transfer_and_its_place_in_the_queue() {
        let (db, acq, effects, token) = jobs_app().await;
        let job = downloading_track_job(&acq).await;
        acq.ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![DownloadProgress {
                queued_at_source: true,
                attempts: 1,
                ..transfer(
                    "carol_shares",
                    "@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac",
                    "890f943c-02e1-4d45-af76-d55e3d855684",
                    DownloadState::Queued,
                )
                .with_job(JobSlot {
                    job: job.clone(),
                    slot: 0,
                })
            }],
        });
        acq.ledger.settled().await;
        effects.stub().set_queue_position(Some(4));
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": [job], "detail": "full"}),
            )
            .await,
        );
        let attempt = &doc["jobs"][0]["slots"][0]["current_attempt"];
        assert_eq!(
            (
                attempt["queue_position"].as_u64(),
                attempt["source_presence"].as_str(),
                attempt["transfer_attempts"].as_u64()
            ),
            (Some(4), Some("online"), Some(1)),
            "{doc}"
        );
        assert_eq!(
            (attempt["source"].as_str(), attempt["remote_file"].as_str()),
            (
                Some("carol_shares"),
                Some("@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac")
            )
        );
        assert_eq!(
            doc["jobs"][0]["next"],
            json!([{"tool": "get_jobs", "args": {"job_ids": [job], "detail": "full",
                "wait_seconds": 60, "wait_for": "any_change"}},
                {"tool": "cancel_job", "args": {"job_id": job}}])
        );
    }

    #[tokio::test]
    async fn another_users_job_id_reads_like_an_unknown_one() {
        let (db, acq, _, token) = jobs_app().await;
        folder_of(&db, "bob").await;
        let bob_job = track_job_of(&acq, "bob").await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": [bob_job]}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["error"]["code"].as_str(),
                doc["error"]["message"].as_str()
            ),
            (Some("unknown_handle"), Some("Job not found"))
        );
        assert_eq!(doc["error"]["next"], json!([]));
        // One of alice's ids with bob's: the call fails naming bob's, and offers alice's alone.
        let mine = downloading_track_job(&acq).await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": [mine, bob_job]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (&error["facts"]["unknown_job_ids"], &error["next"]),
            (
                &json!([bob_job]),
                &json!([{"tool": "get_jobs", "args": {"job_ids": [mine]}}])
            )
        );
    }

    // On the real clock: the search polls once, after 2 s.
    #[tokio::test]
    async fn a_job_whose_search_found_nothing_shows_what_the_search_read() {
        let (db, acq, effects, token) = jobs_app().await;
        effects.stub().script_polls(vec![Ok(SearchResult {
            stats: SearchStats {
                query_text: "Marrow Choir O Holy Night".into(),
                response_count: 0,
                file_count: 0,
                ..Default::default()
            },
            ..search_result("search", SearchState::Completed, vec![])
        })]);
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", auto_options()).await;
        drive(&acq, &test_ctx("alice"), &job).await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": [job]}),
            )
            .await,
        );
        let failure = &doc["jobs"][0]["failure"];
        assert_eq!(
            (
                failure["code"].as_str(),
                failure["facts"]["query_text_sent"].as_str(),
                failure["facts"]["response_count"].as_u64()
            ),
            (
                Some("search_no_results"),
                Some("Marrow Choir O Holy Night"),
                Some(0)
            ),
            "{doc}"
        );
        // Other words, or let it go.
        assert_eq!(
            (doc["jobs"][0]["state"].as_str(), &failure["next"]),
            (
                Some("failed"),
                &json!([{"tool": "retry_job", "args": {"job_id": job, "mode": "new_search"}},
                    {"tool": "dismiss_job", "args": {"job_id": job}}])
            )
        );
    }

    #[tokio::test]
    async fn a_wait_returns_when_the_job_changes() {
        let (db, acq, _, token) = jobs_app().await;
        let job = downloading_track_job(&acq).await;
        let app = app_of(&db, &acq);
        let wait = tokio::spawn({
            let (app, token, job) = (app.clone(), token.clone(), job.clone());
            async move {
                call_tool(
                    app,
                    &token,
                    "get_jobs",
                    json!({"job_ids": [job], "wait_seconds": 120}),
                )
                .await
            }
        });
        // Under the router test client's 5 s read.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "the job has not changed yet");
        acq.ledger.record(JobEvent::Ended {
            job: job.clone(),
            state: JobState::Cancelled,
            failure: None,
        });
        let doc = document(&wait.await.unwrap());
        assert_eq!(doc["jobs"][0]["state"], "cancelled", "{doc}");
    }

    #[tokio::test]
    async fn a_wait_past_since_skips_a_failure_already_seen() {
        let (db, acq, _, token) = jobs_app().await;
        let first = downloading_track_job(&acq).await;
        let second = downloading_track_job_with(&acq, "Silent Night", auto_options()).await;
        let app = app_of(&db, &acq);
        // Soulful's own text for a transfer the user cancelled.
        let stopped = Failure::new(
            FailureCode::TransferCancelled,
            Stage::Transfer,
            RetryAdvice::NotRetryable,
            "Cancelled on request",
        );
        fail_slot(&acq, &first, stopped.clone()).await;
        let call = json!({"job_ids": [first, second], "wait_seconds": 120,
            "wait_for": "needs_action"});
        let seen = document(&call_tool(app.clone(), &token, "get_jobs", call.clone()).await);
        let cursor = seen["change_cursor"].clone();
        let mut again = call.clone();
        again["since"] = cursor.clone();
        assert_eq!(
            seen["next"],
            json!([{"tool": "get_jobs", "args": again}]),
            "{seen}"
        );
        let wait = tokio::spawn({
            let (app, token) = (app.clone(), token.clone());
            async move { call_tool(app, &token, "get_jobs", again).await }
        });
        // Under the router test client's 5 s read.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "the failure was already seen");
        fail_slot(&acq, &second, stopped).await;
        let doc = document(&wait.await.unwrap());
        assert!(doc["change_cursor"].as_i64() > cursor.as_i64(), "{doc}");
        // Every job ended: nothing to follow.
        assert_eq!(doc["next"], json!([]));
    }

    #[tokio::test]
    async fn an_import_found_in_the_library_names_the_copies_and_why_their_songs_are_unknown() {
        let (db, acq, _, token) = jobs_app().await;
        let dir = tempfile::tempdir().unwrap();
        let existing = ImportedItem {
            path: "Marrow Choir/Winter Songs/03 - O Holy Night.flac".into(),
            recording_mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            beets_id: Some(17),
            release_mbid: None,
            album: "Winter Songs".into(),
            match_per_mille: None,
        };
        let job = duplicate_track_job(
            &acq,
            "O Holy Night",
            &dir.path().join("03 - O Holy Night.flac"),
            vec![existing],
        )
        .await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": [job], "detail": "full"}),
            )
            .await,
        );
        let job = &doc["jobs"][0];
        assert_eq!(
            (job["state"].as_str(), job["failure"]["code"].as_str()),
            (Some("failed"), Some("import_already_in_library")),
            "{doc}"
        );
        let result = &job["results"][0];
        assert_eq!(
            (
                result["kind"].as_str(),
                &result["slots"],
                result["existing"][0]["beets_id"].as_u64(),
                result["existing"][0]["path"].as_str()
            ),
            (
                Some("already_in_library"),
                &json!([0]),
                Some(17),
                Some("Marrow Choir/Winter Songs/03 - O Holy Night.flac")
            )
        );
        // alice has no Navidrome linked: the lookup's failure stands where the songs would.
        assert_eq!(
            (
                &result["library_songs"],
                result["library_songs_failure"]["code"].as_str()
            ),
            (&Value::Null, Some("navidrome_not_linked"))
        );
    }

    #[tokio::test]
    async fn job_ids_and_a_batch_are_not_both_taken() {
        let (db, acq, _, token) = jobs_app().await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"job_ids": ["j1"], "batch_id": "b1"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (
                Some("invalid_input"),
                Some("get_jobs: batch_id must be absent when job_ids is given")
            )
        );
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_jobs",
                json!({"states": ["running"]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(error["facts"]["field"], "states");
    }

    #[tokio::test]
    async fn a_filter_pages_the_callers_jobs_in_creation_order() {
        let (db, acq, _, token) = jobs_app().await;
        let first = downloading_track_job(&acq).await;
        let second = mcp_track_job(&acq, "Marrow Choir", "Silent Night", auto_options()).await;
        folder_of(&db, "bob").await;
        track_job_of(&acq, "bob").await;
        let app = app_of(&db, &acq);
        let page = document(&call_tool(app.clone(), &token, "get_jobs", json!({"limit": 1})).await);
        assert_eq!(page["jobs"][0]["job_id"], first.as_str(), "{page}");
        let rest = document(
            &call_tool(
                app.clone(),
                &token,
                "get_jobs",
                json!({"limit": 1, "cursor": page["cursor"]}),
            )
            .await,
        );
        assert_eq!(
            (rest["jobs"][0]["job_id"].as_str(), &rest["cursor"]),
            (Some(second.as_str()), &json!(null))
        );
        let searching =
            document(&call_tool(app, &token, "get_jobs", json!({"states": ["searching"]})).await);
        let ids: Vec<&str> = searching["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["job_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![second.as_str()]);
        assert_eq!(
            searching["jobs"][0]["wanted"],
            json!({"kind": "track", "artist": "Marrow Choir", "title": "Silent Night"})
        );
    }

    #[tokio::test]
    async fn retry_job_answers_the_jobs_full_document_or_its_refusal_with_what_can_be_done() {
        let (db, acq, _, token) = jobs_app().await;
        let failed = failed_track_job(&acq).await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "retry_job",
                json!({"job_id": failed, "mode": "next_source"}),
            )
            .await,
        );
        assert_eq!(
            (doc["job_id"].as_str(), doc["state"].as_str()),
            (Some(failed.as_str()), Some("queued")),
            "{doc}"
        );
        assert!(doc["slots"].is_array() && doc["timeline"].is_array());
        let running = downloading_track_job_with(&acq, "Silent Night", auto_options()).await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "retry_job",
                json!({"job_id": running, "mode": "new_search"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "The job is downloading; retry_job new_search needs a job that is needs_source_choice, failed or cancelled"
        );
        assert_eq!(
            (&error["facts"]["state"], &error["facts"]["allowed_states"]),
            (
                &json!("downloading"),
                &json!(["needs_source_choice", "failed", "cancelled"])
            )
        );
        assert_eq!(
            error["next"],
            json!([{"tool": "get_jobs", "args": {"job_ids": [running], "detail": "full",
                "wait_seconds": 60, "wait_for": "any_change"}},
                {"tool": "cancel_job", "args": {"job_id": running}}])
        );
    }

    #[tokio::test]
    async fn search_arguments_belong_to_new_search_only() {
        let (db, acq, _, token) = jobs_app().await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "retry_job",
                json!({"job_id": "j1", "mode": "next_source", "lossless_only": true}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "retry_job: lossless_only must be absent unless mode is new_search"
        );
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "retry_job",
                json!({"job_id": "j1", "mode": "new_search", "search_id": "s1",
                    "query_text": "Marrow Choir"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "retry_job: query_text must be absent when search_id is given"
        );
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "retry_job",
                json!({"job_id": "j1"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "retry_job: mode must be one of same_source, next_source, new_search, index_only, reject_result"
        );
    }

    #[tokio::test]
    async fn cancel_job_and_dismiss_job_act_on_the_callers_jobs_only() {
        let (db, acq, _, token) = jobs_app().await;
        let job = downloading_track_job(&acq).await;
        let app = app_of(&db, &acq);
        let doc =
            document(&call_tool(app.clone(), &token, "cancel_job", json!({"job_id": job})).await);
        // Its transfer is stopped by its monitor: the job stays downloading until then.
        assert_eq!(doc["state"], "downloading", "{doc}");
        let web = failed_web_ui_job(&acq).await;
        let error =
            document(&call_tool(app.clone(), &token, "cancel_job", json!({"job_id": web})).await)
                ["error"]
                .clone();
        assert_eq!(
            error["message"],
            "cancel_job: job_id must be a job started by ensure_in_library or download_source; this one was started in Soulful's web page, where it can be cancelled or retried"
        );
        let doc =
            document(&call_tool(app.clone(), &token, "dismiss_job", json!({"job_id": web})).await);
        assert_eq!(doc["state"], "dismissed", "{doc}");
        folder_of(&db, "bob").await;
        let bobs = track_job_of(&acq, "bob").await;
        for tool in ["cancel_job", "dismiss_job"] {
            let error =
                document(&call_tool(app.clone(), &token, tool, json!({"job_id": bobs})).await)
                    ["error"]
                    .clone();
            assert_eq!(
                (error["code"].as_str(), error["message"].as_str()),
                (Some("unknown_handle"), Some("Job not found")),
                "{tool}"
            );
        }
    }

    #[tokio::test]
    async fn only_the_jobs_of_the_page_are_read_in_full() {
        let (db, acq, _, token) = jobs_app().await;
        let first = downloading_track_job(&acq).await;
        let second = downloading_track_job_with(&acq, "Silent Night", auto_options()).await;
        // The second job's slot can no longer be read: only a call showing it reads it.
        sqlx::query("UPDATE job_tracks SET last_row = 'not a row' WHERE job_id = ?")
            .bind(&second)
            .execute(&db)
            .await
            .unwrap();
        let app = app_of(&db, &acq);
        let page = document(&call_tool(app.clone(), &token, "get_jobs", json!({"limit": 1})).await);
        assert_eq!(
            (
                page["jobs"][0]["job_id"].as_str(),
                page["cursor"].is_string()
            ),
            (Some(first.as_str()), true),
            "{page}"
        );
        let rest = document(
            &call_tool(
                app,
                &token,
                "get_jobs",
                json!({"limit": 1, "cursor": page["cursor"]}),
            )
            .await,
        );
        assert_eq!(rest["error"]["code"], "settings_unreadable", "{rest}");
    }
}
