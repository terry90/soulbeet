#[cfg(feature = "server")]
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
#[cfg(feature = "server")]
use soulbeet::{
    beets::{BeetsImporter, BeetsSettings},
    navidrome::NavidromeClientBuilder,
    slskd::{DownloadConfig, SoulseekClientBuilder},
    BackendError, DownloadBackend, MusicImporter, NavidromeClient,
};
#[cfg(feature = "server")]
use tokio::sync::RwLock;

#[cfg(feature = "server")]
use crate::models::app_config::{keys, AppConfig};

pub mod providers {
    pub const MUSICBRAINZ: &str = "musicbrainz";
}

pub mod downloaders {
    pub const SLSKD: &str = shared::slskd::BACKEND_ID;
}

pub mod importers {
    pub const BEETS: &str = "beets";
}

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
    vec![(providers::MUSICBRAINZ, "MusicBrainz")]
}

#[cfg(feature = "server")]
pub fn available_download_backends() -> Vec<(&'static str, &'static str)> {
    vec![(downloaders::SLSKD, "Soulseek")]
}

#[cfg(feature = "server")]
pub fn available_importers() -> Vec<(&'static str, &'static str)> {
    vec![(importers::BEETS, "Beets")]
}

/// Why `download_backend` has no backend to give. Its Display is the text
/// the lookup always showed.
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub enum BackendLookupError {
    /// The backend's settings could not be read from Soulful's database.
    Settings(String),
    /// The backend refused its settings; its own error says what is wrong.
    Backend(BackendError),
    /// No backend has this id.
    Unknown(String),
    /// The backend built for `id` goes by `named`, so nothing routed to `id` would reach it.
    Misnamed { id: String, named: &'static str },
}

#[cfg(feature = "server")]
impl std::fmt::Display for BackendLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendLookupError::Settings(e) => f.write_str(e),
            BackendLookupError::Backend(e) => write!(f, "{e}"),
            BackendLookupError::Unknown(id) => write!(f, "Unknown download backend: {id}"),
            BackendLookupError::Misnamed { id, named } => {
                write!(f, "Download backend {id} names itself {named}")
            }
        }
    }
}

#[cfg(feature = "server")]
impl BackendLookupError {
    /// This lookup error as the failure of `stage`, shown as `message`. The
    /// backend gives the code, cause and user action for its own settings.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        match self {
            BackendLookupError::Backend(e) => e.failure(stage, message),
            BackendLookupError::Settings(e) => Failure::new(
                FailureCode::SettingsUnreadable,
                stage,
                RetryAdvice::SameSourceLater,
                message,
            )
            .with_cause(Service::Database, None, e.clone()),
            // The caller named a backend that does not exist, or that no backend answers to.
            BackendLookupError::Unknown(_) | BackendLookupError::Misnamed { .. } => Failure::new(
                FailureCode::InvalidInput,
                stage,
                RetryAdvice::NotRetryable,
                message,
            ),
        }
    }
}

#[cfg(feature = "server")]
async fn init_download_backend(id: &str) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
    match id {
        downloaders::SLSKD => {
            let url = AppConfig::get(keys::SLSKD_URL)
                .await
                .map_err(BackendLookupError::Settings)?;
            let api_key = AppConfig::get(keys::SLSKD_API_KEY)
                .await
                .map_err(BackendLookupError::Settings)?;
            let client = SoulseekClientBuilder::from_settings(url, api_key)
                .and_then(|builder| {
                    builder
                        .download_config(DownloadConfig {
                            batch_size: 3,
                            batch_delay_ms: 3000,
                            max_retries: 3,
                            retry_base_delay_ms: 1000,
                        })
                        .build()
                })
                .map_err(|e| BackendLookupError::Backend(e.into()))?;

            Ok(Arc::new(client))
        }
        _ => Err(BackendLookupError::Unknown(id.to_string())),
    }
}

/// The backend `download_backend(None)` gives.
#[cfg(feature = "server")]
pub const DEFAULT_DOWNLOAD_BACKEND: &str = downloaders::SLSKD;

