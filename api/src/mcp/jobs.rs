//! `get_jobs`: the caller's jobs, by id or by batch and state, waiting for them to change when
//! asked.

use std::path::Path;
use std::time::Duration;

use futures::future::join_all;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::download::DownloadProgress;
use shared::failure::{Failure, Stage};
use soulbeet::navidrome::NavidromeClient;
use soulbeet::ImportedItem;

use super::args::{repeatable, Args};
use super::contract::{answer, invalid_value, too_large_to_repeat, upstream_error, ToolError};
use super::facts::{
    asks_backend, job_document, live_facts, results_of, unanswered, Detail, LiveFacts, ReadNow,
};
use super::paging::{fit, Page};
use super::refs::{page_schema, wait_schema};
use super::server::{common_codes, ToolSpec};
use super::{wait, McpCaller};
use crate::connection::ConnectionState;
use crate::core::index::library_songs;
use crate::core::jobs::{job_not_found, jobs_view, JobSelect, JobView};
use crate::core::library::{navidrome_failure, navidrome_for, SongFacts};
use crate::ledger::{JobState, SlotResult};

const GET_JOBS_ARGS: &[&str] = &[
    "job_ids",
    "batch_id",
    "states",
    "detail",
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

const MAX_JOB_IDS: usize = 100;

pub(crate) const GET_JOBS: ToolSpec = ToolSpec {
    name: "get_jobs",
    title: "Show jobs",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows jobs by job_ids, or by batch_id and states. Summary detail gives each job's state and failure code; full detail gives the current download with live bytes, speed, place in the source's queue and whether the source is online, earlier attempts with their failures (tried), the timeline, what beets applied and Navidrome lists (or, while Navidrome has not listed it yet, when the import ended and whether a scan was asked for; get_status says whether Navidrome is scanning), and next: the calls that can be made for the job now. wait_seconds (0 to 180) with wait_for any_change, needs_action or all_terminal waits for the jobs to change. Job ids stay valid 30 days after the job ends. Failure codes: {common}, unknown_handle (a job id this user has no job for; facts.unknown_job_ids names them).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"job_ids": {"type": "array", "minItems": 1, "maxItems": 100, "items": {"type": "string", "minLength": 1}}, "batch_id": {"type": "string", "minLength": 1}, "states": {"type": "array", "items": {"enum": ["searching", "queued", "downloading", "importing", "indexing", "done", "needs_source_choice", "needs_decision", "failed", "cancelled", "dismissed"]}}, "detail": {"enum": ["summary", "full"]}, "#,
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
    /// Over the selected jobs: AnyChange when one's changed_seq is above `since`; NeedsAction
    /// when one is needs_source_choice, needs_decision or failed, or all are terminal;
    /// AllTerminal when all are.
    pub(crate) fn holds(self, jobs: &[JobView], since: i64) -> bool {
        let all_terminal = || jobs.iter().all(|v| v.job.state.is_terminal());
        match self {
            WaitFor::AnyChange => jobs.iter().any(|v| v.job.changed_seq > since),
            WaitFor::NeedsAction => {
                jobs.iter().any(|v| {
                    matches!(
                        v.job.state,
                        JobState::NeedsSourceChoice | JobState::NeedsDecision | JobState::Failed
                    )
                }) || all_terminal()
            }
            WaitFor::AllTerminal => all_terminal(),
        }
    }
}

/// A get_jobs call as read.
struct Call {
    select: JobSelect,
    detail: Detail,
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
    let wait = args.opt_u32("wait_seconds", 0, 180)?.unwrap_or(0);
    let wait_for =
        match args.opt_choice("wait_for", &["any_change", "needs_action", "all_terminal"])? {
            Some("needs_action") => WaitFor::NeedsAction,
            Some("all_terminal") => WaitFor::AllTerminal,
            _ => WaitFor::AnyChange,
        };
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
        wait: Duration::from_secs(wait.into()),
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
            // The wait takes no cursor: it waits from the change number read now.
            let since = *acquire.ledger.changes().borrow();
            let views = wait::until(
                &acquire.ledger,
                call.wait,
                || jobs_view(&acquire.db, &acquire.ledger, &caller.ctx, &call.select),
                // An id that is no job of the caller's fails the call at once.
                |views| {
                    !unknown_ids(&call.select, views).is_empty()
                        || call.wait_for.holds(views, since)
                },
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            let unknown = unknown_ids(&call.select, &views);
            if !unknown.is_empty() {
                return Err(unknown_jobs(&args, &call.select, unknown));
            }
            let shown = views.get(call.page.offset..).unwrap_or_default();
            let shown = &shown[..shown.len().min(call.page.limit)];
            let reads = match call.detail {
                Detail::Summary => shown.iter().map(|_| ReadNow::default()).collect(),
                Detail::Full => read_now(state, caller, shown).await,
            };
            let documents = (shown.iter().zip(&reads))
                .map(|(view, now)| job_document(view, call.detail, now))
                .collect();
            let more = views.len() > call.page.offset + shown.len();
            Ok(fit(Map::new(), "jobs", documents, &call.page, more))
        }
        .await,
    )
}

/// The ids of `select` that are none of `views`' jobs, each once, in the order given.
fn unknown_ids(select: &JobSelect, views: &[JobView]) -> Vec<String> {
    let JobSelect::Ids(ids) = select else {
        return vec![];
    };
    let mut unknown: Vec<String> = Vec::new();
    for id in ids {
        if !views.iter().any(|v| &v.job.id == id) && !unknown.contains(id) {
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
/// transfer, all asked at once, and the songs Navidrome lists for each import beets found in the
/// library.
async fn read_now(state: &ConnectionState, caller: &McpCaller, views: &[JobView]) -> Vec<ReadNow> {
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
        let answers: Vec<Option<LiveFacts>> = match state.acquire.effects.backend().await {
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
        match navidrome_for(&*state.navidrome, &caller.ctx, Stage::LibraryCheck).await {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{json, Value};
    use shared::download::{
        DownloadProgress, DownloadState, JobSlot, SearchResult, SearchState, SearchStats,
    };
    use soulbeet::ImportedItem;
    use sqlx::SqlitePool;

    use crate::connection::test_util::{access_token, app_of, call_tool, document, RESOURCE};
    use crate::core::folder_of;
    use crate::core::jobs::runner::{drive, Acquire, TestEffects};
    use crate::core::jobs::test_support::{
        auto_options, downloading_track_job, duplicate_track_job, mcp_track_job,
        test_db_with_alice, track_job_of,
    };
    use crate::core::test_ctx;
    use crate::ledger::{JobEvent, JobState};
    use crate::server_fns::download::test_support::{search_result, transfer};

    /// The jobs of `core::jobs::test_support` are alice's (user id `alice`): a database with her,
    /// the acquisition acting on it, its effects and her access token.
    async fn jobs_app() -> (SqlitePool, Arc<Acquire>, Arc<TestEffects>, String) {
        let db = test_db_with_alice().await;
        let token = access_token(&db, "alice", RESOURCE).await;
        let (acq, effects) = Acquire::for_test(db.clone());
        (db, acq, effects, token)
    }

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
                "wait_seconds": 60, "wait_for": "any_change"}}])
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
        assert_eq!(
            (doc["jobs"][0]["state"].as_str(), &failure["next"]),
            (Some("failed"), &json!([]))
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
}
