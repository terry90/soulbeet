//! The delete tools (design 13): a preview that checks songs and issues a single-use
//! confirmation token for the ones that may be deleted, the deletion that takes the token, and
//! the deletion history.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS};
use shared::navidrome::{DeletionReason, DeletionReview, DAILY_APP_DELETIONS};
use soulbeet::navidrome::NativePlaylist;

use super::args::Args;
use super::contract::{
    answer, failed, items, items_text, upstream_error, ItemResult, Items, NextCall, ToolError,
};
use super::keys::{keyed, request_key, Keyed};
use super::paging::{fit, leave_out, Keep, Page, RESULT_MAX_CHARS};
use super::refs::{page_schema, request_key_schema};
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::sync::cut;
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::delete_guard::quota::permission_not_granted;
use crate::core::delete_guard::Guard;
use crate::core::deletion::tokens::{DeleteTokenRefusal, TokenRefusalKind};
use crate::core::deletion::{
    self, App, ConfirmOutcome, Confirmed, Preview, PreviewEntry, PreviewOutcome, SONGS_PER_TOKEN,
    TOKEN_SECS,
};
use crate::core::library::SongFacts;
use crate::models::deletion_review::DeletionReviewRow;
use crate::oauth::now_unix;

const PREVIEW_ARGS: &[&str] = &["song_ids", "reason"];

const DELETE_ARGS: &[&str] = &["confirmation_token", "request_key"];

const HISTORY_ARGS: &[&str] = &["limit", "cursor"];

/// Most of the caller's playlists an allowed song lists; `playlists_omitted` counts the rest.
const PLAYLISTS_SHOWN: usize = 10;

pub(crate) const PREVIEW_DELETE_TRACKS: ToolSpec = ToolSpec {
    name: "preview_delete_tracks",
    title: "Preview deleting songs",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        let minutes = TOKEN_SECS / 60;
        format!("Checks up to {SONGS_PER_TOKEN} songs (Navidrome song ids from find_in_library, search_library, get_playlist or find_duplicates) for deletion from this user's library and says for each whether it may be deleted and, if not, why: not in one of this user's folders, not an audio file, wanted by a download or import that is not finished, rated, starred or kept in a playlist by another listener, another listener's ratings unreadable, or the file not where Navidrome says. reason is disliked (discovery stops suggesting the track), replaced or cleanup. Returns each song's title, artist, album, file, size, format, this user's rating and playlists, and a confirmation_token for exactly the songs that may be deleted, valid {minutes} minutes and once. Nothing is deleted by this call. Deletion is permanent: show the user this preview, the songs and files, and ask for an explicit yes before calling delete_tracks with the token; the delete_tracks entry in next says requires_user_confirmation. A pending discovery track is not deleted here; remove_discovery_track removes it. Needs deletion switched on for this app in Soulful (Settings > Users > Connected apps). At most {DAILY_APP_DELETIONS} tracks a day per user through connected apps. Failure codes: {common}, {NAVIDROME_CODES}, permission_not_granted, delete_daily_limit, partial, and per song unknown_handle, delete_path_unresolved, delete_outside_library, delete_not_a_track, delete_in_use, delete_blocked_by_other_user, delete_veto_unknown.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["song_ids", "reason"], "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}, "reason": {"enum": ["disliked", "replaced", "cleanup"]}}}"#,
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: false,
    call: |state, caller, args| Box::pin(preview_delete_tracks(state, caller, args)),
};

pub(crate) const DELETE_TRACKS: ToolSpec = ToolSpec {
    name: "delete_tracks",
    title: "Delete songs",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        let minutes = TOKEN_SECS / 60;
        format!("Permanently deletes the files of the songs a preview_delete_tracks confirmation_token allows, from disk and from the beets library; there is no undo and no trash. Call it only after showing the user the preview and getting their explicit yes for those songs, never on your own judgment. Every check runs again for each song first: a song whose file changed since the preview, or that a check now keeps, is not deleted. Returns per song deleted, or the failure that kept it (with the OS error when the file could not be removed). The token works once, for this user and this app, for {minutes} minutes. At most {DAILY_APP_DELETIONS} tracks a day per user through connected apps. Failure codes: {common}, permission_not_granted, unknown_handle (no such token for this user and app), expired_handle (token used or expired), delete_daily_limit, already_running, partial, and per song the preview's codes, delete_changed_since_preview, delete_failed.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["confirmation_token"], "properties": {"confirmation_token": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(delete_tracks(state, caller, args)),
};

pub(crate) const GET_DELETION_HISTORY: ToolSpec = ToolSpec {
    name: "get_deletion_history",
    title: "Deletion history",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Lists the deletions of this user's library files, newest first: title, artist, album, file, the rating it had, why (disliked, replaced, cleanup), how (auto for the rating sync of Soulful itself, ui for a web UI import that replaced it, mcp for a connected app, its rating syncs included, with the app's name) and when. Takes limit and cursor. Failure codes: {common}.")
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
    call: |state, caller, args| Box::pin(get_deletion_history(state, caller, args)),
};