#[cfg(feature = "server")]
pub async fn download_backend(
    id: Option<&str>,
) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
    let requested = id.unwrap_or(DEFAULT_DOWNLOAD_BACKEND);

    if let Some(backend) = DOWNLOAD_BACKENDS.read().await.get(requested) {
        return Ok(backend.clone());
    }

    let backend = answering_to(requested, init_download_backend(requested).await?)?;
    DOWNLOAD_BACKENDS
        .write()
        .await
        .insert(requested.to_string(), backend.clone());
    Ok(backend)
}

/// `backend`, built for `id`, when it goes by `id`: a backend is kept only under its own id.
#[cfg(feature = "server")]
fn answering_to(
    id: &str,
    backend: Arc<dyn DownloadBackend>,
) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
    match backend.id() {
        named if named == id => Ok(backend),
        named => Err(BackendLookupError::Misnamed {
            id: id.to_string(),
            named,
        }),
    }
}

/// Where download backends are looked up by id, so tests answer with stubs.
#[cfg(feature = "server")]
#[async_trait::async_trait]
pub trait Backends: Send + Sync {
    async fn get(&self, id: &str) -> Result<Arc<dyn DownloadBackend>, BackendLookupError>;
}

/// The backends the server is configured with.
#[cfg(feature = "server")]
pub struct Registry;

#[cfg(feature = "server")]
#[async_trait::async_trait]
impl Backends for Registry {
    async fn get(&self, id: &str) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
        download_backend(Some(id)).await
    }
}

/// Backends looked up through `backends`, each id once: a later call for the same id gets the
/// first answer, error included.
#[cfg(feature = "server")]
pub struct BackendCache<'a> {
    backends: &'a dyn Backends,
    found: HashMap<String, Result<Arc<dyn DownloadBackend>, BackendLookupError>>,
}

#[cfg(feature = "server")]
impl<'a> BackendCache<'a> {
    pub fn new(backends: &'a dyn Backends) -> Self {
        Self {
            backends,
            found: HashMap::new(),
        }
    }

    pub async fn get(&mut self, id: &str) -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
        if let Some(found) = self.found.get(id) {
            return found.clone();
        }
        let found = self.backends.get(id).await;
        self.found.insert(id.to_string(), found.clone());
        found
    }
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
                libraries: beets_libraries(config.database_url())?,
                limits: Default::default(),
            })))
        }
        _ => Err(format!("Unknown importer: {}", id)),
    }
}

/// The folder of the beets library databases: `beets-libraries` next to the
/// database file `database_url` names, as `secret.key` is, so they sit with
/// Soulbeet's own data and not on the music's disk, often a network share
/// that SQLite cannot lock. A relative database path is read against the
/// working directory.
#[cfg(feature = "server")]
fn beets_libraries(database_url: &str) -> Result<PathBuf, String> {
    crate::db::database_file(database_url)
        .and_then(Path::parent)
        .map(|dir| dir.join("beets-libraries"))
        .ok_or_else(|| {
            format!(
                "DATABASE_URL '{database_url}' names no database file to keep the beets \
                 libraries next to"
            )
        })
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

/// Why no Navidrome client could be made for a user.
#[cfg(feature = "server")]
#[derive(Debug)]
pub enum NavidromeLookupError {
    /// Soulful's database could not be read.
    Database(String),
    /// The user row is gone.
    UserNotFound,
    /// No Navidrome password is stored for this user.
    NotLinked,
    /// The stored password cannot be decrypted (SECRET_KEY changed).
    PasswordUnreadable,
    /// NAVIDROME_URL is not set.
    NoServer,
    Client(soulbeet::NavidromeError),
}

#[cfg(feature = "server")]
impl std::fmt::Display for NavidromeLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NavidromeLookupError::Database(e) => f.write_str(e),
            NavidromeLookupError::UserNotFound => f.write_str("User not found"),
            NavidromeLookupError::NotLinked => f.write_str("Navidrome not configured for user"),
            NavidromeLookupError::PasswordUnreadable => f.write_str(STORED_PASSWORD_UNREADABLE),
            NavidromeLookupError::NoServer => {
                f.write_str("NAVIDROME_URL environment variable not set")
            }
            NavidromeLookupError::Client(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(feature = "server")]
impl From<NavidromeLookupError> for String {
    fn from(e: NavidromeLookupError) -> String {
        e.to_string()
    }
}

#[cfg(feature = "server")]
impl NavidromeLookupError {
    /// This lookup error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        let not_linked = |message: String, action| {
            Failure::new(
                FailureCode::NavidromeNotLinked,
                stage,
                RetryAdvice::NeedsUser,
                message,
            )
            .with_user_action(action)
        };
        let message = message.into();
        match self {
            NavidromeLookupError::Database(e) => Failure::new(
                FailureCode::SettingsUnreadable,
                stage,
                RetryAdvice::SameSourceLater,
                message,
            )
            .with_cause(Service::Database, None, e.clone()),
            NavidromeLookupError::UserNotFound
            | NavidromeLookupError::NotLinked
            | NavidromeLookupError::PasswordUnreadable => {
                not_linked(message, soulbeet::navidrome::relink_navidrome())
            }
            NavidromeLookupError::NoServer => {
                not_linked(message, soulbeet::navidrome::configure_navidrome())
            }
            NavidromeLookupError::Client(e) => e.failure(stage, message),
        }
    }
}

