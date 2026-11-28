//! MCP tools for 1-Click Existing Library Import.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::download::{AsIsTagOverrides, ImportChoice};
use shared::failure::{cut_json, Stage, TEXT_MAX_CHARS};
use shared::library_import::{
    LibraryImportBatch, LibraryImportBatchDetail, LibraryImportItem, LibraryImportItemStatus,
    LibraryImportMode, LibraryImportOrigin, LibraryImportResolveAction, LibraryScanPreview,
};

use super::args::Args;
use super::contract::{
    answer, failed, invalid_value, success, upstream_error, NextCall, ToolError,
};
use super::deletion::deletion_off_at;
use super::keys::{keyed, request_key, Keyed};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::library_import::resolve::resolve_item_in;
use crate::core::library_import::runner::{
    cancel_batch_in, get_batch_detail_in, pause_batch_in, resume_batch_in, scan_preview_in,
    start_batch_in,
};
use crate::models::library_import::{get_batch_in, get_item_for_user_in};

const SCAN_ARGS: &[&str] = &["source_path"];
const START_ARGS: &[&str] = &["source_path", "mode", "request_key"];
const GET_ARGS: &[&str] = &["batch_id", "status", "limit", "offset"];
const RESOLVE_ARGS: &[&str] = &[
    "item_id",
    "action",
    "data_source",
    "id",
    "musicbrainz",
    "tag_overrides",
];
const CONTROL_ARGS: &[&str] = &["batch_id", "action"];

const TAG_OVERRIDES_FIELDS: &[&str] = &["artist", "title", "album"];

const MODES: [&str; 3] = ["tag_and_organize", "tag_in_place", "register_as_is"];
const ITEM_STATUSES: [&str; 7] = [
    "pending",
    "importing",
    "imported",
    "needs_decision",
    "skipped_duplicate",
    "failed",
    "dismissed",
];
const RESOLVE_ACTIONS: [&str; 5] = ["auto", "as_is", "candidate", "musicbrainz", "dismiss"];
const CONTROL_ACTIONS: [&str; 3] = ["pause", "resume", "cancel"];

pub(crate) const SCAN_UNIMPORTED_FILES: ToolSpec = ToolSpec {
    name: "scan_unimported_files",
    title: "Scan unimported library files",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Scans the user's personal library folder (or a validated external source_path) against the folder's beets database and returns a read-only preview of unimported album folders and loose audio files, excluding hidden dot-directories, Discovery, and already-indexed files. Failure codes: {common}, folder_required, permission_not_granted, import_beets_failed.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"source_path": {"type": "string", "minLength": 1}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(scan_unimported_files(state, caller, args)),
};

pub(crate) const START_LIBRARY_IMPORT: ToolSpec = ToolSpec {
    name: "start_library_import",
    title: "Start existing library import",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Starts a persistent background batch to import unimported audio files into the user's personal library. mode must be one of tag_and_organize (autotag via MusicBrainz and organize into Artist/Album/Track; copies from an external source folder or moves in place), tag_in_place (autotag in place without moving or copying), or register_as_is (fast database registration without modifying files or tags). Modes tag_and_organize and tag_in_place require 'Allow managing library files' enabled for this app; register_as_is does not. Never deletes source files on duplicate skip, failure, cancel, or dismiss. Supports request_key for idempotency. Failure codes: {common}, folder_required, permission_not_granted, already_running, import_beets_failed.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["mode"], "properties": {"source_path": {"type": "string", "minLength": 1}, "mode": {"type": "string", "enum": ["tag_and_organize", "tag_in_place", "register_as_is"]}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: true,
    call: |state, caller, args| Box::pin(start_library_import(state, caller, args)),
};

