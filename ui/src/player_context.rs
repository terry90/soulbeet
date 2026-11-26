use dioxus::prelude::*;

/// Current playback lifecycle status for the active audio backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Buffering,
    Error(String),
}

impl PlaybackStatus {
    pub fn is_playing(&self) -> bool {
        matches!(self, Self::Playing)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Loading | Self::Buffering)
    }
}

/// Queue repeat behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub const fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    pub const fn aria_label(self) -> &'static str {
        match self {
            Self::Off => "Repeat off",
            Self::All => "Repeat all",
            Self::One => "Repeat one",
        }
    }
}

/// Unified UI representation for an owned, staged discovery, or acquiring track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub cover_art_url: Option<String>,
    pub duration_secs: Option<u32>,
    pub is_discovery: bool,
    pub days_remaining: Option<u32>,
    pub starred: bool,
    pub rating: u8,
    pub codec_badge: Option<String>,
    pub is_offline_cached: bool,
    pub reason_label: Option<String>,
}

/// Platform-agnostic playback commands dispatched from shared UI components
/// and fulfilled by either HTML5 Audio (`web`) or `libmpv` + `client-core` (`desktop`).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    PlayTrack(PlayerTrack),
    PlayQueue {
        tracks: Vec<PlayerTrack>,
        start_index: usize,
    },
    TogglePlayPause,
    Next,
    Previous,
    Seek(f64),
    SetVolume(f32),
    ToggleMute,
    ToggleShuffle,
    CycleRepeat,
    EnqueueNext(PlayerTrack),
    EnqueueLast(PlayerTrack),
    RemoveFromQueue(usize),
    ClearQueue,
    SetOfflineOnly(bool),
}

/// Reactive playback state shared across navigation shells, player bars, queues,
/// and track rows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    pub current_track: Option<PlayerTrack>,
    pub queue: Vec<PlayerTrack>,
    pub queue_index: usize,
    pub status: PlaybackStatus,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub offline_only: bool,
    pub active_device_name: Option<String>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            current_track: None,
            queue: Vec::new(),
            queue_index: 0,
            status: PlaybackStatus::Idle,
            position_secs: 0.0,
            duration_secs: 0.0,
            volume: 1.0,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            offline_only: false,
            active_device_name: None,
        }
    }
}

