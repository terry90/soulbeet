//! The rating sync tool: the caller's rating sync run now, with every track it acted on.

use async_trait::async_trait;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{
    error_object, failed, items, items_text, upstream_error, ItemResult, Items, ToolError,
};
use super::keys::{keyed, request_key, Keyed};
use super::paging::RESULT_MAX_CHARS;
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::ratings::{
    self, AutoDeletePermit, AutoDeleteRan, SyncAction, SyncReport, SyncedTrack,
};

/// Where the tool runs a user's rating sync.
#[async_trait]
pub(crate) trait RatingSync: Send + Sync {
    async fn sync(&self, ctx: &UserCtx, permit: AutoDeletePermit) -> Result<SyncReport, Failure>;
}

/// The server's rating sync, `core::ratings::sync`.
pub(crate) struct LiveSync;

#[async_trait]
impl RatingSync for LiveSync {
    async fn sync(&self, ctx: &UserCtx, permit: AutoDeletePermit) -> Result<SyncReport, Failure> {
        ratings::sync(ctx, permit).await
    }
}

const SYNC_ARGS: &[&str] = &["request_key"];

pub(crate) const SYNC_RATINGS: ToolSpec = ToolSpec {
    name: "sync_ratings",
    title: "Run the rating sync",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Runs this user's rating sync now, as Soulful does every 6 hours: pending discovery tracks rated at or above the promote threshold move into the library, 1-star ones are deleted, and with auto-delete on and deletion switched on for this app, other 1-star songs' files are deleted unless the delete checks keep them (another listener rated or starred them, a download wants them, the file cannot be placed). Without the switch, auto-delete is skipped (auto_delete not_allowed_for_this_app) and the 6 hourly sync does it. Returns one entry per track acted on with what was done or the failure that stopped it; the top level is partial when any entry failed. A very long report keeps the first entries and counts the rest in entries_omitted; Settings lists every kept track, unless kept_not_stored holds the error that stopped storing them, when Settings still lists the previous sync's. Takes seconds to minutes with a large library. Failure codes: {common}, {NAVIDROME_CODES}, already_running (a sync for this user is running, or a call with this request_key still runs), partial, and per entry the delete_*, import_* and unknown_handle codes.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
    idempotent: false,
    open_world: false,
    call: |state, caller, args| Box::pin(sync_ratings(state, caller, args)),
};

/// `sync_ratings`: the caller's rating sync, with auto-delete only when the app may delete.
pub(crate) async fn sync_ratings(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SYNC_RATINGS.name, args, SYNC_ARGS);
    let read = || {
        let key = request_key(&args)?;
        args.finish()?;
        Ok::<_, ToolError>(key)
    };
    let key = match read() {
        Ok(key) => key,
        Err(e) => return failed(e),
    };
    let permit = match caller.allow_delete {
        true => AutoDeletePermit::Allowed,
        false => AutoDeletePermit::NotForThisApp,
    };
    let call = args.as_value();
    let (sync, ctx) = (state.ratings.clone(), caller.ctx.clone());
    let work = async move {
        match sync.sync(&ctx, permit).await {
            Ok(report) => Keyed::Kept(report_result(&report)),
            // Every failure of a sync comes before it acts.
            Err(f) => Keyed::Released(failed(upstream_error(f, Map::new(), args.same_call()))),
        }
    };
    keyed(&state.acquire, caller, SYNC_RATINGS.name, key, &call, work).await
}

/// The report as `items`: one entry per track in the run's order, and the run's facts. While the
/// document is over RESULT_MAX_CHARS, entries are left out whole, the done ones from the end
/// first, then the failed ones from the end, so a partial report stays partial; both counts are
/// given. The summary counts the entries shown.
fn report_result(r: &SyncReport) -> CallToolResult {
    let done = (0..r.tracks.len()).filter(|&i| r.tracks[i].failure.is_none());
    let failed = (0..r.tracks.len()).filter(|&i| r.tracks[i].failure.is_some());
    let order: Vec<usize> = done.rev().chain(failed.rev()).collect();
    let with_left_out = |k: usize| report_items(r, &order[..k]);
    // Leaving out more entries never makes the document longer.
    let (mut fits, mut over) = (order.len(), 0);
    while over < fits {
        let k = over + (fits - over) / 2;
        match items_text(&with_left_out(k)).chars().count() <= RESULT_MAX_CHARS {
            true => fits = k,
            false => over = k + 1,
        }
    }
    items(with_left_out(fits))
}

