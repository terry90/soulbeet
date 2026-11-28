//! Track management tools: inspecting library tracks, moving tracks/albums across folders
//! without side effects, viewing move history, and re-tagging/re-matching via beets.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{
    cut_json, Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode, RAW_MAX_CHARS,
    TEXT_MAX_CHARS,
};
use soulbeet::{DriverChoice, TagEdits};

use super::args::Args;
use super::contract::{
    answer, failed, items, upstream_error, ItemResult, Items, NextCall, ToolError,
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
use crate::core::delete_guard::{Guard, Origin};
use crate::core::folders;
use crate::core::time;
use crate::core::tracks::inspect::{self, InspectedTrack};
use crate::core::tracks::move_tracks::{self, MoveEntry, MoveItemError, MoveReport, MoveRequest};
use crate::core::tracks::retag::{self, RetagEntry, RetagMode, RetagReport, RetagRequest};
use crate::core::tracks::{TrackSelection, ALBUM_MAX_TRACKS, SONGS_PER_CALL};
use crate::models::move_history::{MoveHistoryEntry, MoveHistoryRow};

const INSPECT_ARGS: &[&str] = &["song_ids"];

const MOVE_ARGS: &[&str] = &[
    "song_ids",
    "album_id",
    "target_folder_id",
    "dry_run",
    "replace_existing",
    "request_key",
];

const MOVE_HISTORY_ARGS: &[&str] = &["limit", "cursor"];

const RETAG_ARGS: &[&str] = &[
    "song_ids",
    "album_id",
    "tags",
    "rematch",
    "dry_run",
    "relocate",
    "request_key",
];

const TAG_FIELDS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "year",
    "track",
    "disc",
    "genre",
    "comment",
];

const REMATCH_FIELDS: &[&str] = &["mode", "recording_mbid", "release_mbid"];

pub(crate) const INSPECT_TRACKS: ToolSpec = ToolSpec {
    name: "inspect_tracks",
    title: "Inspect library tracks",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Inspects up to {SONGS_PER_CALL} tracks in this user's Navidrome library: exact local file path, which Soulbeet folder holds each track (folder_id, name, path, shared, in_navidrome), Navidrome library id, beets database entry and stored tags, album peer count and album rating/star (to tell if moving across libraries would split an album), caller's playlists, whether other listeners see the track or hold it in a static playlist (never naming who), sidecar files (.lrc, cover art), and whether it is a pending discovery track. Read-only; does not require the file-management switch. Failure codes: {common}, {NAVIDROME_CODES}, partial, and per song unknown_handle.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["song_ids"], "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(inspect_tracks(state, caller, args)),
};

pub(crate) const MOVE_TRACKS: ToolSpec = ToolSpec {
    name: "move_tracks",
    title: "Move tracks between folders",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Moves up to {SONGS_PER_CALL} songs (song_ids) or one whole album (album_id, up to {ALBUM_MAX_TRACKS} tracks) from their current folder to target_folder_id (a folder_id from list_folders; when 'Admin library access (all users' folders)' is enabled for an admin's app, this includes other users' private and shared folders as well as cross-user moves), preserving file bytes, filename, beets libraries, Navidrome song_id, ratings, stars, play counts and static playlists. Whenever multiple folders exist (personal, shared/common, or other users' folders for admins) and the user did not explicitly specify which target folder to move into, ask the user which folder they want before calling move_tracks; if target_folder_id is omitted, move_tracks answers folder_required with all accessible folders so you can ask the user. Pass dry_run: true to preview checks, exact paths, duplicates, multi-user vetoes and partial-album split warnings without moving anything (dry_run does not need the file-management switch). ALWAYS ask the user for confirmation before calling move_tracks with dry_run: false. When moving a subset of an album across Navidrome libraries, Navidrome splits the album and may move album ratings/stars (Soulbeet restores the caller's album rating/star on the original album after scanning); show the warning and offer moving the whole album with album_id instead. Moving out of a shared or multi-user-visible folder into a non-shared folder across libraries runs the multi-user veto and refuses any track another listener rated, starred or kept in a static playlist. Duplicates already in target_folder_id are refused by default with both copies' facts; pass replace_existing: true only after the user agrees to replace the target copy (which runs the full delete guard and counts against the daily delete quota). Executing a move (dry_run: false) requires 'Allow managing library files (delete, move, re-tag)' switched on for this app in Settings > Users > Connected apps. Failure codes: {common}, {NAVIDROME_CODES}, folder_required, permission_not_granted, unknown_handle, already_running, partial, and per song move_same_folder, move_target_exists, move_duplicate_in_target, move_failed, delete_path_unresolved, delete_outside_library, delete_not_a_track, delete_in_use, delete_blocked_by_other_user, delete_veto_unknown, delete_daily_limit.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}, "album_id": {"type": "string", "minLength": 1}, "target_folder_id": {"type": "string", "minLength": 1}, "dry_run": {"type": "boolean"}, "replace_existing": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(move_tracks(state, caller, args)),
};

pub(crate) const GET_MOVE_HISTORY: ToolSpec = ToolSpec {
    name: "get_move_history",
    title: "Move history",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Lists this user's track moves between folders, newest first: song_id, new_song_id, title, artist, album, source and target folders and paths, status (moved, moved_pending_scan, moved_id_changed), warnings, origin, app and when. Takes limit and cursor. Failure codes: {common}.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_move_history(state, caller, args)),
};

pub(crate) const RETAG_TRACKS: ToolSpec = ToolSpec {
    name: "retag_tracks",
    title: "Re-tag or re-match tracks",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Edits tags directly (tags) or re-matches against MusicBrainz via beets (rematch) for up to {SONGS_PER_CALL} songs (song_ids) or one whole album (album_id, up to {ALBUM_MAX_TRACKS} tracks). By default (relocate: false), tags are written in place without renaming or moving the file so Navidrome keeps the same song_id, ratings, stars, play counts and playlists. With relocate: true, Soulbeet runs a 2-scan dance (write tags -> Navidrome scan -> beet move -> Navidrome scan) so song_id stays intact; relocate: true requires a linked Navidrome admin account (otherwise refused retag_relocate_needs_scan). Pass dry_run: true to preview current tags or MusicBrainz candidates without writing anything (dry_run does not need the file-management switch). ALWAYS ask the user before calling retag_tracks with dry_run: false. Executing (dry_run: false) requires 'Allow managing library files (delete, move, re-tag)' switched on in Settings > Users > Connected apps. Failure codes: {common}, {NAVIDROME_CODES}, permission_not_granted, unknown_handle, already_running, retag_relocate_needs_scan, partial, and per song retag_failed, import_no_match, delete_path_unresolved, delete_outside_library, delete_not_a_track, delete_in_use.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}, "album_id": {"type": "string", "minLength": 1}, "tags": {"type": "object", "additionalProperties": false, "properties": {"title": {"type": "string"}, "artist": {"type": "string"}, "album": {"type": "string"}, "album_artist": {"type": "string"}, "year": {"type": "integer", "minimum": 1, "maximum": 9999}, "track": {"type": "integer", "minimum": 1, "maximum": 9999}, "disc": {"type": "integer", "minimum": 1, "maximum": 999}, "genre": {"type": "string"}, "comment": {"type": "string"}}}, "rematch": {"type": "object", "additionalProperties": false, "required": ["mode"], "properties": {"mode": {"type": "string", "enum": ["auto", "pick"]}, "recording_mbid": {"type": "string"}, "release_mbid": {"type": "string"}}}, "dry_run": {"type": "boolean"}, "relocate": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(retag_tracks(state, caller, args)),
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

