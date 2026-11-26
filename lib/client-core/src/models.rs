use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// OpenSubsonic connection parameters scoped to a user (no plaintext password stored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeCredentials {
    pub base_url: String,
    pub username: String,
    pub subsonic_salt: String,
    pub subsonic_token: String,
    pub api_version: String,
    pub client_name: String,
}

impl NavidromeCredentials {
    pub fn normalized_base_url(&self) -> String {
        self.base_url.trim_end_matches('/').to_string()
    }
}

/// Typed authentication credential for Soulbeet HTTP and WebSocket requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "token", rename_all = "snake_case")]
pub enum SoulbeetAuth {
    /// Sends `Authorization: Bearer <token>`.
    Bearer(String),
    /// Sends `Cookie: auth_token=<token>`.
    SessionCookie(String),
}

impl SoulbeetAuth {
    /// Returns the lowercase HTTP header name and formatted header value.
    pub fn header_name_and_value(&self) -> (&'static str, String) {
        match self {
            Self::Bearer(token) => ("authorization", format!("Bearer {token}")),
            Self::SessionCookie(token) => ("cookie", format!("auth_token={token}")),
        }
    }
}

/// Device platform identifier for Connect & quality profile defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    Web,
    Desktop,
    Android,
    AndroidAuto,
}

impl DevicePlatform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Android => "android",
            Self::AndroidAuto => "android_auto",
        }
    }
}

/// Profile-aware streaming & offline download quality policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AudioQualityProfile {
    /// Desktop / Wi-Fi direct lossless stream or download (`format=raw`).
    LosslessFlac,
    /// Mobile / Cellular bandwidth-friendly transcode (`format=opus&maxBitRate=160`).
    CellularOpus160,
    /// Custom Navidrome OpenSubsonic transcoding profile.
    CustomTranscode { format: String, max_bit_rate: u32 },
}

impl AudioQualityProfile {
    pub fn format_label(&self) -> &str {
        match self {
            Self::LosslessFlac => "flac",
            Self::CellularOpus160 => "opus",
            Self::CustomTranscode { format, .. } => format.as_str(),
        }
    }

    pub fn max_bit_rate(&self) -> Option<u32> {
        match self {
            Self::LosslessFlac => None,
            Self::CellularOpus160 => Some(160),
            Self::CustomTranscode { max_bit_rate, .. } => Some(*max_bit_rate),
        }
    }

    pub fn file_extension(&self) -> &str {
        match self {
            Self::LosslessFlac => "flac",
            Self::CellularOpus160 => "opus",
            Self::CustomTranscode { format, .. } => format.as_str(),
        }
    }
}

/// Local offline cache budget and auto-pinning preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicy {
    /// Maximum disk budget in bytes for cached audio files (default: 4 GiB).
    pub max_bytes: u64,
    /// Maximum disk budget in bytes for cached cover art (default: 256 MiB).
    pub cover_art_max_bytes: u64,
    /// When true, background auto-caching & queue prefetch only run on Wi-Fi.
    pub wifi_only_auto_cache: bool,
    /// Number of upcoming tracks in the active queue to prefetch into cache.
    pub prefetch_queue_count: usize,
    pub auto_pin_starred: bool,
    pub auto_pin_weekly_radar: bool,
    pub auto_pin_active_seasonal: bool,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024 * 1024,
            cover_art_max_bytes: 256 * 1024 * 1024,
            wifi_only_auto_cache: true,
            prefetch_queue_count: 3,
            auto_pin_starred: true,
            auto_pin_weekly_radar: true,
            auto_pin_active_seasonal: true,
        }
    }
}

/// Top-level initialization configuration for [`crate::SoulbeetCore`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub cover_art_dir: PathBuf,
    /// Optional Soulbeet server URL (e.g. `https://music.example.com`).
    pub soulbeet_base_url: Option<String>,
    /// Optional typed authentication credential (Bearer token or session cookie) for Soulbeet API calls.
    pub soulbeet_auth: Option<SoulbeetAuth>,
    pub navidrome: Option<NavidromeCredentials>,
    pub device_id: String,
    pub device_name: String,
    pub platform: DevicePlatform,
    pub offline_only: bool,
    pub cache_policy: CachePolicy,
}

