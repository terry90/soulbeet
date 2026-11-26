//! Native `libmpv` audio playback engine for Soulbeet Desktop.
//!
//! Runs `libmpv2` on a dedicated worker thread communicating over bounded `tokio::sync::mpsc`
//! commands and emitting structured progress, gapless transition, and live audio telemetry events.
//! Supports seamless playback of both local offline cached files (`PlaybackSource::LocalFile`)
//! and Navidrome OpenSubsonic HTTP range streams (`PlaybackSource::RemoteStream`) with
//! gapless next-track preloading (`gapless-audio=yes`, `prefetch-playlist=yes`) and ReplayGain.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libmpv2::events::{Event, PropertyData};
use libmpv2::{mpv_end_file_reason, Format, Mpv};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Forces `LC_NUMERIC=C` on Unix before initializing `libmpv`.
///
/// GTK3 (`dioxus-desktop`) initializes the process locale from the user's environment
/// (e.g. `fr_FR.UTF-8`, `de_DE.UTF-8`), which causes `mpv_initialize()` to fail unless
/// `LC_NUMERIC` is reset to `"C"`.
pub fn ensure_c_numeric_locale() {
    #[cfg(unix)]
    unsafe {
        libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr());
    }
}

/// ReplayGain loudness normalization mode configured in `libmpv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayGainMode {
    #[default]
    Track,
    Album,
    Off,
}

impl ReplayGainMode {
    pub const fn as_mpv_value(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Off => "no",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Track => "Track",
            Self::Album => "Album",
            Self::Off => "Off",
        }
    }

    pub const fn cycle(self) -> Self {
        match self {
            Self::Track => Self::Album,
            Self::Album => Self::Off,
            Self::Off => Self::Track,
        }
    }
}

/// Source passed to `MpvEngine` for immediate playback or gapless prefetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpvPlaybackSource {
    LocalFile {
        path: String,
    },
    StreamUrl {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl MpvPlaybackSource {
    pub fn uri(&self) -> &str {
        match self {
            Self::LocalFile { path } => path.as_str(),
            Self::StreamUrl { url, .. } => url.as_str(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::LocalFile { .. })
    }
}

/// Live audio decoder & output telemetry queried directly from `libmpv`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioTelemetry {
    pub codec_name: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub sample_format: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub replaygain_mode: ReplayGainMode,
    pub is_local_cache: bool,
}

impl AudioTelemetry {
    /// Formats a crisp Commit Mono badge for the player bar and Track Inspector:
    /// - Lossless codecs (`flac`, `alac`, `wav`, `pcm_*`, `wavpack`, `ape`): e.g. `FLAC · 24/96`, `FLAC · 16/44.1`
    /// - Lossy codecs (`opus`, `mp3`, `aac`, `vorbis`): e.g. `OPUS 160`, `MP3 320`
    pub fn format_codec_badge(&self) -> Option<String> {
        let raw_codec = self.codec_name.as_deref()?.trim();
        if raw_codec.is_empty() {
            return None;
        }
        let lower = raw_codec.to_ascii_lowercase();
        let display_codec = normalize_codec_label(&lower);

        let is_lossless = matches!(
            lower.as_str(),
            "flac" | "alac" | "wav" | "wavpack" | "ape" | "tta" | "dsd_lsbf" | "dsd_msbf"
        ) || lower.starts_with("pcm");

        if is_lossless {
            let bit_depth = self
                .sample_format
                .as_deref()
                .and_then(parse_bit_depth_from_mpv_format);
            let khz = self.sample_rate_hz.map(format_khz);
            match (bit_depth, khz) {
                (Some(bits), Some(rate)) => Some(format!("{display_codec} · {bits}/{rate}")),
                (None, Some(rate)) => Some(format!("{display_codec} · {rate}kHz")),
                (Some(bits), None) => Some(format!("{display_codec} · {bits}-bit")),
                (None, None) => Some(display_codec.to_string()),
            }
        } else if let Some(bps) = self.bitrate_bps.filter(|&b| b >= 1_000) {
            let kbps = (bps + 500) / 1_000;
            Some(format!("{display_codec} {kbps}"))
        } else if let Some(rate) = self.sample_rate_hz.map(format_khz) {
            Some(format!("{display_codec} · {rate}kHz"))
        } else {
            Some(display_codec.to_string())
        }
    }

    /// Formats a detailed signal-chain summary for the Track Inspector drawer.
    pub fn format_signal_chain(&self) -> String {
        let mut parts = Vec::new();
        if let Some(badge) = self.format_codec_badge() {
            parts.push(badge);
        }
        if let Some(bps) = self.bitrate_bps.filter(|&b| b >= 1_000) {
            parts.push(format!("{} kbps", (bps + 500) / 1_000));
        }
        parts.push(format!("RG {}", self.replaygain_mode.label()));
        parts.push(if self.is_local_cache {
            "libmpv local cache".to_string()
        } else {
            "libmpv direct stream".to_string()
        });
        parts.join(" · ")
    }
}

/// Lifecycle playback states emitted by the `libmpv` worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpvPlaybackState {
    Idle,
    Loading,
    Buffering,
    Playing,
    Paused,
}