#[cfg(feature = "server")]
pub async fn navidrome_client_for_user(
    user_id: &str,
) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
    // Check cache first
    if let Some(client) = NAVIDROME_CLIENTS.read().await.get(user_id) {
        return Ok(client.clone());
    }

    let user = crate::models::user::User::find_by_id(user_id)
        .await
        .map_err(NavidromeLookupError::Database)?
        .ok_or(NavidromeLookupError::UserNotFound)?;

    let password = stored_password_in(&crate::db::DB, crate::secret_key::current(), &user).await?;

    let url = crate::config::CONFIG
        .navidrome_url()
        .ok_or(NavidromeLookupError::NoServer)?;

    let client = NavidromeClientBuilder::new()
        .base_url(url)
        .username(&user.username)
        .password(&password)
        .build()
        .map_err(NavidromeLookupError::Client)?;

    let client = Arc::new(client);
    NAVIDROME_CLIENTS
        .write()
        .await
        .insert(user_id.to_string(), client.clone());
    Ok(client)
}

#[cfg(feature = "server")]
pub const STORED_PASSWORD_UNREADABLE: &str = "The stored Navidrome password cannot be decrypted, probably because SECRET_KEY changed. Enter it again under Connect Navidrome in Settings > Users.";

/// The user's stored Navidrome password. One that cannot be decrypted
/// (SECRET_KEY changed) marks the user to enter it again, once: a user
/// already marked is not marked or logged again.
#[cfg(feature = "server")]
pub(crate) async fn stored_password_in(
    db: &sqlx::SqlitePool,
    key: &crate::secret_key::SecretKey,
    user: &crate::models::user::User,
) -> Result<String, NavidromeLookupError> {
    let encrypted = user
        .navidrome_token
        .as_deref()
        .ok_or(NavidromeLookupError::NotLinked)?;
    if let Ok(password) = crate::crypto::decrypt(key, encrypted) {
        return Ok(password);
    }
    if user.navidrome_status != shared::system::NavidromeStatus::InvalidCredentials.as_str() {
        tracing::warn!(
            "Stored Navidrome password of user {} cannot be decrypted; they are asked to enter it again",
            user.id
        );
        if let Err(e) = mark_password_unreadable(db, &user.id, encrypted).await {
            tracing::warn!(
                "Could not mark user {} to enter their Navidrome password again: {e}",
                user.id
            );
        }
    }
    Err(NavidromeLookupError::PasswordUnreadable)
}