impl PlayerState {
    /// Applies deterministic queue, transport, and preference transitions locally.
    /// Concrete backends (`web` HTML5 `<audio>`, `desktop` `libmpv`) can call this to keep
    /// UI state synchronized immediately while issuing side effects to the audio engine.
    pub fn apply_local_command(&mut self, command: &PlayerCommand) {
        match command {
            PlayerCommand::PlayTrack(track) => {
                let dur = track.duration_secs.map(f64::from).unwrap_or(0.0);
                self.queue = vec![track.clone()];
                self.queue_index = 0;
                self.current_track = Some(track.clone());
                self.position_secs = 0.0;
                self.duration_secs = dur;
                self.status = PlaybackStatus::Loading;
            }
            PlayerCommand::PlayQueue {
                tracks,
                start_index,
            } => {
                if tracks.is_empty() {
                    self.clear_playback();
                    return;
                }
                let idx = (*start_index).min(tracks.len() - 1);
                let current = tracks[idx].clone();
                let dur = current.duration_secs.map(f64::from).unwrap_or(0.0);
                self.queue = tracks.clone();
                self.queue_index = idx;
                self.current_track = Some(current);
                self.position_secs = 0.0;
                self.duration_secs = dur;
                self.status = PlaybackStatus::Loading;
            }
            PlayerCommand::TogglePlayPause => match self.status {
                PlaybackStatus::Playing => self.status = PlaybackStatus::Paused,
                PlaybackStatus::Paused => self.status = PlaybackStatus::Playing,
                PlaybackStatus::Idle if self.current_track.is_some() => {
                    self.status = PlaybackStatus::Loading;
                }
                _ => {}
            },
            PlayerCommand::Next => {
                if self.queue.is_empty() {
                    return;
                }
                if self.repeat == RepeatMode::One {
                    self.position_secs = 0.0;
                    self.status = PlaybackStatus::Loading;
                    return;
                }
                if self.queue_index + 1 < self.queue.len() {
                    self.select_queue_index(self.queue_index + 1);
                } else if self.repeat == RepeatMode::All {
                    self.select_queue_index(0);
                } else {
                    self.position_secs = self.duration_secs;
                    self.status = PlaybackStatus::Paused;
                }
            }
            PlayerCommand::Previous => {
                if self.queue.is_empty() {
                    return;
                }
                if self.position_secs > 3.0 {
                    self.position_secs = 0.0;
                    return;
                }
                if self.queue_index > 0 {
                    self.select_queue_index(self.queue_index - 1);
                } else if self.repeat == RepeatMode::All {
                    self.select_queue_index(self.queue.len() - 1);
                } else {
                    self.position_secs = 0.0;
                }
            }
            PlayerCommand::Seek(secs) => {
                let target = if secs.is_finite() { *secs } else { 0.0 };
                self.position_secs = if self.duration_secs > 0.0 {
                    target.clamp(0.0, self.duration_secs)
                } else {
                    target.max(0.0)
                };
            }
            PlayerCommand::SetVolume(volume) => {
                let clamped = if volume.is_finite() {
                    volume.clamp(0.0, 1.0)
                } else {
                    1.0
                };
                self.volume = clamped;
                if clamped > 0.0 {
                    self.muted = false;
                }
            }
            PlayerCommand::ToggleMute => {
                self.muted = !self.muted;
            }
            PlayerCommand::ToggleShuffle => {
                self.shuffle = !self.shuffle;
            }
            PlayerCommand::CycleRepeat => {
                self.repeat = self.repeat.cycle();
            }
            PlayerCommand::EnqueueNext(track) => {
                if self.queue.is_empty() {
                    self.queue.push(track.clone());
                } else {
                    let insert_at = (self.queue_index + 1).min(self.queue.len());
                    self.queue.insert(insert_at, track.clone());
                }
            }
            PlayerCommand::EnqueueLast(track) => {
                self.queue.push(track.clone());
            }
            PlayerCommand::RemoveFromQueue(idx) => {
                if *idx >= self.queue.len() {
                    return;
                }
                self.queue.remove(*idx);
                if self.queue.is_empty() {
                    self.clear_playback();
                } else if *idx < self.queue_index {
                    self.queue_index -= 1;
                } else if *idx == self.queue_index {
                    let next_idx = (*idx).min(self.queue.len() - 1);
                    self.select_queue_index(next_idx);
                }
            }
            PlayerCommand::ClearQueue => {
                self.clear_playback();
            }
            PlayerCommand::SetOfflineOnly(enabled) => {
                self.offline_only = *enabled;
            }
        }
    }

    fn select_queue_index(&mut self, index: usize) {
        if let Some(track) = self.queue.get(index).cloned() {
            let dur = track.duration_secs.map(f64::from).unwrap_or(0.0);
            self.queue_index = index;
            self.current_track = Some(track);
            self.position_secs = 0.0;
            self.duration_secs = dur;
            self.status = PlaybackStatus::Loading;
        }
    }

    fn clear_playback(&mut self) {
        self.queue.clear();
        self.queue_index = 0;
        self.current_track = None;
        self.position_secs = 0.0;
        self.duration_secs = 0.0;
        self.status = PlaybackStatus::Idle;
    }
}

/// Shared Dioxus context exposing reactive playback state and command dispatch.
#[derive(Clone, Copy)]
pub struct PlayerContext {
    state: Signal<PlayerState>,
    dispatcher: Callback<PlayerCommand>,
}

impl PlayerContext {
    pub fn new(state: Signal<PlayerState>, dispatcher: Callback<PlayerCommand>) -> Self {
        Self { state, dispatcher }
    }

    /// Underlying state signal for platform backends (`web`, `desktop`) to push
    /// timeupdate, buffering, track-end, or Connect device updates.
    pub fn state_signal(&self) -> Signal<PlayerState> {
        self.state
    }

    /// Returns a cloned snapshot of the current playback state, subscribing the caller.
    pub fn snapshot(&self) -> PlayerState {
        self.state.read().clone()
    }

    pub fn current_track(&self) -> Option<PlayerTrack> {
        self.state.read().current_track.clone()
    }

    pub fn status(&self) -> PlaybackStatus {
        self.state.read().status.clone()
    }

    pub fn is_playing(&self) -> bool {
        self.state.read().status.is_playing()
    }

    pub fn is_track_playing(&self, track_id: &str) -> bool {
        let s = self.state.read();
        s.current_track
            .as_ref()
            .is_some_and(|t| t.id == track_id && s.status.is_playing())
    }

    pub fn is_track_current(&self, track_id: &str) -> bool {
        self.state
            .read()
            .current_track
            .as_ref()
            .is_some_and(|t| t.id == track_id)
    }

