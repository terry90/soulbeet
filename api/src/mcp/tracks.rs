//! Track management tools: inspecting library tracks and re-tagging/re-matching via beets.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, RAW_MAX_CHARS};
use soulbeet::{DriverChoice, TagEdits};

use super::args::Args;
use super::contract::{failed, items, upstream_error, ItemResult, Items, ToolError};
use super::deletion::deletion_off_at;
use super::keys::{keyed, request_key, Keyed};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::sync::cut;
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::delete_guard::{Guard, Origin};
use crate::core::tracks::inspect::{self, InspectedTrack};
use crate::core::tracks::retag::{self, RetagEntry, RetagMode, RetagReport, RetagRequest};
use crate::core::tracks::{TrackSelection, ALBUM_MAX_TRACKS, SONGS_PER_CALL};

const INSPECT_ARGS: &[&str] = &["song_ids"];

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
        format!("Inspects up to {SONGS_PER_CALL} tracks in this user's Navidrome library: exact local file path, which Soulbeet folder holds each track (folder_id, name, path, in_navidrome), Navidrome library id, beets database entry and stored tags, album peer count and album rating/star, caller's playlists, whether other listeners see the track or hold it in a static playlist (never naming who), sidecar files (.lrc, cover art), and whether it is a pending discovery track. Read-only; does not require the file-management switch. Failure codes: {common}, {NAVIDROME_CODES}, partial, and per song unknown_handle.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["song_ids"], "properties": {"song_ids": {"type": "array", "minItems": 1, "maxItems": 25, "uniqueItems": true, "items": {"type": "string", "minLength": 1}}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(inspect_tracks(state, caller, args)),
};

pub(crate) const RETAG_TRACKS: ToolSpec = ToolSpec {
    name: "retag_tracks",
    title: "Re-tag or re-match tracks",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Edits tags directly (tags) or re-matches against MusicBrainz via beets (rematch) for up to {SONGS_PER_CALL} songs (song_ids) or one whole album (album_id, up to {ALBUM_MAX_TRACKS} tracks). By default (relocate: false), tags are written in place without renaming or moving the file so Navidrome keeps the same song_id, ratings, stars, play counts and playlists. Hardlinked shared files are automatically detached via copy-on-write before writing tags so other users' copies are never modified. With relocate: true, Soulbeet runs a 2-scan dance (write tags -> Navidrome scan -> beet move -> Navidrome scan) so song_id stays intact; relocate: true requires a linked Navidrome admin account (otherwise refused retag_relocate_needs_scan). Pass dry_run: true to preview current tags or MusicBrainz candidates without writing anything (dry_run does not need the file-management switch). ALWAYS ask the user before calling retag_tracks with dry_run: false. Executing (dry_run: false) requires 'Allow managing library files (delete, re-tag)' switched on in Settings > Users > Connected apps. Failure codes: {common}, {NAVIDROME_CODES}, permission_not_granted, unknown_handle, already_running, retag_relocate_needs_scan, partial, and per song retag_failed, import_no_match, delete_path_unresolved, delete_outside_library, delete_not_a_track, delete_in_use.")
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
    use soulbeet::{BeetsTrackInfo, ImportOutcome, ImportResult, Relocation, SkipReason};

    use crate::connection::test_util::{access_token, call_tool, document, RESOURCE};
    use crate::connection::{router, ConnectionState};
    use crate::core::delete_guard::test_support::{mtime, Library};
    use crate::core::jobs::runner::{Acquire, TestEffects};
    use crate::import_queue::test_support::{candidate, no_match, track_choices};
    use crate::test_navidrome::{subsonic_failed, subsonic_ok, NavidromeFor, NavidromeStub};

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

    #[tokio::test]
    async fn retag_requires_switch_for_writes_but_allows_dry_run_and_inspect() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        let (app, token, _) = track_app(&lib, Some(&navi), false).await;

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
        assert_eq!(err["user_action"]["code"], "enable_deletion");

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

        // inspect_tracks works without switch
        let inspected = document(
            &call_tool(app, &token, "inspect_tracks", json!({"song_ids": ["s1"]})).await,
        );
        assert_eq!(inspected["ok"], true, "{inspected}");
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

    #[tokio::test]
    async fn admin_with_allow_admin_can_inspect_other_users_track() {
        let lib = Library::new().await;
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();
        lib.song("s-bob", "bob/B/y.flac", b"audio");
        // Hide s-bob from alice in Navidrome so admin fallback to bob's linked account is tested.
        lib.fake.unsee("alice", "s-bob");

        let (app, token, _) = track_app(&lib, None, true).await;

        // Enable allow_admin on alice's grant.
        sqlx::query("UPDATE oauth_grants SET allow_admin = 1 WHERE user_id = 'alice'")
            .execute(&lib.db)
            .await
            .unwrap();

        // inspect_tracks finds s-bob via bob's linked account and shows bob's folder owner.
        let inspected = document(
            &call_tool(app, &token, "inspect_tracks", json!({"song_ids": ["s-bob"]})).await,
        );
        assert_eq!(inspected["ok"], true, "{inspected}");
        assert_eq!(inspected["results"][0]["folder"]["folder_id"], "bob-music");
        assert_eq!(inspected["results"][0]["folder"]["owner"], "bob");
        assert_eq!(inspected["results"][0]["folder"]["is_own"], false);
    }
}
