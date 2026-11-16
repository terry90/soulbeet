//! `get_settings` and `list_folders`: the caller's own settings and folders.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::recommendation::DiscoveryProfile;

use super::args::Args;
use super::contract::NextCall;
use super::contract::{answer, failed, invalid_value, success, upstream_error, ToolError};
use super::keys::{keyed, request_key, Keyed};
use super::paging::{fit, Page};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec};
use super::status::{chosen_folder, folder_fields, with_free_space};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::discovery::parse_profiles;
use crate::core::library::library_roots_in;
use crate::core::settings::Setting;
use crate::core::{folders, settings};
use crate::models::folder::Folder;
use crate::models::user_settings::{UpdateUserSettings, UserSettings};

/// The settings document (D14): what is saved, never a key or token.
pub(crate) fn settings_fields(s: &UserSettings, folders: &[Folder]) -> Map<String, Value> {
    let default_folder_id = chosen_folder(s.default_download_folder_id.clone());
    let default_folder_found = default_folder_id
        .as_deref()
        .map(|id| folders.iter().any(|f| f.id == id));
    let profiles: Vec<String> = parse_profiles(&s.discovery_profiles)
        .iter()
        .map(ToString::to_string)
        .collect();
    let playlist_names: Map<String, Value> = DiscoveryProfile::NAMES
        .iter()
        .map(|p| {
            let name = UserSettings::get_playlist_name_for_profile(&s.discovery_playlist_name, p);
            (p.to_string(), json!(name))
        })
        .collect();
    let playlist_ids: Map<String, Value> = DiscoveryProfile::NAMES
        .iter()
        .filter_map(|p| {
            UserSettings::get_playlist_id_for_profile(&s.discovery_navidrome_playlist_id, p)
                .map(|id| (p.to_string(), json!(id)))
        })
        .collect();
    let saved = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
    let Value::Object(fields) = json!({
        "default_folder_id": default_folder_id,
        "default_folder_found": default_folder_found,
        "auto_delete_enabled": s.auto_delete_enabled,
        "default_metadata_provider": s.default_metadata_provider,
        "lastfm": {"username": s.lastfm_username, "api_key_saved": saved(&s.lastfm_api_key)},
        "listenbrainz": {"username": s.listenbrainz_username, "token_saved": saved(&s.listenbrainz_token)},
        "discovery": {
            "enabled": s.discovery_enabled,
            "folder_id": chosen_folder(s.discovery_folder_id.clone()),
            "profiles": profiles,
            "track_count": s.parse_track_counts(),
            "lifetime_days": s.parse_lifetime_days(),
            "playlist_names": playlist_names,
            "playlist_ids": playlist_ids,
            "promote_threshold": s.discovery_promote_threshold,
            "last_generated_at": s.discovery_last_generated_at,
        },
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

/// The caller's settings and folders, read at once. A failed read can be
/// tried again with the same call.
async fn read_rows(
    state: &ConnectionState,
    caller: &McpCaller,
    args: &Args,
) -> Result<(UserSettings, Vec<Folder>), ToolError> {
    let (settings, folders) = tokio::join!(
        settings::get_in(&state.db, &caller.ctx),
        folders::list_in(&state.db, &caller.ctx),
    );
    settings
        .and_then(|settings| Ok((settings, folders?)))
        .map_err(|failure| upstream_error(failure, Map::new(), args.same_call()))
}

/// `get_settings`, which takes no arguments.
pub(crate) async fn get_settings(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_settings", args, &[]);
    answer(
        async {
            args.finish()?;
            let (settings, folders) = read_rows(state, caller, &args).await?;
            Ok(settings_fields(&settings, &folders))
        }
        .await,
    )
}

/// `list_folders`, a page of the caller's folders. Free space is read for
/// the folders of the page only.
pub(crate) async fn list_folders(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_folders", args, &["limit", "cursor"]);
    answer(
        async {
            let page = Page::read(&args, json!({}))?;
            args.finish()?;
            let ((settings, folders), roots) =
                tokio::try_join!(read_rows(state, caller, &args), async {
                    library_roots_in(&state.db, state.navidrome_music_path.as_deref())
                        .await
                        .map_err(|f| upstream_error(f, Map::new(), args.same_call()))
                },)?;
            let default = chosen_folder(settings.default_download_folder_id);
            let more = folders.len() > page.offset.saturating_add(page.limit);
            let shown: Vec<Folder> = folders
                .into_iter()
                .skip(page.offset)
                .take(page.limit)
                .collect();
            let items: Vec<Value> = with_free_space(shown)
                .await
                .iter()
                .map(|f| folder_fields(f, default.as_deref(), &roots))
                .collect();
            let mut fields = Map::new();
            fields.insert("default_folder_id".into(), json!(default));
            Ok(fit(fields, "folders", items, &page, more))
        }
        .await,
    )
}

const UPDATE_SETTINGS_ARGS: &[&str] = &[
    "default_folder_id",
    "auto_delete",
    "discovery_enabled",
    "discovery_folder_id",
    "discovery_profiles",
    "discovery_track_counts",
    "discovery_lifetime_days",
    "discovery_playlist_names",
    "discovery_promote_threshold",
    "request_key",
];

pub(crate) const UPDATE_SETTINGS: ToolSpec = ToolSpec {
    name: "update_settings",
    title: "Change settings",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Changes this user's Soulful settings: the default download folder (a folder_id from list_folders), auto-delete of 1-star songs at the rating sync (it can only be turned off here: the user turns it on in Soulful's Settings, since it deletes the files of songs rated 1 star), and discovery (on or off, the profiles, per profile track count 1 to 50, lifetime 1 to 90 days and playlist name, all three profiles named in each map). The discovery folder and the promote threshold are changed only by the user, in Soulful's Settings, whether or not deletion is switched on for this app: the rating sync moves the folder's pending tracks rated at or above the threshold into the library, and a promote can replace and delete an earlier library copy outside this app's daily limit. Turning discovery off or dropping a profile deletes that profile's discovery playlist in Navidrome; it deletes no music. Only the settings given change. Returns the settings afterwards as get_settings shows them; credentials are never shown. Failure codes: {common}, unknown_handle (a folder that is not this user's), permission_not_granted (auto_delete: true, discovery_folder_id or discovery_promote_threshold), already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"default_folder_id": {"type": "string", "minLength": 1, "maxLength": 500}, "auto_delete": {"type": "boolean"}, "discovery_enabled": {"type": "boolean"}, "discovery_folder_id": {"type": "string", "minLength": 1, "maxLength": 500}, "discovery_profiles": {"type": "array", "minItems": 1, "uniqueItems": true, "items": {"enum": ["Conservative", "Balanced", "Adventurous"]}}, "discovery_track_counts": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "integer"}, "Balanced": {"type": "integer"}, "Adventurous": {"type": "integer"}}}, "discovery_lifetime_days": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "integer"}, "Balanced": {"type": "integer"}, "Adventurous": {"type": "integer"}}}, "discovery_playlist_names": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "string"}, "Balanced": {"type": "string"}, "Adventurous": {"type": "string"}}}, "discovery_promote_threshold": {"type": "integer", "minimum": 0, "maximum": 255}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(update_settings(state, caller, args)),
};

/// Turning auto-delete on makes every rating sync delete the files of the songs the user rated
/// 1 star, so only the user does it, in Soulful.
fn auto_delete_on_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Only you can turn auto-delete on, in Soulful's Settings: it deletes the files of songs you rate 1 star at every rating sync",
    )
    .with_user_action(UserAction::new(
        UserActionCode::EnableAutoDelete,
        "Turn on auto-delete yourself",
        "Soulful Settings > Library",
    ))
}

/// Lowering the promote threshold has the next rating sync promote pending discovery tracks
/// rated below the old one, and a promote can replace an earlier library copy, deleting it. The
/// 6 hourly sync promotes as Soulful's own deletion, past the app's switch, its daily room and
/// its name in the history, so no app changes it, whatever its switch, as no app's rating may
/// promote (`core::ratings::sync_effect`).
fn promote_threshold_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Only you can change the discovery promote threshold, in Soulful's Settings: the rating sync moves the pending discovery tracks rated at or above it into your library, replacing and deleting any earlier copy there",
    )
    .with_user_action(UserAction::new(
        UserActionCode::ChangeDiscoverySettings,
        "Change the discovery promote threshold yourself",
        "Soulful Settings > Library > Discovery",
    ))
}

