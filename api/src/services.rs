#[cfg(feature = "server")]
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, LazyLock},
};

#[cfg(feature = "server")]
use soulbeet::{
    beets::{BeetsImporter, BeetsSettings},
    musicbrainz::MusicBrainzProvider,
    navidrome::NavidromeClientBuilder,
    slskd::{DownloadConfig, SoulseekClientBuilder},
    DownloadBackend, LastFmProvider, MetadataProvider, MusicImporter, NavidromeClient,
};
#[cfg(feature = "server")]
use tokio::sync::RwLock;

#[cfg(feature = "server")]
use crate::models::app_config::{keys, AppConfig};

pub mod providers {
    pub const MUSICBRAINZ: &str = "musicbrainz";
    pub const LASTFM: &str = "lastfm";
}

pub mod downloaders {
    pub const SLSKD: &str = "slskd";
}

pub mod importers {
    pub const BEETS: &str = "beets";
}

#[cfg(feature = "server")]
static METADATA_PROVIDERS: LazyLock<RwLock<HashMap<String, Arc<dyn MetadataProvider>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "server")]
static DOWNLOAD_BACKENDS: LazyLock<RwLock<HashMap<String, Arc<dyn DownloadBackend>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "server")]
static MUSIC_IMPORTERS: LazyLock<RwLock<HashMap<String, Arc<dyn MusicImporter>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "server")]
static NAVIDROME_CLIENTS: LazyLock<RwLock<HashMap<String, Arc<NavidromeClient>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "server")]
pub fn available_metadata_providers() -> Vec<(&'static str, &'static str)> {
    vec![
        (providers::MUSICBRAINZ, "MusicBrainz"),
        (providers::LASTFM, "Last.fm"),
    ]
}

#[cfg(feature = "server")]
pub fn available_download_backends() -> Vec<(&'static str, &'static str)> {
    vec![(downloaders::SLSKD, "Soulseek")]
}

#[cfg(feature = "server")]
pub fn available_importers() -> Vec<(&'static str, &'static str)> {
    vec![(importers::BEETS, "Beets")]
}

#[cfg(feature = "server")]
async fn init_metadata_provider(
    id: &str,
    lastfm_api_key: Option<&str>,
) -> Result<Arc<dyn MetadataProvider>, String> {
    match id {
        providers::LASTFM => {
            let api_key = lastfm_api_key
                .filter(|k| !k.is_empty())
                .ok_or("Last.fm API key not configured")?;
            Ok(Arc::new(LastFmProvider::new(api_key.to_string())))
        }
        _ => Ok(Arc::new(MusicBrainzProvider::new())),
    }
}

#[cfg(feature = "server")]
async fn init_download_backend(id: &str) -> Result<Arc<dyn DownloadBackend>, String> {
    match id {
        downloaders::SLSKD => {
            let url = AppConfig::get(keys::SLSKD_URL)
                .await?
                .ok_or("slskd URL not configured")?;
            let api_key = AppConfig::get(keys::SLSKD_API_KEY)
                .await?
                .ok_or("slskd API key not configured")?;

            if url.is_empty() || api_key.is_empty() {
                return Err("slskd not configured".to_string());
            }

            let client = SoulseekClientBuilder::new()
                .base_url(&url)
                .api_key(&api_key)
                .download_config(DownloadConfig {
                    batch_size: 3,
                    batch_delay_ms: 3000,
                    max_retries: 3,
                    retry_base_delay_ms: 1000,
                })
                .build()
                .map_err(|e| e.to_string())?;

            Ok(Arc::new(client))
        }
        _ => Err(format!("Unknown download backend: {}", id)),
    }
}

