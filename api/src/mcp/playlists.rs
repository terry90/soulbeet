//! The playlist tools: the caller's Navidrome playlists and other users'
//! public ones, and the writes to the caller's own.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, Stage, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{
    answer, error_object, failed, invalid_value, success, too_large_to_repeat, upstream_error,
    NextCall, ToolError,
};
use super::keys::{keyed, request_key, Keyed};
use super::library::navidrome_of;
use super::paging::{fit, leave_out, Keep, Page, RESULT_MAX_CHARS};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::playlists::{
    self, MetaChange, NewPlaylist, PlaylistRefusal, Written, SONG_IDS_MAX,
};

const CREATE_PLAYLIST_ARGS: &[&str] = &[
    "name",
    "song_ids",
    "comment",
    "public",
    "allow_duplicate_name",
    "request_key",
];

const UPDATE_PLAYLIST_ARGS: &[&str] = &[
    "playlist_id",
    "name",
    "comment",
    "public",
    "allow_duplicate_name",
    "request_key",
];

const DELETE_PLAYLIST_ARGS: &[&str] = &["playlist_id", "request_key"];

/// Longest playlist name, in characters after trimming.
const NAME_MAX_CHARS: usize = 255;
const A_NAME: &str = "a name of 1 to 255 characters";
/// Longest playlist comment, in characters after trimming.
const COMMENT_MAX_CHARS: usize = 2000;
/// Most song ids a write's result lists; get_playlist pages through a longer playlist.
const SONG_IDS_LISTED_MAX: usize = 500;