/// Moving the discovery folder has the next rating sync promote the pending tracks of the new
/// one that are rated at or above the threshold, and a promote can replace an earlier library
/// copy, deleting it. Refused from every app, as the promote threshold is.
fn discovery_folder_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Only you can change the discovery folder, in Soulful's Settings: the rating sync moves the folder's pending discovery tracks rated at or above the promote threshold into your library, replacing and deleting any earlier copy there",
    )
    .with_user_action(UserAction::new(
        UserActionCode::ChangeDiscoverySettings,
        "Change the discovery folder yourself",
        "Soulful Settings > Library > Discovery",
    ))
}

/// The settings a call changes.
struct Changes {
    update: UpdateUserSettings,
    auto_delete_on: bool,
}

fn read_changes(args: &Args) -> Result<Changes, ToolError> {
    let profiles = &DiscoveryProfile::NAMES;
    let map = |name| {
        args.opt_object(name, profiles)
            .map(|object| object.map(|o| o.as_value().to_string()))
    };
    let auto_delete = args.opt_bool("auto_delete")?;
    let update = UpdateUserSettings {
        default_download_folder_id: args.opt_string("default_folder_id")?,
        auto_delete_enabled: auto_delete,
        discovery_enabled: args.opt_bool("discovery_enabled")?,
        discovery_folder_id: args.opt_string("discovery_folder_id")?,
        discovery_profiles: args
            .opt_choices("discovery_profiles", profiles)?
            .map(|profiles| profiles.join(",")),
        discovery_track_count: map("discovery_track_counts")?,
        discovery_lifetime_days: map("discovery_lifetime_days")?,
        discovery_playlist_name: map("discovery_playlist_names")?,
        discovery_promote_threshold: args
            .opt_u32("discovery_promote_threshold", 0, 255)?
            .map(|n| n as u8),
        ..Default::default()
    };
    Ok(Changes {
        update,
        auto_delete_on: auto_delete == Some(true),
    })
}

