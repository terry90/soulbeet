//! A user's settings.

use crate::auth::UserCtx;
use crate::core::{database_failure, folders::owned_in};
use crate::models::user_settings::{UpdateUserSettings, UserSettings};
use shared::failure::{Failure, Stage};
use sqlx::SqlitePool;

pub async fn get(ctx: &UserCtx) -> Result<UserSettings, Failure> {
    UserSettings::get(&ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

pub async fn update(ctx: &UserCtx, update: UpdateUserSettings) -> Result<UserSettings, Failure> {
    check_folder_choices(&crate::db::DB, ctx, &update).await?;
    let old = get(ctx).await?;
    let result = UserSettings::upsert(&ctx.user_id, update.clone())
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    cleanup_stale_discovery_playlists(&ctx.user_id, &old, &update).await;

    Ok(result)
}

/// Refuses a discovery or default download folder the user does not own,
/// as "Folder not found", the same as for a folder that does not exist.
/// Discovery downloads into that folder and deletes files there. An empty
/// id clears the choice.
async fn check_folder_choices(
    db: &SqlitePool,
    ctx: &UserCtx,
    update: &UpdateUserSettings,
) -> Result<(), Failure> {
    let chosen = [
        &update.discovery_folder_id,
        &update.default_download_folder_id,
    ];
    for id in chosen.into_iter().flatten().filter(|id| !id.is_empty()) {
        owned_in(db, ctx, id).await?;
    }
    Ok(())
}

/// Delete Navidrome smart playlists that are no longer valid because
/// discovery was disabled, the folder changed, or profiles were removed.
async fn cleanup_stale_discovery_playlists(
    user_id: &str,
    old: &UserSettings,
    update: &UpdateUserSettings,
) {
    use dioxus::logger::tracing::{info, warn};

    let discovery_disabled = update.discovery_enabled == Some(false) && old.discovery_enabled;
    let folder_changed = update.discovery_folder_id.is_some()
        && update.discovery_folder_id != old.discovery_folder_id;
    let profiles_changed = update.discovery_profiles.is_some()
        && update.discovery_profiles.as_deref() != Some(&old.discovery_profiles);

    if !discovery_disabled && !folder_changed && !profiles_changed {
        return;
    }

    let old_ids: std::collections::HashMap<String, String> = old
        .discovery_navidrome_playlist_id
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if old_ids.is_empty() {
        return;
    }

    let navi = match crate::services::navidrome_client_for_user(user_id).await {
        Ok(c) => c,
        Err(_) => return,
    };

    // Determine which profile playlists to delete
    let profiles_to_delete: Vec<String> = if discovery_disabled || folder_changed {
        // All playlists are stale
        old_ids.keys().cloned().collect()
    } else {
        // Only profiles that were removed
        let new_profiles = update.discovery_profiles.as_deref().unwrap_or("");
        let new_set: std::collections::HashSet<&str> =
            new_profiles.split(',').map(|s| s.trim()).collect();
        old_ids
            .keys()
            .filter(|p| !new_set.contains(p.as_str()))
            .cloned()
            .collect()
    };

    for profile in &profiles_to_delete {
        if let Some(playlist_id) = old_ids.get(profile) {
            if let Err(e) = navi.delete_smart_playlist(playlist_id).await {
                warn!(
                    "Failed to delete playlist '{}' ({}): {}",
                    profile, playlist_id, e
                );
            } else {
                info!(
                    "Deleted stale discovery playlist '{}' ({})",
                    profile, playlist_id
                );
            }
        }
    }

    // Clear stale IDs from user_settings
    let remaining: std::collections::HashMap<String, String> = old_ids
        .into_iter()
        .filter(|(k, _)| !profiles_to_delete.contains(k))
        .collect();
    let new_json = if remaining.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&remaining).unwrap_or_default())
    };
    let _ = sqlx::query(
        "UPDATE user_settings SET discovery_navidrome_playlist_id = ? WHERE user_id = ?",
    )
    .bind(&new_json)
    .bind(user_id)
    .execute(&*crate::db::DB)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::folders::folder_not_found;
    use crate::core::{folder_of, test_ctx};
    use crate::db::test_db;

    #[tokio::test]
    async fn another_users_folder_cannot_be_chosen() {
        let db = test_db::migrated().await;
        let alice_folder = folder_of(&db, "alice").await;
        folder_of(&db, "bob").await;
        for update in [
            UpdateUserSettings {
                discovery_folder_id: Some(alice_folder.clone()),
                ..Default::default()
            },
            UpdateUserSettings {
                default_download_folder_id: Some(alice_folder.clone()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                check_folder_choices(&db, &test_ctx("bob"), &update).await,
                Err(folder_not_found())
            );
        }
    }

    #[tokio::test]
    async fn another_users_shared_folder_cannot_be_the_discovery_folder() {
        // Discovery removes files in its folder as the folder's owner, so a shared folder of
        // another user must not be one.
        let db = test_db::migrated().await;
        let alice_folder = folder_of(&db, "alice").await;
        folder_of(&db, "bob").await;
        sqlx::query("UPDATE folders SET shared = 1 WHERE id = ?")
            .bind(&alice_folder)
            .execute(&db)
            .await
            .unwrap();
        let update = UpdateUserSettings {
            discovery_folder_id: Some(alice_folder),
            ..Default::default()
        };
        assert_eq!(
            check_folder_choices(&db, &test_ctx("bob"), &update).await,
            Err(folder_not_found())
        );
    }

    #[tokio::test]
    async fn own_folders_and_an_empty_choice_are_accepted() {
        let db = test_db::migrated().await;
        let bob_folder = folder_of(&db, "bob").await;
        let bob = test_ctx("bob");
        let update = UpdateUserSettings {
            discovery_folder_id: Some(bob_folder.clone()),
            default_download_folder_id: Some(bob_folder),
            ..Default::default()
        };
        assert_eq!(check_folder_choices(&db, &bob, &update).await, Ok(()));
        let cleared = UpdateUserSettings {
            discovery_folder_id: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(check_folder_choices(&db, &bob, &cleared).await, Ok(()));
        assert_eq!(
            check_folder_choices(&db, &bob, &UpdateUserSettings::default()).await,
            Ok(())
        );
    }
}
