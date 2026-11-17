//! The discovery tools: the caller's discovery, a run started now, and keeping or removing a
//! pending track.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, FailureCode, TEXT_MAX_CHARS};
use shared::navidrome::{
    DiscoveryProgress, DiscoveryTrack, GenerationStatus, ProfilePhase, DAILY_APP_DELETIONS,
};
use shared::recommendation::DiscoveryProfile;

use super::args::Args;
use super::contract::{answer, failed, success, upstream_error, NextCall, ToolError};
use super::keys::{keyed, request_key, Keyed};
use super::paging::{leave_out, length, Keep, RESULT_MAX_CHARS};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::delete_guard::Origin;
use crate::core::discovery::{
    self, expires_at, lifetime_of, parse_profiles, NotDone, Overview, Replace,
};
use crate::models::user_settings::UserSettings;

/// The most pending tracks get_discovery lists: three profiles of at most 50 tracks.
const PENDING_MAX: usize = 150;

const RUN_DISCOVERY_ARGS: &[&str] = &["request_key"];

const TRACK_ARGS: &[&str] = &["track_id", "request_key"];

pub(crate) const GET_DISCOVERY: ToolSpec = ToolSpec {
    name: "get_discovery",
    title: "Discovery status",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows this user's discovery: whether it is on, its folder, each profile's track count, lifetime and Navidrome playlist, the promote threshold, the current run or one that ended in the last 5 minutes, and the pending tracks (at most 150, soonest to expire first) with when each expires; the automation removes an expired track at its next round. Track ids stay valid; only pending tracks can be kept or removed. Takes no arguments. Failure codes: {common}.")
    },
    input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_discovery(state, caller, args)),
};

pub(crate) const RUN_DISCOVERY: ToolSpec = ToolSpec {
    name: "run_discovery",
    title: "Run discovery now",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Starts a discovery run for this user: recommendations from Last.fm or ListenBrainz, Soulseek searches, downloads into the discovery folder, imports, and the profile playlists. Returns at once with the run's progress; get_discovery shows how it goes. A run takes minutes. Failure codes: {common}, folder_required (no discovery folder), already_running (a run for this user is going, or a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(run_discovery(state, caller, args)),
};

pub(crate) const PROMOTE_DISCOVERY_TRACK: ToolSpec = ToolSpec {
    name: "promote_discovery_track",
    title: "Keep a discovery track",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Moves a pending discovery track into this user's library folder through beets, as rating it at the promote threshold would at the next rating sync, except that without deletion switched on for this app an earlier library copy of the song is never replaced: the promote then ends import_already_in_library, the track stays pending, and next offers removing the staged copy. With the switch on, an earlier copy is replaced without a preview, as this app's deletion: counted in the {DAILY_APP_DELETIONS} tracks a day connected apps may delete and named in the deletion history. track_id comes from get_discovery. Takes seconds to a minute. Failure codes: {common}, unknown_handle (no pending track of this user has this id), expired_handle (no longer pending: promoted, removed or being promoted), import_files_gone (the staged file is gone; next offers its removal), the import_* codes, already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["track_id"], "properties": {"track_id": {"type": "string", "minLength": 1, "maxLength": 500}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: false,
    open_world: false,
    call: |state, caller, args| Box::pin(promote_discovery_track(state, caller, args)),
};

pub(crate) const REMOVE_DISCOVERY_TRACK: ToolSpec = ToolSpec {
    name: "remove_discovery_track",
    title: "Remove a discovery track",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Deletes a pending discovery track's file from the discovery folder and marks it removed, as the Remove button does. The file was never in the library. track_id comes from get_discovery. Failure codes: {common}, unknown_handle (no pending track of this user has this id), expired_handle (no longer pending), delete_outside_library (the stored file is not inside its profile folder), delete_failed, already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["track_id"], "properties": {"track_id": {"type": "string", "minLength": 1, "maxLength": 500}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(remove_discovery_track(state, caller, args)),
};

/// `get_discovery`, which takes no arguments.
async fn get_discovery(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_DISCOVERY.name, args, &[]);
    answer(
        async {
            args.finish()?;
            let overview = discovery::overview_in(&state.db, state.discovery.as_ref(), &caller.ctx)
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(overview_fields(&overview))
        }
        .await,
    )
}

