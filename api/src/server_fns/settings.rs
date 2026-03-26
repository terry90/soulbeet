use dioxus::prelude::*;

pub use crate::models::user_settings::{UpdateUserSettings, UserSettings};

#[cfg(feature = "server")]
use crate::models::app_config::AppConfig;
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::server_error;

/// Get current user's settings
#[get("/api/settings", auth: AuthSession)]
pub async fn get_user_settings() -> Result<UserSettings, ServerFnError> {
    UserSettings::get(&auth.0.sub).await.map_err(server_error)
}

/// Update current user's settings
#[post("/api/settings", auth: AuthSession)]
pub async fn update_user_settings(
    update: UpdateUserSettings,
) -> Result<UserSettings, ServerFnError> {
    #[cfg(feature = "server")]
    {
        let old = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
        let result = UserSettings::upsert(&auth.0.sub, update.clone())
            .await
            .map_err(server_error)?;
        cleanup_stale_discovery_playlists(&auth.0.sub, &old, &update).await;

        Ok(result)
    }
    #[cfg(not(feature = "server"))]
    {
        let _ = update;
        unreachable!()
    }
}

/// Delete Navidrome smart playlists that are no longer valid because
/// discovery was disabled, the folder changed, or profiles were removed.
#[cfg(feature = "server")]
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

/// Get list of available metadata providers
#[get("/api/settings/providers", auth: AuthSession)]
pub async fn get_metadata_providers() -> Result<Vec<ProviderInfo>, ServerFnError> {
    use crate::services::{available_metadata_providers, metadata_provider};

    let user_settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
    let mut providers = Vec::new();
    for (id, name) in available_metadata_providers() {
        let available = metadata_provider(Some(id), user_settings.lastfm_api_key.as_deref())
            .await
            .is_ok();
        providers.push(ProviderInfo {
            id: id.to_string(),
            name: name.to_string(),
            available,
        });
    }

    Ok(providers)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub available: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct AppConfigValues {
    pub slskd_url: Option<String>,
    pub slskd_api_key: Option<String>,
}

#[get("/api/config", _: AuthSession)]
pub async fn get_app_config() -> Result<AppConfigValues, ServerFnError> {
    use crate::models::app_config::keys;

    let slskd_url = AppConfig::get(keys::SLSKD_URL)
        .await
        .map_err(server_error)?;
    let slskd_api_key = AppConfig::get(keys::SLSKD_API_KEY)
        .await
        .map_err(server_error)?;

    Ok(AppConfigValues {
        slskd_url,
        slskd_api_key,
    })
}

#[post("/api/config", _: AuthSession)]
pub async fn update_app_config(config: AppConfigValues) -> Result<AppConfigValues, ServerFnError> {
    use crate::models::app_config::keys;
    use crate::services::reload_providers;

    async fn set_or_delete(key: &str, value: &Option<String>) -> Result<(), ServerFnError> {
        if let Some(v) = value {
            if v.is_empty() {
                AppConfig::delete(key).await.map_err(server_error)?;
            } else {
                AppConfig::set(key, v).await.map_err(server_error)?;
            }
        }
        Ok(())
    }

    set_or_delete(keys::SLSKD_URL, &config.slskd_url).await?;
    set_or_delete(keys::SLSKD_API_KEY, &config.slskd_api_key).await?;

    reload_providers().await;

    get_app_config().await
}
