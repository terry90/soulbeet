//! Centralized configuration management.
//!
//! All environment variables are loaded and validated at startup through this module.
//! This prevents scattered `env::var()` calls and ensures early failure on missing config.

#[cfg(feature = "server")]
use std::path::PathBuf;

#[cfg(feature = "server")]
const DEFAULT_SECRET_KEY: &str = "secret";

/// Parse a boolean from an environment variable.
///
/// Accepts: "true", "1", "yes" (case-insensitive) as true.
/// Accepts: "false", "0", "no" (case-insensitive) as false.
/// Returns the default for missing or invalid values.
#[cfg(feature = "server")]
fn parse_bool_env(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| match v.to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => {
                tracing::warn!(
                    "Invalid boolean value '{}' for {}, using default: {}",
                    v,
                    key,
                    default
                );
                default
            }
        })
        .unwrap_or(default)
}

/// Application configuration loaded from environment variables.
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// SQLite database URL (default: "sqlite:soulbeet.db")
    database_url: String,
    /// JWT signing secret (MUST be set in production)
    secret_key: String,
    /// slskd API base URL (required)
    slskd_url: String,
    /// slskd API authentication key (required)
    slskd_api_key: String,
    /// Directory where slskd downloads files (default: "/downloads")
    slskd_download_path: PathBuf,
    /// Path to beets configuration file (default: "beets_config.yaml")
    beets_config: PathBuf,
    /// Enable album mode for beets import (groups tracks by folder)
    beets_album_mode: bool,
    /// HTTP server port (default: 9765)
    pub port: u16,
    /// HTTP server bind address (default: "0.0.0.0")
    pub ip: String,
}

#[cfg(feature = "server")]
impl AppConfig {
    /// Load configuration from environment variables.
    ///
    /// # Panics
    /// Panics if required environment variables (SLSKD_URL, SLSKD_API_KEY) are missing.
    pub fn from_env() -> Self {
        let secret_key = std::env::var("SECRET_KEY").unwrap_or_else(|_| {
            tracing::error!(
                "SECRET_KEY environment variable is not set! \
                 Using insecure default. This is a security risk in production. \
                 Set SECRET_KEY to a random, secure value."
            );
            DEFAULT_SECRET_KEY.to_string()
        });

        if secret_key == DEFAULT_SECRET_KEY {
            tracing::error!(
                "SECRET_KEY is set to the default value '{}'. \
                 This is insecure for production use. \
                 Please set a unique, random SECRET_KEY.",
                DEFAULT_SECRET_KEY
            );
        }

        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:soulbeet.db".to_string()),
            secret_key,
            slskd_url: std::env::var("SLSKD_URL").expect("Missing required SLSKD_URL env var"),
            slskd_api_key: std::env::var("SLSKD_API_KEY")
                .expect("Missing required SLSKD_API_KEY env var"),
            slskd_download_path: PathBuf::from(
                std::env::var("SLSKD_DOWNLOAD_PATH").unwrap_or_else(|_| "/downloads".to_string()),
            ),
            beets_config: PathBuf::from(
                std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string()),
            ),
            beets_album_mode: parse_bool_env("BEETS_ALBUM_MODE", false),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(9765),
            ip: std::env::var("IP").unwrap_or_else(|_| "0.0.0.0".to_string()),
        }
    }

    /// Get the database URL.
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Get the JWT secret key.
    pub fn secret_key(&self) -> &str {
        &self.secret_key
    }

    /// Get the slskd base URL.
    pub fn slskd_url(&self) -> &str {
        &self.slskd_url
    }

    /// Get the slskd API key.
    pub fn slskd_api_key(&self) -> &str {
        &self.slskd_api_key
    }

    /// Get the slskd download path.
    pub fn slskd_download_path(&self) -> &PathBuf {
        &self.slskd_download_path
    }

    /// Get the beets config path.
    pub fn beets_config(&self) -> &PathBuf {
        &self.beets_config
    }

    /// Check if album mode is enabled.
    pub fn is_album_mode(&self) -> bool {
        self.beets_album_mode
    }
}

#[cfg(feature = "server")]
use std::sync::LazyLock;

/// Global application configuration singleton.
/// Loaded once at startup from environment variables.
#[cfg(feature = "server")]
pub static CONFIG: LazyLock<AppConfig> = LazyLock::new(AppConfig::from_env);
