//! The Direct Connect settings an admin gives: the AirDC++ sidecar's address, its web user and
//! that user's password. The password is stored encrypted and never sent back.

use dioxus::prelude::*;
pub use shared::airdcpp::{SidecarReport, SidecarSettings};

#[cfg(feature = "server")]
use super::failure_error;
#[cfg(feature = "server")]
use crate::AdminSession;

/// What the settings form saves. An empty address or user name clears it; no password keeps the
/// stored one.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SidecarSettingsUpdate {
    pub url: String,
    pub username: String,
    pub password: Option<String>,
}

impl std::fmt::Debug for SidecarSettingsUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarSettingsUpdate")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password_given", &self.password.is_some())
            .finish()
    }
}

#[get("/api/airdcpp/settings", _: AdminSession)]
pub async fn get_airdcpp_settings() -> Result<SidecarSettings, ServerFnError> {
    stored::read_in(&crate::db::DB).await.map_err(failure_error)
}

/// Saves the settings and drops the built backend, so the next lookup signs in with them, even
/// after AirDC++ refused a password.
#[post("/api/airdcpp/settings", _: AdminSession)]
pub async fn update_airdcpp_settings(
    update: SidecarSettingsUpdate,
) -> Result<SidecarSettings, ServerFnError> {
    stored::store_in(&crate::db::DB, crate::secret_key::current(), &update)
        .await
        .map_err(failure_error)?;
    soulbeet::airdcpp::forget_refusals();
    crate::services::reload_download_backend(crate::services::downloaders::AIRDCPP).await;
    get_airdcpp_settings().await
}

/// What the sidecar the stored settings name says of itself.
#[post("/api/airdcpp/check", _: AdminSession)]
pub async fn check_airdcpp() -> Result<SidecarReport, ServerFnError> {
    crate::services::check_airdcpp()
        .await
        .map_err(failure_error)
}

#[cfg(feature = "server")]
pub(crate) mod stored {
    use super::{SidecarSettings, SidecarSettingsUpdate};
    use crate::core::database_failure;
    use crate::models::app_config::{keys, AppConfig};
    use crate::secret_key::SecretKey;
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
    use sqlx::SqlitePool;

    pub(crate) async fn read_in(db: &SqlitePool) -> Result<SidecarSettings, Failure> {
        let read = |key| async move {
            AppConfig::get_in(db, key)
                .await
                .map_err(|e| database_failure(Stage::Input, e))
        };
        Ok(SidecarSettings {
            url: read(keys::AIRDCPP_URL).await?.unwrap_or_default(),
            username: read(keys::AIRDCPP_USERNAME).await?.unwrap_or_default(),
            password_set: read(keys::AIRDCPP_PASSWORD).await?.is_some(),
        })
    }

    pub(crate) async fn store_in(
        db: &SqlitePool,
        key: &SecretKey,
        update: &SidecarSettingsUpdate,
    ) -> Result<(), Failure> {
        let url = update.url.trim();
        if !url.is_empty() {
            soulbeet::airdcpp::api_base(url).map_err(|e| {
                Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    e.to_string(),
                )
            })?;
        }
        let stored = |e| database_failure(Stage::Input, e);
        let set_or_clear = |name, value: &str| {
            let value = value.trim().to_string();
            async move {
                match value.is_empty() {
                    true => AppConfig::delete_in(db, name).await,
                    false => AppConfig::set_in(db, name, &value).await,
                }
            }
        };
        set_or_clear(keys::AIRDCPP_URL, url).await.map_err(stored)?;
        set_or_clear(keys::AIRDCPP_USERNAME, &update.username)
            .await
            .map_err(stored)?;
        if let Some(password) = update.password.as_deref().filter(|p| !p.is_empty()) {
            let encrypted = crate::crypto::encrypt(key, password).map_err(stored)?;
            AppConfig::set_in(db, keys::AIRDCPP_PASSWORD, &encrypted)
                .await
                .map_err(stored)?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::stored::{read_in, store_in};
    use super::*;
    use crate::db::test_db;
    use crate::models::app_config::{keys, AppConfig};
    use crate::secret_key::SecretKey;
    use crate::services::{airdcpp_connection_in, BackendLookupError};
    use shared::failure::{FailureCode, Stage};

    fn update(url: &str, password: Option<&str>) -> SidecarSettingsUpdate {
        SidecarSettingsUpdate {
            url: url.into(),
            username: "soulbeet".into(),
            password: password.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn the_password_is_stored_encrypted_never_read_back_and_kept_when_none_is_sent() {
        let db = test_db::migrated().await;
        let key = SecretKey::new("k".repeat(40));
        store_in(&db, &key, &update("http://airdcpp:5600", Some("hunter22")))
            .await
            .unwrap();
        let stored = AppConfig::get_in(&db, keys::AIRDCPP_PASSWORD)
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.contains("hunter22"));
        assert_eq!(crate::crypto::decrypt(&key, &stored).unwrap(), "hunter22");

        let shown = read_in(&db).await.unwrap();
        assert_eq!(
            shown,
            SidecarSettings {
                url: "http://airdcpp:5600".into(),
                username: "soulbeet".into(),
                password_set: true,
            }
        );
        assert!(!serde_json::to_string(&shown).unwrap().contains("hunter22"));
        let logged = format!("{:?}", update("http://airdcpp:5600", Some("hunter22")));
        assert!(!logged.contains("hunter22"), "{logged}");

        // A save without a password keeps the stored one.
        store_in(&db, &key, &update("http://airdcpp:5601", None))
            .await
            .unwrap();
        let connection = airdcpp_connection_in(&db, &key).await.unwrap();
        assert_eq!(connection.password, "hunter22");
        assert_eq!(connection.url, "http://airdcpp:5601");

        // Another secret key cannot read it, and says so.
        let other = SecretKey::new("o".repeat(40));
        let Err(BackendLookupError::Backend(e)) = airdcpp_connection_in(&db, &other).await else {
            panic!("a password under another key was read");
        };
        assert_eq!(
            e.failure(Stage::Input, "m").code,
            FailureCode::BackendNotConfigured
        );
    }

    #[tokio::test]
    async fn an_address_with_a_secret_in_it_is_refused_and_nothing_is_stored() {
        let db = test_db::migrated().await;
        let key = SecretKey::new("k".repeat(40));
        let refused = store_in(
            &db,
            &key,
            &update("http://admin:pw@airdcpp:5600", Some("x")),
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, FailureCode::InvalidInput);
        assert!(!refused.message.contains("pw@"), "{}", refused.message);
        assert_eq!(read_in(&db).await.unwrap(), SidecarSettings::default());
    }

    #[tokio::test]
    async fn turned_on_without_an_address_the_backend_is_not_configured() {
        let db = test_db::migrated().await;
        let key = SecretKey::new("k".repeat(40));
        let Err(e) = airdcpp_connection_in(&db, &key).await else {
            panic!("a backend with no address was configured");
        };
        let f = e.failure(Stage::SourceSearch, e.to_string());
        assert_eq!(f.code, FailureCode::BackendNotConfigured);
        assert_eq!(f.message, "The AirDC++ address is not set");
        assert_eq!(
            f.user_action.map(|a| a.code),
            Some(shared::failure::UserActionCode::ConfigureBackend)
        );
    }
}
