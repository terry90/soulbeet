//! `ensure_in_library`: says which tracks the library has and starts a job for each other item,
//! all in one batch; `download_source`: one candidate a search found becomes a job's source.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, Stage, TEXT_MAX_CHARS};

use std::sync::Arc;

use soulbeet::navidrome::NavidromeClient;

use super::args::{repeatable, Args};
use super::contract::{
    error_object, failed, invalid_value, items, items_text, too_large_to_repeat, upstream_error,
    ItemResult, Items, NextCall, ToolError,
};
use super::jobs::JobTool;
use super::keys::{keyed, request_key, Keyed};
use super::library::navidrome_of;
use super::paging::{self, Keep, RESULT_MAX_CHARS};
use super::playlists::{
    name_arg, playlist_fields, refusal_error, ADD_PLAYLIST_TRACKS, ADD_PLAYLIST_TRACKS_ARGS,
    A_NAME, COMMENT_MAX_CHARS, CREATE_PLAYLIST, CREATE_PLAYLIST_ARGS, GET_PLAYLIST_ARGS,
};
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
use crate::core::jobs::retry::{
    download_source as core_download_source, RetryAnswer, SourceTarget,
};
use crate::core::jobs::runner::{Acquire, MCP_DOWNLOADS_PER_USER};
use crate::core::jobs::{ensure, CallRefused, EnsureItem, Wanted};
use crate::core::playlists::fill::{begin, order_of, prepare, PlaylistTarget};
use crate::core::playlists::{NewPlaylist, PlaylistRefusal, Written};
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
    "playlist",
    "request_key",
];

/// The fields of `playlist`, one of them given.
const PLAYLIST_FIELDS: &[&str] = &["playlist_id", "create"];

/// The fields of `playlist.create`, read as create_playlist reads them.
const CREATE_FIELDS: &[&str] = &["name", "comment", "public", "allow_duplicate_name"];

/// The most items one call takes.
const MAX_ITEMS: usize = 50;

pub(crate) const ENSURE_IN_LIBRARY: ToolSpec = ToolSpec {
    name: TOOL,
    title: "Get music into the library",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("The main way to get music: for up to {MAX_ITEMS} track or album references, says which tracks the library already has (by recording MBID or exact artist and title; fuzzy matches are listed, not accepted) and starts a job for each other item, all in one batch. Returns at once with batch_id; follow it with get_batch. Jobs search the download backend one search at a time, pick the best source at min_score (default {AUTO_SELECT_SCORE_THRESHOLD}) whose source has not failed 3 times today, or wait for download_source with pick review, download at most {MCP_DOWNLOADS_PER_USER} at a time per user, fail over to other sources (failover off, or max_attempts, default {MAX_SOURCES_PER_TRACK}), import with beets (never replacing a library file: a duplicate ends import_already_in_library with the library's song ids) and wait until Navidrome lists the file. A user without Navidrome gets a job for every item (library_checked false). A user has at most {MCP_UNFINISHED_JOBS_PER_USER} unfinished jobs from connected apps: an item past that is refused rate_limited. A 40 track list with 28 downloads takes 20 to 60 minutes. Album references need release_mbid. An answer that would pass 60,000 characters leaves out, from the last refused item on, the reference its facts echo, then its next calls, then its cause.raw, then cuts its message, facts.omitted naming what. Failure codes: {common}, folder_required, unknown_handle (folder_id), importer_broken, backend_not_configured, already_running (per item: the job_id that already wants this recording; or a call with this request_key still runs), {NAVIDROME_CODES} (from the library lookup, per item or for the call), partial. With playlist ({{playlist_id}} of one of this user's static playlists, or {{create: {{name, comment?, public?, allow_duplicate_name?}}}}), the playlist gets the owned tracks now in the order given and each other track at its place as its job finishes, whether or not this chat stays open; get_batch shows how far it is. A playlist refused before any job is made fails the call with playlist_not_found, playlist_not_owner, playlist_not_editable or playlist_name_taken; once the jobs are made, playlist.error says what stopped the playlist: those codes, unknown_handle (an owned song whose file is missing), playlist_tracks_not_written, playlist_not_written (a comment or public flag not shown) or one of the Navidrome codes.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["items"], "properties": {"items": {"type": "array", "minItems": 1, "maxItems": 50, "items": "#,
        reference_schema!(),
        r#"}, "folder_id": {"type": "string", "minLength": 1}, "pick": {"enum": ["auto", "review"]}, "min_score": {"type": "number", "minimum": 0, "maximum": 1}, "#,
        filters_schema!(),
        r#", "failover": {"enum": ["auto", "off"]}, "max_attempts": {"type": "integer", "minimum": 1, "maximum": 10}, "label": {"type": "string", "minLength": 1}, "playlist": {"type": "object", "additionalProperties": false, "minProperties": 1, "maxProperties": 1, "properties": {"playlist_id": {"type": "string", "minLength": 1}, "create": {"type": "object", "additionalProperties": false, "required": ["name"], "properties": {"name": {"type": "string", "minLength": 1, "maxLength": 255}, "comment": {"type": "string", "maxLength": 2000}, "public": {"type": "boolean"}, "allow_duplicate_name": {"type": "boolean"}}}}}, "#,
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
    /// The playlist the batch fills.
    playlist: Option<PlaylistTarget>,
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
    // The playlist is written through the caller's own Navidrome.
    let navi = match &read.playlist {
        Some(_) => match navidrome_of(state, caller, Stage::Playlist, &args).await {
            Ok(navi) => Some(navi),
            Err(e) => return failed(e),
        },
        None => None,
    };
    let call = json!({"items": read.items.iter().map(reference_value).collect::<Vec<_>>(),
        "folder_id": read.folder_id, "label": read.label, "options": read.options,
        "playlist": read.playlist.as_ref().map(target_value)});
    let (acquire, navidrome, ctx) = (
        state.acquire.clone(),
        state.navidrome.clone(),
        caller.ctx.clone(),
    );
    let work = async move { run(&acquire, &*navidrome, &ctx, read, navi, &args).await };
    keyed(&state.acquire, caller, TOOL, key, &call, work).await
}

/// `playlist` as read: exactly one of playlist_id and create.
fn read_playlist(args: &Args) -> Result<Option<PlaylistTarget>, ToolError> {
    let Some(playlist) = args.opt_object("playlist", PLAYLIST_FIELDS)? else {
        return Ok(None);
    };
    let playlist_id = playlist.opt_string("playlist_id")?;
    let create = playlist.opt_object("create", CREATE_FIELDS)?;
    let target = match (playlist_id, create) {
        (Some(_), Some(_)) => {
            let expected = "left out when playlist_id is given";
            return Err(invalid_value(TOOL, "playlist.create", expected));
        }
        (None, None) => {
            let expected = "given when create is not";
            return Err(invalid_value(TOOL, "playlist.playlist_id", expected));
        }
        (Some(id), None) => PlaylistTarget::Existing(id),
        (None, Some(create)) => PlaylistTarget::Create(NewPlaylist {
            name: name_arg(&create)?.ok_or_else(|| create.refuse("name", A_NAME))?,
            comment: create.opt_text("comment", COMMENT_MAX_CHARS)?,
            public: create.opt_bool("public")?,
            song_ids: vec![],
            allow_duplicate_name: create.opt_bool("allow_duplicate_name")?.unwrap_or(false),
        }),
    };
    Ok(Some(target))
}

/// `target` as the call it was read from names it, for the request key's comparison.
fn target_value(target: &PlaylistTarget) -> Value {
    match target {
        PlaylistTarget::Existing(id) => json!({ "playlist_id": id }),
        PlaylistTarget::Create(new) => json!({"create": {"name": new.name, "comment": new.comment,
            "public": new.public, "allow_duplicate_name": new.allow_duplicate_name}}),
    }
}

/// The id of the caller's playlist `target` names; None for one to create.
fn target_id(target: &PlaylistTarget) -> Option<&str> {
    match target {
        PlaylistTarget::Existing(id) => Some(id),
        PlaylistTarget::Create(_) => None,
    }
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
    let playlist = read_playlist(args)?;
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
        playlist,
        call: args.as_value(),
    };
    Ok((read, key))
}