    pub fn queue(&self) -> Vec<PlayerTrack> {
        self.state.read().queue.clone()
    }

    pub fn queue_index(&self) -> usize {
        self.state.read().queue_index
    }

    pub fn position_secs(&self) -> f64 {
        self.state.read().position_secs
    }

    pub fn duration_secs(&self) -> f64 {
        self.state.read().duration_secs
    }

    pub fn volume(&self) -> f32 {
        self.state.read().volume
    }

    pub fn muted(&self) -> bool {
        self.state.read().muted
    }

    pub fn shuffle(&self) -> bool {
        self.state.read().shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.state.read().repeat
    }

    pub fn offline_only(&self) -> bool {
        self.state.read().offline_only
    }

    pub fn active_device_name(&self) -> Option<String> {
        self.state.read().active_device_name.clone()
    }

    pub fn dispatch(&self, command: PlayerCommand) {
        self.dispatcher.call(command);
    }

    pub fn play_track(&self, track: PlayerTrack) {
        self.dispatch(PlayerCommand::PlayTrack(track));
    }

    pub fn play_queue(&self, tracks: Vec<PlayerTrack>, start_index: usize) {
        self.dispatch(PlayerCommand::PlayQueue {
            tracks,
            start_index,
        });
    }

    pub fn toggle_play_pause(&self) {
        self.dispatch(PlayerCommand::TogglePlayPause);
    }

    pub fn next(&self) {
        self.dispatch(PlayerCommand::Next);
    }

    pub fn previous(&self) {
        self.dispatch(PlayerCommand::Previous);
    }

    pub fn seek(&self, position_secs: f64) {
        self.dispatch(PlayerCommand::Seek(position_secs));
    }

    pub fn set_volume(&self, volume: f32) {
        self.dispatch(PlayerCommand::SetVolume(volume));
    }

    pub fn toggle_mute(&self) {
        self.dispatch(PlayerCommand::ToggleMute);
    }

    pub fn toggle_shuffle(&self) {
        self.dispatch(PlayerCommand::ToggleShuffle);
    }

    pub fn cycle_repeat(&self) {
        self.dispatch(PlayerCommand::CycleRepeat);
    }

    pub fn enqueue_next(&self, track: PlayerTrack) {
        self.dispatch(PlayerCommand::EnqueueNext(track));
    }

    pub fn enqueue_last(&self, track: PlayerTrack) {
        self.dispatch(PlayerCommand::EnqueueLast(track));
    }

    pub fn remove_from_queue(&self, index: usize) {
        self.dispatch(PlayerCommand::RemoveFromQueue(index));
    }

    pub fn clear_queue(&self) {
        self.dispatch(PlayerCommand::ClearQueue);
    }

    pub fn set_offline_only(&self, enabled: bool) {
        self.dispatch(PlayerCommand::SetOfflineOnly(enabled));
    }

    pub fn toggle_offline_only(&self) {
        let next = !self.offline_only();
        self.set_offline_only(next);
    }
}

/// Access the active `PlayerContext`. Panics if no provider is mounted.
pub fn use_player() -> PlayerContext {
    use_context::<PlayerContext>()
}

/// Try to access the active `PlayerContext` without panicking if absent.
pub fn try_use_player() -> Option<PlayerContext> {
    try_use_context::<PlayerContext>()
}

/// Provider component wired by a platform backend (`web` HTML5 Audio or `desktop` `libmpv`).
#[component]
pub fn PlayerProvider(
    state: Signal<PlayerState>,
    on_command: Callback<PlayerCommand>,
    children: Element,
) -> Element {
    use_context_provider(|| PlayerContext::new(state, on_command));
    rsx! { {children} }
}

