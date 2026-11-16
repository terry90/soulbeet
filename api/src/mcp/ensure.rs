//! `ensure_in_library`: says which tracks the library has and starts a job for each other item,
//! all in one batch.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{FailureCode, Stage};

use super::args::{repeatable, Args};
use super::contract::{
    failed, items, too_large_to_repeat, upstream_error, ItemResult, Items, NextCall, ToolError,
};
use super::keys::{keyed, request_key};
use super::refs::{
    filters, filters_schema, reference_schema, reference_value, request_key_schema, wanted,
    REFERENCE_FIELDS,
};
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::acquire::AUTO_SELECT_SCORE_THRESHOLD;
use crate::core::index::NavidromeOf;
use crate::core::jobs::runner::{Acquire, MCP_DOWNLOADS_PER_USER};
use crate::core::jobs::{ensure, CallRefused, EnsureItem, Wanted};
use crate::ledger::{JobOptions, JobState, Pick, MCP_UNFINISHED_JOBS_PER_USER};
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;

const TOOL: &str = "ensure_in_library";

const ENSURE_ARGS: &[&str] = &[
    "items",
    "folder_id",
    "pick",
    "min_score",
    "formats",
    "min_bitrate_kbps",
    "lossless_only",
    "max_queue_length",
    "failover",
    "max_attempts",
    "label",
    "request_key",
];

/// The most items one call takes.
const MAX_ITEMS: usize = 50;