/// The report's items without the entries at `left_out`.
fn report_items(r: &SyncReport, left_out: &[usize]) -> Items {
    let mut shown = vec![true; r.tracks.len()];
    for &i in left_out {
        shown[i] = false;
    }
    let results = (r.tracks.iter().zip(shown))
        .filter(|(_, shown)| *shown)
        .map(|(t, _)| entry(t))
        .collect();
    let failed_left_out = (left_out.iter())
        .filter(|&&i| r.tracks[i].failure.is_some())
        .count();
    let auto_delete = match r.auto_delete {
        AutoDeleteRan::Off => "off",
        AutoDeleteRan::On => "on",
        AutoDeleteRan::NotAllowed => "not_allowed_for_this_app",
    };
    let Value::Object(fields) = json!({
        "scanned": r.scanned,
        "auto_delete": auto_delete,
        "kept_tracks": r.kept,
        "entries_omitted": left_out.len(),
        "failed_entries_omitted": failed_left_out,
        "kept_not_stored": r.kept_not_stored.as_ref().map(|f| error_object(&ToolError {
            failure: cut(f),
            facts: Map::new(),
            tried: vec![],
            next: vec![],
        })),
    }) else {
        unreachable!("an object literal is an object");
    };
    Items {
        stage: Stage::LibraryCheck,
        noun: "tracks",
        start: 0,
        results,
        pending: 0,
        cursor: None,
        fields,
    }
}

/// One entry: what was done with the track, or the run's failure with the track as its facts.
/// The tags and the failure's texts are cut to their bounds.
fn entry(t: &SyncedTrack) -> ItemResult {
    let action = match t.action {
        SyncAction::Delete => "delete",
        SyncAction::Promote => "promote",
        SyncAction::RemoveFromDiscovery => "remove_from_discovery",
    };
    let Value::Object(fields) = json!({
        "song_id": t.song_id,
        "title": cut_json(t.title.clone(), TEXT_MAX_CHARS),
        "artist": cut_json(t.artist.clone(), TEXT_MAX_CHARS),
        "discovery_track_id": t.discovery_track_id,
        "action": action,
    }) else {
        unreachable!("an object literal is an object");
    };
    let Some(failure) = &t.failure else {
        return ItemResult::Done(fields);
    };
    ItemResult::Failed(ToolError {
        failure: cut(failure),
        facts: fields,
        tried: vec![],
        next: vec![],
    })
}

