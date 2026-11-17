//! `get_batch` and `list_batches`: the caller's batches from the ledger.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use chrono::DateTime;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{answer, error_object, invalid_value, upstream_error, NextCall, ToolError};
use super::facts::{job_document, Detail, ReadNow};
use super::jobs::{
    needs_action, read_now, since_call, untried_of, Since, WaitFor, BATCH_FOLLOW, MAX_JOB_IDS,
};
use super::paging::{chars, fit, fit_whole, longest_fitting, Page};
use super::refs::{page_schema, wait_schema};
use super::server::{common_codes, ToolSpec};
use super::{wait, McpCaller};
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::jobs::{job_records, views_of, JobSelect, JobView, Untried};
use crate::core::time;
use crate::ledger::store::{
    batch_of, batches_of, playlist_of, BatchPlaylist, BatchRecord, JobRecord,
};
use crate::ledger::JobState;

const GET_BATCH_ARGS: &[&str] = &["batch_id", "since", "wait_seconds", "wait_for"];
const LIST_BATCHES_ARGS: &[&str] = &["label", "since", "limit", "cursor"];

pub(crate) const GET_BATCH: ToolSpec = ToolSpec {
    name: "get_batch",
    title: "Follow a batch",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Follows a batch ensure_in_library or download_source started. Returns counts per state, the jobs changed since the cursor (since) in summary, and full detail with next for jobs that need action (needs_source_choice, needs_decision, failed). wait_seconds (0 to 180) waits on the server until wait_for holds: any_change (a job changed after since, or without since after the call came), needs_action (a job needs action; with since, one that changed after it) or all_terminal (every job is done, failed, cancelled or dismissed); needs_action also ends when every job is. Pass the last answer's cursor as since, so a wait does not end again for what was already seen; while a job is searching, queued, downloading, importing or indexing, next offers this call again with since set to the cursor (and a 180 second wait for needs_action when this call waited for nothing); a job waiting for a source choice or an import decision changes only once you act, through its own next in needs_action. Work goes on whether or not anyone asks. A 40 track list with 28 downloads takes 20 to 60 minutes and about 10 to 20 calls with wait_for needs_action. Batch ids stay valid while any of their jobs is kept (30 days after it ends). For a batch ensure_in_library made with a playlist, playlist gives its playlist_id, how many items are settled in it (placed, or ended with nothing to place), how many are waiting (items whose jobs are not finished, or failed: a job failed for good keeps its place waiting until it is retried or dismissed), the failure of the last fill that failed (null once one succeeds; one whose retry is neither same_source_later nor needs_user stops the fill), and hidden_entries_dropped, the entries of libraries this user cannot see that the fill dropped when it placed songs between others and so rewrote the list. Failure codes: {common}, unknown_handle.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["batch_id"], "properties": {"batch_id": {"type": "string", "minLength": 1}, "since": {"type": "integer", "minimum": 0}, "#,
        wait_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_batch(state, caller, args)),
};

pub(crate) const LIST_BATCHES: ToolSpec = ToolSpec {
    name: "list_batches",
    title: "List batches",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Lists this user's batches, newest first, so a later chat can find earlier work by label or date. Failure codes: {common}.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"label": {"type": "string", "minLength": 1}, "since": {"type": "string", "format": "date-time"}, "#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(list_batches(state, caller, args)),
};

/// Batch not found: unknown_handle, input, not_retryable, "Batch not found".
pub(crate) fn batch_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Batch not found",
    )
}