/// Commands accepted by [`MpvEngine`] over its bounded command channel.
#[derive(Debug, Clone, PartialEq)]
pub enum MpvCommand {
    LoadTrack {
        source: MpvPlaybackSource,
        start_paused: bool,
        start_position_secs: Option<f64>,
    },
    PrefetchNext {
        source: MpvPlaybackSource,
    },
    ClearPrefetch,
    Play,
    Pause,
    Seek(f64),
    SetVolume(f32),
    SetMute(bool),
    SetReplayGain(ReplayGainMode),
    Stop,
    Shutdown,
}

/// Events emitted by [`MpvEngine`] to the desktop coordinator / UI bridge.
#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    Progress {
        position_secs: f64,
        duration_secs: f64,
    },
    Telemetry(AudioTelemetry),
    StateChanged(MpvPlaybackState),
    /// Emitted when a track naturally reaches EOF or gaplessly advances to the prefetched next item.
    TrackEnded {
        /// True if `libmpv` already seamlessly began playing the prefetched next track via
        /// `playlist-pos` transition; callers MUST advance UI state without re-calling `LoadTrack`.
        gapless_advanced: bool,
    },
    Error(String),
}

/// Initialization options for [`MpvEngine`].
#[derive(Debug, Clone)]
pub struct MpvInitOptions {
    /// Override `ao` (e.g. `Some("null".into())` for headless tests, `None` for OS default output).
    pub audio_output: Option<String>,
    pub replaygain: ReplayGainMode,
    pub initial_volume: f32,
}

impl Default for MpvInitOptions {
    fn default() -> Self {
        Self {
            audio_output: None,
            replaygain: ReplayGainMode::Track,
            initial_volume: 0.85,
        }
    }
}

/// Minimal backend trait allowing UI/state bridges to be driven by real [`MpvEngine`] or fakes.
#[allow(dead_code)]
pub trait AudioBackend: Send + Sync {
    fn send(&self, cmd: MpvCommand) -> Result<(), String>;
}

/// Dedicated-thread native `libmpv` audio player handle.
pub struct MpvEngine {
    cmd_tx: mpsc::Sender<MpvCommand>,
    shutdown_flag: Arc<AtomicBool>,
    event_rx: Option<mpsc::UnboundedReceiver<MpvEvent>>,
    worker: Option<JoinHandle<()>>,
}

impl AudioBackend for MpvEngine {
    fn send(&self, cmd: MpvCommand) -> Result<(), String> {
        self.send_command(cmd)
    }
}

impl MpvEngine {
    /// Spawns the `libmpv` worker thread using the OS default audio output.
    pub fn new(replaygain: ReplayGainMode, initial_volume: f32) -> Result<Self, String> {
        Self::with_options(MpvInitOptions {
            audio_output: None,
            replaygain,
            initial_volume,
        })
    }

    /// Spawns a headless `libmpv` worker (`ao=null`) suitable for CI and automated smoke tests.
    #[cfg(test)]
    pub fn new_headless(replaygain: ReplayGainMode) -> Result<Self, String> {
        Self::with_options(MpvInitOptions {
            audio_output: Some("null".to_string()),
            replaygain,
            initial_volume: 1.0,
        })
    }

    /// Spawns the `libmpv` worker thread with explicit initialization options.
    pub fn with_options(options: MpvInitOptions) -> Result<Self, String> {
        ensure_c_numeric_locale();
        let (cmd_tx, cmd_rx) = mpsc::channel::<MpvCommand>(64);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<MpvEvent>();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel::<Result<(), String>>(1);
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown_flag.clone();

        let worker = thread::Builder::new()
            .name("soulbeet-mpv".to_string())
            .spawn(move || {
                run_mpv_worker(options, cmd_rx, worker_shutdown, event_tx, ready_tx);
            })
            .map_err(|e| format!("failed to spawn libmpv worker thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                shutdown_flag,
                event_rx: Some(event_rx),
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                let _ = worker.join();
                Err(err)
            }
            Err(err) => {
                let _ = worker.join();
                Err(format!("libmpv worker exited before initialization: {err}"))
            }
        }
    }

    /// Takes ownership of the event stream receiver (typically once at app startup).
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<MpvEvent>> {
        self.event_rx.take()
    }

    /// Returns a cloneable bounded command sender for driving playback from UI callbacks.
    pub fn command_sender(&self) -> mpsc::Sender<MpvCommand> {
        self.cmd_tx.clone()
    }

