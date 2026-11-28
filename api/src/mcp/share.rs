//! Track sharing and legacy folder migration MCP tools.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{
    answer, failed, items, success, upstream_error, ItemResult, Items, NextCall, ToolError,
};
use super::deletion::deletion_off_at;
use super::keys::{keyed, request_key, Keyed};
use super::paging::{fit, Page};
use super::refs::{page_schema, request_key_schema};
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::sync::cut;
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::delete_guard::Guard;
use crate::core::folders;
use crate::core::time;
use crate::core::tracks::migrate_legacy::{
    list_legacy_groups_in, migrate_legacy_folder, MigrateLegacyReport, MigrateLegacyRequest,
};
use crate::core::tracks::share::{
    list_recipients_in, resolve_recipient_ids_in, share_tracks as core_share_tracks,
    ShareItemStatus, ShareResultEntry, ShareTracksReport, ShareTracksRequest,
};
use crate::core::tracks::{TrackSelection, ALBUM_MAX_TRACKS, SONGS_PER_CALL};
use crate::models::track_share::{ShareDirection, ShareHistoryEntry, ShareOrigin, TrackShareRow};

const SHARE_ARGS: &[&str] = &[
    "song_ids",
    "album_id",
    "recipient_usernames",
    "recipient_user_ids",
    "dry_run",
    "request_key",
];

const SHARE_HISTORY_ARGS: &[&str] = &["direction", "limit", "cursor"];

const MIGRATE_ARGS: &[&str] = &[
    "legacy_path",
    "remove_source_after",
    "dry_run",
    "request_key",
];

pub(crate) const LIST_SHARE_RECIPIENTS: ToolSpec = ToolSpec {
    name: "list_share_recipients",
    title: "List share recipients",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Lists other Soulbeet users who accept incoming track shares and have a configured personal library folder, returning each recipient's user_id and username. Never exposes folder paths or admin metadata. Takes no arguments. Failure codes: {common}.")
    },
    input_schema: r#"{"type": "object", "properties": {}, "additionalProperties": false}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(list_share_recipients(state, caller, args)),
};

pub(crate) const SHARE_TRACKS: ToolSpec = ToolSpec {
    name: "share_tracks",
    title: "Share tracks with other users",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shares up to {SONGS_PER_CALL} songs (song_ids) or one whole album (album_id, up to {ALBUM_MAX_TRACKS} tracks) from this user's personal library into one or more recipients' personal libraries via filesystem hardlinks (falling back to reflink or copy across filesystems), registering files in each recipient's beets database and triggering a Navidrome scan. Sharing is additive and never deletes or modifies the sender's files, so it does not require the file-management switch. Specify recipients by recipient_usernames and/or recipient_user_ids (from list_share_recipients). Pass dry_run: true to preview predicted link modes, target paths and duplicates without writing anything. Pending discovery tracks must be promoted before sharing. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle, already_running, share_recipient_disabled, share_no_target_folder, share_self_disallowed, partial, and per item share_target_exists, share_already_in_library, share_failed, delete_path_unresolved, delete_outside_library, delete_not_a_track.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}, "album_id": {"type": "string", "minLength": 1}, "recipient_usernames": {"type": "array", "minItems": 1, "maxItems": 25, "items": {"type": "string", "minLength": 1}}, "recipient_user_ids": {"type": "array", "minItems": 1, "maxItems": 25, "items": {"type": "string", "minLength": 1}}, "dry_run": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(share_tracks(state, caller, args)),
};

pub(crate) const GET_SHARE_HISTORY: ToolSpec = ToolSpec {
    name: "get_share_history",
    title: "Track share history",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Lists this user's track sharing history, newest first: batch_id, sender and recipient usernames, source and target song ids and paths, title, artist, album, link_mode (hardlink, reflink, copy), origin (ui, mcp, migration) and shared_at. Filter by direction (all, incoming, outgoing; default all). Pages of 25 by default (limit up to 100); pass back cursor for the next page. Failure codes: {common}.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"direction": {"type": "string", "enum": ["all", "incoming", "outgoing"]}, "#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_share_history(state, caller, args)),
};