async fn inspect_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(INSPECT_TRACKS.name, args, INSPECT_ARGS);
    let song_ids = match (|| {
        let ids = args.strings("song_ids", 1, SONGS_PER_CALL)?;
        if (1..ids.len()).any(|i| ids[..i].contains(&ids[i])) {
            return Err(args.refuse(
                "song_ids",
                &format!("a list of 1 to {SONGS_PER_CALL} distinct song ids"),
            ));
        }
        args.finish()?;
        Ok::<_, ToolError>(ids)
    })() {
        Ok(ids) => ids,
        Err(e) => return failed(e),
    };

    let entries = match inspect::inspect_tracks(
        &state.db,
        state.listeners.as_ref(),
        &state.acquire.queue,
        state.navidrome_music_path.as_deref(),
        &caller.ctx,
        &song_ids,
        caller.has_admin_access(),
    )
    .await
    {
        Ok(entries) => entries,
        Err(f) => return failed(upstream_error(f, Map::new(), args.same_call())),
    };

    let results: Vec<ItemResult> = entries
        .into_iter()
        .map(|entry| match entry.outcome {
            Ok(info) => inspect_item(info),
            Err(f) => {
                let facts = Map::from_iter([("song_id".to_string(), json!(entry.song_id))]);
                ItemResult::Failed(upstream_error(cut(&f), facts, None))
            }
        })
        .collect();

    items(Items {
        stage: Stage::LibraryCheck,
        noun: "songs",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields: Map::new(),
    })
}

fn inspect_item(info: InspectedTrack) -> ItemResult {
    let Value::Object(fields) = json!(info) else {
        unreachable!("InspectedTrack serializes as a JSON object");
    };
    ItemResult::Done(fields)
}

async fn move_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(MOVE_TRACKS.name, args, MOVE_ARGS);
    let read = || {
        let song_ids = args.opt_strings("song_ids", SONGS_PER_CALL)?;
        let album_id = args.opt_string("album_id")?;
        let target_folder_id = args.opt_string("target_folder_id")?;
        let dry_run = args.opt_bool("dry_run")?.unwrap_or(false);
        let replace_existing = args.opt_bool("replace_existing")?.unwrap_or(false);
        let key = request_key(&args)?;
        args.finish()?;
        let selection = read_selection(&args, song_ids, album_id)?;
        Ok::<_, ToolError>((selection, target_folder_id, dry_run, replace_existing, key))
    };
    let (selection, target_folder_id, dry_run, replace_existing, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };
    if !dry_run && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Move));
    }

    let admin_access = caller.has_admin_access();
    let Some(target_folder_id) = target_folder_id else {
        return folder_required_for_move(state, caller, admin_access).await;
    };

    let req = MoveRequest {
        selection,
        target_folder_id,
        dry_run,
        replace_existing,
        admin_access,
        allow_delete: caller.allow_delete,
    };

    if req.dry_run {
        let guard = guard_of(state);
        let origin = Origin::Mcp {
            grant_id: caller.grant_id.clone(),
        };
        return match move_tracks::move_tracks(
            &guard,
            state.navidrome.as_ref(),
            &state.acquire.queue,
            &caller.ctx,
            &origin,
            req.clone(),
        )
        .await
        {
            Ok(report) => move_answer(&req, report),
            Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
        };
    }

    let call = args.as_value();
    let (db, listeners, navidrome, music_path, queue, ctx, grant_id) = (
        state.db.clone(),
        state.listeners.clone(),
        state.navidrome.clone(),
        state.navidrome_music_path.clone(),
        state.acquire.queue.clone(),
        caller.ctx.clone(),
        caller.grant_id.clone(),
    );
    let same_call = args.same_call();
    let req_clone = req.clone();
    let work = async move {
        // Run the mutation phase in a detached task so a client disconnect mid-move never
        // aborts between `atomic_move_file`, beets registration, Navidrome scan, and
        // annotation restoration.
        let handle = tokio::spawn(async move {
            let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
            let origin = Origin::Mcp { grant_id };
            move_tracks::move_tracks(&guard, navidrome.as_ref(), &queue, &ctx, &origin, req_clone)
                .await
        });
        match handle.await {
            Ok(Ok(report)) => Keyed::Kept(move_answer(&req, report)),
            Ok(Err(f)) => Keyed::Released(failed(upstream_error(f, Map::new(), same_call))),
            Err(e) => {
                let f = Failure::new(
                    FailureCode::MoveFailed,
                    Stage::Move,
                    RetryAdvice::SameSourceLater,
                    format!("Move task stopped unexpectedly: {e}"),
                );
                Keyed::Released(failed(upstream_error(f, Map::new(), same_call)))
            }
        }
    };
    keyed(&state.acquire, caller, MOVE_TRACKS.name, key, &call, work).await
}

async fn folder_required_for_move(
    state: &ConnectionState,
    caller: &McpCaller,
    admin_access: bool,
) -> CallToolResult {
    let folder_list: Vec<Value> = if admin_access {
        match folders::list_all_with_owners_in(&state.db).await {
            Ok(all) => all
                .into_iter()
                .map(|row| {
                    let is_own = row.folder.user_id == caller.ctx.user_id;
                    json!({
                        "folder_id": row.folder.id,
                        "name": cut_json(row.folder.name, TEXT_MAX_CHARS),
                        "owner": cut_json(row.owner, TEXT_MAX_CHARS),
                        "shared": row.folder.shared,
                        "is_own": is_own,
                        "path": row.folder.path,
                    })
                })
                .collect(),
            Err(f) => return failed(upstream_error(f, Map::new(), None)),
        }
    } else {
        match folders::list_in(&state.db, &caller.ctx).await {
            Ok(own) => own
                .into_iter()
                .map(|f| {
                    json!({
                        "folder_id": f.id,
                        "name": cut_json(f.name, TEXT_MAX_CHARS),
                        "owner": cut_json(caller.ctx.username.clone(), TEXT_MAX_CHARS),
                        "shared": f.shared,
                        "is_own": true,
                        "path": f.path,
                    })
                })
                .collect(),
            Err(f) => return failed(upstream_error(f, Map::new(), None)),
        }
    };

    if folder_list.is_empty() {
        let failure = Failure::new(
            FailureCode::FolderRequired,
            Stage::Input,
            RetryAdvice::NeedsUser,
            "No library folders are configured",
        )
        .with_user_action(UserAction::new(
            UserActionCode::AddFolder,
            "Add a library folder",
            "Settings > Library",
        ));
        return failed(ToolError {
            failure,
            facts: Map::new(),
            tried: vec![],
            next: vec![],
        });
    }

    let failure = Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsDecision,
        "No target_folder_id was given. Ask the user which folder to move the track(s) into, then call move_tracks with target_folder_id.",
    );
    let mut facts = Map::new();
    facts.insert("folders".into(), json!(folder_list));
    failed(ToolError {
        failure,
        facts,
        tried: vec![],
        next: vec![NextCall::new("list_folders", json!({}))],
    })
}