pub(crate) const CREATE_PLAYLIST: ToolSpec = ToolSpec {
    name: "create_playlist",
    title: "Create a playlist",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Creates a static Navidrome playlist owned by this user with the given songs in the given order, then reads it back from Navidrome. song_ids are Navidrome song ids (find_in_library, search_library, get_playlist), at most 1000; the list may be empty. Returns the playlist as Navidrome lists it. Playlist ids stay valid until the playlist is deleted; song ids while the file stays in the library. A repeat with the same request_key returns the first result. Takes a few seconds. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle (Navidrome does not list some song ids for this user, or their files are missing; facts name them and next repeats the call without them), playlist_name_taken (this user already has a playlist with that name; pass allow_duplicate_name to make another), playlist_tracks_not_written, already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["name"], "properties": {"name": {"type": "string", "minLength": 1, "maxLength": 255}, "song_ids": {"type": "array", "maxItems": 1000, "items": {"type": "string", "minLength": 1, "maxLength": 500}}, "comment": {"type": "string", "maxLength": 2000}, "public": {"type": "boolean"}, "allow_duplicate_name": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: false,
    call: |state, caller, args| Box::pin(create_playlist(state, caller, args)),
};

pub(crate) const UPDATE_PLAYLIST: ToolSpec = ToolSpec {
    name: "update_playlist",
    title: "Rename or describe a playlist",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Changes the name, comment or public flag of one of this user's playlists; an empty comment clears it. Only the owner's playlists can be changed. Returns the playlist as Navidrome lists it afterwards. Playlist ids stay valid until the playlist is deleted. Failure codes: {common}, {NAVIDROME_CODES}, playlist_not_found, playlist_not_owner, playlist_name_taken, playlist_not_written, already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["playlist_id"], "properties": {"playlist_id": {"type": "string", "minLength": 1}, "name": {"type": "string", "minLength": 1, "maxLength": 255}, "comment": {"type": "string", "maxLength": 2000}, "public": {"type": "boolean"}, "allow_duplicate_name": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(update_playlist(state, caller, args)),
};

pub(crate) const DELETE_PLAYLIST: ToolSpec = ToolSpec {
    name: "delete_playlist",
    title: "Delete a playlist",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Deletes one of this user's playlists. The songs stay in the library. Only the owner's playlists can be deleted. Returns what the playlist was. Failure codes: {common}, {NAVIDROME_CODES}, playlist_not_found, playlist_not_owner, playlist_not_written (Navidrome still lists it), already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["playlist_id"], "properties": {"playlist_id": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(delete_playlist(state, caller, args)),
};

/// `list_playlists`: a page of the playlists the caller may see.
pub(crate) async fn list_playlists(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_playlists", args, &["limit", "cursor"]);
    answer(
        async {
            let page = Page::read(&args, json!({}))?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
            let listed = playlists::list(&navi, &caller.ctx)
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(fit(Map::new(), "playlists", page.of(&listed), &page, false))
        }
        .await,
    )
}

/// `get_playlist`: the playlist and a page of its entries. An id with no
/// playlist the caller may see offers the list to pick one from.
pub(crate) async fn get_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_playlist", args, &["playlist_id", "limit", "cursor"]);
    answer(
        async {
            let playlist_id = args.string("playlist_id")?;
            let page = Page::read(&args, json!({"playlist_id": playlist_id}))?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
            let (playlist, entries) = playlists::get(&navi, &caller.ctx, &playlist_id)
                .await
                .map_err(|f| not_got(f, &playlist_id, &args))?;
            let mut fields = Map::new();
            fields.insert("playlist".into(), json!(playlist));
            Ok(fit(fields, "entries", page.of(&entries), &page, false))
        }
        .await,
    )
}

/// get_playlist's failure: `playlist_not_found` offers the list instead of
/// the same call.
fn not_got(f: Failure, playlist_id: &str, args: &Args) -> ToolError {
    let not_found = f.code == FailureCode::PlaylistNotFound;
    let mut facts = Map::new();
    facts.insert("playlist_id".into(), json!(playlist_id));
    let mut e = upstream_error(f, facts, args.same_call());
    if not_found {
        e.next = vec![list_playlists_call()];
    }
    e
}

fn list_playlists_call() -> NextCall {
    NextCall::new("list_playlists", json!({}))
}

fn get_playlist_call(playlist_id: &str) -> NextCall {
    NextCall::new("get_playlist", json!({"playlist_id": playlist_id}))
}

/// `create_playlist`: a new playlist of the caller's, once per request key.
pub(crate) async fn create_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("create_playlist", args, CREATE_PLAYLIST_ARGS);
    let read = async {
        let name = name_arg(&args)?.ok_or_else(|| invalid_value(args.tool(), "name", A_NAME))?;
        let song_ids = args.opt_strings("song_ids", SONG_IDS_MAX)?;
        let new = NewPlaylist {
            name,
            song_ids: song_ids.unwrap_or_default(),
            comment: args.opt_text("comment", COMMENT_MAX_CHARS)?,
            public: args.opt_bool("public")?,
            allow_duplicate_name: args.opt_bool("allow_duplicate_name")?.unwrap_or(false),
        };
        let key = request_key(&args)?;
        args.finish()?;
        let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
        Ok::<_, ToolError>((new, key, navi))
    };
    let (new, key, navi) = match read.await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let ctx = caller.ctx.clone();
    let work = async move {
        match playlists::create(&navi, &ctx, &new).await {
            Ok(w) => Keyed::Kept(success(playlist_fields(&w))),
            Err(r) => refused(&args, None, &r),
        }
    };
    keyed(&state.acquire, caller, "create_playlist", key, &call, work).await
}

/// `update_playlist`: a new name, comment or public flag for one of the caller's playlists.
pub(crate) async fn update_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("update_playlist", args, UPDATE_PLAYLIST_ARGS);
    let read = async {
        let playlist_id = args.string("playlist_id")?;
        let change = MetaChange {
            name: name_arg(&args)?,
            comment: args.opt_text("comment", COMMENT_MAX_CHARS)?,
            public: args.opt_bool("public")?,
            allow_duplicate_name: args.opt_bool("allow_duplicate_name")?.unwrap_or(false),
        };
        let key = request_key(&args)?;
        args.finish()?;
        if change.name.is_none() && change.comment.is_none() && change.public.is_none() {
            return Err(invalid_value(
                args.tool(),
                "name",
                "given when comment and public are not",
            ));
        }
        let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
        Ok::<_, ToolError>((playlist_id, change, key, navi))
    };
    let (playlist_id, change, key, navi) = match read.await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let ctx = caller.ctx.clone();
    let work = async move {
        match playlists::update(&navi, &ctx, &playlist_id, &change).await {
            Ok(w) => Keyed::Kept(success(playlist_fields(&w))),
            Err(r) => refused(&args, Some(&playlist_id), &r),
        }
    };
    keyed(&state.acquire, caller, "update_playlist", key, &call, work).await
}

