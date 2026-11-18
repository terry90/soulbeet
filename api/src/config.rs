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
use soulbeet::ImportMode;

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

/// Reads `MCP`: `on` or `off` in any case. Unset is on; an unknown value is
/// off, as an unknown `REGISTRATION` closes registration. None when on,
/// else why it is off, as the value is set.
#[cfg(feature = "server")]
pub(crate) fn parse_mcp_switch(value: Option<&str>) -> Option<String> {
    match value.map(str::to_lowercase).as_deref() {
        None | Some("on") => None,
        Some("off") => Some("MCP=off is set".to_string()),
        Some(_) => {
            let value = value.unwrap_or_default();
            tracing::warn!(
                "Invalid MCP value '{}' (expected on or off), using off",
                value
            );
            Some(format!(
                "MCP is set to '{value}', which is neither on nor off"
            ))
        }
    }
}

/// Why `PUBLIC_URL` stops every sign-in, when it is set and cannot be used.
#[cfg(feature = "server")]
pub(crate) fn public_url_problem(
    public_url: Option<&Result<BaseUrl, BaseUrlError>>,
) -> Option<String> {
    match public_url {
        Some(Err(e)) => Some(format!(
            "PUBLIC_URL is not usable: {e}. It must look like https://host or \
             https://host:port. Nobody can sign in until it is fixed or removed."
        )),
        None | Some(Ok(_)) => None,
    }
}

/// Parse a boolean: "true", "1", "yes" and "false", "0", "no", any case.
#[cfg(feature = "server")]
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a boolean from an environment variable.
///
/// Returns the default for missing or invalid values (see [`parse_bool`]).
#[cfg(feature = "server")]
fn parse_bool_env(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| {
            parse_bool(&v).unwrap_or_else(|| {
                tracing::warn!(
                    "Invalid boolean value '{}' for {}, using default: {}",
                    v,
                    key,
                    default
                );
                default
            })
        })
        .unwrap_or(default)
}

/// Sign-in through an OpenID Connect provider, set by the `OIDC_*` variables.
#[cfg(feature = "server")]
#[derive(Clone)]
pub struct OidcConfig {
    /// Issuer URL; its discovery document is at `/.well-known/openid-configuration`
    pub issuer_url: String,
    pub client_id: String,
    /// Unset for a public client, which relies on PKCE alone
    pub client_secret: Option<String>,
    /// This instance's callback, `<public url>/api/auth/oidc/callback`;
    /// derived from `PUBLIC_URL` when unset
    pub redirect_url: String,
    /// Space separated, always with `openid`
    pub scopes: String,
    /// ID token claim used as the Soulbeet username
    pub username_claim: String,
    /// Shown on the login button
    pub provider_name: String,
    /// Create an account on first sign-in for a username that has none;
    /// unset, it does unless `REGISTRATION=admin`
    pub auto_register: bool,
}

#[cfg(feature = "server")]
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "***"))
            .field("redirect_url", &self.redirect_url)
            .field("scopes", &self.scopes)
            .field("username_claim", &self.username_claim)
            .field("provider_name", &self.provider_name)
            .field("auto_register", &self.auto_register)
            .finish()
    }
}