fn move_answer(req: &MoveRequest, report: MoveReport) -> CallToolResult {
    let mut next: Vec<NextCall> = Vec::new();
    let mut offered_albums: Vec<String> = Vec::new();

    let results: Vec<ItemResult> = report
        .entries
        .into_iter()
        .map(|entry| {
            move_entry_result(
                entry,
                &req.target_folder_id,
                req.dry_run,
                &mut next,
                &mut offered_albums,
            )
        })
        .collect();

    if !report.dry_run {
        next.push(NextCall::new(GET_MOVE_HISTORY.name, json!({})));
    }

    let Value::Object(fields) = json!({
        "dry_run": report.dry_run,
        "target_folder": report.target_folder,
        "album_id": report.album_id,
        "scan_status": report.scan_status,
        "next": next,
    }) else {
        unreachable!("an object literal is an object");
    };

    items(Items {
        stage: Stage::Move,
        noun: "songs",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    })
}

fn move_entry_result(
    entry: MoveEntry,
    target_folder_id: &str,
    dry_run: bool,
    next: &mut Vec<NextCall>,
    offered_albums: &mut Vec<String>,
) -> ItemResult {
    match entry.outcome {
        Ok(mut ok) => {
            ok.warnings = ok
                .warnings
                .into_iter()
                .map(|w| cut_json(w, RAW_MAX_CHARS))
                .collect();
            if let Some(split) = &ok.album_split {
                if !offered_albums.contains(&split.album_id) {
                    offered_albums.push(split.album_id.clone());
                    next.push(
                        NextCall::new(
                            MOVE_TRACKS.name,
                            json!({
                                "album_id": split.album_id,
                                "target_folder_id": target_folder_id,
                                "dry_run": dry_run,
                            }),
                        )
                        .with("requires_user_confirmation", json!(!dry_run)),
                    );
                }
            }
            let Value::Object(fields) = json!(ok) else {
                unreachable!("MoveItemSuccess serializes as a JSON object");
            };
            ItemResult::Done(fields)
        }
        Err(MoveItemError {
            failure,
            song,
            existing_in_target,
            discovery_track_id,
        }) => {
            let mut facts = Map::from_iter([
                ("song_id".to_string(), json!(entry.song_id)),
                ("song".to_string(), json!(song)),
            ]);
            if !existing_in_target.is_empty() {
                facts.insert("existing_in_target".into(), json!(existing_in_target));
            }
            let mut item_next = Vec::new();
            if let Some(track_id) = discovery_track_id {
                facts.insert("discovery_track_id".into(), json!(track_id));
                item_next.push(NextCall::new(
                    "promote_discovery_track",
                    json!({"track_id": track_id, "target_folder_id": target_folder_id}),
                ));
            } else if failure.code == FailureCode::MoveDuplicateInTarget {
                item_next.push(
                    NextCall::new(
                        MOVE_TRACKS.name,
                        json!({
                            "song_ids": [entry.song_id],
                            "target_folder_id": target_folder_id,
                            "replace_existing": true,
                        }),
                    )
                    .with("requires_user_confirmation", json!(true)),
                );
            }
            ItemResult::Failed(ToolError {
                failure: cut(&failure),
                facts,
                tried: vec![],
                next: item_next,
            })
        }
    }
}

async fn get_move_history(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_MOVE_HISTORY.name, args, MOVE_HISTORY_ARGS);
    answer(
        async {
            let page = Page::read(&args, json!({}))?;
            args.finish()?;
            let unavailable = |e: String| {
                upstream_error(
                    database_failure(Stage::Move, e),
                    Map::new(),
                    args.same_call(),
                )
            };
            let unreadable = |e: String| {
                let mut f = database_failure(Stage::Move, e);
                f.retry = RetryAdvice::NotRetryable;
                upstream_error(f, Map::new(), None)
            };
            let rows = MoveHistoryRow::page_in(
                &state.db,
                &caller.ctx.user_id,
                page.offset as u32,
                page.limit as u32 + 1,
            )
            .await
            .map_err(unavailable)?;
            let more = rows.len() > page.limit;
            let moves = rows
                .iter()
                .take(page.limit)
                .map(move_history_fields)
                .collect::<Result<Vec<Value>, String>>()
                .map_err(unreadable)?;
            Ok(fit(Map::new(), "moves", moves, &page, more))
        }
        .await,
    )
}

fn move_history_fields(m: &MoveHistoryEntry) -> Result<Value, String> {
    let moved_at = chrono::NaiveDateTime::parse_from_str(&m.created_at, "%Y-%m-%d %H:%M:%S")
        .map_err(|e| format!("Unreadable move time {}: {e}", m.created_at))?
        .and_utc();
    let cut_text = |text: &str| cut_json(text.to_string(), TEXT_MAX_CHARS);
    Ok(json!({
        "song_id": m.song_id,
        "new_song_id": m.new_song_id,
        "title": cut_text(&m.title),
        "artist": cut_text(&m.artist),
        "album": cut_text(&m.album),
        "source_folder_id": m.source_folder_id,
        "source_path": m.source_path,
        "target_folder_id": m.target_folder_id,
        "target_path": m.target_path,
        "status": m.status,
        "warnings": m.warnings,
        "origin": m.origin,
        "app": m.client,
        "moved_at": time::at(moved_at),
    }))
}

