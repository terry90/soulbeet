//! Library audit and extras (cover art & lyrics) MCP tools.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{Failure, Stage};

use super::args::Args;
use super::contract::{
    answer, failed, items, upstream_error, ItemResult, Items, ToolError,
};
use super::deletion::deletion_off_at;
use super::keys::{keyed, request_key, Keyed};
use super::library::navidrome_of;
use super::paging::RESULT_MAX_CHARS;
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::sync::cut;
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::delete_guard::quota::deleting_app_in;
use crate::core::delete_guard::Guard;
use crate::core::tracks::audit::{
    self, AuditReport, AUDIT_DEFAULT_SAMPLE, AUDIT_MAX_SAMPLE,
};
use crate::core::tracks::extras::{
    self, ExtrasEntry, ExtrasReport, ExtrasRequest, LiveExtrasFetcher,
};
use crate::core::tracks::{TrackSelection, ALBUM_MAX_TRACKS, SONGS_PER_CALL};

const AUDIT_ARGS: &[&str] = &["folder_id", "sample_limit"];

const FETCH_EXTRAS_ARGS: &[&str] = &[
    "song_ids",
    "album_id",
    "artwork",
    "lyrics",
    "dry_run",
    "overwrite",
    "request_key",
];

static LIVE_EXTRAS: LazyLock<LiveExtrasFetcher> = LazyLock::new(LiveExtrasFetcher::new);

pub(crate) const AUDIT_LIBRARY: ToolSpec = ToolSpec {
    name: "audit_library",
    title: "Audit library health",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Runs a read-only health audit across the user's library folders and Navidrome songs (pass folder_id to scope to one folder from list_folders). Reports exact totals and up to sample_limit (1 to {AUDIT_MAX_SAMPLE}, default {AUDIT_DEFAULT_SAMPLE}) examples for six checks: untracked_on_disk (audio files on disk not indexed by Navidrome), missing_in_navidrome (Navidrome songs whose file is missing on disk), split_albums (albums spanning multiple Soulbeet folders or Navidrome libraries), missing_mbids (songs without a MusicBrainz recording id), missing_cover_art (album directories without cover art), and missing_lyrics (tracks without .lrc or .txt lyrics sidecars). Skips hidden dot-directories and Discovery/ staging folders. Read-only; does not require the file-management switch. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"folder_id": {"type": "string", "minLength": 1}, "sample_limit": {"type": "integer", "minimum": 1, "maximum": 100}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(audit_library(state, caller, args)),
};