    /// Sends a command non-blockingly to the `libmpv` worker.
    pub fn send_command(&self, cmd: MpvCommand) -> Result<(), String> {
        self.cmd_tx
            .try_send(cmd)
            .map_err(|e| format!("failed to enqueue mpv command: {e}"))
    }

    /// Explicitly requests shutdown (via lock-free `AtomicBool` + best-effort command)
    /// and joins the worker thread cleanly without deadlocking even if the command channel is full.
    pub fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.try_send(MpvCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MpvEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Sanitizes `(name, value)` HTTP headers into individual `"Key: Value"` list entries for
/// mpv's `change-list http-header-fields append ...` API. Strips CR/LF to prevent header
/// injection while preserving commas inside header values (e.g. `Cookie` or `Date`).
pub fn sanitize_http_headers(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            let key = k.replace(['\r', '\n', ':'], "").trim().to_string();
            let val = v.replace(['\r', '\n'], "").trim().to_string();
            if key.is_empty() {
                None
            } else {
                Some(format!("{key}: {val}"))
            }
        })
        .collect()
}

fn normalize_codec_label(lower: &str) -> &'static str {
    match lower {
        "flac" => "FLAC",
        "opus" => "OPUS",
        "mp3" => "MP3",
        "aac" => "AAC",
        "vorbis" => "OGG",
        "alac" => "ALAC",
        "wavpack" => "WV",
        "ape" => "APE",
        "wav" => "WAV",
        s if s.starts_with("pcm") => "PCM",
        s if s.starts_with("dsd") => "DSD",
        _ => "AUDIO",
    }
}

fn parse_bit_depth_from_mpv_format(fmt: &str) -> Option<u8> {
    let lower = fmt.trim().to_ascii_lowercase();
    if lower.starts_with("s16") || lower.starts_with("u16") {
        Some(16)
    } else if lower.starts_with("s24") || lower.starts_with("s32") || lower.starts_with("float") {
        Some(24)
    } else if lower.starts_with("u8") || lower.starts_with("s8") {
        Some(8)
    } else {
        None
    }
}

fn format_khz(hz: u32) -> String {
    if hz == 0 {
        return "0".to_string();
    }
    if hz.is_multiple_of(1_000) {
        format!("{}", hz / 1_000)
    } else {
        let khz = f64::from(hz) / 1_000.0;
        format!("{khz:.1}")
    }
}

const PROP_TIME_POS: u64 = 1;
const PROP_DURATION: u64 = 2;
const PROP_PAUSE: u64 = 3;
const PROP_CORE_IDLE: u64 = 4;
const PROP_PAUSED_FOR_CACHE: u64 = 5;
const PROP_PLAYLIST_POS: u64 = 6;
const PROP_AUDIO_CODEC: u64 = 7;
const PROP_SAMPLE_RATE: u64 = 8;
const PROP_SAMPLE_FMT: u64 = 9;
const PROP_BITRATE: u64 = 10;

struct WorkerTrackState {
    position_secs: f64,
    duration_secs: f64,
    paused: bool,
    core_idle: bool,
    buffering: bool,
    loading: bool,
    has_active_track: bool,
    /// Number of `loadfile replace` commands whose corresponding `StartFile` has not arrived yet.
    /// Suppresses stale `EndFile(Eof)` events from a track being replaced!
    pending_replace_loads: u32,
    pending_start_seek_secs: Option<f64>,
    prefetched_next: bool,
    current_is_local: bool,
    prefetched_is_local: bool,
    last_playlist_pos: i64,
    last_state: MpvPlaybackState,
    last_progress_emit: Instant,
    telemetry: AudioTelemetry,
}

impl WorkerTrackState {
    fn new(replaygain: ReplayGainMode) -> Self {
        Self {
            position_secs: 0.0,
            duration_secs: 0.0,
            paused: false,
            core_idle: true,
            buffering: false,
            loading: false,
            has_active_track: false,
            pending_replace_loads: 0,
            pending_start_seek_secs: None,
            prefetched_next: false,
            current_is_local: false,
            prefetched_is_local: false,
            last_playlist_pos: -1,
            last_state: MpvPlaybackState::Idle,
            last_progress_emit: Instant::now() - Duration::from_secs(1),
            telemetry: AudioTelemetry {
                replaygain_mode: replaygain,
                ..Default::default()
            },
        }
    }

    fn compute_state(&self) -> MpvPlaybackState {
        if !self.has_active_track {
            MpvPlaybackState::Idle
        } else if self.loading {
            MpvPlaybackState::Loading
        } else if self.buffering {
            MpvPlaybackState::Buffering
        } else if self.paused {
            MpvPlaybackState::Paused
        } else if self.core_idle {
            MpvPlaybackState::Loading
        } else {
            MpvPlaybackState::Playing
        }
    }