/// The argument that sets `setting`.
fn argument_of(setting: Setting) -> &'static str {
    match setting {
        Setting::DefaultFolder => "default_folder_id",
        Setting::DiscoveryFolder => "discovery_folder_id",
        Setting::DiscoveryProfiles => "discovery_profiles",
        Setting::DiscoveryTrackCounts => "discovery_track_counts",
        Setting::DiscoveryLifetimes => "discovery_lifetime_days",
        Setting::DiscoveryPlaylistNames => "discovery_playlist_names",
        Setting::DiscoveryPromoteThreshold => "discovery_promote_threshold",
    }
}

/// `update_settings`: the settings a call names, checked as the web page's are, and the
/// settings as saved.
async fn update_settings(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(UPDATE_SETTINGS.name, args, UPDATE_SETTINGS_ARGS);
    let read = || {
        let changes = read_changes(&args)?;
        let key = request_key(&args)?;
        args.finish()?;
        if changes.update == UpdateUserSettings::default() {
            return Err(invalid_value(
                args.tool(),
                "default_folder_id",
                "given when no other setting is",
            ));
        }
        Ok::<_, ToolError>((changes, key))
    };
    let (changes, key) = match read() {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = args.as_value();
    let (db, navidrome, ctx) = (
        state.db.clone(),
        state.navidrome.clone(),
        caller.ctx.clone(),
    );
    let work = async move {
        if changes.auto_delete_on {
            return refusal(&args, auto_delete_on_refused(), field_facts("auto_delete"));
        }
        if changes.update.discovery_folder_id.is_some() {
            let facts = field_facts("discovery_folder_id");
            return refusal(&args, discovery_folder_refused(), facts);
        }
        if changes.update.discovery_promote_threshold.is_some() {
            let facts = field_facts("discovery_promote_threshold");
            return refusal(&args, promote_threshold_refused(), facts);
        }
        let saved = match settings::update_in(&db, &*navidrome, &ctx, changes.update).await {
            Ok(saved) => saved,
            Err(not_saved) => {
                let facts = match not_saved.setting {
                    Some(setting) => field_facts(argument_of(setting)),
                    None => Map::new(),
                };
                let mut error = upstream_error(not_saved.failure, facts, args.same_call());
                if error.failure.code == FailureCode::UnknownHandle {
                    error.next = vec![NextCall::new("list_folders", json!({}))];
                }
                return Keyed::Released(failed(error));
            }
        };
        // The settings are saved: a folder list that fails now still keeps the key.
        match folders::list_in(&db, &ctx).await {
            Ok(folders) => {
                let mut fields = Map::new();
                fields.insert(
                    "settings".into(),
                    Value::Object(settings_fields(&saved, &folders)),
                );
                Keyed::Kept(success(fields))
            }
            Err(failure) => Keyed::Kept(failed(upstream_error(failure, Map::new(), None))),
        }
    };
    keyed(
        &state.acquire,
        caller,
        UPDATE_SETTINGS.name,
        key,
        &call,
        work,
    )
    .await
}

fn field_facts(field: &str) -> Map<String, Value> {
    Map::from_iter([("field".to_string(), json!(field))])
}

/// A refusal before anything is saved, which lets the request key go.
fn refusal(args: &Args, failure: Failure, facts: Map<String, Value>) -> Keyed {
    Keyed::Released(failed(upstream_error(failure, facts, args.same_call())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::{alice_caller, text_of};
    use crate::db::test_db;

    #[tokio::test]
    async fn settings_name_what_is_saved_but_never_the_key() {
        let db = test_db::migrated().await;
        let mut s = UserSettings::get_in(&db, "u1").await.unwrap();
        s.lastfm_api_key = Some("3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e".into());
        s.lastfm_username = Some("alice_fm".into());
        s.listenbrainz_token = Some(String::new());
        s.default_download_folder_id = Some("f1".into());
        let folders = vec![Folder {
            id: "f1".into(),
            user_id: "u1".into(),
            name: "Music".into(),
            path: "/music/alice".into(),
            shared: false,
        }];
        let doc = Value::Object(settings_fields(&s, &folders));
        assert_eq!(
            doc,
            json!({
                "default_folder_id": "f1", "default_folder_found": true, "auto_delete_enabled": false,
                "default_metadata_provider": "musicbrainz",
                "lastfm": {"username": "alice_fm", "api_key_saved": true},
                "listenbrainz": {"username": null, "token_saved": false},
                "discovery": {"enabled": false, "folder_id": null, "profiles": ["Conservative", "Balanced", "Adventurous"],
                    "track_count": {"Conservative": 10, "Balanced": 10, "Adventurous": 10},
                    "lifetime_days": {"Conservative": 7, "Balanced": 7, "Adventurous": 7},
                    "playlist_names": {"Conservative": "Comfort Zone", "Balanced": "Fresh Picks", "Adventurous": "Deep Cuts"},
                    "playlist_ids": {}, "promote_threshold": 3, "last_generated_at": null}
            })
        );
        assert!(!doc.to_string().contains("3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e"));
        s.default_download_folder_id = Some("gone".into());
        assert_eq!(settings_fields(&s, &folders)["default_folder_found"], false);
        s.default_download_folder_id = Some(String::new());
        let doc = settings_fields(&s, &folders);
        assert_eq!(
            (&doc["default_folder_id"], &doc["default_folder_found"]),
            (&Value::Null, &Value::Null)
        );
    }

    #[tokio::test]
    async fn a_cleared_discovery_folder_is_no_folder() {
        let db = test_db::migrated().await;
        let mut s = UserSettings::get_in(&db, "u1").await.unwrap();
        // How Settings clears a folder choice (core/settings.rs, check_folder_choices).
        s.discovery_folder_id = Some(String::new());
        assert_eq!(
            settings_fields(&s, &[])["discovery"]["folder_id"],
            Value::Null
        );
        s.discovery_folder_id = Some("f1".into());
        assert_eq!(settings_fields(&s, &[])["discovery"]["folder_id"], "f1");
    }

    /// The tool's document when the caller's rows cannot be read: the
    /// folders table is gone, so sqlx fails as SQLite does on any missing
    /// table.
    async fn unreadable_rows(tool: &str) -> Value {
        let db = test_db::migrated().await;
        sqlx::query("DROP TABLE folders")
            .execute(&db)
            .await
            .unwrap();
        let state = ConnectionState::for_test(db);
        let caller = alice_caller(false);
        let args = json!({"limit": 5}).as_object().cloned();
        let result = match tool {
            "get_settings" => get_settings(&state, &caller, None).await,
            _ => list_folders(&state, &caller, args).await,
        };
        assert_eq!(result.is_error, Some(true), "{tool}");
        serde_json::from_str(text_of(&result)).unwrap()
    }

    #[tokio::test]
    async fn settings_and_folders_that_cannot_be_read_offer_the_same_call_again() {
        for (tool, call) in [
            ("get_settings", json!({"tool": "get_settings", "args": {}})),
            (
                "list_folders",
                json!({"tool": "list_folders", "args": {"limit": 5}}),
            ),
        ] {
            let e = &unreadable_rows(tool).await["error"];
            assert_eq!(
                (
                    e["code"].as_str(),
                    e["stage"].as_str(),
                    e["retry"].as_str(),
                    e["message"].as_str()
                ),
                (
                    Some("settings_unreadable"),
                    Some("input"),
                    Some("same_source_later"),
                    Some("Soulful's database is unavailable")
                ),
                "{tool}"
            );
            // sqlx's text for SQLite's "no such table" (sqlx-core 0.8.6 src/error.rs:44)
            assert_eq!(
                e["cause"],
                json!({"service": "database", "status": null,
                    "raw": "error returned from database: (code: 1) no such table: folders"}),
                "{tool}"
            );
            assert_eq!(e["next"], json!([call]), "{tool}");
        }
    }

    #[tokio::test]
    async fn discovery_settings_read_as_discovery_reads_them() {
        let db = test_db::migrated().await;
        let mut s = UserSettings::get_in(&db, "u1").await.unwrap();
        // The forms the Settings page writes (ui/src/components/settings/folder_manager.rs:415,445) and the
        // playlist sync stores (UserSettings::update_discovery_playlist_id, user_settings.rs).
        s.discovery_track_count = r#"{"Conservative":5,"Balanced":10,"Adventurous":15}"#.into();
        s.discovery_playlist_name =
            r#"{"Conservative":"Comfort Zone","Balanced":"Weekly Mix","Adventurous":"Deep Cuts"}"#
                .into();
        s.discovery_navidrome_playlist_id = Some(r#"{"Balanced":"9fR2kLm4NpQ7sT1vW3xY5z"}"#.into());
        let doc = settings_fields(&s, &[]);
        let d = &doc["discovery"];
        assert_eq!(
            (&d["track_count"], &d["playlist_names"], &d["playlist_ids"]),
            (
                &json!({"Conservative": 5, "Balanced": 10, "Adventurous": 15}),
                &json!({"Conservative": "Comfort Zone", "Balanced": "Weekly Mix", "Adventurous": "Deep Cuts"}),
                &json!({"Balanced": "9fR2kLm4NpQ7sT1vW3xY5z"})
            )
        );
    }

    use crate::connection::test_util::{call_tool, document, mcp_app};

    #[tokio::test]
    async fn auto_delete_is_turned_on_only_in_soulfuls_settings() {
        let (db, app, token) = mcp_app().await;
        for allow_delete in [0, 1] {
            sqlx::query("UPDATE oauth_grants SET allow_delete = ?")
                .bind(allow_delete)
                .execute(&db)
                .await
                .unwrap();
            let result = call_tool(
                app.clone(),
                &token,
                "update_settings",
                json!({"auto_delete": true, "request_key": "k"}),
            )
            .await;
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
                "Only you can turn auto-delete on, in Soulful's Settings: it deletes the files of songs you rate 1 star at every rating sync"
            );
            assert_eq!(
                error["user_action"],
                json!({"code": "enable_auto_delete", "what": "Turn on auto-delete yourself", "where": "Soulful Settings > Library"})
            );
            assert_eq!(
                (&error["facts"], &error["next"]),
                (&json!({"field": "auto_delete"}), &json!([]))
            );
            assert!(
                !UserSettings::get_in(&db, "u1")
                    .await
                    .unwrap()
                    .auto_delete_enabled
            );
        }
        // Turned on in Soulful, it is turned off from the app, switch or not.
        let on = UpdateUserSettings {
            auto_delete_enabled: Some(true),
            ..Default::default()
        };
        UserSettings::upsert_in(&db, "u1", on).await.unwrap();
        let off = document(
            &call_tool(
                app,
                &token,
                "update_settings",
                json!({"auto_delete": false}),
            )
            .await,
        );
        assert_eq!(off["settings"]["auto_delete_enabled"], false);
        assert!(
            !UserSettings::get_in(&db, "u1")
                .await
                .unwrap()
                .auto_delete_enabled
        );
    }

    #[tokio::test]
    async fn the_promote_threshold_and_the_discovery_folder_are_changed_only_in_soulfuls_settings()
    {
        // A lower threshold, or a folder whose pending tracks are rated at or above it, has the
        // 6 hourly sync promote them as Soulful's own deletion, and a promote can replace an
        // earlier library copy: outside the app's switch, its daily room and its name in the
        // history. So no app changes either, whatever its switch.
        let (db, app, token) = mcp_app().await;
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f1', 'u1', 'Music', '/music/u1')")
            .execute(&db)
            .await
            .unwrap();
        let calls = [
            (
                "discovery_promote_threshold",
                json!({"discovery_promote_threshold": 1, "request_key": "k"}),
                "Only you can change the discovery promote threshold, in Soulful's Settings: the rating sync moves the pending discovery tracks rated at or above it into your library, replacing and deleting any earlier copy there",
                "Change the discovery promote threshold yourself",
            ),
            (
                "discovery_folder_id",
                json!({"discovery_folder_id": "f1", "request_key": "k"}),
                "Only you can change the discovery folder, in Soulful's Settings: the rating sync moves the folder's pending discovery tracks rated at or above the promote threshold into your library, replacing and deleting any earlier copy there",
                "Change the discovery folder yourself",
            ),
        ];
        for allow_delete in [0, 1] {
            sqlx::query("UPDATE oauth_grants SET allow_delete = ?")
                .bind(allow_delete)
                .execute(&db)
                .await
                .unwrap();
            // The same key each time: nothing was saved, so each refusal let it go.
            for (field, call, message, what) in &calls {
                let result = call_tool(app.clone(), &token, "update_settings", call.clone()).await;
                let error = &document(&result)["error"];
                assert_eq!(
                    (
                        error["code"].as_str(),
                        error["stage"].as_str(),
                        error["retry"].as_str(),
                        error["message"].as_str()
                    ),
                    (
                        Some("permission_not_granted"),
                        Some("input"),
                        Some("needs_user"),
                        Some(*message)
                    ),
                    "{field}, allow_delete {allow_delete}"
                );
                assert_eq!(
                    error["user_action"],
                    json!({"code": "change_discovery_settings", "what": what, "where": "Soulful Settings > Library > Discovery"}),
                    "{field}, allow_delete {allow_delete}"
                );
                assert_eq!(
                    (&error["facts"], &error["next"]),
                    (&json!({"field": field}), &json!([])),
                    "{field}, allow_delete {allow_delete}"
                );
            }
            let saved = UserSettings::get_in(&db, "u1").await.unwrap();
            assert_eq!(
                (saved.discovery_promote_threshold, saved.discovery_folder_id),
                (3, None),
                "allow_delete {allow_delete}"
            );
        }
    }

    #[tokio::test]
    async fn another_users_folder_is_not_a_choice_and_credentials_never_show() {
        let (db, app, token) = mcp_app().await;
        let bobs = crate::core::folder_of(&db, "bob").await;
        UserSettings::upsert_in(
            &db,
            "u1",
            UpdateUserSettings {
                lastfm_api_key: Some("3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e".into()),
                listenbrainz_token: Some("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let error = &document(
            &call_tool(
                app.clone(),
                &token,
                "update_settings",
                json!({"default_folder_id": bobs}),
            )
            .await,
        )["error"];
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (Some("unknown_handle"), Some("Folder not found"))
        );
        assert_eq!(
            (&error["facts"], &error["next"]),
            (
                &json!({"field": "default_folder_id"}),
                &json!([{"tool": "list_folders", "args": {}}])
            )
        );
        let text = call_tool(
            app,
            &token,
            "update_settings",
            json!({"discovery_enabled": true}),
        )
        .await
        .to_string();
        assert!(text.contains(r#"\"enabled\":true"#), "{text}");
        assert!(
            !text.contains("3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e")
                && !text.contains("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_value_out_of_bounds_names_its_argument() {
        let (_, app, token) = mcp_app().await;
        let error = &document(
            &call_tool(app.clone(), &token, "update_settings",
                json!({"discovery_track_counts": {"Conservative": 10, "Balanced": 0, "Adventurous": 10}})).await,
        )["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                &error["facts"]
            ),
            (
                Some("invalid_input"),
                Some("Discovery track count for Balanced must be 1 to 50, not 0"),
                &json!({"field": "discovery_track_counts"})
            )
        );
        let error = &document(
            &call_tool(app.clone(), &token, "update_settings",
                json!({"discovery_lifetime_days": {"Conservative": 7, "Balanced": 7, "Explore": 7}})).await,
        )["error"];
        assert_eq!(
            error["message"],
            "update_settings takes no argument named 'discovery_lifetime_days.Explore': it takes only Conservative, Balanced, Adventurous"
        );
        let error =
            &document(&call_tool(app.clone(), &token, "update_settings", json!({})).await)["error"];
        assert_eq!(
            error["message"],
            "update_settings: default_folder_id must be given when no other setting is"
        );
        let error = &document(
            &call_tool(
                app.clone(),
                &token,
                "update_settings",
                json!({"discovery_profiles": ["Balanced", "Balanced"]}),
            )
            .await,
        )["error"];
        assert_eq!(
            error["message"],
            "update_settings: discovery_profiles must be a list of one or more of Conservative, Balanced, Adventurous, each once"
        );
        let error = &document(
            &call_tool(
                app,
                &token,
                "update_settings",
                json!({"discovery_profiles": []}),
            )
            .await,
        )["error"];
        assert_eq!(
            error["message"],
            "update_settings: discovery_profiles must be a list of one or more of Conservative, Balanced, Adventurous, each once"
        );
    }

    #[tokio::test]
    async fn profiles_and_maps_are_saved_as_the_page_saves_them() {
        let (db, app, token) = mcp_app().await;
        let doc = document(
            &call_tool(app, &token, "update_settings", json!({
                "discovery_profiles": ["Adventurous", "Balanced"],
                "discovery_playlist_names": {"Conservative": "Calm", "Balanced": "Mid", "Adventurous": "Wild"},
            })).await,
        );
        assert_eq!(
            doc["settings"]["discovery"]["profiles"],
            json!(["Adventurous", "Balanced"])
        );
        assert_eq!(
            doc["settings"]["discovery"]["playlist_names"]["Adventurous"],
            "Wild"
        );
        let saved = UserSettings::get_in(&db, "u1").await.unwrap();
        assert_eq!(saved.discovery_profiles, "Adventurous,Balanced");
    }

    #[tokio::test]
    async fn a_playlist_name_is_saved_trimmed() {
        let (db, app, token) = mcp_app().await;
        let doc = document(
            &call_tool(app, &token, "update_settings", json!({
                "discovery_playlist_names": {"Conservative": " Calm ", "Balanced": "Mid", "Adventurous": "Wild\n"},
            })).await,
        );
        assert_eq!(
            doc["settings"]["discovery"]["playlist_names"],
            json!({"Conservative": "Calm", "Balanced": "Mid", "Adventurous": "Wild"})
        );
        let saved = UserSettings::get_in(&db, "u1").await.unwrap();
        assert_eq!(
            UserSettings::get_playlist_name_for_profile(
                &saved.discovery_playlist_name,
                "Conservative"
            ),
            "Calm"
        );
    }
}