/// The caller as deletion needs it.
fn app_of(caller: &McpCaller) -> App {
    App {
        user: caller.ctx.id(),
        grant_id: caller.grant_id.clone(),
    }
}

/// The guard over the state's database, Navidrome as each user sees it and the music path.
fn guard_of(state: &ConnectionState) -> Guard<'_> {
    Guard::new(
        &state.db,
        state.listeners.as_ref(),
        state.navidrome_music_path.as_deref(),
    )
}

/// An app without the deletion switch, refused before any argument value is used.
fn deletion_off() -> ToolError {
    ToolError {
        failure: permission_not_granted(),
        facts: Map::new(),
        tried: vec![],
        next: vec![],
    }
}

/// `preview_delete_tracks`: each song checked, and a token for those every rule allows.
async fn preview_delete_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(PREVIEW_DELETE_TRACKS.name, args, PREVIEW_ARGS);
    let read = || {
        let song_ids = args.strings("song_ids", 1, SONGS_PER_TOKEN)?;
        if (1..song_ids.len()).any(|i| song_ids[..i].contains(&song_ids[i])) {
            return Err(args.refuse(
                "song_ids",
                &format!("a list of 1 to {SONGS_PER_TOKEN} distinct song ids"),
            ));
        }
        let reason = (args.opt_choice("reason", DeletionReason::NAMES)?)
            .and_then(DeletionReason::parse)
            .ok_or_else(|| {
                let names = DeletionReason::NAMES.join(", ");
                args.refuse("reason", &format!("one of {names}"))
            })?;
        args.finish()?;
        Ok::<_, ToolError>((song_ids, reason))
    };
    let (song_ids, reason) = match read() {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    if !caller.allow_delete {
        return failed(deletion_off());
    }
    let previewed = deletion::preview(
        &guard_of(state),
        &app_of(caller),
        &song_ids,
        reason,
        now_unix(),
    )
    .await;
    match previewed {
        Ok(preview) => preview_answer(&args, reason, preview),
        Err(f) => {
            let facts = Map::from_iter([("requested".to_string(), json!(song_ids.len()))]);
            failed(upstream_error(f, facts, args.same_call()))
        }
    }
}

/// The preview as `items`: one entry per song in input order, the token and what it allows
/// deleting, the day's count and the calls that act on it. No allowed entry and no file is ever
/// left out or cut, so the user sees everything the token deletes: while the answer is over
/// RESULT_MAX_CHARS, entries from the last one up keep only the title of their song facts; when
/// it still does not fit, the call is refused without the token's secret, and the stored token
/// expires unused.
fn preview_answer(args: &Args, reason: DeletionReason, p: Preview) -> CallToolResult {
    let mut next = Vec::new();
    if let Some(token) = &p.token {
        next.push(
            NextCall::new("delete_tracks", json!({"confirmation_token": token.secret}))
                .with("requires_user_confirmation", json!(true)),
        );
    }
    let results: Vec<ItemResult> = (p.entries.iter())
        .map(|entry| {
            let (result, remove) = preview_entry(entry, reason);
            next.extend(remove);
            result
        })
        .collect();
    let token = p.token.as_ref();
    let Value::Object(fields) = json!({
        "reason": reason.as_str(),
        "confirmation_token": token.map(|t| &t.secret),
        "expires_at": token
            .and_then(|t| chrono::DateTime::from_timestamp(t.expires_at, 0))
            .map(|at| at.to_rfc3339()),
        "will_delete": token.map_or(&[][..], |t| &t.song_ids[..]),
        "daily_limit": {"limit": DAILY_APP_DELETIONS, "used_last_24h": p.used_today},
        "next": next,
    }) else {
        unreachable!("an object literal is an object");
    };
    let mut answered = Items {
        stage: Stage::Delete,
        noun: "songs",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    };
    let over = |answered: &Items| items_text(answered).chars().count() > RESULT_MAX_CHARS;
    for at in (0..answered.results.len()).rev() {
        if !over(&answered) {
            break;
        }
        title_only(&mut answered.results[at]);
    }
    if over(&answered) {
        return failed(args.refuse(
            "song_ids",
            "fewer songs: this preview does not fit in one answer, so preview fewer songs at a time",
        ));
    }
    items(answered)
}

