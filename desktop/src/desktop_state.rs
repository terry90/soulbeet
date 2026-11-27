//! In-process `SoulbeetCore` host, persistent desktop configuration (`0o600` on Unix),
//! monotonic OpenSubsonic scrobbler, local SQLite library queries, cover-art asset protocol
//! handler, and bridge between Dioxus `PlayerContext`, native `libmpv`, `souvlaki` MPRIS2/SMTC,
//! Connect WS, and the floating Mini-Player.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use client_core::models::{
    AudioQualityProfile, CachePolicy, CacheStats, ClientConfig, ConnectCommandDto,
    ConnectDeviceDto, ConnectServerEventDto, DevicePlatform as CoreDevicePlatform, LocalAlbum,
    LocalDiscoveryTrack, LocalPlaylist, LocalTrack, NavidromeCredentials, OutboxMutation,
    PersistedPlaybackSession, PinTarget, PlaybackSnapshotDto, PlaybackSource, RemoteCatalogAlbum,
    RemoteCatalogTrack, SoulbeetAuth, SyncReport,
};
use client_core::SoulbeetCore;
use directories::ProjectDirs;
use dioxus::prelude::*;
use dioxus_desktop::wry::http::{header, Response as HttpResponse, StatusCode};
use dioxus_desktop::{use_asset_handler, DesktopContext};
use serde::{Deserialize, Serialize};
use shared::client::{
    ClientBootstrap, ConnectDevice, DeleteGuardPreview, DevicePlatform as SharedDevicePlatform,
    PlaybackSnapshot, TrackLyrics,
};
use shared::download::{
    AcquireAndPlayInitialStatus, AcquireAndPlayRequest, AcquireAndPlayResponse,
};
use shared::navidrome::{DiscoveryProgress, GenerationStatus, ProfilePhase};
use shared::recommendation::{ContextualMixSummary, CultureSteering, SteerSeasonalPackRequest};
use tokio::sync::{broadcast, mpsc, watch};
use ui::{
    classify_guard_mode, classify_local_playlist_category, discovery_days_remaining,
    percent_encode_param, AcquireStageBadge, DrawerTab, GuardModalMode, PendingDropItem,
    PendingDropQueue, PlaybackStatus, PlayerCommand, PlayerContext, PlayerState, PlayerTrack,
    SidebarPlaylistItem, StudioNavSection,
};
use uuid::Uuid;

use crate::audio_mpv::{
    ensure_c_numeric_locale, AudioTelemetry, MpvCommand, MpvEngine, MpvEvent, MpvPlaybackSource,
    MpvPlaybackState, ReplayGainMode,
};
use crate::media_controls::{
    extract_windows_hwnd, MediaControlsBridge, MediaKeyAction, MediaTrackMeta, OsMediaCommand,
};
use crate::mini_player::open_mini_player_window;

/// Authentication scheme persisted for Soulbeet server calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAuthKind {
    #[default]
    Bearer,
    SessionCookie,
}

/// Strips redundant `"Bearer "` / `"auth_token="` prefixes users frequently paste into token fields.
pub fn normalize_auth_token(raw: &str, kind: DesktopAuthKind) -> String {
    let trimmed = raw.trim();
    match kind {
        DesktopAuthKind::Bearer => trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
            .unwrap_or(trimmed)
            .trim()
            .to_string(),
        DesktopAuthKind::SessionCookie => trimmed
            .strip_prefix("auth_token=")
            .or_else(|| trimmed.strip_prefix("Cookie: auth_token="))
            .unwrap_or(trimmed)
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

/// Extracts the `auth_token` cookie value from a `Set-Cookie` HTTP response header.
pub fn extract_auth_cookie_from_header(set_cookie: &str) -> Option<String> {
    let pair = set_cookie.split(';').next()?.trim();
    let val = pair.strip_prefix("auth_token=")?.trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Result of authenticating with `POST {base}/api/auth/login`.
#[derive(Debug, Clone)]
pub struct PasswordLoginResult {
    pub session_cookie: String,
    pub password_change_required: bool,
}

/// Authenticates against a Soulbeet server with username + password and returns the session cookie.
pub async fn login_with_password(
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<PasswordLoginResult, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Enter the Soulbeet server URL first.".to_string());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let url = format!("{base}/api/auth/login");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "username": username.trim(),
            "password": password,
        }))
        .send()
        .await
        .map_err(|e| format!("Could not reach Soulbeet server: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let detail = body.trim();
        return Err(if detail.is_empty() {
            format!("Sign-in failed (HTTP {})", status.as_u16())
        } else {
            format!("Sign-in failed (HTTP {}): {detail}", status.as_u16())
        });
    }

    let cookie = resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(extract_auth_cookie_from_header)
        .ok_or_else(|| {
            "Server succeeded but did not issue an auth_token session cookie.".to_string()
        })?;

    #[derive(Deserialize)]
    struct LoginBody {
        #[serde(default)]
        password_change_required: bool,
    }
    let body: LoginBody = resp.json().await.unwrap_or(LoginBody {
        password_change_required: false,
    });

    Ok(PasswordLoginResult {
        session_cookie: cookie,
        password_change_required: body.password_change_required,
    })
}

/// Verifies a candidate [`DesktopConfig`] against Soulbeet (`/api/client/bootstrap`) and/or
/// Navidrome (`/rest/ping`) BEFORE persisting `config.json`.
pub async fn verify_desktop_config(
    cfg: &DesktopConfig,
) -> Result<Option<ClientBootstrap>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    if let Some(base) = cfg
        .soulbeet_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let url = format!("{}/api/client/bootstrap", base.trim_end_matches('/'));
        let mut req = client.get(&url);
        if let Some(auth) = cfg.soulbeet_auth() {
            let (k, v) = auth.header_name_and_value();
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("Could not reach Soulbeet server: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Soulbeet rejected credentials (HTTP {}): {}",
                status.as_u16(),
                body.trim()
            ));
        }
        let bootstrap: ClientBootstrap = resp
            .json()
            .await
            .map_err(|e| format!("Invalid bootstrap response from Soulbeet: {e}"))?;
        return Ok(Some(bootstrap));
    }

    if let Some(ref nav) = cfg.navidrome {
        let mut url = url::Url::parse(&format!("{}/rest/ping", nav.normalized_base_url()))
            .map_err(|e| format!("Invalid Navidrome URL: {e}"))?;
        url.query_pairs_mut()
            .append_pair("u", &nav.username)
            .append_pair("t", &nav.subsonic_token)
            .append_pair("s", &nav.subsonic_salt)
            .append_pair("v", &nav.api_version)
            .append_pair("c", &nav.client_name)
            .append_pair("f", "json");
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Could not reach Navidrome: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "Navidrome returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid Navidrome ping JSON: {e}"))?;
        let ok = val
            .pointer("/subsonic-response/status")
            .and_then(|v| v.as_str())
            == Some("ok");
        if !ok {
            let msg = val
                .pointer("/subsonic-response/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("Navidrome rejected Subsonic credentials");
            return Err(msg.to_string());
        }
        return Ok(None);
    }

    Err("Enter a Soulbeet server or direct Navidrome credentials.".to_string())
}

/// Persistent desktop preferences & server credentials stored in platform config dir
/// (`~/.config/Soulbeet/config.json` on Linux with `0o600` permissions,
/// `%APPDATA%/soulbeet/Soulbeet/config.json` on Windows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopConfig {
    #[serde(default)]
    pub soulbeet_base_url: Option<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub auth_kind: DesktopAuthKind,
    #[serde(default)]
    pub navidrome: Option<NavidromeCredentials>,
    #[serde(default = "default_device_id")]
    pub device_id: String,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    #[serde(default)]
    pub replaygain: ReplayGainMode,
    #[serde(default)]
    pub oled_mode: bool,
    #[serde(default)]
    pub offline_only: bool,
    #[serde(default = "default_cache_max_gb")]
    pub cache_max_gb: u64,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub cover_art_dir: Option<PathBuf>,
}

fn default_device_id() -> String {
    format!("desktop-{}", Uuid::new_v4())
}

fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .map(|h| format!("Soulbeet Desktop ({h})"))
        .unwrap_or_else(|| "Soulbeet Desktop".to_string())
}

fn default_cache_max_gb() -> u64 {
    8
}

fn default_volume() -> f32 {
    0.85
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            soulbeet_base_url: None,
            auth_token: None,
            auth_kind: DesktopAuthKind::Bearer,
            navidrome: None,
            device_id: default_device_id(),
            device_name: default_device_name(),
            replaygain: ReplayGainMode::Track,
            oled_mode: false,
            offline_only: false,
            cache_max_gb: default_cache_max_gb(),
            volume: default_volume(),
            db_path: None,
            cache_dir: None,
            cover_art_dir: None,
        }
    }
}

impl DesktopConfig {
    /// Returns true once the user has configured either Navidrome directly or a Soulbeet server.
    pub fn is_configured(&self) -> bool {
        let has_navidrome = self
            .navidrome
            .as_ref()
            .is_some_and(|n| !n.base_url.trim().is_empty() && !n.username.trim().is_empty());
        let has_soulbeet = self
            .soulbeet_base_url
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
            && self
                .auth_token
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
        has_navidrome || has_soulbeet
    }

    pub fn soulbeet_auth(&self) -> Option<SoulbeetAuth> {
        let raw = self.auth_token.as_deref()?;
        let token = normalize_auth_token(raw, self.auth_kind);
        if token.is_empty() {
            return None;
        }
        Some(match self.auth_kind {
            DesktopAuthKind::Bearer => SoulbeetAuth::Bearer(token),
            DesktopAuthKind::SessionCookie => SoulbeetAuth::SessionCookie(token),
        })
    }

    pub fn resolved_paths(&self) -> (PathBuf, PathBuf, PathBuf) {
        let (default_data, default_cache) = default_app_dirs();
        let db_path = self
            .db_path
            .clone()
            .unwrap_or_else(|| default_data.join("soulbeet-client.db"));
        let cache_dir = self
            .cache_dir
            .clone()
            .unwrap_or_else(|| default_cache.join("audio"));
        let cover_art_dir = self
            .cover_art_dir
            .clone()
            .unwrap_or_else(|| default_cache.join("cover-art"));
        (db_path, cache_dir, cover_art_dir)
    }

    pub fn to_client_config(&self) -> ClientConfig {
        let (db_path, cache_dir, cover_art_dir) = self.resolved_paths();
        let max_bytes = self.cache_max_gb.clamp(1, 512) * 1024 * 1024 * 1024;
        ClientConfig {
            db_path,
            cache_dir,
            cover_art_dir,
            soulbeet_base_url: self
                .soulbeet_base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_end_matches('/').to_string()),
            soulbeet_auth: self.soulbeet_auth(),
            navidrome: self.navidrome.clone(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            platform: CoreDevicePlatform::Desktop,
            offline_only: self.offline_only,
            cache_policy: CachePolicy {
                max_bytes,
                wifi_only_auto_cache: false,
                prefetch_queue_count: 3,
                ..CachePolicy::default()
            },
        }
    }

    pub fn config_file_path() -> PathBuf {
        if let Some(dirs) = ProjectDirs::from("org", "soulbeet", "Soulbeet") {
            dirs.config_dir().join("config.json")
        } else {
            PathBuf::from(".soulbeet-desktop-config.json")
        }
    }

    pub fn load_from_disk() -> Self {
        let path = Self::config_file_path();
        Self::load_from_path(&path).unwrap_or_default()
    }

    pub fn save_to_disk(&self) -> Result<(), String> {
        let path = Self::config_file_path();
        self.save_to_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read config: {e}"))?;
        let mut cfg: Self =
            serde_json::from_str(&raw).map_err(|e| format!("parse config JSON: {e}"))?;
        if cfg.device_id.trim().is_empty() {
            cfg.device_id = default_device_id();
        }
        if cfg.device_name.trim().is_empty() {
            cfg.device_name = default_device_name();
        }
        cfg.volume = if cfg.volume.is_finite() {
            cfg.volume.clamp(0.0, 1.0)
        } else {
            default_volume()
        };
        Ok(cfg)
    }

    /// Writes config atomically with strict owner-only permissions (`0o700` dir, `0o600` file)
    /// on Unix so persisted authentication tokens are never world-readable.
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create config dir: {e}"))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize config JSON: {e}"))?;
        let tmp = path.with_extension("json.tmp");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("open temp config: {e}"))?;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            file.write_all(json.as_bytes())
                .map_err(|e| format!("write temp config: {e}"))?;
            file.flush()
                .map_err(|e| format!("flush temp config: {e}"))?;
        }

        #[cfg(not(unix))]
        {
            let mut file = std::fs::File::create(&tmp)
                .map_err(|e| format!("create temp config: {e}"))?;
            file.write_all(json.as_bytes())
                .map_err(|e| format!("write temp config: {e}"))?;
            file.flush()
                .map_err(|e| format!("flush temp config: {e}"))?;
        }

        std::fs::rename(&tmp, path).map_err(|e| format!("commit config: {e}"))?;
        Ok(())
    }
}

fn default_app_dirs() -> (PathBuf, PathBuf) {
    if let Some(dirs) = ProjectDirs::from("org", "soulbeet", "Soulbeet") {
        (
            dirs.data_local_dir().to_path_buf(),
            dirs.cache_dir().to_path_buf(),
        )
    } else {
        (
            PathBuf::from(".soulbeet-data"),
            PathBuf::from(".soulbeet-cache"),
        )
    }
}

/// Deterministic monotonic listening-time scrobbler following OpenSubsonic / Last.fm rules:
/// - Tracks shorter than 30 seconds are never submitted as scrobbles.
/// - Threshold is `clamp(duration / 2.0, 30.0, 240.0)`.
/// - Accumulates ONLY real monotonic wall-clock time elapsed while actively playing
///   (seeking forward or pausing never inflates elapsed listening time).
#[derive(Debug, Clone)]
pub struct ScrobbleTracker {
    active_song_id: Option<String>,
    duration_secs: f64,
    accumulated_secs: f64,
    last_playing_tick: Option<Instant>,
    started_at_unix: i64,
    submitted: bool,
}

impl Default for ScrobbleTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrobbleTracker {
    pub fn new() -> Self {
        Self {
            active_song_id: None,
            duration_secs: 0.0,
            accumulated_secs: 0.0,
            last_playing_tick: None,
            started_at_unix: 0,
            submitted: false,
        }
    }

    pub fn scrobble_threshold_secs(duration_secs: f64) -> Option<f64> {
        if !duration_secs.is_finite() || duration_secs < 30.0 {
            return None;
        }
        Some((duration_secs * 0.5).clamp(30.0, 240.0))
    }

    /// Starts tracking a new song and returns a `NowPlaying` (`submission: false`) mutation.
    pub fn start_track(
        &mut self,
        song_id: String,
        duration_secs: f64,
        started_at_unix: i64,
    ) -> Option<OutboxMutation> {
        if song_id.trim().is_empty() {
            self.reset();
            return None;
        }
        self.active_song_id = Some(song_id.clone());
        self.duration_secs = if duration_secs.is_finite() {
            duration_secs.max(0.0)
        } else {
            0.0
        };
        self.accumulated_secs = 0.0;
        self.last_playing_tick = None;
        self.started_at_unix = started_at_unix;
        self.submitted = false;

        Some(OutboxMutation::Scrobble {
            song_id,
            submission: false,
            played_at_unix: started_at_unix,
        })
    }

    /// Advances monotonic elapsed time on state / progress ticks.
    /// Returns `Some(OutboxMutation::Scrobble { submission: true, .. })` at most once per track.
    pub fn tick(
        &mut self,
        now: Instant,
        is_playing: bool,
        observed_duration_secs: f64,
    ) -> Option<OutboxMutation> {
        if observed_duration_secs.is_finite() && observed_duration_secs > 0.0 {
            self.duration_secs = observed_duration_secs;
        }

        if !is_playing {
            self.last_playing_tick = None;
            return None;
        }

        if let Some(prev) = self.last_playing_tick {
            let delta = now.saturating_duration_since(prev).as_secs_f64();
            if delta > 0.0 && delta <= 5.0 {
                self.accumulated_secs += delta;
            }
        }
        self.last_playing_tick = Some(now);

        if self.submitted {
            return None;
        }
        let song_id = self.active_song_id.as_ref()?;
        let threshold = Self::scrobble_threshold_secs(self.duration_secs)?;
        if self.accumulated_secs >= threshold {
            self.submitted = true;
            return Some(OutboxMutation::Scrobble {
                song_id: song_id.clone(),
                submission: true,
                played_at_unix: self.started_at_unix,
            });
        }
        None
    }

    #[cfg(test)]
    pub fn accumulated_secs(&self) -> f64 {
        self.accumulated_secs
    }

    #[cfg(test)]
    pub fn is_submitted(&self) -> bool {
        self.submitted
    }

    pub fn reset(&mut self) {
        self.active_song_id = None;
        self.duration_secs = 0.0;
        self.accumulated_secs = 0.0;
        self.last_playing_tick = None;
        self.submitted = false;
    }
}

/// Fail-closed DeleteGuardPreview used while auditing or when the guard endpoint is unreachable.
pub fn fail_closed_guard_preview(song_id: String, reason: String) -> DeleteGuardPreview {
    DeleteGuardPreview {
        song_id,
        is_pending_discovery: false,
        discovery_track_id: None,
        can_delete: false,
        sole_owner: false,
        other_listeners: Vec::new(),
        own_playlists: Vec::new(),
        block_reason: Some(reason),
    }
}

/// Filters a track list for playback when `offline_only == true` so album/playlist Play buttons
/// never attempt to stream uncached remote tracks.
pub fn filter_tracks_for_offline_play(
    tracks: Vec<PlayerTrack>,
    offline_only: bool,
) -> Vec<PlayerTrack> {
    if offline_only {
        tracks.into_iter().filter(|t| t.is_offline_cached).collect()
    } else {
        tracks
    }
}

/// Live reactive snapshot pushed from the main Studio window to the `360×110` Mini-Player.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MiniPlayerSnapshot {
    pub current_track: Option<PlayerTrack>,
    pub status: PlaybackStatus,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub oled_mode: bool,
    pub offline_only: bool,
}

/// User actions sent from the floating Mini-Player window back to the main Studio controller.
#[derive(Debug, Clone, PartialEq)]
pub enum MiniPlayerAction {
    TogglePlayPause,
    Next,
    Previous,
    Seek(f64),
    Star(PlayerTrack, bool),
    Keep(PlayerTrack),
    Drop(PlayerTrack),
    RestoreStudio,
    Closed(u64),
}

/// Cloneable channel handle passed into the secondary Mini-Player `VirtualDom`.
#[derive(Clone)]
pub struct MiniPlayerBridge {
    pub generation: u64,
    pub snapshot_rx: watch::Receiver<MiniPlayerSnapshot>,
    pub action_tx: mpsc::UnboundedSender<MiniPlayerAction>,
    pub core: Option<Arc<SoulbeetCore>>,
}

/// Background sync / outbox status rendered in Library & Activity panels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SyncUiStatus {
    pub syncing: bool,
    pub last_report: Option<SyncReport>,
    pub last_sync_unix: Option<i64>,
    pub last_error: Option<String>,
    pub pending_outbox_count: usize,
    pub dead_letter_count: usize,
    pub connect_online: bool,
}

