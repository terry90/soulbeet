//! The rating and star tools: the caller's own rating or star of a song or album.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Stage, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{failed, invalid_value, success, upstream_error, NextCall, ToolError};
use super::discovery::PROMOTE_DISCOVERY_TRACK;
use super::keys::{keyed, request_key, Keyed};
use super::library::navidrome_of;
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::ratings::{self, Annotated, NotSet, SyncEffect, Target};

const SET_RATING_ARGS: &[&str] = &["song_id", "album_id", "rating", "request_key"];

const SET_STAR_ARGS: &[&str] = &["song_id", "album_id", "starred", "request_key"];

pub(crate) const SET_RATING: ToolSpec = ToolSpec {
    name: "set_rating",
    title: "Rate a song or album",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Sets this user's own rating (0 to 5, 0 clears it) of a song or album in Navidrome; other users' ratings are theirs and are never read or changed. For a song, rating_sync says what the next rating sync will do with it: move a pending discovery track into the library, replacing an earlier copy of the song there, or delete one rated 1. While this user's auto-delete is on, a 1-star rating of a song is refused and nothing is written, since the next rating sync would delete its file: the user rates it 1 star in Navidrome instead. A rating at or above the promote threshold of a song that is a pending discovery track, in the discovery folder or another folder of this user, is refused too, whether or not deletion is switched on for this app, since a rating sync would move it into the library, which can replace and delete an earlier copy outside this app's daily limit; next offers promote_discovery_track, which replaces one only while deletion is switched on for this app, counted in its daily limit. Album ratings are never refused. Song and album ids stay valid while the files stay in the library. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle (Navidrome lists no such song or album for this user), permission_not_granted (a 1-star song rating while auto-delete is on, or a promoting rating), already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["rating"], "properties": {"song_id": {"type": "string", "minLength": 1, "maxLength": 500}, "album_id": {"type": "string", "minLength": 1, "maxLength": 500}, "rating": {"type": "integer", "minimum": 0, "maximum": 5}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(set_rating(state, caller, args)),
};

pub(crate) const SET_STAR: ToolSpec = ToolSpec {
    name: "set_star",
    title: "Star a song or album",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Stars or unstars a song or album for this user in Navidrome. Song and album ids stay valid while the files stay in the library. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle, already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["starred"], "properties": {"song_id": {"type": "string", "minLength": 1, "maxLength": 500}, "album_id": {"type": "string", "minLength": 1, "maxLength": 500}, "starred": {"type": "boolean"}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(set_star(state, caller, args)),
};

/// `set_rating`: the caller's rating of one song or album, and for a song what the next rating
/// sync does with it.
async fn set_rating(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SET_RATING.name, args, SET_RATING_ARGS);
    let read = async {
        let (song_id, album_id) = (args.opt_string("song_id")?, args.opt_string("album_id")?);
        let rating = args.u32("rating", 0, 5)? as u8;
        let key = request_key(&args)?;
        args.finish()?;
        let target = target_of(&args, song_id, album_id)?;
        let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
        Ok::<_, ToolError>((target, rating, key, navi))
    };
    let (target, rating, key, navi) = match read.await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let (db, user) = (state.db.clone(), caller.ctx.id());
    let work = async move {
        match ratings::rate(&db, &navi, &user, &target, rating).await {
            Ok(a) => {
                let mut fields = annotated_fields(&a);
                fields.insert("rating".into(), json!(rating));
                fields.insert("rating_sync".into(), rating_sync(a.effect));
                Keyed::Kept(success(fields))
            }
            Err(r) => not_set(&args, &target, r),
        }
    };
    keyed(&state.acquire, caller, SET_RATING.name, key, &call, work).await
}

/// `set_star`: the caller's star of one song or album, set or cleared.
async fn set_star(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SET_STAR.name, args, SET_STAR_ARGS);
    let read = async {
        let (song_id, album_id) = (args.opt_string("song_id")?, args.opt_string("album_id")?);
        let starred = args.bool("starred")?;
        let key = request_key(&args)?;
        args.finish()?;
        let target = target_of(&args, song_id, album_id)?;
        let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
        Ok::<_, ToolError>((target, starred, key, navi))
    };
    let (target, starred, key, navi) = match read.await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let work = async move {
        match ratings::star(&navi, &target, starred).await {
            Ok(a) => {
                let mut fields = annotated_fields(&a);
                fields.insert("starred".into(), json!(starred));
                Keyed::Kept(success(fields))
            }
            Err(r) => not_set(&args, &target, r),
        }
    };
    keyed(&state.acquire, caller, SET_STAR.name, key, &call, work).await
}