/// One song of the preview, and the call that removes it when it is a pending discovery track
/// (which deletes its staged file, so it waits for the user's yes like delete_tracks).
fn preview_entry(entry: &PreviewEntry, reason: DeletionReason) -> (ItemResult, Option<NextCall>) {
    let song = |facts: Option<SongFacts>| json!(facts);
    let record = entry.record.as_ref().map(SongFacts::from);
    match &entry.outcome {
        PreviewOutcome::Allowed(checked) => {
            let shown = checked.playlists.len().min(PLAYLISTS_SHOWN);
            let Value::Object(fields) = json!({
                "song_id": entry.song_id,
                "verdict": "allowed",
                "song": SongFacts::from(checked.song.as_ref()),
                "file": checked.path.to_string_lossy(),
                "playlists": checked.playlists[..shown].iter().map(playlist).collect::<Vec<_>>(),
                "playlists_omitted": checked.playlists.len() - shown,
            }) else {
                unreachable!("an object literal is an object");
            };
            (ItemResult::Done(fields), None)
        }
        PreviewOutcome::Discovery(track) => {
            let remove = NextCall::new("remove_discovery_track", json!({"track_id": track.id}))
                .with("requires_user_confirmation", json!(true));
            let Value::Object(fields) = json!({
                "song_id": entry.song_id,
                "verdict": "discovery_track",
                "song": song(record),
                "discovery_track_id": track.id,
                "profile": track.profile,
                "remove_with": remove,
            }) else {
                unreachable!("an object literal is an object");
            };
            (ItemResult::Done(fields), Some(remove))
        }
        PreviewOutcome::Blocked(failure) => {
            let facts = Map::from_iter([
                ("song_id".to_string(), json!(entry.song_id)),
                ("verdict".to_string(), json!("blocked")),
                ("song".to_string(), song(record)),
            ]);
            let again = NextCall::new(
                "preview_delete_tracks",
                json!({"song_ids": [entry.song_id], "reason": reason.as_str()}),
            );
            let error = upstream_error(cut(failure), facts, Some(again));
            (ItemResult::Failed(error), None)
        }
    }
}

/// One of the caller's playlists holding a song, its name cut to TEXT_MAX_CHARS.
fn playlist(p: &NativePlaylist) -> Value {
    json!({
        "playlist_id": p.id,
        "name": cut_json(p.name.clone(), TEXT_MAX_CHARS),
        "smart": p.is_smart(),
    })
}

/// Leaves out the song facts of `result` but their title, marking it with
/// `song_facts_omitted`.
fn title_only(result: &mut ItemResult) {
    let fields = match result {
        ItemResult::Done(fields) => fields,
        ItemResult::Failed(error) => &mut error.facts,
    };
    let Some(Value::Object(song)) = fields.get("song") else {
        return;
    };
    let title = song.get("title").cloned().unwrap_or(Value::Null);
    fields.insert("song".into(), json!({"title": title}));
    fields.insert("song_facts_omitted".into(), json!(true));
}