/// Stores again under the key in use every Navidrome password that only an
/// earlier key reads: one an install stored without `SECRET_KEY` (under the
/// built-in key) or under a `SECRET_KEY` too short to be used now. Those keys
/// sign nothing any more, so this keeps every user linked without keeping a
/// weakness: anyone who could read the database could already read these.
/// Run at startup, before anything reads a stored password.
#[cfg(feature = "server")]
pub async fn store_passwords_under_the_current_key() {
    let key = crate::secret_key::current();
    match passwords_under_current_key_in(&crate::db::DB, key, crate::secret_key::earlier()).await {
        Ok(0) => {}
        Ok(n) => {
            tracing::info!("Stored {n} Navidrome password(s) again under the secret key in use")
        }
        Err(e) => tracing::warn!(
            "Could not store Navidrome passwords again under the secret key in use: {e}"
        ),
    }
}

/// `store_passwords_under_the_current_key` on `db`: how many it stored again.
#[cfg(feature = "server")]
pub(crate) async fn passwords_under_current_key_in(
    db: &sqlx::SqlitePool,
    key: &crate::secret_key::SecretKey,
    earlier: &[crate::secret_key::SecretKey],
) -> Result<usize, String> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, navidrome_token FROM users WHERE navidrome_token IS NOT NULL")
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?;
    let mut stored = 0;
    for (id, token) in rows {
        if crate::crypto::decrypt(key, &token).is_ok() {
            continue;
        }
        let Some(password) = earlier
            .iter()
            .find_map(|old| crate::crypto::decrypt(old, &token).ok())
        else {
            continue;
        };
        let again = crate::crypto::encrypt(key, &password)?;
        // Only the token read: a sign-in meanwhile stored a newer one.
        sqlx::query("UPDATE users SET navidrome_token = ? WHERE id = ? AND navidrome_token = ?")
            .bind(&again)
            .bind(&id)
            .bind(&token)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        stored += 1;
    }
    Ok(stored)
}