/// `get_batch`: the caller's batch once `wait_for` holds for its jobs or `wait_seconds` passed.
pub(crate) async fn get_batch(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_batch", args, GET_BATCH_ARGS);
    answer(
        async {
            let batch_id = args.string("batch_id")?;
            // A change number, compared with the jobs' user_seq.
            let since = args.opt_u32("since", 0, u32::MAX)?;
            let (wait, wait_for) = WaitFor::read(&args)?;
            args.finish()?;
            let acquire = &state.acquire;
            let given = since.map(i64::from);
            let since = (Since::of(given, &acquire.db, &caller.ctx).await)
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            let select = JobSelect::Filter {
                batch_id: Some(batch_id.clone()),
                states: vec![],
            };
            let failed = |f| upstream_error(f, Map::new(), args.same_call());
            // The wait and the counts read records; only the jobs shown are read in full.
            let (batch, records) = wait::until(
                &acquire.ledger,
                wait,
                || async {
                    let batch = batch_of(&acquire.db, &batch_id, &caller.ctx.id())
                        .await
                        .map_err(|e| database_failure(Stage::Input, e))?;
                    let records = match batch {
                        Some(_) => job_records(&acquire.db, &caller.ctx, &select).await?,
                        None => vec![],
                    };
                    Ok((batch, records))
                },
                // A batch that is no batch of the caller's fails the call at once.
                |(batch, records)| batch.is_none() || wait_for.holds(records, since),
            )
            .await
            .map_err(failed)?;
            let Some(batch) = batch else {
                return Err(ToolError {
                    failure: batch_not_found(),
                    facts: Map::new(),
                    tried: vec![],
                    next: vec![NextCall::new("list_batches", json!({}))],
                });
            };
            let again = args.as_value();
            batch_document(state, caller, &batch, records, since.seq(), again)
                .await
                .map_err(failed)
        }
        .await,
    )
}

/// The batch's fields: its jobs counted by state from `jobs`, the cursor (the highest
/// user_seq of its jobs), the jobs changed since `since` and the jobs that need action, the
/// last two read in full. While a job is unfinished, `next` offers the call `again` (the
/// arguments given) with the cursor as its `since`.
async fn batch_document(
    state: &ConnectionState,
    caller: &McpCaller,
    batch: &BatchRecord,
    jobs: Vec<JobRecord>,
    since: i64,
    again: Value,
) -> Result<Map<String, Value>, Failure> {
    let mut summary: BTreeMap<&str, u32> = BTreeMap::new();
    for job in &jobs {
        *summary.entry(job.state.as_str()).or_default() += 1;
    }
    let cursor = jobs.iter().map(|job| job.user_seq).max().unwrap_or(0);
    let all_terminal = jobs.iter().all(|job| job.state.is_terminal());
    // Only a job that moves on its own changes without the caller: a job stranded until the
    // next start does not change before it, one waiting for a person not before they act.
    let moving = (jobs.iter())
        .any(|job| job.state.moves_on_its_own() && !state.acquire.ledger.stranded(&job.id));
    let follow = moving.then(|| since_call("get_batch", again, cursor, BATCH_FOLLOW));
    let shown = (jobs.iter())
        .filter(|job| job.user_seq > since || needs_action(job.state))
        .cloned()
        .collect();
    let acquire = &state.acquire;
    let views = views_of(&acquire.db, &acquire.ledger, &caller.ctx, shown).await?;
    let untried = untried_of(&acquire.db, &views).await?;
    let mut changed: Vec<(&JobView, &Untried)> = (views.iter().zip(&untried))
        .filter(|(v, _)| v.job.user_seq > since)
        .collect();
    changed.sort_by_key(|(v, _)| Reverse(v.job.user_seq));
    let deletion_enabled = caller.allow_delete;
    let playlist = (playlist_of(&acquire.db, &batch.id, &caller.ctx.id()).await)
        .map_err(|e| database_failure(Stage::Input, e))?;
    let changed: Vec<Value> = (changed.into_iter())
        .map(|(v, untried)| {
            job_document(
                v,
                untried,
                Detail::Summary,
                &ReadNow::default(),
                deletion_enabled,
            )
        })
        .collect();
    let Value::Object(fields) = json!({
        "batch_id": batch.id,
        "label": label_of(batch),
        "origin": batch.origin,
        "created_at": time::unix(batch.created_at),
        "summary": summary,
        "cursor": cursor,
        "all_terminal": all_terminal,
        "playlist": playlist.map(playlist_state),
    }) else {
        unreachable!("an object literal is an object");
    };
    // In creation order, as job_records reads them.
    let (needs, needs_untried): (Vec<JobView>, Vec<Untried>) = (views.into_iter().zip(untried))
        .filter(|(v, _)| needs_action(v.job.state))
        .unzip();
    let reads = read_now(acquire, &*state.navidrome, &caller.ctx, &needs).await;
    let needs = (needs.iter().zip(&needs_untried).zip(&reads))
        .map(|((view, untried), now)| {
            let document = job_document(view, untried, Detail::Full, now, deletion_enabled);
            (view.job.id.clone(), document)
        })
        .collect();
    Ok(fit_actions(fields, needs, changed, follow))
}