#[cfg(feature = "server")]
pub async fn metadata_provider(
    id: Option<&str>,
    lastfm_api_key: Option<&str>,
) -> Result<Arc<dyn MetadataProvider>, String> {
    let requested = id.unwrap_or(providers::MUSICBRAINZ);

    // Don't cache Last.fm providers since keys are per-user
    if requested != providers::LASTFM {
        if let Some(provider) = METADATA_PROVIDERS.read().await.get(requested) {
            return Ok(provider.clone());
        }
    }

    let (key, provider) = match init_metadata_provider(requested, lastfm_api_key).await {
        Ok(p) => (requested.to_string(), p),
        Err(_) if requested != providers::MUSICBRAINZ => (
            providers::MUSICBRAINZ.to_string(),
            init_metadata_provider(providers::MUSICBRAINZ, None).await?,
        ),
        Err(e) => return Err(e),
    };

    // Only cache non-Last.fm providers
    if key != providers::LASTFM {
        METADATA_PROVIDERS
            .write()
            .await
            .insert(key, provider.clone());
    }
    Ok(provider)
}

#[cfg(feature = "server")]
pub async fn download_backend(id: Option<&str>) -> Result<Arc<dyn DownloadBackend>, String> {
    let requested = id.unwrap_or(downloaders::SLSKD);

    if let Some(backend) = DOWNLOAD_BACKENDS.read().await.get(requested) {
        return Ok(backend.clone());
    }

    let backend = init_download_backend(requested).await?;
    DOWNLOAD_BACKENDS
        .write()
        .await
        .insert(requested.to_string(), backend.clone());
    Ok(backend)
}

#[cfg(feature = "server")]
fn init_importer(id: &str) -> Result<Arc<dyn MusicImporter>, String> {
    match id {
        importers::BEETS => {
            // Named in full: `AppConfig` here is the database-backed settings.
            let config = &crate::config::CONFIG;
            Ok(Arc::new(BeetsImporter::new(BeetsSettings {
                config: config.beets_config().clone(),
                beet: config.beet_bin().to_path_buf(),
                python: config.beets_python().map(Path::to_path_buf),
                limits: Default::default(),
            })))
        }
        _ => Err(format!("Unknown importer: {}", id)),
    }
}

#[cfg(feature = "server")]
pub async fn music_importer(id: Option<&str>) -> Result<Arc<dyn MusicImporter>, String> {
    let requested = id.unwrap_or(importers::BEETS);

    if let Some(importer) = MUSIC_IMPORTERS.read().await.get(requested) {
        return Ok(importer.clone());
    }

    // Built under the write lock, so concurrent first callers share one
    // instance and its health.
    let mut importers = MUSIC_IMPORTERS.write().await;
    if let Some(importer) = importers.get(requested) {
        return Ok(importer.clone());
    }
    let importer = init_importer(requested)?;
    importers.insert(requested.to_string(), importer.clone());
    Ok(importer)
}

#[cfg(feature = "server")]
pub async fn navidrome_client_for_user(user_id: &str) -> Result<Arc<NavidromeClient>, String> {
    // Check cache first
    if let Some(client) = NAVIDROME_CLIENTS.read().await.get(user_id) {
        return Ok(client.clone());
    }

    let user = crate::models::user::User::get_by_id(user_id).await?;

    let encrypted_token = user
        .navidrome_token
        .ok_or("Navidrome not configured for user")?;

    let password = match crate::crypto::decrypt(&encrypted_token) {
        Ok(password) => password,
        Err(_) => {
            tracing::warn!("Stored Navidrome password of user {user_id} cannot be decrypted");
            if user.navidrome_status != shared::system::NavidromeStatus::InvalidCredentials.as_str()
            {
                if let Err(e) =
                    mark_password_unreadable(&crate::db::DB, user_id, &encrypted_token).await
                {
                    tracing::warn!("Could not mark user {user_id} for a new sign-in: {e}");
                }
            }
            return Err(STORED_PASSWORD_UNREADABLE.to_string());
        }
    };

    let url = std::env::var("NAVIDROME_URL")
        .map_err(|_| "NAVIDROME_URL environment variable not set".to_string())?;

    let client = NavidromeClientBuilder::new()
        .base_url(&url)
        .username(&user.username)
        .password(&password)
        .build()
        .map_err(|e| e.to_string())?;

    let client = Arc::new(client);
    NAVIDROME_CLIENTS
        .write()
        .await
        .insert(user_id.to_string(), client.clone());
    Ok(client)
}