pub(crate) const GET_LIBRARY_IMPORT: ToolSpec = ToolSpec {
    name: "get_library_import",
    title: "Get library import batch",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows a library import batch (the caller's latest batch when batch_id is omitted) with its aggregate counts and a paginated, optionally status-filtered list of items (including match choices for needs_decision items). limit defaults to 25 (up to 100), offset defaults to 0. Failure codes: {common}, unknown_handle.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"batch_id": {"type": "string", "minLength": 1}, "status": {"type": "string", "enum": ["pending", "importing", "imported", "needs_decision", "skipped_duplicate", "failed", "dismissed"]}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}, "offset": {"type": "integer", "minimum": 0, "maximum": 100000}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_library_import(state, caller, args)),
};

pub(crate) const RESOLVE_LIBRARY_IMPORT_ITEM: ToolSpec = ToolSpec {
    name: "resolve_library_import_item",
    title: "Resolve library import item",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Resolves a library import item in needs_decision or failed state with action: auto (retry automatic beets matching), as_is (import with existing tags; optional tag_overrides with artist, title, and optional album stamps tags onto single-track files after copy-on-write hardlink isolation), candidate (pick one of beets' candidates by data_source and id), musicbrainz (pick a MusicBrainz release or recording ID/URL), or dismiss (mark dismissed without deleting any file). Resolving items in tag_and_organize or tag_in_place batches, or passing tag_overrides, requires 'Allow managing library files' enabled for this app; dismiss and register_as_is do not. Failure codes: {common}, permission_not_granted, unknown_handle, wrong_job_state, import_choice_invalid, import_files_gone, import_beets_failed.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["item_id", "action"], "properties": {"item_id": {"type": "string", "minLength": 1}, "action": {"type": "string", "enum": ["auto", "as_is", "candidate", "musicbrainz", "dismiss"]}, "data_source": {"type": "string", "minLength": 1}, "id": {"type": "string", "minLength": 1}, "musicbrainz": {"type": "string", "minLength": 1}, "tag_overrides": {"type": "object", "additionalProperties": false, "required": ["artist", "title"], "properties": {"artist": {"type": "string", "minLength": 1}, "title": {"type": "string", "minLength": 1}, "album": {"type": "string"}}}}}"#,
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(resolve_library_import_item(state, caller, args)),
};

pub(crate) const CONTROL_LIBRARY_IMPORT: ToolSpec = ToolSpec {
    name: "control_library_import",
    title: "Control library import batch",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Pauses, resumes, or cancels a library import batch (action: pause, resume, or cancel). Resuming a tag_and_organize or tag_in_place batch requires 'Allow managing library files' enabled for this app; pause, cancel, and resuming register_as_is do not. Never deletes source files. Failure codes: {common}, permission_not_granted, unknown_handle, wrong_job_state, already_running.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["batch_id", "action"], "properties": {"batch_id": {"type": "string", "minLength": 1}, "action": {"type": "string", "enum": ["pause", "resume", "cancel"]}}}"#,
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(control_library_import(state, caller, args)),
};

fn preview_fields(preview: LibraryScanPreview) -> Map<String, Value> {
    let sample_units: Vec<Value> = preview
        .sample_units
        .into_iter()
        .map(|u| {
            json!({
                "source_path": cut_json(u.source_path, TEXT_MAX_CHARS),
                "display_name": cut_json(u.display_name, TEXT_MAX_CHARS),
                "is_album_dir": u.is_album_dir,
                "file_count": u.file_count,
            })
        })
        .collect();
    let Value::Object(fields) = json!({
        "source_path": cut_json(preview.source_path, TEXT_MAX_CHARS),
        "target_path": cut_json(preview.target_path, TEXT_MAX_CHARS),
        "in_place": preview.in_place,
        "total_units": preview.total_units,
        "album_units": preview.album_units,
        "singleton_units": preview.singleton_units,
        "total_files": preview.total_files,
        "already_indexed_files": preview.already_indexed_files,
        "sample_units": sample_units,
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

fn batch_value(batch: &LibraryImportBatch) -> Value {
    json!({
        "id": batch.id,
        "user_id": batch.user_id,
        "target_folder_id": batch.target_folder_id,
        "target_path": cut_json(batch.target_path.clone(), TEXT_MAX_CHARS),
        "source_path": cut_json(batch.source_path.clone(), TEXT_MAX_CHARS),
        "mode": batch.mode.as_str(),
        "status": batch.status.as_str(),
        "total_units": batch.total_units,
        "total_files": batch.total_files,
        "completed_units": batch.completed_units,
        "imported_units": batch.imported_units,
        "needs_decision_units": batch.needs_decision_units,
        "skipped_units": batch.skipped_units,
        "failed_units": batch.failed_units,
        "already_indexed_files": batch.already_indexed_files,
        "origin": batch.origin.as_str(),
        "error": batch.error.as_deref().map(|e| cut_json(e.to_string(), TEXT_MAX_CHARS)),
        "created_at": batch.created_at,
        "updated_at": batch.updated_at,
        "finished_at": batch.finished_at,
    })
}

fn item_value(item: &LibraryImportItem) -> Value {
    json!({
        "id": item.id,
        "batch_id": item.batch_id,
        "seq": item.seq,
        "source_path": cut_json(item.source_path.clone(), TEXT_MAX_CHARS),
        "display_name": cut_json(item.display_name.clone(), TEXT_MAX_CHARS),
        "is_album_dir": item.is_album_dir,
        "file_count": item.file_count,
        "status": item.status.as_str(),
        "skip_kind": item.skip_kind,
        "failure": item.failure,
        "choices": item.choices,
        "updated_at": item.updated_at,
    })
}

fn detail_fields(detail: &LibraryImportBatchDetail) -> Map<String, Value> {
    let items: Vec<Value> = detail.items.iter().map(item_value).collect();
    let Value::Object(fields) = json!({
        "batch": batch_value(&detail.batch),
        "items": items,
        "total_matching_items": detail.total_matching_items,
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

async fn scan_unimported_files(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SCAN_UNIMPORTED_FILES.name, args, SCAN_ARGS);
    answer(
        async {
            let source_path = args.opt_string("source_path")?;
            args.finish()?;
            let preview = scan_preview_in(
                &state.db,
                &state.acquire.queue,
                &caller.ctx,
                source_path.as_deref(),
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(preview_fields(preview))
        }
        .await,
    )
}

async fn start_library_import(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(START_LIBRARY_IMPORT.name, args, START_ARGS);
    let read = || {
        let source_path = args.opt_string("source_path")?;
        let mode_str = args
            .opt_choice("mode", &MODES)?
            .ok_or_else(|| invalid_value(START_LIBRARY_IMPORT.name, "mode", &format!("one of {}", MODES.join(", "))))?;
        let mode = LibraryImportMode::from_str(mode_str).expect("validated mode");
        let key = request_key(&args)?;
        args.finish()?;
        Ok::<_, ToolError>((source_path, mode, key))
    };
    let (source_path, mode, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };

    // Permission gate: tag_and_organize and tag_in_place modify files/tags and require allow_delete.
    if mode != LibraryImportMode::RegisterAsIs && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Import));
    }

    let call = json!({
        "source_path": source_path,
        "mode": mode.as_str(),
    });
    let coordinator = state.library_import.clone();
    let db = state.db.clone();
    let queue = state.acquire.queue.clone();
    let navidrome = state.navidrome.clone();
    let ctx = caller.ctx.clone();
    let same_call = args.same_call();

    let work = async move {
        match start_batch_in(
            coordinator,
            &db,
            queue,
            navidrome,
            &ctx,
            source_path.as_deref(),
            mode,
            LibraryImportOrigin::Mcp,
        )
        .await
        {
            Ok(batch) => {
                let mut fields = Map::new();
                fields.insert("batch".into(), batch_value(&batch));
                fields.insert(
                    "next".into(),
                    json!([NextCall::new(
                        GET_LIBRARY_IMPORT.name,
                        json!({"batch_id": batch.id})
                    )]),
                );
                Keyed::Kept(success(fields))
            }
            Err(f) => Keyed::Released(failed(upstream_error(f, Map::new(), same_call))),
        }
    };

    keyed(&state.acquire, caller, START_LIBRARY_IMPORT.name, key, &call, work).await
}

async fn get_library_import(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_LIBRARY_IMPORT.name, args, GET_ARGS);
    answer(
        async {
            let batch_id = args.opt_string("batch_id")?;
            let status = args
                .opt_choice("status", &ITEM_STATUSES)?
                .and_then(LibraryImportItemStatus::from_str);
            let limit = args.opt_u32("limit", 1, 100)?.unwrap_or(25);
            let offset = args.opt_u32("offset", 0, 100_000)?.unwrap_or(0);
            args.finish()?;

            let detail = get_batch_detail_in(
                &state.db,
                &caller.ctx,
                batch_id.as_deref(),
                status,
                limit,
                offset,
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(detail_fields(&detail))
        }
        .await,
    )
}

async fn resolve_library_import_item(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(RESOLVE_LIBRARY_IMPORT_ITEM.name, args, RESOLVE_ARGS);
    let read = || {
        let item_id = args.string("item_id")?;
        let action_str = args.opt_choice("action", &RESOLVE_ACTIONS)?.ok_or_else(|| {
            invalid_value(
                RESOLVE_LIBRARY_IMPORT_ITEM.name,
                "action",
                &format!("one of {}", RESOLVE_ACTIONS.join(", ")),
            )
        })?;
        let (action, has_tag_overrides) = match action_str {
            "auto" => (LibraryImportResolveAction::Choose(ImportChoice::Auto), false),
            "as_is" => match args.opt_object("tag_overrides", TAG_OVERRIDES_FIELDS)? {
                Some(obj) => {
                    let artist = obj.string("artist")?;
                    let title = obj.string("title")?;
                    let album = obj.opt_string("album")?;
                    (
                        LibraryImportResolveAction::Choose(ImportChoice::AsIsWithTags(
                            AsIsTagOverrides {
                                artist,
                                title,
                                album,
                            },
                        )),
                        true,
                    )
                }
                None => (LibraryImportResolveAction::Choose(ImportChoice::AsIs), false),
            },
            "candidate" => (
                LibraryImportResolveAction::Choose(ImportChoice::Candidate {
                    data_source: args.string("data_source")?,
                    id: args.string("id")?,
                }),
                false,
            ),
            "musicbrainz" => (
                LibraryImportResolveAction::Choose(ImportChoice::MusicBrainz(
                    args.string("musicbrainz")?,
                )),
                false,
            ),
            _ => (LibraryImportResolveAction::Dismiss, false),
        };
        if action_str != "candidate" {
            args.absent("data_source", "absent unless action is candidate")?;
            args.absent("id", "absent unless action is candidate")?;
        }
        if action_str != "musicbrainz" {
            args.absent("musicbrainz", "absent unless action is musicbrainz")?;
        }
        if action_str != "as_is" {
            args.absent("tag_overrides", "absent unless action is as_is")?;
        }
        args.finish()?;
        Ok::<_, ToolError>((item_id, action, has_tag_overrides))
    };

    let (item_id, action, has_tag_overrides) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };

    // Permission gate: tag_overrides always mutates audio files; non-dismiss actions on
    // tag_and_organize or tag_in_place batches mutate files/tags and require allow_delete.
    if has_tag_overrides && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Import));
    }

    if !matches!(action, LibraryImportResolveAction::Dismiss) && !caller.allow_delete {
        match get_item_for_user_in(
            &state.db,
            &item_id,
            &caller.ctx.user_id,
            caller.ctx.role.is_admin(),
        )
        .await
        {
            Ok(Some((batch, _))) if batch.mode != LibraryImportMode::RegisterAsIs => {
                return failed(deletion_off_at(Stage::Import));
            }
            Ok(_) => {}
            Err(e) => {
                return failed(upstream_error(
                    database_failure(Stage::Import, e),
                    Map::new(),
                    args.same_call(),
                ));
            }
        }
    }

    match resolve_item_in(
        state.library_import.clone(),
        &state.db,
        &state.acquire.queue,
        state.navidrome.clone(),
        &caller.ctx,
        &item_id,
        action,
    )
    .await
    {
        Ok(detail) => success(detail_fields(&detail)),
        Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
    }
}