pub(crate) const MIGRATE_LEGACY_FOLDERS: ToolSpec = ToolSpec {
    name: "migrate_legacy_folders",
    title: "Migrate legacy or shared folders",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Admin-only tool to inspect or migrate retired legacy and shared folders recorded during the single-folder-per-user upgrade. Requires an Admin account with 'Admin library access (all users' folders)' enabled for this app. Call with no legacy_path (or dry_run: true) to list unmigrated legacy folder groups or preview the migration counts and predicted link modes. Pass legacy_path with dry_run: false to hardlink each audio file into each participating user's personal library folder, register in beets, trigger a Navidrome scan and best-effort restore ratings, stars and static playlists. Passing remove_source_after: true (with dry_run: false) also unlinks source files after verifying every target exists with matching byte size, and requires 'Allow managing library files' enabled for this app. Failure codes: {common}, {NAVIDROME_CODES}, permission_not_granted, unknown_handle, already_running, share_failed.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"legacy_path": {"type": "string", "minLength": 1}, "remove_source_after": {"type": "boolean"}, "dry_run": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(migrate_legacy_folders(state, caller, args)),
};

fn guard_of(state: &ConnectionState) -> Guard<'_> {
    Guard::new(
        &state.db,
        state.listeners.as_ref(),
        state.navidrome_music_path.as_deref(),
    )
}

fn read_selection(
    args: &Args,
    song_ids: Option<Vec<String>>,
    album_id: Option<String>,
) -> Result<TrackSelection, ToolError> {
    match (song_ids, album_id) {
        (Some(ids), None) => {
            if ids.is_empty() || (1..ids.len()).any(|i| ids[..i].contains(&ids[i])) {
                return Err(args.refuse(
                    "song_ids",
                    &format!("a list of 1 to {SONGS_PER_CALL} distinct song ids"),
                ));
            }
            Ok(TrackSelection::Songs(ids))
        }
        (None, Some(aid)) => Ok(TrackSelection::Album(aid)),
        (Some(_), Some(_)) => Err(args.refuse("song_ids", "either song_ids or album_id, not both")),
        (None, None) => Err(args.refuse("song_ids", "either song_ids or album_id")),
    }
}

async fn list_share_recipients(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(LIST_SHARE_RECIPIENTS.name, args, &[]);
    answer(
        async {
            args.finish()?;
            let recipients = list_recipients_in(&state.db, &caller.ctx)
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            let list: Vec<Value> = recipients
                .into_iter()
                .map(|r| {
                    json!({
                        "user_id": r.user_id,
                        "username": cut_json(r.username, TEXT_MAX_CHARS),
                    })
                })
                .collect();
            let mut fields = Map::new();
            fields.insert("recipients".into(), json!(list));
            Ok(fields)
        }
        .await,
    )
}

async fn share_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SHARE_TRACKS.name, args, SHARE_ARGS);
    let read = || {
        let song_ids = args.opt_strings("song_ids", SONGS_PER_CALL)?;
        let album_id = args.opt_string("album_id")?;
        let recipient_usernames = args.opt_strings("recipient_usernames", SONGS_PER_CALL)?;
        let recipient_user_ids = args.opt_strings("recipient_user_ids", SONGS_PER_CALL)?;
        let dry_run = args.opt_bool("dry_run")?.unwrap_or(false);
        let key = request_key(&args)?;
        args.finish()?;
        let selection = read_selection(&args, song_ids, album_id)?;
        let usernames = recipient_usernames.unwrap_or_default();
        let user_ids = recipient_user_ids.unwrap_or_default();
        if usernames.is_empty() && user_ids.is_empty() {
            return Err(args.refuse(
                "recipient_usernames",
                "at least one recipient username or recipient_user_ids entry",
            ));
        }
        Ok::<_, ToolError>((selection, user_ids, usernames, dry_run, key))
    };
    let (selection, raw_ids, raw_usernames, dry_run, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };

    let recipient_user_ids =
        match resolve_recipient_ids_in(&state.db, &raw_ids, &raw_usernames).await {
            Ok(ids) => ids,
            Err(f) => {
                let mut err = upstream_error(f, Map::new(), args.same_call());
                if err.failure.code == FailureCode::UnknownHandle {
                    err.next = vec![NextCall::new(LIST_SHARE_RECIPIENTS.name, json!({}))];
                }
                return failed(err);
            }
        };

    let is_admin = caller.ctx.role.is_admin();

    if dry_run {
        let guard = guard_of(state);
        return match core_share_tracks(
            &guard,
            state.navidrome.as_ref(),
            &state.acquire.queue,
            &caller.ctx,
            ShareTracksRequest {
                selection,
                recipient_user_ids,
                dry_run: true,
                origin: ShareOrigin::Mcp,
            },
        )
        .await
        {
            Ok(report) => share_answer(report, is_admin),
            Err(f) => failed(share_call_error(f, args.same_call())),
        };
    }

    let call = args.as_value();
    let (db, listeners, navidrome, music_path, queue, ctx) = (
        state.db.clone(),
        state.listeners.clone(),
        state.navidrome.clone(),
        state.navidrome_music_path.clone(),
        state.acquire.queue.clone(),
        caller.ctx.clone(),
    );
    let same_call = args.same_call();
    let work = async move {
        // Run the mutation phase in a detached task so a client disconnect mid-share never
        // aborts between filesystem linking, beets registration, Navidrome scan, and audit writes.
        let handle = tokio::spawn(async move {
            let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
            core_share_tracks(
                &guard,
                navidrome.as_ref(),
                &queue,
                &ctx,
                ShareTracksRequest {
                    selection,
                    recipient_user_ids,
                    dry_run: false,
                    origin: ShareOrigin::Mcp,
                },
            )
            .await
        });
        match handle.await {
            Ok(Ok(report)) => Keyed::Kept(share_answer(report, is_admin)),
            Ok(Err(f)) => Keyed::Released(failed(share_call_error(f, same_call))),
            Err(e) => {
                let f = Failure::new(
                    FailureCode::ShareFailed,
                    Stage::Share,
                    RetryAdvice::SameSourceLater,
                    format!("Share task stopped unexpectedly: {e}"),
                );
                Keyed::Released(failed(share_call_error(f, same_call)))
            }
        }
    };
    keyed(&state.acquire, caller, SHARE_TRACKS.name, key, &call, work).await
}

