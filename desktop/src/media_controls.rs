//! Cross-platform OS media controls integration via `souvlaki`
//! (Linux MPRIS2 over D-Bus & Windows SMTC).
//!
//! Gracefully degrades to a no-op with a warning log in headless / D-Bus-free environments
//! or when no Windows HWND is available so the desktop app and tests never panic.

use std::path::{Path, PathBuf};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tokio::sync::mpsc;
use ui::{PlaybackStatus, PlayerCommand};

/// High-level commands received from OS media keys / MPRIS2 / Windows SMTC.
#[derive(Debug, Clone, PartialEq)]
pub enum OsMediaCommand {
    Player(Box<PlayerCommand>),
    SeekRelative(f64),
    SetShuffle(bool),
    Stop,
    RaiseWindow,
    Quit,
}

/// Metadata payload pushed to MPRIS2 / SMTC when the active track changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaTrackMeta {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub local_cover_art_path: Option<PathBuf>,
    pub fallback_cover_url: Option<String>,
}

/// Converts a local cached artwork path into a platform-appropriate `file://` URI accepted by
/// Linux MPRIS2 (`file:///path/to/art.jpg`) and Windows SMTC (`file://C:\path\to\art.jpg`,
/// avoiding the leading slash rejected by WinRT `StorageFile::GetFileFromPathAsync`).
pub fn format_cover_art_uri_for_platform(
    local_path: Option<&Path>,
    fallback_url: Option<&str>,
    is_windows: bool,
) -> Option<String> {
    if let Some(path) = local_path {
        if is_windows {
            let raw = path.display().to_string();
            if !raw.trim().is_empty() {
                return Some(format!("file://{raw}"));
            }
        } else if let Ok(url) = url::Url::from_file_path(path) {
            return Some(url.to_string());
        }
    }
    fallback_url
        .map(str::trim)
        .filter(|u| {
            u.starts_with("http://") || u.starts_with("https://") || u.starts_with("file://")
        })
        .map(ToString::to_string)
}

pub fn format_cover_art_uri(
    local_path: Option<&Path>,
    fallback_url: Option<&str>,
) -> Option<String> {
    format_cover_art_uri_for_platform(local_path, fallback_url, cfg!(target_os = "windows"))
}

/// Translates a [`souvlaki::MediaControlEvent`] into an [`OsMediaCommand`].
pub fn map_media_control_event(event: MediaControlEvent) -> Option<OsMediaCommand> {
    match event {
        MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
            Some(OsMediaCommand::Player(Box::new(
                PlayerCommand::TogglePlayPause,
            )))
        }
        MediaControlEvent::Next => Some(OsMediaCommand::Player(Box::new(PlayerCommand::Next))),
        MediaControlEvent::Previous => {
            Some(OsMediaCommand::Player(Box::new(PlayerCommand::Previous)))
        }
        MediaControlEvent::Stop => Some(OsMediaCommand::Stop),
        MediaControlEvent::SetPosition(MediaPosition(pos)) => Some(OsMediaCommand::Player(
            Box::new(PlayerCommand::Seek(pos.as_secs_f64())),
        )),
        MediaControlEvent::Seek(dir) => {
            let delta = match dir {
                SeekDirection::Forward => 5.0,
                SeekDirection::Backward => -5.0,
            };
            Some(OsMediaCommand::SeekRelative(delta))
        }
        MediaControlEvent::SeekBy(dir, dur) => {
            let secs = dur.as_secs_f64();
            let delta = match dir {
                SeekDirection::Forward => secs,
                SeekDirection::Backward => -secs,
            };
            Some(OsMediaCommand::SeekRelative(delta))
        }
        MediaControlEvent::SetVolume(vol) => {
            if vol.is_finite() {
                Some(OsMediaCommand::Player(Box::new(PlayerCommand::SetVolume(
                    (vol as f32).clamp(0.0, 1.0),
                ))))
            } else {
                None
            }
        }
        MediaControlEvent::SetShuffle(enabled) => Some(OsMediaCommand::SetShuffle(enabled)),
        MediaControlEvent::Raise => Some(OsMediaCommand::RaiseWindow),
        MediaControlEvent::Quit => Some(OsMediaCommand::Quit),
        _ => None,
    }
}

/// Explicit Play/Pause aware mapping so OS `Play` never accidentally pauses an already playing track.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaKeyAction {
    Play,
    Pause,
    Toggle,
    Other(OsMediaCommand),
}

pub fn map_media_key_action(event: MediaControlEvent) -> Option<MediaKeyAction> {
    match event {
        MediaControlEvent::Play => Some(MediaKeyAction::Play),
        MediaControlEvent::Pause => Some(MediaKeyAction::Pause),
        MediaControlEvent::Toggle => Some(MediaKeyAction::Toggle),
        other => map_media_control_event(other).map(MediaKeyAction::Other),
    }
}

/// Managed wrapper around `souvlaki::MediaControls`.
pub struct MediaControlsBridge {
    controls: Option<MediaControls>,
}

