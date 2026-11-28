use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use crate::connect::ConnectState;
use crate::error::CoreError;
use crate::models::{
    AudioQualityProfile, AutoBrowseKind, AutoBrowseNode, CachePolicy, CacheStats,
    ClientBootstrapDto, ClientConfig, ConnectCommandDto, ConnectDeviceDto, ConnectServerEventDto,
    DevicePlatform, DrainReport, LibraryStats, LocalAlbum, LocalArtist, LocalDiscoveryTrack,
    LocalPlaylist, LocalTrack, NavidromeCredentials, OfflineTrackEntry, OutboxMutation, OutboxRow,
    OutboxSummary, PersistedPlaybackSession, PinRecord, PinTarget, PlaybackSnapshotDto,
    PlaybackSource, RemoteAlbumDetail, RemoteArtistDetail, RemoteCatalogAlbum, RemoteCatalogArtist,
    RemoteCatalogResults, RemoteCatalogTrack, SearchResults, SoulbeetAuth, SyncReport,
    VoiceMatchKind, VoiceMatchResult,
};
use crate::SoulbeetCore;

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Error, uniffi::Error)]
pub enum FfiError {
    #[error("database error: {detail}")]
    Database { detail: String },
    #[error("migration error: {detail}")]
    Migration { detail: String },
    #[error("http error: {detail}")]
    Http { detail: String },
    #[error("json error: {detail}")]
    Json { detail: String },
    #[error("io error: {detail}")]
    Io { detail: String },
    #[error("url parse error: {detail}")]
    Url { detail: String },
    #[error("websocket error: {detail}")]
    WebSocket { detail: String },
    #[error("not configured: {field}")]
    NotConfigured { field: String },
    #[error("offline mode active and resource not cached: {detail}")]
    Offline { detail: String },
    #[error("subsonic error {code}: {detail}")]
    Subsonic { code: i64, detail: String },
    #[error("server returned HTTP {status}: {body}")]
    ServerHttp { status: u16, body: String },
    #[error("invalid input: {detail}")]
    InvalidInput { detail: String },
    #[error("internal runtime error: {detail}")]
    Internal { detail: String },
}

impl From<CoreError> for FfiError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::Db(e) => Self::Database {
                detail: e.to_string(),
            },
            CoreError::Migrate(e) => Self::Migration {
                detail: e.to_string(),
            },
            CoreError::Http(e) => Self::Http {
                detail: e.to_string(),
            },
            CoreError::Json(e) => Self::Json {
                detail: e.to_string(),
            },
            CoreError::Io(e) => Self::Io {
                detail: e.to_string(),
            },
            CoreError::Url(e) => Self::Url {
                detail: e.to_string(),
            },
            CoreError::WebSocket(detail) => Self::WebSocket { detail },
            CoreError::NotConfigured(field) => Self::NotConfigured {
                field: field.to_string(),
            },
            CoreError::Offline(detail) => Self::Offline { detail },
            CoreError::Subsonic { code, message } => Self::Subsonic {
                code,
                detail: message,
            },
            CoreError::ServerHttp { status, body } => Self::ServerHttp { status, body },
            CoreError::InvalidInput(detail) => Self::InvalidInput { detail },
        }
    }
}

// ============================================================================
// Config & Auth DTOs
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiDevicePlatform {
    Web,
    Desktop,
    Android,
    AndroidAuto,
}

impl From<FfiDevicePlatform> for DevicePlatform {
    fn from(v: FfiDevicePlatform) -> Self {
        match v {
            FfiDevicePlatform::Web => Self::Web,
            FfiDevicePlatform::Desktop => Self::Desktop,
            FfiDevicePlatform::Android => Self::Android,
            FfiDevicePlatform::AndroidAuto => Self::AndroidAuto,
        }
    }
}