/// The get_discovery document. Every profile value is the one discovery runs with. The pending
/// tracks that do not fit in one answer are left out from the end and counted.
fn overview_fields(o: &Overview) -> Map<String, Value> {
    let s = &o.settings;
    let active = parse_profiles(&s.discovery_profiles);
    let lifetimes = s.parse_lifetime_days();
    let profiles: Vec<Value> = DiscoveryProfile::ALL
        .iter()
        .map(|profile| {
            let name = profile.to_string();
            json!({
                "profile": name,
                "active": active.contains(profile),
                "track_count": s.track_count_for_profile(&name),
                "lifetime_days": lifetime_of(&lifetimes, &name),
                "playlist_name": UserSettings::get_playlist_name_for_profile(&s.discovery_playlist_name, &name),
                "navidrome_playlist_id": UserSettings::get_playlist_id_for_profile(&s.discovery_navidrome_playlist_id, &name),
            })
        })
        .collect();
    let pending: Vec<Value> = (o.pending.iter().take(PENDING_MAX))
        .map(|track| {
            let cut = |text: &str| cut_json(text.to_string(), TEXT_MAX_CHARS);
            json!({
                "track_id": track.id,
                "profile": track.profile,
                "artist": cut(&track.artist),
                "title": cut(&track.title),
                "album": cut(&track.album),
                "song_id": track.song_id,
                "rating": track.rating,
                "created_at": track.created_at,
                "expires_at": expires_at(track, &lifetimes).map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    let folder = (o.folder.as_ref()).map(|f| json!({"folder_id": f.id, "name": f.name}));
    let Value::Object(mut fields) = json!({
        "enabled": s.discovery_enabled,
        "folder": folder,
        "promote_threshold": s.discovery_promote_threshold,
        "last_generated_at": s.discovery_last_generated_at,
        "profiles": profiles,
        "run": o.run.as_ref().map(run_fields),
        "pending": pending,
        "pending_total": o.pending.len(),
    }) else {
        unreachable!("an object literal is an object");
    };
    // As `success` sends it.
    let sent = length(&fields) + r#""ok":true,"#.len();
    leave_out(
        &mut fields,
        "pending",
        Keep::First,
        sent.saturating_sub(RESULT_MAX_CHARS),
    );
    fields
}

/// A run as get_discovery and run_discovery show it.
fn run_fields(p: &DiscoveryProgress) -> Value {
    let status = match p.status {
        GenerationStatus::Idle => "idle",
        GenerationStatus::Running => "running",
        GenerationStatus::Complete => "complete",
        GenerationStatus::Error => "error",
    };
    let profiles: Vec<Value> = (p.profiles.iter())
        .map(|pp| {
            let phase = match pp.phase {
                ProfilePhase::Waiting => "waiting",
                ProfilePhase::PullingCandidates => "pulling_candidates",
                ProfilePhase::GeneratingRecommendations => "generating_recommendations",
                ProfilePhase::SearchingSoulseek => "searching_soulseek",
                ProfilePhase::Downloading => "downloading",
                ProfilePhase::Importing => "importing",
                ProfilePhase::Done => "done",
                ProfilePhase::Skipped => "skipped",
            };
            json!({"profile": pp.profile, "phase": phase, "current": pp.current, "total": pp.total})
        })
        .collect();
    json!({
        "status": status,
        "started_at": p.started_at,
        "completed_at": p.completed_at,
        "profiles": profiles,
        "result": p.result,
        "failure": p.failure,
    })
}

/// `run_discovery`: a run started for the caller, answered at once.
async fn run_discovery(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(RUN_DISCOVERY.name, args, RUN_DISCOVERY_ARGS);
    let read = || {
        let key = request_key(&args)?;
        args.finish()?;
        Ok::<_, ToolError>(key)
    };
    let key = match read() {
        Ok(key) => key,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let (db, effects, ctx) = (
        state.db.clone(),
        state.discovery.clone(),
        caller.ctx.clone(),
    );
    let work = async move {
        match discovery::start_in(&db, effects.as_ref(), &ctx).await {
            Ok(progress) => {
                let fields = Map::from_iter([("run".to_string(), run_fields(&progress))]);
                Keyed::Kept(success(fields))
            }
            // Every refusal comes before the run is claimed.
            Err(f) => Keyed::Released(failed(start_refused(&args, f))),
        }
    };
    keyed(&state.acquire, caller, RUN_DISCOVERY.name, key, &call, work).await
}

/// A run not started, with the call that can make one possible.
fn start_refused(args: &Args, failure: Failure) -> ToolError {
    let next = match failure.code {
        FailureCode::InvalidInput => {
            NextCall::new("update_settings", json!({"discovery_enabled": true}))
        }
        FailureCode::FolderRequired => NextCall::new("list_folders", json!({})),
        FailureCode::AlreadyRunning => NextCall::new("get_discovery", json!({})),
        _ => return upstream_error(failure, Map::new(), args.same_call()),
    };
    ToolError {
        failure,
        facts: Map::new(),
        tried: vec![],
        next: vec![next],
    }
}

/// The `track_id` and request key of a promote or removal.
fn read_track_call(args: &Args) -> Result<(String, Option<String>), ToolError> {
    let track_id = args.string("track_id")?;
    let key = request_key(args)?;
    args.finish()?;
    Ok((track_id, key))
}

/// `promote_discovery_track`: the caller's pending track moved into the library. An earlier
/// copy there is replaced only when the app may delete, as the app (checked against its switch
/// and the day's room when it is removed, and named in the history).
async fn promote_discovery_track(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(PROMOTE_DISCOVERY_TRACK.name, args, TRACK_ARGS);
    let (track_id, key) = match read_track_call(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let replace = match caller.allow_delete {
        true => Replace::Guarded(Origin::Mcp {
            grant_id: caller.grant_id.clone(),
        }),
        false => Replace::Never,
    };
    let call = args.as_value();
    let (db, effects, user) = (state.db.clone(), state.discovery.clone(), caller.ctx.id());
    let work = async move {
        let promoted = discovery::promote(&db, &effects, &user, &track_id, replace).await;
        track_answer(&args, &track_id, promoted, "promoted")
    };
    let name = PROMOTE_DISCOVERY_TRACK.name;
    keyed(&state.acquire, caller, name, key, &call, work).await
}

/// `remove_discovery_track`: the caller's pending track's staged file deleted.
async fn remove_discovery_track(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(REMOVE_DISCOVERY_TRACK.name, args, TRACK_ARGS);
    let (track_id, key) = match read_track_call(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let (db, user) = (state.db.clone(), caller.ctx.id());
    let work = async move {
        let removed = discovery::remove_in(&db, &user, &track_id).await;
        track_answer(&args, &track_id, removed, "removed")
    };
    let name = REMOVE_DISCOVERY_TRACK.name;
    keyed(&state.acquire, caller, name, key, &call, work).await
}

/// The answer to a promote or removal of `track_id`. A refusal made before the track was
/// claimed lets the key go; a failure after the claim is kept.
fn track_answer(
    args: &Args,
    track_id: &str,
    result: Result<DiscoveryTrack, NotDone>,
    status: &str,
) -> Keyed {
    let not_done = match result {
        Ok(track) => {
            let cut = |text: String| cut_json(text, TEXT_MAX_CHARS);
            let Value::Object(fields) = json!({
                "track_id": track.id,
                "artist": cut(track.artist),
                "title": cut(track.title),
                "status": status,
            }) else {
                unreachable!("an object literal is an object");
            };
            return Keyed::Kept(success(fields));
        }
        Err(not_done) => not_done,
    };
    let released = matches!(not_done, NotDone::Unclaimed(_));
    let failure = not_done.into_failure();
    use FailureCode::*;
    let next = match failure.code {
        UnknownHandle | ExpiredHandle => Some(NextCall::new("get_discovery", json!({}))),
        ImportFilesGone | ImportAlreadyInLibrary => Some(NextCall::new(
            "remove_discovery_track",
            json!({"track_id": track_id}),
        )),
        _ => None,
    };
    let facts = Map::from_iter([("track_id".to_string(), json!(track_id))]);
    let error = match next {
        Some(next) => ToolError {
            failure,
            facts,
            tried: vec![],
            next: vec![next],
        },
        None => upstream_error(failure, facts, args.same_call()),
    };
    match released {
        true => Keyed::Released(failed(error)),
        false => Keyed::Kept(failed(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use serde_json::json;
    use soulbeet::{ImportOutcome, ImportResult, SkipReason};

    use crate::connection::test_util::{access_token, call_tool, document, grant_of, RESOURCE};
    use crate::connection::{router, ConnectionState};
    use crate::core::delete_guard::test_support::Library;
    use crate::core::delete_guard::Origin;
    use crate::core::discovery::test_support::TestDiscovery;
    use crate::core::discovery::Replace;
    use crate::import_queue::import_failure;
    use crate::import_queue::test_support::no_match;
    use crate::mcp::paging::RESULT_MAX_CHARS;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};

    async fn discovery_app(lib: &Library, effects: Arc<TestDiscovery>) -> (Router, String) {
        let token = access_token(&lib.db, "alice", RESOURCE).await;
        let state = ConnectionState {
            discovery: effects,
            ..ConnectionState::for_test(lib.db.clone())
        };
        (router(state), token)
    }

    #[tokio::test]
    async fn get_discovery_lists_pending_tracks_with_when_they_expire() {
        let lib = Library::new().await;
        UserSettings::upsert_in(
            &lib.db,
            "alice",
            UpdateUserSettings {
                discovery_enabled: Some(true),
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        lib.pending_track(
            "t1",
            "alice-music",
            "Balanced",
            &lib.root.join("music/alice/Discovery/Balanced/03.flac"),
            None,
        )
        .await;
        sqlx::query(
            "UPDATE discovery_tracks SET created_at = '2026-11-15 20:00:00' WHERE id = 't1'",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        let (app, token) = discovery_app(&lib, Arc::new(TestDiscovery::default())).await;
        let doc = document(&call_tool(app, &token, "get_discovery", json!({})).await);
        assert_eq!(
            (
                doc["pending_total"].as_u64(),
                doc["pending"][0]["track_id"].as_str(),
                doc["pending"][0]["expires_at"].as_str()
            ),
            (Some(1), Some("t1"), Some("2026-11-22T20:00:00+00:00"))
        );
        assert_eq!(
            (doc["folder"]["folder_id"].as_str(), doc["run"].is_null()),
            (Some("alice-music"), true)
        );
    }

    #[tokio::test]
    async fn only_the_callers_own_discovery_folder_is_listed() {
        // A stored choice of bob's folder, as no check of today allows but an older one did.
        let lib = Library::new().await;
        UserSettings::upsert_in(&lib.db, "alice", UpdateUserSettings::default())
            .await
            .unwrap();
        sqlx::query("UPDATE user_settings SET discovery_folder_id = 'bob-music'")
            .execute(&lib.db)
            .await
            .unwrap();
        let staged = lib.root.join("music/bob/Discovery/Balanced/03.flac");
        lib.pending_track("t1", "bob-music", "Balanced", &staged, None)
            .await;
        let (app, token) = discovery_app(&lib, Arc::new(TestDiscovery::default())).await;
        let doc = document(&call_tool(app, &token, "get_discovery", json!({})).await);
        assert_eq!(
            (
                &doc["folder"],
                &doc["pending"],
                doc["pending_total"].as_u64()
            ),
            (&json!(null), &json!([]), Some(0))
        );
    }

    #[tokio::test]
    async fn at_most_150_pending_tracks_are_listed_and_a_long_list_is_cut_and_counted() {
        let lib = Library::new().await;
        let folder = UpdateUserSettings {
            discovery_folder_id: Some("alice-music".into()),
            ..Default::default()
        };
        UserSettings::upsert_in(&lib.db, "alice", folder)
            .await
            .unwrap();
        let long = "x".repeat(400);
        for i in 0..151 {
            let id = format!("t{i:03}");
            let staged = lib
                .root
                .join(format!("music/alice/Discovery/Balanced/{id}.flac"));
            lib.pending_track(&id, "alice-music", "Balanced", &staged, None)
                .await;
        }
        let (app, token) = discovery_app(&lib, Arc::new(TestDiscovery::default())).await;
        let doc = document(&call_tool(app.clone(), &token, "get_discovery", json!({})).await);
        assert_eq!(
            (
                doc["pending"].as_array().map(Vec::len),
                doc["pending_total"].as_u64(),
                doc.get("pending_omitted")
            ),
            (Some(150), Some(151), None)
        );
        // Tags long enough that 150 tracks do not fit in one answer.
        sqlx::query("UPDATE discovery_tracks SET title = ?, artist = ?, album = ?")
            .bind(&long)
            .bind(&long)
            .bind(&long)
            .execute(&lib.db)
            .await
            .unwrap();
        let result = call_tool(app, &token, "get_discovery", json!({})).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= RESULT_MAX_CHARS);
        let doc = document(&result);
        let (shown, omitted) = (
            doc["pending"].as_array().unwrap().len() as u64,
            doc["pending_omitted"].as_u64().unwrap(),
        );
        assert_eq!(
            (shown + omitted, doc["pending_total"].as_u64()),
            (150, Some(151))
        );
        assert_eq!(doc["pending"][0]["track_id"], "t000");
    }

    #[tokio::test]
    async fn a_promote_that_fails_after_its_claim_keeps_its_key() {
        // The import is done, then the database refuses to mark the track promoted.
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        sqlx::query(
            "CREATE TRIGGER refuse_promoted BEFORE UPDATE ON discovery_tracks \
             WHEN NEW.status = 'Promoted' BEGIN SELECT RAISE(FAIL, 'disk I/O error'); END",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        let effects = Arc::new(TestDiscovery::default());
        let (app, token) = discovery_app(&lib, effects.clone()).await;
        let call = json!({"track_id": "t1", "request_key": "k"});
        let first = call_tool(app.clone(), &token, "promote_discovery_track", call.clone()).await;
        assert_eq!(
            document(&first)["error"]["code"].as_str(),
            Some("settings_unreadable")
        );
        // The repeat answers the stored result instead of trying again.
        let again = call_tool(app, &token, "promote_discovery_track", call).await;
        assert_eq!(again["content"], first["content"]);
        assert_eq!(effects.imports().len(), 1);
    }

    #[tokio::test]
    async fn run_discovery_refuses_a_cleared_folder() {
        let lib = Library::new().await;
        let cleared = UpdateUserSettings {
            discovery_enabled: Some(true),
            discovery_folder_id: Some(String::new()),
            ..Default::default()
        };
        UserSettings::upsert_in(&lib.db, "alice", cleared)
            .await
            .unwrap();
        let effects = Arc::new(TestDiscovery::default());
        let (app, token) = discovery_app(&lib, effects.clone()).await;
        let error = &document(&call_tool(app, &token, "run_discovery", json!({})).await)["error"];
        assert_eq!(
            (error["code"].as_str(), &error["next"]),
            (
                Some("folder_required"),
                &json!([{"tool": "list_folders", "args": {}}])
            )
        );
        assert!(effects.generated().is_empty());
    }

    #[tokio::test]
    async fn run_discovery_says_what_is_missing_or_already_running() {
        let lib = Library::new().await;
        let effects = Arc::new(TestDiscovery::default());
        let (app, token) = discovery_app(&lib, effects.clone()).await;
        let error =
            &document(&call_tool(app.clone(), &token, "run_discovery", json!({})).await)["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["next"]
            ),
            (
                Some("invalid_input"),
                Some("Discovery is not enabled"),
                &json!([{"tool": "update_settings", "args": {"discovery_enabled": true}}])
            )
        );
        UserSettings::upsert_in(
            &lib.db,
            "alice",
            UpdateUserSettings {
                discovery_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let error =
            &document(&call_tool(app.clone(), &token, "run_discovery", json!({})).await)["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["user_action"]["where"].as_str(),
                &error["next"]
            ),
            (
                Some("folder_required"),
                Some("Settings > Library > Discovery"),
                &json!([{"tool": "list_folders", "args": {}}])
            )
        );
        UserSettings::upsert_in(
            &lib.db,
            "alice",
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            document(&call_tool(app.clone(), &token, "run_discovery", json!({})).await)["run"]
                ["status"],
            "running"
        );
        let error = &document(&call_tool(app, &token, "run_discovery", json!({})).await)["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["next"]
            ),
            (
                Some("already_running"),
                Some("Discovery generation already in progress"),
                &json!([{"tool": "get_discovery", "args": {}}])
            )
        );
        assert_eq!(effects.generated(), vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn a_promote_refusal_says_what_to_call_next() {
        let lib = Library::new().await;
        lib.pending_track(
            "t1",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/Discovery/Explore/gone.flac"),
            None,
        )
        .await;
        let bobs_root = lib.root.join("music/bob/Discovery/Explore/04.flac");
        lib.pending_track("t2", "bob-music", "Explore", &bobs_root, None)
            .await;
        let (app, token) = discovery_app(&lib, Arc::new(TestDiscovery::default())).await;
        let error = &document(
            &call_tool(
                app.clone(),
                &token,
                "promote_discovery_track",
                json!({"track_id": "no-such"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["next"]
            ),
            (
                Some("unknown_handle"),
                Some("Discovery track not found"),
                &json!([{"tool": "get_discovery", "args": {}}])
            )
        );
        let error = &document(
            &call_tool(
                app.clone(),
                &token,
                "promote_discovery_track",
                json!({"track_id": "t1"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), &error["facts"], &error["next"]),
            (
                Some("import_files_gone"),
                &json!({"track_id": "t1"}),
                &json!([{"tool": "remove_discovery_track", "args": {"track_id": "t1"}}])
            )
        );
        let error = &document(
            &call_tool(
                app,
                &token,
                "remove_discovery_track",
                json!({"track_id": "t2"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (Some("unknown_handle"), Some("Discovery track not found"))
        );
        assert_eq!(lib.track_status("t2").await, "Pending");
    }

    #[tokio::test]
    async fn a_kept_track_replaces_a_library_copy_only_when_the_app_may_delete() {
        let lib = Library::new().await;
        let staged = |name: &str| {
            let staged = lib.root.join("music/alice/Discovery/Explore").join(name);
            std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
            std::fs::write(&staged, b"audio").unwrap();
            staged
        };
        let (first, second) = (staged("03.flac"), staged("04.flac"));
        lib.pending_track("t1", "alice-music", "Explore", &first, None)
            .await;
        lib.pending_track("t2", "alice-music", "Explore", &second, None)
            .await;
        let effects = Arc::new(TestDiscovery::default());
        let (app, token) = discovery_app(&lib, effects.clone()).await;
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "promote_discovery_track",
                json!({"track_id": "t1"}),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "track_id": "t1", "artist": "Marrow Choir", "title": "t1", "status": "promoted"})
        );
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&lib.db)
            .await
            .unwrap();
        let doc = document(
            &call_tool(
                app,
                &token,
                "promote_discovery_track",
                json!({"track_id": "t2"}),
            )
            .await,
        );
        assert_eq!(doc["status"], "promoted");
        let music = lib.root.join("music/alice");
        let app = Origin::Mcp {
            grant_id: grant_of(&lib.db, "alice").await,
        };
        assert_eq!(
            effects.imports(),
            vec![
                (first, music.clone(), Replace::Never),
                (second, music, Replace::Guarded(app))
            ]
        );
    }

    #[tokio::test]
    async fn a_kept_track_the_library_has_goes_back_to_pending_and_its_removal_is_offered() {
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let effects = Arc::new(TestDiscovery::default());
        let skipped = ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
            ..no_match(0)
        };
        effects.fail_imports(import_failure(&skipped).unwrap());
        let (app, token) = discovery_app(&lib, effects.clone()).await;
        let error = &document(
            &call_tool(
                app,
                &token,
                "promote_discovery_track",
                json!({"track_id": "t1"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), &error["next"]),
            (
                Some("import_already_in_library"),
                &json!([{"tool": "remove_discovery_track", "args": {"track_id": "t1"}}])
            )
        );
        assert_eq!(effects.imports()[0].2, Replace::Never);
        assert_eq!(lib.track_status("t1").await, "Pending");
        assert!(staged.exists());
    }

    #[tokio::test]
    async fn a_removed_track_is_gone_and_a_second_removal_is_refused() {
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let (app, token) = discovery_app(&lib, Arc::new(TestDiscovery::default())).await;
        let remove = json!({"track_id": "t1"});
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "remove_discovery_track",
                remove.clone(),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "track_id": "t1", "artist": "Marrow Choir", "title": "t1", "status": "removed"})
        );
        assert!(!staged.exists());
        let error =
            &document(&call_tool(app, &token, "remove_discovery_track", remove).await)["error"];
        assert_eq!(
            (error["code"].as_str(), &error["next"]),
            (
                Some("expired_handle"),
                &json!([{"tool": "get_discovery", "args": {}}])
            )
        );
    }
}