/// `delete_playlist`: one of the caller's playlists gone, answered with what it was.
pub(crate) async fn delete_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("delete_playlist", args, DELETE_PLAYLIST_ARGS);
    let read = async {
        let playlist_id = args.string("playlist_id")?;
        let key = request_key(&args)?;
        args.finish()?;
        let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
        Ok::<_, ToolError>((playlist_id, key, navi))
    };
    let (playlist_id, key, navi) = match read.await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let ctx = caller.ctx.clone();
    let work = async move {
        match playlists::delete(&navi, &ctx, &playlist_id).await {
            Ok(before) => Keyed::Kept(success(object(json!(before)))),
            Err(r) => refused(&args, Some(&playlist_id), &r),
        }
    };
    keyed(&state.acquire, caller, "delete_playlist", key, &call, work).await
}

/// `value`'s fields; it is a serialized struct.
fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        _ => unreachable!("a struct serializes as a JSON object"),
    }
}

/// The `name` argument, trimmed: given, it is 1 to 255 characters.
fn name_arg(args: &Args) -> Result<Option<String>, ToolError> {
    let refused = || invalid_value(args.tool(), "name", A_NAME);
    match args.opt_string("name").map_err(|_| refused())? {
        Some(name) if name.chars().count() <= NAME_MAX_CHARS => Ok(Some(name)),
        None if !args.has("name") => Ok(None),
        _ => Err(refused()),
    }
}

/// The playlist's facts, `song_ids` in Navidrome's order (null above 500 entries) and
/// `song_ids_omitted` saying so.
fn playlist_fields(w: &Written) -> Map<String, Value> {
    let mut fields = object(json!(w.playlist));
    let listed = w.song_ids.len() <= SONG_IDS_LISTED_MAX;
    fields.insert(
        "song_ids".into(),
        if listed {
            json!(w.song_ids)
        } else {
            Value::Null
        },
    );
    let omitted = (!listed).then_some("more than 500 entries: get_playlist pages through them");
    fields.insert("song_ids_omitted".into(), json!(omitted));
    fields
}

/// The answer to a refusal; its key keeps it once a write request was sent, since the playlist
/// may have changed.
fn refused(args: &Args, playlist_id: Option<&str>, r: &PlaylistRefusal) -> Keyed {
    let result = failed(refusal_error(args, playlist_id, r));
    if r.after_write() {
        Keyed::Kept(result)
    } else {
        Keyed::Released(result)
    }
}