pub(crate) const FETCH_ARTWORK_AND_LYRICS: ToolSpec = ToolSpec {
    name: "fetch_artwork_and_lyrics",
    title: "Fetch cover art and lyrics",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Fetches missing album cover art (cover.jpg or cover.png from Cover Art Archive via MusicBrainz, fetched once per album directory) and/or synced or plain lyrics ({{stem}}.lrc from LRCLIB) for up to {SONGS_PER_CALL} songs (song_ids) or one whole album (album_id, up to {ALBUM_MAX_TRACKS} tracks), writing files atomically inside the user's owned library folders and triggering a Navidrome scan when files are written. Both artwork and lyrics default to true (at least one must be true). Pass dry_run: true to preview what would be fetched and written without writing any files. By default (overwrite: false), existing cover art and .lrc files are skipped (already_present) and the call is purely additive so it does not require the file-management switch. Passing overwrite: true (with dry_run: false) overwrites existing cover art or .lrc files and requires 'Allow managing library files (delete, move, re-tag)' switched on for this app in Settings > Users > Connected apps; ALWAYS ask the user before calling with overwrite: true. Failure codes: {common}, {NAVIDROME_CODES}, permission_not_granted, unknown_handle, already_running, partial, and per song unknown_handle, invalid_input, delete_path_unresolved, delete_outside_library, delete_not_a_track.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}, "album_id": {"type": "string", "minLength": 1}, "artwork": {"type": "boolean"}, "lyrics": {"type": "boolean"}, "dry_run": {"type": "boolean"}, "overwrite": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: true,
    call: |state, caller, args| Box::pin(fetch_artwork_and_lyrics(state, caller, args)),
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

async fn audit_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(AUDIT_LIBRARY.name, args, AUDIT_ARGS);
    answer(
        async {
            let folder_id = args.opt_string("folder_id")?;
            let sample_limit = args
                .opt_u32("sample_limit", 1, AUDIT_MAX_SAMPLE as u32)?
                .map(|n| n as usize)
                .unwrap_or(AUDIT_DEFAULT_SAMPLE);
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let mut report = audit::audit_library(
                &state.db,
                &navi,
                state.navidrome_music_path.as_deref(),
                &caller.ctx,
                folder_id.as_deref(),
                sample_limit,
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            trim_audit_to_fit(&mut report);
            let Value::Object(fields) = json!(report) else {
                unreachable!("AuditReport serializes as a JSON object");
            };
            Ok(fields)
        }
        .await,
    )
}

/// Ensures the serialized `audit_library` document fits within `RESULT_MAX_CHARS` by trimming the
/// longest sample arrays first and updating `omitted` and `truncated`.
fn trim_audit_to_fit(report: &mut AuditReport) {
    // Account for `{"ok":true,` wrapper added by `contract::success`.
    const WRAPPER_OVERHEAD: usize = 16;
    loop {
        let len = serde_json::to_string(report)
            .map(|s| s.chars().count())
            .unwrap_or(0)
            + WRAPPER_OVERHEAD;
        if len <= RESULT_MAX_CHARS {
            return;
        }
        let counts = [
            report.untracked_on_disk.len(),
            report.missing_in_navidrome.len(),
            report.split_albums.len(),
            report.missing_mbids.len(),
            report.missing_cover_art.len(),
            report.missing_lyrics.len(),
        ];
        let Some((idx, &max_len)) = counts.iter().enumerate().max_by_key(|(_, c)| **c) else {
            return;
        };
        if max_len == 0 {
            return;
        }
        let drop_n = (max_len / 4).max(1);
        match idx {
            0 => {
                let new_len = report.untracked_on_disk.len() - drop_n;
                report.untracked_on_disk.truncate(new_len);
                report.omitted.untracked_on_disk += drop_n;
            }
            1 => {
                let new_len = report.missing_in_navidrome.len() - drop_n;
                report.missing_in_navidrome.truncate(new_len);
                report.omitted.missing_in_navidrome += drop_n;
            }
            2 => {
                let new_len = report.split_albums.len() - drop_n;
                report.split_albums.truncate(new_len);
                report.omitted.split_albums += drop_n;
            }
            3 => {
                let new_len = report.missing_mbids.len() - drop_n;
                report.missing_mbids.truncate(new_len);
                report.omitted.missing_mbids += drop_n;
            }
            4 => {
                let new_len = report.missing_cover_art.len() - drop_n;
                report.missing_cover_art.truncate(new_len);
                report.omitted.missing_cover_art += drop_n;
            }
            _ => {
                let new_len = report.missing_lyrics.len() - drop_n;
                report.missing_lyrics.truncate(new_len);
                report.omitted.missing_lyrics += drop_n;
            }
        }
        report.truncated = true;
    }
}

async fn fetch_artwork_and_lyrics(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(FETCH_ARTWORK_AND_LYRICS.name, args, FETCH_EXTRAS_ARGS);
    let read = || {
        let song_ids = args.opt_strings("song_ids", SONGS_PER_CALL)?;
        let album_id = args.opt_string("album_id")?;
        let artwork = args.opt_bool("artwork")?.unwrap_or(true);
        let lyrics = args.opt_bool("lyrics")?.unwrap_or(true);
        let dry_run = args.opt_bool("dry_run")?.unwrap_or(false);
        let overwrite = args.opt_bool("overwrite")?.unwrap_or(false);
        let key = request_key(&args)?;
        args.finish()?;
        let selection = read_selection(&args, song_ids, album_id)?;
        if !artwork && !lyrics {
            return Err(args.refuse("artwork", "at least one of artwork or lyrics set to true"));
        }
        Ok::<_, ToolError>((
            ExtrasRequest {
                selection,
                artwork,
                lyrics,
                dry_run,
                overwrite,
            },
            key,
        ))
    };
    let (req, key) = match read() {
        Ok(r) => r,
        Err(e) => return failed(e),
    };

    if !req.dry_run && req.overwrite && !caller.allow_delete {
        return failed(deletion_off_at(Stage::Retag));
    }

    if req.dry_run {
        let guard = guard_of(state);
        return match extras::fetch_artwork_and_lyrics(
            &guard,
            state.navidrome.as_ref(),
            &state.acquire.queue,
            &caller.ctx,
            &*LIVE_EXTRAS,
            req,
        )
        .await
        {
            Ok(report) => extras_answer(report),
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
        if req.overwrite {
            let user = ctx.id();
            let mut conn = match db.acquire().await {
                Ok(c) => c,
                Err(e) => {
                    return Keyed::Released(failed(upstream_error(
                        database_failure(Stage::Retag, e),
                        Map::new(),
                        same_call,
                    )));
                }
            };
            if let Err(f) = deleting_app_in(&mut conn, &user, &grant_id).await {
                return Keyed::Released(failed(upstream_error(
                    Failure {
                        stage: Stage::Retag,
                        ..f
                    },
                    Map::new(),
                    same_call,
                )));
            }
        }
        let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
        match extras::fetch_artwork_and_lyrics(
            &guard,
            navidrome.as_ref(),
            &queue,
            &ctx,
            &*LIVE_EXTRAS,
            req,
        )
        .await
        {
            Ok(report) => Keyed::Kept(extras_answer(report)),
            Err(f) => Keyed::Released(failed(upstream_error(f, Map::new(), same_call))),
        }
    };
    keyed(
        &state.acquire,
        caller,
        FETCH_ARTWORK_AND_LYRICS.name,
        key,
        &call,
        work,
    )
    .await
}

fn extras_answer(report: ExtrasReport) -> CallToolResult {
    let results: Vec<ItemResult> = report
        .entries
        .into_iter()
        .map(|entry: ExtrasEntry| match entry.outcome {
            Ok(ok) => {
                let Value::Object(fields) = json!(ok) else {
                    unreachable!("ExtrasItemSuccess serializes as a JSON object");
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
        "album_id": report.album_id,
        "artwork_files_written": report.artwork_files_written,
        "lyrics_files_written": report.lyrics_files_written,
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