#[cfg(feature = "server")]
impl OidcConfig {
    /// Read the `OIDC_*` variables through `get`. `Ok(None)` when
    /// `OIDC_ISSUER_URL` is unset, an error naming what is missing or wrong
    /// otherwise. `public_url` (a usable `PUBLIC_URL`) gives the default
    /// callback, `registration` the default of `OIDC_AUTO_REGISTER`.
    fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        public_url: Option<&BaseUrl>,
        registration: RegistrationMode,
    ) -> Result<Option<Self>, String> {
        let get = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let Some(issuer_url) = get("OIDC_ISSUER_URL") else {
            return Ok(None);
        };
        let client_id = get("OIDC_CLIENT_ID").ok_or("OIDC_CLIENT_ID is not set")?;
        let redirect_url = get("OIDC_REDIRECT_URL")
            .or_else(|| public_url.map(|base| format!("{base}{}", crate::auth::OIDC_CALLBACK_PATH)))
            .ok_or(
                "Neither OIDC_REDIRECT_URL nor PUBLIC_URL is set \
                 (e.g. OIDC_REDIRECT_URL=https://soulbeet.example.com/api/auth/oidc/callback)",
            )?;

        let parse_http_url = |key: &str, value: &str| {
            let parsed =
                url::Url::parse(value).map_err(|e| format!("{key} is not a valid URL: {e}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("{key} must be an http(s) URL"));
            }
            Ok(parsed)
        };
        parse_http_url("OIDC_ISSUER_URL", &issuer_url)?;
        // The provider must send the browser back to the callback route, or
        // the login cookie (scoped to that path) never reaches it.
        let callback = parse_http_url("OIDC_REDIRECT_URL", &redirect_url)?;
        if callback.path() != crate::auth::OIDC_CALLBACK_PATH {
            return Err(format!(
                "OIDC_REDIRECT_URL must end with {} (e.g. https://soulbeet.example.com{})",
                crate::auth::OIDC_CALLBACK_PATH,
                crate::auth::OIDC_CALLBACK_PATH
            ));
        }

        let mut scopes: Vec<String> = get("OIDC_SCOPES")
            .unwrap_or_else(|| "openid profile email".to_string())
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !scopes.iter().any(|s| s == "openid") {
            scopes.insert(0, "openid".to_string());
        }

        // The provider decides who may sign in; an admin-only instance
        // creates no accounts unless told to.
        let auto_register = match get("OIDC_AUTO_REGISTER") {
            None => registration != RegistrationMode::Admin,
            Some(v) => parse_bool(&v)
                .ok_or_else(|| format!("OIDC_AUTO_REGISTER has an invalid value '{v}'"))?,
        };

        Ok(Some(Self {
            issuer_url,
            client_id,
            client_secret: get("OIDC_CLIENT_SECRET"),
            redirect_url,
            scopes: scopes.join(" "),
            username_claim: get("OIDC_USERNAME_CLAIM")
                .unwrap_or_else(|| "preferred_username".to_string()),
            provider_name: get("OIDC_PROVIDER_NAME").unwrap_or_else(|| "SSO".to_string()),
            auto_register,
        }))
    }
}

/// Application configuration loaded from environment variables.
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// SQLite database URL (default: "sqlite:soulbeet.db")
    database_url: String,
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
    /// `NAVIDROME_MUSIC_PATH`; `None` when unset or empty
    navidrome_music_path: Option<PathBuf>,
    /// Who can create accounts (`REGISTRATION`)
    registration: RegistrationMode,
    /// Why `MCP` switches MCP access off; None when it is on
    mcp_off: Option<String>,
    /// HTTP server port (default: 9765)
    pub port: u16,
    /// HTTP server bind address (default: "0.0.0.0")
    pub ip: String,
    /// OpenID Connect sign-in, when configured
    oidc: Option<OidcConfig>,
}

#[cfg(feature = "server")]
impl AppConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
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

        let public_url = std::env::var("PUBLIC_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| BaseUrl::parse(&v));
        if let Some(problem) = public_url_problem(public_url.as_ref()) {
            tracing::error!("{problem}");
        }

        let oidc = OidcConfig::from_lookup(
            |key| std::env::var(key).ok(),
            public_url.as_ref().and_then(|url| url.as_ref().ok()),
            registration,
        )
        .unwrap_or_else(|e| {
            tracing::error!("OIDC sign-in is disabled: {e}");
            None
        });

        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:soulbeet.db".to_string()),
            download_path: PathBuf::from(
                std::env::var("DOWNLOAD_PATH").unwrap_or_else(|_| "/downloads".to_string()),
            ),
            beets_config: PathBuf::from(
                std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string()),
            ),
            beets_album_mode: parse_bool_env("BEETS_ALBUM_MODE", false),
            beet_bin,
            beets_python,
            public_url,
            navidrome_url,
            navidrome_music_path: std::env::var_os("NAVIDROME_MUSIC_PATH")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            registration,
            mcp_off: parse_mcp_switch(
                std::env::var("MCP")
                    .ok()
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty()),
            ),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(9765),
            ip: std::env::var("IP").unwrap_or_else(|_| "0.0.0.0".to_string()),
            oidc,
        }
    }

    /// OpenID Connect sign-in settings, when configured.
    pub fn oidc(&self) -> Option<&OidcConfig> {
        self.oidc.as_ref()
    }

    /// Get the database URL.
    pub fn database_url(&self) -> &str {
        &self.database_url
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

    /// Why `PUBLIC_URL` stops every sign-in, if it does.
    pub fn public_url_problem(&self) -> Option<String> {
        public_url_problem(self.public_url.as_ref())
    }

    /// The Navidrome server, when `NAVIDROME_URL` is set.
    pub fn navidrome_url(&self) -> Option<&str> {
        self.navidrome_url.as_deref()
    }

    /// NAVIDROME_MUSIC_PATH: where the root of Navidrome's default library is on this machine.
    pub fn navidrome_music_path(&self) -> Option<&Path> {
        self.navidrome_music_path.as_deref()
    }

    /// Why NAVIDROME_MUSIC_PATH cannot be used, when it is set and no folder here.
    pub fn navidrome_music_path_problem(&self) -> Option<String> {
        navidrome_music_path_problem(self.navidrome_music_path())
    }

    /// Who can create accounts.
    pub fn registration(&self) -> RegistrationMode {
        self.registration
    }

    /// Why `MCP` switches MCP access off; None when it is on.
    pub fn mcp_off(&self) -> Option<&str> {
        self.mcp_off.as_deref()
    }

    /// The mode every batch of this run imports in.
    pub fn import_mode(&self) -> ImportMode {
        match self.beets_album_mode {
            true => ImportMode::Album,
            false => ImportMode::Singleton,
        }
    }
}