fn share_call_error(failure: Failure, same_call: Option<NextCall>) -> ToolError {
    let mut err = upstream_error(failure, Map::new(), same_call);
    if matches!(
        err.failure.code,
        FailureCode::UnknownHandle
            | FailureCode::ShareRecipientDisabled
            | FailureCode::ShareNoTargetFolder
    ) {
        err.next = vec![NextCall::new(LIST_SHARE_RECIPIENTS.name, json!({}))];
    }
    err
}

fn share_answer(report: ShareTracksReport, include_target_path: bool) -> CallToolResult {
    let results: Vec<ItemResult> = report
        .entries
        .into_iter()
        .map(|entry| share_entry_result(entry, include_target_path))
        .collect();

    let mut next = Vec::new();
    if !report.dry_run && report.shared_count > 0 {
        next.push(NextCall::new(GET_SHARE_HISTORY.name, json!({})));
    }

    let Value::Object(fields) = json!({
        "batch_id": report.batch_id,
        "dry_run": report.dry_run,
        "shared_count": report.shared_count,
        "already_in_library_count": report.already_in_library_count,
        "failed_count": report.failed_count,
        "scan_status": report.scan_status,
        "next": next,
    }) else {
        unreachable!("an object literal is an object");
    };

    items(Items {
        stage: Stage::Share,
        noun: "shares",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    })
}

fn share_entry_result(entry: ShareResultEntry, include_target_path: bool) -> ItemResult {
    let cut_text = |text: String| cut_json(text, TEXT_MAX_CHARS);
    if entry.status == ShareItemStatus::Failed {
        let failure = entry.failure.unwrap_or_else(|| {
            Failure::new(
                FailureCode::ShareFailed,
                Stage::Share,
                RetryAdvice::NotRetryable,
                "Track sharing failed",
            )
        });
        let facts = Map::from_iter([
            ("song_id".to_string(), json!(entry.song_id)),
            (
                "recipient_user_id".to_string(),
                json!(entry.recipient_user_id),
            ),
            (
                "recipient_username".to_string(),
                json!(cut_text(entry.recipient_username)),
            ),
        ]);
        return ItemResult::Failed(upstream_error(cut(&failure), facts, None));
    }

    let status = match entry.status {
        ShareItemStatus::Shared => "shared",
        ShareItemStatus::WouldShare => "would_share",
        ShareItemStatus::AlreadyInLibrary => "already_in_library",
        ShareItemStatus::Failed => unreachable!(),
    };
    let warnings: Vec<String> = entry
        .warnings
        .into_iter()
        .map(|w| cut_json(w, RAW_MAX_CHARS))
        .collect();
    let Value::Object(mut fields) = json!({
        "song_id": entry.song_id,
        "title": cut_text(entry.title),
        "artist": cut_text(entry.artist),
        "album": cut_text(entry.album),
        "recipient_user_id": entry.recipient_user_id,
        "recipient_username": cut_text(entry.recipient_username),
        "status": status,
        "link_mode": entry.link_mode.map(|m| m.as_str()),
        "target_song_id": entry.target_song_id,
        "warnings": warnings,
    }) else {
        unreachable!("an object literal is an object");
    };
    if include_target_path {
        fields.insert("target_path".into(), json!(entry.target_path));
    }
    ItemResult::Done(fields)
}