/// Self-contained local provider for previews, tests, and offline/noop shells.
/// Applies all commands deterministically to local reactive state.
#[component]
pub fn LocalPlayerProvider(
    #[props(default)] initial_state: Option<PlayerState>,
    children: Element,
) -> Element {
    let mut state = use_signal(move || initial_state.clone().unwrap_or_default());
    let dispatcher = use_callback(move |cmd: PlayerCommand| {
        state.write().apply_local_command(&cmd);
    });
    use_context_provider(|| PlayerContext::new(state, dispatcher));
    rsx! { {children} }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, duration_secs: u32) -> PlayerTrack {
        PlayerTrack {
            id: id.to_string(),
            title: format!("Track {id}"),
            artist: "Artist".to_string(),
            duration_secs: Some(duration_secs),
            ..Default::default()
        }
    }

    #[test]
    fn play_queue_clamps_start_index_and_sets_metadata() {
        let mut state = PlayerState::default();
        state.apply_local_command(&PlayerCommand::PlayQueue {
            tracks: vec![track("a", 180), track("b", 240)],
            start_index: 99,
        });
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.current_track.as_ref().unwrap().id, "b");
        assert_eq!(state.duration_secs, 240.0);
        assert_eq!(state.status, PlaybackStatus::Loading);
    }

    #[test]
    fn next_and_previous_respect_repeat_modes_and_3s_restart_rule() {
        let mut state = PlayerState::default();
        state.apply_local_command(&PlayerCommand::PlayQueue {
            tracks: vec![track("a", 100), track("b", 200)],
            start_index: 0,
        });

        // Advance to second track
        state.apply_local_command(&PlayerCommand::Next);
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.current_track.as_ref().unwrap().id, "b");

        // Past end with Repeat::Off pauses at end of duration
        state.apply_local_command(&PlayerCommand::Next);
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.status, PlaybackStatus::Paused);
        assert_eq!(state.position_secs, 200.0);

        // Previous when position > 3s restarts current track instead of skipping back
        state.apply_local_command(&PlayerCommand::Previous);
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.position_secs, 0.0);

        // Previous at 0s moves back to first track
        state.apply_local_command(&PlayerCommand::Previous);
        assert_eq!(state.queue_index, 0);
        assert_eq!(state.current_track.as_ref().unwrap().id, "a");

        // Repeat::All wraps around both directions
        state.apply_local_command(&PlayerCommand::CycleRepeat);
        assert_eq!(state.repeat, RepeatMode::All);
        state.apply_local_command(&PlayerCommand::Previous);
        assert_eq!(state.queue_index, 1);
        state.apply_local_command(&PlayerCommand::Next);
        assert_eq!(state.queue_index, 0);

        // Repeat::One restarts same track on Next
        state.apply_local_command(&PlayerCommand::CycleRepeat);
        assert_eq!(state.repeat, RepeatMode::One);
        state.position_secs = 42.0;
        state.apply_local_command(&PlayerCommand::Next);
        assert_eq!(state.queue_index, 0);
        assert_eq!(state.position_secs, 0.0);
    }

    #[test]
    fn enqueue_and_remove_keep_queue_index_consistent() {
        let mut state = PlayerState::default();
        state.apply_local_command(&PlayerCommand::PlayQueue {
            tracks: vec![track("a", 100), track("b", 120)],
            start_index: 0,
        });
        state.apply_local_command(&PlayerCommand::EnqueueNext(track("next", 90)));
        state.apply_local_command(&PlayerCommand::EnqueueLast(track("last", 150)));
        let ids: Vec<&str> = state.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "next", "b", "last"]);

        // Move to index 2 ("b") and remove index 0 ("a") -> index shifts to 1 ("b")
        state.queue_index = 2;
        state.current_track = Some(state.queue[2].clone());
        state.apply_local_command(&PlayerCommand::RemoveFromQueue(0));
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.current_track.as_ref().unwrap().id, "b");

        // Removing the currently playing item selects the next available item
        state.apply_local_command(&PlayerCommand::RemoveFromQueue(1));
        assert_eq!(state.queue_index, 1);
        assert_eq!(state.current_track.as_ref().unwrap().id, "last");

        // Clearing queue resets to Idle
        state.apply_local_command(&PlayerCommand::ClearQueue);
        assert!(state.queue.is_empty());
        assert!(state.current_track.is_none());
        assert_eq!(state.status, PlaybackStatus::Idle);
    }

    #[test]
    fn seek_and_volume_clamp_safely() {
        let mut state = PlayerState::default();
        state.apply_local_command(&PlayerCommand::PlayTrack(track("a", 120)));
        state.apply_local_command(&PlayerCommand::Seek(999.0));
        assert_eq!(state.position_secs, 120.0);
        state.apply_local_command(&PlayerCommand::Seek(-10.0));
        assert_eq!(state.position_secs, 0.0);
        state.apply_local_command(&PlayerCommand::Seek(f64::NAN));
        assert_eq!(state.position_secs, 0.0);

        state.apply_local_command(&PlayerCommand::ToggleMute);
        assert!(state.muted);
        state.apply_local_command(&PlayerCommand::SetVolume(0.75));
        assert_eq!(state.volume, 0.75);
        assert!(!state.muted);
        state.apply_local_command(&PlayerCommand::SetVolume(2.5));
        assert_eq!(state.volume, 1.0);
    }
}