/// `fields` with `needs_action`, the full documents of `needs` (each with its job id), and
/// `changed`, the summaries of the changed jobs, newest change first, kept within
/// RESULT_MAX_CHARS once "ok" is added. The needs-action documents come first: the ids of those
/// that do not fit go in `needs_action_more`, the first MAX_JOB_IDS of them offered in `next` as
/// one get_jobs call, before `follow`. The changes fill what is left, `changed_more` counting
/// those left out.
fn fit_actions(
    mut fields: Map<String, Value>,
    needs: Vec<(String, Value)>,
    changed: Vec<Value>,
    follow: Option<NextCall>,
) -> Map<String, Value> {
    fields.insert("needs_action".into(), json!([]));
    fields.insert("needs_action_more".into(), json!([]));
    fields.insert("next".into(), json!(follow.iter().collect::<Vec<_>>()));
    // Room for the changes' empty list and their count at its longest.
    fields.insert("changed".into(), json!([]));
    fields.insert("changed_more".into(), json!(changed.len()));
    let mut empty = fields.clone();
    empty.insert("ok".into(), true.into());
    let base = chars(&Value::Object(empty));
    let (ids, mut documents): (Vec<String>, Vec<Value>) = needs.into_iter().unzip();
    let more_call = |more: &[String]| {
        let shown = &more[..more.len().min(MAX_JOB_IDS)];
        NextCall::new("get_jobs", json!({"job_ids": shown, "detail": "full"}))
    };
    // What the ids left out and their call add to the empty lists.
    let tail = |k: usize| match &ids[k..] {
        [] => 0,
        more => chars(&json!(more)) - 2 + chars(&json!(more_call(more))),
    };
    let k = longest_fitting(base, &documents, documents.len(), tail);
    documents.truncate(k);
    fields.insert("needs_action".into(), Value::Array(documents));
    let more = &ids[k..];
    if !more.is_empty() {
        fields.insert("needs_action_more".into(), json!(more));
        let next: Vec<NextCall> = Some(more_call(more)).into_iter().chain(follow).collect();
        fields.insert("next".into(), json!(next));
    }
    fit_whole(fields, "changed", changed, "changed_more")
}

/// A batch's playlist as get_batch shows it: its items settled and waiting, the failure of its
/// last fill as an error with no facts or calls, its message cut to TEXT_MAX_CHARS, and the
/// hidden entries its fill dropped.
fn playlist_state(playlist: BatchPlaylist) -> Value {
    let settled = playlist.order.iter().filter(|entry| entry.settled).count();
    let failure = playlist.failure.map(|mut failure| {
        failure.message = cut_json(failure.message, TEXT_MAX_CHARS);
        error_object(&ToolError {
            failure,
            facts: Map::new(),
            tried: vec![],
            next: vec![],
        })
    });
    json!({"playlist_id": playlist.playlist_id, "settled": settled,
        "waiting": playlist.order.len() - settled, "failure": failure,
        "hidden_entries_dropped": playlist.hidden_dropped})
}

/// The batch's label, cut as a text from outside Soulful (an album title).
fn label_of(batch: &BatchRecord) -> Option<String> {
    (batch.label.clone()).map(|label| cut_json(label, TEXT_MAX_CHARS))
}