async fn get_share_history(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_SHARE_HISTORY.name, args, SHARE_HISTORY_ARGS);
    answer(
        async {
            let direction_arg = args.opt_choice("direction", &["all", "incoming", "outgoing"])?;
            let list_key = match direction_arg {
                Some(d) => json!({"direction": d}),
                None => json!({}),
            };
            let page = Page::read(&args, list_key)?;
            args.finish()?;

            let direction = match direction_arg {
                Some("incoming") => ShareDirection::Incoming,
                Some("outgoing") => ShareDirection::Outgoing,
                _ => ShareDirection::All,
            };

            let rows = TrackShareRow::page_for_user_in(
                &state.db,
                &caller.ctx.user_id,
                direction,
                page.limit,
                page.offset,
            )
            .await
            .map_err(|e| {
                upstream_error(
                    database_failure(Stage::Share, e),
                    Map::new(),
                    args.same_call(),
                )
            })?;

            let more = rows.next_cursor.is_some();
            let shares: Vec<Value> = rows
                .entries
                .iter()
                .map(|entry| share_history_fields(entry, caller))
                .collect();
            let mut fields = Map::new();
            fields.insert("direction".into(), json!(direction_arg.unwrap_or("all")));
            Ok(fit(fields, "shares", shares, &page, more))
        }
        .await,
    )
}

fn share_history_fields(entry: &ShareHistoryEntry, caller: &McpCaller) -> Value {
    let cut_text = |text: &str| cut_json(text.to_string(), TEXT_MAX_CHARS);
    let is_admin = caller.ctx.role.is_admin();
    let source_path = (is_admin || entry.sender_user_id == caller.ctx.user_id)
        .then_some(entry.source_path.as_str());
    let target_path = (is_admin || entry.recipient_user_id == caller.ctx.user_id)
        .then_some(entry.target_path.as_str());
    json!({
        "id": entry.id,
        "batch_id": entry.batch_id,
        "sender_user_id": entry.sender_user_id,
        "sender_username": cut_text(&entry.sender_username),
        "recipient_user_id": entry.recipient_user_id,
        "recipient_username": cut_text(&entry.recipient_username),
        "source_song_id": entry.source_song_id,
        "target_song_id": entry.target_song_id,
        "source_path": source_path,
        "target_path": target_path,
        "artist": cut_text(&entry.artist),
        "album": cut_text(&entry.album),
        "title": cut_text(&entry.title),
        "link_mode": entry.link_mode.as_str(),
        "origin": entry.origin.as_str(),
        "shared_at": time::text(&entry.created_at),
    })
}