/// The one song or album a call names: exactly one of `song_id` and `album_id`.
fn target_of(
    args: &Args,
    song_id: Option<String>,
    album_id: Option<String>,
) -> Result<Target, ToolError> {
    match (song_id, album_id) {
        (Some(id), None) => Ok(Target::Song(id)),
        (None, Some(id)) => Ok(Target::Album(id)),
        (None, None) => Err(invalid_value(
            args.tool(),
            "song_id",
            "given when album_id is not",
        )),
        (Some(_), Some(_)) => Err(invalid_value(
            args.tool(),
            "album_id",
            "left out when song_id is given",
        )),
    }
}

/// `{"kind": "song" | "album", "id"}` of `target`.
fn target_fields(target: &Target) -> Map<String, Value> {
    let kind = match target {
        Target::Song(_) => "song",
        Target::Album(_) => "album",
    };
    let mut fields = Map::new();
    fields.insert("kind".into(), json!(kind));
    fields.insert("id".into(), json!(target.id()));
    fields
}

/// The target and what Navidrome listed for it before the change; its tags are cut, as
/// Navidrome's text.
fn annotated_fields(a: &Annotated) -> Map<String, Value> {
    let cut = |text: &Option<String>| text.clone().map(|t| cut_json(t, TEXT_MAX_CHARS));
    let mut fields = target_fields(&a.target);
    fields.insert(
        "title".into(),
        json!(cut_json(a.title.clone(), TEXT_MAX_CHARS)),
    );
    fields.insert("artist".into(), json!(cut(&a.artist)));
    fields.insert("album".into(), json!(cut(&a.album)));
    fields.insert("previous_rating".into(), json!(a.previous_rating));
    fields.insert("previously_starred".into(), json!(a.previously_starred));
    fields
}

/// `{"effect", "message"}` of what the next rating sync does, or null.
fn rating_sync(effect: Option<SyncEffect>) -> Value {
    effect.map_or(
        Value::Null,
        |e| json!({"effect": e.as_str(), "message": e.message()}),
    )
}