/// `list_batches`: a page of the caller's batches, newest first.
pub(crate) async fn list_batches(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_batches", args, LIST_BATCHES_ARGS);
    answer(
        async {
            let label = args.opt_string("label")?;
            let refused = || {
                invalid_value(
                    "list_batches",
                    "since",
                    "an RFC 3339 time such as 2026-11-15T10:00:00Z",
                )
            };
            let since = match args.opt_string("since").map_err(|_| refused())? {
                Some(text) => Some(
                    DateTime::parse_from_rfc3339(&text)
                        .map_err(|_| refused())?
                        .timestamp(),
                ),
                None => None,
            };
            let page = Page::read(&args, json!({"label": label, "since": since}))?;
            args.finish()?;
            let batches = batches_of(&state.acquire.db, &caller.ctx.id(), label.as_deref(), since)
                .await
                .map_err(|e| {
                    let failure = database_failure(Stage::Input, e);
                    upstream_error(failure, Map::new(), args.same_call())
                })?;
            let shown = (batches.get(page.offset..).unwrap_or_default().iter())
                .map(batch_summary)
                .collect();
            Ok(fit(Map::new(), "batches", shown, &page, false))
        }
        .await,
    )
}

/// One batch as `list_batches` shows it; `unfinished` counts its jobs not yet in a terminal state.
fn batch_summary(batch: &BatchRecord) -> Value {
    let unfinished: u32 = (batch.counts.iter())
        .filter(|(state, _)| JobState::parse(state).is_some_and(|s| !s.is_terminal()))
        .map(|(_, n)| n)
        .sum();
    json!({
        "batch_id": batch.id,
        "label": label_of(batch),
        "origin": batch.origin,
        "created_at": time::unix(batch.created_at),
        "summary": batch.counts,
        "unfinished": unfinished,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Map, Value};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};

    use super::{fit_actions, NextCall};
    use crate::mcp::paging::RESULT_MAX_CHARS;

    use crate::connection::test_util::{app_of, call_tool, document, jobs_app};
    use crate::core::folder_of;
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{batch_of_downloads, fail_slot, named_batch};
    use crate::ledger::{JobEvent, JobState};

    // On the real clock: the ledger's writer answers on SQLite's own thread.
    #[tokio::test]
    async fn a_batch_wait_for_needs_action_returns_with_the_failed_job_in_full() {
        let (db, acq, _, token) = jobs_app().await;
        let (batch, jobs) = batch_of_downloads(&acq, 2).await;
        let app = app_of(&db, &acq);
        let first = document(
            &call_tool(app.clone(), &token, "get_batch", json!({"batch_id": batch})).await,
        );
        assert_eq!(
            (
                first["summary"].clone(),
                first["changed"].clone(),
                first["all_terminal"].clone()
            ),
            (json!({"downloading": 2}), json!([]), json!(false)),
            "{first}"
        );
        let cursor = first["cursor"].clone();
        // A call that waited for nothing is followed by one that waits.
        assert_eq!(
            first["next"],
            json!([{"tool": "get_batch", "args": {"batch_id": batch, "since": cursor,
                "wait_seconds": 180, "wait_for": "needs_action"}}])
        );
        let wait = tokio::spawn({
            let (app, token, batch, cursor) =
                (app.clone(), token.clone(), batch.clone(), cursor.clone());
            async move {
                call_tool(
                    app,
                    &token,
                    "get_batch",
                    json!({"batch_id": batch, "since": cursor,
                    "wait_seconds": 180, "wait_for": "needs_action"}),
                )
                .await
            }
        });
        // Under the router test client's 5 s read.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "no job needs action yet");
        // slskd's text for a peer that queued and then denied the file (failure texts 2.2,
        // Application.cs:668).
        fail_slot(
            &acq,
            &jobs[1],
            Failure::new(
                FailureCode::TransferRejectedNotShared,
                Stage::Transfer,
                RetryAdvice::OtherSource,
                "File not shared.",
            )
            .with_cause(Service::Slskd, None, "File not shared."),
        )
        .await;
        let doc = document(&wait.await.unwrap());
        assert_eq!(
            (
                doc["summary"]["downloading"].as_u64(),
                doc["summary"]["failed"].as_u64()
            ),
            (Some(1), Some(1)),
            "{doc}"
        );
        assert_eq!(
            doc["changed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|j| j["job_id"].clone())
                .collect::<Vec<_>>(),
            vec![json!(jobs[1])]
        );
        assert_eq!(
            doc["needs_action"][0]["failure"]["code"],
            "transfer_rejected_not_shared"
        );
        // A full document: its slots and timeline are there.
        assert!(doc["needs_action"][0]["slots"].is_array(), "{doc}");
        assert!(doc["cursor"].as_i64() > cursor.as_i64());
        // A job still downloads: the same call again, past what this answer showed.
        assert_eq!(
            (
                &doc["needs_action_more"],
                doc["changed_more"].as_u64(),
                &doc["next"]
            ),
            (
                &json!([]),
                Some(0),
                &json!([{"tool": "get_batch", "args": {"batch_id": batch, "since": doc["cursor"],
                    "wait_seconds": 180, "wait_for": "needs_action"}}])
            )
        );
    }

    #[tokio::test]
    async fn a_batch_whose_only_unfinished_job_waits_for_a_decision_is_not_followed() {
        let (db, acq, _, token) = jobs_app().await;
        let (batch, jobs) = batch_of_downloads(&acq, 2).await;
        denied(&acq, &jobs[0]).await;
        let row = (crate::core::jobs::test_support::view_of(&acq, &jobs[1]).await).slots[0]
            .row
            .clone()
            .unwrap();
        acq.ledger.record(JobEvent::Slots {
            state: JobState::NeedsDecision,
            rows: vec![row],
        });
        acq.ledger.settled().await;
        let app = app_of(&db, &acq);
        let doc = document(&call_tool(app, &token, "get_batch", json!({"batch_id": batch})).await);
        assert_eq!(
            doc["summary"],
            json!({"failed": 1, "needs_decision": 1}),
            "{doc}"
        );
        // Nothing changes until the user decides: no wait to follow, the job's own next acts.
        assert_eq!(doc["next"], json!([]), "{doc}");
        assert_eq!(doc["needs_action"][1]["job_id"], json!(jobs[1]), "{doc}");
    }

    /// The transfer of `job` was denied, no source left.
    async fn denied(acq: &Acquire, job: &str) {
        // slskd's text for a peer that queued and then denied the file (failure texts 2.2).
        let text = "File not shared.";
        let failure = Failure::new(
            FailureCode::TransferRejectedNotShared,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            text,
        )
        .with_cause(Service::Slskd, None, text);
        fail_slot(acq, job, failure).await;
    }

    /// get_batch of `batch` with `args` added, in its own task.
    fn waiting_get_batch(
        app: &axum::Router,
        token: &str,
        batch: &str,
        args: Value,
    ) -> tokio::task::JoinHandle<Value> {
        let (app, token) = (app.clone(), token.to_string());
        let mut call = json!({"batch_id": batch});
        call.as_object_mut()
            .unwrap()
            .extend(args.as_object().unwrap().clone());
        tokio::spawn(async move { call_tool(app, &token, "get_batch", call).await })
    }

    // On the real clock: the ledger's writer answers on SQLite's own thread.
    #[tokio::test]
    async fn a_needs_action_wait_past_a_seen_failure_waits_for_the_next_one_or_the_end() {
        let (db, acq, _, token) = jobs_app().await;
        let (batch, jobs) = batch_of_downloads(&acq, 3).await;
        let app = app_of(&db, &acq);
        denied(&acq, &jobs[0]).await;
        let wait = json!({"wait_seconds": 180, "wait_for": "needs_action"});
        // Without since, a job that needs action answers at once.
        let seen = document(
            &waiting_get_batch(&app, &token, &batch, wait.clone())
                .await
                .unwrap(),
        );
        assert_eq!(seen["summary"]["failed"], 1, "{seen}");
        // Past the failure seen, the wait sleeps until another job needs action.
        let mut since = wait.clone();
        since["since"] = seen["cursor"].clone();
        let next = waiting_get_batch(&app, &token, &batch, since);
        // Under the router test client's 5 s read.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!next.is_finished(), "the failure was already seen");
        denied(&acq, &jobs[1]).await;
        let doc = document(&next.await.unwrap());
        assert_eq!(
            (&doc["summary"], &doc["changed"][0]["job_id"]),
            (&json!({"downloading": 1, "failed": 2}), &json!(jobs[1])),
            "{doc}"
        );
        // Past both failures, the wait sleeps until every job has ended.
        let mut since = wait;
        since["since"] = doc["cursor"].clone();
        let end = waiting_get_batch(&app, &token, &batch, since);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!end.is_finished(), "both failures were already seen");
        acq.ledger.record(JobEvent::Ended {
            job: jobs[2].clone(),
            state: JobState::Cancelled,
            failure: None,
        });
        let doc = document(&end.await.unwrap());
        assert_eq!(
            (&doc["summary"], &doc["all_terminal"]),
            (&json!({"cancelled": 1, "failed": 2}), &json!(true)),
            "{doc}"
        );
    }

    #[tokio::test]
    async fn only_the_jobs_a_batch_shows_are_read_in_full() {
        let (db, acq, _, token) = jobs_app().await;
        let (batch, jobs) = batch_of_downloads(&acq, 2).await;
        // The second job's slot can no longer be read: a batch showing neither change nor
        // action needs only its state.
        sqlx::query("UPDATE job_tracks SET last_row = 'not a row' WHERE job_id = ?")
            .bind(&jobs[1])
            .execute(&db)
            .await
            .unwrap();
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "get_batch",
                json!({"batch_id": batch}),
            )
            .await,
        );
        assert_eq!(
            (&doc["summary"], &doc["changed"], &doc["needs_action"]),
            (&json!({"downloading": 2}), &json!([]), &json!([])),
            "{doc}"
        );
    }

    #[test]
    fn jobs_past_the_cap_are_named_and_offered_to_get_jobs() {
        let pad = "x".repeat(1_000);
        let needs: Vec<(String, Value)> = (0..200)
            .map(|i| {
                let id = format!("{i:036}");
                (id.clone(), json!({"job_id": id, "pad": pad}))
            })
            .collect();
        let changed: Vec<Value> = (0..40).map(|i| json!({"job_id": i, "pad": pad})).collect();
        let fields = Map::from_iter([("batch_id".to_string(), json!("b1"))]);
        let follow = NextCall::new("get_batch", json!({"batch_id": "b1", "since": 7}));
        let mut doc = fit_actions(fields, needs.clone(), changed, Some(follow.clone()));
        let shown = doc["needs_action"].as_array().unwrap().len();
        let more: Vec<Value> = needs[shown..].iter().map(|(id, _)| json!(id)).collect();
        assert!(shown > 1 && shown < 200, "{shown}");
        assert_eq!(doc["needs_action_more"], Value::Array(more.clone()));
        assert_eq!(
            doc["next"],
            json!([{"tool": "get_jobs", "args": {"job_ids": more[..100], "detail": "full"}},
                follow])
        );
        // The needs-action documents leave no room for the changes: they are counted.
        assert_eq!(
            (doc["changed"].clone(), doc["changed_more"].clone()),
            (json!([]), json!(40))
        );
        doc.insert("ok".into(), json!(true));
        assert!(Value::Object(doc).to_string().chars().count() <= RESULT_MAX_CHARS);

        // With room, the newest changes come first and the oldest are counted.
        let needs = needs[..1].to_vec();
        let changed: Vec<Value> = (0..80).map(|i| json!({"job_id": i, "pad": pad})).collect();
        let doc = fit_actions(Map::new(), needs, changed, None);
        let kept = doc["changed"].as_array().unwrap().len();
        assert!(kept > 1 && kept < 80, "{kept}");
        assert_eq!(doc["changed"][0]["job_id"], 0);
        assert_eq!(doc["changed_more"], json!(80 - kept));
        assert_eq!(
            (&doc["needs_action_more"], &doc["next"]),
            (&json!([]), &json!([]))
        );
    }

    #[tokio::test]
    async fn batches_are_listed_newest_first_and_found_by_label() {
        let (db, acq, _, token) = jobs_app().await;
        named_batch(&acq, "Advent").await;
        named_batch(&acq, "Christmas 2026").await;
        let doc = document(&call_tool(app_of(&db, &acq), &token, "list_batches", json!({})).await);
        assert_eq!(
            doc["batches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["label"].clone())
                .collect::<Vec<_>>(),
            vec![json!("Christmas 2026"), json!("Advent")]
        );
        assert_eq!(
            (
                &doc["batches"][0]["origin"],
                &doc["batches"][0]["summary"],
                &doc["batches"][0]["unfinished"]
            ),
            (&json!("mcp"), &json!({"searching": 1}), &json!(1)),
            "{doc}"
        );
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "list_batches",
                json!({"label": "christmas"}),
            )
            .await,
        );
        assert_eq!(doc["batches"].as_array().unwrap().len(), 1);
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "list_batches",
                json!({"since": "yesterday"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "list_batches: since must be an RFC 3339 time such as 2026-11-15T10:00:00Z"
        );
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "list_batches",
                json!({"since": "2999-01-01T00:00:00Z"}),
            )
            .await,
        );
        assert_eq!(doc["batches"], json!([]));
    }

    #[tokio::test]
    async fn another_users_batch_reads_like_an_unknown_one() {
        let (db, acq, _, token) = jobs_app().await;
        folder_of(&db, "bob").await;
        let bob = crate::core::jobs::test_support::track_job_of(&acq, "bob").await;
        let bob_batch =
            crate::ledger::store::job_record(&db, &bob, &crate::auth::UserId::new("bob"))
                .await
                .unwrap()
                .unwrap()
                .batch_id;
        for batch in [bob_batch.as_str(), "3f1c0d52-8a8e-4c2b-9d61-2b7f0e5a4c19"] {
            let result = call_tool(
                app_of(&db, &acq),
                &token,
                "get_batch",
                json!({"batch_id": batch}),
            )
            .await;
            assert_eq!(result["isError"], true);
            let error = &document(&result)["error"];
            assert_eq!(
                (
                    error["code"].as_str(),
                    error["stage"].as_str(),
                    error["retry"].as_str(),
                    error["message"].as_str()
                ),
                (
                    Some("unknown_handle"),
                    Some("input"),
                    Some("not_retryable"),
                    Some("Batch not found")
                )
            );
            assert_eq!(error["next"], json!([{"tool": "list_batches", "args": {}}]));
        }
    }

    #[test]
    fn a_fill_failure_shows_as_an_error_with_its_message_cut() {
        use crate::core::playlists::playlist_not_found;
        use crate::ledger::store::BatchPlaylist;
        use crate::ledger::OrderEntry;
        use shared::failure::{json_chars, TEXT_MAX_CHARS};
        // An id as long as a caller may send one makes the longest message Soulful writes here.
        let id = "p".repeat(500);
        let entry = |settled: bool| OrderEntry {
            job_id: (!settled).then(|| "j1".to_string()),
            song_ids: vec![],
            settled,
        };
        let state = super::playlist_state(BatchPlaylist {
            playlist_id: id.clone(),
            order: vec![entry(true), entry(false), entry(false)],
            failure: Some(playlist_not_found(&id)),
            hidden_dropped: 2,
        });
        assert_eq!(
            (
                &state["settled"],
                &state["waiting"],
                &state["failure"]["code"]
            ),
            (&json!(1), &json!(2), &json!("playlist_not_found"))
        );
        assert_eq!(state["hidden_entries_dropped"], 2);
        assert_eq!(
            (&state["failure"]["facts"], &state["failure"]["next"]),
            (&json!({}), &json!([]))
        );
        let message = state["failure"]["message"].as_str().unwrap();
        assert!(json_chars(message) <= TEXT_MAX_CHARS, "{message}");
    }
}
