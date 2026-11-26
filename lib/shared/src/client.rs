use serde::{Deserialize, Serialize};

use crate::navidrome::DeletionReason;

/// Initial session payload returned by `GET /api/client/bootstrap` for Web, Desktop, and Android
/// clients. Carries authenticated OpenSubsonic token coordinates (never the plaintext password),
/// user curation settings, active discovery context, and server capabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientBootstrap {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
    pub navidrome: Option<NavidromeSessionInfo>,
    pub capabilities: ServerCapabilities,
    pub promote_threshold: u8,
    pub auto_delete_enabled: bool,
    pub discovery_enabled: bool,
    #[serde(default)]
    pub active_culture_profile: Option<CultureProfileSummary>,
    #[serde(default)]
    pub active_seasonal_packs: Vec<SeasonalPackSummary>,
}

/// Scoped Navidrome OpenSubsonic connection parameters for direct media streaming, cover art,
/// offline caching, and delta sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeSessionInfo {
    pub base_url: String,
    pub username: String,
    pub subsonic_salt: String,
    pub subsonic_token: String,
    pub api_version: String,
    pub client_name: String,
    #[serde(default)]
    pub server_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub acquire_and_play: bool,
    pub connect_ws: bool,
    pub lyrics_fetch: bool,
    pub discovery: bool,
    pub multi_listener_guard: bool,
}

/// Lightweight summary of a user's inferred/configured cultural listening profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CultureProfileSummary {
    pub home_country: String,
    pub primary_languages: Vec<String>,
    pub local_culture_ratio: f32,
}

/// Lightweight summary of an active seasonal discovery pack for client bootstrap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeasonalPackSummary {
    pub slug: String,
    pub title: String,
    pub subtitle: String,
    pub track_count: u32,
    pub local_ratio: f32,
}

/// Structured or plain-text lyrics for a track returned to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackLyrics {
    #[serde(default)]
    pub display_artist: Option<String>,
    #[serde(default)]
    pub display_title: Option<String>,
    #[serde(default)]
    pub lang: String,
    pub synced: bool,
    #[serde(default)]
    pub offset_ms: Option<i64>,
    pub lines: Vec<TrackLyricLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackLyricLine {
    /// Timestamp in milliseconds from start of track when synced.
    #[serde(default)]
    pub start_ms: Option<i64>,
    pub text: String,
}

// --- Unified Curation ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepTrackRequest {
    pub song_id: String,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
    #[serde(default)]
    pub target_folder_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarTrackRequest {
    pub song_id: String,
    pub starred: bool,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateTrackRequest {
    pub song_id: String,
    /// Rating from 0 (clear) to 5.
    pub rating: u8,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropTrackRequest {
    pub song_id: String,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
    /// For permanent library tracks where the caller is the sole owner, must be `true` to
    /// actually unlink the file from disk; `false` returns the guard preview without deleting.
    #[serde(default)]
    pub confirm_delete: bool,
    #[serde(default)]
    pub reason: Option<DeletionReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurationAction {
    /// Pending discovery track promoted to permanent library, or library track affirmed.
    Kept,
    /// Track starred (and promoted if it was pending discovery).
    Starred,
    /// Track unstarred.
    Unstarred,
    /// Track rating updated (and promoted if >= threshold on pending discovery).
    Rated,
    /// Pending discovery track removed from staging and recorded as rejected.
    DroppedDiscovery,
    /// Permanent library track deleted from disk (sole owner confirmed).
    DeletedFromLibrary,
    /// Permanent library track protected by another listener: file kept, unstarred & 1★ for caller.
    ProtectedAndUnstarred,
    /// Guard preview returned without mutating disk (awaiting confirmation).
    ConfirmationRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurationResult {
    pub song_id: String,
    pub action: CurationAction,
    pub promoted_discovery: bool,
    #[serde(default)]
    pub starred: Option<bool>,
    #[serde(default)]
    pub rating: Option<u8>,
    pub deleted_from_disk: bool,
    /// Hook flag indicating a background archival FLAC upgrade was queued on Keep/Star.
    pub archival_upgrade_queued: bool,
    #[serde(default)]
    pub guard_preview: Option<DeleteGuardPreview>,
    pub message: String,
}

/// Pre-delete and Track Inspector summary of Soulbeet's Multi-Listener Delete Guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteGuardPreview {
    pub song_id: String,
    pub is_pending_discovery: bool,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
    pub can_delete: bool,
    pub sole_owner: bool,
    #[serde(default)]
    pub other_listeners: Vec<ListenerProtection>,
    #[serde(default)]
    pub own_playlists: Vec<String>,
    #[serde(default)]
    pub block_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerProtection {
    pub kind: ListenerProtectionKind,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerProtectionKind {
    OtherListenerMark,
    OtherListenerPlaylist,
    UnreadableListener,
    OutsideLibrary,
    PathUnresolved,
    InUse,
    Other,
}

// --- Multi-Device Connect ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    Web,
    Desktop,
    Android,
    AndroidAuto,
}

impl DevicePlatform {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "desktop" => Self::Desktop,
            "android" => Self::Android,
            "android_auto" | "android-auto" | "auto" => Self::AndroidAuto,
            _ => Self::Web,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectDevice {
    pub device_id: String,
    pub name: String,
    pub platform: DevicePlatform,
    pub is_active: bool,
    pub last_seen_unix: i64,
    #[serde(default)]
    pub snapshot: Option<PlaybackSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    #[serde(default)]
    pub song_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub cover_art_id: Option<String>,
    pub is_playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    #[serde(default)]
    pub queue_song_ids: Vec<String>,
    pub queue_index: usize,
    pub volume: f32,
    #[serde(default)]
    pub is_pending_discovery: bool,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectCommand {
    Play,
    Pause,
    TogglePlay,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        volume: f32,
    },
    LoadQueue {
        song_ids: Vec<String>,
        start_index: usize,
        start_position_ms: u64,
        autoplay: bool,
    },
    TransferPlayback {
        from_device_id: String,
        snapshot: PlaybackSnapshot,
    },
}

/// Messages sent from a client device over `WS /api/ws/connect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectClientMessage {
    Register {
        device_id: String,
        name: String,
        platform: DevicePlatform,
    },
    Heartbeat,
    UpdateState {
        snapshot: PlaybackSnapshot,
    },
    SendCommand {
        target_device_id: String,
        command: ConnectCommand,
    },
    ClaimActive,
}

/// Events sent from the server to a client device over `WS /api/ws/connect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectServerEvent {
    DeviceList {
        devices: Vec<ConnectDevice>,
        active_device_id: Option<String>,
    },
    Command {
        from_device_id: String,
        command: ConnectCommand,
    },
    PlaybackUpdated {
        device_id: String,
        snapshot: PlaybackSnapshot,
    },
}