async fn retag_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(RETAG_TRACKS.name, args, RETAG_ARGS);
    let read = || {
        let song_ids = args.opt_strings("song_ids", SONGS_PER_CALL)?;
        let album_id = args.opt_string("album_id")?;
        let tags_obj = args.opt_object("tags", TAG_FIELDS)?;
        let rematch_obj = args.opt_object("rematch", REMATCH_FIELDS)?;
        let dry_run = args.opt_bool("dry_run")?.unwrap_or(false);
        let relocate = args.opt_bool("relocate")?.unwrap_or(false);
        let key = request_key(&args)?;
        args.finish()?;
        let selection = read_selection(&args, song_ids, album_id)?;
        let mode = match (tags_obj, rematch_obj) {
            (Some(t), None) => {
                let edits = TagEdits {
                    title: t.opt_string("title")?,
                    artist: t.opt_string("artist")?,
                    album: t.opt_string("album")?,
                    album_artist: t.opt_string("album_artist")?,
                    year: t.opt_u32("year", 1, 9999)?,
                    track: t.opt_u32("track", 1, 9999)?,
                    disc: t.opt_u32("disc", 1, 999)?,
                    genre: t.opt_string("genre")?,
                    comment: t.opt_string("comment")?,
                };
                t.finish()?;
                if edits.is_empty() {
                    return Err(args.refuse("tags", "at least one tag field to modify"));
                }
                RetagMode::Direct(edits)
            }
            (None, Some(r)) => {
                let mode_str = r
                    .opt_choice("mode", &["auto", "pick"])?
                    .ok_or_else(|| r.refuse("mode", "one of auto, pick"))?;
                let recording_mbid = r.opt_mbid("recording_mbid")?;
                let release_mbid = r.opt_mbid("release_mbid")?;
                r.finish()?;
                let choice = match mode_str {
                    "auto" => DriverChoice::Auto,
                    "pick" => match (recording_mbid, release_mbid) {
                        (Some(id), None) | (None, Some(id)) => DriverChoice::Pick {
                            data_source: "MusicBrainz".into(),
                            id,
                        },
                        (Some(_), Some(_)) => {
                            return Err(r.refuse(
                                "recording_mbid",
                                "either recording_mbid or release_mbid, not both",
                            ));
                        }
                        (None, None) => {
                            return Err(r.refuse(
                                "recording_mbid",
                                "recording_mbid or release_mbid when mode is pick",
                            ));
                        }
                    },
                    _ => unreachable!(),
                };
                let album_mode = matches!(selection, TrackSelection::Album(_));
                RetagMode::Rematch { choice, album_mode }
            }
            (Some(_), Some(_)) => {
                return Err(args.refuse("tags", "either tags or rematch, not both"));
            }
            (None, None) => return Err(args.refuse("tags", "either tags or rematch")),
        };
        Ok::<_, ToolError>((
            RetagRequest {
                selection,
                mode,
                dry_run,
                relocate,
            },
            key,
        ))
    };
    let (req, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };
    if !req.dry_run && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Retag));
    }

    if req.dry_run {
        let guard = guard_of(state);
        let origin = Origin::Mcp {
            grant_id: caller.grant_id.clone(),
        };
        return match retag::retag_tracks(
            &guard,
            state.navidrome.as_ref(),
            &state.acquire.queue,
            &caller.ctx,
            &origin,
            req,
        )
        .await
        {
            Ok(report) => retag_answer(report),
            Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
        };
    }

    let call = args.as_value();
    let (db, listeners, navidrome, music_path, queue, ctx, grant_id) = (
        state.db.clone(),
        state.listeners.clone(),
        state.navidrome.clone(),
        state.navidrome_music_path.clone(),
        state.acquire.queue.clone(),
        caller.ctx.clone(),
        caller.grant_id.clone(),
    );
    let same_call = args.same_call();
    let work = async move {
        // Run the mutation phase in a detached task so a client disconnect mid-retag never
        // aborts after `queue.forget` before beets registration and Navidrome scan finish.
        let handle = tokio::spawn(async move {
            let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
            let origin = Origin::Mcp { grant_id };
            retag::retag_tracks(&guard, navidrome.as_ref(), &queue, &ctx, &origin, req).await
        });
        match handle.await {
            Ok(Ok(report)) => Keyed::Kept(retag_answer(report)),
            Ok(Err(f)) => Keyed::Released(failed(upstream_error(f, Map::new(), same_call))),
            Err(e) => {
                let f = Failure::new(
                    FailureCode::RetagFailed,
                    Stage::Retag,
                    RetryAdvice::SameSourceLater,
                    format!("Retag task stopped unexpectedly: {e}"),
                );
                Keyed::Released(failed(upstream_error(f, Map::new(), same_call)))
            }
        }
    };
    keyed(&state.acquire, caller, RETAG_TRACKS.name, key, &call, work).await
}