/// The answer to a rating or star not made; its key keeps it once the write request was sent,
/// since the change may have been made. A rating refused for what the sync would do says what
/// that is, and a refused promote offers promote_discovery_track.
fn not_set(args: &Args, target: &Target, r: NotSet) -> Keyed {
    let mut facts = target_fields(target);
    let mut next = vec![];
    if let NotSet::WouldDelete {
        effect,
        discovery_track_id,
        ..
    } = &r
    {
        facts.insert("rating_sync".into(), rating_sync(Some(*effect)));
        if let Some(track_id) = discovery_track_id {
            facts.insert("discovery_track_id".into(), json!(track_id));
            let promote = json!({"track_id": track_id});
            next.push(NextCall::new(PROMOTE_DISCOVERY_TRACK.name, promote));
        }
    }
    let mut error = upstream_error(r.failure().clone(), facts, args.same_call());
    error.next.extend(next);
    let result = failed(error);
    if r.after_write() {
        Keyed::Kept(result)
    } else {
        Keyed::Released(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::connection::test_util::{access_token, call_tool, document, mcp_app_with, RESOURCE};
    use crate::connection::{router, ConnectionState};
    use crate::core::delete_guard::test_support::Library;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};
    use crate::test_navidrome::{
        song_body, subsonic_failed, NavidromeFor, NavidromeStub, HOLY, OK,
    };

    #[tokio::test]
    async fn a_one_star_rating_is_refused_while_auto_delete_is_on_whatever_the_switch() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(4), false)],
        );
        navi.answer("/rest/setRating", &[OK]);
        let (db, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let auto_delete = |on: bool| {
            let db = db.clone();
            async move {
                let on = UpdateUserSettings {
                    auto_delete_enabled: Some(on),
                    ..Default::default()
                };
                UserSettings::upsert_in(&db, "u1", on).await.unwrap();
            }
        };
        auto_delete(true).await;
        let one_star = json!({"song_id": HOLY.0, "rating": 1, "request_key": "r1"});
        for allow_delete in [0, 1] {
            sqlx::query("UPDATE oauth_grants SET allow_delete = ?")
                .bind(allow_delete)
                .execute(&db)
                .await
                .unwrap();
            let result = call_tool(app.clone(), &token, "set_rating", one_star.clone()).await;
            let error = &document(&result)["error"];
            assert_eq!(
                (
                    error["code"].as_str(),
                    error["stage"].as_str(),
                    error["retry"].as_str()
                ),
                (
                    Some("permission_not_granted"),
                    Some("input"),
                    Some("needs_user")
                ),
                "allow_delete {allow_delete}"
            );
            assert_eq!(
                error["message"],
                "A 1-star rating from a connected app would have Soulful delete this song's file at the next rating sync, because auto-delete is on. Rate it 1 star yourself in Navidrome, or turn auto-delete off in Soulful."
            );
            assert_eq!(
                error["user_action"],
                json!({"code": "rate_in_navidrome", "what": "Rate the song 1 star yourself in Navidrome", "where": "Navidrome"})
            );
            assert_eq!(
                error["facts"],
                json!({"kind": "song", "id": HOLY.0, "rating_sync": {"effect": "deletes_file",
                    "message": "Auto-delete is on: the next rating sync deletes this song's file unless the delete checks keep it"}})
            );
            assert_eq!(error["next"], json!([]));
        }
        assert!(!navi.asked().contains(&"/rest/setRating".to_string()));
        // Nothing was written, so the key was let go: with auto-delete off the same call rates.
        auto_delete(false).await;
        let doc = document(&call_tool(app.clone(), &token, "set_rating", one_star).await);
        assert_eq!(
            (doc["rating"].as_u64(), &doc["rating_sync"]),
            (Some(1), &json!(null))
        );
        let doc = document(
            &call_tool(
                app,
                &token,
                "set_rating",
                json!({"song_id": HOLY.0, "rating": 2}),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "kind": "song", "id": HOLY.0, "title": "O Holy Night",
                "artist": "Marrow Choir", "album": "Winter Songs", "rating": 2,
                "previous_rating": 4, "previously_starred": false, "rating_sync": null})
        );
    }

    #[tokio::test]
    async fn a_promoting_rating_is_refused_whatever_the_switch() {
        // The 6 hourly sync promotes a pending track through the guard, which replaces an
        // earlier library copy of the song as Soulful's own deletion: no app may set that up.
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some(HOLY.0))
            .await;
        let folder = UpdateUserSettings {
            discovery_folder_id: Some("alice-music".into()),
            ..Default::default()
        };
        UserSettings::upsert_in(&lib.db, "alice", folder)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, false)]);
        navi.answer("/rest/setRating", &[OK]);
        let state = ConnectionState {
            navidrome: Arc::new(NavidromeFor(Ok(navi.client()))),
            ..ConnectionState::for_test(lib.db.clone())
        };
        let (app, token) = (
            router(state),
            access_token(&lib.db, "alice", RESOURCE).await,
        );
        let five = json!({"song_id": HOLY.0, "rating": 5});
        let refused = |doc: serde_json::Value| {
            let error = &doc["error"];
            assert_eq!(
                (
                    error["code"].as_str(),
                    error["retry"].as_str(),
                    error["user_action"]["code"].as_str()
                ),
                (
                    Some("permission_not_granted"),
                    Some("needs_user"),
                    Some("rate_in_navidrome")
                )
            );
            assert_eq!(
                (&error["facts"], &error["next"]),
                (
                    &json!({"kind": "song", "id": HOLY.0, "discovery_track_id": "t1",
                        "rating_sync": {"effect": "promotes_discovery_track",
                            "message": "This is a pending discovery track: the next rating sync moves it into your library"}}),
                    &json!([{"tool": "promote_discovery_track", "args": {"track_id": "t1"}}])
                )
            );
        };
        refused(document(
            &call_tool(app.clone(), &token, "set_rating", five.clone()).await,
        ));
        assert!(!navi.asked().contains(&"/rest/setRating".to_string()));
        // Below the threshold nothing is promoted, so the rating is written.
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "set_rating",
                json!({"song_id": HOLY.0, "rating": 2}),
            )
            .await,
        );
        assert_eq!(doc["rating_sync"], json!(null));
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&lib.db)
            .await
            .unwrap();
        refused(document(&call_tool(app, &token, "set_rating", five).await));
        assert_eq!(navi.posted("/rest/setRating").len(), 1);
    }

    #[tokio::test]
    async fn no_app_can_move_the_discovery_folder_around_a_promoting_rating() {
        // The route: move the discovery folder away from a pending track's folder, rate the
        // track at the threshold (the sync reads the discovery folder only), move it back. The
        // 6 hourly sync would then promote the track through the guard as Soulful's own
        // deletion, replacing an earlier library copy outside the app's switch and daily room.
        // Each step is refused for every app, whatever its switch.
        let lib = Library::new().await;
        lib.folder("alice", "alice-other", "music/alice-other", false)
            .await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some(HOLY.0))
            .await;
        let folder = |id: &str| UpdateUserSettings {
            discovery_folder_id: Some(id.into()),
            ..Default::default()
        };
        UserSettings::upsert_in(&lib.db, "alice", folder("alice-music"))
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, false)]);
        navi.answer("/rest/setRating", &[OK]);
        let state = ConnectionState {
            navidrome: Arc::new(NavidromeFor(Ok(navi.client()))),
            ..ConnectionState::for_test(lib.db.clone())
        };
        let (app, token) = (
            router(state),
            access_token(&lib.db, "alice", RESOURCE).await,
        );
        let discovery_folder = || async {
            (UserSettings::get_in(&lib.db, "alice").await)
                .unwrap()
                .discovery_folder_id
        };
        let move_to =
            |id: &str| json!({"discovery_folder_id": id, "request_key": format!("move-{id}")});
        let refused = |doc: &serde_json::Value, field: &str| {
            let error = &doc["error"];
            assert_eq!(
                (
                    error["code"].as_str(),
                    error["retry"].as_str(),
                    error["user_action"]["code"].as_str(),
                    &error["facts"],
                    &error["next"]
                ),
                (
                    Some("permission_not_granted"),
                    Some("needs_user"),
                    Some("change_discovery_settings"),
                    &json!({"field": field}),
                    &json!([])
                )
            );
        };
        let away = document(
            &call_tool(
                app.clone(),
                &token,
                "update_settings",
                move_to("alice-other"),
            )
            .await,
        );
        refused(&away, "discovery_folder_id");
        assert_eq!(away["error"]["message"], "Only you can change the discovery folder, in Soulful's Settings: the rating sync moves the folder's pending discovery tracks rated at or above the promote threshold into your library, replacing and deleting any earlier copy there");
        assert_eq!(discovery_folder().await.as_deref(), Some("alice-music"));

        // The user moves it in Soulful: the rating is still refused, t1 being alice's.
        UserSettings::upsert_in(&lib.db, "alice", folder("alice-other"))
            .await
            .unwrap();
        let five = json!({"song_id": HOLY.0, "rating": 5});
        let rated = document(&call_tool(app.clone(), &token, "set_rating", five.clone()).await);
        assert_eq!(
            (
                rated["error"]["code"].as_str(),
                &rated["error"]["facts"]["discovery_track_id"],
                &rated["error"]["next"]
            ),
            (
                Some("permission_not_granted"),
                &json!("t1"),
                &json!([{"tool": "promote_discovery_track", "args": {"track_id": "t1"}}])
            )
        );
        assert!(!navi.asked().contains(&"/rest/setRating".to_string()));

        let back = document(
            &call_tool(
                app.clone(),
                &token,
                "update_settings",
                move_to("alice-music"),
            )
            .await,
        );
        refused(&back, "discovery_folder_id");
        assert_eq!(discovery_folder().await.as_deref(), Some("alice-other"));

        // With the switch the folder stays, and so does the refusal of the rating.
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&lib.db)
            .await
            .unwrap();
        let switched = document(
            &call_tool(
                app.clone(),
                &token,
                "update_settings",
                move_to("alice-music"),
            )
            .await,
        );
        refused(&switched, "discovery_folder_id");
        assert_eq!(discovery_folder().await.as_deref(), Some("alice-other"));
        let rated = document(&call_tool(app, &token, "set_rating", five).await);
        assert_eq!(rated["error"]["code"], "permission_not_granted");
        assert!(!navi.asked().contains(&"/rest/setRating".to_string()));
    }

    #[tokio::test]
    async fn a_star_takes_one_id_and_an_unknown_album_says_so() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getAlbum", &[&subsonic_failed(70, "Album not found")]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let error = &document(
            &call_tool(
                app.clone(),
                &token,
                "set_star",
                json!({"song_id": HOLY.0, "album_id": "7hQ2nLr5VxZ0cYbM8wTkAf", "starred": true}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (
                Some("invalid_input"),
                Some("set_star: album_id must be left out when song_id is given")
            )
        );
        let error =
            &document(&call_tool(app.clone(), &token, "set_star", json!({"starred": true})).await)
                ["error"];
        assert_eq!(
            error["message"],
            "set_star: song_id must be given when album_id is not"
        );
        let error = &document(
            &call_tool(
                app,
                &token,
                "set_star",
                json!({"album_id": "gone", "starred": true}),
            )
            .await,
        )["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["facts"],
                &error["next"]
            ),
            (
                Some("unknown_handle"),
                Some("Navidrome has no album gone that this account can see"),
                &json!({"kind": "album", "id": "gone"}),
                &json!([])
            )
        );
        assert!(!navi.asked().contains(&"/rest/star".to_string()));
    }
}
