//! `get_settings` and `list_folders`: the caller's own settings and folders.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::recommendation::DiscoveryProfile;

use super::args::Args;
use super::contract::{answer, failed, invalid_value, success, upstream_error, ToolError};
use super::keys::{keyed, request_key, Keyed};
use super::paging::{fit, Page};
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec};
use super::status::{owned_folder_fields, with_free_space};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::discovery::parse_profiles;
use crate::core::library::library_roots_in;
use crate::core::settings::Setting;
use crate::core::time;
use crate::core::{folders, settings};
use crate::models::folder::Folder;
use crate::models::user_settings::{UpdateUserSettings, UserSettings, PROMOTE_THRESHOLDS};

/// The settings document (D14): what is saved, never a key or token.
pub(crate) fn settings_fields(s: &UserSettings) -> Map<String, Value> {
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
        "auto_delete_enabled": s.auto_delete_enabled,
        "accept_shares": s.accept_shares,
        "listenbrainz": {"username": s.listenbrainz_username, "token_saved": saved(&s.listenbrainz_token)},
        "discovery": {
            "enabled": s.discovery_enabled,
            "profiles": profiles,
            "track_count": s.parse_track_counts(),
            "lifetime_days": s.parse_lifetime_days(),
            "playlist_names": playlist_names,
            "playlist_ids": playlist_ids,
            "promote_threshold": s.discovery_promote_threshold,
            "last_generated_at": s.discovery_last_generated_at.as_deref().map(time::text),
            "deezer_recommendations": s.deezer_recommendations,
        },
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
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
            let s = settings::get_in(&state.db, &caller.ctx)
                .await
                .map_err(|failure| upstream_error(failure, Map::new(), args.same_call()))?;
            Ok(settings_fields(&s))
        }
        .await,
    )
}

/// `list_folders`, a page of the caller's personal library folder (or all users' personal
/// folders when admin library access is enabled and `scope` is `"all"` or omitted). Free space is
/// read for the folders of the page only.
pub(crate) async fn list_folders(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_folders", args, &["scope", "limit", "cursor"]);
    answer(
        async {
            let scope_arg = args.opt_choice("scope", &["own", "all"])?;
            let list_key = match scope_arg {
                Some(s) => json!({"scope": s}),
                None => json!({}),
            };
            let page = Page::read(&args, list_key)?;
            args.finish()?;

            let has_admin = caller.has_admin_access();
            let effective_scope = scope_arg.unwrap_or(if has_admin { "all" } else { "own" });
            if effective_scope == "all" && !has_admin {
                return Err(upstream_error(
                    folders::admin_access_required(
                        &caller.ctx,
                        caller.allow_delete,
                        Stage::Input,
                        false,
                    ),
                    Map::new(),
                    None,
                ));
            }

            let roots = library_roots_in(&state.db, state.navidrome_music_path.as_deref())
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;

            let owned_list: Vec<(Folder, String, bool)> = if effective_scope == "all" {
                let all = folders::list_all_with_owners_in(&state.db)
                    .await
                    .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
                all.into_iter()
                    .map(|row| {
                        let is_own = row.folder.user_id == caller.ctx.user_id;
                        (row.folder, row.owner, is_own)
                    })
                    .collect()
            } else {
                let own = folders::list_in(&state.db, &caller.ctx)
                    .await
                    .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
                own.into_iter()
                    .map(|f| (f, caller.ctx.username.clone(), true))
                    .collect()
            };

            let more = owned_list.len() > page.offset.saturating_add(page.limit);
            let shown: Vec<(Folder, String, bool)> = owned_list
                .into_iter()
                .skip(page.offset)
                .take(page.limit)
                .collect();
            let meta: Vec<(String, bool)> = shown
                .iter()
                .map(|(_, owner, is_own)| (owner.clone(), *is_own))
                .collect();
            let raw_folders: Vec<Folder> = shown.into_iter().map(|(f, _, _)| f).collect();
            let items: Vec<Value> = with_free_space(raw_folders)
                .await
                .iter()
                .zip(meta.iter())
                .map(|(f, (owner, is_own))| owned_folder_fields(f, owner, *is_own, &roots))
                .collect();
            let mut fields = Map::new();
            fields.insert("scope".into(), json!(effective_scope));
            Ok(fit(fields, "folders", items, &page, more))
        }
        .await,
    )
}