fn retag_answer(report: RetagReport) -> CallToolResult {
    let results: Vec<ItemResult> = report
        .entries
        .into_iter()
        .map(|entry: RetagEntry| match entry.outcome {
            Ok(mut ok) => {
                ok.warnings = ok
                    .warnings
                    .into_iter()
                    .map(|w| cut_json(w, RAW_MAX_CHARS))
                    .collect();
                let Value::Object(fields) = json!(ok) else {
                    unreachable!("RetagItemSuccess serializes as a JSON object");
                };
                ItemResult::Done(fields)
            }
            Err(f) => {
                let facts = Map::from_iter([("song_id".to_string(), json!(entry.song_id))]);
                ItemResult::Failed(upstream_error(cut(&f), facts, None))
            }
        })
        .collect();

    let Value::Object(fields) = json!({
        "dry_run": report.dry_run,
        "relocated": report.relocated,
        "scan_status": report.scan_status,
    }) else {
        unreachable!("an object literal is an object");
    };

    items(Items {
        stage: Stage::Retag,
        noun: "songs",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::Router;
    use serde_json::json;
    use soulbeet::navidrome::models::NativeSong;
    use soulbeet::{BeetsTrackInfo, ImportOutcome, ImportResult, Relocation, SkipReason};

    use crate::connection::test_util::{access_token, call_tool, document, RESOURCE};
    use crate::connection::{router, ConnectionState};
    use crate::core::delete_guard::test_support::{mtime, Library};
    use crate::core::jobs::runner::{Acquire, TestEffects};
    use crate::import_queue::test_support::{candidate, no_match, track_choices};
    use crate::models::navidrome_library::NavidromeLibraryRow;
    use crate::test_navidrome::{subsonic_failed, subsonic_ok, NavidromeFor, NavidromeStub};

    async fn teach_library(db: &sqlx::SqlitePool, library_id: i64, root: &str) {
        NavidromeLibraryRow::upsert_in(
            db,
            &NavidromeLibraryRow {
                library_id,
                navidrome_path: root.into(),
                local_path: root.into(),
            },
        )
        .await
        .unwrap();
    }

    async fn track_app(
        lib: &Library,
        navi: Option<&NavidromeStub>,
        allow_delete: bool,
    ) -> (Router, String, Arc<TestEffects>) {
        let (acq, effects) = Acquire::for_test(lib.db.clone());
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
        (router(state), token, effects)
    }

    fn scan_status(scanning: bool, last_scan: &str) -> String {
        subsonic_ok(&format!(
            r#""scanStatus":{{"scanning":{scanning},"count":10,"lastScan":"{last_scan}"}}"#
        ))
    }

    fn user_role(admin: bool) -> String {
        subsonic_ok(&format!(
            r#""user":{{"username":"alice","adminRole":{admin}}}"#
        ))
    }

    fn native_song_json(
        id: &str,
        path: &str,
        title: &str,
        size: u64,
        library_id: i64,
        library_path: &str,
        album_id: &str,
    ) -> String {
        json!({
            "id": id,
            "path": path,
            "title": title,
            "artist": "Marrow Choir",
            "album": "Winter Songs",
            "albumArtist": "Marrow Choir",
            "albumId": album_id,
            "size": size,
            "libraryId": library_id,
            "libraryPath": library_path,
            "missing": false,
            "updatedAt": mtime(),
            "suffix": "flac",
            "bitRate": 912,
            "trackNumber": 3,
            "year": 2019,
            "duration": 263.0,
            "rating": 4,
            "starred": true,
            "playCount": 3
        })
        .to_string()
    }

    fn make_song(
        id: &str,
        path: &str,
        title: &str,
        size: u64,
        library_id: i64,
        library_path: &str,
        album_id: &str,
    ) -> NativeSong {
        NativeSong {
            id: id.into(),
            path: path.into(),
            title: title.into(),
            size,
            library_id,
            library_path: library_path.into(),
            missing: false,
            updated_at: mtime(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            album_artist: "Marrow Choir".into(),
            rating: 0,
            average_rating: None,
            mbz_recording_id: None,
            album_id: album_id.into(),
            track_number: 3,
            year: 2019,
            duration: 263.0,
            suffix: "flac".into(),
            bit_rate: 912,
            starred: false,
            starred_at: None,
            rated_at: None,
            play_count: 0,
            play_date: None,
            mbz_artist_id: None,
            mbz_album_artist_id: None,
            participants: Default::default(),
        }
    }

    #[tokio::test]
    async fn move_and_retag_require_switch_for_writes_but_allow_dry_run_and_inspect() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();
        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        let (app, token, _) = track_app(&lib, Some(&navi), false).await;

        // Real move refused without switch
        let err = &document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (err["code"].as_str(), err["stage"].as_str()),
            (Some("permission_not_granted"), Some("move"))
        );
        assert_eq!(err["user_action"]["code"], "enable_deletion");

        // Real retag refused without switch
        let err = &document(
            &call_tool(
                app.clone(),
                &token,
                "retag_tracks",
                json!({"song_ids": ["s1"], "tags": {"genre": "Ambient"}}),
            )
            .await,
        )["error"];
        assert_eq!(
            (err["code"].as_str(), err["stage"].as_str()),
            (Some("permission_not_granted"), Some("retag"))
        );

        // dry_run move works without switch and touches nothing on disk
        let dry_move = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music", "dry_run": true}),
            )
            .await,
        );
        assert_eq!(dry_move["ok"], true, "{dry_move}");
        assert_eq!(dry_move["dry_run"], true);
        assert_eq!(dry_move["results"][0]["status"], "would_move");
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert!(!lib.root.join("music/shared/A/x.flac").exists());

        // dry_run retag works without switch
        let dry_retag = document(
            &call_tool(
                app.clone(),
                &token,
                "retag_tracks",
                json!({"song_ids": ["s1"], "tags": {"genre": "Ambient"}, "dry_run": true}),
            )
            .await,
        );
        assert_eq!(dry_retag["ok"], true, "{dry_retag}");
        assert_eq!(dry_retag["results"][0]["status"], "would_retag");

        // inspect_tracks and get_move_history work without switch
        let inspected = document(
            &call_tool(
                app.clone(),
                &token,
                "inspect_tracks",
                json!({"song_ids": ["s1"]}),
            )
            .await,
        );
        assert_eq!(inspected["ok"], true, "{inspected}");
        let history = document(&call_tool(app, &token, "get_move_history", json!({})).await);
        assert_eq!(history["ok"], true);
        assert_eq!(history["moves"], json!([]));
    }

    #[tokio::test]
    async fn inspect_tracks_reports_folder_beets_album_playlists_sidecars_and_privacy() {
        let lib = Library::new().await;
        // Add sidecar .lrc and cover.jpg next to s1
        lib.write("alice/A/x.lrc", b"[00:01.00] O Holy Night");
        lib.write("alice/A/cover.jpg", b"jpegdata");
        lib.fake.playlist("alice", "Winter Mix", &["s1"], false, false);
        lib.fake.playlist("bob", "Bob Private", &["s1"], false, false);
        lib.fake.rates_album("alice", "al1", 5);
        lib.fake.stars_album("alice", "al1");

        let (app, token, effects) = track_app(&lib, None, false).await;
        effects.importer().script_inspect(Ok(Some(BeetsTrackInfo {
            beets_id: 42,
            path: lib.root.join("music/alice/A/x.flac"),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            album_artist: "Marrow Choir".into(),
            year: Some(2019),
            track: Some(3),
            disc: Some(1),
            genre: "Choral".into(),
            comments: "".into(),
            recording_mbid: None,
            release_mbid: None,
        })));

        let doc = document(
            &call_tool(app, &token, "inspect_tracks", json!({"song_ids": ["s1"]})).await,
        );
        assert_eq!(doc["ok"], true, "{doc}");
        let item = &doc["results"][0];
        assert_eq!(item["folder"]["folder_id"], "alice-music");
        assert_eq!(item["folder"]["shared"], false);
        assert_eq!(item["beets"]["beets_id"], 42);
        assert_eq!(item["beets"]["genre"], "Choral");
        assert_eq!(item["album"]["album_id"], "al1");
        assert_eq!(item["album"]["user_rating"], 5);
        assert_eq!(item["album"]["starred"], true);
        assert_eq!(item["playlists"][0]["name"], "Winter Mix");
        assert_eq!(item["other_listeners_in_playlists"], true);
        assert_eq!(item["visible_to_other_users"], true);
        assert_eq!(item["sidecars"], json!(["x.lrc"]));
        assert_eq!(item["album_art"], json!(["cover.jpg"]));
        assert!(
            !doc.to_string().contains("bob"),
            "inspect_tracks must never reveal other listener names: {doc}"
        );
    }

    #[tokio::test]
    async fn move_tracks_personal_to_shared_moves_files_updates_beets_scans_and_records_history() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();
        // Sidecars next to s1 (which is the only track in alice/A/)
        lib.write("alice/A/x.lrc", b"lyrics");
        lib.write("alice/A/cover.jpg", b"art");

        let navi = NavidromeStub::start();
        let lib_path = lib.root.join("music").to_string_lossy().into_owned();
        navi.answer("/rest/getUser", &[&user_role(true)]);
        // No duplicates in target, and post-scan get_native_song finds s1 at its new path
        navi.answer("/api/song", &["[]"]);
        navi.answer_when(
            "/api/song",
            "id=s1",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s1",
                    "shared/A/x.flac",
                    "O Holy Night",
                    5,
                    1,
                    &lib_path,
                    "al1",
                )
            ),
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );

        let (app, token, effects) = track_app(&lib, Some(&navi), true).await;
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music"}),
            )
            .await,
        );
        assert_eq!(doc["ok"], true, "{doc}");
        let entry = &doc["results"][0];
        assert_eq!(entry["status"], "moved");
        assert_eq!(entry["song_id_preserved"], true);
        assert_eq!(entry["new_song_id"], "s1");
        assert_eq!(entry["source_folder"]["folder_id"], "alice-music");
        assert_eq!(entry["target_folder"]["folder_id"], "shared-music");
        assert_eq!(entry["target_folder"]["shared"], true);

        // Files moved and empty source directory pruned
        assert!(!lib.root.join("music/alice/A/x.flac").exists());
        assert!(!lib.root.join("music/alice/A/x.lrc").exists());
        assert!(!lib.root.join("music/alice/A/cover.jpg").exists());
        assert!(!lib.root.join("music/alice/A").exists());
        assert!(lib.root.join("music/shared/A/x.flac").exists());
        assert!(lib.root.join("music/shared/A/x.lrc").exists());
        assert!(lib.root.join("music/shared/A/cover.jpg").exists());

        // Beets forgot from source and registered in target
        assert_eq!(effects.importer().forgets().len(), 1);
        assert_eq!(effects.importer().registers().len(), 1);
        assert!(navi.asked().iter().any(|p| p == "/rest/startScan"));

        // Move history recorded and readable via get_move_history
        let hist = document(&call_tool(app, &token, "get_move_history", json!({})).await);
        assert_eq!(hist["ok"], true, "{hist}");
        assert_eq!(hist["moves"].as_array().unwrap().len(), 1);
        assert_eq!(hist["moves"][0]["song_id"], "s1");
        assert_eq!(hist["moves"][0]["new_song_id"], "s1");
        assert_eq!(hist["moves"][0]["source_folder_id"], "alice-music");
        assert_eq!(hist["moves"][0]["target_folder_id"], "shared-music");
        assert_eq!(hist["moves"][0]["status"], "moved");
        assert_eq!(hist["moves"][0]["app"], "claude.ai");
    }

    #[tokio::test]
    async fn move_tracks_without_admin_scan_returns_moved_pending_scan() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();

        let navi = NavidromeStub::start();
        navi.answer("/rest/getUser", &[&user_role(false)]);
        navi.answer("/api/song", &["[]"]);
        navi.answer(
            "/rest/getScanStatus",
            &[&scan_status(false, "2026-11-14T21:00:00Z")],
        );
        navi.answer("/rest/startScan", &[&subsonic_failed(50, "not authorized")]);

        let (app, token, _) = track_app(&lib, Some(&navi), true).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music"}),
            )
            .await,
        );
        assert_eq!(doc["ok"], true, "{doc}");
        assert_eq!(doc["results"][0]["status"], "moved_pending_scan");
        assert!(doc["results"][0]["song_id_preserved"].is_null());
        assert!(lib.root.join("music/shared/A/x.flac").exists());
    }

    #[tokio::test]
    async fn move_tracks_same_folder_and_duplicate_handling_and_replace_existing() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();

        // Put a duplicate copy s2 in shared-music
        let lib_path = lib.root.join("music").to_string_lossy().into_owned();
        lib.fake.file(
            make_song("s2", "shared/Other/x_dup.flac", "O Holy Night", 4, 1, &lib_path, "al2"),
            "al2",
        );
        lib.write("shared/Other/x_dup.flac", b"old!");
        lib.fake.sees("alice", "s2");
        lib.fake.sees("bob", "s2");
        lib.fake.rates("alice", "s2", 1);

        let navi = NavidromeStub::start();
        navi.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song_json(
                    "s2",
                    "shared/Other/x_dup.flac",
                    "O Holy Night",
                    4,
                    1,
                    &lib_path,
                    "al2"
                )
            )],
        );
        navi.answer(
            "/api/song/s1",
            &[&native_song_json(
                "s1",
                "shared/A/x.flac",
                "O Holy Night",
                5,
                1,
                &lib_path,
                "al1",
            )],
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );

        let (app, token, _) = track_app(&lib, Some(&navi), true).await;

        // 1. Same folder refused
        let same = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "alice-music"}),
            )
            .await,
        );
        assert_eq!(same["results"][0]["error"]["code"], "move_same_folder");

        // 2. Duplicate in target refused by default with both copies' facts + replace_existing offer
        let dup = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music"}),
            )
            .await,
        );
        let err = &dup["results"][0]["error"];
        assert_eq!(err["code"], "move_duplicate_in_target", "{dup}");
        assert_eq!(err["facts"]["existing_in_target"][0]["song_id"], "s2");
        assert_eq!(err["next"][0]["tool"], "move_tracks");
        assert_eq!(err["next"][0]["args"]["replace_existing"], true);
        assert_eq!(err["next"][0]["requires_user_confirmation"], true);

        // 3. If bob stars s2, replace_existing: true is blocked by delete guard!
        lib.fake.stars("bob", "s2");
        let blocked = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music", "replace_existing": true}),
            )
            .await,
        );
        assert_eq!(
            blocked["results"][0]["error"]["code"],
            "delete_blocked_by_other_user",
            "{blocked}"
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert!(lib.root.join("music/shared/Other/x_dup.flac").exists());

        // 4. With bob's star removed (create a fresh s3 duplicate nobody starred), replace_existing succeeds
        let lib2 = Library::new().await;
        lib2.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib2.root.join("music/shared")).unwrap();
        let lib2_path = lib2.root.join("music").to_string_lossy().into_owned();
        lib2.fake.file(
            make_song("s2", "shared/Other/x_dup.flac", "O Holy Night", 4, 1, &lib2_path, "al2"),
            "al2",
        );
        lib2.write("shared/Other/x_dup.flac", b"old!");
        lib2.fake.sees("alice", "s2");
        lib2.fake.sees("bob", "s2");
        lib2.fake.rates("alice", "s2", 1);
        let navi2 = NavidromeStub::start();
        navi2.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song_json(
                    "s2",
                    "shared/Other/x_dup.flac",
                    "O Holy Night",
                    4,
                    1,
                    &lib2_path,
                    "al2"
                )
            )],
        );
        navi2.answer(
            "/api/song/s1",
            &[&native_song_json(
                "s1",
                "shared/A/x.flac",
                "O Holy Night",
                5,
                1,
                &lib2_path,
                "al1",
            )],
        );
        navi2.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );
        let (app2, token2, _) = track_app(&lib2, Some(&navi2), true).await;
        let replaced = document(
            &call_tool(
                app2.clone(),
                &token2,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music", "replace_existing": true}),
            )
            .await,
        );
        assert_eq!(replaced["ok"], true, "{replaced}");
        assert_eq!(replaced["results"][0]["replaced"][0]["song_id"], "s2");
        assert!(!lib2.root.join("music/shared/Other/x_dup.flac").exists());
        assert!(lib2.root.join("music/shared/A/x.flac").exists());
        // Recorded in deletion_reviews (counts toward daily quota)
        let del_hist = document(&call_tool(app2, &token2, "get_deletion_history", json!({})).await);
        assert_eq!(del_hist["deletions"][0]["song_id"], "s2");
        assert_eq!(del_hist["deletions"][0]["reason"], "replaced");
    }

    #[tokio::test]
    async fn move_tracks_shared_to_personal_across_libraries_enforces_multi_user_veto() {
        let lib = Library::new().await;
        // Shared folder lives in Navidrome library 2; personal alice-music is in library 1
        lib.folder("alice", "shared-music", "shared_lib", true)
            .await;
        let shared_root = lib.root.join("shared_lib");
        std::fs::create_dir_all(&shared_root).unwrap();
        let shared_root_str = shared_root.to_string_lossy().into_owned();
        let track_rel = "A/shared_track.flac";
        std::fs::create_dir_all(shared_root.join("A")).unwrap();
        let track_path = shared_root.join(track_rel);
        std::fs::write(&track_path, b"audio").unwrap();
        filetime_set(&track_path, mtime());

        lib.fake.file(
            make_song("s_shared", track_rel, "Shared Tune", 5, 2, &shared_root_str, "al_s"),
            "al_s",
        );
        lib.fake.sees("alice", "s_shared");
        lib.fake.sees("bob", "s_shared");
        lib.fake.stars("bob", "s_shared");

        // Teach library root 1 and 2
        teach_library(&lib.db, 1, &lib.root.join("music").to_string_lossy()).await;
        teach_library(&lib.db, 2, &shared_root_str).await;

        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        let (app, token, _) = track_app(&lib, Some(&navi), true).await;

        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s_shared"], "target_folder_id": "alice-music"}),
            )
            .await,
        );
        assert_eq!(doc["ok"], false, "{doc}");
        assert_eq!(
            doc["results"][0]["error"]["code"],
            "delete_blocked_by_other_user"
        );
        assert!(!doc.to_string().contains("bob"));
        assert!(track_path.exists());
    }

    #[tokio::test]
    async fn partial_album_cross_library_move_warns_offers_whole_album_and_restores_album_marks() {
        let lib = Library::new().await;
        // Personal library = 1 ({root}/music), Shared library = 2 ({root}/shared_lib)
        lib.folder("alice", "shared-music", "shared_lib", true)
            .await;
        let shared_root = lib.root.join("shared_lib");
        std::fs::create_dir_all(&shared_root).unwrap();
        let music_root_str = lib.root.join("music").to_string_lossy().into_owned();
        let shared_root_str = shared_root.to_string_lossy().into_owned();
        teach_library(&lib.db, 1, &music_root_str).await;
        teach_library(&lib.db, 2, &shared_root_str).await;

        // Add a second track s2 to album al1 so moving only s1 splits the album!
        lib.fake.file(
            make_song("s2", "alice/A/y.flac", "Silent Night", 5, 1, &music_root_str, "al1"),
            "al1",
        );
        lib.write("alice/A/y.flac", b"audio");
        lib.fake.sees("alice", "s2");
        lib.fake.rates_album("alice", "al1", 5);
        lib.fake.stars_album("alice", "al1");

        let navi = NavidromeStub::start();
        navi.answer("/rest/getUser", &[&user_role(true)]);
        navi.answer("/api/song", &["[]"]);
        navi.answer_when(
            "/api/song",
            "id=s1",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s1",
                    "A/x.flac",
                    "O Holy Night",
                    5,
                    2,
                    &shared_root_str,
                    "al1_new",
                )
            ),
        );
        // Simulate Navidrome ReassignAnnotation having cleared alice's rating/star on al1
        navi.answer(
            "/rest/getAlbum",
            &[&subsonic_ok(
                r#""album":{"id":"al1","name":"Winter Songs","songCount":1,"userRating":0,"song":[]}"#,
            )],
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );

        let (app, token, _) = track_app(&lib, Some(&navi), true).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "shared-music"}),
            )
            .await,
        );
        assert_eq!(doc["ok"], true, "{doc}");
        let entry = &doc["results"][0];
        assert_eq!(entry["crosses_navidrome_library"], true);
        assert_eq!(entry["album_split"]["album_id"], "al1");
        assert_eq!(entry["album_split"]["moving_tracks"], 1);
        assert_eq!(entry["album_split"]["total_tracks"], 2);
        assert_eq!(entry["album_annotation_restored"], true);
        assert!(entry["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("will split the album")));
        // Next offers moving the whole album via album_id
        assert!(doc["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["tool"] == "move_tracks" && n["args"]["album_id"] == "al1"));
        // Verify setRating and star were called on Navidrome to restore al1's marks
        let ratings = navi.posted("/rest/setRating");
        assert!(
            ratings
                .iter()
                .any(|p| p.contains("id=al1") && p.contains("rating=5")),
            "expected setRating for al1=5, got {ratings:?}"
        );
        let stars = navi.posted("/rest/star");
        assert!(
            stars.iter().any(|p| p.contains("albumId=al1")),
            "expected star for albumId=al1, got {stars:?}"
        );
    }

    #[tokio::test]
    async fn move_tracks_whole_album_moves_all_tracks_without_split_warning() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "shared_lib", true)
            .await;
        let shared_root = lib.root.join("shared_lib");
        std::fs::create_dir_all(&shared_root).unwrap();
        let music_root_str = lib.root.join("music").to_string_lossy().into_owned();
        let shared_root_str = shared_root.to_string_lossy().into_owned();
        teach_library(&lib.db, 1, &music_root_str).await;
        teach_library(&lib.db, 2, &shared_root_str).await;

        lib.fake.file(
            make_song("s2", "alice/A/y.flac", "Silent Night", 5, 1, &music_root_str, "al1"),
            "al1",
        );
        lib.write("alice/A/y.flac", b"audio");
        lib.fake.sees("alice", "s2");

        let navi = NavidromeStub::start();
        navi.answer("/rest/getUser", &[&user_role(true)]);
        navi.answer("/api/song", &["[]"]);
        navi.answer(
            "/rest/getAlbum",
            &[&subsonic_ok(
                r#""album":{"id":"al1","name":"Winter Songs","songCount":2,"song":[{"id":"s1","title":"O Holy Night"},{"id":"s2","title":"Silent Night"}]}"#,
            )],
        );
        navi.answer_when(
            "/api/song",
            "id=s1",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s1",
                    "A/x.flac",
                    "O Holy Night",
                    5,
                    2,
                    &shared_root_str,
                    "al1_new",
                )
            ),
        );
        navi.answer_when(
            "/api/song",
            "id=s2",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s2",
                    "A/y.flac",
                    "Silent Night",
                    5,
                    2,
                    &shared_root_str,
                    "al1_new",
                )
            ),
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );

        let (app, token, _) = track_app(&lib, Some(&navi), true).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"album_id": "al1", "target_folder_id": "shared-music"}),
            )
            .await,
        );
        assert_eq!(doc["ok"], true, "{doc}");
        assert_eq!(doc["album_id"], "al1");
        assert_eq!(doc["results"].as_array().unwrap().len(), 2);
        assert!(doc["results"][0]["album_split"].is_null());
        assert!(doc["results"][1]["album_split"].is_null());
        assert!(shared_root.join("A/x.flac").exists());
        assert!(shared_root.join("A/y.flac").exists());
    }

    #[tokio::test]
    async fn move_tracks_refuses_pending_discovery_track_and_offers_promote_with_target() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();
        let staged = lib.root.join("music/alice/Discovery/Balanced/d1.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        filetime_set(&staged, mtime());
        let music_root_str = lib.root.join("music").to_string_lossy().into_owned();
        lib.fake.file(
            make_song(
                "s_disc",
                "alice/Discovery/Balanced/d1.flac",
                "New Gem",
                5,
                1,
                &music_root_str,
                "al_d",
            ),
            "al_d",
        );
        lib.fake.sees("alice", "s_disc");
        lib.pending_track("dt1", "alice-music", "Balanced", &staged, Some("s_disc"))
            .await;

        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        let (app, token, _) = track_app(&lib, Some(&navi), true).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s_disc"], "target_folder_id": "shared-music"}),
            )
            .await,
        );
        let err = &doc["results"][0]["error"];
        assert_eq!(err["code"], "invalid_input", "{doc}");
        assert_eq!(err["facts"]["discovery_track_id"], "dt1");
        assert_eq!(
            err["next"][0],
            json!({"tool": "promote_discovery_track", "args": {"track_id": "dt1", "target_folder_id": "shared-music"}})
        );
    }

    #[tokio::test]
    async fn retag_tracks_direct_rematch_and_relocate_two_scan_dance() {
        let lib = Library::new().await;
        lib.write("alice/A/x.lrc", b"lyrics");
        let music_root_str = lib.root.join("music").to_string_lossy().into_owned();
        let navi = NavidromeStub::start();
        navi.answer("/rest/getUser", &[&user_role(true)]);
        navi.answer_when(
            "/api/song",
            "id=s1",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s1",
                    "alice/A/x.flac",
                    "O Holy Night",
                    5,
                    1,
                    &music_root_str,
                    "al1",
                )
            ),
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                // Call 1 (direct): idle, baseline, poll
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
                // Call 3 (relocate): idle, scan #1 baseline, scan #1 poll, scan #2 baseline, scan #2 poll
                &scan_status(false, "2026-11-14T21:00:05Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
                &scan_status(false, "2026-11-14T21:00:10Z"),
                &scan_status(false, "2026-11-14T21:00:10Z"),
                &scan_status(false, "2026-11-14T21:00:15Z"),
            ],
        );

        let (app, token, effects) = track_app(&lib, Some(&navi), true).await;

        // 1. Direct tag edit in place
        let direct = document(
            &call_tool(
                app.clone(),
                &token,
                "retag_tracks",
                json!({"song_ids": ["s1"], "tags": {"genre": "Ambient", "year": 2024}}),
            )
            .await,
        );
        assert_eq!(direct["ok"], true, "{direct}");
        assert_eq!(direct["relocated"], false);
        assert_eq!(direct["results"][0]["status"], "retagged");
        assert_eq!(direct["results"][0]["beets"]["genre"], "Ambient");
        assert_eq!(direct["results"][0]["beets"]["year"], 2024);
        assert_eq!(effects.importer().tag_edits().len(), 1);
        assert!(lib.root.join("music/alice/A/x.flac").exists());

        // 2. Rematch dry_run returns candidates without writing
        effects.importer().script(ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::NoMatch { candidates: Some(1) }),
            choices: Some(track_choices(vec![candidate("MusicBrainz", "mb-123")])),
            ..no_match(1)
        });
        let preview = document(
            &call_tool(
                app.clone(),
                &token,
                "retag_tracks",
                json!({"song_ids": ["s1"], "rematch": {"mode": "auto"}, "dry_run": true}),
            )
            .await,
        );
        assert_eq!(preview["ok"], true, "{preview}");
        assert_eq!(preview["results"][0]["status"], "would_retag");
        assert_eq!(
            preview["results"][0]["candidates"]["candidates"][0]["id"],
            "mb-123"
        );
        assert_eq!(effects.importer().rematches().len(), 1);
        assert!(effects.importer().rematches()[0].2, "dry_run must pass pretend=true");

        // 3. Relocate with admin runs 2-scan dance and moves sidecar .lrc
        let relocated_dest = std::fs::canonicalize(lib.root.join("music/alice"))
            .unwrap()
            .join("Marrow Choir/Winter Songs/03 - O Holy Night.flac");
        std::fs::create_dir_all(relocated_dest.parent().unwrap()).unwrap();
        std::fs::write(&relocated_dest, b"audio").unwrap();
        effects.importer().script_relocate(Relocation {
            now: vec![(
                1,
                PathBuf::from("Marrow Choir/Winter Songs/03 - O Holy Night.flac"),
            )],
            failure: None,
        });
        let reloc = document(
            &call_tool(
                app,
                &token,
                "retag_tracks",
                json!({"song_ids": ["s1"], "tags": {"album": "Winter Songs (Deluxe)"}, "relocate": true}),
            )
            .await,
        );
        assert_eq!(reloc["ok"], true, "{reloc}");
        assert_eq!(reloc["relocated"], true);
        assert_eq!(reloc["results"][0]["status"], "retagged_relocated");
        assert_eq!(effects.importer().relocate_files().len(), 1);
        assert!(
            relocated_dest.with_extension("lrc").exists(),
            "sidecar .lrc should follow the relocated file"
        );

        // 4. Relocate refused before writing tags when no linked account is a Navidrome admin
        sqlx::query("UPDATE users SET navidrome_token = NULL WHERE id = 'bob'")
            .execute(&lib.db)
            .await
            .unwrap();
        let navi_no_admin = NavidromeStub::start();
        navi_no_admin.answer("/rest/getUser", &[&user_role(false)]);
        navi_no_admin.answer(
            "/rest/getScanStatus",
            &[&scan_status(false, "2026-11-14T21:00:00Z")],
        );
        navi_no_admin.answer("/rest/startScan", &[&subsonic_failed(50, "not authorized")]);
        let (app_no_admin, token_no_admin, effects_no_admin) =
            track_app(&lib, Some(&navi_no_admin), true).await;
        let refused = document(
            &call_tool(
                app_no_admin,
                &token_no_admin,
                "retag_tracks",
                json!({"song_ids": ["s1"], "tags": {"genre": "Folk"}, "relocate": true}),
            )
            .await,
        );
        assert_eq!(refused["error"]["code"], "retag_relocate_needs_scan", "{refused}");
        assert!(
            effects_no_admin.importer().tag_edits().is_empty(),
            "no tags should be modified when relocate is refused up front"
        );
    }

    fn filetime_set(path: &std::path::Path, dt: chrono::DateTime<chrono::Utc>) {
        let ts = std::time::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(ts).unwrap();
    }

    #[tokio::test]
    async fn move_tracks_without_target_folder_returns_folder_required() {
        let lib = Library::new().await;
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();
        let (app, token, _) = track_app(&lib, None, true).await;

        let doc = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "dry_run": true}),
            )
            .await,
        );
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["error"]["code"], "folder_required", "{doc}");
        assert_eq!(doc["error"]["retry"], "needs_decision");
        let folders = doc["error"]["facts"]["folders"].as_array().unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.iter().any(|f| f["folder_id"] == "shared-music"));
    }

    #[tokio::test]
    async fn admin_with_allow_admin_can_inspect_and_move_other_users_track_into_shared_folder() {
        let lib = Library::new().await;
        // Make alice an admin and give her a shared folder; bob has a private track s-bob.
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();
        lib.folder("alice", "shared-music", "music/shared", true)
            .await;
        std::fs::create_dir_all(lib.root.join("music/shared")).unwrap();
        lib.song("s-bob", "bob/B/y.flac", b"audio");
        let bob_file = lib.root.join("music/bob/B/y.flac");
        // Hide s-bob from alice in Navidrome so admin fallback to bob's linked account is tested!
        lib.fake.unsee("alice", "s-bob");

        let navi = NavidromeStub::start();
        let lib_path = lib.root.join("music").to_string_lossy().into_owned();
        navi.answer("/rest/getUser", &[&user_role(true)]);
        navi.answer("/api/song", &["[]"]);
        navi.answer_when(
            "/api/song",
            "id=s-bob",
            200,
            &format!(
                "[{}]",
                native_song_json(
                    "s-bob",
                    "shared/B/y.flac",
                    "Bob Track",
                    5,
                    1,
                    &lib_path,
                    "al-bob",
                )
            ),
        );
        navi.answer(
            "/rest/getScanStatus",
            &[
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:00Z"),
                &scan_status(false, "2026-11-14T21:00:05Z"),
            ],
        );

        let (app, token, _) = track_app(&lib, Some(&navi), true).await;

        // Without allow_admin, alice cannot move into bob's folder or move bob's track.
        let refused = document(
            &call_tool(
                app.clone(),
                &token,
                "move_tracks",
                json!({"song_ids": ["s1"], "target_folder_id": "bob-music", "dry_run": true}),
            )
            .await,
        );
        assert_eq!(refused["error"]["code"], "permission_not_granted", "{refused}");
        assert!(refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Admin library access (all users' folders)"));

        // Enable allow_admin on alice's grant.
        sqlx::query("UPDATE oauth_grants SET allow_admin = 1 WHERE user_id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();

        // inspect_tracks finds s-bob via bob's linked account and shows bob's folder owner.
        let inspected = document(
            &call_tool(
                app.clone(),
                &token,
                "inspect_tracks",
                json!({"song_ids": ["s-bob"]}),
            )
            .await,
        );
        assert_eq!(inspected["ok"], true, "{inspected}");
        assert_eq!(inspected["results"][0]["folder"]["folder_id"], "bob-music");
        assert_eq!(inspected["results"][0]["folder"]["owner"], "bob");
        assert_eq!(inspected["results"][0]["folder"]["is_own"], false);

        // Moving bob's private track into a shared folder expands visibility and succeeds!
        let moved = document(
            &call_tool(
                app,
                &token,
                "move_tracks",
                json!({"song_ids": ["s-bob"], "target_folder_id": "shared-music", "dry_run": false}),
            )
            .await,
        );
        assert_eq!(moved["ok"], true, "{moved}");
        assert_eq!(moved["results"][0]["status"], "moved");
        assert_eq!(moved["results"][0]["source_folder"]["owner"], "bob");
        assert!(!bob_file.exists());
        assert!(lib.root.join("music/shared/B/y.flac").exists());
    }
}
