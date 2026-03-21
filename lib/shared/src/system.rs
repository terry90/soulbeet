use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum NavidromeStatus {
    /// Credentials verified against Navidrome, all features available.
    Connected,
    /// Navidrome rejected the credentials. Local auth succeeded but
    /// Navidrome features (discovery playlists, rating sync) won't work
    /// until the user logs in with matching credentials.
    InvalidCredentials,
    /// Navidrome was unreachable at login time. Existing token may still work.
    Offline,
    /// No Navidrome auth has been attempted yet (fresh account).
    #[default]
    Unknown,
}

impl NavidromeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::InvalidCredentials => "invalid_credentials",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

impl std::fmt::Display for NavidromeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for NavidromeStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "connected" => Ok(Self::Connected),
            "invalid_credentials" => Ok(Self::InvalidCredentials),
            "offline" => Ok(Self::Offline),
            _ => Ok(Self::Unknown),
        }
    }
}

impl From<&str> for NavidromeStatus {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or(Self::Unknown)
    }
}

impl From<String> for NavidromeStatus {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemHealth {
    pub downloader_online: bool,
    pub beets_ready: bool,
    pub navidrome_online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AvailableBackends {
    pub metadata: Vec<BackendInfo>,
    pub download: Vec<BackendInfo>,
    pub importer: Vec<BackendInfo>,
}
