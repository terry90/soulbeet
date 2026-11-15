use dioxus::prelude::*;

pub use crate::models::user_settings::{UpdateUserSettings, UserSettings};

#[cfg(feature = "server")]
use crate::models::app_config::AppConfig;
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};

#[cfg(feature = "server")]
use super::{failure_error, server_error};

/// Get current user's settings
#[get("/api/settings", auth: AuthSession)]
pub async fn get_user_settings() -> Result<UserSettings, ServerFnError> {
    crate::core::settings::get(&auth.0)
        .await
        .map_err(failure_error)
}

/// Update current user's settings
#[post("/api/settings", auth: AuthSession)]
pub async fn update_user_settings(
    update: UpdateUserSettings,
) -> Result<UserSettings, ServerFnError> {
    crate::core::settings::update(&auth.0, update)
        .await
        .map_err(failure_error)
}

/// Get list of available metadata providers
#[get("/api/settings/providers", auth: AuthSession)]
pub async fn get_metadata_providers() -> Result<Vec<ProviderInfo>, ServerFnError> {
    use crate::services::{available_metadata_providers, providers, usable_lastfm_key};

    let user_settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let key = user_settings.lastfm_api_key.as_deref();
    Ok(available_metadata_providers()
        .into_iter()
        .map(|(id, name)| ProviderInfo {
            id: id.to_string(),
            name: name.to_string(),
            available: id != providers::LASTFM || usable_lastfm_key(key).is_some(),
        })
        .collect())
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

#[get("/api/config", _: AdminSession)]
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

#[post("/api/config", _: AdminSession)]
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