/// `delete_tracks`: the token taken and each of its files deleted that every rule still allows
/// and that is still the file the preview checked.
async fn delete_tracks(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(DELETE_TRACKS.name, args, DELETE_ARGS);
    let read = || {
        let secret = args.string("confirmation_token")?;
        let key = request_key(&args)?;
        args.finish()?;
        Ok::<_, ToolError>((secret, key))
    };
    let (secret, key) = match read() {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    if !caller.allow_delete {
        return failed(deletion_off());
    }
    // The key's ledger row holds only the call's hash, never the token.
    let call = args.as_value();
    let (db, listeners, music_path, queue) = (
        state.db.clone(),
        state.listeners.clone(),
        state.navidrome_music_path.clone(),
        state.acquire.queue.clone(),
    );
    let app = app_of(caller);
    let work = async move {
        let guard = Guard::new(&db, listeners.as_ref(), music_path.as_deref());
        match deletion::confirm(&guard, &queue, &app, &secret, now_unix()).await {
            Ok(done) => Keyed::Kept(confirmed_answer(done)),
            // A refused take changed nothing: the token is as it was.
            Err(r) => Keyed::Released(failed(token_refused(r))),
        }
    };
    keyed(&state.acquire, caller, DELETE_TRACKS.name, key, &call, work).await
}

/// The confirm as `items`: one entry per previewed file, deleted or the failure that kept it,
/// never one left out. While the answer is over RESULT_MAX_CHARS, entries from the last one up
/// keep only the title of their song facts, then leave out warnings, counted in
/// `warnings_omitted`.
fn confirmed_answer(done: Confirmed) -> CallToolResult {
    let results = (done.entries.into_iter())
        .map(|entry| {
            let file = entry.file.to_string_lossy().into_owned();
            match entry.outcome {
                ConfirmOutcome::Deleted { song, warnings } => {
                    let warnings: Vec<String> = (warnings.into_iter())
                        .map(|w| cut_json(w, RAW_MAX_CHARS))
                        .collect();
                    let Value::Object(fields) = json!({
                        "song_id": entry.song_id,
                        "status": "deleted",
                        "song": SongFacts::from(song.as_ref()),
                        "file": file,
                        "warnings": warnings,
                    }) else {
                        unreachable!("an object literal is an object");
                    };
                    ItemResult::Done(fields)
                }
                ConfirmOutcome::Refused(failure) => ItemResult::Failed(ToolError {
                    failure: cut(&failure),
                    facts: Map::from_iter([
                        ("song_id".to_string(), json!(entry.song_id)),
                        ("file".to_string(), json!(file)),
                    ]),
                    tried: vec![],
                    next: vec![],
                }),
            }
        })
        .collect();
    let Value::Object(fields) = json!({
        "reason": done.reason.as_str(),
        "next": [NextCall::new("get_deletion_history", json!({}))],
    }) else {
        unreachable!("an object literal is an object");
    };
    let mut answered = Items {
        stage: Stage::Delete,
        noun: "songs",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    };
    let over_by =
        |answered: &Items| (items_text(answered).chars().count()).saturating_sub(RESULT_MAX_CHARS);
    for at in (0..answered.results.len()).rev() {
        if over_by(&answered) == 0 {
            break;
        }
        title_only(&mut answered.results[at]);
    }
    for at in (0..answered.results.len()).rev() {
        let over = over_by(&answered);
        if over == 0 {
            break;
        }
        if let ItemResult::Done(fields) = &mut answered.results[at] {
            leave_out(fields, "warnings", Keep::First, over);
        }
    }
    items(answered)
}

/// A DeleteTokenRefusal as a tool error: the token's songs and reason as facts when the token is
/// the caller's, and the call that can still help: get_deletion_history for a used token, a new
/// preview of the same songs for an expired one or one the day's room cannot hold yet (after
/// retry_after_seconds); nothing else, and never this call again.
pub(crate) fn token_refused(r: DeleteTokenRefusal) -> ToolError {
    let known = r
        .reason
        .map(|reason| json!({"song_ids": r.song_ids, "reason": reason.as_str()}));
    let next = match r.kind {
        TokenRefusalKind::Used => vec![NextCall::new("get_deletion_history", json!({}))],
        TokenRefusalKind::Expired | TokenRefusalKind::Room => (known.clone())
            .map(|args| NextCall::new("preview_delete_tracks", args))
            .into_iter()
            .collect(),
        TokenRefusalKind::NotFound | TokenRefusalKind::Permission | TokenRefusalKind::Database => {
            vec![]
        }
    };
    let facts = match known {
        Some(Value::Object(facts)) => facts,
        _ => Map::new(),
    };
    ToolError {
        failure: r.failure,
        facts,
        tried: vec![],
        next,
    }
}

/// `get_deletion_history`: a page of the caller's deletions, newest first.
async fn get_deletion_history(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_DELETION_HISTORY.name, args, HISTORY_ARGS);
    answer(
        async {
            let page = Page::read(&args, json!({}))?;
            args.finish()?;
            let unavailable = |e: String| {
                upstream_error(
                    database_failure(Stage::Delete, e),
                    Map::new(),
                    args.same_call(),
                )
            };
            // One more than the page says whether more follow.
            let rows = DeletionReviewRow::page_in(
                &state.db,
                &caller.ctx.id().to_string(),
                page.offset as u32,
                page.limit as u32 + 1,
            )
            .await
            .map_err(unavailable)?;
            let more = rows.len() > page.limit;
            let deletions = (rows.iter().take(page.limit))
                .map(deletion_fields)
                .collect::<Result<Vec<Value>, String>>()
                .map_err(unavailable)?;
            Ok(fit(Map::new(), "deletions", deletions, &page, more))
        }
        .await,
    )
}