impl MediaControlsBridge {
    /// Initializes OS media controls. Pass `hwnd` on Windows (`None` on Linux/macOS).
    /// Logs a warning and degrades cleanly if D-Bus / SMTC / HWND is unavailable.
    pub fn init(
        hwnd: Option<*mut std::ffi::c_void>,
        tx: mpsc::UnboundedSender<MediaKeyAction>,
    ) -> Self {
        #[cfg(target_os = "windows")]
        let platform_hwnd = {
            if hwnd.is_none() {
                tracing::warn!(
                    "Windows HWND unavailable; disabling SMTC OS media controls gracefully"
                );
                return Self { controls: None };
            }
            hwnd
        };
        #[cfg(not(target_os = "windows"))]
        let platform_hwnd = {
            let _ = hwnd;
            None
        };

        let config = PlatformConfig {
            dbus_name: "org.soulbeet.Desktop",
            display_name: "Soulbeet",
            hwnd: platform_hwnd,
        };

        match MediaControls::new(config) {
            Ok(mut controls) => {
                if let Err(err) = controls.attach(move |event| {
                    if let Some(action) = map_media_key_action(event) {
                        let _ = tx.send(action);
                    }
                }) {
                    tracing::warn!("Failed to attach OS media controls listener: {err:?}");
                    return Self { controls: None };
                }
                tracing::info!("Initialized OS media controls (MPRIS2 / SMTC)");
                Self {
                    controls: Some(controls),
                }
            }
            Err(err) => {
                tracing::warn!(
                    "OS media controls unavailable (headless or no D-Bus session): {err:?}"
                );
                Self { controls: None }
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.controls.is_some()
    }

    pub fn update_metadata(&mut self, meta: &MediaTrackMeta) {
        let Some(controls) = self.controls.as_mut() else {
            return;
        };
        let cover_uri = format_cover_art_uri(
            meta.local_cover_art_path.as_deref(),
            meta.fallback_cover_url.as_deref(),
        );
        let duration = meta.duration_secs.map(|s| Duration::from_secs(u64::from(s)));
        let payload = MediaMetadata {
            title: Some(meta.title.as_str()),
            artist: Some(meta.artist.as_str()),
            album: meta.album.as_deref(),
            cover_url: cover_uri.as_deref(),
            duration,
        };
        if let Err(err) = controls.set_metadata(payload) {
            tracing::debug!("Failed to update OS media metadata: {err:?}");
        }
    }

    pub fn update_playback(&mut self, status: &PlaybackStatus, position_secs: f64) {
        let Some(controls) = self.controls.as_mut() else {
            return;
        };
        let progress = if position_secs.is_finite() && position_secs >= 0.0 {
            Some(MediaPosition(Duration::from_secs_f64(position_secs)))
        } else {
            None
        };
        let playback = match status {
            PlaybackStatus::Playing | PlaybackStatus::Loading | PlaybackStatus::Buffering => {
                MediaPlayback::Playing { progress }
            }
            PlaybackStatus::Paused => MediaPlayback::Paused { progress },
            PlaybackStatus::Idle | PlaybackStatus::Error(_) => MediaPlayback::Stopped,
        };
        if let Err(err) = controls.set_playback(playback) {
            tracing::debug!("Failed to update OS media playback state: {err:?}");
        }
    }

    pub fn update_volume(&mut self, volume: f32) {
        #[cfg(target_os = "linux")]
        if let Some(controls) = self.controls.as_mut() {
            let _ = controls.set_volume(f64::from(volume.clamp(0.0, 1.0)));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = volume;
        }
    }
}

/// Extracts the native Win32 HWND from a `dioxus_desktop::DesktopContext` for SMTC on Windows.
#[cfg(target_os = "windows")]
pub fn extract_windows_hwnd(
    desktop: &dioxus_desktop::DesktopContext,
) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = desktop.window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn extract_windows_hwnd(
    _desktop: &dioxus_desktop::DesktopContext,
) -> Option<*mut std::ffi::c_void> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_local_file_uris_for_linux_and_windows_smtc() {
        let linux_uri = format_cover_art_uri_for_platform(
            Some(Path::new("/tmp/soulbeet cache/cover 01.jpg")),
            Some("https://example.com/fallback.jpg"),
            false,
        );
        assert_eq!(
            linux_uri.as_deref(),
            Some("file:///tmp/soulbeet%20cache/cover%2001.jpg")
        );

        let win_uri = format_cover_art_uri_for_platform(
            Some(Path::new(r"C:\Users\alice\AppData\Local\Soulbeet\art.jpg")),
            None,
            true,
        );
        assert_eq!(
            win_uri.as_deref(),
            Some(r"file://C:\Users\alice\AppData\Local\Soulbeet\art.jpg")
        );

        let fallback = format_cover_art_uri(None, Some("https://music.example.com/art.png"));
        assert_eq!(
            fallback.as_deref(),
            Some("https://music.example.com/art.png")
        );

        // Rejects relative webview URLs that MPRIS2/SMTC cannot fetch
        assert_eq!(format_cover_art_uri(None, Some("/cover-art/abc")), None);
    }

    #[test]
    fn maps_os_media_key_events_including_explicit_shuffle_bool() {
        assert_eq!(
            map_media_key_action(MediaControlEvent::Play),
            Some(MediaKeyAction::Play)
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::Pause),
            Some(MediaKeyAction::Pause)
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::Toggle),
            Some(MediaKeyAction::Toggle)
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::SetShuffle(true)),
            Some(MediaKeyAction::Other(OsMediaCommand::SetShuffle(true)))
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::SetShuffle(false)),
            Some(MediaKeyAction::Other(OsMediaCommand::SetShuffle(false)))
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::Next),
            Some(MediaKeyAction::Other(OsMediaCommand::Player(Box::new(
                PlayerCommand::Next
            ))))
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::SetPosition(MediaPosition(
                Duration::from_secs(42)
            ))),
            Some(MediaKeyAction::Other(OsMediaCommand::Player(Box::new(
                PlayerCommand::Seek(42.0)
            ))))
        );
        assert_eq!(
            map_media_key_action(MediaControlEvent::SeekBy(
                SeekDirection::Backward,
                Duration::from_secs(10)
            )),
            Some(MediaKeyAction::Other(OsMediaCommand::SeekRelative(-10.0)))
        );
    }
}