/// Why `path`, the NAVIDROME_MUSIC_PATH set, cannot be used: it is no folder here. Earlier
/// versions read it as Navidrome's own path of its library, which is often not one here.
#[cfg(feature = "server")]
fn navidrome_music_path_problem(path: Option<&Path>) -> Option<String> {
    let path = path.filter(|p| !p.is_dir())?;
    Some(format!(
        "NAVIDROME_MUSIC_PATH is {}, which is not a folder Soulbeet can see. It names where \
         Soulbeet sees the root of Navidrome's default library (earlier versions read it as \
         Navidrome's own path). Until it names that folder or is removed, no song of that \
         library is deleted or replaced: auto-delete keeps every one.",
        path.display()
    ))
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
    fn an_unusable_public_url_is_reported() {
        let missing_scheme = BaseUrl::parse("music.example.com");
        let problem = public_url_problem(Some(&missing_scheme)).unwrap();
        assert!(
            problem
                .starts_with("PUBLIC_URL is not usable: 'music.example.com' is not a valid host."),
            "{problem}"
        );
        assert!(problem.ends_with("Nobody can sign in until it is fixed or removed."));
        assert_eq!(public_url_problem(None), None);
        let good = BaseUrl::parse("https://music.example.com");
        assert_eq!(public_url_problem(Some(&good)), None);
    }

    #[test]
    fn a_navidrome_music_path_that_is_no_folder_here_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(navidrome_music_path_problem(None), None);
        assert_eq!(navidrome_music_path_problem(Some(dir.path())), None);
        let missing = dir.path().join("media/music");
        let problem = navidrome_music_path_problem(Some(&missing)).unwrap();
        assert!(
            problem.starts_with(&format!(
                "NAVIDROME_MUSIC_PATH is {}, which is not a folder Soulbeet can see.",
                missing.display()
            )),
            "{problem}"
        );
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        assert!(navidrome_music_path_problem(Some(&file)).is_some());
    }

    #[test]
    fn mcp_is_on_unless_switched_off() {
        for (value, want) in [
            (None, None),
            (Some("on"), None),
            (Some("ON"), None),
            (Some("off"), Some("MCP=off is set")),
            (Some("Off"), Some("MCP=off is set")),
            (
                Some("Disabled"),
                Some("MCP is set to 'Disabled', which is neither on nor off"),
            ),
        ] {
            assert_eq!(parse_mcp_switch(value).as_deref(), want, "{value:?}");
        }
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

#[cfg(all(test, feature = "server"))]
mod oidc_tests {
    use super::{OidcConfig, RegistrationMode};
    use crate::base_url::BaseUrl;
    use std::collections::HashMap;

    fn parse_with(
        vars: &[(&str, &str)],
        public_url: Option<&str>,
        registration: RegistrationMode,
    ) -> Result<Option<OidcConfig>, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let public_url = public_url.map(|u| BaseUrl::parse(u).unwrap());
        OidcConfig::from_lookup(
            |key| vars.get(key).cloned(),
            public_url.as_ref(),
            registration,
        )
    }

    fn parse(vars: &[(&str, &str)]) -> Result<Option<OidcConfig>, String> {
        parse_with(vars, None, RegistrationMode::Navidrome)
    }

    const REQUIRED: [(&str, &str); 3] = [
        ("OIDC_ISSUER_URL", "https://id.example.com"),
        ("OIDC_CLIENT_ID", "soulbeet"),
        (
            "OIDC_REDIRECT_URL",
            "https://soulbeet.example.com/api/auth/oidc/callback",
        ),
    ];

    #[test]
    fn disabled_without_issuer() {
        assert!(parse(&[]).unwrap().is_none());
        assert!(parse(&[("OIDC_ISSUER_URL", "  ")]).unwrap().is_none());
    }

    #[test]
    fn defaults() {
        let config = parse(&REQUIRED).unwrap().unwrap();
        assert_eq!(config.client_secret, None);
        assert_eq!(config.scopes, "openid profile email");
        assert_eq!(config.username_claim, "preferred_username");
        assert_eq!(config.provider_name, "SSO");
        assert!(config.auto_register);
    }

    #[test]
    fn missing_client_id_or_callback_is_an_error() {
        let err = parse(&REQUIRED[..1]).unwrap_err();
        assert!(err.contains("OIDC_CLIENT_ID"), "{err}");
        let err = parse(&REQUIRED[..2]).unwrap_err();
        assert!(err.contains("OIDC_REDIRECT_URL"), "{err}");
    }

    #[test]
    fn the_callback_defaults_to_the_public_url() {
        let config = parse_with(
            &REQUIRED[..2],
            Some("https://music.example.com"),
            RegistrationMode::Navidrome,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.redirect_url,
            "https://music.example.com/api/auth/oidc/callback"
        );
        // An explicit callback wins.
        let config = parse_with(
            &REQUIRED,
            Some("https://music.example.com"),
            RegistrationMode::Navidrome,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.redirect_url, REQUIRED[2].1);
    }

    #[test]
    fn invalid_urls_are_errors() {
        let mut vars = REQUIRED.to_vec();
        vars[0].1 = "id.example.com";
        assert!(parse(&vars).unwrap_err().contains("OIDC_ISSUER_URL"));
        vars[0].1 = "ftp://id.example.com";
        assert!(parse(&vars).unwrap_err().contains("OIDC_ISSUER_URL"));
    }

    #[test]
    fn redirect_url_must_be_the_callback_route() {
        let mut vars = REQUIRED.to_vec();
        vars[2].1 = "https://soulbeet.example.com/oidc/callback";
        assert!(parse(&vars)
            .unwrap_err()
            .contains("/api/auth/oidc/callback"));
        vars[2].1 = "https://soulbeet.example.com";
        assert!(parse(&vars).is_err());
    }

    #[test]
    fn openid_scope_is_always_requested() {
        let mut vars = REQUIRED.to_vec();
        vars.push(("OIDC_SCOPES", "profile groups"));
        let config = parse(&vars).unwrap().unwrap();
        assert_eq!(config.scopes, "openid profile groups");
    }

    #[test]
    fn auto_register_follows_registration_unless_set() {
        let auto = |vars: &[(&str, &str)], mode| {
            parse_with(vars, None, mode).unwrap().unwrap().auto_register
        };
        assert!(auto(&REQUIRED, RegistrationMode::Navidrome));
        assert!(auto(&REQUIRED, RegistrationMode::Open));
        assert!(!auto(&REQUIRED, RegistrationMode::Admin));
        let mut vars = REQUIRED.to_vec();
        vars.push(("OIDC_AUTO_REGISTER", "yes"));
        assert!(auto(&vars, RegistrationMode::Admin));
    }

    #[test]
    fn overrides() {
        let mut vars = REQUIRED.to_vec();
        vars.extend([
            ("OIDC_CLIENT_SECRET", "s3cret"),
            ("OIDC_USERNAME_CLAIM", "email"),
            ("OIDC_PROVIDER_NAME", "Pocket ID"),
            ("OIDC_AUTO_REGISTER", "false"),
        ]);
        let config = parse(&vars).unwrap().unwrap();
        assert_eq!(config.client_secret.as_deref(), Some("s3cret"));
        assert_eq!(config.username_claim, "email");
        assert_eq!(config.provider_name, "Pocket ID");
        assert!(!config.auto_register);
        assert!(!format!("{config:?}").contains("s3cret"));
    }

    #[test]
    fn invalid_auto_register_is_an_error() {
        let mut vars = REQUIRED.to_vec();
        vars.push(("OIDC_AUTO_REGISTER", "maybe"));
        assert!(parse(&vars).unwrap_err().contains("OIDC_AUTO_REGISTER"));
    }
}