/// The tool error for a refusal of the call `args` on `playlist_id` (None for a create): its
/// failure, facts naming the playlist and the refusal's lists, and the calls that can make
/// progress. Names from Navidrome and the caller go into facts only.
fn refusal_error(args: &Args, playlist_id: Option<&str>, r: &PlaylistRefusal) -> ToolError {
    let failure = r.failure();
    let mut facts = Map::new();
    facts.insert("playlist_id".into(), json!(playlist_id));
    let mut fact = |key: &str, value: Value| facts.insert(key.into(), value);
    // The call to offer, None when it is too large to repeat.
    let next: Option<NextCall> = match r {
        PlaylistRefusal::Failed(f) | PlaylistRefusal::WriteFailed(f)
            if f.code != FailureCode::PlaylistNotFound =>
        {
            return within_cap(upstream_error(failure, facts, args.same_call()));
        }
        PlaylistRefusal::Failed(_)
        | PlaylistRefusal::WriteFailed(_)
        | PlaylistRefusal::NotOwner { .. } => Some(list_playlists_call()),
        PlaylistRefusal::NameTaken { name, existing } => {
            fact("name", json!(cut_json(name.clone(), TEXT_MAX_CHARS)));
            fact("existing_playlist_ids", json!(existing));
            args.call_with(json!({"allow_duplicate_name": true}))
        }
        PlaylistRefusal::SongsNotPresent {
            unknown,
            missing_file,
        } => {
            fact("unknown_song_ids", json!(unknown));
            fact("missing_file_song_ids", json!(missing_file));
            let asked = args.opt_strings("song_ids", SONG_IDS_MAX).ok().flatten();
            let kept: Vec<String> = (asked.unwrap_or_default().into_iter())
                .filter(|id| !unknown.contains(id) && !missing_file.contains(id))
                .collect();
            args.call_with(json!({"song_ids": kept}))
        }
        PlaylistRefusal::TracksNotWritten {
            playlist_id,
            name,
            missing,
            unexpected,
            order_kept,
        } => {
            fact("playlist_id", json!(playlist_id));
            fact("name", json!(name));
            fact("missing_song_ids", json!(missing));
            fact("unexpected_song_ids", json!(unexpected));
            fact("order_kept", json!(order_kept));
            Some(get_playlist_call(playlist_id))
        }
        PlaylistRefusal::NotWritten {
            playlist_id, name, ..
        } => {
            fact("playlist_id", json!(playlist_id));
            fact("name", json!(name));
            Some(get_playlist_call(playlist_id))
        }
        PlaylistRefusal::Unfinished {
            playlist_id: id,
            described,
            ..
        } => {
            fact("playlist_id", json!(id));
            fact("created", json!(true));
            if *described {
                Some(get_playlist_call(id))
            } else {
                // The comment and public flag the call asked for, still to be written.
                let mut asked = json!({"playlist_id": id});
                if let Ok(Some(comment)) = args.opt_text("comment", COMMENT_MAX_CHARS) {
                    asked["comment"] = json!(comment);
                }
                if let Ok(Some(public)) = args.opt_bool("public") {
                    asked["public"] = json!(public);
                }
                Some(NextCall::new("update_playlist", asked))
            }
        }
    };
    let mut e = ToolError {
        failure,
        facts,
        tried: vec![],
        next: Vec::from_iter(next.clone()),
    };
    if next.is_none() {
        e.failure.message = too_large_to_repeat(&e.failure.message);
    }
    within_cap(e)
}

