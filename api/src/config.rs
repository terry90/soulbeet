//! Centralized configuration management.
//!
//! All environment variables are loaded and validated at startup through this module.
//! This prevents scattered `env::var()` calls and ensures early failure on missing config.

#[cfg(feature = "server")]
use std::path::{Path, PathBuf};

#[cfg(feature = "server")]
use crate::auth::RegistrationMode;
#[cfg(feature = "server")]
use crate::base_url::{BaseUrl, BaseUrlError};

#[cfg(feature = "server")]
const DEFAULT_SECRET_KEY: &str = "secret";

/// Why the configured `SECRET_KEY` is not safe to expose to the internet.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKeyWeakness {
    NotSet,
    Default,
    TooShort,
}

#[cfg(feature = "server")]
impl SecretKeyWeakness {
    pub fn message(&self) -> String {
        let problem = match self {
            Self::NotSet => "SECRET_KEY is not set.",
            Self::Default => "SECRET_KEY is the built-in default.",
            Self::TooShort => "SECRET_KEY is shorter than 32 bytes.",
        };
        format!(
            "{problem} Set it to a random value of at least 32 bytes, for example the output of \
             `openssl rand -hex 32`. Changing SECRET_KEY makes the stored Navidrome passwords \
             unreadable: each user is asked to sign in again once."
        )
    }
}

#[cfg(feature = "server")]
pub(crate) fn secret_key_weakness(value: Option<&str>) -> Option<SecretKeyWeakness> {
    match value {
        None => Some(SecretKeyWeakness::NotSet),
        Some(DEFAULT_SECRET_KEY) => Some(SecretKeyWeakness::Default),
        Some(v) if v.len() < 32 => Some(SecretKeyWeakness::TooShort),
        Some(_) => None,
    }
}

/// Reads `REGISTRATION`. Unset follows whether Navidrome is configured; an
/// unknown value falls to the closed mode.
#[cfg(feature = "server")]
pub(crate) fn parse_registration(
    value: Option<&str>,
    navidrome_configured: bool,
) -> RegistrationMode {
    match value.map(str::to_lowercase).as_deref() {
        None if navidrome_configured => RegistrationMode::Navidrome,
        None => RegistrationMode::Admin,
        Some("navidrome") => RegistrationMode::Navidrome,
        Some("admin") => RegistrationMode::Admin,
        Some("open") => RegistrationMode::Open,
        Some(other) => {
            tracing::warn!(
                "Invalid REGISTRATION value '{}' (expected navidrome, admin or open), using admin",
                other
            );
            RegistrationMode::Admin
        }
    }
}

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
    /// Why `secret_key` is weak, computed before the default is substituted
    secret_key_weakness: Option<SecretKeyWeakness>,
    /// Directory where downloads are saved (default: "/downloads")
    download_path: PathBuf,
    /// Path to beets configuration file (default: "beets_config.yaml")
    beets_config: PathBuf,
    /// Enable album mode for beets import (groups tracks by folder)
    beets_album_mode: bool,
    /// The `beet` executable, resolved on PATH once
    beet_bin: PathBuf,
    /// The Python that runs `beet` (BEETS_PYTHON, else beet's shebang)
    beets_python: Option<PathBuf>,
    /// `PUBLIC_URL`, parsed once; `None` when unset or empty
    public_url: Option<Result<BaseUrl, BaseUrlError>>,
    /// `NAVIDROME_URL`; `None` when unset or empty
    navidrome_url: Option<String>,
    /// Who can create accounts (`REGISTRATION`)
    registration: RegistrationMode,
    /// HTTP server port (default: 9765)
    pub port: u16,
    /// HTTP server bind address (default: "0.0.0.0")
    pub ip: String,
}

#[cfg(feature = "server")]
impl AppConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let secret_key_env = std::env::var("SECRET_KEY").ok();
        let secret_key_weakness = secret_key_weakness(secret_key_env.as_deref());
        if let Some(w) = secret_key_weakness {
            tracing::error!("{}", w.message());
        }
        let secret_key = secret_key_env.unwrap_or_else(|| DEFAULT_SECRET_KEY.to_string());

        // Left as a bare name when it is not on PATH, so the spawn error names it.
        let beet_bin =
            soulbeet::beets::resolve_on_path("beet").unwrap_or_else(|| PathBuf::from("beet"));
        let beets_python = std::env::var_os("BEETS_PYTHON")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| soulbeet::beets::python_from_shebang(&beet_bin));

        let navidrome_url = std::env::var("NAVIDROME_URL")
            .ok()
            .filter(|v| !v.is_empty());
        let registration = parse_registration(
            std::env::var("REGISTRATION")
                .ok()
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty()),
            navidrome_url.is_some(),
        );

        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:soulbeet.db".to_string()),
            secret_key,
            secret_key_weakness,
            download_path: PathBuf::from(
                std::env::var("DOWNLOAD_PATH").unwrap_or_else(|_| "/downloads".to_string()),
            ),
            beets_config: PathBuf::from(
                std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string()),
            ),
            beets_album_mode: parse_bool_env("BEETS_ALBUM_MODE", false),
            beet_bin,
            beets_python,
            public_url: std::env::var("PUBLIC_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(|v| BaseUrl::parse(&v)),
            navidrome_url,
            registration,
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

    /// Why the secret key is weak, if it is.
    pub fn secret_key_weakness(&self) -> Option<SecretKeyWeakness> {
        self.secret_key_weakness
    }

    /// Get the download path.
    pub fn download_path(&self) -> &PathBuf {
        &self.download_path
    }

    /// Get the beets config path.
    pub fn beets_config(&self) -> &PathBuf {
        &self.beets_config
    }

    /// The `beet` executable.
    pub fn beet_bin(&self) -> &Path {
        &self.beet_bin
    }

    /// The Python that runs `beet`, when it is known.
    pub fn beets_python(&self) -> Option<&Path> {
        self.beets_python.as_deref()
    }

    /// The pinned public address, when `PUBLIC_URL` is set.
    pub fn public_url(&self) -> Option<&Result<BaseUrl, BaseUrlError>> {
        self.public_url.as_ref()
    }

    /// The Navidrome server, when `NAVIDROME_URL` is set.
    pub fn navidrome_url(&self) -> Option<&str> {
        self.navidrome_url.as_deref()
    }

    /// Who can create accounts.
    pub fn registration(&self) -> RegistrationMode {
        self.registration
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

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn secret_key_strength() {
        assert_eq!(secret_key_weakness(None), Some(SecretKeyWeakness::NotSet));
        assert_eq!(
            secret_key_weakness(Some("secret")),
            Some(SecretKeyWeakness::Default)
        );
        assert_eq!(
            secret_key_weakness(Some(&"a".repeat(31))),
            Some(SecretKeyWeakness::TooShort)
        );
        assert_eq!(secret_key_weakness(Some(&"a".repeat(32))), None);
    }

    #[test]
    fn registration_defaults_follow_navidrome() {
        assert_eq!(parse_registration(None, true), RegistrationMode::Navidrome);
        assert_eq!(parse_registration(None, false), RegistrationMode::Admin);
        assert_eq!(
            parse_registration(Some("OPEN"), true),
            RegistrationMode::Open
        );
        assert_eq!(
            parse_registration(Some("bogus"), false),
            RegistrationMode::Admin
        );
    }
}