/// Keeps the undecryptable token and asks the user to enter their Navidrome
/// password again.
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
    use crate::server_fns::download::test_support::{StubBackend, StubBackends};

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
    async fn a_password_stored_under_the_old_built_in_key_asks_for_a_new_sign_in_once() {
        use crate::models::user::User;
        use crate::secret_key::SecretKey;

        let db = test_db::migrated().await;
        let old = SecretKey::new("secret".into());
        let generated = crate::secret_key::resolve(
            None,
            tempfile::tempdir().unwrap().path(),
            "sqlite:soulbeet.db",
        )
        .unwrap()
        .key;
        let cipher = crate::crypto::encrypt(&old, "navidrome-pw").unwrap();
        sqlx::query(
            "UPDATE users SET navidrome_token = ?, navidrome_status = 'connected' WHERE id = ?",
        )
        .bind(&cipher)
        .bind(SEEDED_ADMIN_ID)
        .execute(&db)
        .await
        .unwrap();
        let user = || async {
            User::find_by_id_in(&db, SEEDED_ADMIN_ID)
                .await
                .unwrap()
                .unwrap()
        };

        assert_eq!(
            stored_password_in(&db, &old, &user().await).await.unwrap(),
            "navidrome-pw"
        );
        assert!(matches!(
            stored_password_in(&db, &generated, &user().await).await,
            Err(NavidromeLookupError::PasswordUnreadable)
        ));
        assert_eq!(user().await.navidrome_status, "invalid_credentials");

        // Already marked: the banner the user dismissed since stays dismissed.
        sqlx::query("UPDATE user_settings SET navidrome_banner_dismissed = 1 WHERE user_id = ?")
            .bind(SEEDED_ADMIN_ID)
            .execute(&db)
            .await
            .unwrap();
        assert!(stored_password_in(&db, &generated, &user().await)
            .await
            .is_err());
        let dismissed: i64 = sqlx::query_scalar(
            "SELECT navidrome_banner_dismissed FROM user_settings WHERE user_id = ?",
        )
        .bind(SEEDED_ADMIN_ID)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(dismissed, 1);
    }

    #[tokio::test]
    async fn passwords_only_an_earlier_key_reads_are_stored_again_under_the_current_one() {
        use crate::secret_key::SecretKey;

        let db = test_db::migrated().await;
        let current = SecretKey::new("c".repeat(64));
        let earlier = [
            SecretKey::new("change-me-in-production".into()),
            SecretKey::new("secret".into()),
        ];
        let add = |id: &'static str, token: Option<String>| {
            let db = db.clone();
            async move {
                sqlx::query("INSERT INTO users (id, username, password_hash, navidrome_token) VALUES (?, ?, 'x', ?)")
                    .bind(id)
                    .bind(id)
                    .bind(token)
                    .execute(&db)
                    .await
                    .unwrap();
            }
        };
        let current_token = crate::crypto::encrypt(&current, "pw-current").unwrap();
        add(
            "built-in",
            Some(crate::crypto::encrypt(&earlier[1], "pw-built-in").unwrap()),
        )
        .await;
        add(
            "short",
            Some(crate::crypto::encrypt(&earlier[0], "pw-short").unwrap()),
        )
        .await;
        add("current", Some(current_token.clone())).await;
        add(
            "other",
            Some(crate::crypto::encrypt(&SecretKey::new("o".repeat(40)), "pw").unwrap()),
        )
        .await;
        add("unlinked", None).await;

        assert_eq!(
            passwords_under_current_key_in(&db, &current, &earlier).await,
            Ok(2)
        );

        let token = |id: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>(
                    "SELECT navidrome_token FROM users WHERE id = ?",
                )
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        for (id, password) in [("built-in", "pw-built-in"), ("short", "pw-short")] {
            let stored = token(id).await.unwrap();
            assert_eq!(crate::crypto::decrypt(&current, &stored).unwrap(), password);
        }
        assert_eq!(token("current").await, Some(current_token));
        assert!(crate::crypto::decrypt(&current, &token("other").await.unwrap()).is_err());
        assert_eq!(token("unlinked").await, None);
        // Nothing is left to store again.
        assert_eq!(
            passwords_under_current_key_in(&db, &current, &earlier).await,
            Ok(0)
        );
    }

    #[tokio::test]
    async fn a_user_without_a_stored_token_is_not_linked() {
        use crate::models::user::User;

        let db = test_db::migrated().await;
        sqlx::query("UPDATE users SET navidrome_token = NULL WHERE id = ?")
            .bind(SEEDED_ADMIN_ID)
            .execute(&db)
            .await
            .unwrap();
        let user = User::find_by_id_in(&db, SEEDED_ADMIN_ID)
            .await
            .unwrap()
            .unwrap();
        let key = crate::secret_key::SecretKey::new("secret".into());

        assert!(matches!(
            stored_password_in(&db, &key, &user).await,
            Err(NavidromeLookupError::NotLinked)
        ));
    }

    #[test]
    fn a_navidrome_lookup_error_names_what_to_fix() {
        use shared::failure::UserActionCode;

        let cases = [
            (
                NavidromeLookupError::Database("database is locked".into()),
                "database is locked",
                FailureCode::SettingsUnreadable,
                None,
            ),
            (
                NavidromeLookupError::UserNotFound,
                "User not found",
                FailureCode::NavidromeNotLinked,
                Some(UserActionCode::RelinkNavidrome),
            ),
            (
                NavidromeLookupError::NotLinked,
                "Navidrome not configured for user",
                FailureCode::NavidromeNotLinked,
                Some(UserActionCode::RelinkNavidrome),
            ),
            (
                NavidromeLookupError::PasswordUnreadable,
                STORED_PASSWORD_UNREADABLE,
                FailureCode::NavidromeNotLinked,
                Some(UserActionCode::RelinkNavidrome),
            ),
            (
                NavidromeLookupError::NoServer,
                "NAVIDROME_URL environment variable not set",
                FailureCode::NavidromeNotLinked,
                Some(UserActionCode::ConfigureNavidrome),
            ),
            (
                NavidromeLookupError::Client(soulbeet::NavidromeError::CircuitOpen),
                "Navidrome appears unavailable (circuit breaker open)",
                FailureCode::NavidromeCircuitOpen,
                None,
            ),
        ];
        for (e, text, code, action) in cases {
            assert_eq!(e.to_string(), text);
            let f = e.failure(Stage::LibraryCheck, "m");
            assert_eq!(f.code, code, "{e:?}");
            assert_eq!(f.user_action.map(|a| a.code), action, "{e:?}");
        }
    }

    /// An unreadable password gives one instruction, and one that works for an
    /// account made through single sign-on, where signing in again stores
    /// nothing.
    #[test]
    fn an_unreadable_password_points_where_its_user_action_does() {
        let e = NavidromeLookupError::PasswordUnreadable;
        let text = e.to_string();
        let action = e.failure(Stage::LibraryCheck, text.clone()).user_action;
        assert_eq!(
            action.map(|a| a.what),
            Some("Enter your current Navidrome password under Connect Navidrome".to_string())
        );
        assert!(
            text.contains("under Connect Navidrome in Settings > Users"),
            "{text}"
        );
        assert!(!text.contains("Sign in again"), "{text}");
    }

    #[test]
    fn the_beets_libraries_sit_next_to_the_database() {
        assert_eq!(
            beets_libraries("sqlite:/data/soulbeet.db"),
            Ok(PathBuf::from("/data/beets-libraries"))
        );
        assert_eq!(
            beets_libraries("sqlite:soulbeet.db"),
            Ok(PathBuf::from("beets-libraries"))
        );
        assert!(beets_libraries("sqlite::memory:").is_err());
    }

    /// Each lookup error fails at the caller's stage: an unreadable settings
    /// database keeps sqlx's text, an unknown id is the caller's input.
    #[test]
    fn a_lookup_error_fails_at_the_callers_stage() {
        let locked = BackendLookupError::Settings(
            "error returned from database: (code: 5) database is locked".into(),
        );
        let unknown = BackendLookupError::Unknown("nicotine".into());
        for stage in [Stage::SourceSearch, Stage::Enqueue, Stage::Transfer] {
            let f = locked.failure(stage, "m");
            assert_eq!(
                (f.code, f.stage, f.retry),
                (
                    FailureCode::SettingsUnreadable,
                    stage,
                    RetryAdvice::SameSourceLater
                )
            );
            let cause = f.cause.unwrap();
            assert_eq!(
                (cause.service, cause.raw.as_str()),
                (
                    Service::Database,
                    "error returned from database: (code: 5) database is locked"
                )
            );
            let f = unknown.failure(stage, "Unknown download backend: nicotine");
            assert_eq!(
                (f.code, f.stage, f.retry),
                (FailureCode::InvalidInput, stage, RetryAdvice::NotRetryable)
            );
        }
    }

    #[test]
    fn a_backend_is_kept_only_under_its_own_id() {
        let stub: Arc<dyn DownloadBackend> = StubBackend::new(vec![]);
        let own = stub.id();
        assert_eq!(answering_to(own, stub.clone()).unwrap().id(), own);
        let Err(e) = answering_to(downloaders::SLSKD, stub) else {
            panic!("kept under another backend's id");
        };
        assert_eq!(
            e.to_string(),
            format!("Download backend slskd names itself {own}")
        );
        let f = e.failure(Stage::Enqueue, "m");
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::InvalidInput,
                Stage::Enqueue,
                RetryAdvice::NotRetryable
            )
        );
    }

    #[tokio::test]
    async fn a_backend_cache_asks_each_id_once() {
        let stub = StubBackend::new(vec![]);
        let backends = StubBackends::default().with("a", stub.clone());
        backends.fail("b", BackendLookupError::Unknown("b".into()));
        let mut cache = BackendCache::new(&backends);
        for _ in 0..2 {
            assert_eq!(cache.get("a").await.unwrap().id(), stub.id());
            let Err(e) = cache.get("b").await else {
                panic!("b was found");
            };
            assert_eq!(e.to_string(), "Unknown download backend: b");
        }
        assert_eq!(backends.asked(), ["a", "b"]);
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