#[cfg(feature = "server")]
pub const STORED_PASSWORD_UNREADABLE: &str = "The stored Navidrome password cannot be decrypted, probably because SECRET_KEY changed. Sign in again to store it.";

/// Keeps the undecryptable token and asks the user to sign in again.
#[cfg(feature = "server")]
pub(crate) async fn mark_password_unreadable(
    db: &sqlx::SqlitePool,
    user_id: &str,
    encrypted_token: &str,
) -> Result<(), String> {
    crate::models::user::User::update_navidrome_token_in(
        db,
        user_id,
        Some(encrypted_token),
        shared::system::NavidromeStatus::InvalidCredentials.as_str(),
    )
    .await?;
    crate::models::user_settings::UserSettings::reset_navidrome_banner_in(db, user_id).await
}

#[cfg(feature = "server")]
pub async fn evict_navidrome_client(user_id: &str) {
    NAVIDROME_CLIENTS.write().await.remove(user_id);
}

#[cfg(feature = "server")]
pub async fn reload_providers() {
    METADATA_PROVIDERS.write().await.clear();
    DOWNLOAD_BACKENDS.write().await.clear();
    // MUSIC_IMPORTERS stays: importers are built from the environment only,
    // and dropping one would drop its cached health.
    NAVIDROME_CLIENTS.write().await.clear();
}

#[cfg(feature = "server")]
pub async fn is_slskd_configured() -> bool {
    let url = AppConfig::get(keys::SLSKD_URL).await.ok().flatten();
    let api_key = AppConfig::get(keys::SLSKD_API_KEY).await.ok().flatten();
    matches!((url, api_key), (Some(u), Some(k)) if !u.is_empty() && !k.is_empty())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::security::SEEDED_ADMIN_ID;

    #[tokio::test]
    async fn an_unreadable_password_marks_the_user_and_resets_the_banner() {
        let db = test_db::migrated().await;
        sqlx::query("UPDATE users SET navidrome_token = 'cipher', navidrome_status = 'connected' WHERE id = ?")
            .bind(SEEDED_ADMIN_ID)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_settings (user_id, navidrome_banner_dismissed) VALUES (?, 1) ON CONFLICT(user_id) DO UPDATE SET navidrome_banner_dismissed = 1",
        )
        .bind(SEEDED_ADMIN_ID)
        .execute(&db)
        .await
        .unwrap();

        mark_password_unreadable(&db, SEEDED_ADMIN_ID, "cipher")
            .await
            .unwrap();

        let user: (Option<String>, String) =
            sqlx::query_as("SELECT navidrome_token, navidrome_status FROM users WHERE id = ?")
                .bind(SEEDED_ADMIN_ID)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            user,
            (
                Some("cipher".to_string()),
                "invalid_credentials".to_string()
            )
        );
        let dismissed: i64 = sqlx::query_scalar(
            "SELECT navidrome_banner_dismissed FROM user_settings WHERE user_id = ?",
        )
        .bind(SEEDED_ADMIN_ID)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(dismissed, 0);
    }

    #[tokio::test]
    async fn the_importer_and_its_health_survive_a_provider_reload() {
        let first = music_importer(None).await.unwrap();
        reload_providers().await;
        let second = music_importer(None).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_one_importer() {
        let callers: Vec<_> = (0..16)
            .map(|_| tokio::spawn(music_importer(None)))
            .collect();
        let mut importers = Vec::new();
        for caller in callers {
            importers.push(caller.await.unwrap().unwrap());
        }
        assert!(importers.iter().all(|i| Arc::ptr_eq(i, &importers[0])));
    }
}
