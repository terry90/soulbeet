use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatedSong {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_id: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<u32>,
    pub path: Option<String>,
    pub user_rating: Option<u8>,
    pub average_rating: Option<f64>,
    pub play_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletionReview {
    pub id: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: Option<String>,
    pub rating: Option<u8>,
    pub reason: DeletionReason,
    pub created_at: String,
    /// What started the deletion: "ui", "auto" or "mcp" (a connected app).
    #[serde(default)]
    pub origin: String,
    /// The connected app's host, for an app's deletion.
    #[serde(default)]
    pub client: Option<String>,
}

/// Deletions per user in any 24 hours through connected apps (design 13, D4).
pub const DAILY_APP_DELETIONS: u32 = 100;

/// Why a file was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionReason {
    /// Its listener rated it down.
    Disliked,
    /// A new copy of it was imported.
    Replaced,
    /// Its listener asked to have it removed.
    Cleanup,
}

impl DeletionReason {
    /// Every reason's name, in variant order; as_str and parse read it.
    pub const NAMES: &'static [&'static str] = &["disliked", "replaced", "cleanup"];

    const ALL: [DeletionReason; 3] = [
        DeletionReason::Disliked,
        DeletionReason::Replaced,
        DeletionReason::Cleanup,
    ];

    pub fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    /// The reason named `text`, None for a name NAMES does not hold.
    pub fn parse(text: &str) -> Option<DeletionReason> {
        let at = Self::NAMES.iter().position(|name| *name == text)?;
        Some(Self::ALL[at])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TopPlayedTrack {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub play_count: u64,
    pub rating: Option<u8>,
    pub duration: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LibraryStats {
    pub total_tracks: u32,
    pub rated_tracks: u32,
    pub unrated_tracks: u32,
    pub average_rating: f64,
    pub rating_distribution: [u32; 5],
    pub total_albums: u32,
    pub total_artists: u32,
    pub genres: Vec<(String, u32)>,
    #[serde(default)]
    pub total_plays: u64,
    #[serde(default)]
    pub total_duration_secs: u64,
    #[serde(default)]
    pub top_played_artists: Vec<(String, u64)>,
    #[serde(default)]
    pub top_played_tracks: Vec<TopPlayedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryTrack {
    pub id: String,
    pub song_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub folder_id: String,
    pub profile: String,
    pub rating: Option<u8>,
    pub status: DiscoveryStatus,
    pub created_at: String,
    #[serde(default)]
    pub exclude_from_core_taste: bool,
    #[serde(default)]
    pub reason: Option<Box<crate::recommendation::CandidateReason>>,
    #[serde(default)]
    pub culture_bucket: Option<String>,
    #[serde(default)]
    pub pack_slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DiscoveryStatus {
    Pending,
    Promoting,
    Promoted,
    /// Claimed for removal: its staged file is being deleted.
    Removing,
    Removed,
}

impl std::fmt::Display for DiscoveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryStatus::Pending => write!(f, "Pending"),
            DiscoveryStatus::Promoting => write!(f, "Promoting"),
            DiscoveryStatus::Promoted => write!(f, "Promoted"),
            DiscoveryStatus::Removing => write!(f, "Removing"),
            DiscoveryStatus::Removed => write!(f, "Removed"),
        }
    }
}

impl std::str::FromStr for DiscoveryStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Pending" => Ok(DiscoveryStatus::Pending),
            "Promoting" => Ok(DiscoveryStatus::Promoting),
            "Promoted" => Ok(DiscoveryStatus::Promoted),
            "Removing" => Ok(DiscoveryStatus::Removing),
            "Removed" => Ok(DiscoveryStatus::Removed),
            _ => Err(format!("Unknown discovery status: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub folder_id: Option<String>,
    pub folder_name: Option<String>,
    pub track_counts: std::collections::HashMap<String, u32>,
    pub lifetime_days: std::collections::HashMap<String, u32>,
    pub profiles: String,
    pub playlist_names: std::collections::HashMap<String, String>,
    pub last_generated_at: Option<String>,
    #[serde(default)]
    pub last_run: Option<LastRun>,
}

/// What the user's latest recommendation run found: whether there was listening data, the
/// sources it used and the ones that failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub created_at: String,
    pub state: crate::recommendation::EngineState,
    pub sources: Vec<String>,
    pub source_errors: Vec<crate::recommendation::SourceError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GenerationResult {
    pub total_imported: u32,
    pub profiles: Vec<ProfileGenerationStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProfileGenerationStats {
    pub profile: String,
    pub target: u32,
    pub attempts: u32,
    pub candidates_tried: u32,
    pub candidates_skipped_seen: u32,
    pub search_hits: u32,
    pub search_misses: u32,
    pub search_errors: u32,
    pub downloads_queued: u32,
    pub downloads_completed: u32,
    pub downloads_failed: u32,
    pub downloads_timed_out: u32,
    pub imports_succeeded: u32,
    pub imports_skipped: u32,
    pub imports_failed: u32,
    pub imports_file_missing: u32,
}

/// A Soulful account whose Navidrome ratings the delete guard could not read, so deletions
/// are refused until a read succeeds. Shown to admins only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnreadableListener {
    pub username: String,
    /// What the last failed read gave.
    pub error: String,
    /// When reads of this account started failing (SQLite `datetime('now')`, UTC).
    pub since: String,
}

/// A track auto-delete kept at the user's last rating sync, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockedDeletion {
    pub title: String,
    pub artist: String,
    /// The guard's refusal.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SyncResult {
    pub deleted_tracks: u32,
    pub promoted_tracks: u32,
    pub removed_tracks: u32,
    pub total_songs_scanned: u32,
    /// The 1-star tracks auto-delete kept.
    #[serde(default)]
    pub kept_tracks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum GenerationStatus {
    #[default]
    Idle,
    Running,
    Complete,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum ProfilePhase {
    #[default]
    Waiting,
    PullingCandidates,
    GeneratingRecommendations,
    SearchingSoulseek,
    Downloading,
    Importing,
    Done,
    Skipped,
}

impl std::fmt::Display for ProfilePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfilePhase::Waiting => write!(f, "Waiting"),
            ProfilePhase::PullingCandidates => write!(f, "Pulling candidates"),
            ProfilePhase::GeneratingRecommendations => write!(f, "Generating recs"),
            ProfilePhase::SearchingSoulseek => write!(f, "Searching Soulseek"),
            ProfilePhase::Downloading => write!(f, "Downloading"),
            ProfilePhase::Importing => write!(f, "Importing"),
            ProfilePhase::Done => write!(f, "Done"),
            ProfilePhase::Skipped => write!(f, "Skipped"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProfileProgress {
    pub profile: String,
    pub phase: ProfilePhase,
    pub current: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DiscoveryProgress {
    pub status: GenerationStatus,
    pub profiles: Vec<ProfileProgress>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub result: Option<GenerationResult>,
    /// Why the run failed or could not go on, when its status is `Error`.
    #[serde(default)]
    pub failure: Option<crate::failure::Failure>,
}

impl DiscoveryProgress {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            GenerationStatus::Complete | GenerationStatus::Error
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_reads_back_from_its_name() {
        for name in DeletionReason::NAMES {
            let reason = DeletionReason::parse(name).unwrap();
            assert_eq!(reason.as_str(), *name);
            // The wire name is the same.
            assert_eq!(serde_json::to_value(reason).unwrap(), *name);
        }
        assert_eq!(DeletionReason::parse("because"), None);
    }

    #[test]
    fn progress_from_an_older_server_reads_without_a_failure() {
        let p: DiscoveryProgress = serde_json::from_value(serde_json::json!({
            "status": "Error", "profiles": [], "started_at": null, "completed_at": null,
            "result": null, "error": "Discovery is not enabled" }))
        .unwrap();
        assert_eq!(p.failure, None);
    }
}