async fn control_library_import(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(CONTROL_LIBRARY_IMPORT.name, args, CONTROL_ARGS);
    answer(
        async {
            let batch_id = args.string("batch_id")?;
            let action = args
                .opt_choice("action", &CONTROL_ACTIONS)?
                .ok_or_else(|| {
                    invalid_value(
                        CONTROL_LIBRARY_IMPORT.name,
                        "action",
                        &format!("one of {}", CONTROL_ACTIONS.join(", ")),
                    )
                })?;
            args.finish()?;

            // Permission gate: resuming tag_and_organize or tag_in_place batches modifies
            // files/tags and requires allow_delete.
            if action == "resume" && !caller.allow_delete {
                let owned_batch = get_batch_in(&state.db, &batch_id)
                    .await
                    .map_err(|e| {
                        upstream_error(database_failure(Stage::Import, e), Map::new(), args.same_call())
                    })?
                    .filter(|b| caller.ctx.role.is_admin() || b.user_id == caller.ctx.user_id);
                if let Some(batch) = owned_batch {
                    if batch.mode != LibraryImportMode::RegisterAsIs {
                        return Err(deletion_off_at(Stage::Import));
                    }
                }
            }

            let batch = match action {
                "pause" => {
                    pause_batch_in(state.library_import.clone(), &state.db, &caller.ctx, &batch_id)
                        .await
                }
                "resume" => {
                    resume_batch_in(
                        state.library_import.clone(),
                        &state.db,
                        state.acquire.queue.clone(),
                        state.navidrome.clone(),
                        &caller.ctx,
                        &batch_id,
                    )
                    .await
                }
                _ => {
                    cancel_batch_in(
                        state.library_import.clone(),
                        &state.db,
                        &caller.ctx,
                        &batch_id,
                    )
                    .await
                }
            }
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;

            let mut fields = Map::new();
            fields.insert("batch".into(), batch_value(&batch));
            Ok(fields)
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::time::Duration;

    use crate::connection::test_util::{
        access_token, add_user, call_tool, document, RESOURCE,
    };
    use crate::connection::{router, ConnectionState};
    use crate::core::jobs::runner::Acquire;
    use crate::db::test_db;
    use crate::import_queue::test_support::{candidate, no_match, track_choices};
    use soulbeet::ImportResult;

    async fn setup_app(
        allow_delete: bool,
    ) -> (
        tempfile::TempDir,
        sqlx::SqlitePool,
        std::sync::Arc<Acquire>,
        std::sync::Arc<crate::core::jobs::runner::TestEffects>,
        axum::Router,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let music = root.join("music/alice");
        std::fs::create_dir_all(music.join("Artist/Album")).unwrap();
        std::fs::write(music.join("Artist/Album/01.flac"), b"audio1").unwrap();
        std::fs::write(music.join("loose.flac"), b"audio2").unwrap();

        let db = test_db::migrated().await;
        add_user(&db, "alice", "alice").await;
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f-alice', 'alice', 'Music', ?)")
            .bind(music.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();
        let (acq, effects) = Acquire::for_test(db.clone());
        let state = ConnectionState::on(db.clone(), acq.clone());
        let token = access_token(&db, "alice", RESOURCE).await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = ? WHERE user_id = 'alice'")
            .bind(allow_delete)
            .execute(&db)
            .await
            .unwrap();
        (dir, db, acq, effects, router(state), token)
    }

    #[tokio::test]
    async fn start_library_import_enforces_allow_delete_and_request_key_idempotency() {
        let (_dir, _db, _acq, _effects, app, token) = setup_app(false).await;

        // 1. Scan works without allow_delete
        let scan = document(&call_tool(app.clone(), &token, "scan_unimported_files", json!({})).await);
        assert_eq!(scan["ok"], true, "{scan}");
        assert_eq!(scan["total_units"], 2);
        assert_eq!(scan["album_units"], 1);
        assert_eq!(scan["singleton_units"], 1);

        // 2. tag_and_organize and tag_in_place require allow_delete
        for mode in ["tag_and_organize", "tag_in_place"] {
            let err = document(
                &call_tool(app.clone(), &token, "start_library_import", json!({"mode": mode})).await,
            );
            assert_eq!(
                err["error"]["code"], "permission_not_granted",
                "{mode}: {err}"
            );
        }

        // 3. register_as_is succeeds without allow_delete and honors request_key idempotency
        let first = document(
            &call_tool(
                app.clone(),
                &token,
                "start_library_import",
                json!({"mode": "register_as_is", "request_key": "lib-imp-1"}),
            )
            .await,
        );
        assert_eq!(first["ok"], true, "{first}");
        let batch_id = first["batch"]["id"].as_str().unwrap().to_string();

        // Wait for background worker to finish
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let got = document(
                    &call_tool(
                        app.clone(),
                        &token,
                        "get_library_import",
                        json!({"batch_id": batch_id}),
                    )
                    .await,
                );
                if got["batch"]["status"] == "completed" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("register_as_is batch did not complete");

        // Repeat with the same request_key returns the exact same first response (same batch id)
        let repeat = document(
            &call_tool(
                app,
                &token,
                "start_library_import",
                json!({"mode": "register_as_is", "request_key": "lib-imp-1"}),
            )
            .await,
        );
        assert_eq!(repeat, first);
    }

    #[tokio::test]
    async fn resolve_library_import_item_gates_allow_delete_correctly() {
        let (_dir, db, _acq, effects, app, token) = setup_app(true).await;

        // Script a NoMatch so an item lands in needs_decision
        effects.importer().script(ImportResult {
            choices: Some(track_choices(vec![candidate("MusicBrainz", "m1")])),
            ..no_match(1)
        });
        let started = document(
            &call_tool(
                app.clone(),
                &token,
                "start_library_import",
                json!({"mode": "tag_in_place"}),
            )
            .await,
        );
        assert_eq!(started["ok"], true, "{started}");
        let batch_id = started["batch"]["id"].as_str().unwrap().to_string();

        // Wait for completion
        let detail = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let got = document(
                    &call_tool(
                        app.clone(),
                        &token,
                        "get_library_import",
                        json!({"batch_id": batch_id, "status": "needs_decision"}),
                    )
                    .await,
                );
                if got["batch"]["status"] == "completed" {
                    break got;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("tag_in_place batch did not complete");
        let item_id = detail["items"][0]["id"].as_str().unwrap().to_string();

        // Revoke allow_delete: choosing as_is or tag_overrides is refused, dismiss is allowed
        sqlx::query("UPDATE oauth_grants SET allow_delete = 0 WHERE user_id = 'alice'")
            .execute(&db)
            .await
            .unwrap();

        let refused_asis = document(
            &call_tool(
                app.clone(),
                &token,
                "resolve_library_import_item",
                json!({"item_id": item_id, "action": "as_is"}),
            )
            .await,
        );
        assert_eq!(refused_asis["error"]["code"], "permission_not_granted");

        let refused_tags = document(
            &call_tool(
                app.clone(),
                &token,
                "resolve_library_import_item",
                json!({
                    "item_id": item_id,
                    "action": "as_is",
                    "tag_overrides": {"artist": "A", "title": "B"}
                }),
            )
            .await,
        );
        assert_eq!(refused_tags["error"]["code"], "permission_not_granted");

        let dismissed = document(
            &call_tool(
                app,
                &token,
                "resolve_library_import_item",
                json!({"item_id": item_id, "action": "dismiss"}),
            )
            .await,
        );
        assert_eq!(dismissed["ok"], true, "{dismissed}");
        assert_eq!(dismissed["batch"]["needs_decision_units"], 0);
    }

    #[tokio::test]
    async fn control_library_import_gates_resume_on_allow_delete_for_mutating_modes() {
        use crate::models::library_import::{
            create_batch_with_items_in, NewLibraryImportBatch, NewLibraryImportItem,
        };
        use shared::library_import::{
            LibraryImportBatchStatus, LibraryImportMode, LibraryImportOrigin,
        };

        let (dir, db, _acq, _effects, app, token) = setup_app(false).await;
        let music = std::fs::canonicalize(dir.path()).unwrap().join("music/alice");
        let track = music.join("loose.flac");

        // Seed a paused tag_in_place batch and a paused register_as_is batch.
        for (bid, iid, mode) in [
            ("b-tag", "i-tag", LibraryImportMode::TagInPlace),
            ("b-reg", "i-reg", LibraryImportMode::RegisterAsIs),
        ] {
            create_batch_with_items_in(
                &db,
                &NewLibraryImportBatch {
                    id: bid.into(),
                    user_id: "alice".into(),
                    target_folder_id: "f-alice".into(),
                    target_path: music.to_string_lossy().into_owned(),
                    source_path: music.to_string_lossy().into_owned(),
                    mode,
                    status: LibraryImportBatchStatus::Paused,
                    total_units: 1,
                    total_files: 1,
                    already_indexed_files: 0,
                    origin: LibraryImportOrigin::Mcp,
                },
                &[NewLibraryImportItem {
                    id: iid.into(),
                    seq: 0,
                    source_path: track.to_string_lossy().into_owned(),
                    display_name: "loose.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                }],
            )
            .await
            .unwrap();
        }

        // Resuming tag_in_place without allow_delete MUST be refused.
        let refused = document(
            &call_tool(
                app.clone(),
                &token,
                "control_library_import",
                json!({"batch_id": "b-tag", "action": "resume"}),
            )
            .await,
        );
        assert_eq!(refused["error"]["code"], "permission_not_granted", "{refused}");

        // Cancelling tag_in_place without allow_delete IS allowed.
        let cancelled = document(
            &call_tool(
                app.clone(),
                &token,
                "control_library_import",
                json!({"batch_id": "b-tag", "action": "cancel"}),
            )
            .await,
        );
        assert_eq!(cancelled["ok"], true, "{cancelled}");
        assert_eq!(cancelled["batch"]["status"], "cancelled");

        // Resuming register_as_is without allow_delete IS allowed.
        let resumed = document(
            &call_tool(
                app,
                &token,
                "control_library_import",
                json!({"batch_id": "b-reg", "action": "resume"}),
            )
            .await,
        );
        assert_eq!(resumed["ok"], true, "{resumed}");
        assert_eq!(resumed["batch"]["status"], "running");
    }
}
