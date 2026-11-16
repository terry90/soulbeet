//! `get_settings` and `list_folders`: the caller's own settings and folders.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::Failure;

use super::args::Args;
use super::contract::{failed, success, upstream_error, ToolError};
use super::paging::{fit, Page};
use super::status::{chosen_folder, folder_fields, with_free_space};
use super::McpCaller;
use crate::connection::ConnectionState;
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
            "folder_id": s.discovery_folder_id,
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

/// A failed read of the caller's rows: the same call can be tried again.
fn unreadable(failure: Failure, args: &Args) -> CallToolResult {
    failed(upstream_error(failure, Map::new(), args.same_call()))
}

/// `get_settings`, which takes no arguments.
pub(crate) async fn get_settings(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_settings", args, &[]);
    if let Err(e) = args.finish() {
        return failed(e);
    }
    let (settings, folders) = tokio::join!(
        settings::get_in(&state.db, &caller.ctx),
        folders::list_in(&state.db, &caller.ctx),
    );
    match (settings, folders) {
        (Ok(settings), Ok(folders)) => success(settings_fields(&settings, &folders)),
        (Err(failure), _) | (_, Err(failure)) => unreadable(failure, &args),
    }
}

/// `list_folders`, a page of the caller's folders.
pub(crate) async fn list_folders(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_folders", args, &["limit", "cursor"]);
    match read_page(&args) {
        Ok(page) => list_page(state, caller, &args, page).await,
        Err(e) => failed(e),
    }
}

fn read_page(args: &Args) -> Result<Page, ToolError> {
    let page = Page::read(args, json!({}))?;
    args.finish()?;
    Ok(page)
}

async fn list_page(
    state: &ConnectionState,
    caller: &McpCaller,
    args: &Args,
    page: Page,
) -> CallToolResult {
    let (settings, folders) = tokio::join!(
        settings::get_in(&state.db, &caller.ctx),
        folders::list_in(&state.db, &caller.ctx),
    );
    let (settings, folders) = match (settings, folders) {
        (Ok(settings), Ok(folders)) => (settings, folders),
        (Err(failure), _) | (_, Err(failure)) => return unreadable(failure, args),
    };
    let default = chosen_folder(settings.default_download_folder_id);
    let rest: Vec<Folder> = folders.into_iter().skip(page.offset).collect();
    let items: Vec<Value> = with_free_space(rest)
        .await
        .iter()
        .map(|f| folder_fields(f, default.as_deref()))
        .collect();
    let mut fields = Map::new();
    fields.insert("default_folder_id".into(), json!(default));
    success(fit(fields, "folders", items, &page, false))
}

#[cfg(test)]
mod tests {
    use super::*;
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