/// `e` with whole ids left out of the longest id list of its facts, again and again, until its
/// result is at most RESULT_MAX_CHARS long: the ids come from the call, up to 1000 of up to 500
/// characters each, and an id is never cut.
fn within_cap(mut e: ToolError) -> ToolError {
    loop {
        let document = json!({"ok": false, "error": error_object(&e)});
        let over = document
            .to_string()
            .chars()
            .count()
            .saturating_sub(RESULT_MAX_CHARS);
        let longest = (e.facts.iter())
            .filter(|(_, value)| value.as_array().is_some_and(|list| !list.is_empty()))
            .max_by_key(|(_, value)| value.to_string().chars().count())
            .map(|(key, _)| key.clone());
        match longest {
            Some(list) if over > 0 => leave_out(&mut e.facts, &list, Keep::First, over),
            _ => return e,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::connection::test_util::{call_tool, document, mcp_app, mcp_app_with, text_of};
    use crate::core::playlists::PlaylistRefusal;
    use crate::mcp::args::tests::obj;
    use crate::mcp::contract::failed;
    use crate::mcp::paging::RESULT_MAX_CHARS;
    use crate::test_navidrome::{
        native_songs, playlist_body, playlist_row, playlist_rows, playlists_body, subsonic_failed,
        subsonic_ok, NavidromeFor, NavidromeStub, ADVENT, AWAY, HOLY,
    };

    #[tokio::test]
    async fn get_playlist_of_an_unknown_id_offers_the_list() {
        let navi = NavidromeStub::start();
        // server/subsonic/playlists.go:46
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_failed(70, "playlist not found")],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let e = &document(
            &call_tool(app, &token, "get_playlist", json!({"playlist_id": "gone"})).await,
        )["error"];
        assert_eq!(
            (e["code"].as_str(), &e["next"]),
            (
                Some("playlist_not_found"),
                &json!([{"tool": "list_playlists", "args": {}}])
            )
        );
        assert_eq!(e["facts"], json!({"playlist_id": "gone"}));
    }

    #[tokio::test]
    async fn a_playlist_comes_with_a_page_of_its_entries() {
        let navi = NavidromeStub::start();
        // getPlaylist of alice's playlist (server/subsonic/playlists.go:41-57), its entries not read.
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_ok(
                r#""playlist":{"id":"9fR2kLm4NpQ7sT1vW3xY5z","name":"Christmas","comment":"traditional","songCount":2,"duration":526,"public":true,"owner":"alice","created":"2026-11-15T09:12:03.418Z","changed":"2026-11-15T09:14:47.002Z","readonly":false,"entry":[]}"#,
            )],
        );
        navi.answer(
            "/api/playlist/9fR2kLm4NpQ7sT1vW3xY5z/tracks",
            &[&format!(
                "[{},{}]",
                playlist_row(1, "s9", "Marrow Choir", "Silent Night", false),
                playlist_row(2, "s1", "Marrow Choir", "O Holy Night", true)
            )],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let args = json!({"playlist_id": "9fR2kLm4NpQ7sT1vW3xY5z", "limit": 1});
        let first = document(&call_tool(app.clone(), &token, "get_playlist", args.clone()).await);
        assert_eq!(
            first["playlist"],
            json!({"playlist_id": "9fR2kLm4NpQ7sT1vW3xY5z", "name": "Christmas", "comment": "traditional",
                "owner": "alice", "owned": true, "public": true, "song_count": 2, "duration_s": 526,
                "created": "2026-11-15T09:12:03.418Z", "changed": "2026-11-15T09:14:47.002Z"})
        );
        let e = &first["entries"][0];
        assert_eq!(
            (&e["position"], &e["missing"], &e["song_id"], &e["path"]),
            (
                &json!(0),
                &json!(false),
                &json!("s9"),
                &json!("Marrow Choir/Winter Songs/03 - Silent Night.flac")
            )
        );
        let mut next = args;
        next["cursor"] = first["cursor"].clone();
        let second = document(&call_tool(app, &token, "get_playlist", next).await);
        assert_eq!(
            (
                &second["entries"][0]["position"],
                &second["entries"][0]["missing"],
                &second["cursor"]
            ),
            (&json!(1), &json!(true), &Value::Null)
        );
    }

    #[tokio::test]
    async fn an_unreachable_navidrome_is_offered_again() {
        let navi = NavidromeStub::start();
        // nginx's 502 page (src/http/ngx_http_special_response.c)
        navi.answer_when("/rest/getPlaylists", "", 502, "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n");
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let e = &document(&call_tool(app, &token, "list_playlists", json!({"limit": 10})).await)
            ["error"];
        // NavidromeError::Http from the Subsonic path (lib/soulbeet/src/navidrome/client.rs, `subsonic`)
        assert_eq!(
            (
                e["code"].as_str(),
                e["stage"].as_str(),
                e["message"].as_str()
            ),
            (
                Some("navidrome_unreachable"),
                Some("playlist"),
                Some("Navidrome request failed: HTTP 502")
            )
        );
        assert_eq!(
            e["next"],
            json!([{"tool": "list_playlists", "args": {"limit": 10}}])
        );
    }

    #[tokio::test]
    async fn list_playlists_pages_through_what_the_caller_may_see() {
        let navi = NavidromeStub::start();
        // getPlaylists by name (server/subsonic/playlists.go:18-30); bob's private one is left out.
        let pl = |id: &str, name: &str, owner: &str, public: bool| {
            json!({"id": id, "name": name, "songCount": 1, "duration": 263, "public": public,
                "owner": owner, "created": "2026-11-10T18:00:00Z", "changed": "2026-11-10T18:00:00Z"})
        };
        navi.answer(
            "/rest/getPlaylists",
            &[&subsonic_ok(&format!(
                r#""playlists":{{"playlist":{}}}"#,
                json!([
                    pl("p1", "Advent", "bob", true),
                    pl("p2", "Bob's mix", "bob", false),
                    pl("p3", "Christmas", "alice", false),
                    pl("p4", "Epiphany", "alice", true)
                ])
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let first =
            document(&call_tool(app.clone(), &token, "list_playlists", json!({"limit": 2})).await);
        let names = |doc: &Value| {
            doc["playlists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p["name"].as_str().unwrap().to_string(),
                        p["owned"].as_bool().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&first),
            [
                ("Advent".to_string(), false),
                ("Christmas".to_string(), true)
            ]
        );
        let second = document(
            &call_tool(
                app,
                &token,
                "list_playlists",
                json!({"limit": 2, "cursor": first["cursor"]}),
            )
            .await,
        );
        assert_eq!(names(&second), [("Epiphany".to_string(), true)]);
        assert_eq!(second["cursor"], Value::Null);
    }

    fn tracks_of(id: &str) -> String {
        format!("/api/playlist/{id}/tracks")
    }

    #[tokio::test]
    async fn create_playlist_answers_the_playlist_as_navidrome_lists_it() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer("/api/song", &[&native_songs(&[(HOLY.0, false)])]);
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Advent",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "create_playlist",
                json!({"name": "Advent", "song_ids": [HOLY.0]}),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "playlist_id": ADVENT, "name": "Advent", "comment": null, "owner": "alice",
                "owned": true, "public": false, "song_count": 1, "duration_s": 263,
                "created": "2026-11-15T09:00:00.123456Z", "changed": "2026-11-15T09:00:04.5Z",
                "song_ids": [HOLY.0], "song_ids_omitted": null})
        );
    }

    #[test]
    fn a_long_playlist_names_its_songs_in_get_playlist() {
        let playlist = format!(
            r#"{{"id":"{ADVENT}","name":"Advent","songCount":501,"duration":131763,"owner":"alice","created":"2026-11-15T09:00:00.123456Z","changed":"2026-11-15T09:00:04.5Z"}}"#
        );
        let w = playlists::Written {
            playlist: playlists::facts(&serde_json::from_str(&playlist).unwrap(), "alice"),
            song_ids: vec![HOLY.0.to_string(); 501],
        };
        let fields = playlist_fields(&w);
        assert_eq!(
            (&fields["song_ids"], &fields["song_ids_omitted"]),
            (
                &Value::Null,
                &json!("more than 500 entries: get_playlist pages through them")
            )
        );
    }

    #[tokio::test]
    async fn unknown_songs_come_back_with_the_call_that_leaves_them_out() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer("/api/song", &[&native_songs(&[(HOLY.0, false)])]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let call = json!({"name": "Advent", "song_ids": [HOLY.0, AWAY.0], "request_key": "k1"});
        let result = call_tool(app.clone(), &token, "create_playlist", call.clone()).await;
        assert_eq!(result["isError"], true);
        let error = &document(&result)["error"];
        assert_eq!(
            (error["code"].as_str(), error["stage"].as_str()),
            (Some("unknown_handle"), Some("input"))
        );
        assert_eq!(
            error["facts"],
            json!({"playlist_id": null, "unknown_song_ids": [AWAY.0], "missing_file_song_ids": []})
        );
        assert_eq!(
            error["next"],
            json!([{"tool": "create_playlist", "args": {"name": "Advent", "song_ids": [HOLY.0]}}])
        );
        // Nothing was written, so the key is free: the same call runs again.
        call_tool(app, &token, "create_playlist", call).await;
        assert_eq!(navi.asked().iter().filter(|p| *p == "/api/song").count(), 2);
    }

    #[tokio::test]
    async fn a_taken_name_offers_a_duplicate() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[("p-old", "Advent", "alice", false)])],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let error = &document(
            &call_tool(
                app,
                &token,
                "create_playlist",
                json!({"name": "Advent", "song_ids": [HOLY.0]}),
            )
            .await,
        )["error"];
        assert_eq!(error["code"], "playlist_name_taken");
        assert_eq!(
            error["facts"],
            json!({"playlist_id": null, "name": "Advent", "existing_playlist_ids": ["p-old"]})
        );
        assert_eq!(
            error["next"],
            json!([{"tool": "create_playlist", "args": {"name": "Advent", "song_ids": [HOLY.0], "allow_duplicate_name": true}}])
        );
    }

    #[tokio::test]
    async fn a_user_without_navidrome_is_told_what_to_fix() {
        let (_, app, token) = mcp_app().await;
        let error = &document(
            &call_tool(
                app,
                &token,
                "delete_playlist",
                json!({"playlist_id": ADVENT}),
            )
            .await,
        )["error"];
        // NavidromeLookupError::NotLinked's text and failure (api/src/services.rs)
        assert_eq!(
            (
                error["code"].as_str(),
                error["stage"].as_str(),
                error["message"].as_str(),
                error["user_action"]["code"].as_str()
            ),
            (
                Some("navidrome_not_linked"),
                Some("playlist"),
                Some("Navidrome not configured for user"),
                Some("relink_navidrome")
            )
        );
        assert_eq!(error["next"], json!([]));
    }

    #[tokio::test]
    async fn an_unreachable_navidrome_offers_the_same_call_without_its_key() {
        let navi = NavidromeStub::start();
        // nginx's 502 page (src/http/ngx_http_special_response.c)
        navi.answer_when("/rest/getPlaylist", "", 502, "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n");
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let error = &document(
            &call_tool(
                app,
                &token,
                "update_playlist",
                json!({"playlist_id": ADVENT, "public": true, "request_key": "k2"}),
            )
            .await,
        )["error"];
        // NavidromeError::Http from the Subsonic path (lib/soulbeet/src/navidrome/client.rs, `subsonic`)
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (
                Some("navidrome_unreachable"),
                Some("Navidrome request failed: HTTP 502")
            )
        );
        assert_eq!(
            error["next"],
            json!([{"tool": "update_playlist", "args": {"playlist_id": ADVENT, "public": true}}])
        );
    }

    #[tokio::test]
    async fn a_repeated_create_with_its_request_key_makes_one_playlist() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(&tracks_of(ADVENT), &["[]"]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let args = json!({"name": "Advent", "request_key": "advent-1"});
        let first = call_tool(app.clone(), &token, "create_playlist", args.clone()).await;
        assert_eq!(first["isError"], false);
        assert_eq!(call_tool(app, &token, "create_playlist", args).await, first);
        assert_eq!(
            navi.asked()
                .iter()
                .filter(|p| *p == "/rest/createPlaylist")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_failed_comment_after_the_create_offers_to_set_it_again() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        // mapToSubsonicError's "Internal Server Error: {err}", server/subsonic/api.go:290-291
        navi.answer(
            "/rest/updatePlaylist",
            &[&subsonic_failed(
                0,
                "Internal Server Error: database is locked",
            )],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let args = json!({"name": "Advent", "comment": " Carols ", "request_key": "advent-1"});
        let first = call_tool(app.clone(), &token, "create_playlist", args.clone()).await;
        let error = &document(&first)["error"];
        assert_eq!(
            (error["code"].as_str(), &error["facts"], &error["next"]),
            (
                Some("navidrome_refused"),
                &json!({"playlist_id": ADVENT, "created": true}),
                &json!([{"tool": "update_playlist", "args": {"playlist_id": ADVENT, "comment": "Carols"}}])
            )
        );
        // The playlist exists, so the key keeps the answer.
        assert_eq!(call_tool(app, &token, "create_playlist", args).await, first);
        assert_eq!(
            navi.asked()
                .iter()
                .filter(|p| *p == "/rest/createPlaylist")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn update_playlist_needs_something_to_change() {
        let (_, app, token) = mcp_app().await;
        let error = &document(
            &call_tool(
                app,
                &token,
                "update_playlist",
                json!({"playlist_id": ADVENT}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["message"].as_str(), error["facts"]["field"].as_str()),
            (
                Some("update_playlist: name must be given when comment and public are not"),
                Some("name")
            )
        );
    }

    #[tokio::test]
    async fn a_name_is_1_to_255_characters() {
        let (_, app, token) = mcp_app().await;
        for name in [json!(" "), json!("x".repeat(256)), json!(7)] {
            let error = &document(
                &call_tool(
                    app.clone(),
                    &token,
                    "update_playlist",
                    json!({"playlist_id": ADVENT, "name": name}),
                )
                .await,
            )["error"];
            assert_eq!(
                error["message"], "update_playlist: name must be a name of 1 to 255 characters",
                "{name}"
            );
        }
        let error = &document(&call_tool(app, &token, "create_playlist", json!({})).await)["error"];
        assert_eq!(
            error["message"],
            "create_playlist: name must be a name of 1 to 255 characters"
        );
    }

    #[tokio::test]
    async fn delete_playlist_answers_what_the_playlist_was() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[
                &playlist_body(ADVENT, "Advent", "alice", true, "Carols", &[HOLY]),
                &subsonic_failed(70, "playlist not found"), // server/subsonic/playlists.go:46
            ],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "delete_playlist",
                json!({"playlist_id": ADVENT}),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "playlist_id": ADVENT, "name": "Advent", "comment": "Carols", "owner": "alice",
                "owned": true, "public": true, "song_count": 1, "duration_s": 263,
                "created": "2026-11-15T09:00:00.123456Z", "changed": "2026-11-15T09:00:04.5Z"})
        );
        // Now gone: the list to pick another from.
        let error = &document(
            &call_tool(
                app,
                &token,
                "delete_playlist",
                json!({"playlist_id": ADVENT}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), &error["facts"], &error["next"]),
            (
                Some("playlist_not_found"),
                &json!({"playlist_id": ADVENT}),
                &json!([{"tool": "list_playlists", "args": {}}])
            )
        );
    }

    #[tokio::test]
    async fn update_playlist_answers_the_playlist_as_navidrome_shows_it() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer(
            "/rest/getPlaylist",
            &[
                &playlist_body(ADVENT, "Advent", "alice", false, "", &[HOLY]),
                &playlist_body(ADVENT, "Advent 2026", "alice", false, "Carols", &[HOLY]),
            ],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "update_playlist",
                json!({"playlist_id": ADVENT, "name": "Advent 2026", "comment": "Carols"}),
            )
            .await,
        );
        assert_eq!(
            (&doc["name"], &doc["comment"], &doc["song_ids"]),
            (&json!("Advent 2026"), &json!("Carols"), &json!([HOLY.0]))
        );
    }

    #[test]
    fn a_refusal_naming_many_long_ids_stays_within_the_result_cap() {
        let unknown: Vec<String> = (0..1000).map(|i| format!("{i:0>400}")).collect();
        let args = Args::new(
            "create_playlist",
            obj(json!({"name": "Advent", "song_ids": unknown})),
            CREATE_PLAYLIST_ARGS,
        );
        let (missing, unknown) = unknown.split_at(400);
        let refusal = PlaylistRefusal::SongsNotPresent {
            unknown: unknown.to_vec(),
            missing_file: missing.to_vec(),
        };
        let e = refusal_error(&args, None, &refusal);
        // Ids leave the longest list first, from its end, each counted.
        let left = |list: &str| e.facts[list].as_array().unwrap().len();
        let (unknown_left, missing_left) =
            (left("unknown_song_ids"), left("missing_file_song_ids"));
        assert_eq!(
            (
                &e.facts["unknown_song_ids_omitted"],
                &e.facts["missing_file_song_ids_omitted"]
            ),
            (&json!(600 - unknown_left), &json!(400 - missing_left))
        );
        assert!(
            unknown_left + missing_left > 100,
            "{unknown_left} {missing_left}"
        );
        assert_eq!(
            e.next,
            vec![NextCall::new(
                "create_playlist",
                json!({"name": "Advent", "song_ids": []})
            )]
        );
        assert!(text_of(&failed(e)).chars().count() <= RESULT_MAX_CHARS);
        // The call offered with a duplicate name is too large to offer again.
        let taken = PlaylistRefusal::NameTaken {
            name: "Advent".into(),
            existing: vec!["p-old".into()],
        };
        let e = refusal_error(&args, None, &taken);
        assert_eq!(
            (e.next.len(), e.failure.message.as_str()),
            (
                0,
                "You already have a playlist with this name. The call is too large to offer again in next."
            )
        );
    }
}
