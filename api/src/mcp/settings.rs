//! `get_settings` and `list_folders`: the caller's own settings and folders.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};

use super::args::Args;
use super::contract::{answer, upstream_error, ToolError};
use super::paging::{fit, Page};
use super::status::{chosen_folder, folder_fields, with_free_space};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::library::library_roots_in;
use crate::core::{folders, settings};
use crate::models::folder::Folder;
use crate::models::user_settings::UserSettings;
use crate::server_fns::discovery::parse_profiles;

/// The discovery profiles, as `UserSettings` names them in its maps.
const PROFILES: [&str; 3] = ["Conservative", "Balanced", "Adventurous"];

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
    let playlist_names: Map<String, Value> = PROFILES
        .iter()
        .map(|p| {
            let name = UserSettings::get_playlist_name_for_profile(&s.discovery_playlist_name, p);
            (p.to_string(), json!(name))
        })
        .collect();
    let playlist_ids: Map<String, Value> = PROFILES
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
}