async fn migrate_legacy_folders(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(MIGRATE_LEGACY_FOLDERS.name, args, MIGRATE_ARGS);
    let read = || {
        let legacy_path = args.opt_string("legacy_path")?;
        let remove_source_after = args.opt_bool("remove_source_after")?.unwrap_or(false);
        let dry_run = args.opt_bool("dry_run")?.unwrap_or(true);
        let key = request_key(&args)?;
        args.finish()?;
        Ok::<_, ToolError>((legacy_path, remove_source_after, dry_run, key))
    };
    let (legacy_path, remove_source_after, dry_run, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };

    if !caller.has_admin_access() {
        return failed(upstream_error(
            folders::admin_access_required(
                &caller.ctx,
                caller.allow_delete,
                Stage::Share,
                !dry_run && remove_source_after,
            ),
            Map::new(),
            None,
        ));
    }

    if !dry_run && remove_source_after && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Share));
    }

    let Some(legacy_path) = legacy_path else {
        return match list_legacy_groups_in(&state.db).await {
            Ok(groups) => {
                let mut fields = Map::new();
                fields.insert("legacy_folders".into(), json!(groups));
                success(fields)
            }
            Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
        };
    };

    let req = MigrateLegacyRequest {
        legacy_path,
        remove_source_after,
        dry_run,
    };

    if dry_run {
        let guard = guard_of(state);
        return match migrate_legacy_folder(
            &guard,
            state.navidrome.as_ref(),
            &state.acquire.queue,
            &caller.ctx,
            req,
        )
        .await
        {
            Ok(report) => migrate_answer(report),
            Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
        };
    }

    let call = args.as_value();
    let (db, listeners, navidrome, music_path, queue, ctx) = (
        state.db.clone(),
        state.listeners.clone(),
        state.navidrome.clone(),
        state.navidrome_music_path.clone(),
        state.acquire.queue.clone(),
        caller.ctx.clone(),
    );
    let same_call = args.same_call();
    let work = async move {
        // Run the mutation phase in a detached task so a client disconnect mid-migration never
        // aborts between hardlinking files, beets registration, Navidrome scan, and cleanup.
        let handle = tokio::spawn(async move {
            let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
            migrate_legacy_folder(&guard, navidrome.as_ref(), &queue, &ctx, req).await
        });
        match handle.await {
            Ok(Ok(report)) => Keyed::Kept(migrate_answer(report)),
            Ok(Err(f)) => Keyed::Released(failed(upstream_error(f, Map::new(), same_call))),
            Err(e) => {
                let f = Failure::new(
                    FailureCode::ShareFailed,
                    Stage::Share,
                    RetryAdvice::SameSourceLater,
                    format!("Legacy folder migration task stopped unexpectedly: {e}"),
                );
                Keyed::Released(failed(upstream_error(f, Map::new(), same_call)))
            }
        }
    };
    keyed(
        &state.acquire,
        caller,
        MIGRATE_LEGACY_FOLDERS.name,
        key,
        &call,
        work,
    )
    .await
}