/// Aggregated local SQLite snapshot for the Home view.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HomeSnapshot {
    pub recent_albums: Vec<LocalAlbum>,
    pub starred_tracks: Vec<PlayerTrack>,
    pub recent_tracks: Vec<PlayerTrack>,
    pub pending_discovery_count: usize,
    pub total_track_count: usize,
    pub cached_track_count: usize,
}

/// Local SQLite playlist summary enriched with offline-pin state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopPlaylist {
    pub playlist: LocalPlaylist,
    pub is_pinned: bool,
}

/// Builds a webview-safe local cover-art URL routed through `use_asset_handler("cover-art", ...)`.
pub fn desktop_cover_art_url(cover_art_id: Option<&str>) -> Option<String> {
    let id = cover_art_id?.trim();
    if id.is_empty() {
        return None;
    }
    Some(format!("/cover-art/{}", percent_encode_param(id)))
}

/// Registers the cross-platform `cover-art` asset handler on the current `dioxus-desktop` window.
pub fn use_cover_art_asset_handler(core_getter: impl Fn() -> Option<Arc<SoulbeetCore>> + 'static) {
    use_asset_handler("cover-art", move |request, responder| {
        let path = request.uri().path().to_string();
        let raw_id = path
            .strip_prefix("/cover-art/")
            .unwrap_or("")
            .trim_matches('/');
        let cover_id = percent_decode(raw_id);
        let core_opt = core_getter();

        tokio::spawn(async move {
            let Some(core) = core_opt else {
                responder.respond(not_found_response());
                return;
            };
            if cover_id.is_empty() {
                responder.respond(not_found_response());
                return;
            }
            let art_path = match core
                .cache()
                .get_or_fetch_cover_art(&cover_id, Some(320), core.is_offline_only())
                .await
            {
                Ok(p) => p,
                Err(_) => {
                    responder.respond(not_found_response());
                    return;
                }
            };
            match tokio::fs::read(&art_path).await {
                Ok(bytes) => {
                    let mime = match art_path.extension().and_then(|e| e.to_str()) {
                        Some("png") => "image/png",
                        Some("webp") => "image/webp",
                        _ => "image/jpeg",
                    };
                    let resp = HttpResponse::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime)
                        .header(header::CACHE_CONTROL, "public, max-age=86400")
                        .body(bytes)
                        .unwrap_or_else(|_| not_found_response());
                    responder.respond(resp);
                }
                Err(_) => responder.respond(not_found_response()),
            }
        });
    });
}

fn not_found_response() -> HttpResponse<Vec<u8>> {
    HttpResponse::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Vec::new())
        .expect("valid 404 response")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Converts a `client_core::LocalTrack` into a shared UI `PlayerTrack`.
pub fn local_track_to_player_track(
    track: &LocalTrack,
    cached_ids: &HashSet<String>,
    discovery_by_song: &HashMap<String, LocalDiscoveryTrack>,
) -> PlayerTrack {
    let disc = discovery_by_song.get(&track.id);
    let is_discovery = track.is_discovery || disc.is_some_and(|d| d.status == "pending");
    let days_remaining = disc.and_then(|d| {
        compute_discovery_days_remaining(d.expires_at.as_deref(), d.created_at.as_deref())
    });
    let reason_label = disc.and_then(|d| extract_reason_label(d.reason_json.as_deref()));
    let codec_badge = format_local_codec_badge(track.suffix.as_deref(), track.bit_rate);

    PlayerTrack {
        id: track.id.clone(),
        title: track.title.clone(),
        artist: track.artist_name.clone(),
        album: (!track.album_name.trim().is_empty()).then(|| track.album_name.clone()),
        cover_art_url: desktop_cover_art_url(track.cover_art_id.as_deref()),
        cover_art_id: track.cover_art_id.clone(),
        duration_secs: u32::try_from(track.duration_secs.max(0)).ok(),
        is_discovery,
        discovery_track_id: disc.map(|d| d.id.clone()),
        days_remaining,
        starred: track.starred,
        rating: u8::try_from(track.user_rating.clamp(0, 5)).unwrap_or(0),
        codec_badge,
        is_offline_cached: cached_ids.contains(&track.id),
        reason_label,
    }
}

/// Converts a pending `LocalDiscoveryTrack` (optionally joined with a `LocalTrack`) into `PlayerTrack`.
pub fn discovery_to_player_track(
    disc: &LocalDiscoveryTrack,
    local_track: Option<&LocalTrack>,
    cached_ids: &HashSet<String>,
) -> PlayerTrack {
    let song_id = disc
        .song_id
        .clone()
        .or_else(|| local_track.map(|t| t.id.clone()))
        .unwrap_or_else(|| disc.id.clone());
    let cover_art_id = local_track.and_then(|t| t.cover_art_id.clone());
    let codec_badge = local_track
        .and_then(|t| format_local_codec_badge(t.suffix.as_deref(), t.bit_rate));

    PlayerTrack {
        id: song_id.clone(),
        title: disc.title.clone(),
        artist: disc.artist.clone(),
        album: disc
            .album
            .clone()
            .or_else(|| local_track.map(|t| t.album_name.clone())),
        cover_art_url: desktop_cover_art_url(cover_art_id.as_deref()),
        cover_art_id,
        duration_secs: local_track.and_then(|t| u32::try_from(t.duration_secs.max(0)).ok()),
        is_discovery: disc.status == "pending",
        discovery_track_id: Some(disc.id.clone()),
        days_remaining: compute_discovery_days_remaining(
            disc.expires_at.as_deref(),
            disc.created_at.as_deref(),
        ),
        starred: local_track.is_some_and(|t| t.starred),
        rating: local_track
            .and_then(|t| u8::try_from(t.user_rating.clamp(0, 5)).ok())
            .unwrap_or(0),
        codec_badge,
        is_offline_cached: cached_ids.contains(&song_id),
        reason_label: extract_reason_label(disc.reason_json.as_deref()),
    }
}

fn compute_discovery_days_remaining(
    expires_at: Option<&str>,
    created_at: Option<&str>,
) -> Option<u32> {
    if let Some(exp) = expires_at.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(exp)
            .map(|d| d.with_timezone(&Utc))
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(exp, "%Y-%m-%d %H:%M:%S")
                    .map(|n| n.and_utc())
            })
        {
            let days = dt.signed_duration_since(Utc::now()).num_days().max(0);
            return u32::try_from(days).ok();
        }
    }
    created_at.and_then(|c| discovery_days_remaining(c, 14))
}

fn format_local_codec_badge(suffix: Option<&str>, bit_rate: Option<i64>) -> Option<String> {
    let ext = suffix?.trim().to_ascii_uppercase();
    if ext.is_empty() {
        return None;
    }
    if matches!(ext.as_str(), "FLAC" | "ALAC" | "WAV" | "WV" | "APE") {
        Some(ext)
    } else if let Some(br) = bit_rate.filter(|&b| b > 0) {
        Some(format!("{ext} {br}"))
    } else {
        Some(ext)
    }
}

fn extract_reason_label(reason_json: Option<&str>) -> Option<String> {
    let raw = reason_json?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(s) = val
            .get("summary")
            .or_else(|| val.get("label"))
            .or_else(|| val.get("reason"))
            .and_then(|v| v.as_str())
        {
            if !s.trim().is_empty() {
                return Some(s.trim().to_string());
            }
        }
        if let Some(seed) = val.get("seed_artist").and_then(|v| v.as_str()) {
            return Some(format!("Similar to {seed}"));
        }
    }
    None
}

// --- Direct Sub-Millisecond SQLite Mirror Queries ---

pub async fn load_cached_track_ids(core: &SoulbeetCore) -> HashSet<String> {
    sqlx::query_scalar::<_, String>("SELECT track_id FROM offline_downloads")
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

pub async fn load_pinned_targets(core: &SoulbeetCore) -> HashSet<(String, String)> {
    sqlx::query_as::<_, (String, String)>("SELECT target_kind, target_id FROM offline_pins")
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

pub async fn load_pending_discovery_map(
    core: &SoulbeetCore,
) -> HashMap<String, LocalDiscoveryTrack> {
    let rows = sqlx::query_as::<_, LocalDiscoveryTrack>(
        "SELECT * FROM discovery_tracks WHERE status = 'pending' ORDER BY updated_at DESC",
    )
    .fetch_all(core.pool())
    .await
    .unwrap_or_default();

    let mut map = HashMap::new();
    for row in rows {
        if let Some(ref sid) = row.song_id {
            map.insert(sid.clone(), row);
        }
    }
    map
}

pub async fn load_local_top_genres(core: &SoulbeetCore) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT genre
        FROM tracks
        WHERE genre IS NOT NULL AND TRIM(genre) != ''
        GROUP BY LOWER(TRIM(genre))
        ORDER BY COUNT(1) DESC
        LIMIT 8
        "#,
    )
    .fetch_all(core.pool())
    .await
    .unwrap_or_default()
}