const UPDATE_SETTINGS_ARGS: &[&str] = &[
    "auto_delete",
    "accept_shares",
    "discovery_enabled",
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
        format!("Changes this user's Soulbeet settings: auto-delete of 1-star songs at the rating sync (it can only be turned off here: the user turns it on in Soulbeet's Settings, since it deletes the files of songs rated 1 star), accept_shares (whether other household users can share tracks into this user's library), and discovery (on or off, the profiles, per profile track count 1 to 50, lifetime 1 to 90 days and playlist name, all three profiles named in each map). The promote threshold is changed only by the user, in Soulbeet's Settings, whether or not deletion is switched on for this app: the rating sync moves pending tracks rated at or above the threshold into the library, and a promote can replace and delete an earlier library copy outside this app's daily limit. Turning discovery off or dropping a profile deletes that profile's discovery playlist in Navidrome; it deletes no music. Only the settings given change. Returns the settings afterwards as get_settings shows them; credentials are never shown. Failure codes: {common}, permission_not_granted (auto_delete: true or discovery_promote_threshold), already_running.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"auto_delete": {"type": "boolean"}, "accept_shares": {"type": "boolean"}, "discovery_enabled": {"type": "boolean"}, "discovery_profiles": {"type": "array", "minItems": 1, "uniqueItems": true, "items": {"enum": ["Conservative", "Balanced", "Adventurous"]}}, "discovery_track_counts": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "integer", "minimum": 1, "maximum": 50}, "Balanced": {"type": "integer", "minimum": 1, "maximum": 50}, "Adventurous": {"type": "integer", "minimum": 1, "maximum": 50}}}, "discovery_lifetime_days": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "integer", "minimum": 1, "maximum": 90}, "Balanced": {"type": "integer", "minimum": 1, "maximum": 90}, "Adventurous": {"type": "integer", "minimum": 1, "maximum": 90}}}, "discovery_playlist_names": {"type": "object", "additionalProperties": false, "properties": {"Conservative": {"type": "string"}, "Balanced": {"type": "string"}, "Adventurous": {"type": "string"}}}, "discovery_promote_threshold": {"type": "integer", "minimum": 1, "maximum": 5}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: true,
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
        "Only you can turn auto-delete on, in Soulbeet's Settings: it deletes the files of songs you rate 1 star at every rating sync",
    )
    .with_user_action(UserAction::new(
        UserActionCode::EnableAutoDelete,
        "Turn on auto-delete yourself",
        "Soulbeet Settings > Library",
    ))
}