/// The call: a call-level refusal, or each item's result with the batch, whether the library
/// was looked in, the call that follows the batch, and the playlist it fills when `args` name one
/// (`navi` the caller's Navidrome). A playlist refused before `ensure` refuses the call. A request
/// key keeps the answer of a call that made a batch, wrote the playlist or tried to, or refused
/// no item; a refusal of the call, or of an item when nothing was written, and the same call can
/// run again.
async fn run(
    acquire: &Arc<Acquire>,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    read: Ensure,
    navi: Option<Arc<NavidromeClient>>,
    args: &Args,
) -> Keyed {
    let Ensure {
        items: wanted,
        given,
        folder_id,
        label,
        options,
        playlist,
        call,
    } = read;
    let playlist = playlist.zip(navi);
    // Held until begin ran: no other playlist write of Soulful's comes between the checks and it.
    let held = match &playlist {
        Some((target, navi)) => match prepare(navi, ctx, target).await {
            Ok(held) => held,
            Err(r) => return Keyed::Released(failed(playlist_refused(args, target, &r))),
        },
        None => vec![],
    };
    let folder_id = folder_id.as_deref();
    let for_playlist = playlist.is_some();
    let mut ensured = match ensure(
        acquire,
        navidrome,
        ctx,
        folder_id,
        label,
        wanted,
        options,
        for_playlist,
    )
    .await
    {
        Ok(ensured) => ensured,
        Err(refused) => return Keyed::Released(failed(call_refused(refused, &call))),
    };
    let written = match &playlist {
        Some((target, navi)) => {
            let batch = &mut ensured.batch_id;
            let written = begin(&acquire.ledger, navi, ctx, batch, target, &ensured.items).await;
            Some((target, written))
        }
        None => None,
    };
    drop(held);
    let keep = ensured.batch_id.is_some()
        || match &written {
            None => !(ensured.items.iter()).any(|item| matches!(item, EnsureItem::Refused(..))),
            Some((_, Ok(_))) => true,
            Some((_, Err(r))) => r.after_write(),
        };
    let playlist_field = (written.as_ref())
        .map(|(target, written)| playlist_answer(target, &ensured.items, written));
    let (playlist_written, playlist_error) = match playlist_field {
        Some(Ok(written)) => (Some(written), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
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
    let mut fields = Map::from_iter([
        ("batch_id".to_string(), json!(ensured.batch_id)),
        (
            "library_checked".to_string(),
            json!(ensured.library_checked),
        ),
        ("next".to_string(), json!(next)),
    ]);
    if let Some(playlist) = playlist_written {
        fields.insert("playlist".to_string(), playlist);
    }
    let mut answered = Items {
        stage: Stage::Input,
        noun: "items",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    };
    bounded(&mut answered, playlist_error);
    let result = items(answered);
    if keep {
        Keyed::Kept(result)
    } else {
        Keyed::Released(result)
    }
}

/// A playlist refused before any job was made: refusal_error of this call, a taken name offering
/// the call with `playlist.create.allow_duplicate_name` set.
fn playlist_refused(args: &Args, target: &PlaylistTarget, r: &PlaylistRefusal) -> ToolError {
    let mut e = refusal_error(args, target_id(target), r);
    if let PlaylistRefusal::NameTaken { .. } = r {
        let mut duplicate = args.as_value();
        duplicate["playlist"]["create"]["allow_duplicate_name"] = json!(true);
        e.failure.message = r.failure().message;
        e.next = repeatable(TOOL, duplicate).into_iter().collect();
        if e.next.is_empty() {
            e.failure.message = too_large_to_repeat(&e.failure.message);
        }
    }
    e
}

/// The answer's `playlist`: the playlist as written, or, when `begin` failed, its error with the
/// playlist call `begin` made offered on its own (the call itself would start the jobs again).
fn playlist_answer(
    target: &PlaylistTarget,
    items: &[EnsureItem],
    written: &Result<Written, PlaylistRefusal>,
) -> Result<Value, ToolError> {
    let r = match written {
        Ok(w) => {
            let mut fields = Map::from_iter([("ok".to_string(), json!(true))]);
            fields.extend(playlist_fields(w));
            return Ok(Value::Object(fields));
        }
        Err(r) => r,
    };
    let song_ids: Vec<String> = (order_of(items).into_iter())
        .flat_map(|entry| entry.song_ids)
        .collect();
    let (tool, known, made) = match target {
        PlaylistTarget::Create(new) => {
            let mut made = json!({"name": new.name, "song_ids": song_ids,
                "allow_duplicate_name": new.allow_duplicate_name});
            if let Some(comment) = &new.comment {
                made["comment"] = json!(comment);
            }
            if let Some(public) = new.public {
                made["public"] = json!(public);
            }
            (CREATE_PLAYLIST.name, CREATE_PLAYLIST_ARGS, made)
        }
        // No song was added: the playlist was only read.
        PlaylistTarget::Existing(id) if song_ids.is_empty() => (
            "get_playlist",
            GET_PLAYLIST_ARGS,
            json!({ "playlist_id": id }),
        ),
        PlaylistTarget::Existing(id) => (
            ADD_PLAYLIST_TRACKS.name,
            ADD_PLAYLIST_TRACKS_ARGS,
            json!({"playlist_id": id, "song_ids": song_ids}),
        ),
    };
    let made = Args::new(tool, made.as_object().cloned(), known);
    Err(refusal_error(&made, target_id(target), r))
}

/// The answer's `playlist` for `begin`'s error `e`.
fn playlist_failed(e: &ToolError) -> Value {
    json!({"ok": false, "error": error_object(e)})
}

/// The reference fields a refused item's facts echo.
const ECHOED: [&str; 7] = [
    "kind",
    "artist",
    "title",
    "album",
    "recording_mbid",
    "release_mbid",
    "duration_s",
];

/// `answered` within RESULT_MAX_CHARS (D4), with `playlist`, the error of its playlist when
/// `begin` failed, as its `playlist`. Past it, the playlist's error, then refused items, the last
/// first, leave out the reference their facts echo (the caller has it by its index), then their
/// calls in `next`, then their failure's `cause.raw`, then cut their failure's message to
/// TEXT_MAX_CHARS as sent; each one's `facts.omitted` names what it left out. Ids (a job that
/// already wants the recording) stay. What is left of a refused item is Soulful's own fields, its
/// ids and that message, so MAX_ITEMS of them keep within the bound whatever the failures held;
/// past that, the playlist's error leaves out whole ids of its longest id list, again and again.
fn bounded(answered: &mut Items, mut playlist: Option<ToolError>) {
    fn shown(answered: &mut Items, playlist: &Option<ToolError>) {
        if let Some(e) = playlist {
            answered
                .fields
                .insert("playlist".into(), playlist_failed(e));
        }
    }
    let over = |answered: &Items| items_text(answered).chars().count() > RESULT_MAX_CHARS;
    shown(answered, &playlist);
    for step in ["reference", "next", "cause.raw", "message"] {
        if let Some(e) = playlist.as_mut().filter(|_| over(answered)) {
            if leave_out(e, step) {
                shown(answered, &playlist);
            }
        }
        for at in (0..answered.results.len()).rev() {
            if !over(answered) {
                return;
            }
            if let ItemResult::Failed(error) = &mut answered.results[at] {
                leave_out(error, step);
            }
        }
    }
    while let Some(e) = playlist.as_mut().filter(|_| over(answered)) {
        let longest = (e.facts.iter())
            .filter(|(_, value)| value.as_array().is_some_and(|list| !list.is_empty()))
            .max_by_key(|(_, value)| value.to_string().chars().count())
            .map(|(key, _)| key.clone());
        let Some(list) = longest else {
            return;
        };
        let over_by = items_text(answered).chars().count() - RESULT_MAX_CHARS;
        paging::leave_out(&mut e.facts, &list, Keep::First, over_by);
        shown(answered, &playlist);
    }
}

/// `step` of `bounded` on `error`: whether it left something out, which its `facts.omitted`
/// then names.
fn leave_out(error: &mut ToolError, step: &'static str) -> bool {
    let left_out = match step {
        "reference" => {
            let before = error.facts.len();
            error
                .facts
                .retain(|name, _| !ECHOED.contains(&name.as_str()));
            error.facts.len() < before
        }
        "next" => !std::mem::take(&mut error.next).is_empty(),
        "cause.raw" => (error.failure.cause.as_mut())
            .is_some_and(|cause| !std::mem::take(&mut cause.raw).is_empty()),
        _ => {
            let message = &mut error.failure.message;
            let cut = cut_json(message.clone(), TEXT_MAX_CHARS);
            let changed = cut != *message;
            *message = cut;
            changed
        }
    };
    if left_out {
        let omitted = error.facts.entry("omitted").or_insert_with(|| json!([]));
        if let Value::Array(omitted) = omitted {
            omitted.push(json!(step));
        }
    }
    left_out
}

/// One item's entry. A refused one offers what follows its failure: the job that already wants
/// the recording, the user's batches when they hold the most jobs allowed, else this call for that
/// item alone when the same source can answer later (D13).
fn item_result(item: EnsureItem, given: &Value, call: &Value) -> ItemResult {
    let fields = match item {
        EnsureItem::InLibrary { song_ids } => json!({"status": "in_library", "song_ids": song_ids}),
        EnsureItem::Created { job_id, fuzzy } => json!({"status": "job_created", "job_id": job_id,
            "state": JobState::Searching.as_str(), "fuzzy_matches": fuzzy}),
        EnsureItem::AlreadyRunning {
            job_id,
            failure,
            facts,
        } => {
            let next = vec![NextCall::new(
                "get_jobs",
                json!({"job_ids": [job_id], "detail": "full"}),
            )];
            return ItemResult::Failed(ToolError {
                failure,
                facts,
                tried: vec![],
                next,
            });
        }
        EnsureItem::Refused(failure, facts) => {
            let next = match failure.code {
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

const DOWNLOAD_SOURCE: &str = "download_source";

const DOWNLOAD_SOURCE_ARGS: &[&str] = &[
    "candidate_id",
    "job_id",
    "reference",
    "folder_id",
    "request_key",
];

pub(crate) const DOWNLOAD_SOURCE_TOOL: ToolSpec = ToolSpec {
    name: DOWNLOAD_SOURCE,
    title: "Download a chosen source",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Downloads one candidate of a find_sources or job search: for job_id (a job waiting for a source choice, failed or cancelled) it becomes the job's source; with a reference (and folder_id, else the default folder) it starts a new job in a new batch. The candidate's search must be of the same kind as the job or reference (track or album). Jobs started in Soulful's web page take their source there. Answers the job's full document, as get_jobs gives it. Failure codes: {common}, unknown_handle (job, candidate or folder), expired_handle, wrong_job_state, folder_required, importer_broken, backend_not_configured, already_running (a job already wants this recording; or a call with this request_key still runs), metadata_not_found, metadata_unavailable (album tracklist).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["candidate_id"], "properties": {"candidate_id": {"type": "string", "minLength": 1}, "job_id": {"type": "string", "minLength": 1}, "reference": "#,
        reference_schema!(),
        r#", "folder_id": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(download_source(state, caller, args)),
};

/// A download_source call as read: the candidate, whose job it becomes, the key.
fn read_download(args: &Args) -> Result<(String, SourceTarget, Option<String>), ToolError> {
    let candidate_id = args.string("candidate_id")?;
    let target = match args.opt_string("job_id")? {
        Some(job_id) => {
            args.absent("reference", "absent when job_id is given")?;
            args.absent("folder_id", "absent when job_id is given")?;
            SourceTarget::Job(job_id)
        }
        None if args.has("reference") => SourceTarget::New {
            wanted: wanted(&args.object("reference", REFERENCE_FIELDS)?)?,
            folder_id: args.opt_string("folder_id")?,
        },
        None => {
            let expected = "a job id, or a reference with folder_id";
            return Err(invalid_value(DOWNLOAD_SOURCE, "job_id", expected));
        }
    };
    let key = request_key(args)?;
    args.finish()?;
    Ok((candidate_id, target, key))
}

/// `download_source`: a candidate becomes a job's source, once per request key.
pub(crate) async fn download_source(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(DOWNLOAD_SOURCE, args, DOWNLOAD_SOURCE_ARGS);
    let (candidate_id, target, key) = match read_download(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = match &target {
        SourceTarget::Job(job_id) => json!({"candidate_id": candidate_id, "job_id": job_id}),
        SourceTarget::New { wanted, folder_id } => json!({"candidate_id": candidate_id,
            "reference": reference_value(wanted), "folder_id": folder_id}),
    };
    let on = JobTool::of(DOWNLOAD_SOURCE, state, caller);
    let (again, given) = (args.same_call(), args.as_value());
    let work = async move {
        let job = match &target {
            SourceTarget::Job(id) => Some(id.clone()),
            SourceTarget::New { .. } => None,
        };
        let ctx = &on.caller.ctx;
        match core_download_source(&on.acquire, ctx, target, &candidate_id).await {
            // No default folder: the call again with each of the user's folders.
            Ok(RetryAnswer::Unavailable(failure, facts)) if facts.contains_key("folders") => {
                Keyed::Released(failed(with_folders(failure, facts, &given)))
            }
            answered => on.answer(job.as_deref(), answered, again).await,
        }
    };
    keyed(&state.acquire, caller, DOWNLOAD_SOURCE, key, &call, work).await
}

/// folder_required with the user's `folders` in `facts`: the call `given` again with each one's
/// folder_id.
fn with_folders(mut failure: Failure, facts: Map<String, Value>, given: &Value) -> ToolError {
    let ids = (facts["folders"].as_array().into_iter().flatten()).map(|f| f["folder_id"].clone());
    let next: Vec<NextCall> = ids
        .filter_map(|folder_id| {
            let mut call = given.clone();
            call["folder_id"] = folder_id;
            repeatable(DOWNLOAD_SOURCE, call)
        })
        .collect();
    if next.is_empty() {
        failure.message = too_large_to_repeat(&failure.message);
    }
    ToolError {
        failure,
        facts,
        tried: vec![],
        next,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::{json, Value};

    use std::time::Duration;

    use super::{Failure, FailureCode, Stage, RESULT_MAX_CHARS};
    use shared::download::{DownloadQuery, SearchState, SearchStats};

    use crate::auth::UserId;
    use crate::connection::test_util::{
        add_folder, app_of, app_with_acquire, app_with_navidrome, call_tool, count, document,
        jobs_app, set_default_folder,
    };
    use crate::core::acquire::{Acquisition, NewBatch};
    use crate::core::folder_of;
    use crate::core::jobs::runner::{Acquire, TestEffects};
    use crate::core::jobs::test_support::{
        batch_of_searches, choosing_track_job, settle_done, start_downloading, view_of,
    };
    use crate::core::playlists::fill::round;
    use crate::import_queue::test_support::{broken, ready};
    use crate::ledger::MCP_UNFINISHED_JOBS_PER_USER;
    use crate::ledger::{NewSearch, StoredSpec};
    use crate::server_fns::download::test_support::{group, item, search_result, track};
    use crate::test_navidrome::{
        native_playlist, native_song, native_songs, playlist_body, playlist_rows, playlists_body,
        NavidromeFor, NavidromeStub, ADVENT, BAD_GATEWAY, HOLY, SILENT,
    };

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
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+Silent+Night",
            502,
            BAD_GATEWAY,
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
    async fn fifty_refused_items_keep_their_answer_within_the_cap() {
        let (db, acq, _, token) = app_with_acquire().await;
        let navi = NavidromeStub::start();
        // nginx's 502 page for every lookup.
        navi.answer_when("/api/song", "title=", 502, BAD_GATEWAY);
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let long = |c: char, i: usize| format!("{} {i}", c.to_string().repeat(480));
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"artist": long('a', i), "title": long('t', i), "album": long('l', i)}))
            .collect();
        let result = call_tool(app, &token, "ensure_in_library", json!({"items": items})).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.chars().count() <= crate::mcp::paging::RESULT_MAX_CHARS,
            "{}",
            text.chars().count()
        );
        let doc = document(&result);
        let results = doc["results"].as_array().unwrap();
        assert_eq!(results.len(), 50, "every item is answered");
        // Every echo of a reference goes first; the first items keep the call that repeats them.
        let first = &results[0]["error"];
        assert_eq!(
            (first["code"].as_str(), first["facts"].get("artist")),
            (Some("navidrome_unreachable"), None)
        );
        assert_eq!(first["facts"]["omitted"], json!(["reference"]));
        assert_eq!(first["next"][0]["args"]["items"][0]["artist"], long('a', 0));
        let last = &results[49]["error"];
        assert_eq!(last["facts"]["omitted"], json!(["reference", "next"]));
        assert_eq!(last["next"], json!([]));
    }

    /// The answer `run` builds for MAX_ITEMS items each refused with `failure`, before it is
    /// bounded: every item echoes a reference whose texts are as long as a caller may send them
    /// and offers the call again for itself.
    fn refused_with(failure: &Failure) -> super::Items {
        use crate::core::library::Reference;
        let long = |c: char, i: usize| format!("{} {i}", c.to_string().repeat(480));
        let given: Vec<Value> = (0..super::MAX_ITEMS)
            .map(|i| json!({"artist": long('a', i), "title": long('t', i), "album": long('l', i)}))
            .collect();
        let call = json!({ "items": given });
        let results = (0..super::MAX_ITEMS)
            .map(|i| {
                let reference = Reference {
                    artist: long('a', i),
                    title: long('t', i),
                    album: Some(long('l', i)),
                    recording_mbid: None,
                    duration_s: None,
                };
                let facts = crate::core::jobs::Wanted::Track(reference).facts();
                let item = super::EnsureItem::Refused(failure.clone(), facts);
                super::item_result(item, &given[i], &call)
            })
            .collect();
        let fields = serde_json::Map::from_iter([
            ("batch_id".to_string(), Value::Null),
            ("library_checked".to_string(), json!(true)),
            ("next".to_string(), json!([])),
        ]);
        super::Items {
            stage: shared::failure::Stage::Input,
            noun: "items",
            start: 0,
            results,
            pending: 0,
            cursor: None,
            fields,
        }
    }

    #[test]
    fn a_long_playlist_error_is_bounded_with_the_items() {
        use crate::mcp::contract::{NextCall, ToolError};
        use shared::failure::RetryAdvice;
        let failure = Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::LibraryCheck,
            RetryAdvice::SameSourceLater,
            "Find songs failed: HTTP 502",
        );
        let mut answered = refused_with(&failure);
        // begin's refusal of 1000 owned songs Navidrome no longer lists, ids as long as a caller
        // may send them, with the call that would make the playlist of them.
        let unknown: Vec<String> = (0..1000).map(|i| format!("{i:0>500}")).collect();
        let playlist = ToolError {
            failure: Failure::new(
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Navidrome does not list 1000 of these songs for you and has 0 with a missing file",
            ),
            facts: serde_json::Map::from_iter([
                ("playlist_id".to_string(), Value::Null),
                ("unknown_song_ids".to_string(), json!(unknown)),
            ]),
            tried: vec![],
            next: vec![NextCall::new(
                "create_playlist",
                json!({"name": "Christmas", "song_ids": unknown}),
            )],
        };
        super::bounded(&mut answered, Some(playlist));
        let text = super::items_text(&answered);
        assert!(
            text.chars().count() <= RESULT_MAX_CHARS,
            "{}",
            text.chars().count()
        );
        let doc: Value = serde_json::from_str(&text).unwrap();
        let error = &doc["playlist"]["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                &error["next"],
                &error["facts"]["omitted"]
            ),
            (Some("unknown_handle"), &json!([]), &json!(["next"]))
        );
        let (kept, omitted) = (
            error["facts"]["unknown_song_ids"].as_array().unwrap().len() as u64,
            error["facts"]["unknown_song_ids_omitted"].as_u64().unwrap(),
        );
        assert_eq!(kept + omitted, 1000);
    }

    /// The error of `answered`'s item `at`, as sent.
    fn sent_error(answered: &super::Items, at: usize) -> Value {
        let doc: Value = serde_json::from_str(&super::items_text(answered)).unwrap();
        doc["results"][at]["error"].clone()
    }

    #[test]
    fn fifty_refused_items_each_with_a_long_cause_keep_within_the_cap() {
        use shared::failure::{RetryAdvice, Service, RAW_MAX_CHARS};
        // A Navidrome lookup that failed with a cause as long as one is kept, all quotes, each
        // sent as 2 characters.
        let failure = Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::LibraryCheck,
            RetryAdvice::SameSourceLater,
            "Find songs failed: HTTP 502",
        )
        .with_cause(Service::Navidrome, Some(502), "\"".repeat(RAW_MAX_CHARS));
        let mut answered = refused_with(&failure);
        super::bounded(&mut answered, None);
        let text = super::items_text(&answered);
        assert!(
            text.chars().count() <= RESULT_MAX_CHARS,
            "{}",
            text.chars().count()
        );
        let first = sent_error(&answered, 0);
        assert_eq!(first["facts"]["omitted"], json!(["reference", "next"]));
        assert_eq!(
            first["cause"]["raw"]
                .as_str()
                .map(|raw| raw.chars().count()),
            Some(RAW_MAX_CHARS)
        );
        let last = sent_error(&answered, super::MAX_ITEMS - 1);
        assert_eq!(
            last["facts"]["omitted"],
            json!(["reference", "next", "cause.raw"])
        );
        assert_eq!(
            (
                &last["cause"]["service"],
                &last["cause"]["status"],
                &last["cause"]["raw"]
            ),
            (&json!("navidrome"), &json!(502), &json!(""))
        );
        assert_eq!(last["message"], "Find songs failed: HTTP 502");
    }

    #[test]
    fn refused_items_whose_messages_are_long_keep_within_the_cap() {
        use shared::failure::{json_chars, RetryAdvice, Service, RAW_MAX_CHARS};
        // No failure Soulful makes has such a message: the bound holds whatever one holds.
        let failure = Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::LibraryCheck,
            RetryAdvice::SameSourceLater,
            "\"".repeat(5 * RAW_MAX_CHARS),
        )
        .with_cause(Service::Navidrome, Some(502), "\"".repeat(RAW_MAX_CHARS));
        let mut answered = refused_with(&failure);
        super::bounded(&mut answered, None);
        let text = super::items_text(&answered);
        assert!(
            text.chars().count() <= RESULT_MAX_CHARS,
            "{}",
            text.chars().count()
        );
        let last = sent_error(&answered, super::MAX_ITEMS - 1);
        assert_eq!(
            last["facts"]["omitted"],
            json!(["reference", "next", "cause.raw", "message"])
        );
        let message = last["message"].as_str().unwrap();
        assert!(
            json_chars(message) <= shared::failure::TEXT_MAX_CHARS,
            "{message}"
        );
        assert_eq!(last["code"], "navidrome_unreachable");
    }

    #[tokio::test]
    async fn no_default_folder_lists_the_folders_to_choose_from() {
        let (db, acq, _, token) = app_with_acquire().await;
        // Never written to: no job is made.
        add_folder(&db, "u1", "f2", "Christmas", Path::new("/music/christmas")).await;
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
        add_folder(&db, "u2", "b1", "Bob's", Path::new("/music/bob")).await;
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
                    "folder_id": "alice-folder", "request_key": "k1"}),
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
        // Nothing was made: the key is free for the same call later.
        assert_eq!(count(&db, "request_keys").await, 0);
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
    async fn a_keyed_call_refused_before_any_job_runs_again_with_its_key() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.importer().set_health(broken());
        let call = json!({"items": [{"artist": "Marrow Choir", "title": "O Holy Night"}],
            "request_key": "k1"});
        let refused = document(
            &call_tool(app_of(&db, &acq), &token, "ensure_in_library", call.clone()).await,
        );
        assert_eq!(refused["error"]["code"], "importer_broken", "{refused}");
        effects.importer().set_health(ready());
        let ran = document(&call_tool(app_of(&db, &acq), &token, "ensure_in_library", call).await);
        assert_eq!(ran["results"][0]["status"], "job_created", "{ran}");
        assert_eq!(
            (count(&db, "batches").await, count(&db, "jobs").await),
            (1, 1)
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

    /// The id of `source`'s candidate in the search of alice's job `job`.
    async fn candidate_of(acq: &Acquire, job: &str, source: &str) -> String {
        let search = view_of(acq, job).await.search.unwrap();
        let groups = search.candidates.unwrap();
        let index = groups.iter().position(|g| g.source == source).unwrap();
        format!("{}:{index}", search.id)
    }

    /// The enqueues once there are `n`, the job runner sending them in its own task.
    async fn enqueued(effects: &TestEffects, n: usize) -> Vec<(NewBatch, Acquisition)> {
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
    async fn download_source_continues_a_job_waiting_for_a_choice() {
        let (db, acq, effects, token) = jobs_app().await;
        let job = choosing_track_job(&acq).await;
        let candidate = candidate_of(&acq, &job, "peer_b").await;
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"job_id": job, "candidate_id": candidate}),
            )
            .await,
        );
        assert_eq!(
            (doc["job_id"].as_str(), doc["state"].as_str()),
            (Some(job.as_str()), Some("queued")),
            "{doc}"
        );
        let sent = enqueued(&effects, 1).await;
        assert_eq!(
            (sent[0].1.job.as_str(), sent[0].1.items[0].1.source.as_str()),
            (job.as_str(), "peer_b")
        );
        // Queued now: another choice for it is refused, with what can be done instead.
        let other = candidate_of(&acq, &job, "peer_a").await;
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"job_id": job, "candidate_id": other}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["state"].as_str()),
            (Some("wrong_job_state"), Some("queued")),
            "{error}"
        );
        let tools: Vec<&str> = (error["next"].as_array().unwrap().iter())
            .map(|n| n["tool"].as_str().unwrap())
            .collect();
        assert_eq!(tools, vec!["get_jobs", "cancel_job"]);
    }

    #[tokio::test]
    async fn a_candidate_of_another_users_search_reads_like_an_unknown_one() {
        let (db, acq, _, token) = jobs_app().await;
        folder_of(&db, "bob").await;
        let bob = UserId::new("bob");
        let search_id = uuid::Uuid::new_v4().to_string();
        acq.ledger
            .record_search(NewSearch {
                id: search_id.clone(),
                user: bob,
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
                spec: Some(StoredSpec::default()),
            })
            .await
            .unwrap();
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "O Holy Night", "a/03.flac")],
            0.9,
        )];
        acq.ledger
            .search_ended(&search_id, Ok((found, Some(SearchStats::default()))))
            .await
            .unwrap();
        for candidate in [
            format!("{search_id}:0"),
            format!("{search_id}:7"),
            "not a candidate".to_string(),
        ] {
            let error = document(
                &call_tool(
                    app_of(&db, &acq),
                    &token,
                    "download_source",
                    json!({"candidate_id": candidate,
                        "reference": {"artist": "Marrow Choir", "title": "O Holy Night"}}),
                )
                .await,
            )["error"]
                .clone();
            assert_eq!(
                (error["code"].as_str(), error["message"].as_str()),
                (Some("unknown_handle"), Some("Candidate not found")),
                "{candidate}"
            );
        }
        assert_eq!(count(&db, "jobs").await, 0);
    }

    #[tokio::test]
    async fn a_chosen_candidate_with_a_reference_starts_a_job_in_a_new_batch() {
        let (db, acq, effects, token) = jobs_app().await;
        let job = choosing_track_job(&acq).await;
        let candidate = candidate_of(&acq, &job, "peer_b").await;
        // alice has no default folder: the call again with each of hers.
        let reference = json!({"artist": "Marrow Choir", "title": "O Come All Ye Faithful"});
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"candidate_id": candidate, "reference": reference}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(error["code"], "folder_required", "{error}");
        assert_eq!(
            error["next"],
            json!([{"tool": "download_source", "args": {"candidate_id": candidate,
                "reference": reference, "folder_id": "alice-folder"}}])
        );
        // The same recording as alice's waiting job: refused, naming it.
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"candidate_id": candidate, "reference": reference,
                    "folder_id": "alice-folder"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["job_id"].as_str()),
            (Some("already_running"), Some(job.as_str())),
            "{error}"
        );
        let other = json!({"artist": "Marrow Choir", "title": "Adeste Fideles"});
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"candidate_id": candidate, "reference": other,
                    "folder_id": "alice-folder"}),
            )
            .await,
        );
        assert_eq!(doc["state"], "queued", "{doc}");
        assert_ne!(doc["batch_id"], view_of(&acq, &job).await.job.batch_id);
        let sent = enqueued(&effects, 1).await;
        assert_eq!(sent[0].1.items[0].1.source, "peer_b");
        // A track's candidate for an album: refused.
        let album = json!({"kind": "album", "artist": "Kowloon", "title": "Come Over",
            "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f"});
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "download_source",
                json!({"candidate_id": candidate, "reference": album}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            error["message"],
            "download_source: candidate_id must be a candidate of an album search"
        );
    }

    // On the real clock: the database answers from its own thread, and the search polls once,
    // 2 s after it starts.
    #[tokio::test]
    async fn a_review_batch_offers_each_jobs_candidates_through_get_batch() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![
                group(
                    "peer_a",
                    vec![item("peer_a", "O Holy Night", "a/03.flac")],
                    0.9,
                ),
                group(
                    "peer_b",
                    vec![item("peer_b", "O Holy Night", "b/03.flac")],
                    0.8,
                ),
            ],
        ))]);
        let app = app_of(&db, &acq);
        let made = document(
            &call_tool(
                app.clone(),
                &token,
                "ensure_in_library",
                json!({"pick": "review",
                    "items": [{"artist": "Marrow Choir", "title": "O Holy Night"}]}),
            )
            .await,
        );
        let batch = made["batch_id"].clone();
        let doc = document(
            &call_tool(
                app,
                &token,
                "get_batch",
                json!({"batch_id": batch, "wait_seconds": 180, "wait_for": "needs_action"}),
            )
            .await,
        );
        let job = &doc["needs_action"][0];
        assert_eq!(job["state"], "needs_source_choice", "{doc}");
        let choices: Vec<&Value> = (job["next"].as_array().unwrap().iter())
            .filter(|n| n["tool"] == "download_source")
            .collect();
        assert_eq!(
            choices
                .iter()
                .map(|n| n["source"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["peer_a", "peer_b"]
        );
        assert!(
            choices
                .iter()
                .all(|n| n["args"]["job_id"] == job["job_id"]
                    && n["args"]["candidate_id"].is_string())
        );
        assert!(effects.enqueued().is_empty(), "review downloads nothing");
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

    /// `navi` finds Silent Night in alice's library and lists it present; O Holy Night it does
    /// not have; she has no playlist yet.
    fn library_with_silent_night(navi: &NavidromeStub) {
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        let silent = native_song(SILENT.0, "Marrow Choir", SILENT.1, 222.0, None);
        let found = format!("[{silent}]");
        navi.answer_when("/api/song", "title=Marrow+Choir+Silent+Night", 200, &found);
        navi.answer_when("/api/song", "title=Marrow+Choir+O+Holy+Night", 200, "[]");
        let present = native_songs(&[(SILENT.0, false)]);
        navi.answer_when("/api/song", &format!("id={}", SILENT.0), 200, &present);
    }

    fn christmas_items() -> Value {
        json!([{"artist": "Marrow Choir", "title": "Silent Night"},
            {"artist": "Marrow Choir", "title": "O Holy Night"}])
    }

    #[tokio::test]
    async fn an_ensure_with_a_new_playlist_starts_it_with_the_owned_tracks() {
        let navi = NavidromeStub::start();
        library_with_silent_night(&navi);
        let christmas = playlist_body(ADVENT, "Christmas", "alice", false, "", &[SILENT]);
        navi.answer("/rest/createPlaylist", &[&christmas]);
        navi.answer("/rest/getPlaylist", &[&christmas]);
        navi.answer(
            &format!("/api/playlist/{ADVENT}/tracks"),
            &[&playlist_rows(&[(1, SILENT.0, SILENT.1, false)])],
        );
        let (db, acq, _, token) = app_with_acquire().await;
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "ensure_in_library",
                json!({"playlist": {"create": {"name": "Christmas"}}, "items": christmas_items()}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["playlist"]["ok"].as_bool(),
                doc["playlist"]["playlist_id"].as_str(),
                &doc["playlist"]["song_ids"]
            ),
            (Some(true), Some(ADVENT), &json!([SILENT.0])),
            "{doc}"
        );
        assert_eq!(
            navi.posted("/rest/createPlaylist"),
            vec![format!("name=Christmas&songId={}", SILENT.0)]
        );
        let batch = document(
            &call_tool(
                app,
                &token,
                "get_batch",
                json!({"batch_id": doc["batch_id"]}),
            )
            .await,
        );
        assert_eq!(
            batch["playlist"],
            json!({"playlist_id": ADVENT, "settled": 1, "waiting": 1, "failure": null,
                "hidden_entries_dropped": 0})
        );
    }

    #[tokio::test]
    async fn a_taken_playlist_name_refuses_before_any_job_is_made() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[("p-old", "Christmas", "alice", false)])],
        );
        let (db, acq, _, token) = app_with_acquire().await;
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let items = json!([{"artist": "Marrow Choir", "title": "O Holy Night"}]);
        let error = document(
            &call_tool(
                app,
                &token,
                "ensure_in_library",
                json!({"playlist": {"create": {"name": "Christmas"}}, "items": items,
                    "request_key": "k1"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                &error["facts"]["existing_playlist_ids"]
            ),
            (Some("playlist_name_taken"), &json!(["p-old"])),
            "{error}"
        );
        assert_eq!(
            error["next"],
            json!([{"tool": "ensure_in_library", "args": {"items": items,
                "playlist": {"create": {"name": "Christmas", "allow_duplicate_name": true}}}}])
        );
        assert_eq!(count(&db, "batches").await, 0);
        assert_eq!(count(&db, "request_keys").await, 0, "nothing was written");
    }

    #[tokio::test]
    async fn a_playlist_create_that_failed_after_the_jobs_were_made_offers_the_list_to_look_in() {
        let navi = NavidromeStub::start();
        library_with_silent_night(&navi);
        navi.answer_when("/rest/createPlaylist", "", 502, BAD_GATEWAY);
        let (db, acq, _, token) = app_with_acquire().await;
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let call = json!({"playlist": {"create": {"name": "Christmas", "public": true}},
            "items": christmas_items(), "request_key": "k1"});
        let result = call_tool(app.clone(), &token, "ensure_in_library", call.clone()).await;
        let doc = document(&result);
        let error = &doc["playlist"]["error"];
        assert_eq!(
            (
                doc["playlist"]["ok"].as_bool(),
                error["code"].as_str(),
                doc["results"][1]["status"].as_str()
            ),
            (
                Some(false),
                Some("navidrome_unreachable"),
                Some("job_created")
            ),
            "{doc}"
        );
        // The create was sent and may have been made: making it again could make a second one.
        assert_eq!(
            error["next"],
            json!([{"tool": "list_playlists", "args": {}}])
        );
        // The batch exists: the key keeps the answer, and nothing runs twice.
        let again = call_tool(app.clone(), &token, "ensure_in_library", call).await;
        assert_eq!(again, result);
        assert_eq!(
            (count(&db, "batches").await, count(&db, "jobs").await),
            (1, 1)
        );
        let batch = document(
            &call_tool(
                app,
                &token,
                "get_batch",
                json!({"batch_id": doc["batch_id"]}),
            )
            .await,
        );
        assert_eq!(batch["playlist"], Value::Null);
    }

    #[tokio::test]
    async fn a_playlist_of_items_whose_jobs_all_run_already_is_filled_as_those_jobs_finish() {
        let (db, acq, effects, token) = jobs_app().await;
        // The runners stay in their searches: the test moves the jobs on itself.
        let _held = effects.stub().hold_searches();
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "title=", 200, "[]");
        navi.answer(
            "/api/song",
            &[&native_songs(&[(SILENT.0, false), (HOLY.0, false)])],
        );
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        let christmas = playlist_body(ADVENT, "Christmas", "alice", false, "", &[]);
        navi.answer("/rest/createPlaylist", &[&christmas]);
        navi.answer("/rest/getPlaylist", &[&christmas]);
        let tracks = format!("/api/playlist/{ADVENT}/tracks");
        navi.answer(&tracks, &["[]"]);
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let items = json!({"items": christmas_items(), "folder_id": "alice-folder"});
        let first = document(&call_tool(app.clone(), &token, "ensure_in_library", items).await);
        let jobs: Vec<String> = (0..2)
            .map(|i| first["results"][i]["job_id"].as_str().unwrap().to_string())
            .collect();
        let with_playlist = json!({"items": christmas_items(), "folder_id": "alice-folder",
            "playlist": {"create": {"name": "Christmas"}}});
        let doc =
            document(&call_tool(app.clone(), &token, "ensure_in_library", with_playlist).await);
        let batch_id = doc["batch_id"].as_str().expect("a batch for the playlist");
        assert_ne!(Some(batch_id), first["batch_id"].as_str());
        assert_eq!(
            (
                doc["results"][0]["error"]["facts"]["job_id"].as_str(),
                doc["results"][1]["error"]["facts"]["job_id"].as_str()
            ),
            (Some(jobs[0].as_str()), Some(jobs[1].as_str())),
            "{doc}"
        );
        assert_eq!(
            (
                doc["playlist"]["ok"].as_bool(),
                doc["playlist"]["playlist_id"].as_str(),
                &doc["playlist"]["song_ids"]
            ),
            (Some(true), Some(ADVENT), &json!([])),
            "{doc}"
        );
        assert_eq!(
            doc["next"],
            json!([{"tool": "get_batch", "args": {"batch_id": batch_id, "wait_seconds": 180,
                "wait_for": "needs_action"}}])
        );
        let playlist_state = || async {
            document(
                &call_tool(
                    app.clone(),
                    &token,
                    "get_batch",
                    json!({"batch_id": batch_id}),
                )
                .await,
            )["playlist"]
                .clone()
        };
        assert_eq!(
            playlist_state().await,
            json!({"playlist_id": ADVENT, "settled": 0, "waiting": 2, "failure": null,
                "hidden_entries_dropped": 0})
        );
        // The second job ends first: each song still goes to its item's place.
        for (job, (song, title)) in jobs.iter().zip([SILENT, HOLY]).rev() {
            start_downloading(&acq, job, title).await;
            settle_done(&acq, job, song).await;
        }
        navi.answer(
            &tracks,
            &[
                "[]",
                &playlist_rows(&[(1, SILENT.0, SILENT.1, false), (2, HOLY.0, HOLY.1, false)]),
            ],
        );
        navi.answer(
            &format!("/api/playlist/{ADVENT}"),
            &[&native_playlist(ADVENT, "Christmas", 0, false, None)],
        );
        round(&db, &acq.ledger, &NavidromeFor(Ok(navi.client()))).await;
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!(
                "playlistId={ADVENT}&songIdToAdd={}&songIdToAdd={}",
                SILENT.0, HOLY.0
            )]
        );
        assert_eq!(
            playlist_state().await,
            json!({"playlist_id": ADVENT, "settled": 2, "waiting": 0, "failure": null,
                "hidden_entries_dropped": 0})
        );
    }

    #[tokio::test]
    async fn a_playlist_that_fails_for_items_whose_jobs_all_run_already_leaves_no_batch() {
        let (db, acq, effects, token) = jobs_app().await;
        let _held = effects.stub().hold_searches();
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "title=", 200, "[]");
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer_when("/rest/createPlaylist", "", 502, BAD_GATEWAY);
        let app = app_with_navidrome(&db, &acq, Arc::new(NavidromeFor(Ok(navi.client()))));
        let items = json!({"items": christmas_items(), "folder_id": "alice-folder"});
        call_tool(app.clone(), &token, "ensure_in_library", items).await;
        let with_playlist = json!({"items": christmas_items(), "folder_id": "alice-folder",
            "playlist": {"create": {"name": "Christmas"}}});
        let doc = document(&call_tool(app, &token, "ensure_in_library", with_playlist).await);
        assert_eq!(
            (
                doc["playlist"]["ok"].as_bool(),
                doc["playlist"]["error"]["code"].as_str()
            ),
            (Some(false), Some("navidrome_unreachable")),
            "{doc}"
        );
        // Its batch would follow nothing: there is none to follow.
        assert_eq!((&doc["batch_id"], &doc["next"]), (&Value::Null, &json!([])));
        assert_eq!(count(&db, "batches").await, 1);
    }

    #[tokio::test]
    async fn a_playlist_names_one_of_an_id_and_a_new_playlist() {
        let (db, acq, _, token) = app_with_acquire().await;
        let items = json!([{"artist": "Marrow Choir", "title": "O Holy Night"}]);
        for (playlist, message) in [
            (
                json!({}),
                "ensure_in_library: playlist.playlist_id must be given when create is not",
            ),
            (
                json!({"playlist_id": ADVENT, "create": {"name": "Christmas"}}),
                "ensure_in_library: playlist.create must be left out when playlist_id is given",
            ),
            (
                json!({"create": {"comment": "Carols"}}),
                "ensure_in_library: playlist.create.name must be a name of 1 to 255 characters",
            ),
        ] {
            let error = document(
                &call_tool(
                    app_of(&db, &acq),
                    &token,
                    "ensure_in_library",
                    json!({"playlist": playlist, "items": items}),
                )
                .await,
            )["error"]
                .clone();
            assert_eq!(error["message"], message, "{playlist}");
        }
        assert_eq!(count(&db, "batches").await, 0);
    }
}