/// One deletion as get_deletion_history lists it: its tags cut to TEXT_MAX_CHARS, the time it
/// was recorded in RFC 3339 (SQLite stores `YYYY-MM-DD HH:MM:SS` in UTC), and the app's host for
/// an app's deletion. A time that is not in that form is an error.
fn deletion_fields(d: &DeletionReview) -> Result<Value, String> {
    let deleted_at = chrono::NaiveDateTime::parse_from_str(&d.created_at, "%Y-%m-%d %H:%M:%S")
        .map_err(|e| format!("Unreadable deletion time {}: {e}", d.created_at))?
        .and_utc()
        .to_rfc3339();
    let cut_text = |text: &str| cut_json(text.to_string(), TEXT_MAX_CHARS);
    Ok(json!({
        "song_id": d.song_id,
        "title": cut_text(&d.title),
        "artist": cut_text(&d.artist),
        "album": cut_text(&d.album),
        "file": d.path,
        "rating": d.rating,
        "reason": d.reason.as_str(),
        "origin": d.origin,
        "app": d.client,
        "deleted_at": deleted_at,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use soulbeet::navidrome::models::NativeSong;

    use super::*;
    use crate::connection::test_util::{
        access_token, app_on_library, call_tool, document, RESOURCE,
    };
    use crate::core::delete_guard::quota::{daily_limit, enable_deletion};
    use crate::core::delete_guard::test_support::Library;
    use crate::core::deletion::tokens::{token_expired, token_not_found, token_used};

    async fn preview_s1(app: &axum::Router, token: &str) -> Value {
        document(
            &call_tool(
                app.clone(),
                token,
                "preview_delete_tracks",
                json!({"song_ids": ["s1"], "reason": "cleanup"}),
            )
            .await,
        )
    }

    #[tokio::test]
    async fn the_delete_tools_refuse_an_app_without_the_switch() {
        let lib = Library::new().await;
        let (app, token) = app_on_library(&lib, false).await;
        for (tool, args) in [
            (
                "preview_delete_tracks",
                json!({"song_ids": ["s1"], "reason": "cleanup"}),
            ),
            ("delete_tracks", json!({"confirmation_token": "x"})),
        ] {
            let error = &document(&call_tool(app.clone(), &token, tool, args).await)["error"];
            assert_eq!(
                (error["code"].as_str(), error["message"].as_str()),
                (
                    Some("permission_not_granted"),
                    Some("Deleting music is turned off for this app")
                ),
                "{tool}"
            );
            assert_eq!(error["user_action"]["code"], "enable_deletion", "{tool}");
            assert_eq!((&error["facts"], &error["next"]), (&json!({}), &json!([])));
        }
        assert_eq!(lib.fake.reads_by("alice"), 0);
        assert_eq!(
            permission_not_granted().user_action,
            Some(enable_deletion()),
            "one text for one user action"
        );
    }

    #[tokio::test]
    async fn a_preview_then_its_confirm_deletes_the_song_and_the_history_names_the_app() {
        let lib = Library::new().await;
        lib.fake.playlist("alice", "Advent", &["s1"], false, false);
        let (app, token) = app_on_library(&lib, true).await;
        let preview = preview_s1(&app, &token).await;
        let entry = &preview["results"][0];
        assert_eq!(
            (
                entry["verdict"].as_str(),
                entry["song"]["title"].as_str(),
                entry["song"]["size_bytes"].as_u64(),
                entry["playlists"][0]["name"].as_str()
            ),
            (
                Some("allowed"),
                Some("O Holy Night"),
                Some(5),
                Some("Advent")
            )
        );
        let x = std::fs::canonicalize(lib.root.join("music/alice/A/x.flac")).unwrap();
        assert_eq!(entry["file"], json!(x.to_str().unwrap()));
        assert_eq!(preview["will_delete"], json!(["s1"]));
        assert_eq!(
            preview["daily_limit"],
            json!({"limit": DAILY_APP_DELETIONS, "used_last_24h": 0})
        );
        let secret = preview["confirmation_token"].as_str().unwrap().to_string();
        assert_eq!(
            preview["next"][0],
            json!({"tool": "delete_tracks", "args": {"confirmation_token": secret}, "requires_user_confirmation": true})
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        let done = document(
            &call_tool(
                app.clone(),
                &token,
                "delete_tracks",
                json!({"confirmation_token": secret}),
            )
            .await,
        );
        assert_eq!(
            (done["ok"].as_bool(), done["results"][0]["status"].as_str()),
            (Some(true), Some("deleted"))
        );
        assert_eq!(done["results"][0]["file"], json!(x.to_str().unwrap()));
        assert_eq!(
            done["next"],
            json!([{"tool": "get_deletion_history", "args": {}}])
        );
        assert!(
            !done.to_string().contains(&secret),
            "the answer never repeats the token"
        );
        assert!(!lib.root.join("music/alice/A/x.flac").exists());
        let history = document(&call_tool(app, &token, "get_deletion_history", json!({})).await);
        let row = &history["deletions"][0];
        assert_eq!(
            (
                row["song_id"].as_str(),
                row["origin"].as_str(),
                row["app"].as_str(),
                row["reason"].as_str(),
                row["file"].as_str()
            ),
            (
                Some("s1"),
                Some("mcp"),
                Some("claude.ai"),
                Some("cleanup"),
                x.to_str()
            )
        );
        assert!(
            chrono::DateTime::parse_from_rfc3339(row["deleted_at"].as_str().unwrap()).is_ok(),
            "{row}"
        );
    }

    #[tokio::test]
    async fn a_blocked_song_makes_the_preview_partial_and_stays_out_of_the_token() {
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.stars("bob", "s2");
        let (app, token) = app_on_library(&lib, true).await;
        let result = call_tool(
            app,
            &token,
            "preview_delete_tracks",
            json!({"song_ids": ["s1", "s2"], "reason": "disliked"}),
        )
        .await;
        let doc = document(&result);
        assert_eq!(
            (result["isError"].as_bool(), doc["error"]["code"].as_str()),
            (Some(true), Some("partial"))
        );
        let blocked = &doc["results"][1]["error"];
        assert_eq!(
            (
                blocked["code"].as_str(),
                blocked["message"].as_str(),
                blocked["facts"]["verdict"].as_str(),
                blocked["facts"]["song"]["title"].as_str()
            ),
            (
                Some("delete_blocked_by_other_user"),
                Some("Another listener rated, starred or kept this track in a playlist"),
                Some("blocked"),
                Some("O Holy Night")
            )
        );
        assert_eq!(
            blocked["next"],
            json!([]),
            "another listener's block does not pass later"
        );
        assert!(
            !blocked.to_string().contains("bob"),
            "the refusal names no one"
        );
        assert_eq!(doc["will_delete"], json!(["s1"]));
    }

    #[tokio::test]
    async fn a_pending_discovery_track_is_offered_to_its_own_remover_with_the_users_yes() {
        let lib = Library::new().await;
        lib.song("s5", "alice/Discovery/Explore/03.flac", b"audio");
        let staged = lib
            .pending_track(
                "t1",
                "alice-music",
                "Explore",
                &lib.root.join("music/alice/Discovery/Explore/03.flac"),
                Some("s5"),
            )
            .await;
        let (app, token) = app_on_library(&lib, true).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "preview_delete_tracks",
                json!({"song_ids": ["s5"], "reason": "disliked"}),
            )
            .await,
        );
        let remove = json!({"tool": "remove_discovery_track", "args": {"track_id": staged.id},
            "requires_user_confirmation": true});
        let entry = &doc["results"][0];
        assert_eq!(
            (
                entry["verdict"].as_str(),
                entry["discovery_track_id"].as_str(),
                entry["profile"].as_str(),
                &entry["remove_with"]
            ),
            (
                Some("discovery_track"),
                Some("t1"),
                Some("Explore"),
                &remove
            )
        );
        assert_eq!(
            (
                &doc["confirmation_token"],
                &doc["will_delete"],
                &doc["next"]
            ),
            (&Value::Null, &json!([]), &json!([remove]))
        );
    }

    #[tokio::test]
    async fn another_users_token_reads_like_none() {
        let lib = Library::new().await;
        let (app, alice) = app_on_library(&lib, true).await;
        let secret = preview_s1(&app, &alice).await["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();
        let bob = access_token(&lib.db, "bob", RESOURCE).await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1 WHERE user_id = 'bob'")
            .execute(&lib.db)
            .await
            .unwrap();
        let error = document(
            &call_tool(
                app,
                &bob,
                "delete_tracks",
                json!({"confirmation_token": secret}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["facts"],
                &error["next"]
            ),
            (
                Some("unknown_handle"),
                Some("Confirmation token not found"),
                &json!({}),
                &json!([])
            )
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_repeated_confirm_with_its_request_key_answers_the_first_result() {
        let lib = Library::new().await;
        let (app, token) = app_on_library(&lib, true).await;
        let secret = preview_s1(&app, &token).await["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();
        let args = json!({"confirmation_token": secret, "request_key": "del-1"});
        let first = document(&call_tool(app.clone(), &token, "delete_tracks", args.clone()).await);
        assert_eq!(first["results"][0]["status"], "deleted");
        assert_eq!(
            document(&call_tool(app.clone(), &token, "delete_tracks", args).await),
            first
        );
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT args_hash || COALESCE(document, '') FROM request_keys")
                .fetch_all(&lib.db)
                .await
                .unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            !stored[0].contains(&secret),
            "the key's row never holds the token"
        );
        let used = document(
            &call_tool(
                app,
                &token,
                "delete_tracks",
                json!({"confirmation_token": secret}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (used["code"].as_str(), &used["next"]),
            (
                Some("expired_handle"),
                &json!([{"tool": "get_deletion_history", "args": {}}])
            )
        );
    }

    #[test]
    fn each_token_refusal_offers_the_call_that_helps() {
        use TokenRefusalKind::*;
        let known = |kind, failure| DeleteTokenRefusal {
            kind,
            failure,
            song_ids: vec!["s1".into()],
            reason: Some(DeletionReason::Cleanup),
        };
        let preview = json!([{"tool": "preview_delete_tracks", "args": {"song_ids": ["s1"], "reason": "cleanup"}}]);
        for (r, next) in [
            (
                known(Used, token_used()),
                json!([{"tool": "get_deletion_history", "args": {}}]),
            ),
            (
                known(Expired, token_expired(1_800_000_000)),
                preview.clone(),
            ),
            (
                known(Room, daily_limit(100, 1, 1_800_000_400, 1_800_000_000)),
                preview,
            ),
            (known(Permission, permission_not_granted()), json!([])),
            (
                DeleteTokenRefusal {
                    kind: NotFound,
                    failure: token_not_found(),
                    song_ids: vec![],
                    reason: None,
                },
                json!([]),
            ),
        ] {
            let kind = r.kind;
            let e = token_refused(r);
            assert_eq!(serde_json::to_value(&e.next).unwrap(), next, "{kind:?}");
            assert_eq!(
                e.facts.contains_key("song_ids"),
                kind != NotFound,
                "{kind:?}"
            );
            assert!(
                e.next.iter().all(|call| call.tool != "delete_tracks"),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn bad_song_lists_name_the_field() {
        let lib = Library::new().await;
        let (app, token) = app_on_library(&lib, true).await;
        let ids: Vec<String> = (0..26).map(|i| format!("s{i}")).collect();
        for (args, field) in [
            (json!({"song_ids": ids, "reason": "cleanup"}), "song_ids"),
            (
                json!({"song_ids": ["s1", " s1"], "reason": "cleanup"}),
                "song_ids",
            ),
            (json!({"song_ids": ["s1"], "reason": "because"}), "reason"),
            (json!({"song_ids": ["s1"]}), "reason"),
        ] {
            let error = &document(
                &call_tool(app.clone(), &token, "preview_delete_tracks", args.clone()).await,
            )["error"];
            assert_eq!(
                (error["code"].as_str(), error["facts"]["field"].as_str()),
                (Some("invalid_input"), Some(field)),
                "{args}"
            );
        }
        assert_eq!(lib.fake.reads_by("alice"), 0);
    }

    /// 25 songs of `lib` with 500-character titles, artists and albums, each in `playlists` of
    /// alice's with 600-character names (ids too); their ids.
    fn long_songs(lib: &Library, playlists: usize) -> Vec<String> {
        let ids: Vec<String> = (0..25).map(|i| format!("long{i:02}")).collect();
        for (i, id) in ids.iter().enumerate() {
            lib.song(id, &format!("alice/L/{i:02}.flac"), b"audio");
            lib.fake.file(
                NativeSong {
                    title: format!("{i:02}{}", "t".repeat(498)),
                    artist: "a".repeat(500),
                    album: "b".repeat(500),
                    ..lib.record(id)
                },
                "al1",
            );
        }
        let held: Vec<&str> = ids.iter().map(String::as_str).collect();
        for p in 0..playlists {
            lib.fake.playlist(
                "alice",
                &format!("{p:02}{}", "p".repeat(598)),
                &held,
                false,
                false,
            );
        }
        ids
    }

    #[tokio::test]
    async fn a_long_preview_keeps_every_file_whole_and_shortens_song_facts_from_the_end() {
        let lib = Library::new().await;
        let ids = long_songs(&lib, 1);
        let (app, token) = app_on_library(&lib, true).await;
        let result = call_tool(
            app,
            &token,
            "preview_delete_tracks",
            json!({"song_ids": ids, "reason": "cleanup"}),
        )
        .await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= RESULT_MAX_CHARS);
        let doc = document(&result);
        assert_eq!(doc["will_delete"], json!(ids));
        assert!(doc["confirmation_token"].is_string());
        let results = doc["results"].as_array().unwrap();
        assert_eq!(results.len(), 25);
        for (i, entry) in results.iter().enumerate() {
            let file =
                std::fs::canonicalize(lib.root.join(format!("music/alice/L/{i:02}.flac"))).unwrap();
            assert_eq!(entry["file"], json!(file.to_str().unwrap()), "{i}");
            assert!(
                entry["song"]["title"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{i:02}")),
                "{i}"
            );
            assert_eq!(entry["playlists"].as_array().unwrap().len(), 1, "{i}");
        }
        // The first entries keep every fact, the last ones only the title.
        assert_eq!(results[0]["song"]["artist"], "a".repeat(500));
        assert_eq!(
            (
                &results[24]["song"]["artist"],
                &results[24]["song_facts_omitted"]
            ),
            (&Value::Null, &json!(true))
        );
        // A playlist's name is cut, its id is not.
        let listed = &results[0]["playlists"][0];
        assert_eq!(listed["playlist_id"].as_str().unwrap().len(), 600);
        assert!(listed["name"].as_str().unwrap().chars().count() < 600);
    }

    #[tokio::test]
    async fn an_allowed_song_lists_ten_of_the_callers_playlists_and_counts_the_rest() {
        let lib = Library::new().await;
        for p in 0..12 {
            let name = format!("Advent {p:02}");
            lib.fake.playlist("alice", &name, &["s1"], false, p == 11);
        }
        let (app, token) = app_on_library(&lib, true).await;
        let entry = preview_s1(&app, &token).await["results"][0].clone();
        assert_eq!(entry["playlists"].as_array().unwrap().len(), 10);
        assert_eq!(
            (&entry["playlists"][0], &entry["playlists_omitted"]),
            (
                &json!({"playlist_id": "Advent 00", "name": "Advent 00", "smart": false}),
                &json!(2)
            )
        );
    }

    #[tokio::test]
    async fn a_preview_that_cannot_fit_is_refused_without_its_token() {
        let lib = Library::new().await;
        let ids = long_songs(&lib, 12);
        let (app, token) = app_on_library(&lib, true).await;
        let result = call_tool(
            app,
            &token,
            "preview_delete_tracks",
            json!({"song_ids": ids, "reason": "cleanup"}),
        )
        .await;
        let doc = document(&result);
        assert_eq!(
            (
                doc["error"]["code"].as_str(),
                doc["error"]["facts"]["field"].as_str()
            ),
            (Some("invalid_input"), Some("song_ids"))
        );
        assert!(doc.get("confirmation_token").is_none() && doc.get("results").is_none());
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delete_tokens")
            .fetch_one(&lib.db)
            .await
            .unwrap();
        assert_eq!(stored, 1, "the token was stored and expires unused");
    }

    #[tokio::test]
    async fn get_deletion_history_pages_newest_first() {
        let lib = Library::new().await;
        lib.app_deletions(30, 60).await;
        let (app, token) = app_on_library(&lib, true).await;
        let first = document(
            &call_tool(
                app.clone(),
                &token,
                "get_deletion_history",
                json!({"limit": 25}),
            )
            .await,
        );
        assert_eq!(first["deletions"].as_array().unwrap().len(), 25);
        assert!(first["cursor"].is_string());
        let row = &first["deletions"][0];
        assert_eq!(
            (
                row["origin"].as_str(),
                row["app"].as_str(),
                &row["file"],
                &row["rating"]
            ),
            (Some("mcp"), Some("claude.ai"), &Value::Null, &Value::Null)
        );
        let second = document(
            &call_tool(
                app,
                &token,
                "get_deletion_history",
                json!({"limit": 25, "cursor": first["cursor"]}),
            )
            .await,
        );
        assert_eq!(
            (
                second["deletions"].as_array().unwrap().len(),
                &second["cursor"]
            ),
            (5, &Value::Null)
        );
    }

    #[test]
    fn the_descriptions_say_deletion_is_permanent_and_needs_the_users_yes() {
        let preview = (PREVIEW_DELETE_TRACKS.description)(120);
        for p in [
            "Nothing is deleted by this call",
            "Deletion is permanent",
            "show the user this preview",
            "explicit yes before calling delete_tracks",
            "requires_user_confirmation",
        ] {
            assert!(preview.contains(p), "preview_delete_tracks: {p}");
        }
        let delete = (DELETE_TRACKS.description)(120);
        for p in [
            "Permanently deletes",
            "there is no undo and no trash",
            "only after showing the user the preview",
            "explicit yes",
            "never on your own judgment",
        ] {
            assert!(delete.contains(p), "delete_tracks: {p}");
        }
        const {
            assert!(
                DELETE_TRACKS.destructive
                    && !PREVIEW_DELETE_TRACKS.destructive
                    && !PREVIEW_DELETE_TRACKS.read_only
            )
        };
    }

    #[test]
    fn the_tool_texts_carry_the_limits_they_enforce() {
        let schema: Value = serde_json::from_str(PREVIEW_DELETE_TRACKS.input_schema).unwrap();
        assert_eq!(
            schema["properties"]["song_ids"]["maxItems"],
            json!(SONGS_PER_TOKEN)
        );
        assert_eq!(
            schema["properties"]["reason"]["enum"],
            json!(DeletionReason::NAMES)
        );
        let preview = (PREVIEW_DELETE_TRACKS.description)(120);
        let delete = (DELETE_TRACKS.description)(120);
        assert!(preview.contains(&format!("Checks up to {SONGS_PER_TOKEN} songs")));
        assert!(preview.contains(&format!("valid {} minutes", TOKEN_SECS / 60)));
        assert!(delete.contains(&format!("for {} minutes", TOKEN_SECS / 60)));
        for text in [preview, delete] {
            assert!(text.contains(&format!("At most {DAILY_APP_DELETIONS} tracks a day")));
        }
    }
}