/// Lowering the promote threshold has the next rating sync promote pending discovery tracks
/// rated below the old one, and a promote can replace an earlier library copy, deleting it. The
/// 6 hourly sync promotes as Soulful's own deletion, past the app's switch, its daily room and
/// its name in the history, so no app changes it, whatever its switch, as no app's rating may
/// promote (`core::ratings::rating_refusal`).
fn promote_threshold_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Only you can change the discovery promote threshold, in Soulbeet's Settings: the rating sync moves the pending discovery tracks rated at or above it into your library, replacing and deleting any earlier copy there",
    )
    .with_user_action(UserAction::new(
        UserActionCode::ChangeDiscoverySettings,
        "Change the discovery promote threshold yourself",
        // Its own row of the Library tab, below the Discovery block.
        "Soulbeet Settings > Library",
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
        auto_delete_enabled: auto_delete,
        accept_shares: args.opt_bool("accept_shares")?,
        discovery_enabled: args.opt_bool("discovery_enabled")?,
        discovery_profiles: args
            .opt_choices("discovery_profiles", profiles)?
            .map(|profiles| profiles.join(",")),
        discovery_track_count: map("discovery_track_counts")?,
        discovery_lifetime_days: map("discovery_lifetime_days")?,
        discovery_playlist_name: map("discovery_playlist_names")?,
        discovery_promote_threshold: args
            .opt_u32(
                "discovery_promote_threshold",
                u32::from(*PROMOTE_THRESHOLDS.start()),
                u32::from(*PROMOTE_THRESHOLDS.end()),
            )?
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
                "discovery_enabled",
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
                let error = upstream_error(not_saved.failure, facts, args.same_call());
                return Keyed::Released(failed(error));
            }
        };
        let mut fields = Map::new();
        fields.insert("settings".into(), Value::Object(settings_fields(&saved)));
        Keyed::Kept(success(fields))
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
        s.listenbrainz_username = Some("alice_lb".into());
        s.listenbrainz_token = Some("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d".into());
        s.deezer_recommendations = true;
        let doc = Value::Object(settings_fields(&s));
        assert_eq!(
            doc,
            json!({
                "auto_delete_enabled": false,
                "accept_shares": true,
                "listenbrainz": {"username": "alice_lb", "token_saved": true},
                "discovery": {"enabled": false, "profiles": ["Conservative", "Balanced", "Adventurous"],
                    "track_count": {"Conservative": 10, "Balanced": 10, "Adventurous": 10},
                    "lifetime_days": {"Conservative": 7, "Balanced": 7, "Adventurous": 7},
                    "playlist_names": {"Conservative": "Comfort Zone", "Balanced": "Fresh Picks", "Adventurous": "Deep Cuts"},
                    "playlist_ids": {}, "promote_threshold": 3, "last_generated_at": null,
                    "deezer_recommendations": true}
            })
        );
        assert!(!doc
            .to_string()
            .contains("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d"));
        assert!(doc.get("lastfm").is_none() && doc.get("default_metadata_provider").is_none());
    }

    /// The tool's document when the caller's rows cannot be read: the
    /// table is gone, so sqlx fails as SQLite does on any missing table.
    async fn unreadable_rows(tool: &str) -> Value {
        let db = test_db::migrated().await;
        let table = match tool {
            "get_settings" => "user_settings",
            _ => "folders",
        };
        sqlx::query(&format!("DROP TABLE {table}"))
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
        for (tool, table, call) in [
            (
                "get_settings",
                "user_settings",
                json!({"tool": "get_settings", "args": {}}),
            ),
            (
                "list_folders",
                "folders",
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
                    Some("Soulbeet's database is unavailable")
                ),
                "{tool}"
            );
            // sqlx's text for SQLite's "no such table" (sqlx-core 0.8.6 src/error.rs:44)
            assert_eq!(
                e["cause"],
                json!({"service": "database", "status": null,
                    "raw": format!("error returned from database: (code: 1) no such table: {table}")}),
                "{tool}"
            );
            assert_eq!(e["next"], json!([call]), "{tool}");
        }
    }

    #[tokio::test]
    async fn discovery_settings_read_as_discovery_reads_them() {
        let db = test_db::migrated().await;
        let mut s = UserSettings::get_in(&db, "u1").await.unwrap();
        // The forms the Settings page writes and the playlist sync stores.
        s.discovery_track_count = r#"{"Conservative":5,"Balanced":10,"Adventurous":15}"#.into();
        s.discovery_playlist_name =
            r#"{"Conservative":"Comfort Zone","Balanced":"Weekly Mix","Adventurous":"Deep Cuts"}"#
                .into();
        s.discovery_navidrome_playlist_id = Some(r#"{"Balanced":"9fR2kLm4NpQ7sT1vW3xY5z"}"#.into());
        let doc = settings_fields(&s);
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
    use crate::models::user_settings::{LIFETIME_DAYS, TRACK_COUNTS};

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
                "Only you can turn auto-delete on, in Soulbeet's Settings: it deletes the files of songs you rate 1 star at every rating sync"
            );
            assert_eq!(
                error["user_action"],
                json!({"code": "enable_auto_delete", "what": "Turn on auto-delete yourself", "where": "Soulbeet Settings > Library"})
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
    async fn the_promote_threshold_is_changed_only_in_soulfuls_settings() {
        // A lower threshold has the 6 hourly sync promote pending tracks as Soulful's own
        // deletion, and a promote can replace an earlier library copy: outside the app's switch,
        // its daily room and its name in the history. So no app changes it, whatever its switch.
        let (db, app, token) = mcp_app().await;
        let call = json!({"discovery_promote_threshold": 1, "request_key": "k"});
        for allow_delete in [0, 1] {
            sqlx::query("UPDATE oauth_grants SET allow_delete = ?")
                .bind(allow_delete)
                .execute(&db)
                .await
                .unwrap();
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
                    Some("Only you can change the discovery promote threshold, in Soulbeet's Settings: the rating sync moves the pending discovery tracks rated at or above it into your library, replacing and deleting any earlier copy there")
                ),
                "allow_delete {allow_delete}"
            );
            assert_eq!(
                error["user_action"],
                json!({"code": "change_discovery_settings", "what": "Change the discovery promote threshold yourself", "where": "Soulbeet Settings > Library"}),
                "allow_delete {allow_delete}"
            );
            assert_eq!(
                (&error["facts"], &error["next"]),
                (&json!({"field": "discovery_promote_threshold"}), &json!([])),
                "allow_delete {allow_delete}"
            );
            let saved = UserSettings::get_in(&db, "u1").await.unwrap();
            assert_eq!(
                saved.discovery_promote_threshold, 3,
                "allow_delete {allow_delete}"
            );
        }
    }

    #[tokio::test]
    async fn accept_shares_updates_and_credentials_never_show() {
        let (db, app, token) = mcp_app().await;
        UserSettings::upsert_in(
            &db,
            "u1",
            UpdateUserSettings {
                listenbrainz_token: Some("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = call_tool(
            app,
            &token,
            "update_settings",
            json!({"discovery_enabled": true, "accept_shares": false}),
        )
        .await
        .to_string();
        assert!(text.contains(r#"\"enabled\":true"#), "{text}");
        assert!(text.contains(r#"\"accept_shares\":false"#), "{text}");
        assert!(
            !text.contains("3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e")
                && !text.contains("5a1c9e7b-3d2f-4a6b-8c0d-1e2f3a4b5c6d"),
            "{text}"
        );
    }

    #[test]
    fn the_schema_states_the_bounds_settings_keep() {
        let schema: Value = serde_json::from_str(UPDATE_SETTINGS.input_schema).unwrap();
        let bounds = |value: &Value| (value["minimum"].as_i64(), value["maximum"].as_i64());
        let range = |r: &std::ops::RangeInclusive<i64>| (Some(*r.start()), Some(*r.end()));
        for profile in DiscoveryProfile::NAMES {
            let props = &schema["properties"];
            assert_eq!(
                bounds(&props["discovery_track_counts"]["properties"][profile]),
                range(&TRACK_COUNTS)
            );
            assert_eq!(
                bounds(&props["discovery_lifetime_days"]["properties"][profile]),
                range(&LIFETIME_DAYS)
            );
        }
        assert_eq!(
            bounds(&schema["properties"]["discovery_promote_threshold"]),
            (
                Some(i64::from(*PROMOTE_THRESHOLDS.start())),
                Some(i64::from(*PROMOTE_THRESHOLDS.end()))
            )
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
            "update_settings: discovery_enabled must be given when no other setting is"
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

    #[tokio::test]
    async fn list_folders_supports_admin_all_scope_and_refuses_without_admin_access() {
        use crate::connection::test_util::{access_token, add_user, RESOURCE};
        use crate::security::SEEDED_ADMIN_ID;

        let (db, app, user_token) = mcp_app().await;
        add_user(&db, "u2", "wife").await;
        sqlx::query(
            "INSERT INTO folders (id, user_id, name, path) VALUES \
             ('f-alice', 'u1', 'Alice', '/music/alice'), \
             ('f-wife', 'u2', 'Wife', '/music/wife'), \
             ('f-admin', ?, 'Admin', '/music/admin')",
        )
        .bind(SEEDED_ADMIN_ID)
        .execute(&db)
        .await
        .unwrap();

        // Regular user defaults to own and cannot request scope: all.
        let own = document(&call_tool(app.clone(), &user_token, "list_folders", json!({})).await);
        assert_eq!(own["ok"], true, "{own}");
        assert_eq!(own["scope"], "own");
        assert_eq!(own["folders"].as_array().unwrap().len(), 1);
        assert_eq!(own["folders"][0]["folder_id"], "f-alice");
        assert_eq!(own["folders"][0]["is_own"], true);

        let refused_user = document(
            &call_tool(
                app.clone(),
                &user_token,
                "list_folders",
                json!({"scope": "all"}),
            )
            .await,
        );
        assert_eq!(refused_user["error"]["code"], "permission_not_granted");
        assert!(refused_user["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Admin"));

        // Admin WITHOUT allow_admin still defaults to own and refuses scope: all.
        let admin_token = access_token(&db, SEEDED_ADMIN_ID, RESOURCE).await;
        let admin_own =
            document(&call_tool(app.clone(), &admin_token, "list_folders", json!({})).await);
        assert_eq!(admin_own["scope"], "own");
        assert_eq!(admin_own["folders"].as_array().unwrap().len(), 1);

        let refused_admin = document(
            &call_tool(
                app.clone(),
                &admin_token,
                "list_folders",
                json!({"scope": "all"}),
            )
            .await,
        );
        assert_eq!(refused_admin["error"]["code"], "permission_not_granted");
        assert!(refused_admin["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Admin library access (all users' folders)"));

        // Admin WITH allow_admin defaults to scope: all and sees all users' folders with owners.
        sqlx::query("UPDATE oauth_grants SET allow_admin = 1 WHERE user_id = ?")
            .bind(SEEDED_ADMIN_ID)
            .execute(&db)
            .await
            .unwrap();
        let all = document(&call_tool(app.clone(), &admin_token, "list_folders", json!({})).await);
        assert_eq!(all["ok"], true, "{all}");
        assert_eq!(all["scope"], "all");
        let folders = all["folders"].as_array().unwrap();
        assert_eq!(folders.len(), 3);
        let wife_folder = folders.iter().find(|f| f["folder_id"] == "f-wife").unwrap();
        assert_eq!(wife_folder["owner"], "wife");
        assert_eq!(wife_folder["is_own"], false);

        // Admin can still filter to scope: own explicitly.
        let own_only = document(
            &call_tool(app, &admin_token, "list_folders", json!({"scope": "own"})).await,
        );
        assert_eq!(own_only["scope"], "own");
        assert_eq!(own_only["folders"].as_array().unwrap().len(), 1);
        assert_eq!(own_only["folders"][0]["folder_id"], "f-admin");
    }
}
