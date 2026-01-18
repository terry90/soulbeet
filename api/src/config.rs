//! Centralized configuration management.
//!
//! All environment variables are loaded and validated at startup through this module.
//! This prevents scattered `env::var()` calls and ensures early failure on missing config.

#[cfg(feature = "server")]
use std::path::PathBuf;

/// Application configuration loaded from environment variables.
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// SQLite database URL (default: "sqlite:soulbeet.db")
    pub database_url: String,
    /// JWT signing secret (default: "secret" - CHANGE IN PRODUCTION)
    pub secret_key: String,
    /// slskd API base URL (required)
    pub slskd_url: String,
    /// slskd API authentication key (required)
    pub slskd_api_key: String,
    /// Directory where slskd downloads files (default: "/downloads")
    pub slskd_download_path: PathBuf,
    /// Path to beets configuration file (default: "beets_config.yaml")
    pub beets_config: PathBuf,
    /// Enable album mode for beets import (groups tracks by folder)
    pub beets_album_mode: bool,
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
        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:soulbeet.db".to_string()),
            secret_key: std::env::var("SECRET_KEY").unwrap_or_else(|_| "secret".to_string()),
            slskd_url: std::env::var("SLSKD_URL").expect("Missing required SLSKD_URL env var"),
            slskd_api_key: std::env::var("SLSKD_API_KEY")
                .expect("Missing required SLSKD_API_KEY env var"),
            slskd_download_path: PathBuf::from(
                std::env::var("SLSKD_DOWNLOAD_PATH").unwrap_or_else(|_| "/downloads".to_string()),
            ),
            beets_config: PathBuf::from(
                std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string()),
            ),
            beets_album_mode: std::env::var("BEETS_ALBUM_MODE").is_ok(),
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