    fn emit_state_if_changed(&mut self, tx: &mpsc::UnboundedSender<MpvEvent>) {
        let next = self.compute_state();
        if next != self.last_state {
            self.last_state = next;
            let _ = tx.send(MpvEvent::StateChanged(next));
        }
    }

    fn emit_progress(&mut self, tx: &mpsc::UnboundedSender<MpvEvent>, force: bool) {
        let now = Instant::now();
        if force || now.duration_since(self.last_progress_emit) >= Duration::from_millis(120) {
            self.last_progress_emit = now;
            let _ = tx.send(MpvEvent::Progress {
                position_secs: self.position_secs.max(0.0),
                duration_secs: self.duration_secs.max(0.0),
            });
        }
    }
}

fn init_mpv_handle(options: &MpvInitOptions) -> Result<Mpv, String> {
    ensure_c_numeric_locale();
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vid", "no")?;
        init.set_option("audio-display", "no")?;
        init.set_option("gapless-audio", "yes")?;
        init.set_option("prefetch-playlist", "yes")?;
        init.set_option("replaygain", options.replaygain.as_mpv_value())?;
        init.set_option("cache", "yes")?;
        init.set_option("demuxer-max-bytes", "64MiB")?;
        init.set_option("audio-client-name", "Soulbeet")?;
        init.set_option("keep-open", "no")?;
        if let Some(ref ao) = options.audio_output {
            init.set_option("ao", ao.as_str())?;
        }
        Ok(())
    })
    .map_err(|e| format!("libmpv initialization failed: {e:?}"))?;

    let vol = f64::from(options.initial_volume.clamp(0.0, 1.0) * 100.0);
    let _ = mpv.set_property("volume", vol);

    // Observe playback & audiophile telemetry properties
    let _ = mpv.observe_property("time-pos", Format::Double, PROP_TIME_POS);
    let _ = mpv.observe_property("duration", Format::Double, PROP_DURATION);
    let _ = mpv.observe_property("pause", Format::Flag, PROP_PAUSE);
    let _ = mpv.observe_property("core-idle", Format::Flag, PROP_CORE_IDLE);
    let _ = mpv.observe_property("paused-for-cache", Format::Flag, PROP_PAUSED_FOR_CACHE);
    let _ = mpv.observe_property("playlist-pos", Format::Int64, PROP_PLAYLIST_POS);
    let _ = mpv.observe_property("audio-codec-name", Format::String, PROP_AUDIO_CODEC);
    let _ = mpv.observe_property("audio-params/samplerate", Format::Int64, PROP_SAMPLE_RATE);
    let _ = mpv.observe_property("audio-params/format", Format::String, PROP_SAMPLE_FMT);
    let _ = mpv.observe_property("audio-bitrate", Format::Int64, PROP_BITRATE);

    Ok(mpv)
}

fn query_telemetry_snapshot(mpv: &Mpv, st: &mut WorkerTrackState) {
    st.telemetry.codec_name = mpv
        .get_property::<String>("audio-codec-name")
        .ok()
        .filter(|s| !s.trim().is_empty());
    st.telemetry.sample_rate_hz = mpv
        .get_property::<i64>("audio-params/samplerate")
        .ok()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0);
    st.telemetry.sample_format = mpv
        .get_property::<String>("audio-params/format")
        .ok()
        .filter(|s| !s.trim().is_empty());
    st.telemetry.bitrate_bps = mpv
        .get_property::<i64>("audio-bitrate")
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .filter(|&v| v > 0);
    st.telemetry.is_local_cache = st.current_is_local;
}

fn clear_prefetched_playlist_tail(mpv: &Mpv) {
    if let Ok(count) = mpv.get_property::<i64>("playlist-count") {
        for idx in (1..count).rev() {
            let idx_str = idx.to_string();
            let _ = mpv.command("playlist-remove", &[idx_str.as_str()]);
        }
    }
}

/// Applies HTTP headers using mpv's comma-safe `change-list` command so commas inside header
/// values (like `Cookie:` or HTTP dates) never split into separate malformed headers.
fn apply_source_headers(mpv: &Mpv, source: &MpvPlaybackSource) {
    let _ = mpv.command("change-list", &["http-header-fields", "clr", ""]);
    if let MpvPlaybackSource::StreamUrl { headers, .. } = source {
        for entry in sanitize_http_headers(headers) {
            let _ = mpv.command("change-list", &["http-header-fields", "append", &entry]);
        }
    }
}

