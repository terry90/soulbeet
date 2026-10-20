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
    /// ReportRealPath is not enabled on the Soulbeet player in Navidrome.
    /// Path resolution and auto-delete features will not work correctly.
    MissingReportRealPath,
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
            Self::MissingReportRealPath => "missing_report_real_path",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected | Self::MissingReportRealPath)
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
            "missing_report_real_path" => Ok(Self::MissingReportRealPath),
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
    #[serde(default)]
    pub importer: ImporterHealth,
    pub navidrome_online: bool,
}

/// How well the importer can do its job; later variants are worse.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum HealthLevel {
    #[default]
    Checking,
    Ready,
    Degraded,
    Broken,
}

impl HealthLevel {
    pub fn chip(&self) -> &'static str {
        match self {
            Self::Checking => "CHECKING",
            Self::Ready => "READY",
            Self::Degraded => "WARN",
            Self::Broken => "BROKEN",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthIssue {
    pub severity: HealthLevel,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ImporterHealth {
    pub level: HealthLevel,
    pub issues: Vec<HealthIssue>,
    pub facts: Vec<(String, String)>,
}

impl ImporterHealth {
    /// The first issue that keeps imports from running.
    pub fn blocking_issue(&self) -> Option<&HealthIssue> {
        self.issues
            .iter()
            .find(|i| i.severity == HealthLevel::Broken)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importer_health_defaults_to_checking_and_finds_the_blocking_issue() {
        let h = ImporterHealth::default();
        assert_eq!(h.level, HealthLevel::Checking);
        assert_eq!(HealthLevel::Degraded.chip(), "WARN");
        let issue = |s, t: &str| HealthIssue {
            severity: s,
            title: t.into(),
            detail: String::new(),
        };
        let h = ImporterHealth {
            level: HealthLevel::Broken,
            issues: vec![
                issue(HealthLevel::Degraded, "w"),
                issue(HealthLevel::Broken, "b"),
            ],
            facts: vec![],
        };
        assert_eq!(h.blocking_issue().unwrap().title, "b");
    }

    #[test]
    fn system_health_without_importer_field_deserializes() {
        let v = serde_json::json!({"downloader_online":true,"navidrome_online":false});
        let h: SystemHealth = serde_json::from_value(v).unwrap();
        assert_eq!(h.importer.level, HealthLevel::Checking);
    }
}