impl From<DevicePlatform> for FfiDevicePlatform {
    fn from(v: DevicePlatform) -> Self {
        match v {
            DevicePlatform::Web => Self::Web,
            DevicePlatform::Desktop => Self::Desktop,
            DevicePlatform::Android => Self::Android,
            DevicePlatform::AndroidAuto => Self::AndroidAuto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiNavidromeCredentials {
    pub base_url: String,
    pub username: String,
    pub subsonic_salt: String,
    pub subsonic_token: String,
    pub api_version: String,
    pub client_name: String,
}

impl From<FfiNavidromeCredentials> for NavidromeCredentials {
    fn from(v: FfiNavidromeCredentials) -> Self {
        Self {
            base_url: v.base_url,
            username: v.username,
            subsonic_salt: v.subsonic_salt,
            subsonic_token: v.subsonic_token,
            api_version: v.api_version,
            client_name: v.client_name,
        }
    }
}

impl From<NavidromeCredentials> for FfiNavidromeCredentials {
    fn from(v: NavidromeCredentials) -> Self {
        Self {
            base_url: v.base_url,
            username: v.username,
            subsonic_salt: v.subsonic_salt,
            subsonic_token: v.subsonic_token,
            api_version: v.api_version,
            client_name: v.client_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSoulbeetAuth {
    None,
    Bearer { token: String },
    SessionCookie { token: String },
}

impl From<FfiSoulbeetAuth> for Option<SoulbeetAuth> {
    fn from(v: FfiSoulbeetAuth) -> Self {
        match v {
            FfiSoulbeetAuth::None => None,
            FfiSoulbeetAuth::Bearer { token } => Some(SoulbeetAuth::Bearer(token)),
            FfiSoulbeetAuth::SessionCookie { token } => Some(SoulbeetAuth::SessionCookie(token)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiAudioQualityProfile {
    LosslessFlac,
    CellularOpus160,
    CustomTranscode { format: String, max_bit_rate: u32 },
}

impl From<FfiAudioQualityProfile> for AudioQualityProfile {
    fn from(v: FfiAudioQualityProfile) -> Self {
        match v {
            FfiAudioQualityProfile::LosslessFlac => Self::LosslessFlac,
            FfiAudioQualityProfile::CellularOpus160 => Self::CellularOpus160,
            FfiAudioQualityProfile::CustomTranscode {
                format,
                max_bit_rate,
            } => Self::CustomTranscode {
                format,
                max_bit_rate,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiCachePolicy {
    pub max_bytes: u64,
    pub cover_art_max_bytes: u64,
    pub wifi_only_auto_cache: bool,
    pub prefetch_queue_count: u32,
    pub auto_pin_starred: bool,
    pub auto_pin_weekly_radar: bool,
    pub auto_pin_active_seasonal: bool,
}

impl From<FfiCachePolicy> for CachePolicy {
    fn from(v: FfiCachePolicy) -> Self {
        Self {
            max_bytes: v.max_bytes,
            cover_art_max_bytes: v.cover_art_max_bytes,
            wifi_only_auto_cache: v.wifi_only_auto_cache,
            prefetch_queue_count: v.prefetch_queue_count as usize,
            auto_pin_starred: v.auto_pin_starred,
            auto_pin_weekly_radar: v.auto_pin_weekly_radar,
            auto_pin_active_seasonal: v.auto_pin_active_seasonal,
        }
    }
}

impl From<CachePolicy> for FfiCachePolicy {
    fn from(v: CachePolicy) -> Self {
        Self {
            max_bytes: v.max_bytes,
            cover_art_max_bytes: v.cover_art_max_bytes,
            wifi_only_auto_cache: v.wifi_only_auto_cache,
            prefetch_queue_count: u32::try_from(v.prefetch_queue_count).unwrap_or(u32::MAX),
            auto_pin_starred: v.auto_pin_starred,
            auto_pin_weekly_radar: v.auto_pin_weekly_radar,
            auto_pin_active_seasonal: v.auto_pin_active_seasonal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiClientConfig {
    pub db_path: String,
    pub cache_dir: String,
    pub cover_art_dir: String,
    pub soulbeet_base_url: Option<String>,
    pub soulbeet_auth: FfiSoulbeetAuth,
    pub navidrome: Option<FfiNavidromeCredentials>,
    pub device_id: String,
    pub device_name: String,
    pub platform: FfiDevicePlatform,
    pub offline_only: bool,
    pub cache_policy: FfiCachePolicy,
}

impl From<FfiClientConfig> for ClientConfig {
    fn from(v: FfiClientConfig) -> Self {
        Self {
            db_path: PathBuf::from(v.db_path),
            cache_dir: PathBuf::from(v.cache_dir),
            cover_art_dir: PathBuf::from(v.cover_art_dir),
            soulbeet_base_url: v.soulbeet_base_url,
            soulbeet_auth: v.soulbeet_auth.into(),
            navidrome: v.navidrome.map(Into::into),
            device_id: v.device_id,
            device_name: v.device_name,
            platform: v.platform.into(),
            offline_only: v.offline_only,
            cache_policy: v.cache_policy.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiClientBootstrap {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
    pub navidrome: Option<FfiNavidromeCredentials>,
    pub promote_threshold: u8,
    pub auto_delete_enabled: bool,
    pub discovery_enabled: bool,
}

impl From<ClientBootstrapDto> for FfiClientBootstrap {
    fn from(v: ClientBootstrapDto) -> Self {
        Self {
            user_id: v.user_id,
            username: v.username,
            is_admin: v.is_admin,
            navidrome: v.navidrome.map(Into::into),
            promote_threshold: v.promote_threshold,
            auto_delete_enabled: v.auto_delete_enabled,
            discovery_enabled: v.discovery_enabled,
        }
    }
}

// ============================================================================
// Playback & Cache DTOs
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiHttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiPlaybackSource {
    LocalFile {
        path: String,
        format: String,
        bit_rate: Option<u32>,
    },
    StreamUrl {
        url: String,
        format: String,
        bit_rate: Option<u32>,
        headers: Vec<FfiHttpHeader>,
    },
}

impl From<PlaybackSource> for FfiPlaybackSource {
    fn from(v: PlaybackSource) -> Self {
        match v {
            PlaybackSource::LocalFile {
                path,
                format,
                bit_rate,
            } => Self::LocalFile {
                path,
                format,
                bit_rate,
            },
            PlaybackSource::RemoteStream {
                url,
                format,
                bit_rate,
            } => Self::StreamUrl {
                url,
                format,
                bit_rate,
                headers: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiCacheStats {
    pub total_audio_bytes: u64,
    pub pinned_audio_bytes: u64,
    pub unpinned_audio_bytes: u64,
    pub cached_track_count: u64,
    pub pinned_track_count: u64,
    pub total_cover_art_bytes: u64,
    pub cached_cover_art_count: u64,
    pub max_audio_bytes: u64,
}

impl From<CacheStats> for FfiCacheStats {
    fn from(v: CacheStats) -> Self {
        Self {
            total_audio_bytes: v.total_audio_bytes,
            pinned_audio_bytes: v.pinned_audio_bytes,
            unpinned_audio_bytes: v.unpinned_audio_bytes,
            cached_track_count: v.cached_track_count,
            pinned_track_count: v.pinned_track_count,
            total_cover_art_bytes: v.total_cover_art_bytes,
            cached_cover_art_count: v.cached_cover_art_count,
            max_audio_bytes: v.max_audio_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiPinTarget {
    Track { id: String },
    Album { id: String },
    Artist { id: String },
    Playlist { id: String },
    Starred,
    WeeklyRadar,
    SeasonalPack { slug: String },
}

impl From<FfiPinTarget> for PinTarget {
    fn from(v: FfiPinTarget) -> Self {
        match v {
            FfiPinTarget::Track { id } => Self::Track(id),
            FfiPinTarget::Album { id } => Self::Album(id),
            FfiPinTarget::Artist { id } => Self::Artist(id),
            FfiPinTarget::Playlist { id } => Self::Playlist(id),
            FfiPinTarget::Starred => Self::Starred,
            FfiPinTarget::WeeklyRadar => Self::WeeklyRadar,
            FfiPinTarget::SeasonalPack { slug } => Self::SeasonalPack(slug),
        }
    }
}

impl From<PinTarget> for FfiPinTarget {
    fn from(v: PinTarget) -> Self {
        match v {
            PinTarget::Track(id) => Self::Track { id },
            PinTarget::Album(id) => Self::Album { id },
            PinTarget::Artist(id) => Self::Artist { id },
            PinTarget::Playlist(id) => Self::Playlist { id },
            PinTarget::Starred => Self::Starred,
            PinTarget::WeeklyRadar => Self::WeeklyRadar,
            PinTarget::SeasonalPack(slug) => Self::SeasonalPack { slug },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPinState {
    pub target: FfiPinTarget,
    pub pinned_at: i64,
}

impl From<PinRecord> for FfiPinState {
    fn from(v: PinRecord) -> Self {
        Self {
            target: v.target.into(),
            pinned_at: v.pinned_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiDownloadOutcome {
    pub track_id: String,
    pub local_path: String,
    pub format: String,
    pub bit_rate: Option<u32>,
    pub size_bytes: u64,
    pub pinned: bool,
    pub pin_source: Option<String>,
    pub downloaded_at: i64,
    pub last_accessed_at: i64,
}

impl From<OfflineTrackEntry> for FfiDownloadOutcome {
    fn from(v: OfflineTrackEntry) -> Self {
        Self {
            track_id: v.track_id,
            local_path: v.local_path,
            format: v.format,
            bit_rate: v.bit_rate.map(|b| u32::try_from(b.max(0)).unwrap_or(u32::MAX)),
            size_bytes: v.size_bytes.max(0) as u64,
            pinned: v.pinned,
            pin_source: v.pin_source,
            downloaded_at: v.downloaded_at,
            last_accessed_at: v.last_accessed_at,
        }
    }
}

// ============================================================================
// Sync & Library DTOs
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSyncReport {
    pub full_sync: bool,
    pub artists_synced: u64,
    pub albums_synced: u64,
    pub tracks_synced: u64,
    pub playlists_synced: u64,
    pub discovery_tracks_synced: u64,
    pub starred_reconciled: u64,
    pub warnings: Vec<String>,
}

impl From<SyncReport> for FfiSyncReport {
    fn from(v: SyncReport) -> Self {
        Self {
            full_sync: v.full_sync,
            artists_synced: v.artists_synced as u64,
            albums_synced: v.albums_synced as u64,
            tracks_synced: v.tracks_synced as u64,
            playlists_synced: v.playlists_synced as u64,
            discovery_tracks_synced: v.discovery_tracks_synced as u64,
            starred_reconciled: v.starred_reconciled as u64,
            warnings: v.warnings,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiArtist {
    pub id: String,
    pub name: String,
    pub album_count: i64,
    pub cover_art_id: Option<String>,
    pub starred: bool,
    pub musicbrainz_id: Option<String>,
    pub updated_at: i64,
}

impl From<LocalArtist> for FfiArtist {
    fn from(v: LocalArtist) -> Self {
        Self {
            id: v.id,
            name: v.name,
            album_count: v.album_count,
            cover_art_id: v.cover_art_id,
            starred: v.starred,
            musicbrainz_id: v.musicbrainz_id,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiAlbum {
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

impl From<LocalAlbum> for FfiAlbum {
    fn from(v: LocalAlbum) -> Self {
        Self {
            id: v.id,
            name: v.name,
            artist_id: v.artist_id,
            artist_name: v.artist_name,
            year: v.year,
            genre: v.genre,
            song_count: v.song_count,
            duration_secs: v.duration_secs,
            cover_art_id: v.cover_art_id,
            starred: v.starred,
            created_at: v.created_at,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiTrack {
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

impl From<LocalTrack> for FfiTrack {
    fn from(v: LocalTrack) -> Self {
        Self {
            id: v.id,
            title: v.title,
            artist_id: v.artist_id,
            artist_name: v.artist_name,
            album_id: v.album_id,
            album_name: v.album_name,
            track_number: v.track_number,
            disc_number: v.disc_number,
            duration_secs: v.duration_secs,
            year: v.year,
            genre: v.genre,
            bit_rate: v.bit_rate,
            suffix: v.suffix,
            content_type: v.content_type,
            size_bytes: v.size_bytes,
            cover_art_id: v.cover_art_id,
            path: v.path,
            starred: v.starred,
            user_rating: v.user_rating,
            play_count: v.play_count,
            last_played_at: v.last_played_at,
            musicbrainz_id: v.musicbrainz_id,
            is_discovery: v.is_discovery,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPlaylist {
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

impl From<LocalPlaylist> for FfiPlaylist {
    fn from(v: LocalPlaylist) -> Self {
        Self {
            id: v.id,
            name: v.name,
            comment: v.comment,
            owner: v.owner,
            song_count: v.song_count,
            duration_secs: v.duration_secs,
            cover_art_id: v.cover_art_id,
            is_smart: v.is_smart,
            kind: v.kind,
            slug: v.slug,
            changed_at: v.changed_at,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiDiscoveryTrack {
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

impl From<LocalDiscoveryTrack> for FfiDiscoveryTrack {
    fn from(v: LocalDiscoveryTrack) -> Self {
        Self {
            id: v.id,
            song_id: v.song_id,
            artist: v.artist,
            title: v.title,
            album: v.album,
            folder_path: v.folder_path,
            profile_slug: v.profile_slug,
            mix_slug: v.mix_slug,
            status: v.status,
            created_at: v.created_at,
            expires_at: v.expires_at,
            reason_json: v.reason_json,
            exclude_from_core_taste: v.exclude_from_core_taste,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiLibraryStats {
    pub artist_count: u64,
    pub album_count: u64,
    pub track_count: u64,
    pub playlist_count: u64,
    pub starred_track_count: u64,
    pub pending_discovery_count: u64,
}

impl From<LibraryStats> for FfiLibraryStats {
    fn from(v: LibraryStats) -> Self {
        Self {
            artist_count: v.artist_count,
            album_count: v.album_count,
            track_count: v.track_count,
            playlist_count: v.playlist_count,
            starred_track_count: v.starred_track_count,
            pending_discovery_count: v.pending_discovery_count,
        }
    }
}

// ============================================================================
// Outbox & Curation DTOs
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiOutboxMutation {
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
        discovery_track_id: Option<String>,
    },
    Unstar {
        song_id: String,
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

impl TryFrom<FfiOutboxMutation> for OutboxMutation {
    type Error = FfiError;

    fn try_from(v: FfiOutboxMutation) -> Result<Self, Self::Error> {
        match v {
            FfiOutboxMutation::Scrobble {
                song_id,
                submission,
                played_at_unix,
            } => Ok(Self::Scrobble {
                song_id,
                submission,
                played_at_unix,
            }),
            FfiOutboxMutation::Keep {
                song_id,
                discovery_track_id,
                target_folder_id,
            } => Ok(Self::Keep {
                song_id,
                discovery_track_id,
                target_folder_id,
            }),
            FfiOutboxMutation::Star {
                song_id,
                discovery_track_id,
            } => Ok(Self::Star {
                song_id,
                starred: true,
                discovery_track_id,
            }),
            FfiOutboxMutation::Unstar {
                song_id,
                discovery_track_id,
            } => Ok(Self::Star {
                song_id,
                starred: false,
                discovery_track_id,
            }),
            FfiOutboxMutation::Rate {
                song_id,
                rating,
                discovery_track_id,
            } => {
                if rating > 5 {
                    return Err(FfiError::InvalidInput {
                        detail: format!("rating must be between 0 and 5 (got {rating})"),
                    });
                }
                Ok(Self::Rate {
                    song_id,
                    rating,
                    discovery_track_id,
                })
            }
            FfiOutboxMutation::Drop {
                song_id,
                discovery_track_id,
                confirm_delete,
            } => Ok(Self::Drop {
                song_id,
                discovery_track_id,
                confirm_delete,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOutboxRow {
    pub seq: i64,
    pub idempotency_key: String,
    pub kind: String,
    pub song_id: String,
    pub payload_json: String,
    pub created_at: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub next_retry_at: i64,
    pub dead_letter: bool,
}

impl From<OutboxRow> for FfiOutboxRow {
    fn from(v: OutboxRow) -> Self {
        Self {
            seq: v.seq,
            idempotency_key: v.idempotency_key,
            kind: v.kind,
            song_id: v.song_id,
            payload_json: v.payload_json,
            created_at: v.created_at,
            attempts: v.attempts,
            last_error: v.last_error,
            next_retry_at: v.next_retry_at,
            dead_letter: v.dead_letter,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOutboxSummary {
    pub pending_count: u64,
    pub dead_letter_count: u64,
    pub oldest_pending_at: Option<i64>,
    pub last_error: Option<String>,
}

impl From<OutboxSummary> for FfiOutboxSummary {
    fn from(v: OutboxSummary) -> Self {
        Self {
            pending_count: v.pending_count as u64,
            dead_letter_count: v.dead_letter_count as u64,
            oldest_pending_at: v.oldest_pending_at,
            last_error: v.last_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiDrainReport {
    pub replayed: u64,
    pub failed: u64,
    pub remaining: u64,
}

impl From<DrainReport> for FfiDrainReport {
    fn from(v: DrainReport) -> Self {
        Self {
            replayed: v.replayed as u64,
            failed: v.failed as u64,
            remaining: v.remaining as u64,
        }
    }
}

// ============================================================================
// Unified Search DTOs
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOnlineTrackMatch {
    pub id: String,
    pub mbid: String,
    pub release_mbid: Option<String>,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub album: String,
    pub album_id: Option<String>,
    pub duration_secs: u32,
    pub year: Option<u32>,
    pub cover_url: Option<String>,
    pub variant_label: Option<String>,
    pub provider: String,
}

impl From<RemoteCatalogTrack> for FfiOnlineTrackMatch {
    fn from(v: RemoteCatalogTrack) -> Self {
        Self {
            id: v.id,
            mbid: v.mbid,
            release_mbid: v.release_mbid,
            title: v.title,
            artist: v.artist,
            artist_id: v.artist_id,
            album: v.album,
            album_id: v.album_id,
            duration_secs: v.duration_secs,
            year: v.year,
            cover_url: v.cover_url,
            variant_label: v.variant_label,
            provider: v.provider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOnlineAlbumMatch {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub release_date: Option<String>,
    pub cover_url: Option<String>,
    pub mbid: Option<String>,
    pub release_types: Vec<String>,
    pub track_count: Option<u32>,
    pub provider: String,
}

impl From<RemoteCatalogAlbum> for FfiOnlineAlbumMatch {
    fn from(v: RemoteCatalogAlbum) -> Self {
        Self {
            id: v.id,
            title: v.title,
            artist: v.artist,
            artist_id: v.artist_id,
            release_date: v.release_date,
            cover_url: v.cover_url,
            mbid: v.mbid,
            release_types: v.release_types,
            track_count: v.track_count,
            provider: v.provider,
        }
    }
}

impl From<FfiOnlineAlbumMatch> for RemoteCatalogAlbum {
    fn from(v: FfiOnlineAlbumMatch) -> Self {
        Self {
            id: v.id,
            title: v.title,
            artist: v.artist,
            artist_id: v.artist_id,
            release_date: v.release_date,
            cover_url: v.cover_url,
            mbid: v.mbid,
            release_types: v.release_types,
            track_count: v.track_count,
            provider: v.provider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOnlineArtistMatch {
    pub id: String,
    pub name: String,
    pub disambiguation: Option<String>,
    pub picture_url: Option<String>,
    pub fan_count: Option<u64>,
    pub mbid: Option<String>,
    pub provider: String,
}

impl From<RemoteCatalogArtist> for FfiOnlineArtistMatch {
    fn from(v: RemoteCatalogArtist) -> Self {
        Self {
            id: v.id,
            name: v.name,
            disambiguation: v.disambiguation,
            picture_url: v.picture_url,
            fan_count: v.fan_count,
            mbid: v.mbid,
            provider: v.provider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiRemoteCatalogResults {
    pub query: String,
    pub provider: String,
    pub fallback_used: bool,
    pub tracks: Vec<FfiOnlineTrackMatch>,
    pub albums: Vec<FfiOnlineAlbumMatch>,
    pub artists: Vec<FfiOnlineArtistMatch>,
}

impl From<RemoteCatalogResults> for FfiRemoteCatalogResults {
    fn from(v: RemoteCatalogResults) -> Self {
        Self {
            query: v.query,
            provider: v.provider,
            fallback_used: v.fallback_used,
            tracks: v.tracks.into_iter().map(Into::into).collect(),
            albums: v.albums.into_iter().map(Into::into).collect(),
            artists: v.artists.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiRemoteAlbumDetail {
    pub album: FfiOnlineAlbumMatch,
    pub tracks: Vec<FfiOnlineTrackMatch>,
}

impl From<RemoteAlbumDetail> for FfiRemoteAlbumDetail {
    fn from(v: RemoteAlbumDetail) -> Self {
        Self {
            album: v.album.into(),
            tracks: v.tracks.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiRemoteArtistDetail {
    pub artist: FfiOnlineArtistMatch,
    pub provider: String,
    pub others: Vec<FfiOnlineArtistMatch>,
    pub top_tracks: Vec<FfiOnlineTrackMatch>,
    pub albums: Vec<FfiOnlineAlbumMatch>,
}

impl From<RemoteArtistDetail> for FfiRemoteArtistDetail {
    fn from(v: RemoteArtistDetail) -> Self {
        Self {
            artist: v.artist.into(),
            provider: v.provider,
            others: v.others.into_iter().map(Into::into).collect(),
            top_tracks: v.top_tracks.into_iter().map(Into::into).collect(),
            albums: v.albums.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSearchResults {
    pub query: String,
    pub offline_only: bool,
    pub elapsed_ms: u64,
    pub local_tracks: Vec<FfiTrack>,
    pub local_albums: Vec<FfiAlbum>,
    pub local_artists: Vec<FfiArtist>,
    pub online_tracks: Vec<FfiOnlineTrackMatch>,
    pub online_albums: Vec<FfiOnlineAlbumMatch>,
    pub online_artists: Vec<FfiOnlineArtistMatch>,
    pub remote_provider: Option<String>,
    pub remote_fallback_used: bool,
}

impl From<SearchResults> for FfiSearchResults {
    fn from(v: SearchResults) -> Self {
        Self {
            query: v.query,
            offline_only: v.offline_only,
            elapsed_ms: v.elapsed_ms,
            local_tracks: v.local_tracks.into_iter().map(Into::into).collect(),
            local_albums: v.local_albums.into_iter().map(Into::into).collect(),
            local_artists: v.local_artists.into_iter().map(Into::into).collect(),
            online_tracks: v.remote_tracks.into_iter().map(Into::into).collect(),
            online_albums: v.remote_albums.into_iter().map(Into::into).collect(),
            online_artists: v.remote_artists.into_iter().map(Into::into).collect(),
            remote_provider: v.remote_provider,
            remote_fallback_used: v.remote_fallback_used,
        }
    }
}

// ============================================================================
// Android Auto DTOs
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiAutoNodeKind {
    Tab,
    Folder,
    Playlist,
    Album,
    Artist,
    Track,
    RadioSeed,
}

impl From<AutoBrowseKind> for FfiAutoNodeKind {
    fn from(v: AutoBrowseKind) -> Self {
        match v {
            AutoBrowseKind::Tab => Self::Tab,
            AutoBrowseKind::Folder => Self::Folder,
            AutoBrowseKind::Playlist => Self::Playlist,
            AutoBrowseKind::Album => Self::Album,
            AutoBrowseKind::Artist => Self::Artist,
            AutoBrowseKind::Track => Self::Track,
            AutoBrowseKind::RadioSeed => Self::RadioSeed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiAutoBrowseNode {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub kind: FfiAutoNodeKind,
    pub playable: bool,
    pub browsable: bool,
    pub cover_art_id: Option<String>,
    pub cover_art_local_path: Option<String>,
    pub is_offline_ready: bool,
    pub is_pending_discovery: bool,
}

impl From<AutoBrowseNode> for FfiAutoBrowseNode {
    fn from(v: AutoBrowseNode) -> Self {
        Self {
            id: v.id,
            title: v.title,
            subtitle: v.subtitle,
            kind: v.kind.into(),
            playable: v.playable,
            browsable: v.browsable,
            cover_art_id: v.cover_art_id,
            cover_art_local_path: v.cover_art_local_path,
            is_offline_ready: v.is_offline_ready,
            is_pending_discovery: v.is_pending_discovery,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiVoiceMatchKind {
    Playlist,
    Artist,
    Album,
    Track,
}

impl From<VoiceMatchKind> for FfiVoiceMatchKind {
    fn from(v: VoiceMatchKind) -> Self {
        match v {
            VoiceMatchKind::Playlist => Self::Playlist,
            VoiceMatchKind::Artist => Self::Artist,
            VoiceMatchKind::Album => Self::Album,
            VoiceMatchKind::Track => Self::Track,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiVoiceMatchResult {
    pub kind: FfiVoiceMatchKind,
    pub target_id: String,
    pub label: String,
    pub track_ids: Vec<String>,
    pub elapsed_ms: u64,
}

impl From<VoiceMatchResult> for FfiVoiceMatchResult {
    fn from(v: VoiceMatchResult) -> Self {
        Self {
            kind: v.kind.into(),
            target_id: v.target_id,
            label: v.label,
            track_ids: v.track_ids,
            elapsed_ms: v.elapsed_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPersistedPlaybackSession {
    pub queue_track_ids: Vec<String>,
    pub current_index: u32,
    pub position_ms: u64,
    pub is_playing: bool,
    pub source_label: Option<String>,
    pub updated_at: i64,
}

impl From<FfiPersistedPlaybackSession> for PersistedPlaybackSession {
    fn from(v: FfiPersistedPlaybackSession) -> Self {
        Self {
            queue_track_ids: v.queue_track_ids,
            current_index: v.current_index as usize,
            position_ms: v.position_ms,
            is_playing: v.is_playing,
            source_label: v.source_label,
            updated_at: v.updated_at,
        }
    }
}

impl From<PersistedPlaybackSession> for FfiPersistedPlaybackSession {
    fn from(v: PersistedPlaybackSession) -> Self {
        Self {
            queue_track_ids: v.queue_track_ids,
            current_index: u32::try_from(v.current_index).unwrap_or(u32::MAX),
            position_ms: v.position_ms,
            is_playing: v.is_playing,
            source_label: v.source_label,
            updated_at: v.updated_at,
        }
    }
}

// ============================================================================
// Multi-Device Connect WS DTOs
// ============================================================================

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiPlaybackSnapshot {
    pub song_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_art_id: Option<String>,
    pub is_playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub queue_song_ids: Vec<String>,
    pub queue_index: u32,
    pub volume: f32,
    pub is_pending_discovery: bool,
    pub updated_at_unix: i64,
}

fn validate_volume(volume: f32) -> Result<f32, FfiError> {
    if !volume.is_finite() {
        return Err(FfiError::InvalidInput {
            detail: format!("volume must be finite (got {volume})"),
        });
    }
    Ok(volume.clamp(0.0, 1.0))
}

impl TryFrom<FfiPlaybackSnapshot> for PlaybackSnapshotDto {
    type Error = FfiError;

    fn try_from(v: FfiPlaybackSnapshot) -> Result<Self, Self::Error> {
        let volume = validate_volume(v.volume)?;
        Ok(Self {
            song_id: v.song_id,
            title: v.title,
            artist: v.artist,
            album: v.album,
            cover_art_id: v.cover_art_id,
            is_playing: v.is_playing,
            position_ms: v.position_ms,
            duration_ms: v.duration_ms,
            queue_song_ids: v.queue_song_ids,
            queue_index: v.queue_index as usize,
            volume,
            is_pending_discovery: v.is_pending_discovery,
            updated_at_unix: v.updated_at_unix,
        })
    }
}

impl From<PlaybackSnapshotDto> for FfiPlaybackSnapshot {
    fn from(v: PlaybackSnapshotDto) -> Self {
        Self {
            song_id: v.song_id,
            title: v.title,
            artist: v.artist,
            album: v.album,
            cover_art_id: v.cover_art_id,
            is_playing: v.is_playing,
            position_ms: v.position_ms,
            duration_ms: v.duration_ms,
            queue_song_ids: v.queue_song_ids,
            queue_index: u32::try_from(v.queue_index).unwrap_or(u32::MAX),
            volume: v.volume,
            is_pending_discovery: v.is_pending_discovery,
            updated_at_unix: v.updated_at_unix,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiConnectDevice {
    pub device_id: String,
    pub name: String,
    pub platform: FfiDevicePlatform,
    pub is_active: bool,
    pub last_seen_unix: i64,
    pub snapshot: Option<FfiPlaybackSnapshot>,
}

impl From<ConnectDeviceDto> for FfiConnectDevice {
    fn from(v: ConnectDeviceDto) -> Self {
        Self {
            device_id: v.device_id,
            name: v.name,
            platform: v.platform.into(),
            is_active: v.is_active,
            last_seen_unix: v.last_seen_unix,
            snapshot: v.snapshot.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiConnectState {
    pub connected: bool,
    pub devices: Vec<FfiConnectDevice>,
    pub active_device_id: Option<String>,
    pub snapshots: HashMap<String, FfiPlaybackSnapshot>,
}

impl From<ConnectState> for FfiConnectState {
    fn from(v: ConnectState) -> Self {
        Self {
            connected: v.connected,
            devices: v.devices.into_iter().map(Into::into).collect(),
            active_device_id: v.active_device_id,
            snapshots: v
                .snapshots
                .into_iter()
                .map(|(k, snap)| (k, snap.into()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum FfiConnectCommand {
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
        start_index: u32,
        start_position_ms: u64,
        autoplay: bool,
    },
    TransferPlayback {
        from_device_id: String,
        snapshot: FfiPlaybackSnapshot,
    },
}

impl TryFrom<FfiConnectCommand> for ConnectCommandDto {
    type Error = FfiError;

    fn try_from(v: FfiConnectCommand) -> Result<Self, Self::Error> {
        match v {
            FfiConnectCommand::Play => Ok(Self::Play),
            FfiConnectCommand::Pause => Ok(Self::Pause),
            FfiConnectCommand::TogglePlay => Ok(Self::TogglePlay),
            FfiConnectCommand::Next => Ok(Self::Next),
            FfiConnectCommand::Previous => Ok(Self::Previous),
            FfiConnectCommand::Seek { position_ms } => Ok(Self::Seek { position_ms }),
            FfiConnectCommand::SetVolume { volume } => Ok(Self::SetVolume {
                volume: validate_volume(volume)?,
            }),
            FfiConnectCommand::LoadQueue {
                song_ids,
                start_index,
                start_position_ms,
                autoplay,
            } => Ok(Self::LoadQueue {
                song_ids,
                start_index: start_index as usize,
                start_position_ms,
                autoplay,
            }),
            FfiConnectCommand::TransferPlayback {
                from_device_id,
                snapshot,
            } => Ok(Self::TransferPlayback {
                from_device_id,
                snapshot: snapshot.try_into()?,
            }),
        }
    }
}

impl From<ConnectCommandDto> for FfiConnectCommand {
    fn from(v: ConnectCommandDto) -> Self {
        match v {
            ConnectCommandDto::Play => Self::Play,
            ConnectCommandDto::Pause => Self::Pause,
            ConnectCommandDto::TogglePlay => Self::TogglePlay,
            ConnectCommandDto::Next => Self::Next,
            ConnectCommandDto::Previous => Self::Previous,
            ConnectCommandDto::Seek { position_ms } => Self::Seek { position_ms },
            ConnectCommandDto::SetVolume { volume } => Self::SetVolume { volume },
            ConnectCommandDto::LoadQueue {
                song_ids,
                start_index,
                start_position_ms,
                autoplay,
            } => Self::LoadQueue {
                song_ids,
                start_index: u32::try_from(start_index).unwrap_or(u32::MAX),
                start_position_ms,
                autoplay,
            },
            ConnectCommandDto::TransferPlayback {
                from_device_id,
                snapshot,
            } => Self::TransferPlayback {
                from_device_id,
                snapshot: snapshot.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum FfiConnectEvent {
    DeviceList {
        devices: Vec<FfiConnectDevice>,
        active_device_id: Option<String>,
    },
    Command {
        from_device_id: String,
        command: FfiConnectCommand,
    },
    PlaybackUpdated {
        device_id: String,
        snapshot: FfiPlaybackSnapshot,
    },
}

impl From<ConnectServerEventDto> for FfiConnectEvent {
    fn from(v: ConnectServerEventDto) -> Self {
        match v {
            ConnectServerEventDto::DeviceList {
                devices,
                active_device_id,
            } => Self::DeviceList {
                devices: devices.into_iter().map(Into::into).collect(),
                active_device_id,
            },
            ConnectServerEventDto::Command {
                from_device_id,
                command,
            } => Self::Command {
                from_device_id,
                command: command.into(),
            },
            ConnectServerEventDto::PlaybackUpdated {
                device_id,
                snapshot,
            } => Self::PlaybackUpdated {
                device_id,
                snapshot: snapshot.into(),
            },
        }
    }
}

// ============================================================================
// Reactive Event Callback Interface
// ============================================================================

/// Callback interface implemented by Kotlin/Android clients to receive reactive
/// updates from `SoulbeetCoreHandle`.
///
/// All callbacks are dispatched on a dedicated background thread with zero internal
/// locks held, so implementations may safely re-enter `SoulbeetCoreHandle` without
/// deadlocking.
#[uniffi::export(callback_interface)]
pub trait CoreEventListener: Send + Sync {
    fn on_connect_state_changed(&self, state: FfiConnectState);
    fn on_connect_event(&self, event: FfiConnectEvent);
    fn on_sync_completed(&self, report: FfiSyncReport);
    fn on_outbox_drained(&self, report: FfiDrainReport);
    fn on_cache_changed(&self, stats: FfiCacheStats);
}

enum CoreEvent {
    ConnectEvent(FfiConnectEvent),
    SyncCompleted(FfiSyncReport),
    OutboxDrained(FfiDrainReport),
}

/// Bounded capacity for FFI discrete event delivery so a slow foreign listener
/// cannot trigger unbounded memory growth.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Aborts the wrapped Tokio [`JoinHandle`] if the outer future is dropped (e.g.
/// when Kotlin cancels a `suspend fun` coroutine) instead of detaching a zombie task.
/// Only used for pure read/query futures; state mutators use cooperative cancellation.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Signals cooperative cancellation to a spawned state-mutating task when the caller
/// future is dropped, allowing any in-flight HTTP dispatch + DB commit to finish
/// atomically before stopping at the next item boundary.
struct CancelFlagGuard(Arc<AtomicBool>);

impl Drop for CancelFlagGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// ============================================================================
// Owned Tokio Runtime Wrapper (Safe Drop from Any Context)
// ============================================================================

struct OwnedRuntime {
    inner: Option<tokio::runtime::Runtime>,
}

impl OwnedRuntime {
    fn new() -> Result<Self, FfiError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("soulbeet-core")
            .enable_all()
            .build()
            .map_err(|e| FfiError::Internal {
                detail: format!("failed to initialize Tokio runtime: {e}"),
            })?;
        Ok(Self { inner: Some(rt) })
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        self.inner
            .as_ref()
            .expect("OwnedRuntime accessed after drop")
            .handle()
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.inner.take() {
            // `shutdown_background` never panics even if `SoulbeetCoreHandle` is dropped
            // inside an active Tokio runtime context (e.g. a coroutine callback or async test).
            rt.shutdown_background();
        }
    }
}

// ============================================================================
// Top-Level Helper Functions
// ============================================================================

#[uniffi::export]
pub fn default_cache_policy() -> FfiCachePolicy {
    CachePolicy::default().into()
}

// ============================================================================
// SoulbeetCoreHandle — Main UniFFI Object
// ============================================================================

#[derive(uniffi::Object)]
pub struct SoulbeetCoreHandle {
    core: Arc<SoulbeetCore>,
    runtime: OwnedRuntime,
    listener: Arc<StdRwLock<Option<Arc<dyn CoreEventListener>>>>,
    event_tx: mpsc::Sender<CoreEvent>,
    cache_stats_tx: watch::Sender<Option<FfiCacheStats>>,
    background_tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl Drop for SoulbeetCoreHandle {
    fn drop(&mut self) {
        // Release any registered foreign listener outside the RwLock guard so its Drop
        // cannot deadlock if it re-enters or triggers foreign cleanup.
        let prev = {
            let mut guard = self
                .listener
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take()
        };
        drop(prev);

        let mut tasks = self
            .background_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for handle in tasks.drain(..) {
            handle.abort();
        }
    }
}

impl SoulbeetCoreHandle {
    fn build_from_initialized(runtime: OwnedRuntime, core: SoulbeetCore) -> Arc<Self> {
        let core = Arc::new(core);
        let listener: Arc<StdRwLock<Option<Arc<dyn CoreEventListener>>>> =
            Arc::new(StdRwLock::new(None));
        let (event_tx, mut event_rx) = mpsc::channel::<CoreEvent>(EVENT_CHANNEL_CAPACITY);
        let (cache_stats_tx, mut cache_stats_rx) = watch::channel::<Option<FfiCacheStats>>(None);
        let (resync_state_tx, mut resync_state_rx) = watch::channel::<Option<FfiConnectState>>(None);

        // 1. Dedicated sequential event dispatcher: clones the listener Arc out of the
        //    RwLock (dropping the lock immediately) and runs each callback on Tokio's
        //    blocking pool so foreign code can safely call back into `SoulbeetCoreHandle`
        //    without deadlocking or blocking async workers.
        //    Latest-value state updates (`ConnectState`, `CacheStats`) travel via `watch`
        //    channels so they coalesce cleanly and are never dropped under queue pressure.
        let dispatch_listener = listener.clone();
        let mut connect_state_rx = core.connect_client().subscribe_state();
        let dispatcher_task = runtime.handle().spawn(async move {
            let mut event_open = true;
            let mut state_open = true;
            let mut resync_open = true;
            let mut cache_open = true;

            while event_open || state_open || resync_open || cache_open {
                enum DispatchItem {
                    Event(CoreEvent),
                    ConnectState(FfiConnectState),
                    CacheStats(FfiCacheStats),
                }

                let item = tokio::select! {
                    maybe_ev = event_rx.recv(), if event_open => match maybe_ev {
                        Some(ev) => Some(DispatchItem::Event(ev)),
                        None => {
                            event_open = false;
                            None
                        }
                    },
                    changed = connect_state_rx.changed(), if state_open => match changed {
                        Ok(()) => {
                            let st = connect_state_rx.borrow_and_update().clone();
                            Some(DispatchItem::ConnectState(st.into()))
                        }
                        Err(_) => {
                            state_open = false;
                            None
                        }
                    },
                    changed = resync_state_rx.changed(), if resync_open => match changed {
                        Ok(()) => resync_state_rx
                            .borrow_and_update()
                            .clone()
                            .map(DispatchItem::ConnectState),
                        Err(_) => {
                            resync_open = false;
                            None
                        }
                    },
                    changed = cache_stats_rx.changed(), if cache_open => match changed {
                        Ok(()) => cache_stats_rx
                            .borrow_and_update()
                            .clone()
                            .map(DispatchItem::CacheStats),
                        Err(_) => {
                            cache_open = false;
                            None
                        }
                    },
                };

                let Some(item) = item else {
                    continue;
                };

                let cb = {
                    let guard = dispatch_listener
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.clone()
                };
                let Some(cb) = cb else {
                    continue;
                };
                let res = tokio::task::spawn_blocking(move || match item {
                    DispatchItem::ConnectState(state) => cb.on_connect_state_changed(state),
                    DispatchItem::CacheStats(stats) => cb.on_cache_changed(stats),
                    DispatchItem::Event(CoreEvent::ConnectEvent(event)) => {
                        cb.on_connect_event(event)
                    }
                    DispatchItem::Event(CoreEvent::SyncCompleted(report)) => {
                        cb.on_sync_completed(report)
                    }
                    DispatchItem::Event(CoreEvent::OutboxDrained(report)) => {
                        cb.on_outbox_drained(report)
                    }
                })
                .await;
                if let Err(err) = res {
                    if err.is_panic() {
                        tracing::error!("CoreEventListener callback panicked: {err}");
                    }
                }
            }
        });

        // 2. Forward ConnectClient server events into the dispatcher.
        //    Commands (`TransferPlayback`, `LoadQueue`, etc.) await channel space instead of
        //    dropping; on `broadcast` lag or full channel for state-carrying events, immediately
        //    push an authoritative `ConnectState` resync so the foreign listener never drifts.
        let mut connect_ev_rx = core.connect_client().subscribe();
        let connect_tx = event_tx.clone();
        let forwarder_core = core.clone();
        let connect_forwarder_task = runtime.handle().spawn(async move {
            loop {
                match connect_ev_rx.recv().await {
                    Ok(event) => {
                        let is_command = matches!(event, ConnectServerEventDto::Command { .. });
                        if is_command {
                            if connect_tx
                                .send(CoreEvent::ConnectEvent(event.into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            match connect_tx.try_send(CoreEvent::ConnectEvent(event.into())) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    tracing::warn!(
                                        "FFI event channel full ({EVENT_CHANNEL_CAPACITY}); triggering ConnectState resync"
                                    );
                                    let latest = forwarder_core.connect_client().state().await;
                                    let _ = resync_state_tx.send(Some(latest.into()));
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "FFI Connect event forwarder lagged; triggering ConnectState resync"
                        );
                        let latest = forwarder_core.connect_client().state().await;
                        let _ = resync_state_tx.send(Some(latest.into()));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Arc::new(Self {
            core,
            runtime,
            listener,
            event_tx,
            cache_stats_tx,
            background_tasks: StdMutex::new(vec![dispatcher_task, connect_forwarder_task]),
        })
    }

    async fn run_async<F, T>(&self, fut: F) -> Result<T, FfiError>
    where
        F: Future<Output = Result<T, FfiError>> + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = AbortOnDrop(self.runtime.handle().spawn(fut));
        (&mut guard.0).await.map_err(|e| FfiError::Internal {
            detail: format!("core runtime task failed: {e}"),
        })?
    }

    /// Runs a non-cancel-safe state mutator on the internal Tokio runtime without aborting
    /// mid-flight if the foreign coroutine cancels; the task runs its atomic operation to completion.
    async fn run_async_uninterruptible<F, T>(&self, fut: F) -> Result<T, FfiError>
    where
        F: Future<Output = Result<T, FfiError>> + Send + 'static,
        T: Send + 'static,
    {
        self.runtime
            .handle()
            .spawn(fut)
            .await
            .map_err(|e| FfiError::Internal {
                detail: format!("core runtime task failed: {e}"),
            })?
    }

    /// Runs a multi-step state mutator with cooperative cancellation between items.
    /// Dropping the outer future sets the cancellation flag instead of aborting the Tokio task,
    /// guaranteeing in-flight HTTP dispatches and SQLite commits finish cleanly.
    async fn run_async_cooperative<Mk, F, T>(&self, make_fut: Mk) -> Result<T, FfiError>
    where
        Mk: FnOnce(Arc<AtomicBool>) -> F,
        F: Future<Output = Result<T, FfiError>> + Send + 'static,
        T: Send + 'static,
    {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let _cancel_guard = CancelFlagGuard(cancel_flag.clone());
        let handle = self.runtime.handle().spawn(make_fut(cancel_flag));
        handle.await.map_err(|e| FfiError::Internal {
            detail: format!("core runtime task failed: {e}"),
        })?
    }

    fn run_blocking<F, T>(&self, fut: F) -> Result<T, FfiError>
    where
        F: Future<Output = Result<T, FfiError>> + Send + 'static,
        T: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            let handle = self.runtime.handle().clone();
            std::thread::Builder::new()
                .name("soulbeet-core-bridge".to_string())
                .spawn(move || handle.block_on(fut))
                .map_err(|e| FfiError::Internal {
                    detail: format!("failed to spawn core blocking bridge thread: {e}"),
                })?
                .join()
                .map_err(|_| FfiError::Internal {
                    detail: "core blocking bridge thread panicked".to_string(),
                })?
        } else {
            self.runtime.handle().block_on(fut)
        }
    }

    fn emit_event(&self, event: CoreEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.event_tx.try_send(event) {
            tracing::warn!(
                "FFI event channel full ({EVENT_CHANNEL_CAPACITY}); dropping event notification"
            );
        }
    }

    fn emit_cache_changed(&self, stats: FfiCacheStats) {
        let _ = self.cache_stats_tx.send(Some(stats));
    }

    /// Exposes the underlying Rust [`SoulbeetCore`] for in-process Rust tests and callers.
    pub fn core(&self) -> &Arc<SoulbeetCore> {
        &self.core
    }
}

// ============================================================================
// Exported FFI Surface (Executor-Agnostic Async + Zero-Coroutine Sync Methods)
// ============================================================================

#[uniffi::export]
impl SoulbeetCoreHandle {
    /// Asynchronously opens and migrates the local SQLite database and initializes the core engine.
    #[uniffi::constructor]
    pub async fn open(config: FfiClientConfig) -> Result<Arc<Self>, FfiError> {
        let runtime = OwnedRuntime::new()?;
        let core_config: ClientConfig = config.into();
        let mut init_guard = AbortOnDrop(
            runtime
                .handle()
                .spawn(async move { SoulbeetCore::init(core_config).await.map_err(FfiError::from) }),
        );
        let core = (&mut init_guard.0)
            .await
            .map_err(|e| FfiError::Internal {
                detail: format!("core init task failed: {e}"),
            })??;
        Ok(Self::build_from_initialized(runtime, core))
    }

    /// Synchronously opens and migrates the local SQLite database and initializes the core engine.
    /// Safe to call from non-Tokio threads (e.g. Android `Application.onCreate` or `ContentProvider`).
    #[uniffi::constructor]
    pub fn open_sync(config: FfiClientConfig) -> Result<Arc<Self>, FfiError> {
        let runtime = OwnedRuntime::new()?;
        let core_config: ClientConfig = config.into();
        let core = if tokio::runtime::Handle::try_current().is_ok() {
            let handle = runtime.handle().clone();
            std::thread::Builder::new()
                .name("soulbeet-core-init".to_string())
                .spawn(move || handle.block_on(SoulbeetCore::init(core_config)))
                .map_err(|e| FfiError::Internal {
                    detail: format!("failed to spawn core init bridge thread: {e}"),
                })?
                .join()
                .map_err(|_| FfiError::Internal {
                    detail: "core init bridge thread panicked".to_string(),
                })?
                .map_err(FfiError::from)?
        } else {
            runtime
                .handle()
                .block_on(SoulbeetCore::init(core_config))
                .map_err(FfiError::from)?
        };
        Ok(Self::build_from_initialized(runtime, core))
    }

    pub fn is_offline_only(&self) -> bool {
        self.core.is_offline_only()
    }

    pub fn set_offline_only(&self, offline_only: bool) {
        self.core.set_offline_only(offline_only);
    }

    /// Registers a reactive [`CoreEventListener`] for state, sync, outbox, and cache notifications.
    ///
    /// **Lifecycle note:** Avoid holding a strong reference to `SoulbeetCoreHandle` inside the
    /// listener implementation (use a weak reference), OR call [`Self::clear_event_listener`] /
    /// [`Self::shutdown`] during teardown to break any cross-FFI reference cycle.
    pub fn set_event_listener(&self, listener: Box<dyn CoreEventListener>) {
        let arc_listener: Arc<dyn CoreEventListener> = Arc::from(listener);
        let prev = {
            let mut guard = self
                .listener
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.replace(arc_listener)
        };
        // Drop previous listener outside the RwLock write guard to prevent re-entrancy deadlocks.
        drop(prev);
    }

    /// Clears the registered [`CoreEventListener`], dropping it outside internal locks.
    pub fn clear_event_listener(&self) {
        let prev = {
            let mut guard = self
                .listener
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take()
        };
        drop(prev);
    }

    /// Gracefully shuts down this handle: clears any registered event listener (breaking
    /// cross-FFI reference cycles), disconnects the Connect WebSocket, aborts background
    /// forwarder tasks, and closes the SQLite connection pool.
    pub async fn shutdown(&self) -> Result<(), FfiError> {
        self.clear_event_listener();
        {
            let mut tasks = self
                .background_tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        let core = self.core.clone();
        self.run_async_uninterruptible(async move {
            core.disconnect_ws().await;
            core.pool().close().await;
            Ok(())
        })
        .await
    }

    /// Returns the 4 fixed Android Auto root tabs without touching disk or network.
    pub fn auto_root_tabs(&self) -> Vec<FfiAutoBrowseNode> {
        self.core
            .auto_root_tabs()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Pure read-only local SQLite + filesystem cover-art lookup with zero network I/O and
    /// zero SQLite writes. Designed for Android Auto's synchronous `ContentProvider.openFile`
    /// Binder threads.
    pub fn cover_art_local_path_sync(&self, cover_art_id: String) -> Result<Option<String>, FfiError> {
        let core = self.core.clone();
        self.run_blocking(async move {
            let path = core
                .cover_art_local_path(&cover_art_id)
                .await
                .map_err(FfiError::from)?;
            Ok(path.map(|p| p.display().to_string()))
        })
    }

    // --- Auth & Bootstrap ---

    pub async fn set_soulbeet_auth(&self, auth: FfiSoulbeetAuth) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_auth: Option<SoulbeetAuth> = auth.into();
        self.run_async(async move {
            core.set_soulbeet_auth(native_auth).await;
            Ok(())
        })
        .await
    }

    pub async fn set_navidrome_credentials(
        &self,
        creds: FfiNavidromeCredentials,
    ) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_creds: NavidromeCredentials = creds.into();
        self.run_async(async move {
            core.set_navidrome_credentials(native_creds).await;
            Ok(())
        })
        .await
    }

    pub async fn clear_navidrome_credentials(&self) -> Result<(), FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            core.clear_navidrome_credentials().await;
            Ok(())
        })
        .await
    }

    pub async fn fetch_bootstrap(&self) -> Result<Option<FfiClientBootstrap>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let dto = core.fetch_bootstrap().await.map_err(FfiError::from)?;
            Ok(dto.map(Into::into))
        })
        .await
    }

    // --- Sync ---

    pub async fn sync_now(&self, force_full: bool) -> Result<FfiSyncReport, FfiError> {
        let core = self.core.clone();
        let report: FfiSyncReport = self
            .run_async_cooperative(|cancel| async move {
                let rep = core
                    .sync_now_cancellable(force_full, &cancel)
                    .await
                    .map_err(FfiError::from)?;
                Ok(rep.into())
            })
            .await?;
        self.emit_event(CoreEvent::SyncCompleted(report.clone()));
        Ok(report)
    }

    // --- Library Queries (Pure Local SQLite) ---

    pub async fn get_track(&self, track_id: String) -> Result<Option<FfiTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let track = core.get_track(&track_id).await.map_err(FfiError::from)?;
            Ok(track.map(Into::into))
        })
        .await
    }

    pub async fn get_tracks_by_ids(&self, ids: Vec<String>) -> Result<Vec<FfiTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let tracks = core.get_tracks_by_ids(&ids).await.map_err(FfiError::from)?;
            Ok(tracks.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn get_album(&self, album_id: String) -> Result<Option<FfiAlbum>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let album = core.get_album(&album_id).await.map_err(FfiError::from)?;
            Ok(album.map(Into::into))
        })
        .await
    }

    pub async fn get_artist(&self, artist_id: String) -> Result<Option<FfiArtist>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let artist = core.get_artist(&artist_id).await.map_err(FfiError::from)?;
            Ok(artist.map(Into::into))
        })
        .await
    }

    pub async fn list_artists(&self) -> Result<Vec<FfiArtist>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let artists = core.list_artists().await.map_err(FfiError::from)?;
            Ok(artists.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_albums(&self, artist_id: Option<String>) -> Result<Vec<FfiAlbum>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let albums = core
                .list_albums(artist_id.as_deref())
                .await
                .map_err(FfiError::from)?;
            Ok(albums.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_tracks_by_album(&self, album_id: String) -> Result<Vec<FfiTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let tracks = core
                .list_tracks_by_album(&album_id)
                .await
                .map_err(FfiError::from)?;
            Ok(tracks.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_tracks_by_playlist(
        &self,
        playlist_id: String,
    ) -> Result<Vec<FfiTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let tracks = core
                .list_tracks_by_playlist(&playlist_id)
                .await
                .map_err(FfiError::from)?;
            Ok(tracks.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_playlists(&self) -> Result<Vec<FfiPlaylist>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let playlists = core.list_playlists().await.map_err(FfiError::from)?;
            Ok(playlists.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_starred_tracks(&self) -> Result<Vec<FfiTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let tracks = core.list_starred_tracks().await.map_err(FfiError::from)?;
            Ok(tracks.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_discovery_triage(
        &self,
        pending_only: bool,
    ) -> Result<Vec<FfiDiscoveryTrack>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let tracks = core
                .list_discovery_triage(pending_only)
                .await
                .map_err(FfiError::from)?;
            Ok(tracks.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn library_stats(&self) -> Result<FfiLibraryStats, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let stats = core.library_stats().await.map_err(FfiError::from)?;
            Ok(stats.into())
        })
        .await
    }

    // --- Playback, Cache & Cover Art ---

    pub async fn resolve_playback_source(
        &self,
        track_id: String,
        profile: FfiAudioQualityProfile,
    ) -> Result<FfiPlaybackSource, FfiError> {
        let core = self.core.clone();
        let native_profile: AudioQualityProfile = profile.into();
        self.run_async(async move {
            let src = core
                .resolve_playback_source(&track_id, &native_profile)
                .await
                .map_err(FfiError::from)?;
            Ok(src.into())
        })
        .await
    }

    pub async fn download_track(
        &self,
        track_id: String,
        profile: FfiAudioQualityProfile,
        pinned: bool,
        pin_source: Option<String>,
    ) -> Result<FfiDownloadOutcome, FfiError> {
        let core = self.core.clone();
        let native_profile: AudioQualityProfile = profile.into();
        let (outcome, stats) = self
            .run_async_cooperative(|cancel| async move {
                let entry = core
                    .download_track_cancellable(
                        &track_id,
                        &native_profile,
                        pinned,
                        pin_source.as_deref(),
                        &cancel,
                    )
                    .await
                    .map_err(FfiError::from)?;
                let stats = core.cache_stats().await.map_err(FfiError::from)?;
                Ok((FfiDownloadOutcome::from(entry), FfiCacheStats::from(stats)))
            })
            .await?;
        self.emit_cache_changed(stats);
        Ok(outcome)
    }

    pub async fn prefetch_queue(
        &self,
        queue_track_ids: Vec<String>,
        current_index: u32,
        profile: FfiAudioQualityProfile,
        is_wifi: bool,
    ) -> Result<Vec<String>, FfiError> {
        let core = self.core.clone();
        let native_profile: AudioQualityProfile = profile.into();
        let (downloaded, maybe_stats) = self
            .run_async_cooperative(|cancel| async move {
                let ids = core
                    .prefetch_queue_cancellable(
                        &queue_track_ids,
                        current_index as usize,
                        &native_profile,
                        is_wifi,
                        &cancel,
                    )
                    .await
                    .map_err(FfiError::from)?;
                let stats = if ids.is_empty() {
                    None
                } else {
                    Some(FfiCacheStats::from(
                        core.cache_stats().await.map_err(FfiError::from)?,
                    ))
                };
                Ok((ids, stats))
            })
            .await?;
        if let Some(stats) = maybe_stats {
            self.emit_cache_changed(stats);
        }
        Ok(downloaded)
    }

    pub async fn run_auto_cache_rules(
        &self,
        profile: FfiAudioQualityProfile,
        is_wifi: bool,
    ) -> Result<Vec<String>, FfiError> {
        let core = self.core.clone();
        let native_profile: AudioQualityProfile = profile.into();
        let (completed, maybe_stats) = self
            .run_async_cooperative(|cancel| async move {
                let ids = core
                    .run_auto_cache_rules_cancellable(&native_profile, is_wifi, &cancel)
                    .await
                    .map_err(FfiError::from)?;
                let stats = if ids.is_empty() {
                    None
                } else {
                    Some(FfiCacheStats::from(
                        core.cache_stats().await.map_err(FfiError::from)?,
                    ))
                };
                Ok((ids, stats))
            })
            .await?;
        if let Some(stats) = maybe_stats {
            self.emit_cache_changed(stats);
        }
        Ok(completed)
    }

    pub async fn pin(&self, target: FfiPinTarget) -> Result<Vec<String>, FfiError> {
        let core = self.core.clone();
        let native_target: PinTarget = target.into();
        let (missing, stats) = self
            .run_async_uninterruptible(async move {
                let missing = core.pin(&native_target).await.map_err(FfiError::from)?;
                let stats = core.cache_stats().await.map_err(FfiError::from)?;
                Ok((missing, FfiCacheStats::from(stats)))
            })
            .await?;
        self.emit_cache_changed(stats);
        Ok(missing)
    }

    pub async fn unpin(&self, target: FfiPinTarget) -> Result<u64, FfiError> {
        let core = self.core.clone();
        let native_target: PinTarget = target.into();
        let (evicted, stats) = self
            .run_async_uninterruptible(async move {
                let evicted = core.unpin(&native_target).await.map_err(FfiError::from)?;
                let stats = core.cache_stats().await.map_err(FfiError::from)?;
                Ok((evicted as u64, FfiCacheStats::from(stats)))
            })
            .await?;
        self.emit_cache_changed(stats);
        Ok(evicted)
    }

    pub async fn list_pinned_targets(&self) -> Result<Vec<FfiPinState>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let pins = core.list_pinned_targets().await.map_err(FfiError::from)?;
            Ok(pins.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_offline_tracks(&self) -> Result<Vec<FfiDownloadOutcome>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let rows = core.list_offline_tracks().await.map_err(FfiError::from)?;
            Ok(rows.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn cache_policy(&self) -> Result<FfiCachePolicy, FfiError> {
        let core = self.core.clone();
        self.run_async(async move { Ok(core.cache_policy().await.into()) })
            .await
    }

    pub async fn set_cache_policy(&self, policy: FfiCachePolicy) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_policy: CachePolicy = policy.into();
        let stats = self
            .run_async_uninterruptible(async move {
                core.set_cache_policy(native_policy)
                    .await
                    .map_err(FfiError::from)?;
                let stats = core.cache_stats().await.map_err(FfiError::from)?;
                Ok(FfiCacheStats::from(stats))
            })
            .await?;
        self.emit_cache_changed(stats);
        Ok(())
    }

    pub async fn enforce_cache_budget(&self) -> Result<u64, FfiError> {
        let core = self.core.clone();
        let (evicted, stats) = self
            .run_async_uninterruptible(async move {
                let evicted = core.enforce_cache_budget().await.map_err(FfiError::from)?;
                let stats = core.cache_stats().await.map_err(FfiError::from)?;
                Ok((evicted as u64, FfiCacheStats::from(stats)))
            })
            .await?;
        if evicted > 0 {
            self.emit_cache_changed(stats);
        }
        Ok(evicted)
    }

    pub async fn cache_stats(&self) -> Result<FfiCacheStats, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let stats = core.cache_stats().await.map_err(FfiError::from)?;
            Ok(stats.into())
        })
        .await
    }

    pub async fn cover_art_local_path(
        &self,
        cover_art_id: String,
    ) -> Result<Option<String>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let path = core
                .cover_art_local_path(&cover_art_id)
                .await
                .map_err(FfiError::from)?;
            Ok(path.map(|p| p.display().to_string()))
        })
        .await
    }

    pub async fn get_or_fetch_cover_art(
        &self,
        cover_art_id: String,
        size: Option<u32>,
    ) -> Result<String, FfiError> {
        let core = self.core.clone();
        let (path, maybe_stats) = self
            .run_async(async move {
                let (path, newly_fetched) = core
                    .get_or_fetch_cover_art_with_status(&cover_art_id, size)
                    .await
                    .map_err(FfiError::from)?;
                let stats = if newly_fetched {
                    Some(FfiCacheStats::from(
                        core.cache_stats().await.map_err(FfiError::from)?,
                    ))
                } else {
                    None
                };
                Ok((path.display().to_string(), stats))
            })
            .await?;
        if let Some(stats) = maybe_stats {
            self.emit_cache_changed(stats);
        }
        Ok(path)
    }

    // --- Outbox & Curation ---

    pub async fn enqueue_mutation(&self, mutation: FfiOutboxMutation) -> Result<i64, FfiError> {
        let core = self.core.clone();
        let native_mut: OutboxMutation = mutation.try_into()?;
        self.run_async_uninterruptible(async move {
            core.enqueue_mutation(native_mut)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn enqueue_mutation_with_key(
        &self,
        mutation: FfiOutboxMutation,
        idempotency_key: String,
    ) -> Result<i64, FfiError> {
        let core = self.core.clone();
        let native_mut: OutboxMutation = mutation.try_into()?;
        self.run_async_uninterruptible(async move {
            core.enqueue_mutation_with_key(native_mut, idempotency_key)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn outbox_summary(&self) -> Result<FfiOutboxSummary, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let summary = core.outbox_summary().await.map_err(FfiError::from)?;
            Ok(summary.into())
        })
        .await
    }

    pub async fn pending_outbox(&self) -> Result<Vec<FfiOutboxRow>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let rows = core.pending_outbox().await.map_err(FfiError::from)?;
            Ok(rows.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn dead_letter_outbox(&self) -> Result<Vec<FfiOutboxRow>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let rows = core.dead_letter_outbox().await.map_err(FfiError::from)?;
            Ok(rows.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn retry_dead_letters(&self) -> Result<u64, FfiError> {
        let core = self.core.clone();
        self.run_async_uninterruptible(async move {
            let n = core.retry_dead_letters().await.map_err(FfiError::from)?;
            Ok(n as u64)
        })
        .await
    }

    pub async fn drain_outbox(&self) -> Result<FfiDrainReport, FfiError> {
        let core = self.core.clone();
        let report: FfiDrainReport = self
            .run_async_cooperative(|cancel| async move {
                let rep = core
                    .drain_outbox_cancellable(&cancel)
                    .await
                    .map_err(FfiError::from)?;
                Ok(rep.into())
            })
            .await?;
        self.emit_event(CoreEvent::OutboxDrained(report.clone()));
        Ok(report)
    }

    // --- Unified Search & Android Auto ---

    pub async fn search(&self, query: String, limit: u32) -> Result<FfiSearchResults, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core
                .search(&query, limit as usize)
                .await
                .map_err(FfiError::from)?;
            Ok(res.into())
        })
        .await
    }

    pub async fn search_local(
        &self,
        query: String,
        limit: u32,
    ) -> Result<FfiSearchResults, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core
                .search_local(&query, limit as usize)
                .await
                .map_err(FfiError::from)?;
            Ok(res.into())
        })
        .await
    }

    pub async fn search_remote(
        &self,
        query: String,
        provider: Option<String>,
        limit: u32,
    ) -> Result<FfiRemoteCatalogResults, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core
                .search_remote(&query, provider.as_deref(), limit as usize)
                .await
                .map_err(FfiError::from)?;
            Ok(res.into())
        })
        .await
    }

    pub async fn get_remote_album(
        &self,
        album_id: String,
    ) -> Result<FfiRemoteAlbumDetail, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core
                .get_remote_album(&album_id)
                .await
                .map_err(FfiError::from)?;
            Ok(res.into())
        })
        .await
    }

    pub async fn get_remote_artist(
        &self,
        artist_id: String,
        name: Option<String>,
    ) -> Result<Option<FfiRemoteArtistDetail>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core
                .get_remote_artist(&artist_id, name.as_deref())
                .await
                .map_err(FfiError::from)?;
            Ok(res.map(Into::into))
        })
        .await
    }

    pub async fn download_remote_album(
        &self,
        album: FfiOnlineAlbumMatch,
    ) -> Result<String, FfiError> {
        let core = self.core.clone();
        let native_album: RemoteCatalogAlbum = album.into();
        self.run_async(async move {
            core.download_remote_album(&native_album)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn auto_children(&self, parent_id: String) -> Result<Vec<FfiAutoBrowseNode>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let nodes = core
                .auto_children(&parent_id)
                .await
                .map_err(FfiError::from)?;
            Ok(nodes.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn voice_search(
        &self,
        query: String,
    ) -> Result<Option<FfiVoiceMatchResult>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let res = core.voice_search(&query).await.map_err(FfiError::from)?;
            Ok(res.map(Into::into))
        })
        .await
    }

    pub async fn save_playback_session(
        &self,
        session: FfiPersistedPlaybackSession,
    ) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_session: PersistedPlaybackSession = session.into();
        self.run_async_uninterruptible(async move {
            core.save_playback_session(&native_session)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn load_playback_session(
        &self,
    ) -> Result<Option<FfiPersistedPlaybackSession>, FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            let session = core
                .load_playback_session()
                .await
                .map_err(FfiError::from)?;
            Ok(session.map(Into::into))
        })
        .await
    }

    // --- Multi-Device Connect WS ---

    pub async fn connect_ws(&self, heartbeat_interval_secs: u64) -> Result<(), FfiError> {
        let core = self.core.clone();
        let interval = Duration::from_secs(heartbeat_interval_secs.clamp(1, 3600));
        self.run_async(async move {
            core.connect_ws(interval).await.map_err(FfiError::from)
        })
        .await
    }

    pub async fn disconnect_ws(&self) -> Result<(), FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            core.disconnect_ws().await;
            Ok(())
        })
        .await
    }

    pub async fn connect_state(&self) -> Result<FfiConnectState, FfiError> {
        let core = self.core.clone();
        self.run_async(async move { Ok(core.connect_state().await.into()) })
            .await
    }

    pub async fn connect_update_state(&self, snapshot: FfiPlaybackSnapshot) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_snap: PlaybackSnapshotDto = snapshot.try_into()?;
        self.run_async(async move {
            core.connect_update_state(native_snap)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn connect_send_command(
        &self,
        target_device_id: String,
        command: FfiConnectCommand,
    ) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_cmd: ConnectCommandDto = command.try_into()?;
        self.run_async(async move {
            core.connect_send_command(target_device_id, native_cmd)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    pub async fn connect_claim_active(&self) -> Result<(), FfiError> {
        let core = self.core.clone();
        self.run_async(async move {
            core.connect_claim_active().await.map_err(FfiError::from)
        })
        .await
    }

    pub async fn connect_transfer_playback_to(
        &self,
        target_device_id: String,
        snapshot: FfiPlaybackSnapshot,
    ) -> Result<(), FfiError> {
        let core = self.core.clone();
        let native_snap: PlaybackSnapshotDto = snapshot.try_into()?;
        self.run_async(async move {
            core.connect_transfer_playback_to(target_device_id, native_snap)
                .await
                .map_err(FfiError::from)
        })
        .await
    }
}