fn run_mpv_worker(
    options: MpvInitOptions,
    mut cmd_rx: mpsc::Receiver<MpvCommand>,
    shutdown_flag: Arc<AtomicBool>,
    event_tx: mpsc::UnboundedSender<MpvEvent>,
    ready_tx: std_mpsc::SyncSender<Result<(), String>>,
) {
    let mpv = match init_mpv_handle(&options) {
        Ok(m) => {
            let _ = ready_tx.send(Ok(()));
            m
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let mut st = WorkerTrackState::new(options.replaygain);

    'worker: loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            break 'worker;
        }

        // 1. Drain queued commands without blocking (re-checking shutdown_flag each step).
        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                break 'worker;
            }
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if !handle_command(&mpv, cmd, &mut st, &event_tx) {
                        break 'worker;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break 'worker,
            }
        }

        // 2. Poll libmpv events with a short timeout (20ms) so commands & shutdown stay instant.
        match mpv.wait_event(0.02) {
            None => continue,
            Some(Ok(event)) => {
                if !handle_mpv_event(&mpv, event, &mut st, &event_tx) {
                    break 'worker;
                }
            }
            Some(Err(err)) => {
                tracing::warn!("libmpv event error: {err:?}");
                let _ = event_tx.send(MpvEvent::Error(format!("mpv error: {err:?}")));
            }
        }
    }

    let _ = mpv.command("stop", &[]);
}

fn handle_command(
    mpv: &Mpv,
    cmd: MpvCommand,
    st: &mut WorkerTrackState,
    event_tx: &mpsc::UnboundedSender<MpvEvent>,
) -> bool {
    match cmd {
        MpvCommand::LoadTrack {
            source,
            start_paused,
            start_position_secs,
        } => {
            apply_source_headers(mpv, &source);
            st.current_is_local = source.is_local();
            st.prefetched_next = false;
            st.prefetched_is_local = false;
            st.has_active_track = true;
            st.loading = true;
            st.buffering = false;
            st.paused = start_paused;
            st.pending_replace_loads = st.pending_replace_loads.saturating_add(1);
            st.pending_start_seek_secs = start_position_secs.filter(|s| s.is_finite() && *s > 0.0);
            st.position_secs = st.pending_start_seek_secs.unwrap_or(0.0);
            st.duration_secs = 0.0;
            st.last_playlist_pos = 0;
            st.telemetry.codec_name = None;
            st.telemetry.sample_rate_hz = None;
            st.telemetry.sample_format = None;
            st.telemetry.bitrate_bps = None;
            st.telemetry.is_local_cache = st.current_is_local;

            let _ = mpv.set_property("pause", start_paused);
            if let Err(err) = mpv.command("loadfile", &[source.uri(), "replace"]) {
                st.loading = false;
                st.has_active_track = false;
                st.pending_replace_loads = st.pending_replace_loads.saturating_sub(1);
                st.pending_start_seek_secs = None;
                let _ = event_tx.send(MpvEvent::Error(format!("failed to load audio: {err:?}")));
            }
            st.emit_state_if_changed(event_tx);
        }
        MpvCommand::PrefetchNext { source } => {
            if !st.has_active_track {
                return true;
            }
            apply_source_headers(mpv, &source);
            clear_prefetched_playlist_tail(mpv);
            if let Err(err) = mpv.command("loadfile", &[source.uri(), "append"]) {
                tracing::debug!("gapless prefetch append failed: {err:?}");
                st.prefetched_next = false;
            } else {
                st.prefetched_next = true;
                st.prefetched_is_local = source.is_local();
            }
        }
        MpvCommand::ClearPrefetch => {
            clear_prefetched_playlist_tail(mpv);
            st.prefetched_next = false;
            st.prefetched_is_local = false;
        }
        MpvCommand::Play => {
            st.paused = false;
            if let Err(err) = mpv.set_property("pause", false) {
                let _ = event_tx.send(MpvEvent::Error(format!("failed to resume: {err:?}")));
            }
            st.emit_state_if_changed(event_tx);
        }
        MpvCommand::Pause => {
            st.paused = true;
            if let Err(err) = mpv.set_property("pause", true) {
                let _ = event_tx.send(MpvEvent::Error(format!("failed to pause: {err:?}")));
            }
            st.emit_state_if_changed(event_tx);
        }
        MpvCommand::Seek(secs) => {
            if secs.is_finite() {
                let clamped = secs.max(0.0);
                if st.loading {
                    st.pending_start_seek_secs = Some(clamped);
                    st.position_secs = clamped;
                    st.emit_progress(event_tx, true);
                } else {
                    let target = format!("{clamped:.3}");
                    if let Err(err) = mpv.command("seek", &[target.as_str(), "absolute"]) {
                        tracing::debug!("mpv seek failed: {err:?}");
                    } else {
                        st.position_secs = clamped;
                        st.emit_progress(event_tx, true);
                    }
                }
            }
        }
        MpvCommand::SetVolume(vol) => {
            if vol.is_finite() {
                let mpv_vol = f64::from(vol.clamp(0.0, 1.0) * 100.0);
                let _ = mpv.set_property("volume", mpv_vol);
            }
        }
        MpvCommand::SetMute(muted) => {
            let _ = mpv.set_property("mute", muted);
        }
        MpvCommand::SetReplayGain(mode) => {
            st.telemetry.replaygain_mode = mode;
            if let Err(err) = mpv.set_property("replaygain", mode.as_mpv_value()) {
                tracing::warn!("failed to set replaygain={}: {err:?}", mode.as_mpv_value());
            } else if st.has_active_track {
                let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
            }
        }
        MpvCommand::Stop => {
            st.has_active_track = false;
            st.loading = false;
            st.buffering = false;
            st.prefetched_next = false;
            st.pending_replace_loads = 0;
            st.pending_start_seek_secs = None;
            st.position_secs = 0.0;
            st.duration_secs = 0.0;
            st.last_playlist_pos = -1;
            let _ = mpv.command("stop", &[]);
            st.emit_state_if_changed(event_tx);
            st.emit_progress(event_tx, true);
        }
        MpvCommand::Shutdown => return false,
    }
    true
}