/// `f` with its message and its cause's raw text cut to their bounds.
fn cut(f: &Failure) -> Failure {
    let mut failure = f.clone();
    failure.message = cut_json(failure.message, TEXT_MAX_CHARS);
    if let Some(cause) = &mut failure.cause {
        cause.raw = cut_json(std::mem::take(&mut cause.raw), RAW_MAX_CHARS);
    }
    failure
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::from_str;

    use super::*;
    use crate::connection::test_util::{
        alice_app, call_tool, document, mcp_app, text_of, FixedSync,
    };
    use crate::core::database_failure;
    use crate::core::delete_guard::blocked_by_other_user;
    use crate::db::test_db;

    fn done(id: &str, action: SyncAction) -> SyncedTrack {
        SyncedTrack {
            song_id: id.into(),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            discovery_track_id: None,
            action,
            failure: None,
        }
    }

    #[test]
    fn a_kept_track_makes_the_report_partial_with_every_entry_in_order() {
        let report = SyncReport {
            scanned: 1342,
            auto_delete: AutoDeleteRan::On,
            kept: 1,
            kept_not_stored: None,
            tracks: vec![
                done("s1", SyncAction::Delete),
                SyncedTrack {
                    failure: Some(blocked_by_other_user()),
                    ..done("s2", SyncAction::Delete)
                },
            ],
        };
        let doc: Value = from_str(text_of(&report_result(&report))).unwrap();
        assert_eq!(
            (doc["ok"].as_bool(), doc["error"]["code"].as_str()),
            (Some(false), Some("partial"))
        );
        assert_eq!(
            doc["summary"],
            json!({"requested": 2, "succeeded": 1, "failed": 1, "pending": 0})
        );
        assert_eq!(
            doc["results"][0],
            json!({"index": 0, "ok": true, "song_id": "s1", "title": "O Holy Night",
                "artist": "Marrow Choir", "discovery_track_id": null, "action": "delete"})
        );
        let failed = &doc["results"][1];
        assert_eq!(
            (
                failed["index"].as_u64(),
                failed["error"]["code"].as_str(),
                failed["error"]["facts"]["song_id"].as_str()
            ),
            (Some(1), Some("delete_blocked_by_other_user"), Some("s2"))
        );
        assert_eq!(
            (
                doc["scanned"].as_u64(),
                doc["auto_delete"].as_str(),
                doc["kept_tracks"].as_u64(),
                doc["entries_omitted"].as_u64(),
                doc["failed_entries_omitted"].as_u64()
            ),
            (Some(1342), Some("on"), Some(1), Some(0), Some(0))
        );
        assert_eq!(doc["kept_not_stored"], Value::Null);
    }

    #[test]
    fn kept_tracks_settings_does_not_list_are_named_with_the_error() {
        let report = SyncReport {
            scanned: 3,
            auto_delete: AutoDeleteRan::On,
            kept: 0,
            kept_not_stored: Some(database_failure(Stage::Delete, "disk I/O error")),
            tracks: vec![done("s1", SyncAction::Delete)],
        };
        let doc: Value = from_str(text_of(&report_result(&report))).unwrap();
        let error = &doc["kept_not_stored"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["stage"].as_str(),
                error["retry"].as_str(),
                error["cause"]["raw"].as_str()
            ),
            (
                Some("settings_unreadable"),
                Some("delete"),
                Some("same_source_later"),
                Some("disk I/O error")
            )
        );
        assert_eq!(doc["results"][0]["ok"], true, "the delete was still done");
    }

    #[test]
    fn a_report_too_long_for_one_answer_counts_what_it_leaves_out() {
        let kept = |i: usize| SyncedTrack {
            failure: Some(blocked_by_other_user()),
            ..done(&format!("s{i}"), SyncAction::Delete)
        };
        let tracks: Vec<SyncedTrack> = (0..200).map(kept).collect();
        let report = SyncReport {
            scanned: 200,
            auto_delete: AutoDeleteRan::On,
            kept: 200,
            kept_not_stored: None,
            tracks,
        };
        let text = text_of(&report_result(&report)).to_string();
        assert!(text.chars().count() <= RESULT_MAX_CHARS);
        let doc: Value = from_str(&text).unwrap();
        let (shown, omitted) = (
            doc["results"].as_array().unwrap().len() as u64,
            doc["entries_omitted"].as_u64().unwrap(),
        );
        assert_eq!(
            (shown + omitted, doc["failed_entries_omitted"].as_u64()),
            (200, Some(omitted))
        );
        assert!(omitted > 0);
        assert_eq!(doc["error"]["code"], "partial");
        // The first entries are kept.
        assert_eq!(doc["results"][0]["error"]["facts"]["song_id"], "s0");
    }

    #[test]
    fn the_done_entries_are_left_out_before_any_failed_one() {
        let long = "x".repeat(400);
        let mut tracks: Vec<SyncedTrack> = (0..200)
            .map(|i| SyncedTrack {
                title: long.clone(),
                artist: long.clone(),
                ..done(&format!("s{i}"), SyncAction::Promote)
            })
            .collect();
        tracks.insert(
            0,
            SyncedTrack {
                failure: Some(blocked_by_other_user()),
                ..done("kept", SyncAction::Delete)
            },
        );
        let report = SyncReport {
            scanned: 201,
            auto_delete: AutoDeleteRan::On,
            kept: 1,
            kept_not_stored: None,
            tracks,
        };
        let doc: Value = from_str(text_of(&report_result(&report))).unwrap();
        assert_eq!(
            (
                doc["error"]["code"].as_str(),
                doc["results"][0]["error"]["facts"]["song_id"].as_str(),
                doc["failed_entries_omitted"].as_u64()
            ),
            (Some("partial"), Some("kept"), Some(0))
        );
        assert!(doc["entries_omitted"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn the_deletion_switch_decides_whether_auto_delete_runs() {
        let fixed = Arc::new(FixedSync::new(Ok(SyncReport {
            scanned: 0,
            auto_delete: AutoDeleteRan::NotAllowed,
            kept: 0,
            kept_not_stored: None,
            tracks: vec![],
        })));
        let (db, app, token) = alice_app(ConnectionState {
            ratings: fixed.clone(),
            ..ConnectionState::for_test(test_db::migrated().await)
        })
        .await;
        let doc = document(&call_tool(app.clone(), &token, "sync_ratings", json!({})).await);
        assert_eq!(doc["auto_delete"], "not_allowed_for_this_app");
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&db)
            .await
            .unwrap();
        call_tool(app, &token, "sync_ratings", json!({})).await;
        assert_eq!(
            fixed.calls(),
            vec![
                ("u1".to_string(), AutoDeletePermit::NotForThisApp),
                ("u1".to_string(), AutoDeletePermit::Allowed)
            ]
        );
    }

    #[tokio::test]
    async fn a_sync_that_cannot_start_is_one_failure() {
        // for_test's FixedSync answers sync_running().
        let (_, app, token) = mcp_app().await;
        let error = &document(&call_tool(app, &token, "sync_ratings", json!({})).await)["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                error["retry"].as_str()
            ),
            (
                Some("already_running"),
                Some("A rating sync is already running for this account"),
                Some("same_source_later")
            )
        );
        assert_eq!(error["next"], json!([{"tool": "sync_ratings", "args": {}}]));
    }
}