// --- Local SQLite Mirror Models ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalArtist {
    pub id: String,
    pub name: String,
    pub album_count: i64,
    pub cover_art_id: Option<String>,
    pub starred: bool,
    pub musicbrainz_id: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalAlbum {
    pub id: String,
    pub name: String,
    pub artist_id: Option<String>,
    pub artist_name: String,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub song_count: i64,
    pub duration_secs: i64,
    pub cover_art_id: Option<String>,
    pub starred: bool,
    pub created_at: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalTrack {
    pub id: String,
    pub title: String,
    pub artist_id: Option<String>,
    pub artist_name: String,
    pub album_id: Option<String>,
    pub album_name: String,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub duration_secs: i64,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub bit_rate: Option<i64>,
    pub suffix: Option<String>,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub cover_art_id: Option<String>,
    pub path: Option<String>,
    pub starred: bool,
    pub user_rating: i64,
    pub play_count: i64,
    pub last_played_at: Option<i64>,
    pub musicbrainz_id: Option<String>,
    pub is_discovery: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalPlaylist {
    pub id: String,
    pub name: String,
    pub comment: Option<String>,
    pub owner: Option<String>,
    pub song_count: i64,
    pub duration_secs: i64,
    pub cover_art_id: Option<String>,
    pub is_smart: bool,
    pub kind: String,
    pub slug: Option<String>,
    pub changed_at: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalDiscoveryTrack {
    pub id: String,
    pub song_id: Option<String>,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub folder_path: Option<String>,
    pub profile_slug: Option<String>,
    pub mix_slug: Option<String>,
    pub status: String,
    pub created_at: Option<String>,
    pub expires_at: Option<String>,
    pub reason_json: Option<String>,
    pub exclude_from_core_taste: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct OfflineTrackEntry {
    pub track_id: String,
    pub local_path: String,
    pub format: String,
    pub bit_rate: Option<i64>,
    pub size_bytes: i64,
    pub pinned: bool,
    pub pin_source: Option<String>,
    pub downloaded_at: i64,
    pub last_accessed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_audio_bytes: u64,
    pub pinned_audio_bytes: u64,
    pub unpinned_audio_bytes: u64,
    pub cached_track_count: u64,
    pub pinned_track_count: u64,
    pub total_cover_art_bytes: u64,
    pub cached_cover_art_count: u64,
    pub max_audio_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum PlaybackSource {
    LocalFile {
        path: String,
        format: String,
        bit_rate: Option<u32>,
    },
    RemoteStream {
        url: String,
        format: String,
        bit_rate: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PinTarget {
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Starred,
    WeeklyRadar,
    SeasonalPack(String),
}

impl PinTarget {
    pub fn as_db_pair(&self) -> (&'static str, String) {
        match self {
            Self::Track(id) => ("track", id.clone()),
            Self::Album(id) => ("album", id.clone()),
            Self::Artist(id) => ("artist", id.clone()),
            Self::Playlist(id) => ("playlist", id.clone()),
            Self::Starred => ("starred", "starred".to_string()),
            Self::WeeklyRadar => ("weekly_radar", "weekly_radar".to_string()),
            Self::SeasonalPack(slug) => ("seasonal_pack", slug.clone()),
        }
    }

    pub fn from_db_pair(kind: &str, id: &str) -> Option<Self> {
        match kind {
            "track" => Some(Self::Track(id.to_string())),
            "album" => Some(Self::Album(id.to_string())),
            "artist" => Some(Self::Artist(id.to_string())),
            "playlist" => Some(Self::Playlist(id.to_string())),
            "starred" => Some(Self::Starred),
            "weekly_radar" => Some(Self::WeeklyRadar),
            "seasonal_pack" => Some(Self::SeasonalPack(id.to_string())),
            _ => None,
        }
    }
}

// --- Mutation Outbox ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboxMutation {
    Scrobble {
        song_id: String,
        submission: bool,
        played_at_unix: i64,
    },
    Keep {
        song_id: String,
        discovery_track_id: Option<String>,
        target_folder_id: Option<String>,
    },
    Star {
        song_id: String,
        starred: bool,
        discovery_track_id: Option<String>,
    },
    Rate {
        song_id: String,
        rating: u8,
        discovery_track_id: Option<String>,
    },
    Drop {
        song_id: String,
        discovery_track_id: Option<String>,
        confirm_delete: bool,
    },
}

impl OutboxMutation {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Scrobble { .. } => "scrobble",
            Self::Keep { .. } => "keep",
            Self::Star { .. } => "star",
            Self::Rate { .. } => "rate",
            Self::Drop { .. } => "drop",
        }
    }

    pub fn song_id(&self) -> &str {
        match self {
            Self::Scrobble { song_id, .. }
            | Self::Keep { song_id, .. }
            | Self::Star { song_id, .. }
            | Self::Rate { song_id, .. }
            | Self::Drop { song_id, .. } => song_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct OutboxRow {
    pub seq: i64,
    pub idempotency_key: String,
    pub kind: String,
    pub song_id: String,
    pub payload_json: String,
    pub created_at: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub next_retry_at: i64,
    #[serde(default)]
    pub dead_letter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DrainReport {
    pub replayed: usize,
    pub failed: usize,
    pub remaining: usize,
}

// --- Unified Search ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCatalogTrack {
    pub mbid: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u64>,
    pub year: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResults {
    pub query: String,
    pub offline_only: bool,
    pub elapsed_ms: u64,
    pub local_tracks: Vec<LocalTrack>,
    pub local_albums: Vec<LocalAlbum>,
    pub local_artists: Vec<LocalArtist>,
    pub remote_tracks: Vec<RemoteCatalogTrack>,
}

// --- Android Auto Bounded Browse & Ignition Resumption ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoBrowseKind {
    Tab,
    Folder,
    Playlist,
    Album,
    Artist,
    Track,
    RadioSeed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoBrowseNode {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub kind: AutoBrowseKind,
    pub playable: bool,
    pub browsable: bool,
    pub cover_art_id: Option<String>,
    pub cover_art_local_path: Option<String>,
    pub is_offline_ready: bool,
    pub is_pending_discovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMatchKind {
    Playlist,
    Artist,
    Album,
    Track,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceMatchResult {
    pub kind: VoiceMatchKind,
    pub target_id: String,
    pub label: String,
    pub track_ids: Vec<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedPlaybackSession {
    pub queue_track_ids: Vec<String>,
    pub current_index: usize,
    pub position_ms: u64,
    pub is_playing: bool,
    pub source_label: Option<String>,
    pub updated_at: i64,
}

// --- Sync Report ---

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub full_sync: bool,
    pub artists_synced: usize,
    pub albums_synced: usize,
    pub tracks_synced: usize,
    pub playlists_synced: usize,
    pub discovery_tracks_synced: usize,
    pub starred_reconciled: usize,
    pub warnings: Vec<String>,
}

// --- Soulbeet Wire DTOs (compatible with PR 1A / 1B / 1C) ---

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientBootstrapDto {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
    pub navidrome: Option<NavidromeCredentials>,
    #[serde(default)]
    pub promote_threshold: u8,
    #[serde(default)]
    pub auto_delete_enabled: bool,
    #[serde(default)]
    pub discovery_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectDeviceDto {
    pub device_id: String,
    pub name: String,
    pub platform: DevicePlatform,
    pub is_active: bool,
    pub last_seen_unix: i64,
    #[serde(default)]
    pub snapshot: Option<PlaybackSnapshotDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshotDto {
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
pub enum ConnectCommandDto {
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
        snapshot: PlaybackSnapshotDto,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectClientMessageDto {
    Register {
        device_id: String,
        name: String,
        platform: DevicePlatform,
    },
    Heartbeat,
    UpdateState {
        snapshot: PlaybackSnapshotDto,
    },
    SendCommand {
        target_device_id: String,
        command: ConnectCommandDto,
    },
    ClaimActive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectServerEventDto {
    DeviceList {
        devices: Vec<ConnectDeviceDto>,
        active_device_id: Option<String>,
    },
    Command {
        from_device_id: String,
        command: ConnectCommandDto,
    },
    PlaybackUpdated {
        device_id: String,
        snapshot: PlaybackSnapshotDto,
    },
}