pub async fn load_home_snapshot(core: &SoulbeetCore, offline_only: bool) -> HomeSnapshot {
    let cached_ids = load_cached_track_ids(core).await;
    let disc_map = load_pending_discovery_map(core).await;

    let recent_albums = if offline_only {
        sqlx::query_as::<_, LocalAlbum>(
            r#"
            SELECT DISTINCT a.*
            FROM albums a
            INNER JOIN tracks t ON t.album_id = a.id
            INNER JOIN offline_downloads od ON od.track_id = t.id
            ORDER BY COALESCE(a.created_at, '') DESC, a.updated_at DESC
            LIMIT 12
            "#,
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, LocalAlbum>(
            "SELECT * FROM albums ORDER BY COALESCE(created_at, '') DESC, updated_at DESC LIMIT 12",
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    };

    let starred_rows = if offline_only {
        sqlx::query_as::<_, LocalTrack>(
            r#"
            SELECT t.* FROM tracks t
            INNER JOIN offline_downloads od ON od.track_id = t.id
            WHERE t.starred = 1
            ORDER BY t.updated_at DESC
            LIMIT 24
            "#,
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, LocalTrack>(
            "SELECT * FROM tracks WHERE starred = 1 ORDER BY updated_at DESC LIMIT 24",
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    };

    let recent_rows = if offline_only {
        sqlx::query_as::<_, LocalTrack>(
            r#"
            SELECT t.* FROM tracks t
            INNER JOIN offline_downloads od ON od.track_id = t.id
            ORDER BY COALESCE(t.last_played_at, 0) DESC, t.updated_at DESC
            LIMIT 24
            "#,
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, LocalTrack>(
            "SELECT * FROM tracks ORDER BY COALESCE(last_played_at, 0) DESC, t.updated_at DESC LIMIT 24",
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    };

    let total_track_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM tracks")
        .fetch_one(core.pool())
        .await
        .unwrap_or(0);
    let pending_discovery_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(1) FROM discovery_tracks WHERE status = 'pending'",
    )
    .fetch_one(core.pool())
    .await
    .unwrap_or(0);

    HomeSnapshot {
        recent_albums,
        starred_tracks: starred_rows
            .iter()
            .map(|t| local_track_to_player_track(t, &cached_ids, &disc_map))
            .collect(),
        recent_tracks: recent_rows
            .iter()
            .map(|t| local_track_to_player_track(t, &cached_ids, &disc_map))
            .collect(),
        pending_discovery_count: pending_discovery_count.max(0) as usize,
        total_track_count: total_track_count.max(0) as usize,
        cached_track_count: cached_ids.len(),
    }
}

pub async fn load_pending_discovery_tracks(
    core: &SoulbeetCore,
    offline_only: bool,
) -> Vec<PlayerTrack> {
    let cached_ids = load_cached_track_ids(core).await;
    let rows = sqlx::query_as::<_, LocalDiscoveryTrack>(
        "SELECT * FROM discovery_tracks WHERE status = 'pending' ORDER BY updated_at DESC LIMIT 100",
    )
    .fetch_all(core.pool())
    .await
    .unwrap_or_default();

    let song_ids: Vec<String> = rows.iter().filter_map(|r| r.song_id.clone()).collect();
    let local_tracks: HashMap<String, LocalTrack> =
        client_core::db::get_tracks_by_ids(core.pool(), &song_ids)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();

    rows.iter()
        .map(|d| {
            let local = d.song_id.as_ref().and_then(|sid| local_tracks.get(sid));
            discovery_to_player_track(d, local, &cached_ids)
        })
        .filter(|t| !offline_only || t.is_offline_cached)
        .collect()
}

pub async fn load_desktop_playlists(core: &SoulbeetCore) -> Vec<DesktopPlaylist> {
    let pins = load_pinned_targets(core).await;
    let rows = sqlx::query_as::<_, LocalPlaylist>(
        "SELECT * FROM playlists ORDER BY CASE kind WHEN 'seasonal' THEN 0 WHEN 'discovery' THEN 1 ELSE 2 END, name COLLATE NOCASE ASC",
    )
    .fetch_all(core.pool())
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|playlist| {
            let is_pinned = pins.contains(&("playlist".to_string(), playlist.id.clone()))
                || (playlist.kind == "discovery"
                    && pins.contains(&("weekly_radar".to_string(), "weekly_radar".to_string())))
                || playlist.slug.as_ref().is_some_and(|slug| {
                    playlist.kind == "seasonal"
                        && pins.contains(&("seasonal_pack".to_string(), slug.clone()))
                });
            DesktopPlaylist {
                playlist,
                is_pinned,
            }
        })
        .collect()
}

pub async fn load_library_albums(core: &SoulbeetCore, offline_only: bool) -> Vec<(LocalAlbum, bool)> {
    let pins = load_pinned_targets(core).await;
    let rows = if offline_only {
        sqlx::query_as::<_, LocalAlbum>(
            r#"
            SELECT DISTINCT a.*
            FROM albums a
            INNER JOIN tracks t ON t.album_id = a.id
            INNER JOIN offline_downloads od ON od.track_id = t.id
            ORDER BY a.name COLLATE NOCASE ASC
            LIMIT 200
            "#,
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, LocalAlbum>(
            "SELECT * FROM albums ORDER BY name COLLATE NOCASE ASC LIMIT 200",
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    };

    rows.into_iter()
        .map(|alb| {
            let pinned = pins.contains(&("album".to_string(), alb.id.clone()));
            (alb, pinned)
        })
        .collect()
}

pub async fn load_library_tracks(core: &SoulbeetCore, offline_only: bool) -> Vec<PlayerTrack> {
    let cached_ids = load_cached_track_ids(core).await;
    let disc_map = load_pending_discovery_map(core).await;
    let rows = if offline_only {
        sqlx::query_as::<_, LocalTrack>(
            r#"
            SELECT t.* FROM tracks t
            INNER JOIN offline_downloads od ON od.track_id = t.id
            ORDER BY t.artist_name COLLATE NOCASE ASC, t.album_name COLLATE NOCASE ASC, COALESCE(t.track_number, 0) ASC
            LIMIT 250
            "#,
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, LocalTrack>(
            "SELECT * FROM tracks ORDER BY artist_name COLLATE NOCASE ASC, album_name COLLATE NOCASE ASC, COALESCE(track_number, 0) ASC LIMIT 250",
        )
        .fetch_all(core.pool())
        .await
        .unwrap_or_default()
    };

    rows.iter()
        .map(|t| local_track_to_player_track(t, &cached_ids, &disc_map))
        .collect()
}

pub async fn load_tracks_for_album(core: &SoulbeetCore, album_id: &str) -> Vec<PlayerTrack> {
    let cached_ids = load_cached_track_ids(core).await;
    let disc_map = load_pending_discovery_map(core).await;
    let rows = sqlx::query_as::<_, LocalTrack>(
        "SELECT * FROM tracks WHERE album_id = ? ORDER BY COALESCE(disc_number, 1) ASC, COALESCE(track_number, 0) ASC",
    )
    .bind(album_id)
    .fetch_all(core.pool())
    .await
    .unwrap_or_default();

    rows.iter()
        .map(|t| local_track_to_player_track(t, &cached_ids, &disc_map))
        .collect()
}

pub async fn load_tracks_for_playlist(core: &SoulbeetCore, playlist_id: &str) -> Vec<PlayerTrack> {
    let cached_ids = load_cached_track_ids(core).await;
    let disc_map = load_pending_discovery_map(core).await;
    let rows = client_core::db::list_tracks_for_playlist(core.pool(), playlist_id)
        .await
        .unwrap_or_default();

    rows.iter()
        .map(|t| local_track_to_player_track(t, &cached_ids, &disc_map))
        .collect()
}

pub fn convert_connect_devices(devices: &[ConnectDeviceDto]) -> Vec<ConnectDevice> {
    devices
        .iter()
        .map(|d| ConnectDevice {
            device_id: d.device_id.clone(),
            name: d.name.clone(),
            platform: match d.platform {
                CoreDevicePlatform::Web => SharedDevicePlatform::Web,
                CoreDevicePlatform::Desktop => SharedDevicePlatform::Desktop,
                CoreDevicePlatform::Android => SharedDevicePlatform::Android,
                CoreDevicePlatform::AndroidAuto => SharedDevicePlatform::AndroidAuto,
            },
            is_active: d.is_active,
            last_seen_unix: d.last_seen_unix,
            snapshot: d.snapshot.as_ref().map(|s| PlaybackSnapshot {
                song_id: s.song_id.clone(),
                title: s.title.clone(),
                artist: s.artist.clone(),
                album: s.album.clone(),
                cover_art_id: s.cover_art_id.clone(),
                is_playing: s.is_playing,
                position_ms: s.position_ms,
                duration_ms: s.duration_ms,
                queue_song_ids: s.queue_song_ids.clone(),
                queue_index: s.queue_index,
                volume: s.volume,
                is_pending_discovery: s.is_pending_discovery,
                updated_at_unix: s.updated_at_unix,
            }),
        })
        .collect()
}

/// Compares two Connect device slices ignoring volatile keepalive timestamps (`last_seen_unix`
/// and snapshot `updated_at_unix`) so periodic heartbeats do not cause redundant re-renders.
pub fn connect_devices_visibly_equal(a: &[ConnectDevice], b: &[ConnectDevice]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.device_id == y.device_id
                && x.name == y.name
                && x.platform == y.platform
                && x.is_active == y.is_active
                && match (x.snapshot.as_ref(), y.snapshot.as_ref()) {
                    (None, None) => true,
                    (Some(sx), Some(sy)) => {
                        sx.song_id == sy.song_id
                            && sx.title == sy.title
                            && sx.artist == sy.artist
                            && sx.album == sy.album
                            && sx.cover_art_id == sy.cover_art_id
                            && sx.is_playing == sy.is_playing
                            && sx.position_ms == sy.position_ms
                            && sx.duration_ms == sy.duration_ms
                            && sx.queue_song_ids == sy.queue_song_ids
                            && sx.queue_index == sy.queue_index
                            && sx.volume.to_bits() == sy.volume.to_bits()
                            && sx.is_pending_discovery == sy.is_pending_discovery
                    }
                    _ => false,
                }
        })
}

/// Live UI state for 1-tap remote album downloads triggered from Desktop Search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAlbumDownloadState {
    Downloading,
    Queued { batch_id: String },
    Failed(String),
}

/// Shared Dioxus context controlling the entire Desktop Studio app.
#[derive(Clone, Copy)]
pub struct DesktopStudio {
    pub config: Signal<DesktopConfig>,
    pub bootstrap: Signal<Option<ClientBootstrap>>,
    pub core: Signal<Option<Arc<SoulbeetCore>>>,
    pub core_error: Signal<Option<String>>,
    pub active_section: Signal<StudioNavSection>,
    pub selected_playlist_id: Signal<Option<String>>,
    pub search_seed_query: Signal<String>,
    pub active_drawer: Signal<Option<DrawerTab>>,
    pub inspected_track: Signal<Option<PlayerTrack>>,
    pub cmdk_open: Signal<bool>,
    pub mini_player_open: Signal<bool>,
    pub mini_window: Signal<Option<DesktopContext>>,
    pub mini_gen: Signal<Arc<AtomicU64>>,
    pub telemetry: Signal<Option<AudioTelemetry>>,
    pub sync_status: Signal<SyncUiStatus>,
    pub cache_stats: Signal<Option<CacheStats>>,
    pub playlists: Signal<Vec<DesktopPlaylist>>,
    pub pending_drops: Signal<PendingDropQueue>,
    pub in_flight_triage: Signal<HashSet<String>>,
    pub optimistic_kept: Signal<HashSet<String>>,
    pub discovery_generating: Signal<bool>,
    pub discovery_progress_label: Signal<Option<String>>,
    pub discovery_progress_pct: Signal<Option<u8>>,
    pub discovery_refresh_error: Signal<Option<String>>,
    pub discovery_poll_gen: Signal<Arc<AtomicU64>>,
    pub acquiring_tracks: Signal<HashMap<String, AcquireStageBadge>>,
    pub downloading_albums: Signal<HashMap<String, RemoteAlbumDownloadState>>,
    pub connect_devices: Signal<Vec<ConnectDevice>>,
    pub lyrics: Signal<Vec<TrackLyrics>>,
    pub lyrics_loading: Signal<bool>,
    pub lyrics_error: Signal<Option<String>>,
    pub guard_modal_track: Signal<Option<PlayerTrack>>,
    pub guard_preview: Signal<Option<DeleteGuardPreview>>,
    pub guard_loading: Signal<bool>,
    pub status_toast: Signal<Option<String>>,
    pub library_revision: Signal<u64>,
    pub mpv_cmd_tx: Signal<Option<mpsc::Sender<MpvCommand>>>,
    pub mpv_error: Signal<Option<String>>,
    pub load_gen: Signal<Arc<AtomicU64>>,
    pub connect_gen: Signal<Arc<AtomicU64>>,
    pub volume_debounce_gen: Signal<Arc<AtomicU64>>,
    pub mini_snapshot_tx: Signal<Arc<watch::Sender<MiniPlayerSnapshot>>>,
    pub mini_action_tx: Signal<mpsc::UnboundedSender<MiniPlayerAction>>,
    pub media_controls: Signal<Arc<Mutex<MediaControlsBridge>>>,
    pub scrobbler: Signal<Arc<Mutex<ScrobbleTracker>>>,
}

/// Builds all non-empty deduplicated triage guard keys (`discovery_track_id` and `song_id`).
fn triage_guard_keys(disc_id: Option<&str>, song_id: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(did) = disc_id.filter(|s| !s.is_empty()) {
        keys.push(did.to_string());
    }
    if !song_id.is_empty() && !keys.iter().any(|k| k == song_id) {
        keys.push(song_id.to_string());
    }
    keys
}

/// Computes a human-readable status label and completion percentage for `DiscoveryProgress`.
fn desktop_discovery_progress_summary(progress: &DiscoveryProgress) -> (String, u8) {
    if progress.profiles.is_empty() {
        return ("Preparing recommendations...".to_string(), 8);
    }
    let total_profiles = progress.profiles.len() as f64;
    let mut completed_units = 0.0f64;
    let mut active_label: Option<String> = None;

    for pp in &progress.profiles {
        match pp.phase {
            ProfilePhase::Done | ProfilePhase::Skipped => {
                completed_units += 1.0;
            }
            ProfilePhase::Waiting => {}
            _ => {
                let frac = if pp.total > 0 {
                    (pp.current as f64 / pp.total as f64).clamp(0.0, 1.0)
                } else {
                    0.25
                };
                completed_units += frac;
                if active_label.is_none() {
                    if pp.total > 0 {
                        active_label = Some(format!(
                            "{}: {} ({}/{})",
                            pp.profile, pp.phase, pp.current, pp.total
                        ));
                    } else {
                        active_label = Some(format!("{}: {}", pp.profile, pp.phase));
                    }
                }
            }
        }
    }

    let pct = ((completed_units / total_profiles) * 100.0)
        .round()
        .clamp(6.0, 98.0) as u8;
    let label = active_label.unwrap_or_else(|| "Generating fresh discovery tracks...".to_string());
    (label, pct)
}

/// Parses a Soulbeet server error body (Dioxus `ErrorPayload` JSON or raw text) and flags
/// `already_running` so callers can attach cleanly to an in-flight discovery run.
fn parse_server_error_message(body: &str, status: u16) -> (String, bool) {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        let code = val.pointer("/data/code").and_then(|v| v.as_str());
        let msg = val
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let already_running = code == Some("already_running")
            || msg.is_some_and(|m| m.to_ascii_lowercase().contains("already in progress"));
        let display = msg
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("Server returned HTTP {status}"));
        return (display, already_running);
    }
    let trimmed = body.trim();
    let already_running = trimmed.to_ascii_lowercase().contains("already in progress");
    let display = if trimmed.is_empty() {
        format!("Server returned HTTP {status}")
    } else {
        format!("HTTP {status}: {trimmed}")
    };
    (display, already_running)
}

/// Fetches `GET {base}/api/discovery/progress` using the configured Soulbeet auth header.
async fn fetch_discovery_progress(
    client: &reqwest::Client,
    base: &str,
    auth: Option<&SoulbeetAuth>,
) -> Result<Option<DiscoveryProgress>, String> {
    let url = format!("{}/api/discovery/progress", base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(a) = auth {
        let (k, v) = a.header_name_and_value();
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Could not reach Soulbeet server: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let (msg, _) = parse_server_error_message(&body, status.as_u16());
        return Err(msg);
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed reading discovery progress: {e}"))?;
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }
    serde_json::from_str::<Option<DiscoveryProgress>>(trimmed)
        .map_err(|e| format!("Invalid discovery progress JSON: {e}"))
}

pub fn use_desktop_studio() -> DesktopStudio {
    use_context::<DesktopStudio>()
}

impl DesktopStudio {
    pub fn bump_library(&mut self) {
        let next = self.library_revision.peek().wrapping_add(1);
        self.library_revision.set(next);
    }

    #[allow(dead_code)]
    pub fn promote_threshold(&self) -> u8 {
        self.bootstrap
            .peek()
            .as_ref()
            .map(|b| b.promote_threshold.clamp(1, 5))
            .unwrap_or(4)
    }

    pub fn sidebar_playlists(&self) -> Vec<SidebarPlaylistItem> {
        self.playlists
            .read()
            .iter()
            .filter_map(|p| {
                let song_count = u32::try_from(p.playlist.song_count.max(0)).unwrap_or(0);
                let category = classify_local_playlist_category(
                    &p.playlist.kind,
                    p.playlist.comment.as_deref(),
                    p.playlist.is_smart,
                );
                if category == ui::PlaylistCategory::Seasonal && song_count == 0 {
                    return None;
                }
                Some(SidebarPlaylistItem {
                    id: p.playlist.id.clone(),
                    name: p.playlist.name.clone(),
                    song_count,
                    is_smart: p.playlist.is_smart,
                    is_offline_pinned: p.is_pinned,
                    category,
                })
            })
            .collect()
    }

    pub fn toggle_drawer(&mut self, tab: DrawerTab) {
        let current = *self.active_drawer.peek();
        if current == Some(tab) {
            self.active_drawer.set(None);
        } else {
            self.active_drawer.set(Some(tab));
        }
    }

    pub fn inspect_track(&mut self, track: PlayerTrack) {
        self.inspected_track.set(Some(track));
        self.active_drawer.set(Some(DrawerTab::Inspector));
    }

    /// Plays `tracks` respecting `player.offline_only()` so album/playlist Play buttons never
    /// attempt to stream uncached tracks while offline.
    pub fn play_collection(
        &mut self,
        player: PlayerContext,
        tracks: Vec<PlayerTrack>,
        start_index: usize,
        label: &str,
    ) {
        let offline = player.offline_only();
        let filtered = filter_tracks_for_offline_play(tracks, offline);
        if filtered.is_empty() {
            self.status_toast.set(Some(if offline {
                format!("No tracks in \"{label}\" are cached offline — pin it first")
            } else {
                format!("No playable tracks in \"{label}\"")
            }));
            return;
        }
        let idx = start_index.min(filtered.len() - 1);
        player.play_queue(filtered, idx);
    }

    pub fn push_mini_snapshot(&self, player: PlayerContext) {
        let st = player.state_signal().peek().clone();
        let pos = *player.position_signal().peek();
        let dur = *player.duration_signal().peek();
        let oled_mode = self.config.peek().oled_mode;
        let snap = MiniPlayerSnapshot {
            current_track: st.current_track,
            status: st.status,
            position_secs: pos,
            duration_secs: dur,
            oled_mode,
            offline_only: st.offline_only,
        };
        let _ = self.mini_snapshot_tx.peek().send(snap);
    }

    pub fn send_mpv(&self, cmd: MpvCommand) {
        if let Some(tx) = self.mpv_cmd_tx.peek().as_ref() {
            if let Err(err) = tx.try_send(cmd) {
                tracing::warn!("failed to send mpv command: {err}");
            }
        }
    }

    /// Resolves the next upcoming track in the queue (matching `shuffle` and `repeat` identically)
    /// and preloads it into `libmpv` for zero-gap transitions.
    pub fn schedule_gapless_prefetch(&self, player: PlayerContext) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let st = player.state_signal().peek().clone();
        let Some(cur_idx) = st.queue_index else {
            self.send_mpv(MpvCommand::ClearPrefetch);
            return;
        };
        let mut probe = st.clone();
        let mut probe_pos = 0.0;
        let mut probe_dur = 0.0;
        probe.apply_local_command(&PlayerCommand::TrackEnded, &mut probe_pos, &mut probe_dur);
        let next_track = if probe.status == PlaybackStatus::Loading {
            probe.current_track
        } else {
            None
        };

        let queue_ids: Vec<String> = st.queue.iter().map(|t| t.id.clone()).collect();
        let mpv_tx = self.mpv_cmd_tx.peek().clone();
        let offline_only = core.is_offline_only();

        spawn(async move {
            if let Some(tx) = mpv_tx {
                if let Some(track) = next_track {
                    match core
                        .resolve_playback_source(&track.id, &AudioQualityProfile::LosslessFlac)
                        .await
                    {
                        Ok(source) => {
                            let mpv_src = playback_source_to_mpv(source);
                            let _ = tx.try_send(MpvCommand::PrefetchNext { source: mpv_src });
                        }
                        Err(_) => {
                            let _ = tx.try_send(MpvCommand::ClearPrefetch);
                        }
                    }
                } else {
                    let _ = tx.try_send(MpvCommand::ClearPrefetch);
                }
            }
            if !offline_only && !queue_ids.is_empty() {
                let _ = core
                    .cache()
                    .prefetch_queue(&queue_ids, cur_idx, &AudioQualityProfile::LosslessFlac, true)
                    .await;
            }
        });
    }

    /// Loads `track` into `libmpv` using `client-core`'s offline-first resolver, guarded by a
    /// monotonic `load_gen` token so rapid track switching never lets a stale load clobber the
    /// active track. Supports `start_paused` and `start_position_secs` for session restoration
    /// and Connect `TransferPlayback`.
    pub fn load_current_track_in_mpv(
        &mut self,
        player: PlayerContext,
        track: PlayerTrack,
        start_paused: bool,
        start_position_secs: Option<f64>,
    ) {
        let Some(core) = self.core.peek().clone() else {
            let mut st = player.state_signal();
            st.write().status =
                PlaybackStatus::Error("Configure Soulbeet or Navidrome first".into());
            return;
        };
        let my_gen = self.load_gen.peek().fetch_add(1, Ordering::SeqCst) + 1;
        let load_gen_arc = self.load_gen.peek().clone();
        let studio = *self;
        let mpv_tx = self.mpv_cmd_tx.peek().clone();
        let queue_ids: Vec<String> = player
            .state_signal()
            .peek()
            .queue
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let cur_idx = player.state_signal().peek().queue_index.unwrap_or(0);
        let init_pos_ms = start_position_secs
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        // Start monotonic scrobbler for this track (skip NowPlaying if loading paused on restore)
        let now_playing = {
            let sc_arc = self.scrobbler.peek().clone();
            let mut sc = sc_arc.lock().unwrap_or_else(|e| e.into_inner());
            let np = sc.start_track(
                track.id.clone(),
                track.duration_secs.map(f64::from).unwrap_or(0.0),
                Utc::now().timestamp(),
            );
            if start_paused {
                None
            } else {
                np
            }
        };

        spawn(async move {
            if let Some(np) = now_playing {
                let _ = core.enqueue_mutation(np).await;
            }

            let _ = core
                .save_playback_session(&PersistedPlaybackSession {
                    queue_track_ids: queue_ids,
                    current_index: cur_idx,
                    position_ms: init_pos_ms,
                    is_playing: !start_paused,
                    source_label: track.album.clone(),
                    updated_at: Utc::now().timestamp(),
                })
                .await;

            match core
                .resolve_playback_source(&track.id, &AudioQualityProfile::LosslessFlac)
                .await
            {
                Ok(source) => {
                    if load_gen_arc.load(Ordering::SeqCst) != my_gen {
                        return;
                    }
                    let mpv_src = playback_source_to_mpv(source);
                    if let Some(tx) = mpv_tx {
                        let _ = tx.try_send(MpvCommand::LoadTrack {
                            source: mpv_src,
                            start_paused,
                            start_position_secs,
                        });
                    }
                    let local_art = match track.cover_art_id.as_deref() {
                        Some(cid) => core
                            .cache()
                            .get_or_fetch_cover_art(cid, Some(320), core.is_offline_only())
                            .await
                            .ok(),
                        None => None,
                    };
                    if load_gen_arc.load(Ordering::SeqCst) != my_gen {
                        return;
                    }
                    if let Ok(mut mc) = studio.media_controls.peek().lock() {
                        mc.update_metadata(&MediaTrackMeta {
                            title: track.title.clone(),
                            artist: track.artist.clone(),
                            album: track.album.clone(),
                            duration_secs: track.duration_secs,
                            local_cover_art_path: local_art,
                            fallback_cover_url: None,
                        });
                        mc.update_playback(
                            if start_paused {
                                &PlaybackStatus::Paused
                            } else {
                                &PlaybackStatus::Playing
                            },
                            start_position_secs.unwrap_or(0.0),
                        );
                    }
                    studio.schedule_gapless_prefetch(player);
                    studio.publish_connect_snapshot(player);
                }
                Err(err) => {
                    if load_gen_arc.load(Ordering::SeqCst) != my_gen {
                        return;
                    }
                    tracing::warn!("Could not resolve playback source for {}: {err}", track.id);
                    let mut st = player.state_signal();
                    st.write().status = PlaybackStatus::Error(err.to_string());
                    studio.push_mini_snapshot(player);
                }
            }
        });
    }

    /// Central handler for `PlayerCommand` dispatched by any shared Studio UI component.
    pub fn handle_player_command(&mut self, mut player: PlayerContext, cmd: PlayerCommand) {
        let (prev_track_id, prev_queue_index, prev_status) = {
            let st_sig = player.state_signal();
            let st = st_sig.peek();
            (
                st.current_track.as_ref().map(|t| t.id.clone()),
                st.queue_index,
                st.status.clone(),
            )
        };

        let mut pos = *player.position_signal().peek();
        let mut dur = *player.duration_signal().peek();
        {
            let mut st_sig = player.state_signal();
            st_sig
                .write()
                .apply_local_command(&cmd, &mut pos, &mut dur);
        }
        player.set_progress(pos, dur);

        let next_state = player.state_signal().peek().clone();
        let next_track = next_state.current_track.clone();
        let next_track_id = next_track.as_ref().map(|t| t.id.clone());
        let mut publish_connect_now = true;

        match cmd {
            PlayerCommand::PlayTrack(_) | PlayerCommand::PlayQueue { .. } => {
                if let Some(track) = next_track {
                    self.load_current_track_in_mpv(player, track, false, None);
                } else {
                    self.load_gen.peek().fetch_add(1, Ordering::SeqCst);
                    self.send_mpv(MpvCommand::Stop);
                }
            }
            PlayerCommand::LoadQueue { autoplay, .. } => {
                if let Some(track) = next_track {
                    self.load_current_track_in_mpv(player, track, !autoplay, None);
                } else {
                    self.load_gen.peek().fetch_add(1, Ordering::SeqCst);
                    self.send_mpv(MpvCommand::Stop);
                }
            }
            PlayerCommand::TogglePlayPause => match next_state.status {
                PlaybackStatus::Paused => {
                    self.send_mpv(MpvCommand::Pause);
                    if let Ok(mut mc) = self.media_controls.peek().lock() {
                        mc.update_playback(&PlaybackStatus::Paused, pos);
                    }
                }
                PlaybackStatus::Playing => {
                    self.send_mpv(MpvCommand::Play);
                    if let Ok(mut mc) = self.media_controls.peek().lock() {
                        mc.update_playback(&PlaybackStatus::Playing, pos);
                    }
                }
                PlaybackStatus::Loading => {
                    if let Some(track) = next_track {
                        self.load_current_track_in_mpv(player, track, false, None);
                    }
                }
                _ => {}
            },
            PlayerCommand::Next | PlayerCommand::TrackEnded | PlayerCommand::Previous => {
                let track_changed = next_track_id != prev_track_id;
                let restarted_same = next_state.status == PlaybackStatus::Loading
                    && !track_changed
                    && next_track.is_some();
                if track_changed || restarted_same {
                    if let Some(track) = next_track {
                        self.load_current_track_in_mpv(player, track, false, None);
                    }
                } else if matches!(next_state.status, PlaybackStatus::Paused)
                    && matches!(prev_status, PlaybackStatus::Playing)
                {
                    self.send_mpv(MpvCommand::Pause);
                } else if pos == 0.0 {
                    self.send_mpv(MpvCommand::Seek(0.0));
                }
            }
            PlayerCommand::Seek(secs) => {
                self.send_mpv(MpvCommand::Seek(secs));
            }
            PlayerCommand::SetVolume(vol) => {
                publish_connect_now = false;
                let clamped = vol.clamp(0.0, 1.0);
                self.send_mpv(MpvCommand::SetVolume(clamped));
                self.send_mpv(MpvCommand::SetMute(false));
                if let Ok(mut mc) = self.media_controls.peek().lock() {
                    mc.update_volume(clamped);
                }
                self.config.write().volume = clamped;
                // Debounce disk write + Connect WS broadcast (~300ms) while dragging volume slider.
                let deb_gen = self.volume_debounce_gen.peek().fetch_add(1, Ordering::SeqCst) + 1;
                let deb_arc = self.volume_debounce_gen.peek().clone();
                let studio = *self;
                spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if deb_arc.load(Ordering::SeqCst) == deb_gen {
                        let _ = studio.config.peek().save_to_disk();
                        studio.publish_connect_snapshot(player);
                    }
                });
            }
            PlayerCommand::ToggleMute => {
                self.send_mpv(MpvCommand::SetMute(next_state.muted));
            }
            PlayerCommand::ToggleShuffle
            | PlayerCommand::CycleRepeat
            | PlayerCommand::EnqueueNext(_)
            | PlayerCommand::EnqueueLast(_) => {
                self.schedule_gapless_prefetch(player);
            }
            PlayerCommand::RemoveFromQueue(idx) => {
                let active_slot_removed = prev_queue_index == Some(idx);
                if next_track_id != prev_track_id || active_slot_removed {
                    if let Some(track) = next_track {
                        self.load_current_track_in_mpv(
                            player,
                            track,
                            !next_state.status.is_active(),
                            None,
                        );
                    } else {
                        self.load_gen.peek().fetch_add(1, Ordering::SeqCst);
                        self.send_mpv(MpvCommand::Stop);
                        if let Ok(mut sc) = self.scrobbler.peek().lock() {
                            sc.reset();
                        }
                        if let Ok(mut mc) = self.media_controls.peek().lock() {
                            mc.update_playback(&PlaybackStatus::Idle, 0.0);
                        }
                    }
                } else {
                    self.schedule_gapless_prefetch(player);
                }
            }
            PlayerCommand::RemoveTrackFromQueue(ref track_id) => {
                let active_slot_removed =
                    !track_id.is_empty() && prev_track_id.as_deref() == Some(track_id.as_str());
                if next_track_id != prev_track_id || active_slot_removed {
                    if let Some(track) = next_track {
                        self.load_current_track_in_mpv(
                            player,
                            track,
                            !next_state.status.is_active(),
                            None,
                        );
                    } else {
                        self.load_gen.peek().fetch_add(1, Ordering::SeqCst);
                        self.send_mpv(MpvCommand::Stop);
                        if let Ok(mut sc) = self.scrobbler.peek().lock() {
                            sc.reset();
                        }
                        if let Ok(mut mc) = self.media_controls.peek().lock() {
                            mc.update_playback(&PlaybackStatus::Idle, 0.0);
                        }
                    }
                } else {
                    self.schedule_gapless_prefetch(player);
                }
            }
            PlayerCommand::ClearQueue => {
                self.load_gen.peek().fetch_add(1, Ordering::SeqCst);
                self.send_mpv(MpvCommand::Stop);
                if let Ok(mut sc) = self.scrobbler.peek().lock() {
                    sc.reset();
                }
                if let Ok(mut mc) = self.media_controls.peek().lock() {
                    mc.update_playback(&PlaybackStatus::Idle, 0.0);
                }
            }
            PlayerCommand::SetOfflineOnly(enabled) => {
                self.apply_offline_only(enabled, player);
            }
        }

        self.push_mini_snapshot(player);
        if publish_connect_now {
            self.publish_connect_snapshot(player);
        }
    }

    /// Handles a seamless gapless transition emitted by `libmpv` without re-issuing `LoadTrack`.
    pub fn handle_gapless_advance(&mut self, mut player: PlayerContext) {
        let mut pos = *player.position_signal().peek();
        let mut dur = *player.duration_signal().peek();
        {
            let mut st_sig = player.state_signal();
            let mut st = st_sig.write();
            st.apply_local_command(&PlayerCommand::TrackEnded, &mut pos, &mut dur);
            if st.current_track.is_some() {
                st.status = PlaybackStatus::Playing;
            }
        }
        player.set_progress(0.0, dur);

        if let Some(track) = player.state_signal().peek().current_track.clone() {
            let now_playing = {
                let sc_arc = self.scrobbler.peek().clone();
                let mut sc = sc_arc.lock().unwrap_or_else(|e| e.into_inner());
                sc.start_track(
                    track.id.clone(),
                    track.duration_secs.map(f64::from).unwrap_or(0.0),
                    Utc::now().timestamp(),
                )
            };
            let core_opt = self.core.peek().clone();
            let mc_arc = self.media_controls.peek().clone();
            let queue_ids: Vec<String> = player
                .state_signal()
                .peek()
                .queue
                .iter()
                .map(|t| t.id.clone())
                .collect();
            let cur_idx = player.state_signal().peek().queue_index.unwrap_or(0);
            spawn(async move {
                if let Some(core) = core_opt {
                    if let Some(np) = now_playing {
                        let _ = core.enqueue_mutation(np).await;
                    }
                    let _ = core
                        .save_playback_session(&PersistedPlaybackSession {
                            queue_track_ids: queue_ids,
                            current_index: cur_idx,
                            position_ms: 0,
                            is_playing: true,
                            source_label: track.album.clone(),
                            updated_at: Utc::now().timestamp(),
                        })
                        .await;
                    let local_art = match track.cover_art_id.as_deref() {
                        Some(cid) => core
                            .cache()
                            .get_or_fetch_cover_art(cid, Some(320), core.is_offline_only())
                            .await
                            .ok(),
                        None => None,
                    };
                    if let Ok(mut mc) = mc_arc.lock() {
                        mc.update_metadata(&MediaTrackMeta {
                            title: track.title,
                            artist: track.artist,
                            album: track.album,
                            duration_secs: track.duration_secs,
                            local_cover_art_path: local_art,
                            fallback_cover_url: None,
                        });
                        mc.update_playback(&PlaybackStatus::Playing, 0.0);
                    }
                }
            });
            self.schedule_gapless_prefetch(player);
        }

        self.push_mini_snapshot(player);
        self.publish_connect_snapshot(player);
    }

    pub fn apply_offline_only(&mut self, enabled: bool, player: PlayerContext) {
        self.config.write().offline_only = enabled;
        let _ = self.config.peek().save_to_disk();
        let core_opt = self.core.peek().clone();
        if let Some(ref core) = core_opt {
            core.set_offline_only(enabled);
        }
        self.bump_library();

        if enabled {
            // Immediately sever Connect WS and cancel any reconnect supervisor loop.
            self.connect_gen.peek().fetch_add(1, Ordering::SeqCst);
            self.sync_status.write().connect_online = false;
            self.connect_devices.set(Vec::new());
            self.acquiring_tracks.write().clear();
            self.downloading_albums.write().clear();
            if let Some(core) = core_opt {
                spawn(async move {
                    core.connect_client().disconnect().await;
                });
            }
        } else {
            self.drain_outbox_bg();
            if let Some(core) = core_opt {
                if self.config.peek().soulbeet_base_url.is_some() {
                    self.start_supervised_connect(core, player);
                }
            }
        }
    }

    /// Connects once to `client-core`'s internal Connect WS supervisor and drives `connect_online`
    /// and `connect_devices` reactively from `subscribe_state()` without racing `client-core`'s
    /// built-in exponential-backoff reconnect loop.
    pub fn start_supervised_connect(&mut self, core: Arc<SoulbeetCore>, player: PlayerContext) {
        let my_gen = self.connect_gen.peek().fetch_add(1, Ordering::SeqCst) + 1;
        let gen_arc = self.connect_gen.peek().clone();
        let mut studio = *self;

        spawn(async move {
            let mut event_rx = core.connect_client().subscribe();
            let mut state_rx = core.connect_client().subscribe_state();

            // Retry ONLY the very first `connect_ws` dial until `client-core`'s own internal
            // reconnect supervisor starts cleanly.
            let mut backoff_secs = 2u64;
            loop {
                if gen_arc.load(Ordering::SeqCst) != my_gen || core.is_offline_only() {
                    return;
                }
                match core.connect_ws(Duration::from_secs(20)).await {
                    Ok(()) => break,
                    Err(err) => {
                        tracing::debug!("Initial Connect WS dial failed: {err}");
                        if studio.sync_status.peek().connect_online {
                            studio.sync_status.write().connect_online = false;
                        }
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(30);
                    }
                }
            }

            let apply_state = |studio: &mut DesktopStudio,
                               player: PlayerContext,
                               snap: &client_core::connect::ConnectState| {
                if studio.sync_status.peek().connect_online != snap.connected {
                    studio.sync_status.write().connect_online = snap.connected;
                }
                let next_devices = convert_connect_devices(&snap.devices);
                let active_name = next_devices
                    .iter()
                    .find(|d| d.is_active)
                    .map(|d| d.name.clone());
                if player.state_signal().peek().active_device_name != active_name {
                    player.state_signal().write().active_device_name = active_name;
                }
                if !connect_devices_visibly_equal(&studio.connect_devices.peek(), &next_devices) {
                    studio.connect_devices.set(next_devices);
                }
            };

            let initial_state = state_rx.borrow_and_update().clone();
            apply_state(&mut studio, player, &initial_state);

            loop {
                if gen_arc.load(Ordering::SeqCst) != my_gen || core.is_offline_only() {
                    if core.is_offline_only() {
                        core.connect_client().disconnect().await;
                    }
                    break;
                }

                tokio::select! {
                    changed = state_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        if gen_arc.load(Ordering::SeqCst) != my_gen || core.is_offline_only() {
                            if core.is_offline_only() {
                                core.connect_client().disconnect().await;
                            }
                            break;
                        }
                        let snap = state_rx.borrow_and_update().clone();
                        apply_state(&mut studio, player, &snap);
                    }
                    ev_res = event_rx.recv() => {
                        match ev_res {
                            Ok(ConnectServerEventDto::Command { command, .. }) => {
                                apply_connect_command(studio, player, &core, command).await;
                            }
                            Ok(ConnectServerEventDto::DeviceList { .. })
                            | Ok(ConnectServerEventDto::PlaybackUpdated { .. }) => {
                                // Driven authoritatively by `state_rx.changed()` above.
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::debug!("Connect WS lagged {n} events; continuing");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }

            if gen_arc.load(Ordering::SeqCst) == my_gen {
                if studio.sync_status.peek().connect_online {
                    studio.sync_status.write().connect_online = false;
                }
                if !studio.connect_devices.peek().is_empty() {
                    studio.connect_devices.set(Vec::new());
                }
            }
        });
    }

    pub fn set_replaygain(&mut self, mode: ReplayGainMode) {
        self.config.write().replaygain = mode;
        let _ = self.config.peek().save_to_disk();
        self.send_mpv(MpvCommand::SetReplayGain(mode));
    }

    pub fn set_oled_mode(&mut self, enabled: bool, player: PlayerContext) {
        self.config.write().oled_mode = enabled;
        let _ = self.config.peek().save_to_disk();
        self.push_mini_snapshot(player);
    }

    // --- Offline-First Outbox Curation Actions ---

    fn finalize_inspected_promoted_discovery(
        &mut self,
        song_id: &str,
        discovery_track_id: Option<&str>,
    ) {
        let mut inspected = self.inspected_track.write();
        if let Some(t) = inspected.as_mut() {
            let matches = (!song_id.is_empty() && t.id == song_id)
                || discovery_track_id
                    .filter(|d| !d.is_empty())
                    .is_some_and(|did| t.discovery_track_id.as_deref() == Some(did));
            if matches {
                t.is_discovery = false;
                t.discovery_track_id = None;
                t.days_remaining = None;
            }
        }
    }

    pub fn keep_track(&mut self, player: PlayerContext, track: PlayerTrack) {
        let disc_id = track.discovery_track_id.clone();
        let song_id = track.id.clone();
        let prev_is_discovery = track.is_discovery;
        let prev_days = track.days_remaining;
        let guard_keys = triage_guard_keys(disc_id.as_deref(), &song_id);

        // Synchronous dual-key in-flight guard blocks rapid double-clicks across surfaces
        if guard_keys
            .iter()
            .any(|k| self.in_flight_triage.peek().contains(k))
        {
            return;
        }
        if !guard_keys.is_empty() {
            let mut in_flight = self.in_flight_triage.write();
            for key in &guard_keys {
                in_flight.insert(key.clone());
            }
        }
        {
            let mut kept = self.optimistic_kept.write();
            for key in &guard_keys {
                kept.insert(key.clone());
            }
        }

        // Immediate optimistic UI update (never stamps fake star ratings!)
        // Preserve `discovery_track_id` until outbox confirmation so rollback always matches.
        if !song_id.is_empty() {
            player
                .state_signal()
                .write()
                .apply_curation_update(&song_id, None, None, true);
        }
        if let Some(ref did) = disc_id {
            if !did.is_empty() && *did != song_id {
                player
                    .state_signal()
                    .write()
                    .apply_curation_update(did, None, None, true);
            }
        }
        {
            let mut inspected = self.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!song_id.is_empty() && t.id == song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.is_discovery = false;
                    t.days_remaining = None;
                }
            }
        }
        self.push_mini_snapshot(player);

        let Some(core) = self.core.peek().clone() else {
            {
                let mut kept = self.optimistic_kept.write();
                for key in &guard_keys {
                    kept.remove(key);
                }
            }
            if !guard_keys.is_empty() {
                let mut in_flight = self.in_flight_triage.write();
                for key in &guard_keys {
                    in_flight.remove(key);
                }
            }
            if prev_is_discovery {
                player.state_signal().write().restore_discovery_state(
                    &song_id,
                    disc_id.as_deref(),
                    prev_days,
                );
                let mut inspected = self.inspected_track.write();
                if let Some(t) = inspected.as_mut() {
                    let matches = (!song_id.is_empty() && t.id == song_id)
                        || (disc_id.is_some() && t.discovery_track_id == disc_id);
                    if matches {
                        t.is_discovery = true;
                        t.discovery_track_id = disc_id.clone();
                        t.days_remaining = prev_days;
                    }
                }
            }
            self.push_mini_snapshot(player);
            return;
        };
        let mut studio = *self;
        spawn(async move {
            let res = core
                .enqueue_mutation(OutboxMutation::Keep {
                    song_id: song_id.clone(),
                    discovery_track_id: disc_id.clone(),
                    target_folder_id: None,
                })
                .await;
            if let Err(err) = res {
                {
                    let mut kept = studio.optimistic_kept.write();
                    for key in &guard_keys {
                        kept.remove(key);
                    }
                }
                if !guard_keys.is_empty() {
                    let mut in_flight = studio.in_flight_triage.write();
                    for key in &guard_keys {
                        in_flight.remove(key);
                    }
                }
                if prev_is_discovery {
                    player.state_signal().write().restore_discovery_state(
                        &song_id,
                        disc_id.as_deref(),
                        prev_days,
                    );
                    let mut inspected = studio.inspected_track.write();
                    if let Some(t) = inspected.as_mut() {
                        let matches = (!song_id.is_empty() && t.id == song_id)
                            || (disc_id.is_some() && t.discovery_track_id == disc_id);
                        if matches {
                            t.is_discovery = true;
                            t.discovery_track_id = disc_id.clone();
                            t.days_remaining = prev_days;
                        }
                    }
                }
                studio.push_mini_snapshot(player);
                studio.status_toast.set(Some(format!("Keep failed: {err}")));
                return;
            }
            player
                .state_signal()
                .write()
                .finalize_promoted_discovery(&song_id, disc_id.as_deref());
            studio.finalize_inspected_promoted_discovery(&song_id, disc_id.as_deref());
            studio.push_mini_snapshot(player);
            studio.status_toast.set(Some(format!(
                "Kept \"{}\" in permanent library",
                track.title
            )));
            studio.bump_library();
            studio.refresh_outbox_counts().await;
            {
                let mut kept = studio.optimistic_kept.write();
                for key in &guard_keys {
                    kept.remove(key);
                }
            }
            if !guard_keys.is_empty() {
                let mut in_flight = studio.in_flight_triage.write();
                for key in &guard_keys {
                    in_flight.remove(key);
                }
            }
            if !core.is_offline_only() {
                let _ = core.drain_outbox().await;
                studio.refresh_outbox_counts().await;
            }
        });
    }

    pub fn star_track(&mut self, player: PlayerContext, track: PlayerTrack, starred: bool) {
        let promoted = starred && track.is_discovery;
        let disc_id = track.discovery_track_id.clone();
        let song_id = track.id.clone();
        let prev_days = track.days_remaining;
        let guard_keys = triage_guard_keys(disc_id.as_deref(), &song_id);

        if promoted {
            if guard_keys
                .iter()
                .any(|k| self.in_flight_triage.peek().contains(k))
            {
                return;
            }
            if !guard_keys.is_empty() {
                let mut in_flight = self.in_flight_triage.write();
                for key in &guard_keys {
                    in_flight.insert(key.clone());
                }
            }
            let mut kept = self.optimistic_kept.write();
            for key in &guard_keys {
                kept.insert(key.clone());
            }
        }

        // Never stamps fake star ratings! Preserve `discovery_track_id` until confirmed.
        if !song_id.is_empty() {
            player.state_signal().write().apply_curation_update(
                &song_id,
                Some(starred),
                None,
                promoted,
            );
        }
        if promoted {
            if let Some(ref did) = disc_id {
                if !did.is_empty() && *did != song_id {
                    player.state_signal().write().apply_curation_update(
                        did,
                        Some(starred),
                        None,
                        true,
                    );
                }
            }
        }
        {
            let mut inspected = self.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!song_id.is_empty() && t.id == song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.starred = starred;
                    if promoted {
                        t.is_discovery = false;
                        t.days_remaining = None;
                    }
                }
            }
        }
        self.push_mini_snapshot(player);

        let Some(core) = self.core.peek().clone() else {
            if !song_id.is_empty() {
                player.state_signal().write().apply_curation_update(
                    &song_id,
                    Some(!starred),
                    None,
                    false,
                );
            } else if let Some(ref did) = disc_id {
                if !did.is_empty() {
                    player.state_signal().write().apply_curation_update(
                        did,
                        Some(!starred),
                        None,
                        false,
                    );
                }
            }
            if promoted {
                {
                    let mut kept = self.optimistic_kept.write();
                    for key in &guard_keys {
                        kept.remove(key);
                    }
                }
                if !guard_keys.is_empty() {
                    let mut in_flight = self.in_flight_triage.write();
                    for key in &guard_keys {
                        in_flight.remove(key);
                    }
                }
                player.state_signal().write().restore_discovery_state(
                    &song_id,
                    disc_id.as_deref(),
                    prev_days,
                );
            }
            {
                let mut inspected = self.inspected_track.write();
                if let Some(t) = inspected.as_mut() {
                    let matches = (!song_id.is_empty() && t.id == song_id)
                        || (disc_id.is_some() && t.discovery_track_id == disc_id);
                    if matches {
                        t.starred = !starred;
                        if promoted {
                            t.is_discovery = true;
                            t.discovery_track_id = disc_id.clone();
                            t.days_remaining = prev_days;
                        }
                    }
                }
            }
            self.push_mini_snapshot(player);
            return;
        };
        let mut studio = *self;
        spawn(async move {
            let res = core
                .enqueue_mutation(OutboxMutation::Star {
                    song_id: song_id.clone(),
                    starred,
                    discovery_track_id: disc_id.clone(),
                })
                .await;
            if let Err(err) = res {
                if !song_id.is_empty() {
                    player.state_signal().write().apply_curation_update(
                        &song_id,
                        Some(!starred),
                        None,
                        false,
                    );
                } else if let Some(ref did) = disc_id {
                    if !did.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            did,
                            Some(!starred),
                            None,
                            false,
                        );
                    }
                }
                if promoted {
                    {
                        let mut kept = studio.optimistic_kept.write();
                        for key in &guard_keys {
                            kept.remove(key);
                        }
                    }
                    if !guard_keys.is_empty() {
                        let mut in_flight = studio.in_flight_triage.write();
                        for key in &guard_keys {
                            in_flight.remove(key);
                        }
                    }
                    player.state_signal().write().restore_discovery_state(
                        &song_id,
                        disc_id.as_deref(),
                        prev_days,
                    );
                }
                {
                    let mut inspected = studio.inspected_track.write();
                    if let Some(t) = inspected.as_mut() {
                        let matches = (!song_id.is_empty() && t.id == song_id)
                            || (disc_id.is_some() && t.discovery_track_id == disc_id);
                        if matches {
                            t.starred = !starred;
                            if promoted {
                                t.is_discovery = true;
                                t.discovery_track_id = disc_id.clone();
                                t.days_remaining = prev_days;
                            }
                        }
                    }
                }
                studio.push_mini_snapshot(player);
                studio
                    .status_toast
                    .set(Some(format!("Star update failed: {err}")));
                return;
            }
            if promoted {
                player
                    .state_signal()
                    .write()
                    .finalize_promoted_discovery(&song_id, disc_id.as_deref());
                studio.finalize_inspected_promoted_discovery(&song_id, disc_id.as_deref());
                studio.push_mini_snapshot(player);
            }
            studio.bump_library();
            studio.refresh_outbox_counts().await;
            if promoted {
                {
                    let mut kept = studio.optimistic_kept.write();
                    for key in &guard_keys {
                        kept.remove(key);
                    }
                }
                if !guard_keys.is_empty() {
                    let mut in_flight = studio.in_flight_triage.write();
                    for key in &guard_keys {
                        in_flight.remove(key);
                    }
                }
            }
            if !core.is_offline_only() {
                let _ = core.drain_outbox().await;
                studio.refresh_outbox_counts().await;
            }
        });
    }

    pub fn rate_track(&mut self, player: PlayerContext, track: PlayerTrack, rating: u8) {
        let clamped = rating.min(5);
        let prev_rating = track.rating;
        let song_id = track.id.clone();
        let disc_id = track.discovery_track_id.clone();

        // Pure 1-5 rating everywhere (never promotes or drops discovery tracks!)
        player.state_signal().write().apply_curation_update(
            &song_id,
            None,
            Some(clamped),
            false,
        );
        {
            let mut inspected = self.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!song_id.is_empty() && t.id == song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.rating = clamped;
                }
            }
        }
        self.push_mini_snapshot(player);

        let Some(core) = self.core.peek().clone() else {
            player.state_signal().write().apply_curation_update(
                &song_id,
                None,
                Some(prev_rating),
                false,
            );
            {
                let mut inspected = self.inspected_track.write();
                if let Some(t) = inspected.as_mut() {
                    let matches = (!song_id.is_empty() && t.id == song_id)
                        || (disc_id.is_some() && t.discovery_track_id == disc_id);
                    if matches {
                        t.rating = prev_rating;
                    }
                }
            }
            self.push_mini_snapshot(player);
            return;
        };
        let mut studio = *self;
        spawn(async move {
            let res = core
                .enqueue_mutation(OutboxMutation::Rate {
                    song_id: song_id.clone(),
                    rating: clamped,
                    discovery_track_id: disc_id.clone(),
                })
                .await;
            if let Err(err) = res {
                player.state_signal().write().apply_curation_update(
                    &song_id,
                    None,
                    Some(prev_rating),
                    false,
                );
                {
                    let mut inspected = studio.inspected_track.write();
                    if let Some(t) = inspected.as_mut() {
                        let matches = (!song_id.is_empty() && t.id == song_id)
                            || (disc_id.is_some() && t.discovery_track_id == disc_id);
                        if matches {
                            t.rating = prev_rating;
                        }
                    }
                }
                studio.push_mini_snapshot(player);
                studio
                    .status_toast
                    .set(Some(format!("Rating update failed: {err}")));
                return;
            }
            studio.bump_library();
            studio.refresh_outbox_counts().await;
            if !core.is_offline_only() {
                let _ = core.drain_outbox().await;
                studio.refresh_outbox_counts().await;
            }
        });
    }

    /// Pending Discovery drops stage behind the 5-second `UndoSnackbar` and skip immediately;
    /// Permanent Library removals open the Multi-Listener Delete Guard sheet (failing closed while loading).
    pub fn drop_track(&mut self, player: PlayerContext, track: PlayerTrack) {
        if track.is_discovery {
            let displaced = self.pending_drops.write().stage_drop(
                track.id.clone(),
                track.discovery_track_id.clone(),
                track.title.clone(),
                track.artist.clone(),
            );
            if let Some(prev) = displaced {
                self.commit_pending_drop_item(prev);
            }
            if !track.id.is_empty() {
                player.remove_track_from_queue(&track.id);
            }
        } else {
            self.guard_preview.set(Some(fail_closed_guard_preview(
                track.id.clone(),
                "Inspecting household listener protection…".into(),
            )));
            self.guard_modal_track.set(Some(track.clone()));
            self.audit_delete_guard(track.id);
        }
    }

    pub fn undo_active_drop(&mut self) {
        if let Some(item) = self.pending_drops.write().undo_active() {
            self.status_toast
                .set(Some(format!("Restored \"{}\"", item.title)));
        }
    }

    pub fn expire_active_drop(&mut self, key: String) {
        let committed = self.pending_drops.write().commit_active_if_key(&key);
        if let Some(item) = committed {
            self.commit_pending_drop_item(item);
        }
    }

    pub fn commit_pending_drop_item(&mut self, item: PendingDropItem) {
        let Some(core) = self.core.peek().clone() else {
            self.pending_drops.write().restore_failed(&item);
            return;
        };
        let mut studio = *self;
        spawn(async move {
            let res = core
                .enqueue_mutation(OutboxMutation::Drop {
                    song_id: item.song_id.clone(),
                    discovery_track_id: item.discovery_track_id.clone(),
                    confirm_delete: true,
                })
                .await;
            match res {
                Ok(_) => {
                    studio.pending_drops.write().finish_committed(&item);
                    studio.bump_library();
                    studio.refresh_outbox_counts().await;
                    if !core.is_offline_only() {
                        let _ = core.drain_outbox().await;
                        studio.refresh_outbox_counts().await;
                    }
                }
                Err(err) => {
                    studio.pending_drops.write().restore_failed(&item);
                    studio
                        .status_toast
                        .set(Some(format!("Failed to drop \"{}\": {err}", item.title)));
                }
            }
        });
    }

    /// Commits a library track removal strictly according to [`GuardModalMode`]:
    /// - `ListenerProtected`: unstars & sets rating=1 (ZERO TOCTOU disk-deletion risk!)
    /// - `PermanentDelete`: enqueues `OutboxMutation::Drop { confirm_delete: true }`
    /// - `HardBlocked`: closes cleanly with zero mutation.
    pub fn confirm_library_remove(
        &mut self,
        player: PlayerContext,
        track: PlayerTrack,
        preview: DeleteGuardPreview,
    ) {
        self.guard_modal_track.set(None);
        let mode = classify_guard_mode(&preview);
        if mode == GuardModalMode::HardBlocked {
            return;
        }
        if player.is_track_current(&track.id) {
            player.next();
        }
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let mut studio = *self;

        if mode == GuardModalMode::ListenerProtected {
            player
                .state_signal()
                .write()
                .apply_curation_update(&track.id, Some(false), Some(1), false);
            studio.push_mini_snapshot(player);
            spawn(async move {
                let _ = core
                    .enqueue_mutation(OutboxMutation::Star {
                        song_id: track.id.clone(),
                        starred: false,
                        discovery_track_id: None,
                    })
                    .await;
                let res = core
                    .enqueue_mutation(OutboxMutation::Rate {
                        song_id: track.id.clone(),
                        rating: 1,
                        discovery_track_id: None,
                    })
                    .await;
                match res {
                    Ok(_) => {
                        studio.status_toast.set(Some(format!(
                            "Kept file for other listeners — unstarred & hidden \"{}\"",
                            track.title
                        )));
                        studio.bump_library();
                        studio.refresh_outbox_counts().await;
                        if !core.is_offline_only() {
                            let _ = core.drain_outbox().await;
                            studio.refresh_outbox_counts().await;
                        }
                    }
                    Err(err) => {
                        studio
                            .status_toast
                            .set(Some(format!("Failed to hide track: {err}")));
                    }
                }
            });
            return;
        }

        let disc_id = if mode == GuardModalMode::PendingDiscovery {
            preview.discovery_track_id.clone()
        } else {
            None
        };

        spawn(async move {
            let res = core
                .enqueue_mutation(OutboxMutation::Drop {
                    song_id: track.id.clone(),
                    discovery_track_id: disc_id,
                    confirm_delete: true,
                })
                .await;
            match res {
                Ok(_) => {
                    studio
                        .status_toast
                        .set(Some(format!("Queued removal for \"{}\"", track.title)));
                    studio.bump_library();
                    studio.refresh_outbox_counts().await;
                    if !core.is_offline_only() {
                        let _ = core.drain_outbox().await;
                        studio.refresh_outbox_counts().await;
                    }
                }
                Err(err) => {
                    studio
                        .status_toast
                        .set(Some(format!("Remove failed: {err}")));
                }
            }
        });
    }

    pub fn start_local_radio(&mut self, player: PlayerContext, seed: PlayerTrack) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let offline_only = core.is_offline_only();
        let mut studio = *self;
        spawn(async move {
            let all_tracks = load_library_tracks(&core, offline_only).await;
            let mut radio = vec![seed.clone()];
            for t in all_tracks {
                if t.id != seed.id
                    && (t.artist.eq_ignore_ascii_case(&seed.artist) || radio.len() < 25)
                {
                    radio.push(t);
                    if radio.len() >= 25 {
                        break;
                    }
                }
            }
            let count = radio.len();
            player.play_queue(radio, 0);
            studio.status_toast.set(Some(format!(
                "Started Track Radio from \"{}\" ({count} tracks)",
                seed.title
            )));
        });
    }

    /// Calls `POST {base}/api/acquire-and-play` (`{"req": AcquireAndPlayRequest}`) to acquire
    /// an unowned remote catalog track and immediately play it once ready.
    pub fn acquire_and_play_remote(
        &mut self,
        player: PlayerContext,
        remote: RemoteCatalogTrack,
    ) {
        let cfg = self.config.peek().clone();
        if cfg.offline_only {
            self.status_toast
                .set(Some("Disable Offline Only mode to acquire catalog tracks.".into()));
            return;
        }
        let Some(base) = cfg.soulbeet_base_url.clone() else {
            self.status_toast.set(Some(
                "Connect a Soulbeet server in Settings to use Acquire & Play.".into(),
            ));
            return;
        };
        let row_key = remote_track_acquire_key(&remote);
        let album_opt = (!remote.album.trim().is_empty()).then(|| remote.album.clone());
        let mbid_opt = (!remote.mbid.trim().is_empty()).then(|| remote.mbid.clone());
        let release_mbid_opt = remote
            .release_mbid
            .clone()
            .filter(|s| !s.trim().is_empty());
        let duration_opt = (remote.duration_secs > 0).then_some(remote.duration_secs);

        self.acquiring_tracks.write().insert(
            row_key.clone(),
            AcquireStageBadge::Acquiring {
                label: Some("Soulbeet".into()),
            },
        );
        let auth = cfg.soulbeet_auth();
        let core_opt = self.core.peek().clone();
        let mut studio = *self;

        spawn(async move {
            let client = reqwest::Client::new();
            let url = format!("{}/api/acquire-and-play", base.trim_end_matches('/'));
            let payload = AcquireAndPlayRequest {
                artist: remote.artist.clone(),
                title: remote.title.clone(),
                album: album_opt.clone(),
                recording_mbid: mbid_opt,
                release_mbid: release_mbid_opt,
                duration_s: duration_opt,
                folder_id: None,
                prefer_fast: true,
            };
            let mut req = client
                .post(&url)
                .json(&serde_json::json!({ "req": payload }));
            if let Some(a) = auth {
                let (k, v) = a.header_name_and_value();
                req = req.header(k, v);
            }

            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<AcquireAndPlayResponse>().await {
                        Ok(acq) => match acq.status {
                            AcquireAndPlayInitialStatus::AlreadyOwned {
                                navidrome_song_id,
                                codec,
                                ..
                            } => {
                                studio.acquiring_tracks.write().remove(&row_key);
                                let track = PlayerTrack {
                                    id: navidrome_song_id,
                                    title: remote.title.clone(),
                                    artist: remote.artist.clone(),
                                    album: album_opt.clone(),
                                    cover_art_url: remote.cover_url.clone(),
                                    duration_secs: duration_opt,
                                    codec_badge: codec,
                                    ..Default::default()
                                };
                                player.play_track(track);
                                studio.status_toast.set(Some(format!(
                                    "Already in library: playing \"{}\"",
                                    remote.title
                                )));
                            }
                            AcquireAndPlayInitialStatus::Started { backend_ids, .. } => {
                                let label = backend_ids.first().cloned();
                                studio.acquiring_tracks.write().insert(
                                    row_key.clone(),
                                    AcquireStageBadge::Acquiring { label },
                                );
                                studio.status_toast.set(Some(format!(
                                    "Acquiring \"{}\": will sync when imported",
                                    remote.title
                                )));
                                // Background delta sync check to pick up the acquired track cleanly.
                                if let Some(core) = core_opt {
                                    let q_artist = remote.artist.clone();
                                    let q_title = remote.title.clone();
                                    for _ in 0..8 {
                                        tokio::time::sleep(Duration::from_secs(5)).await;
                                        let same_session = studio
                                            .core
                                            .peek()
                                            .as_ref()
                                            .is_some_and(|active| Arc::ptr_eq(active, &core));
                                        if !same_session || core.is_offline_only() {
                                            return;
                                        }
                                        let _ = core.sync_now(false).await;
                                        if let Ok(found) = core
                                            .search_local(&format!("{q_artist} {q_title}"), 10)
                                            .await
                                        {
                                            if let Some(local) = found.local_tracks.into_iter().find(
                                                |t| {
                                                    acquired_track_matches(
                                                        &t.artist_name,
                                                        &t.title,
                                                        &q_artist,
                                                        &q_title,
                                                    )
                                                },
                                            ) {
                                                let cached = load_cached_track_ids(&core).await;
                                                let disc = load_pending_discovery_map(&core).await;
                                                let pt = local_track_to_player_track(
                                                    &local, &cached, &disc,
                                                );
                                                studio.acquiring_tracks.write().remove(&row_key);
                                                studio.bump_library();
                                                player.play_track(pt);
                                                return;
                                            }
                                        }
                                    }
                                    studio.acquiring_tracks.write().remove(&row_key);
                                }
                            }
                        },
                        Err(err) => {
                            studio.acquiring_tracks.write().insert(
                                row_key,
                                AcquireStageBadge::Failed {
                                    message: Some(format!("Invalid response: {err}")),
                                },
                            );
                        }
                    }
                }
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    studio.acquiring_tracks.write().insert(
                        row_key,
                        AcquireStageBadge::Failed {
                            message: Some(format!("HTTP {code}: {}", body.trim())),
                        },
                    );
                }
                Err(err) => {
                    studio.acquiring_tracks.write().insert(
                        row_key,
                        AcquireStageBadge::Failed {
                            message: Some(err.to_string()),
                        },
                    );
                }
            }
        });
    }

    /// Triggers a 1-tap server-side remote album download via `SoulbeetCore::download_remote_album`
    /// and updates live per-album download feedback state + toast notifications.
    pub fn download_remote_album(&mut self, album: RemoteCatalogAlbum) {
        let cfg = self.config.peek().clone();
        if cfg.offline_only {
            self.status_toast
                .set(Some("Disable Offline Only mode to download catalog albums.".into()));
            return;
        }
        let Some(core) = self.core.peek().clone() else {
            self.status_toast.set(Some(
                "Connect a Soulbeet server in Settings to download albums.".into(),
            ));
            return;
        };
        let album_key = remote_album_download_key(&album);
        if matches!(
            self.downloading_albums.peek().get(&album_key),
            Some(RemoteAlbumDownloadState::Downloading)
        ) {
            return;
        }

        self.downloading_albums
            .write()
            .insert(album_key.clone(), RemoteAlbumDownloadState::Downloading);
        let mut studio = *self;

        spawn(async move {
            let result = core.download_remote_album(&album).await;
            let same_session = studio
                .core
                .peek()
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &core));
            if !same_session || core.is_offline_only() {
                return;
            }
            match result {
                Ok(batch_id) => {
                    let queued_state = RemoteAlbumDownloadState::Queued { batch_id };
                    studio
                        .downloading_albums
                        .write()
                        .insert(album_key.clone(), queued_state.clone());
                    studio.status_toast.set(Some(format!(
                        "Queued \"{}\" in Activity",
                        album.title
                    )));
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    let mut map = studio.downloading_albums.write();
                    if map.get(&album_key) == Some(&queued_state) {
                        map.remove(&album_key);
                    }
                }
                Err(err) => {
                    let msg = err.to_string();
                    studio.downloading_albums.write().insert(
                        album_key,
                        RemoteAlbumDownloadState::Failed(msg.clone()),
                    );
                    studio
                        .status_toast
                        .set(Some(format!("Album download failed: {msg}")));
                }
            }
        });
    }

    /// Calls `POST {base}/api/discovery/seasonal/steer` to adjust cultural mix steering on the server.
    pub fn steer_seasonal_pack(&mut self, slug: String, steering: CultureSteering) {
        let cfg = self.config.peek().clone();
        if cfg.offline_only {
            self.status_toast
                .set(Some("Steering saved locally (offline mode).".into()));
            return;
        }
        let Some(base) = cfg.soulbeet_base_url.clone() else {
            return;
        };
        let auth = cfg.soulbeet_auth();
        let mut studio = *self;
        spawn(async move {
            let client = reqwest::Client::new();
            let url = format!(
                "{}/api/discovery/seasonal/steer",
                base.trim_end_matches('/')
            );
            let req_body = SteerSeasonalPackRequest { slug, steering };
            let mut req = client
                .post(&url)
                .json(&serde_json::json!({ "req": req_body }));
            if let Some(a) = auth {
                let (k, v) = a.header_name_and_value();
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(summary) = resp.json::<ContextualMixSummary>().await {
                        studio.status_toast.set(Some(format!(
                            "Updated seasonal mix steering ({} tracks)",
                            summary.track_count
                        )));
                    }
                    studio.sync_now(false);
                }
                Ok(resp) => {
                    tracing::warn!(
                        "seasonal steer returned HTTP {}",
                        resp.status().as_u16()
                    );
                }
                Err(err) => {
                    tracing::warn!("seasonal steer request failed: {err}");
                }
            }
        });
    }

    /// Triggers server-side discovery generation (`POST {base}/api/discovery/start-generation`),
    /// polls `GET {base}/api/discovery/progress` every 2.5s without consuming stale terminal
    /// snapshots from a prior run, and triggers `sync_now(false)` on completion so newly
    /// generated discovery tracks and seasonal mixes appear immediately in Desktop.
    pub fn refresh_discovery(&self) {
        let mut studio = *self;
        if *studio.discovery_generating.peek() {
            return;
        }
        let cfg = studio.config.peek().clone();
        if cfg.offline_only {
            studio
                .status_toast
                .set(Some("Disable Offline Only mode to refresh discovery.".into()));
            return;
        }
        let Some(base) = cfg
            .soulbeet_base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
        else {
            studio.status_toast.set(Some(
                "Connect a Soulbeet server in Settings to generate new discovery tracks.".into(),
            ));
            return;
        };
        let auth = cfg.soulbeet_auth();
        let my_gen = studio.discovery_poll_gen.peek().fetch_add(1, Ordering::SeqCst) + 1;
        let poll_gen_arc = studio.discovery_poll_gen.peek().clone();

        studio.discovery_generating.set(true);
        studio.discovery_refresh_error.set(None);
        studio
            .discovery_progress_label
            .set(Some("Starting discovery refresh...".into()));
        studio.discovery_progress_pct.set(Some(8));

        spawn(async move {
            let client = match reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(err) => {
                    if poll_gen_arc.load(Ordering::SeqCst) == my_gen {
                        let msg = format!("HTTP client error: {err}");
                        studio.discovery_generating.set(false);
                        studio.discovery_progress_label.set(None);
                        studio.discovery_progress_pct.set(None);
                        studio.discovery_refresh_error.set(Some(msg.clone()));
                        studio.status_toast.set(Some(msg));
                    }
                    return;
                }
            };

            // Snapshot any prior terminal run BEFORE starting so we never consume a stale
            // Complete/Error cached for 5 minutes from a previous run.
            let initial_progress = fetch_discovery_progress(&client, &base, auth.as_ref())
                .await
                .ok()
                .flatten();
            let pre_click_started_at = initial_progress
                .as_ref()
                .and_then(|p| p.started_at.clone());
            let mut observed_running = initial_progress
                .as_ref()
                .is_some_and(|p| p.status == GenerationStatus::Running);

            if !observed_running {
                let start_url = format!(
                    "{}/api/discovery/start-generation",
                    base.trim_end_matches('/')
                );
                let mut req = client.post(&start_url).json(&serde_json::json!({}));
                if let Some(ref a) = auth {
                    let (k, v) = a.header_name_and_value();
                    req = req.header(k, v);
                }
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {}
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp.text().await.unwrap_or_default();
                        let (msg, already_running) = parse_server_error_message(&body, status);
                        if already_running {
                            observed_running = true;
                        } else {
                            if poll_gen_arc.load(Ordering::SeqCst) == my_gen {
                                studio.discovery_generating.set(false);
                                studio.discovery_progress_label.set(None);
                                studio.discovery_progress_pct.set(None);
                                studio.discovery_refresh_error.set(Some(msg.clone()));
                                studio.status_toast.set(Some(msg));
                            }
                            return;
                        }
                    }
                    Err(err) => {
                        if poll_gen_arc.load(Ordering::SeqCst) == my_gen {
                            let msg = format!("Could not start discovery refresh: {err}");
                            studio.discovery_generating.set(false);
                            studio.discovery_progress_label.set(None);
                            studio.discovery_progress_pct.set(None);
                            studio.discovery_refresh_error.set(Some(msg.clone()));
                            studio.status_toast.set(Some(msg));
                        }
                        return;
                    }
                }
            }

            let mut consecutive_errors = 0u32;
            let mut consecutive_idle = 0u32;

            loop {
                if poll_gen_arc.load(Ordering::SeqCst) != my_gen {
                    return;
                }
                match fetch_discovery_progress(&client, &base, auth.as_ref()).await {
                    Ok(Some(progress)) => {
                        consecutive_errors = 0;
                        match progress.status {
                            GenerationStatus::Running => {
                                consecutive_idle = 0;
                                observed_running = true;
                                let (label, pct) = desktop_discovery_progress_summary(&progress);
                                studio.discovery_progress_label.set(Some(label));
                                studio.discovery_progress_pct.set(Some(pct));
                            }
                            GenerationStatus::Complete => {
                                if observed_running
                                    || progress.started_at != pre_click_started_at
                                {
                                    let total_new = progress
                                        .result
                                        .as_ref()
                                        .map(|r| r.total_imported)
                                        .unwrap_or(0);
                                    let noun = if total_new == 1 { "track" } else { "tracks" };
                                    studio.discovery_generating.set(false);
                                    studio.discovery_progress_label.set(None);
                                    studio.discovery_progress_pct.set(None);
                                    studio.discovery_refresh_error.set(None);
                                    studio.status_toast.set(Some(format!(
                                        "Discovery refreshed: {total_new} new {noun}. Syncing library..."
                                    )));
                                    studio.sync_now(false);
                                    return;
                                }
                            }
                            GenerationStatus::Error => {
                                if observed_running
                                    || progress.started_at != pre_click_started_at
                                {
                                    let msg = progress
                                        .failure
                                        .as_ref()
                                        .map(|f| f.message.clone())
                                        .unwrap_or_else(|| {
                                            "Discovery generation failed".to_string()
                                        });
                                    studio.discovery_generating.set(false);
                                    studio.discovery_progress_label.set(None);
                                    studio.discovery_progress_pct.set(None);
                                    studio.discovery_refresh_error.set(Some(msg.clone()));
                                    studio.status_toast.set(Some(msg));
                                    return;
                                }
                            }
                            GenerationStatus::Idle => {
                                consecutive_idle += 1;
                                if observed_running || consecutive_idle >= 3 {
                                    studio.discovery_generating.set(false);
                                    studio.discovery_progress_label.set(None);
                                    studio.discovery_progress_pct.set(None);
                                    studio.sync_now(false);
                                    return;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        consecutive_errors = 0;
                        consecutive_idle += 1;
                        if observed_running || consecutive_idle >= 3 {
                            studio.discovery_generating.set(false);
                            studio.discovery_progress_label.set(None);
                            studio.discovery_progress_pct.set(None);
                            studio.sync_now(false);
                            return;
                        }
                    }
                    Err(err) => {
                        consecutive_errors += 1;
                        tracing::warn!("discovery progress poll failed: {err}");
                        if consecutive_errors >= 5 {
                            studio.discovery_generating.set(false);
                            studio.discovery_progress_label.set(None);
                            studio.discovery_progress_pct.set(None);
                            studio.discovery_refresh_error.set(Some(err.clone()));
                            studio.status_toast.set(Some(err));
                            return;
                        }
                    }
                }

                tokio::time::sleep(Duration::from_millis(2500)).await;
            }
        });
    }

    // --- Sync, Outbox, Cache Pinning & Remote Metadata Helpers ---

    pub async fn refresh_outbox_counts(&mut self) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let pending = core.outbox().pending_count().await.unwrap_or(0);
        let dead = core
            .outbox()
            .dead_lettered()
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        let mut st = self.sync_status.write();
        st.pending_outbox_count = pending;
        st.dead_letter_count = dead;
    }

    pub async fn refresh_cache_and_playlists(&mut self) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        if let Ok(stats) = core.cache_stats().await {
            self.cache_stats.set(Some(stats));
        }
        let pls = load_desktop_playlists(&core).await;
        self.playlists.set(pls);
        self.refresh_outbox_counts().await;
    }

    pub fn sync_now(&mut self, force_full: bool) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        if core.is_offline_only() {
            self.status_toast
                .set(Some("Disable Offline Only mode to sync library".into()));
            return;
        }
        if self.sync_status.peek().syncing {
            return;
        }
        self.sync_status.write().syncing = true;
        self.sync_status.write().last_error = None;
        let cfg = self.config.peek().clone();
        let mut studio = *self;
        spawn(async move {
            let drain = core.drain_outbox().await;
            if let Err(err) = drain {
                tracing::warn!("outbox drain before sync failed: {err}");
            }
            if let Ok(Some(boot)) = verify_desktop_config(&cfg).await {
                studio.bootstrap.set(Some(boot));
            }
            match core.sync_now(force_full).await {
                Ok(report) => {
                    {
                        let mut st = studio.sync_status.write();
                        st.syncing = false;
                        st.last_report = Some(report.clone());
                        st.last_sync_unix = Some(Utc::now().timestamp());
                        st.last_error = None;
                    }
                    studio.refresh_cache_and_playlists().await;
                    studio.bump_library();
                    studio.status_toast.set(Some(format!(
                        "Synced {} tracks, {} albums, {} playlists",
                        report.tracks_synced, report.albums_synced, report.playlists_synced
                    )));
                }
                Err(err) => {
                    let mut st = studio.sync_status.write();
                    st.syncing = false;
                    st.last_error = Some(err.to_string());
                    studio.status_toast.set(Some(format!("Sync failed: {err}")));
                }
            }
        });
    }

    pub fn drain_outbox_bg(&mut self) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        if core.is_offline_only() {
            return;
        }
        let mut studio = *self;
        spawn(async move {
            let _ = core.drain_outbox().await;
            studio.refresh_outbox_counts().await;
        });
    }

    pub fn pin_target_offline(&mut self, target: PinTarget, label: String) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let mut studio = *self;
        spawn(async move {
            match core.pin(&target).await {
                Ok(missing_ids) => {
                    let total = missing_ids.len();
                    studio.status_toast.set(Some(if total == 0 {
                        format!("Pinned \"{label}\" (already cached offline)")
                    } else {
                        format!("Pinned \"{label}\" — caching {total} tracks…")
                    }));
                    studio.refresh_cache_and_playlists().await;
                    if !core.is_offline_only() {
                        let (_, pin_src) = target.as_db_pair();
                        for tid in missing_ids {
                            let _ = core
                                .cache()
                                .download_track(
                                    &tid,
                                    &AudioQualityProfile::LosslessFlac,
                                    true,
                                    Some(&pin_src),
                                )
                                .await;
                        }
                    }
                    studio.refresh_cache_and_playlists().await;
                    studio.bump_library();
                }
                Err(err) => {
                    studio.status_toast.set(Some(format!("Pin failed: {err}")));
                }
            }
        });
    }

    pub fn unpin_target_offline(&mut self, target: PinTarget, label: String) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let mut studio = *self;
        spawn(async move {
            match core.unpin(&target).await {
                Ok(evicted) => {
                    studio.status_toast.set(Some(format!(
                        "Unpinned \"{label}\" ({evicted} cached files freed)"
                    )));
                    studio.refresh_cache_and_playlists().await;
                    studio.bump_library();
                }
                Err(err) => {
                    studio.status_toast.set(Some(format!("Unpin failed: {err}")));
                }
            }
        });
    }

    /// Calls the real Soulbeet lyrics endpoint: `GET {base}/api/client/lyrics?song_id=...&artist=...&title=...`.
    pub fn fetch_lyrics(&mut self, song_id: String, artist: Option<String>, title: Option<String>) {
        let cfg = self.config.peek().clone();
        if cfg.offline_only {
            self.lyrics_error
                .set(Some("Offline Only mode active — cannot fetch remote lyrics.".into()));
            return;
        }
        let Some(base) = cfg.soulbeet_base_url.clone() else {
            self.lyrics_error
                .set(Some("Connect a Soulbeet server in Settings to fetch synced lyrics.".into()));
            return;
        };
        let auth = cfg.soulbeet_auth();
        self.lyrics_loading.set(true);
        self.lyrics_error.set(None);
        let mut studio = *self;
        spawn(async move {
            let client = reqwest::Client::new();
            let mut url = match url::Url::parse(&format!(
                "{}/api/client/lyrics",
                base.trim_end_matches('/')
            )) {
                Ok(u) => u,
                Err(err) => {
                    studio
                        .lyrics_error
                        .set(Some(format!("Invalid Soulbeet URL: {err}")));
                    studio.lyrics_loading.set(false);
                    return;
                }
            };
            {
                let mut qp = url.query_pairs_mut();
                qp.append_pair("song_id", &song_id);
                if let Some(a) = artist.as_deref().filter(|s| !s.trim().is_empty()) {
                    qp.append_pair("artist", a);
                }
                if let Some(t) = title.as_deref().filter(|s| !s.trim().is_empty()) {
                    qp.append_pair("title", t);
                }
            }
            let mut req = client.get(url);
            if let Some(a) = auth {
                let (k, v) = a.header_name_and_value();
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<Vec<TrackLyrics>>().await
                {
                    Ok(items) => {
                        studio.lyrics.set(items);
                        studio.lyrics_error.set(None);
                    }
                    Err(err) => {
                        studio
                            .lyrics_error
                            .set(Some(format!("Could not parse lyrics: {err}")));
                    }
                },
                Ok(resp) => {
                    studio.lyrics_error.set(Some(format!(
                        "Server returned HTTP {} for lyrics",
                        resp.status().as_u16()
                    )));
                }
                Err(err) => {
                    studio
                        .lyrics_error
                        .set(Some(format!("Lyrics fetch failed: {err}")));
                }
            }
            studio.lyrics_loading.set(false);
        });
    }

    /// Calls the real Soulbeet delete-guard endpoint: `GET {base}/api/curation/guard-preview/{song_id}`.
    /// Strictly fails CLOSED on offline mode, missing server URL, or any HTTP/network error.
    pub fn audit_delete_guard(&mut self, song_id: String) {
        let cfg = self.config.peek().clone();
        if cfg.offline_only {
            self.guard_loading.set(false);
            self.guard_preview.set(Some(fail_closed_guard_preview(
                song_id,
                "Multi-listener guard audit requires an online Soulbeet connection.".into(),
            )));
            return;
        }
        let Some(base) = cfg.soulbeet_base_url.clone() else {
            self.guard_loading.set(false);
            self.guard_preview.set(Some(fail_closed_guard_preview(
                song_id,
                "Connect a Soulbeet server to inspect household listener protection.".into(),
            )));
            return;
        };
        let auth = cfg.soulbeet_auth();
        self.guard_loading.set(true);
        self.guard_preview.set(Some(fail_closed_guard_preview(
            song_id.clone(),
            "Inspecting household listener protection…".into(),
        )));
        let mut studio = *self;
        spawn(async move {
            let client = reqwest::Client::new();
            let encoded_id = percent_encode_param(&song_id);
            let url = format!(
                "{}/api/curation/guard-preview/{encoded_id}",
                base.trim_end_matches('/')
            );
            let mut req = client.get(&url);
            if let Some(a) = auth {
                let (k, v) = a.header_name_and_value();
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<DeleteGuardPreview>().await {
                        Ok(preview) => studio.guard_preview.set(Some(preview)),
                        Err(err) => {
                            studio.guard_preview.set(Some(fail_closed_guard_preview(
                                song_id,
                                format!("Could not decode guard preview: {err}"),
                            )));
                        }
                    }
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    studio.guard_preview.set(Some(fail_closed_guard_preview(
                        song_id,
                        format!("Delete guard audit failed (HTTP {status}); deletion blocked for safety."),
                    )));
                }
                Err(err) => {
                    studio.guard_preview.set(Some(fail_closed_guard_preview(
                        song_id,
                        format!("Could not verify household listener protection: {err}"),
                    )));
                }
            }
            studio.guard_loading.set(false);
        });
    }

    pub fn publish_connect_snapshot(&self, player: PlayerContext) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        if core.is_offline_only() {
            return;
        }
        let st = player.state_signal().peek().clone();
        let pos = *player.position_signal().peek();
        let dur = *player.duration_signal().peek();
        let snap = PlaybackSnapshotDto {
            song_id: st.current_track.as_ref().map(|t| t.id.clone()),
            title: st.current_track.as_ref().map(|t| t.title.clone()),
            artist: st.current_track.as_ref().map(|t| t.artist.clone()),
            album: st.current_track.as_ref().and_then(|t| t.album.clone()),
            cover_art_id: st
                .current_track
                .as_ref()
                .and_then(|t| t.cover_art_id.clone()),
            is_playing: st.status.is_playing(),
            position_ms: (pos.max(0.0) * 1000.0) as u64,
            duration_ms: (dur.max(0.0) * 1000.0) as u64,
            queue_song_ids: st.queue.iter().map(|t| t.id.clone()).collect(),
            queue_index: st.queue_index.unwrap_or(0),
            volume: st.volume,
            is_pending_discovery: st.current_track.as_ref().is_some_and(|t| t.is_discovery),
            updated_at_unix: Utc::now().timestamp(),
        };
        spawn(async move {
            let _ = core.connect_client().update_state(snap).await;
        });
    }

    pub fn claim_connect_active(&self) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        spawn(async move {
            let _ = core.connect_client().claim_active().await;
        });
    }

    pub fn transfer_playback_to(&self, player: PlayerContext, target_device_id: String) {
        let Some(core) = self.core.peek().clone() else {
            return;
        };
        let st = player.state_signal().peek().clone();
        let pos = *player.position_signal().peek();
        let dur = *player.duration_signal().peek();
        let snap = PlaybackSnapshotDto {
            song_id: st.current_track.as_ref().map(|t| t.id.clone()),
            title: st.current_track.as_ref().map(|t| t.title.clone()),
            artist: st.current_track.as_ref().map(|t| t.artist.clone()),
            album: st.current_track.as_ref().and_then(|t| t.album.clone()),
            cover_art_id: st
                .current_track
                .as_ref()
                .and_then(|t| t.cover_art_id.clone()),
            is_playing: st.status.is_playing(),
            position_ms: (pos.max(0.0) * 1000.0) as u64,
            duration_ms: (dur.max(0.0) * 1000.0) as u64,
            queue_song_ids: st.queue.iter().map(|t| t.id.clone()).collect(),
            queue_index: st.queue_index.unwrap_or(0),
            volume: st.volume,
            is_pending_discovery: st.current_track.as_ref().is_some_and(|t| t.is_discovery),
            updated_at_unix: Utc::now().timestamp(),
        };
        spawn(async move {
            let _ = core
                .connect_client()
                .transfer_playback_to(target_device_id, snap)
                .await;
        });
    }

    /// Initializes or reloads `Arc<SoulbeetCore>` from `next_config`, saves config with `0o600`
    /// permissions, starts supervised Connect WS + sync, and loads any persisted playback session
    /// paused into `libmpv` at the exact saved position so pressing Play immediately resumes!
    pub fn connect_with_config(
        &mut self,
        next_config: DesktopConfig,
        initial_bootstrap: Option<ClientBootstrap>,
        mut player: PlayerContext,
    ) {
        if let Err(err) = next_config.save_to_disk() {
            self.core_error.set(Some(err));
            return;
        }
        self.config.set(next_config.clone());
        if initial_bootstrap.is_some() {
            self.bootstrap.set(initial_bootstrap);
        }
        self.core_error.set(None);
        self.acquiring_tracks.write().clear();
        self.downloading_albums.write().clear();
        let mut studio = *self;

        spawn(async move {
            let client_cfg = next_config.to_client_config();
            match SoulbeetCore::init(client_cfg).await {
                Ok(core) => {
                    let core = Arc::new(core);
                    studio.core.set(Some(core.clone()));
                    studio.refresh_cache_and_playlists().await;
                    studio.bump_library();

                    // Restore persisted playback session AND preload paused into libmpv at exact position
                    if player.state_signal().peek().queue.is_empty() {
                        if let Ok(Some(session)) = core.load_playback_session().await {
                            if !session.queue_track_ids.is_empty() {
                                let cached = load_cached_track_ids(&core).await;
                                let disc = load_pending_discovery_map(&core).await;
                                let tracks_map: HashMap<String, LocalTrack> =
                                    client_core::db::get_tracks_by_ids(
                                        core.pool(),
                                        &session.queue_track_ids,
                                    )
                                    .await
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|t| (t.id.clone(), t))
                                    .collect();
                                let restored: Vec<PlayerTrack> = session
                                    .queue_track_ids
                                    .iter()
                                    .filter_map(|id| tracks_map.get(id))
                                    .map(|t| local_track_to_player_track(t, &cached, &disc))
                                    .collect();
                                if !restored.is_empty() {
                                    let idx = session.current_index.min(restored.len() - 1);
                                    let current = restored.get(idx).cloned();
                                    let resume_pos = session.position_ms as f64 / 1000.0;
                                    let dur = current
                                        .as_ref()
                                        .and_then(|t| t.duration_secs)
                                        .map(f64::from)
                                        .unwrap_or(0.0);
                                    {
                                        let mut st = player.state_signal();
                                        let mut w = st.write();
                                        w.queue = restored;
                                        w.queue_index = Some(idx);
                                        w.current_track = current.clone();
                                        w.status = PlaybackStatus::Paused;
                                    }
                                    player.set_progress(resume_pos, dur);
                                    if let Some(track) = current {
                                        studio.load_current_track_in_mpv(
                                            player,
                                            track,
                                            true,
                                            (resume_pos > 0.5).then_some(resume_pos),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Start supervised Connect WS reconnect loop when online
                    if !core.is_offline_only() && next_config.soulbeet_base_url.is_some() {
                        studio.start_supervised_connect(core.clone(), player);
                    }

                    // Initial background sync when online
                    if !core.is_offline_only() {
                        studio.sync_now(false);
                    }
                }
                Err(err) => {
                    studio
                        .core_error
                        .set(Some(format!("Failed to open local SQLite core: {err}")));
                }
            }
        });
    }

    pub fn disconnect_server(&mut self, player: PlayerContext) {
        // Flush any staged Undo drop before detaching core so pending mutations are never lost.
        let staged = self.pending_drops.write().drain_active();
        if let Some(item) = staged {
            if let Some(core) = self.core.peek().clone() {
                spawn(async move {
                    let _ = core
                        .enqueue_mutation(OutboxMutation::Drop {
                            song_id: item.song_id,
                            discovery_track_id: item.discovery_track_id,
                            confirm_delete: true,
                        })
                        .await;
                });
            }
        }
        self.pending_drops.set(PendingDropQueue::new());
        self.in_flight_triage.write().clear();
        self.optimistic_kept.write().clear();
        self.acquiring_tracks.write().clear();
        self.downloading_albums.write().clear();
        self.discovery_poll_gen.peek().fetch_add(1, Ordering::SeqCst);
        self.discovery_generating.set(false);
        self.discovery_progress_label.set(None);
        self.discovery_progress_pct.set(None);
        self.discovery_refresh_error.set(None);

        self.connect_gen.peek().fetch_add(1, Ordering::SeqCst);
        if let Some(core) = self.core.peek().clone() {
            spawn(async move {
                core.connect_client().disconnect().await;
            });
        }
        self.handle_player_command(player, PlayerCommand::ClearQueue);
        self.close_mini_player();
        let mut cfg = self.config.peek().clone();
        cfg.soulbeet_base_url = None;
        cfg.auth_token = None;
        cfg.navidrome = None;
        let _ = cfg.save_to_disk();
        self.config.set(cfg);
        self.bootstrap.set(None);
        self.core.set(None);
        self.playlists.set(Vec::new());
        self.connect_devices.set(Vec::new());
        self.sync_status.write().connect_online = false;
    }

    // --- Floating 360×110 Mini-Player Window Lifecycle (Generation-Guarded) ---

    pub fn toggle_mini_player(&mut self, player: PlayerContext) {
        if *self.mini_player_open.peek() {
            self.close_mini_player();
        } else {
            self.open_mini_player(player);
        }
    }

    pub fn open_mini_player(&mut self, player: PlayerContext) {
        if *self.mini_player_open.peek() {
            if let Some(win) = self.mini_window.peek().as_ref() {
                win.set_focus();
            }
            return;
        }
        let my_gen = self.mini_gen.peek().fetch_add(1, Ordering::SeqCst) + 1;
        let gen_arc = self.mini_gen.peek().clone();
        self.push_mini_snapshot(player);
        let bridge = MiniPlayerBridge {
            generation: my_gen,
            snapshot_rx: self.mini_snapshot_tx.peek().subscribe(),
            action_tx: self.mini_action_tx.peek().clone(),
            core: self.core.peek().clone(),
        };
        self.mini_player_open.set(true);
        let mut studio = *self;
        spawn(async move {
            let ctx = open_mini_player_window(bridge).await;
            if gen_arc.load(Ordering::SeqCst) != my_gen {
                // Superseded by close/reopen while creating webview: close immediately!
                ctx.close();
                return;
            }
            studio.mini_window.set(Some(ctx));
        });
    }

    pub fn close_mini_player(&mut self) {
        self.mini_gen.peek().fetch_add(1, Ordering::SeqCst);
        if let Some(win) = self.mini_window.take() {
            win.close();
        }
        self.mini_player_open.set(false);
    }
}

async fn apply_connect_command(
    mut studio: DesktopStudio,
    mut player: PlayerContext,
    core: &SoulbeetCore,
    cmd: ConnectCommandDto,
) {
    match cmd {
        ConnectCommandDto::Play => {
            if !player.is_playing() {
                player.toggle_play_pause();
            }
        }
        ConnectCommandDto::Pause => {
            if player.is_playing() {
                player.toggle_play_pause();
            }
        }
        ConnectCommandDto::TogglePlay => player.toggle_play_pause(),
        ConnectCommandDto::Next => player.next(),
        ConnectCommandDto::Previous => player.previous(),
        ConnectCommandDto::Seek { position_ms } => player.seek(position_ms as f64 / 1000.0),
        ConnectCommandDto::SetVolume { volume } => player.set_volume(volume),
        ConnectCommandDto::LoadQueue {
            song_ids,
            start_index,
            start_position_ms,
            autoplay,
        } => {
            let cached = load_cached_track_ids(core).await;
            let disc = load_pending_discovery_map(core).await;
            let rows = client_core::db::get_tracks_by_ids(core.pool(), &song_ids)
                .await
                .unwrap_or_default();
            let by_id: HashMap<String, LocalTrack> =
                rows.into_iter().map(|t| (t.id.clone(), t)).collect();
            let queue: Vec<PlayerTrack> = song_ids
                .iter()
                .filter_map(|id| by_id.get(id))
                .map(|t| local_track_to_player_track(t, &cached, &disc))
                .collect();
            if !queue.is_empty() {
                let idx = start_index.min(queue.len() - 1);
                let current = queue.get(idx).cloned();
                let start_pos = start_position_ms as f64 / 1000.0;
                let dur = current
                    .as_ref()
                    .and_then(|t| t.duration_secs)
                    .map(f64::from)
                    .unwrap_or(0.0);
                {
                    let mut st = player.state_signal();
                    let mut w = st.write();
                    w.queue = queue;
                    w.queue_index = Some(idx);
                    w.current_track = current.clone();
                    w.status = if autoplay {
                        PlaybackStatus::Loading
                    } else {
                        PlaybackStatus::Paused
                    };
                }
                player.set_progress(start_pos, dur);
                if let Some(track) = current {
                    studio.load_current_track_in_mpv(
                        player,
                        track,
                        !autoplay,
                        (start_pos > 0.0).then_some(start_pos),
                    );
                }
            }
        }
        ConnectCommandDto::TransferPlayback { snapshot, .. } => {
            let cached = load_cached_track_ids(core).await;
            let disc = load_pending_discovery_map(core).await;
            let rows = client_core::db::get_tracks_by_ids(core.pool(), &snapshot.queue_song_ids)
                .await
                .unwrap_or_default();
            let by_id: HashMap<String, LocalTrack> =
                rows.into_iter().map(|t| (t.id.clone(), t)).collect();
            let queue: Vec<PlayerTrack> = snapshot
                .queue_song_ids
                .iter()
                .filter_map(|id| by_id.get(id))
                .map(|t| local_track_to_player_track(t, &cached, &disc))
                .collect();
            if !queue.is_empty() {
                let idx = snapshot.queue_index.min(queue.len() - 1);
                let current = queue.get(idx).cloned();
                let start_pos = snapshot.position_ms as f64 / 1000.0;
                let dur = if snapshot.duration_ms > 0 {
                    snapshot.duration_ms as f64 / 1000.0
                } else {
                    current
                        .as_ref()
                        .and_then(|t| t.duration_secs)
                        .map(f64::from)
                        .unwrap_or(0.0)
                };
                let target_vol = if snapshot.volume.is_finite() {
                    snapshot.volume.clamp(0.0, 1.0)
                } else {
                    player.volume()
                };
                {
                    let mut st = player.state_signal();
                    let mut w = st.write();
                    w.queue = queue;
                    w.queue_index = Some(idx);
                    w.current_track = current.clone();
                    w.volume = target_vol;
                    w.status = if snapshot.is_playing {
                        PlaybackStatus::Loading
                    } else {
                        PlaybackStatus::Paused
                    };
                }
                player.set_progress(start_pos, dur);
                studio.send_mpv(MpvCommand::SetVolume(target_vol));
                if let Some(track) = current {
                    studio.load_current_track_in_mpv(
                        player,
                        track,
                        !snapshot.is_playing,
                        (start_pos > 0.0).then_some(start_pos),
                    );
                }
                studio.push_mini_snapshot(player);
            }
        }
    }
}

fn playback_source_to_mpv(source: PlaybackSource) -> MpvPlaybackSource {
    match source {
        PlaybackSource::LocalFile { path, .. } => MpvPlaybackSource::LocalFile { path },
        PlaybackSource::RemoteStream { url, .. } => MpvPlaybackSource::StreamUrl {
            url,
            headers: Vec::new(),
        },
    }
}

/// Top-level Dioxus provider that owns the `libmpv` worker, `souvlaki` MPRIS2/SMTC bridge,
/// `PlayerProvider`, Mini-Player watch/action channels, and in-process `SoulbeetCore`.
#[component]
pub fn DesktopStudioProvider(children: Element) -> Element {
    let initial_config = use_hook(DesktopConfig::load_from_disk);
    let initial_offline = initial_config.offline_only;
    let initial_volume = initial_config.volume;
    let initial_replaygain = initial_config.replaygain;

    let mut player_state = use_signal(move || {
        let mut st = PlayerState::default();
        st.volume = initial_volume;
        st.offline_only = initial_offline;
        st
    });
    let position_secs = use_signal(|| 0.0f64);
    let duration_secs = use_signal(|| 0.0f64);

    let config = use_signal(move || initial_config);
    let bootstrap = use_signal(|| None::<ClientBootstrap>);
    let core = use_signal(|| None::<Arc<SoulbeetCore>>);
    let core_error = use_signal(|| None::<String>);
    let active_section = use_signal(|| StudioNavSection::Home);
    let selected_playlist_id = use_signal(|| None::<String>);
    let search_seed_query = use_signal(String::new);
    let active_drawer = use_signal(|| None::<DrawerTab>);
    let inspected_track = use_signal(|| None::<PlayerTrack>);
    let cmdk_open = use_signal(|| false);
    let mini_player_open = use_signal(|| false);
    let mini_window = use_signal(|| None::<DesktopContext>);
    let mini_gen = use_signal(|| Arc::new(AtomicU64::new(0)));
    let telemetry = use_signal(|| None::<AudioTelemetry>);
    let sync_status = use_signal(SyncUiStatus::default);
    let cache_stats = use_signal(|| None::<CacheStats>);
    let playlists = use_signal(Vec::<DesktopPlaylist>::new);
    let pending_drops = use_signal(PendingDropQueue::new);
    let in_flight_triage = use_signal(HashSet::<String>::new);
    let optimistic_kept = use_signal(HashSet::<String>::new);
    let discovery_generating = use_signal(|| false);
    let discovery_progress_label = use_signal(|| None::<String>);
    let discovery_progress_pct = use_signal(|| None::<u8>);
    let discovery_refresh_error = use_signal(|| None::<String>);
    let discovery_poll_gen = use_signal(|| Arc::new(AtomicU64::new(0)));
    let acquiring_tracks = use_signal(HashMap::<String, AcquireStageBadge>::new);
    let downloading_albums = use_signal(HashMap::<String, RemoteAlbumDownloadState>::new);
    let connect_devices = use_signal(Vec::<ConnectDevice>::new);
    let lyrics = use_signal(Vec::<TrackLyrics>::new);
    let lyrics_loading = use_signal(|| false);
    let lyrics_error = use_signal(|| None::<String>);
    let guard_modal_track = use_signal(|| None::<PlayerTrack>);
    let guard_preview = use_signal(|| None::<DeleteGuardPreview>);
    let guard_loading = use_signal(|| false);
    let status_toast = use_signal(|| None::<String>);
    let library_revision = use_signal(|| 0u64);
    let mpv_cmd_tx = use_signal(|| None::<mpsc::Sender<MpvCommand>>);
    let mpv_error = use_signal(|| None::<String>);
    let load_gen = use_signal(|| Arc::new(AtomicU64::new(0)));
    let connect_gen = use_signal(|| Arc::new(AtomicU64::new(0)));
    let volume_debounce_gen = use_signal(|| Arc::new(AtomicU64::new(0)));

    let (mini_snap_tx_raw, _) = use_hook(|| watch::channel(MiniPlayerSnapshot::default()));
    let mini_snapshot_tx = use_signal(move || Arc::new(mini_snap_tx_raw));

    let mini_action_pair = use_hook(|| {
        let (tx, rx) = mpsc::unbounded_channel::<MiniPlayerAction>();
        (tx, Arc::new(Mutex::new(Some(rx))))
    });
    let mini_action_tx = use_signal({
        let tx = mini_action_pair.0.clone();
        move || tx
    });
    let mini_action_rx_slot = mini_action_pair.1.clone();

    let os_media_pair = use_hook(|| {
        let (tx, rx) = mpsc::unbounded_channel::<MediaKeyAction>();
        let hwnd = extract_windows_hwnd(&dioxus_desktop::window());
        let bridge = MediaControlsBridge::init(hwnd, tx);
        (Arc::new(Mutex::new(bridge)), Arc::new(Mutex::new(Some(rx))))
    });
    let media_controls = use_signal({
        let mc = os_media_pair.0.clone();
        move || mc
    });
    let os_media_rx_slot = os_media_pair.1.clone();
    let scrobbler = use_signal(|| Arc::new(Mutex::new(ScrobbleTracker::new())));

    // Reset LC_NUMERIC=C right after GTK3 initializes the window and before libmpv starts!
    let mpv_events_slot = use_hook(|| {
        ensure_c_numeric_locale();
        match MpvEngine::new(initial_replaygain, initial_volume) {
            Ok(mut engine) => {
                let cmd_tx = engine.command_sender();
                let events = engine.take_events();
                (
                    Arc::new(Mutex::new(Some(engine))),
                    Some(cmd_tx),
                    Arc::new(Mutex::new(events)),
                    None::<String>,
                )
            }
            Err(err) => {
                tracing::error!("Failed to initialize native libmpv: {err}");
                (
                    Arc::new(Mutex::new(None)),
                    None,
                    Arc::new(Mutex::new(None)),
                    Some(err),
                )
            }
        }
    });
    let _mpv_guard = mpv_events_slot.0.clone();

    // Seed mpv command sender & error once
    use_hook({
        let mut mpv_cmd_tx = mpv_cmd_tx;
        let mut mpv_error = mpv_error;
        let tx = mpv_events_slot.1.clone();
        let err = mpv_events_slot.3.clone();
        move || {
            mpv_cmd_tx.set(tx);
            mpv_error.set(err);
        }
    });

    // Register local `/cover-art/{id}` asset handler for zero-network offline artwork
    use_cover_art_asset_handler(move || core.peek().clone());

    let studio = DesktopStudio {
        config,
        bootstrap,
        core,
        core_error,
        active_section,
        selected_playlist_id,
        search_seed_query,
        active_drawer,
        inspected_track,
        cmdk_open,
        mini_player_open,
        mini_window,
        mini_gen,
        telemetry,
        sync_status,
        cache_stats,
        playlists,
        pending_drops,
        in_flight_triage,
        optimistic_kept,
        discovery_generating,
        discovery_progress_label,
        discovery_progress_pct,
        discovery_refresh_error,
        discovery_poll_gen,
        acquiring_tracks,
        downloading_albums,
        connect_devices,
        lyrics,
        lyrics_loading,
        lyrics_error,
        guard_modal_track,
        guard_preview,
        guard_loading,
        status_toast,
        library_revision,
        mpv_cmd_tx,
        mpv_error,
        load_gen,
        connect_gen,
        volume_debounce_gen,
        mini_snapshot_tx,
        mini_action_tx,
        media_controls,
        scrobbler,
    };
    use_context_provider(|| studio);

    // Ensure floating Mini-Player closes cleanly when the main Studio window is torn down.
    use_drop(move || {
        let mut st = studio;
        st.close_mini_player();
    });

    let on_command = use_callback(move |cmd: PlayerCommand| {
        let mut st = studio;
        let player = PlayerContext::new(
            player_state,
            position_secs,
            duration_secs,
            Callback::new(move |_| {}),
        );
        st.handle_player_command(player, cmd);
    });

    let player_ctx = PlayerContext::new(player_state, position_secs, duration_secs, on_command);

    // 1. Auto-initialize SoulbeetCore if already configured on disk
    use_hook(move || {
        let cfg = config.peek().clone();
        if cfg.is_configured() {
            let mut st = studio;
            st.connect_with_config(cfg, None, player_ctx);
        }
    });

    // 2. Pump `MpvEvent` stream into `PlayerContext`, Telemetry, Scrobbler, MPRIS2, and Mini-Player
    let mpv_rx_slot = mpv_events_slot.2.clone();
    use_future(move || {
        let rx_slot = mpv_rx_slot.clone();
        let mut st = studio;
        let mut p_ctx = player_ctx;
        async move {
            let Some(mut rx) = rx_slot.lock().ok().and_then(|mut g| g.take()) else {
                return;
            };
            while let Some(ev) = rx.recv().await {
                match ev {
                    MpvEvent::Progress {
                        position_secs: pos,
                        duration_secs: dur,
                    } => {
                        p_ctx.set_progress(pos, dur);
                        let is_playing = player_state.peek().status.is_playing();
                        let scrobble_due = st
                            .scrobbler
                            .peek()
                            .lock()
                            .ok()
                            .and_then(|mut sc| sc.tick(Instant::now(), is_playing, dur));
                        if let Some(mutation) = scrobble_due {
                            if let Some(c) = st.core.peek().clone() {
                                spawn(async move {
                                    let _ = c.enqueue_mutation(mutation).await;
                                    if !c.is_offline_only() {
                                        let _ = c.drain_outbox().await;
                                    }
                                });
                            }
                        }
                        st.push_mini_snapshot(p_ctx);
                    }
                    MpvEvent::Telemetry(t) => {
                        let badge = t.format_codec_badge();
                        st.telemetry.set(Some(t));
                        if let Some(b) = badge {
                            let mut w = player_state.write();
                            if let Some(ref mut cur) = w.current_track {
                                cur.codec_badge = Some(b);
                            }
                        }
                        st.push_mini_snapshot(p_ctx);
                    }
                    MpvEvent::StateChanged(mpv_st) => {
                        let mapped = match mpv_st {
                            MpvPlaybackState::Idle => PlaybackStatus::Idle,
                            MpvPlaybackState::Loading => PlaybackStatus::Loading,
                            MpvPlaybackState::Buffering => PlaybackStatus::Buffering,
                            MpvPlaybackState::Playing => PlaybackStatus::Playing,
                            MpvPlaybackState::Paused => PlaybackStatus::Paused,
                        };
                        let pos = *position_secs.peek();
                        let dur = *duration_secs.peek();
                        player_state.write().status = mapped.clone();
                        let is_playing = mapped.is_playing();
                        let _ = st
                            .scrobbler
                            .peek()
                            .lock()
                            .ok()
                            .and_then(|mut sc| sc.tick(Instant::now(), is_playing, dur));
                        if let Ok(mut mc) = st.media_controls.peek().lock() {
                            mc.update_playback(&mapped, pos);
                        }
                        st.push_mini_snapshot(p_ctx);
                    }
                    MpvEvent::TrackEnded { gapless_advanced } => {
                        if gapless_advanced {
                            st.handle_gapless_advance(p_ctx);
                        } else {
                            st.handle_player_command(p_ctx, PlayerCommand::TrackEnded);
                        }
                    }
                    MpvEvent::Error(msg) => {
                        player_state.write().status = PlaybackStatus::Error(msg);
                        st.push_mini_snapshot(p_ctx);
                    }
                }
            }
        }
    });

    // 3. Pump OS media key events (`souvlaki` MPRIS2 / SMTC)
    use_future(move || {
        let rx_slot = os_media_rx_slot.clone();
        let mut st = studio;
        let p_ctx = player_ctx;
        async move {
            let Some(mut rx) = rx_slot.lock().ok().and_then(|mut g| g.take()) else {
                return;
            };
            while let Some(action) = rx.recv().await {
                match action {
                    MediaKeyAction::Play => {
                        if !player_state.peek().status.is_playing() {
                            st.handle_player_command(p_ctx, PlayerCommand::TogglePlayPause);
                        }
                    }
                    MediaKeyAction::Pause => {
                        if player_state.peek().status.is_playing() {
                            st.handle_player_command(p_ctx, PlayerCommand::TogglePlayPause);
                        }
                    }
                    MediaKeyAction::Toggle => {
                        st.handle_player_command(p_ctx, PlayerCommand::TogglePlayPause);
                    }
                    MediaKeyAction::Other(OsMediaCommand::Player(cmd)) => {
                        st.handle_player_command(p_ctx, *cmd);
                    }
                    MediaKeyAction::Other(OsMediaCommand::SetShuffle(target)) => {
                        if player_state.peek().shuffle != target {
                            st.handle_player_command(p_ctx, PlayerCommand::ToggleShuffle);
                        }
                    }
                    MediaKeyAction::Other(OsMediaCommand::SeekRelative(delta)) => {
                        let cur = *position_secs.peek();
                        st.handle_player_command(
                            p_ctx,
                            PlayerCommand::Seek((cur + delta).max(0.0)),
                        );
                    }
                    MediaKeyAction::Other(OsMediaCommand::Stop) => {
                        st.handle_player_command(p_ctx, PlayerCommand::ClearQueue);
                    }
                    MediaKeyAction::Other(OsMediaCommand::RaiseWindow) => {
                        dioxus_desktop::window().set_focus();
                    }
                    MediaKeyAction::Other(OsMediaCommand::Quit) => {
                        st.close_mini_player();
                        dioxus_desktop::window().close();
                    }
                }
            }
        }
    });

    // 4. Pump floating Mini-Player window actions back into Studio controller
    use_future(move || {
        let rx_slot = mini_action_rx_slot.clone();
        let mut st = studio;
        let p_ctx = player_ctx;
        async move {
            let Some(mut rx) = rx_slot.lock().ok().and_then(|mut g| g.take()) else {
                return;
            };
            while let Some(action) = rx.recv().await {
                match action {
                    MiniPlayerAction::TogglePlayPause => {
                        st.handle_player_command(p_ctx, PlayerCommand::TogglePlayPause);
                    }
                    MiniPlayerAction::Next => {
                        st.handle_player_command(p_ctx, PlayerCommand::Next);
                    }
                    MiniPlayerAction::Previous => {
                        st.handle_player_command(p_ctx, PlayerCommand::Previous);
                    }
                    MiniPlayerAction::Seek(pos) => {
                        st.handle_player_command(p_ctx, PlayerCommand::Seek(pos));
                    }
                    MiniPlayerAction::Star(track, starred) => {
                        st.star_track(p_ctx, track, starred);
                    }
                    MiniPlayerAction::Keep(track) => {
                        st.keep_track(p_ctx, track);
                    }
                    MiniPlayerAction::Drop(track) => {
                        st.drop_track(p_ctx, track);
                    }
                    MiniPlayerAction::RestoreStudio => {
                        st.close_mini_player();
                        dioxus_desktop::window().set_focus();
                    }
                    MiniPlayerAction::Closed(closed_gen) => {
                        if st.mini_gen.peek().load(Ordering::SeqCst) == closed_gen {
                            st.mini_window.set(None);
                            st.mini_player_open.set(false);
                        }
                    }
                }
            }
        }
    });

    rsx! {
        ui::PlayerProvider {
            state: player_state,
            position_secs,
            duration_secs,
            on_command,
            {children}
        }
    }
}

/// Stable key for tracking per-row Acquire & Play state on Desktop remote catalog results.
/// Prefers non-empty recording MBID, then non-empty provider track ID (e.g. `deezer:track:...`),
/// then Unicode-lowercased `artist::title::album` so distinct tracks never collide.
pub fn remote_acquire_key(
    mbid: Option<&str>,
    track_id: Option<&str>,
    artist: &str,
    title: &str,
    album: Option<&str>,
) -> String {
    if let Some(m) = mbid.map(str::trim).filter(|m| !m.is_empty()) {
        return format!("mbid:{}", m.to_ascii_lowercase());
    }
    if let Some(id) = track_id.map(str::trim).filter(|id| !id.is_empty()) {
        return format!("id:{}", id.to_lowercase());
    }
    format!(
        "{}::{}::{}",
        artist.trim().to_lowercase(),
        title.trim().to_lowercase(),
        album.unwrap_or_default().trim().to_lowercase()
    )
}

/// Convenience wrapper computing [`remote_acquire_key`] from a [`RemoteCatalogTrack`].
pub fn remote_track_acquire_key(remote: &RemoteCatalogTrack) -> String {
    remote_acquire_key(
        Some(&remote.mbid),
        Some(&remote.id),
        &remote.artist,
        &remote.title,
        Some(&remote.album),
    )
}

/// Matches a newly imported local track against the requested remote track using canonical
/// Unicode/accent/edition normalization so accents, smart quotes, and `(Remastered)` tags match.
pub fn acquired_track_matches(
    local_artist: &str,
    local_title: &str,
    remote_artist: &str,
    remote_title: &str,
) -> bool {
    shared::search_norm::track_dedup_key(local_artist, local_title)
        == shared::search_norm::track_dedup_key(remote_artist, remote_title)
}

/// Stable key for tracking per-album download state in Desktop Search.
pub fn remote_album_download_key(album: &RemoteCatalogAlbum) -> String {
    let trimmed_id = album.id.trim();
    if !trimmed_id.is_empty() {
        return trimmed_id.to_lowercase();
    }
    format!(
        "{}::{}",
        album.artist.trim().to_lowercase(),
        album.title.trim().to_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrobble_tracker_enforces_opensubsonic_duration_and_monotonic_time() {
        let mut tracker = ScrobbleTracker::new();
        let t0 = Instant::now();

        let np_short = tracker.start_track("short-1".into(), 22.0, 1_700_000_000);
        assert!(matches!(
            np_short,
            Some(OutboxMutation::Scrobble {
                submission: false,
                ..
            })
        ));
        for step in 1..=20 {
            assert!(tracker
                .tick(t0 + Duration::from_secs(step * 2), true, 22.0)
                .is_none());
        }
        assert!(!tracker.is_submitted());

        let _ = tracker.start_track("song-200".into(), 200.0, 1_700_000_100);
        assert_eq!(ScrobbleTracker::scrobble_threshold_secs(200.0), Some(100.0));
        assert_eq!(ScrobbleTracker::scrobble_threshold_secs(900.0), Some(240.0));
        assert_eq!(ScrobbleTracker::scrobble_threshold_secs(45.0), Some(30.0));

        let mut cursor = t0;
        assert!(tracker.tick(cursor, true, 200.0).is_none());
        let mut submitted = None;
        for _ in 0..25 {
            cursor += Duration::from_secs(4);
            if let Some(m) = tracker.tick(cursor, true, 200.0) {
                submitted = Some(m);
            }
        }
        assert!(tracker.accumulated_secs() >= 100.0);
        assert!(matches!(
            submitted,
            Some(OutboxMutation::Scrobble {
                ref song_id,
                submission: true,
                played_at_unix: 1_700_000_100,
            }) if song_id == "song-200"
        ));

        cursor += Duration::from_secs(4);
        assert!(tracker.tick(cursor, true, 200.0).is_none());
    }

    #[test]
    fn desktop_config_roundtrips_json_sets_0600_permissions_and_normalizes_tokens() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("subdir").join("config.json");

        let cfg = DesktopConfig {
            soulbeet_base_url: Some("https://music.example.com/".into()),
            auth_token: Some("Bearer secret-tok".into()),
            auth_kind: DesktopAuthKind::Bearer,
            navidrome: Some(NavidromeCredentials {
                base_url: "https://nav.example.com".into(),
                username: "alice".into(),
                subsonic_salt: "salt123".into(),
                subsonic_token: "md5tok".into(),
                api_version: "1.16.1".into(),
                client_name: "Soulbeet".into(),
            }),
            replaygain: ReplayGainMode::Album,
            oled_mode: true,
            offline_only: true,
            cache_max_gb: 12,
            volume: 0.72,
            ..Default::default()
        };
        assert!(cfg.is_configured());
        cfg.save_to_path(&cfg_path).expect("save config");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&cfg_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600, "config.json must be owner-read/write only");
            let dir_mode = std::fs::metadata(cfg_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "config parent dir must be 0o700");
        }

        let loaded = DesktopConfig::load_from_path(&cfg_path).expect("load config");
        assert_eq!(loaded.replaygain, ReplayGainMode::Album);
        assert!(loaded.oled_mode);
        assert!(loaded.offline_only);
        assert_eq!(loaded.cache_max_gb, 12);
        assert_eq!(
            loaded.soulbeet_auth(),
            Some(SoulbeetAuth::Bearer("secret-tok".into()))
        );
        assert_eq!(
            normalize_auth_token("auth_token=abc123; Path=/", DesktopAuthKind::SessionCookie),
            "abc123"
        );
        assert_eq!(
            extract_auth_cookie_from_header("auth_token=xyz999; HttpOnly; SameSite=Lax").as_deref(),
            Some("xyz999")
        );

        let client_cfg = loaded.to_client_config();
        assert_eq!(
            client_cfg.soulbeet_base_url.as_deref(),
            Some("https://music.example.com")
        );
        assert_eq!(client_cfg.cache_policy.max_bytes, 12 * 1024 * 1024 * 1024);
        assert_eq!(client_cfg.platform, CoreDevicePlatform::Desktop);
    }

    #[test]
    fn fail_closed_guard_preview_always_blocks_permanent_deletion() {
        let preview = fail_closed_guard_preview("song-1".into(), "Offline".into());
        assert!(!preview.can_delete);
        assert!(!preview.sole_owner);
        assert_eq!(classify_guard_mode(&preview), GuardModalMode::HardBlocked);
    }

    #[test]
    fn offline_collection_filter_retains_only_cached_tracks() {
        let tracks = vec![
            PlayerTrack {
                id: "cached".into(),
                is_offline_cached: true,
                ..Default::default()
            },
            PlayerTrack {
                id: "uncached".into(),
                is_offline_cached: false,
                ..Default::default()
            },
        ];
        assert_eq!(filter_tracks_for_offline_play(tracks.clone(), false).len(), 2);
        let offline = filter_tracks_for_offline_play(tracks, true);
        assert_eq!(offline.len(), 1);
        assert_eq!(offline[0].id, "cached");
    }

    #[test]
    fn connect_device_visible_equality_ignores_keepalive_timestamps() {
        let base = ConnectDevice {
            device_id: "desk-1".into(),
            name: "Soulbeet Desktop".into(),
            platform: SharedDevicePlatform::Desktop,
            is_active: true,
            last_seen_unix: 1_000,
            snapshot: Some(PlaybackSnapshot {
                song_id: Some("track-1".into()),
                title: Some("Track".into()),
                artist: Some("Artist".into()),
                album: None,
                cover_art_id: None,
                is_playing: true,
                position_ms: 5_000,
                duration_ms: 180_000,
                queue_song_ids: vec!["track-1".into()],
                queue_index: 0,
                volume: 0.85,
                is_pending_discovery: false,
                updated_at_unix: 1_000,
            }),
        };
        let mut keepalive = base.clone();
        keepalive.last_seen_unix = 1_025;
        if let Some(ref mut snap) = keepalive.snapshot {
            snap.updated_at_unix = 1_025;
        }
        assert!(connect_devices_visibly_equal(
            std::slice::from_ref(&base),
            &[keepalive]
        ));

        let mut changed = base.clone();
        changed.name = "Renamed".into();
        assert!(!connect_devices_visibly_equal(&[base], &[changed]));
    }

    #[test]
    fn cover_art_url_encoding_and_decoding_roundtrip() {
        let url = desktop_cover_art_url(Some("al-123/456")).unwrap();
        assert_eq!(url, "/cover-art/al-123%2F456");
        assert_eq!(percent_decode("al-123%2F456"), "al-123/456");
        assert_eq!(desktop_cover_art_url(Some("  ")), None);
    }

    #[test]
    fn triage_guard_keys_collects_both_non_empty_deduplicated_keys() {
        assert_eq!(
            triage_guard_keys(Some("disc-1"), "song-9"),
            vec!["disc-1".to_string(), "song-9".to_string()]
        );
        assert_eq!(
            triage_guard_keys(Some("same"), "same"),
            vec!["same".to_string()]
        );
        assert_eq!(triage_guard_keys(Some("disc-1"), ""), vec!["disc-1".to_string()]);
        assert_eq!(triage_guard_keys(None, "song-9"), vec!["song-9".to_string()]);
        assert!(triage_guard_keys(Some(""), "").is_empty());
    }

    #[test]
    fn discovery_progress_summary_and_error_parser_handle_running_and_already_running() {
        let running = DiscoveryProgress {
            status: GenerationStatus::Running,
            profiles: vec![
                shared::navidrome::ProfileProgress {
                    profile: "Balanced".into(),
                    phase: ProfilePhase::Done,
                    current: 10,
                    total: 10,
                },
                shared::navidrome::ProfileProgress {
                    profile: "Adventurous".into(),
                    phase: ProfilePhase::Downloading,
                    current: 5,
                    total: 10,
                },
            ],
            ..Default::default()
        };
        let (label, pct) = desktop_discovery_progress_summary(&running);
        assert_eq!(label, "Adventurous: Downloading (5/10)");
        assert_eq!(pct, 75);

        let already_json = serde_json::json!({
            "message": "Discovery generation already in progress",
            "code": 409,
            "data": { "code": "already_running" }
        })
        .to_string();
        let (msg, already) = parse_server_error_message(&already_json, 409);
        assert_eq!(msg, "Discovery generation already in progress");
        assert!(already);

        let other_json = serde_json::json!({
            "message": "Discovery is not enabled",
            "code": 409,
            "data": { "code": "invalid_input" }
        })
        .to_string();
        let (msg, already) = parse_server_error_message(&other_json, 409);
        assert_eq!(msg, "Discovery is not enabled");
        assert!(!already);
    }

    #[test]
    fn remote_acquire_key_prefers_mbid_then_track_id_then_artist_title_album() {
        let k1 = remote_acquire_key(
            Some("11111111-1111-1111-1111-111111111111"),
            Some("deezer:track:100"),
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Agents of Fortune"),
        );
        let k2 = remote_acquire_key(
            Some("22222222-2222-2222-2222-222222222222"),
            Some("deezer:track:100"),
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Greatest Hits"),
        );
        assert_ne!(k1, k2);
        assert_eq!(k1, "mbid:11111111-1111-1111-1111-111111111111");

        let deezer_a = remote_acquire_key(
            Some(""),
            Some("deezer:track:101"),
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Agents of Fortune"),
        );
        let deezer_b = remote_acquire_key(
            None,
            Some("deezer:track:202"),
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Agents of Fortune"),
        );
        assert_ne!(deezer_a, deezer_b);
        assert_eq!(deezer_a, "id:deezer:track:101");

        let fallback_a = remote_acquire_key(
            None,
            Some("  "),
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Agents of Fortune"),
        );
        let fallback_b = remote_acquire_key(
            None,
            None,
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            Some("Greatest Hits"),
        );
        assert_ne!(fallback_a, fallback_b);
        assert_eq!(
            fallback_a,
            "blue öyster cult::(don't fear) the reaper::agents of fortune"
        );
    }

    #[test]
    fn acquired_track_matches_normalizes_accents_smart_quotes_and_remaster_tags() {
        assert!(acquired_track_matches(
            "Blue Öyster Cult",
            "(Don’t Fear) The Reaper (Remastered 2001)",
            "Blue Oyster Cult",
            "(Don't Fear) The Reaper",
        ));
        assert!(acquired_track_matches(
            "Hermanos Gutiérrez",
            "El Bueno y el Malo",
            "Hermanos Gutierrez",
            "El Bueno y el Malo - 2022 Remaster",
        ));
        assert!(!acquired_track_matches(
            "Blue Öyster Cult",
            "Godzilla",
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
        ));
    }
}