pub(crate) const ENSURE_IN_LIBRARY: ToolSpec = ToolSpec {
    name: TOOL,
    title: "Get music into the library",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("The main way to get music: for up to {MAX_ITEMS} track or album references, says which tracks the library already has (by recording MBID or exact artist and title; fuzzy matches are listed, not accepted) and starts a job for each other item, all in one batch. Returns at once with batch_id; follow it with get_batch. Jobs search the download backend one search at a time, pick the best source at min_score (default {AUTO_SELECT_SCORE_THRESHOLD}) whose source has not failed 3 times today, or wait for download_source with pick review, download at most {MCP_DOWNLOADS_PER_USER} at a time per user, fail over to other sources (failover off, or max_attempts, default {MAX_SOURCES_PER_TRACK}), import with beets (never replacing a library file: a duplicate ends import_already_in_library with the library's song ids) and wait until Navidrome lists the file. A user without Navidrome gets a job for every item (library_checked false). A user has at most {MCP_UNFINISHED_JOBS_PER_USER} unfinished jobs from connected apps: an item past that is refused rate_limited. A 40 track list with 28 downloads takes 20 to 60 minutes. Album references need release_mbid. Failure codes: {common}, folder_required, unknown_handle (folder_id), importer_broken, backend_not_configured, already_running (per item: the job_id that already wants this recording; or a call with this request_key still runs), {NAVIDROME_CODES} (from the library lookup, per item or for the call), partial.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["items"], "properties": {"items": {"type": "array", "minItems": 1, "maxItems": 50, "items": "#,
        reference_schema!(),
        r#"}, "folder_id": {"type": "string", "minLength": 1}, "pick": {"enum": ["auto", "review"]}, "min_score": {"type": "number", "minimum": 0, "maximum": 1}, "#,
        filters_schema!(),
        r#", "failover": {"enum": ["auto", "off"]}, "max_attempts": {"type": "integer", "minimum": 1, "maximum": 10}, "label": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(ensure_in_library(state, caller, args)),
};

/// An `ensure_in_library` call as read.
struct Ensure {
    items: Vec<Wanted>,
    /// Each item's object as given, for the call that tries it again alone.
    given: Vec<Value>,
    folder_id: Option<String>,
    label: Option<String>,
    options: JobOptions,
    /// The arguments as given, without `request_key`: what a call offered again starts from.
    call: Value,
}

/// `ensure_in_library`: every value read, then the call once per request key.
pub(crate) async fn ensure_in_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(TOOL, args, ENSURE_ARGS);
    let (read, key) = match read_ensure(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = json!({"items": read.items.iter().map(reference_value).collect::<Vec<_>>(),
        "folder_id": read.folder_id, "label": read.label, "options": read.options});
    let (acquire, navidrome, ctx) = (
        state.acquire.clone(),
        state.navidrome.clone(),
        caller.ctx.clone(),
    );
    let work = async move { run(&acquire, &*navidrome, &ctx, read).await };
    keyed(&state.acquire, caller, TOOL, key, &call, work).await
}

fn read_ensure(args: &Args) -> Result<(Ensure, Option<String>), ToolError> {
    let elements = args.objects("items", 1, MAX_ITEMS, REFERENCE_FIELDS)?;
    let items = elements.iter().map(wanted).collect::<Result<Vec<_>, _>>()?;
    let folder_id = args.opt_string("folder_id")?;
    let pick = match args.opt_choice("pick", &["auto", "review"])? {
        Some("review") => Pick::Review,
        _ => Pick::Auto,
    };
    let min_score = (args.opt_unit("min_score")?).unwrap_or(AUTO_SELECT_SCORE_THRESHOLD);
    let filters = filters(args)?;
    let failover_off = args.opt_choice("failover", &["auto", "off"])? == Some("off");
    let max_attempts = args.opt_u32("max_attempts", 1, 10)?;
    let max_sources = if failover_off {
        args.absent("max_attempts", "absent when failover is off")?;
        1
    } else {
        max_attempts.unwrap_or(MAX_SOURCES_PER_TRACK as u32)
    };
    let label = args.opt_string("label")?;
    let key = request_key(args)?;
    args.finish()?;
    let read = Ensure {
        items,
        given: elements.iter().map(Args::as_value).collect(),
        folder_id,
        label,
        options: JobOptions {
            pick,
            min_score,
            filters,
            max_sources,
        },
        call: args.as_value(),
    };
    Ok((read, key))
}

/// The call: a call-level refusal, or each item's result with the batch, whether the library
/// was looked in, and the call that follows the batch.
async fn run(
    acquire: &std::sync::Arc<Acquire>,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    read: Ensure,
) -> CallToolResult {
    let Ensure {
        items: wanted,
        given,
        folder_id,
        label,
        options,
        call,
    } = read;
    let folder_id = folder_id.as_deref();
    let ensured = match ensure(acquire, navidrome, ctx, folder_id, label, wanted, options).await {
        Ok(ensured) => ensured,
        Err(refused) => return failed(call_refused(refused, &call)),
    };
    let results = (ensured.items.into_iter())
        .zip(&given)
        .map(|(item, given)| item_result(item, given, &call))
        .collect();
    let next: Vec<NextCall> = (ensured.batch_id.iter())
        .map(|batch_id| {
            NextCall::new(
                "get_batch",
                json!({"batch_id": batch_id, "wait_seconds": 180, "wait_for": "needs_action"}),
            )
        })
        .collect();
    let fields = Map::from_iter([
        ("batch_id".to_string(), json!(ensured.batch_id)),
        (
            "library_checked".to_string(),
            json!(ensured.library_checked),
        ),
        ("next".to_string(), json!(next)),
    ]);
    items(Items {
        stage: Stage::Input,
        noun: "items",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    })
}

/// One item's entry. A refused one offers what follows its failure: the job that already wants
/// the recording, the user's batches when they hold the most jobs allowed, else this call for that
/// item alone when the same source can answer later (D13).
fn item_result(item: EnsureItem, given: &Value, call: &Value) -> ItemResult {
    let fields = match item {
        EnsureItem::InLibrary { song_ids } => json!({"status": "in_library", "song_ids": song_ids}),
        EnsureItem::Created { job_id, fuzzy } => json!({"status": "job_created", "job_id": job_id,
            "state": JobState::Searching.as_str(), "fuzzy_matches": fuzzy}),
        EnsureItem::Refused(failure, facts) => {
            let next = match failure.code {
                FailureCode::AlreadyRunning => vec![NextCall::new(
                    "get_jobs",
                    json!({"job_ids": [facts["job_id"]], "detail": "full"}),
                )],
                FailureCode::RateLimited => vec![NextCall::new("list_batches", json!({}))],
                _ => {
                    let mut alone = call.clone();
                    alone["items"] = json!([given]);
                    return ItemResult::Failed(upstream_error(
                        failure,
                        facts,
                        repeatable(TOOL, alone),
                    ));
                }
            };
            return ItemResult::Failed(ToolError {
                failure,
                facts,
                tried: vec![],
                next,
            });
        }
    };
    let Value::Object(fields) = fields else {
        unreachable!("an object literal is an object");
    };
    ItemResult::Done(fields)
}

/// A refusal of the whole call. folder_required with folders lists them and offers the call with
/// each one's folder_id; any other failure offers the call again when the same source can answer
/// later (D13).
fn call_refused(refused: CallRefused, call: &Value) -> ToolError {
    let CallRefused {
        mut failure,
        folders,
    } = refused;
    if folders.is_empty() {
        return upstream_error(failure, Map::new(), repeatable(TOOL, call.clone()));
    }
    let listed: Vec<Value> = (folders.iter())
        .map(|f| json!({"folder_id": f.id, "name": f.name}))
        .collect();
    let next: Vec<NextCall> = (folders.iter())
        .filter_map(|f| {
            let mut with_folder = call.clone();
            with_folder["folder_id"] = json!(f.id);
            repeatable(TOOL, with_folder)
        })
        .collect();
    if next.is_empty() {
        failure.message = too_large_to_repeat(&failure.message);
    }
    ToolError {
        failure,
        facts: Map::from_iter([("folders".to_string(), json!(listed))]),
        tried: vec![],
        next,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use crate::connection::test_util::{
        add_folder, app_of, app_with_acquire, app_with_navidrome, call_tool, count, document,
        jobs_app, set_default_folder,
    };
    use crate::core::jobs::test_support::batch_of_searches;
    use crate::import_queue::test_support::broken;
    use crate::ledger::MCP_UNFINISHED_JOBS_PER_USER;
    use crate::test_navidrome::{native_song, NavidromeFor, NavidromeStub};

    #[tokio::test]
    async fn owned_tracks_are_reported_and_the_rest_become_jobs_in_one_batch() {
        let (db, acq, _, token) = app_with_acquire().await;
        let navi = NavidromeStub::start();
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+Silent+Night",
            200,
            &format!(
                "[{}]",
                native_song(
                    "3sG8kPq0WmZ1xYbN4vTcDe",
                    "Marrow Choir",
                    "Silent Night",
                    222.0,
                    None
                )
            ),
        );
        navi.answer_when(
            "/api/song",
            "title=0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c",
            200,
            "[]",
        );
        navi.answer_when("/api/song", "title=Marrow+Choir+O+Holy+Night", 200, "[]");
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let doc = document(
            &call_tool(
                app,
                &token,
                "ensure_in_library",
                json!({"items": [
                {"artist": "Marrow Choir", "title": "Silent Night"},
                {"artist": "Marrow Choir", "title": "O Holy Night",
                    "recording_mbid": "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"}]}),
            )
            .await,
        );
        assert_eq!(
            doc["results"][0],
            json!({"index": 0, "ok": true, "status": "in_library", "song_ids": ["3sG8kPq0WmZ1xYbN4vTcDe"]}),
            "{doc}"
        );
        assert_eq!(
            (
                doc["results"][1]["status"].as_str(),
                doc["results"][1]["state"].as_str()
            ),
            (Some("job_created"), Some("searching"))
        );
        assert_eq!(
            doc["summary"],
            json!({"requested": 2, "succeeded": 2, "failed": 0, "pending": 0})
        );
        assert_eq!(
            (
                doc["library_checked"].as_bool(),
                doc["next"][0]["tool"].as_str()
            ),
            (Some(true), Some("get_batch"))
        );
        let batch_id = doc["batch_id"].as_str().unwrap();
        assert_eq!(
            doc["next"][0]["args"],
            json!({"batch_id": batch_id, "wait_seconds": 180, "wait_for": "needs_action"})
        );
        assert_eq!(doc["results"][1]["fuzzy_matches"], json!([]));
        // One batch, labelled with the one item that needed a job, holding that job.
        let label: Option<String> = sqlx::query_scalar("SELECT label FROM batches WHERE id = ?")
            .bind(batch_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(label.as_deref(), Some("Marrow Choir - O Holy Night"));
        assert_eq!(count(&db, "jobs").await, 1);
    }

    #[tokio::test]
    async fn a_fuzzy_match_is_listed_with_the_job_and_not_accepted() {
        let (db, acq, _, token) = app_with_acquire().await;
        let navi = NavidromeStub::start();
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+Silent+Night",
            200,
            &format!(
                "[{}]",
                native_song(
                    "7kR2mWq9TzX4bNc1vLpQaE",
                    "Marrow Choir",
                    "Silent Night (Live)",
                    240.0,
                    None
                )
            ),
        );
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let doc = document(
            &call_tool(
                app,
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "Silent Night"}]}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["results"][0]["status"].as_str(),
                &doc["results"][0]["fuzzy_matches"]
            ),
            (Some("job_created"), &json!(["7kR2mWq9TzX4bNc1vLpQaE"])),
            "{doc}"
        );
    }

    #[tokio::test]
    async fn a_user_without_navidrome_gets_a_job_for_every_item() {
        // for_test's NavidromeFor answers NotLinked.
        let (db, acq, _, token) = app_with_acquire().await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "Silent Night"},
                    {"kind": "album", "artist": "Kowloon", "title": "Come Over",
                        "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f"}],
                    "label": "Winter list"}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["library_checked"].as_bool(),
                doc["results"][0]["status"].as_str(),
                doc["results"][1]["status"].as_str()
            ),
            (Some(false), Some("job_created"), Some("job_created")),
            "{doc}"
        );
        let kinds: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, import_mode FROM jobs ORDER BY created_at, kind DESC")
                .fetch_all(&db)
                .await
                .unwrap();
        assert!(
            kinds.contains(&("album".into(), "album".into())),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("track".into(), "singleton".into())),
            "{kinds:?}"
        );
        let label: Option<String> = sqlx::query_scalar("SELECT label FROM batches")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(label.as_deref(), Some("Winter list"));
    }

    #[tokio::test]
    async fn a_recording_already_wanted_names_the_running_job() {
        let (db, acq, _, token) = app_with_acquire().await;
        let item = json!({"artist": "Marrow Choir", "title": "O Holy Night"});
        let first = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [item]}),
            )
            .await,
        );
        let job = first["results"][0]["job_id"].clone();
        let again = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [item]}),
            )
            .await,
        );
        assert_eq!(
            (again["ok"].as_bool(), again["error"]["code"].as_str()),
            (Some(false), Some("partial")),
            "{again}"
        );
        let refused = &again["results"][0]["error"];
        assert_eq!(
            (refused["code"].as_str(), &refused["facts"]["job_id"]),
            (Some("already_running"), &job)
        );
        assert_eq!(
            (
                refused["facts"]["state"].as_str(),
                refused["facts"]["title"].as_str(),
                &refused["facts"]["album"]
            ),
            (Some("searching"), Some("O Holy Night"), &Value::Null)
        );
        assert_eq!(
            refused["next"][0],
            json!({"tool": "get_jobs", "args": {"job_ids": [job], "detail": "full"}})
        );
        assert_eq!(again["batch_id"], Value::Null);
        assert_eq!(again["next"], json!([]));
        assert_eq!(count(&db, "batches").await, 1);
    }

    #[tokio::test]
    async fn a_lookup_that_fails_refuses_that_item_only() {
        let (db, acq, _, token) = app_with_acquire().await;
        let navi = NavidromeStub::start();
        // nginx's 502 page, as step 8's one_failed_lookup_makes_the_result_partial sends it.
        navi.answer_when("/api/song", "title=Marrow+Choir+Silent+Night", 502,
            "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n");
        navi.answer_when("/api/song", "title=Marrow+Choir+O+Holy+Night", 200, "[]");
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let doc = document(
            &call_tool(
                app,
                &token,
                "ensure_in_library",
                json!({"items": [
                {"artist": "Marrow Choir", "title": "Silent Night"},
                {"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["error"]["code"].as_str(),
                doc["results"][0]["error"]["code"].as_str()
            ),
            (Some("partial"), Some("navidrome_unreachable")),
            "{doc}"
        );
        assert_eq!(
            doc["results"][0]["error"]["next"],
            json!([{"tool": "ensure_in_library",
            "args": {"items": [{"artist": "Marrow Choir", "title": "Silent Night"}]}}])
        );
        assert_eq!(doc["results"][1]["status"], "job_created");
        assert_eq!(doc["next"][0]["tool"], "get_batch");
    }

    #[tokio::test]
    async fn no_default_folder_lists_the_folders_to_choose_from() {
        let (db, acq, _, token) = app_with_acquire().await;
        add_folder(&db, "u1", "f2", "Christmas").await;
        set_default_folder(&db, "u1", None).await; // two folders, no default
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}],
                    "request_key": "k1"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["retry"].as_str()),
            (Some("folder_required"), Some("needs_decision")),
            "{error}"
        );
        assert_eq!(
            error["facts"]["folders"],
            json!([{"folder_id": "f1", "name": "Music"}, {"folder_id": "f2", "name": "Christmas"}])
        );
        assert_eq!(
            error["next"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["args"]["folder_id"].clone())
                .collect::<Vec<_>>(),
            vec![json!("f1"), json!("f2")]
        );
        assert_eq!(
            error["next"][0]["args"],
            json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}], "folder_id": "f1"})
        );
    }

    #[tokio::test]
    async fn a_user_without_a_folder_is_told_to_add_one() {
        let (db, acq, _, token) = app_with_acquire().await;
        sqlx::query("DELETE FROM folders")
            .execute(&db)
            .await
            .unwrap();
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["retry"].as_str(),
                error["message"].as_str()
            ),
            (
                Some("folder_required"),
                Some("needs_user"),
                Some("This user has no library folder")
            )
        );
        assert_eq!(
            error["user_action"],
            json!({"code": "add_folder", "what": "Add a library folder", "where": "Settings > Library"})
        );
        assert_eq!(error["next"], json!([]));
    }

    #[tokio::test]
    async fn another_users_folder_reads_as_unknown() {
        let (db, acq, _, token) = app_with_acquire().await;
        crate::connection::test_util::add_user(&db, "u2", "bob").await;
        add_folder(&db, "u2", "b1", "Bob's").await;
        for folder_id in ["b1", "nope"] {
            let error = document(
                &call_tool(
                    app_of(&db, &acq),
                    &token,
                    "ensure_in_library",
                    json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}],
                        "folder_id": folder_id}),
                )
                .await,
            )["error"]
                .clone();
            assert_eq!(
                (error["code"].as_str(), error["message"].as_str()),
                (Some("unknown_handle"), Some("Folder not found")),
                "{folder_id}"
            );
        }
        assert_eq!(count(&db, "jobs").await, 0);
    }

    #[tokio::test]
    async fn failover_off_takes_no_attempt_count() {
        let (db, acq, _, token) = app_with_acquire().await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"failover": "off", "max_attempts": 3,
                "items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["field"].as_str()),
            (Some("invalid_input"), Some("max_attempts"))
        );
        assert_eq!(
            error["message"],
            "ensure_in_library: max_attempts must be absent when failover is off"
        );
    }

    #[tokio::test]
    async fn the_options_read_are_the_jobs_options() {
        let (db, acq, _, token) = app_with_acquire().await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"failover": "off", "pick": "review", "min_score": 0.5,
                    "lossless_only": true,
                    "items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        );
        let job = doc["results"][0]["job_id"].as_str().unwrap();
        let options: String = sqlx::query_scalar("SELECT options FROM jobs WHERE id = ?")
            .bind(job)
            .fetch_one(&db)
            .await
            .unwrap();
        let options: Value = serde_json::from_str(&options).unwrap();
        assert_eq!(
            (
                &options["pick"],
                &options["min_score"],
                &options["max_sources"],
                &options["filters"]["lossless_only"]
            ),
            (&json!("review"), &json!(0.5), &json!(1), &json!(true))
        );
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"min_score": 1.5,
                    "items": [{"artist": "Marrow Choir", "title": "Silent Night"}]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["facts"]["field"].as_str(),
                error["facts"]["expected"].as_str()
            ),
            (Some("min_score"), Some("a number from 0 to 1"))
        );
    }

    #[tokio::test]
    async fn a_user_with_the_most_unfinished_jobs_is_told_to_follow_them() {
        let (db, acq, _, token) = jobs_app().await;
        batch_of_searches(&acq, MCP_UNFINISHED_JOBS_PER_USER as usize).await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}],
                    "folder_id": "alice-folder"}),
            )
            .await,
        );
        let refused = &doc["results"][0]["error"];
        assert_eq!(
            (
                refused["code"].as_str(),
                refused["stage"].as_str(),
                refused["retry"].as_str()
            ),
            (
                Some("rate_limited"),
                Some("input"),
                Some("same_source_later")
            ),
            "{doc}"
        );
        assert_eq!(
            refused["message"],
            "This user has 200 unfinished downloads from connected apps, the most allowed; follow them with get_batch or list_batches before asking for more"
        );
        assert_eq!(
            (
                &refused["facts"]["unfinished_jobs"],
                &refused["facts"]["limit"],
                &refused["facts"]["kind"]
            ),
            (&json!(200), &json!(200), &json!("track"))
        );
        assert_eq!(
            refused["next"],
            json!([{"tool": "list_batches", "args": {}}])
        );
        assert_eq!(doc["batch_id"], Value::Null);
    }

    #[tokio::test]
    async fn a_broken_importer_refuses_before_any_job_is_made() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.importer().set_health(broken());
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["stage"].as_str(),
                error["message"].as_str()
            ),
            (
                Some("importer_broken"),
                Some("import"),
                Some("No metadata source plugin loaded")
            )
        );
        assert_eq!(
            (count(&db, "batches").await, count(&db, "jobs").await),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn replace_existing_is_not_an_argument_yet() {
        let (db, acq, _, token) = app_with_acquire().await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "ensure_in_library",
                json!({"replace_existing": true,
                    "items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["field"].as_str()),
            (Some("invalid_input"), Some("replace_existing"))
        );
        assert_eq!(count(&db, "jobs").await, 0);
    }

    #[tokio::test]
    async fn a_repeated_call_with_its_request_key_makes_one_batch() {
        let (db, acq, _, token) = app_with_acquire().await;
        let call = json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}],
            "request_key": "k1"});
        let first = call_tool(app_of(&db, &acq), &token, "ensure_in_library", call.clone()).await;
        let again = call_tool(app_of(&db, &acq), &token, "ensure_in_library", call).await;
        assert_eq!(first, again);
        assert_eq!(
            (count(&db, "batches").await, count(&db, "jobs").await),
            (1, 1)
        );
    }
}