fn migrate_answer(report: MigrateLegacyReport) -> CallToolResult {
    let warnings: Vec<String> = report
        .warnings
        .into_iter()
        .map(|w| cut_json(w, RAW_MAX_CHARS))
        .collect();
    let Value::Object(fields) = json!({
        "legacy_path": report.legacy_path,
        "dry_run": report.dry_run,
        "total_audio_files": report.total_audio_files,
        "target_users": report.target_users,
        "linked_count": report.linked_count,
        "already_present_count": report.already_present_count,
        "failed_count": report.failed_count,
        "removed_source_count": report.removed_source_count,
        "hardlink_count": report.hardlink_count,
        "reflink_count": report.reflink_count,
        "copy_count": report.copy_count,
        "annotations_restored": report.annotations_restored,
        "warnings": warnings,
    }) else {
        unreachable!("an object literal is an object");
    };
    success(fields)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use serde_json::json;

    use crate::connection::test_util::{access_token, call_tool, document, RESOURCE};
    use crate::connection::{router, ConnectionState};
    use crate::core::delete_guard::test_support::Library;
    use crate::core::jobs::runner::Acquire;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};
    use crate::test_navidrome::{subsonic_ok, NavidromeFor, NavidromeStub};

    async fn share_app(lib: &Library, navi: Option<&NavidromeStub>, allow_delete: bool) -> (Router, String) {
        let (acq, _effects) = Acquire::for_test(lib.db.clone());
        let navidrome: Arc<dyn crate::core::index::NavidromeOf> = match navi {
            Some(stub) => Arc::new(NavidromeFor(Ok(stub.client()))),
            None => Arc::new(NavidromeFor(Err(|| {
                crate::services::NavidromeLookupError::NotLinked
            }))),
        };
        let state = ConnectionState {
            listeners: lib.fake.clone(),
            navidrome,
            ..ConnectionState::on(lib.db.clone(), acq)
        };
        let token = access_token(&lib.db, "alice", RESOURCE).await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = ? WHERE user_id = 'alice'")
            .bind(allow_delete)
            .execute(&lib.db)
            .await
            .unwrap();
        (router(state), token)
    }

    #[tokio::test]
    async fn list_share_recipients_and_share_tracks_work_without_delete_switch() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getUser",
            &[&subsonic_ok(r#""user":{"username":"alice","adminRole":true}"#)],
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &subsonic_ok(r#""scanStatus":{"scanning":false,"count":10,"lastScan":"2026-11-14T21:00:00Z"}"#),
                &subsonic_ok(r#""scanStatus":{"scanning":false,"count":10,"lastScan":"2026-11-14T21:00:00Z"}"#),
                &subsonic_ok(r#""scanStatus":{"scanning":false,"count":11,"lastScan":"2026-11-14T21:00:05Z"}"#),
            ],
        );
        navi.answer("/rest/startScan", &[&subsonic_ok(r#""scanStatus":{"scanning":true,"count":10}"#)]);
        let (app, token) = share_app(&lib, Some(&navi), false).await;

        // 1. list_share_recipients returns bob (not alice)
        let recips = document(&call_tool(app.clone(), &token, "list_share_recipients", json!({})).await);
        assert_eq!(recips["ok"], true, "{recips}");
        let list = recips["recipients"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["username"], "bob");

        // 2. dry_run preview works without allow_delete
        let dry = document(
            &call_tool(
                app.clone(),
                &token,
                "share_tracks",
                json!({"song_ids": ["s1"], "recipient_usernames": ["bob"], "dry_run": true}),
            )
            .await,
        );
        assert_eq!(dry["ok"], true, "{dry}");
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["shared_count"], 1);
        assert_eq!(dry["results"][0]["status"], "would_share");
        assert!(!lib.root.join("music/bob/A/x.flac").exists());

        // 3. Real share hardlinks file into bob's library without requiring allow_delete
        //    and omits recipient's target_path for non-admin sender alice.
        let real = document(
            &call_tool(
                app.clone(),
                &token,
                "share_tracks",
                json!({"song_ids": ["s1"], "recipient_usernames": ["bob"], "dry_run": false}),
            )
            .await,
        );
        assert_eq!(real["ok"], true, "{real}");
        assert_eq!(real["shared_count"], 1);
        assert_eq!(real["results"][0]["status"], "shared");
        assert!(real["results"][0].get("target_path").is_none());
        assert!(lib.root.join("music/bob/A/x.flac").exists());

        // 4. get_share_history records the share and redacts recipient's target_path for non-admin sender
        let history = document(
            &call_tool(
                app.clone(),
                &token,
                "get_share_history",
                json!({"direction": "outgoing"}),
            )
            .await,
        );
        assert_eq!(history["ok"], true, "{history}");
        assert_eq!(history["shares"].as_array().unwrap().len(), 1);
        assert_eq!(history["shares"][0]["recipient_username"], "bob");
        assert_eq!(history["shares"][0]["origin"], "mcp");
        assert!(history["shares"][0]["source_path"].is_string());
        assert!(history["shares"][0]["target_path"].is_null());

        // 5. Recipient who disabled accept_shares is refused with share_recipient_disabled
        UserSettings::upsert_in(
            &lib.db,
            "bob",
            UpdateUserSettings {
                accept_shares: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let refused = document(
            &call_tool(
                app,
                &token,
                "share_tracks",
                json!({"song_ids": ["s1"], "recipient_usernames": ["bob"]}),
            )
            .await,
        );
        assert_eq!(refused["error"]["code"], "share_recipient_disabled", "{refused}");
    }

    #[tokio::test]
    async fn migrate_legacy_folders_requires_admin_access_and_delete_switch_for_cleanup() {
        let lib = Library::new().await;
        let (app, token) = share_app(&lib, None, false).await;

        // Non-admin is refused with permission_not_granted
        let err = document(
            &call_tool(app.clone(), &token, "migrate_legacy_folders", json!({})).await,
        );
        assert_eq!(err["error"]["code"], "permission_not_granted", "{err}");

        // Promote alice to admin + allow_admin
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();
        sqlx::query("UPDATE oauth_grants SET allow_admin = 1 WHERE user_id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();

        // Listing unmigrated legacy folders succeeds
        let listed = document(
            &call_tool(app.clone(), &token, "migrate_legacy_folders", json!({})).await,
        );
        assert_eq!(listed["ok"], true, "{listed}");
        assert_eq!(listed["legacy_folders"], json!([]));

        // Passing dry_run: false + remove_source_after: true without allow_delete is refused
        let need_del = document(
            &call_tool(
                app,
                &token,
                "migrate_legacy_folders",
                json!({"legacy_path": "/music/shared", "remove_source_after": true, "dry_run": false}),
            )
            .await,
        );
        assert_eq!(need_del["error"]["code"], "permission_not_granted", "{need_del}");
        assert_eq!(need_del["error"]["stage"], "share");
    }
}