fn handle_mpv_event(
    mpv: &Mpv,
    event: Event<'_>,
    st: &mut WorkerTrackState,
    event_tx: &mpsc::UnboundedSender<MpvEvent>,
) -> bool {
    match event {
        Event::Shutdown => return false,
        Event::StartFile => {
            if st.pending_replace_loads > 0 {
                st.pending_replace_loads -= 1;
            }
            if st.has_active_track {
                st.loading = true;
                st.emit_state_if_changed(event_tx);
            }
        }
        Event::FileLoaded => {
            st.loading = false;
            if let Ok(dur) = mpv.get_property::<f64>("duration") {
                if dur.is_finite() && dur >= 0.0 {
                    st.duration_secs = dur;
                }
            }
            if let Some(seek_to) = st.pending_start_seek_secs.take() {
                let clamped = if st.duration_secs > 0.0 {
                    seek_to.clamp(0.0, (st.duration_secs - 0.1).max(0.0))
                } else {
                    seek_to.max(0.0)
                };
                let target = format!("{clamped:.3}");
                if mpv.command("seek", &[target.as_str(), "absolute"]).is_ok() {
                    st.position_secs = clamped;
                }
            } else if let Ok(pos) = mpv.get_property::<f64>("time-pos") {
                if pos.is_finite() && pos >= 0.0 {
                    st.position_secs = pos;
                }
            }
            query_telemetry_snapshot(mpv, st);
            let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
            st.emit_progress(event_tx, true);
            st.emit_state_if_changed(event_tx);
        }
        Event::AudioReconfig | Event::PlaybackRestart => {
            st.loading = false;
            query_telemetry_snapshot(mpv, st);
            let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
            st.emit_state_if_changed(event_tx);
        }
        Event::EndFile(reason) => {
            // Suppress stale EndFile events from a previous track while a replacement load is queued.
            if st.pending_replace_loads > 0 {
                return true;
            }
            if reason == mpv_end_file_reason::Eof {
                let playlist_count = mpv.get_property::<i64>("playlist-count").unwrap_or(0);
                if !(st.prefetched_next && playlist_count > 1) {
                    st.loading = false;
                    st.has_active_track = false;
                    st.emit_state_if_changed(event_tx);
                    let _ = event_tx.send(MpvEvent::TrackEnded {
                        gapless_advanced: false,
                    });
                }
            } else if reason == mpv_end_file_reason::Error {
                st.loading = false;
                st.has_active_track = false;
                st.emit_state_if_changed(event_tx);
                let _ = event_tx.send(MpvEvent::Error(
                    "Playback failed while decoding or streaming audio".to_string(),
                ));
            }
        }
        Event::PropertyChange {
            reply_userdata,
            change,
            ..
        } => match (reply_userdata, change) {
            (PROP_TIME_POS, PropertyData::Double(pos)) => {
                if pos.is_finite() && pos >= 0.0 {
                    st.position_secs = pos;
                    st.emit_progress(event_tx, false);
                }
            }
            (PROP_DURATION, PropertyData::Double(dur)) => {
                if dur.is_finite() && dur >= 0.0 {
                    st.duration_secs = dur;
                    st.emit_progress(event_tx, true);
                }
            }
            (PROP_PAUSE, PropertyData::Flag(paused)) => {
                st.paused = paused;
                st.emit_state_if_changed(event_tx);
            }
            (PROP_CORE_IDLE, PropertyData::Flag(idle)) => {
                st.core_idle = idle;
                st.emit_state_if_changed(event_tx);
            }
            (PROP_PAUSED_FOR_CACHE, PropertyData::Flag(buffering)) => {
                st.buffering = buffering;
                st.emit_state_if_changed(event_tx);
            }
            (PROP_PLAYLIST_POS, PropertyData::Int64(pos)) => {
                if st.pending_replace_loads == 0
                    && st.prefetched_next
                    && st.last_playlist_pos == 0
                    && pos >= 1
                {
                    st.prefetched_next = false;
                    st.current_is_local = st.prefetched_is_local;
                    st.prefetched_is_local = false;
                    st.position_secs = 0.0;
                    let _ = mpv.command("playlist-remove", &["0"]);
                    st.last_playlist_pos = 0;
                    query_telemetry_snapshot(mpv, st);
                    let _ = event_tx.send(MpvEvent::TrackEnded {
                        gapless_advanced: true,
                    });
                    let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
                } else {
                    st.last_playlist_pos = pos;
                }
            }
            (PROP_AUDIO_CODEC, PropertyData::Str(codec)) => {
                if !codec.trim().is_empty() {
                    st.telemetry.codec_name = Some(codec.to_string());
                    let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
                }
            }
            (PROP_SAMPLE_RATE, PropertyData::Int64(rate)) => {
                if let Ok(hz) = u32::try_from(rate) {
                    if hz > 0 {
                        st.telemetry.sample_rate_hz = Some(hz);
                        let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
                    }
                }
            }
            (PROP_SAMPLE_FMT, PropertyData::Str(fmt)) => {
                if !fmt.trim().is_empty() {
                    st.telemetry.sample_format = Some(fmt.to_string());
                    let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
                }
            }
            (PROP_BITRATE, PropertyData::Int64(bps)) => {
                if let Ok(b) = u64::try_from(bps) {
                    if b > 0 {
                        st.telemetry.bitrate_bps = Some(b);
                        let _ = event_tx.send(MpvEvent::Telemetry(st.telemetry.clone()));
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Generates a valid uncompressed PCM 16-bit 44.1kHz stereo WAV file of silence.
    fn write_silent_wav(path: &std::path::Path, duration_secs: u32) {
        let sample_rate: u32 = 44_100;
        let channels: u16 = 2;
        let bits_per_sample: u16 = 16;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_size = byte_rate * duration_secs;
        let chunk_size = 36 + data_size;

        let mut file = std::fs::File::create(path).expect("create wav");
        file.write_all(b"RIFF").unwrap();
        file.write_all(&chunk_size.to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&channels.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&byte_rate.to_le_bytes()).unwrap();
        file.write_all(&block_align.to_le_bytes()).unwrap();
        file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();
        file.write_all(&vec![0u8; data_size as usize]).unwrap();
        file.flush().unwrap();
    }

    #[test]
    fn formats_lossless_and_lossy_codec_badges_cleanly() {
        let flac_hires = AudioTelemetry {
            codec_name: Some("flac".into()),
            sample_rate_hz: Some(96_000),
            sample_format: Some("s32p".into()),
            bitrate_bps: Some(2_840_000),
            replaygain_mode: ReplayGainMode::Track,
            is_local_cache: true,
        };
        assert_eq!(
            flac_hires.format_codec_badge().as_deref(),
            Some("FLAC · 24/96")
        );
        assert!(flac_hires.format_signal_chain().contains("2840 kbps"));
        assert!(flac_hires.format_signal_chain().contains("libmpv local cache"));

        let flac_cd = AudioTelemetry {
            codec_name: Some("flac".into()),
            sample_rate_hz: Some(44_100),
            sample_format: Some("s16p".into()),
            bitrate_bps: Some(910_000),
            replaygain_mode: ReplayGainMode::Album,
            is_local_cache: false,
        };
        assert_eq!(
            flac_cd.format_codec_badge().as_deref(),
            Some("FLAC · 16/44.1")
        );

        let opus = AudioTelemetry {
            codec_name: Some("opus".into()),
            sample_rate_hz: Some(48_000),
            sample_format: Some("floatp".into()),
            bitrate_bps: Some(160_000),
            replaygain_mode: ReplayGainMode::Track,
            is_local_cache: false,
        };
        assert_eq!(opus.format_codec_badge().as_deref(), Some("OPUS 160"));
    }

    #[test]
    fn sanitizes_http_headers_preserving_commas_and_stripping_crlf() {
        let formatted = sanitize_http_headers(&[
            ("Authorization".into(), "Bearer tok\r\nEvil: 1".into()),
            ("".into(), "ignored".into()),
            ("Cookie".into(), "a=1, b=2".into()),
            ("X-Client".into(), "Soulbeet".into()),
        ]);
        assert_eq!(
            formatted,
            vec![
                "Authorization: Bearer tokEvil: 1",
                "Cookie: a=1, b=2",
                "X-Client: Soulbeet",
            ]
        );
    }

    #[test]
    fn replaygain_modes_map_to_expected_mpv_values() {
        assert_eq!(ReplayGainMode::Track.as_mpv_value(), "track");
        assert_eq!(ReplayGainMode::Album.as_mpv_value(), "album");
        assert_eq!(ReplayGainMode::Off.as_mpv_value(), "no");
        assert_eq!(ReplayGainMode::Track.cycle(), ReplayGainMode::Album);
        assert_eq!(ReplayGainMode::Album.cycle(), ReplayGainMode::Off);
        assert_eq!(ReplayGainMode::Off.cycle(), ReplayGainMode::Track);
    }

    #[tokio::test]
    async fn headless_libmpv_loads_wav_with_start_position_reports_telemetry_and_shuts_down() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wav_path = tmp.path().join("silence.wav");
        write_silent_wav(&wav_path, 3);

        let mut engine =
            MpvEngine::new_headless(ReplayGainMode::Track).expect("init real libmpv 2.x");
        let mut events = engine.take_events().expect("event receiver");

        engine
            .send_command(MpvCommand::LoadTrack {
                source: MpvPlaybackSource::LocalFile {
                    path: wav_path.display().to_string(),
                },
                start_paused: false,
                start_position_secs: Some(1.5),
            })
            .expect("send load");

        let mut saw_duration = false;
        let mut saw_start_seek = false;
        let mut saw_telemetry = false;
        let mut saw_playing = false;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline
            && !(saw_duration && saw_start_seek && saw_telemetry && saw_playing)
        {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(500), events.recv()).await
            {
                match ev {
                    MpvEvent::Progress {
                        position_secs,
                        duration_secs,
                    } => {
                        if duration_secs >= 2.5 {
                            saw_duration = true;
                        }
                        if position_secs >= 1.2 {
                            saw_start_seek = true;
                        }
                    }
                    MpvEvent::Telemetry(t) => {
                        if t.sample_rate_hz == Some(44_100) && t.codec_name.is_some() {
                            saw_telemetry = true;
                        }
                    }
                    MpvEvent::StateChanged(MpvPlaybackState::Playing) => {
                        saw_playing = true;
                    }
                    MpvEvent::Error(e) => panic!("unexpected libmpv error: {e}"),
                    _ => {}
                }
            }
        }

        assert!(saw_duration, "expected libmpv to report ~3s WAV duration");
        assert!(
            saw_start_seek,
            "expected libmpv to honor start_position_secs >= 1.2s"
        );
        assert!(
            saw_telemetry,
            "expected libmpv to report 44.1kHz PCM telemetry"
        );
        assert!(saw_playing, "expected libmpv to enter Playing state");

        engine.send_command(MpvCommand::Pause).unwrap();
        let mut saw_paused = false;
        let pause_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < pause_deadline && !saw_paused {
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(250), events.recv()).await
            {
                if matches!(ev, MpvEvent::StateChanged(MpvPlaybackState::Paused)) {
                    saw_paused = true;
                }
            }
        }
        assert!(saw_paused, "expected libmpv to transition to Paused");

        // Saturate command queue to verify atomic shutdown_flag NEVER deadlocks!
        for _ in 0..80 {
            let _ = engine.send_command(MpvCommand::SetVolume(0.5));
        }
        engine.shutdown();
    }

    #[tokio::test]
    async fn headless_libmpv_gapless_prefetches_and_advances_to_second_wav() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wav1 = tmp.path().join("track1.wav");
        let wav2 = tmp.path().join("track2.wav");
        write_silent_wav(&wav1, 1);
        write_silent_wav(&wav2, 1);

        let mut engine =
            MpvEngine::new_headless(ReplayGainMode::Track).expect("init real libmpv 2.x");
        let mut events = engine.take_events().expect("event receiver");

        engine
            .send_command(MpvCommand::LoadTrack {
                source: MpvPlaybackSource::LocalFile {
                    path: wav1.display().to_string(),
                },
                start_paused: false,
                start_position_secs: None,
            })
            .unwrap();
        engine
            .send_command(MpvCommand::PrefetchNext {
                source: MpvPlaybackSource::LocalFile {
                    path: wav2.display().to_string(),
                },
            })
            .unwrap();

        let mut saw_gapless_advance = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !saw_gapless_advance {
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(500), events.recv()).await
            {
                if matches!(
                    ev,
                    MpvEvent::TrackEnded {
                        gapless_advanced: true
                    }
                ) {
                    saw_gapless_advance = true;
                }
            }
        }
        assert!(
            saw_gapless_advance,
            "expected libmpv to gaplessly transition from track1.wav to prefetched track2.wav"
        );
        engine.shutdown();
    }
}
