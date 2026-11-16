//! `get_batch` and `list_batches`: the caller's batches from the ledger.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use chrono::DateTime;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{answer, invalid_value, upstream_error, NextCall, ToolError};
use super::facts::{job_document, Detail, ReadNow};
use super::jobs::{needs_action, read_now, WaitFor, MAX_JOB_IDS};
use super::paging::{chars, fit, fit_whole, longest_fitting, Page};
use super::refs::{page_schema, wait_schema};
use super::server::{common_codes, ToolSpec};
use super::{wait, McpCaller};
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::jobs::{jobs_view, JobSelect, JobView};
use crate::ledger::store::{batch_of, batches_of, BatchRecord};
use crate::ledger::JobState;

const GET_BATCH_ARGS: &[&str] = &["batch_id", "since", "wait_seconds", "wait_for"];
const LIST_BATCHES_ARGS: &[&str] = &["label", "since", "limit", "cursor"];

pub(crate) const GET_BATCH: ToolSpec = ToolSpec {
    name: "get_batch",
    title: "Follow a batch",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Follows a batch ensure_in_library or download_source started. Returns counts per state, the jobs changed since the cursor (since) in summary, and full detail with next for jobs that need action (needs_source_choice, needs_decision, failed). wait_seconds (0 to 180) with wait_for any_change, needs_action or all_terminal waits on the server; work goes on whether or not anyone asks. A 40 track list with 28 downloads takes 20 to 60 minutes and about 10 to 20 calls with wait_for needs_action. Batch ids stay valid while any of their jobs is kept (30 days after it ends). Failure codes: {common}, unknown_handle.")
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
            // A change number, compared with the jobs' changed_seq.
            let since = args.opt_u32("since", 0, u32::MAX)?;
            let (wait, wait_for) = WaitFor::read(&args)?;
            args.finish()?;
            let acquire = &state.acquire;
            let since = since.map_or_else(|| *acquire.ledger.changes().borrow(), i64::from);
            let select = JobSelect::Filter {
                batch_id: Some(batch_id.clone()),
                states: vec![],
            };
            let (batch, views) = wait::until(
                &acquire.ledger,
                wait,
                || async {
                    let batch = batch_of(&acquire.db, &batch_id, &caller.ctx.id())
                        .await
                        .map_err(|e| database_failure(Stage::Input, e))?;
                    let views = match batch {
                        Some(_) => {
                            jobs_view(&acquire.db, &acquire.ledger, &caller.ctx, &select).await?
                        }
                        None => vec![],
                    };
                    Ok((batch, views))
                },
                // A batch that is no batch of the caller's fails the call at once.
                |(batch, views)| batch.is_none() || wait_for.holds(views, since),
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            let Some(batch) = batch else {
                return Err(ToolError {
                    failure: batch_not_found(),
                    facts: Map::new(),
                    tried: vec![],
                    next: vec![NextCall::new("list_batches", json!({}))],
                });
            };
            Ok(batch_document(state, caller, &batch, views, since).await)
        }
        .await,
    )
}

/// The batch's fields: its jobs counted by state from `views`, the cursor (the highest
/// changed_seq of its jobs), the jobs changed since `since` and the jobs that need action.
async fn batch_document(
    state: &ConnectionState,
    caller: &McpCaller,
    batch: &BatchRecord,
    views: Vec<JobView>,
    since: i64,
) -> Map<String, Value> {
    let mut summary: BTreeMap<&str, u32> = BTreeMap::new();
    for view in &views {
        *summary.entry(view.job.state.as_str()).or_default() += 1;
    }
    let mut changed: Vec<&JobView> = (views.iter())
        .filter(|v| v.job.changed_seq > since)
        .collect();
    changed.sort_by_key(|v| Reverse(v.job.changed_seq));
    let changed: Vec<Value> = (changed.into_iter())
        .map(|v| job_document(v, Detail::Summary, &ReadNow::default()))
        .collect();
    let Value::Object(fields) = json!({
        "batch_id": batch.id,
        "label": label_of(batch),
        "origin": batch.origin,
        "created_at": batch.created_at,
        "summary": summary,
        "cursor": views.iter().map(|v| v.job.changed_seq).max().unwrap_or(0),
        "all_terminal": views.iter().all(|v| v.job.state.is_terminal()),
    }) else {
        unreachable!("an object literal is an object");
    };
    // In creation order, as jobs_view reads them.
    let needs: Vec<JobView> = (views.into_iter())
        .filter(|v| needs_action(v.job.state))
        .collect();
    let reads = read_now(state, caller, &needs).await;
    let needs = (needs.iter().zip(&reads))
        .map(|(view, now)| (view.job.id.clone(), job_document(view, Detail::Full, now)))
        .collect();
    fit_actions(fields, needs, changed)
}

/// `fields` with `needs_action`, the full documents of `needs` (each with its job id), and
/// `changed`, the summaries of the changed jobs, newest change first, kept within
/// RESULT_MAX_CHARS once "ok" is added. The needs-action documents come first: the ids of those
/// that do not fit go in `needs_action_more`, the first MAX_JOB_IDS of them offered in `next` as
/// one get_jobs call. The changes fill what is left, `changed_more` counting those left out.
fn fit_actions(
    mut fields: Map<String, Value>,
    needs: Vec<(String, Value)>,
    changed: Vec<Value>,
) -> Map<String, Value> {
    fields.insert("needs_action".into(), json!([]));
    fields.insert("needs_action_more".into(), json!([]));
    fields.insert("next".into(), json!([]));
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
        fields.insert("next".into(), json!([more_call(more)]));
    }
    fit_whole(fields, "changed", changed, "changed_more")
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
        "created_at": batch.created_at,
        "summary": batch.counts,
        "unfinished": unfinished,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Map, Value};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};

    use super::fit_actions;
    use crate::mcp::paging::RESULT_MAX_CHARS;

    use crate::connection::test_util::{app_of, call_tool, document, jobs_app};
    use crate::core::folder_of;
    use crate::core::jobs::test_support::{batch_of_downloads, fail_slot, named_batch};

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
        assert_eq!(
            (
                &doc["needs_action_more"],
                doc["changed_more"].as_u64(),
                &doc["next"]
            ),
            (&json!([]), Some(0), &json!([]))
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
        let mut doc = fit_actions(fields, needs.clone(), changed);
        let shown = doc["needs_action"].as_array().unwrap().len();
        let more: Vec<Value> = needs[shown..].iter().map(|(id, _)| json!(id)).collect();
        assert!(shown > 1 && shown < 200, "{shown}");
        assert_eq!(doc["needs_action_more"], Value::Array(more.clone()));
        assert_eq!(
            doc["next"],
            json!([{"tool": "get_jobs", "args": {"job_ids": more[..100], "detail": "full"}}])
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
        let doc = fit_actions(Map::new(), needs, changed);
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
}
