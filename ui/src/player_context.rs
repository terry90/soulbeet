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

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Playing | Self::Loading | Self::Buffering)
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
    pub cover_art_id: Option<String>,
    pub duration_secs: Option<u32>,
    pub is_discovery: bool,
    pub discovery_track_id: Option<String>,
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
    /// Loads a queue at `start_index`, either auto-playing (`PlaybackStatus::Loading`)
    /// or staying paused (`PlaybackStatus::Paused`) when `autoplay == false`.
    LoadQueue {
        tracks: Vec<PlayerTrack>,
        start_index: usize,
        autoplay: bool,
    },
    TogglePlayPause,
    /// Explicit user skip to the next track (advances even under `RepeatMode::One`).
    Next,
    /// Natural end-of-track event from the audio engine (obeys `RepeatMode::One`).
    TrackEnded,
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
///
/// High-frequency progress ticks (`position_secs` / `duration_secs`) live on dedicated
/// signals inside [`PlayerContext`] so 4–10Hz audio time updates do not re-render
/// unrelated components subscribing to `PlayerState`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    pub current_track: Option<PlayerTrack>,
    pub queue: Vec<PlayerTrack>,
    pub queue_index: Option<usize>,
    pub status: PlaybackStatus,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub offline_only: bool,
    pub active_device_name: Option<String>,
    /// Deterministic shuffled index order (`0..queue.len()`) when `shuffle` is enabled.
    shuffle_order: Vec<usize>,
    /// Current cursor inside `shuffle_order`.
    shuffle_cursor: Option<usize>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            current_track: None,
            queue: Vec::new(),
            queue_index: None,
            status: PlaybackStatus::Idle,
            volume: 1.0,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            offline_only: false,
            active_device_name: None,
            shuffle_order: Vec::new(),
            shuffle_cursor: None,
        }
    }
}

impl PlayerState {
    /// Applies deterministic queue, transport, and preference transitions locally.
    /// Concrete backends (`web` HTML5 `<audio>`, `desktop` `libmpv`) can call this alongside
    /// their clock signals to keep UI state synchronized immediately while issuing side effects
    /// to the audio engine.
    pub fn apply_local_command(
        &mut self,
        command: &PlayerCommand,
        position_secs: &mut f64,
        duration_secs: &mut f64,
    ) {
        match command {
            PlayerCommand::PlayTrack(track) => {
                let dur = track.duration_secs.map(f64::from).unwrap_or(0.0);
                self.queue = vec![track.clone()];
                self.queue_index = Some(0);
                self.current_track = Some(track.clone());
                *position_secs = 0.0;
                *duration_secs = dur;
                self.status = PlaybackStatus::Loading;
                self.sync_shuffle_order();
            }
            PlayerCommand::PlayQueue {
                tracks,
                start_index,
            } => {
                if tracks.is_empty() {
                    self.clear_playback(position_secs, duration_secs);
                    return;
                }
                let idx = (*start_index).min(tracks.len() - 1);
                self.queue = tracks.clone();
                self.select_queue_index(idx, PlaybackStatus::Loading, position_secs, duration_secs);
                self.sync_shuffle_order();
            }
            PlayerCommand::LoadQueue {
                tracks,
                start_index,
                autoplay,
            } => {
                if tracks.is_empty() {
                    self.clear_playback(position_secs, duration_secs);
                    return;
                }
                let idx = (*start_index).min(tracks.len() - 1);
                let next_status = if *autoplay {
                    PlaybackStatus::Loading
                } else {
                    PlaybackStatus::Paused
                };
                self.queue = tracks.clone();
                self.select_queue_index(idx, next_status, position_secs, duration_secs);
                self.sync_shuffle_order();
            }
            PlayerCommand::TogglePlayPause => match self.status {
                PlaybackStatus::Playing => self.status = PlaybackStatus::Paused,
                PlaybackStatus::Paused => self.status = PlaybackStatus::Playing,
                PlaybackStatus::Error(_) => {
                    if self.current_track.is_some() {
                        self.status = PlaybackStatus::Loading;
                    }
                }
                PlaybackStatus::Idle => {
                    if self.current_track.is_some() {
                        self.status = PlaybackStatus::Loading;
                    } else if !self.queue.is_empty() {
                        self.select_queue_index(
                            0,
                            PlaybackStatus::Loading,
                            position_secs,
                            duration_secs,
                        );
                        self.sync_shuffle_order();
                    }
                }
                _ => {}
            },
            PlayerCommand::Next => {
                self.advance_next(false, position_secs, duration_secs);
            }
            PlayerCommand::TrackEnded => {
                self.advance_next(true, position_secs, duration_secs);
            }
            PlayerCommand::Previous => {
                if self.queue.is_empty() {
                    return;
                }
                if *position_secs > 3.0 {
                    *position_secs = 0.0;
                    return;
                }
                if self.shuffle && !self.shuffle_order.is_empty() {
                    let cur_pos = self.shuffle_cursor.unwrap_or(0);
                    if cur_pos > 0 {
                        let next_pos = cur_pos - 1;
                        let target = self.shuffle_order[next_pos];
                        self.shuffle_cursor = Some(next_pos);
                        self.select_queue_index(
                            target,
                            PlaybackStatus::Loading,
                            position_secs,
                            duration_secs,
                        );
                    } else if self.repeat == RepeatMode::All {
                        let last_pos = self.shuffle_order.len() - 1;
                        let target = self.shuffle_order[last_pos];
                        self.shuffle_cursor = Some(last_pos);
                        self.select_queue_index(
                            target,
                            PlaybackStatus::Loading,
                            position_secs,
                            duration_secs,
                        );
                    } else {
                        *position_secs = 0.0;
                    }
                    return;
                }
                let cur = self.queue_index.unwrap_or(0);
                if cur > 0 {
                    self.select_queue_index(
                        cur - 1,
                        PlaybackStatus::Loading,
                        position_secs,
                        duration_secs,
                    );
                } else if self.repeat == RepeatMode::All {
                    let last = self.queue.len() - 1;
                    self.select_queue_index(
                        last,
                        PlaybackStatus::Loading,
                        position_secs,
                        duration_secs,
                    );
                } else {
                    *position_secs = 0.0;
                }
            }
            PlayerCommand::Seek(secs) => {
                if !secs.is_finite() {
                    return;
                }
                *position_secs = if *duration_secs > 0.0 {
                    secs.clamp(0.0, *duration_secs)
                } else {
                    secs.max(0.0)
                };
            }
            PlayerCommand::SetVolume(volume) => {
                if !volume.is_finite() {
                    return;
                }
                let clamped = volume.clamp(0.0, 1.0);
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
                self.sync_shuffle_order();
            }
            PlayerCommand::CycleRepeat => {
                self.repeat = self.repeat.cycle();
            }
            PlayerCommand::EnqueueNext(track) => {
                if self.queue.is_empty() {
                    self.queue.push(track.clone());
                    let keep_status = self.status.clone();
                    self.select_queue_index(0, keep_status, position_secs, duration_secs);
                } else if self.current_track.is_none() {
                    self.queue.insert(0, track.clone());
                    let keep_status = self.status.clone();
                    self.select_queue_index(0, keep_status, position_secs, duration_secs);
                } else {
                    let insert_at = self
                        .queue_index
                        .map_or(0, |idx| (idx + 1).min(self.queue.len()));
                    self.queue.insert(insert_at, track.clone());
                }
                self.sync_shuffle_order();
            }
            PlayerCommand::EnqueueLast(track) => {
                let was_empty = self.queue.is_empty();
                self.queue.push(track.clone());
                if was_empty || self.current_track.is_none() {
                    let idx = if was_empty { 0 } else { self.queue.len() - 1 };
                    let keep_status = self.status.clone();
                    self.select_queue_index(idx, keep_status, position_secs, duration_secs);
                }
                self.sync_shuffle_order();
            }
            PlayerCommand::RemoveFromQueue(idx) => {
                if *idx >= self.queue.len() {
                    return;
                }
                self.queue.remove(*idx);
                if self.queue.is_empty() {
                    self.clear_playback(position_secs, duration_secs);
                    return;
                }
                let Some(cur) = self.queue_index else {
                    self.sync_shuffle_order();
                    return;
                };
                if *idx < cur {
                    self.queue_index = Some(cur - 1);
                } else if *idx == cur {
                    let next_status = if self.status.is_active() {
                        PlaybackStatus::Loading
                    } else {
                        self.status.clone()
                    };
                    if *idx < self.queue.len() {
                        self.select_queue_index(*idx, next_status, position_secs, duration_secs);
                    } else if self.repeat == RepeatMode::All {
                        self.select_queue_index(0, next_status, position_secs, duration_secs);
                    } else {
                        // Removed the last track under Repeat::Off / Repeat::One: stop cleanly
                        // without jumping backwards or unexpectedly auto-playing an earlier track.
                        self.current_track = None;
                        self.queue_index = None;
                        self.status = PlaybackStatus::Idle;
                        *position_secs = 0.0;
                        *duration_secs = 0.0;
                    }
                }
                self.sync_shuffle_order();
            }
            PlayerCommand::ClearQueue => {
                self.clear_playback(position_secs, duration_secs);
            }
            PlayerCommand::SetOfflineOnly(enabled) => {
                self.offline_only = *enabled;
            }
        }
    }

    fn advance_next(
        &mut self,
        obey_repeat_one: bool,
        position_secs: &mut f64,
        duration_secs: &mut f64,
    ) {
        if self.queue.is_empty() {
            return;
        }
        if obey_repeat_one && self.repeat == RepeatMode::One && self.current_track.is_some() {
            *position_secs = 0.0;
            self.status = PlaybackStatus::Loading;
            return;
        }
        if self.shuffle && !self.shuffle_order.is_empty() {
            match self.shuffle_cursor {
                Some(cur) if cur + 1 < self.shuffle_order.len() => {
                    let next_pos = cur + 1;
                    let target = self.shuffle_order[next_pos];
                    self.shuffle_cursor = Some(next_pos);
                    self.select_queue_index(
                        target,
                        PlaybackStatus::Loading,
                        position_secs,
                        duration_secs,
                    );
                }
                None => {
                    let target = self.shuffle_order[0];
                    self.shuffle_cursor = Some(0);
                    self.select_queue_index(
                        target,
                        PlaybackStatus::Loading,
                        position_secs,
                        duration_secs,
                    );
                }
                Some(_) if self.repeat == RepeatMode::All => {
                    let target = self.shuffle_order[0];
                    self.shuffle_cursor = Some(0);
                    self.select_queue_index(
                        target,
                        PlaybackStatus::Loading,
                        position_secs,
                        duration_secs,
                    );
                }
                Some(_) => {
                    *position_secs = *duration_secs;
                    self.status = PlaybackStatus::Paused;
                }
            }
            return;
        }
        match self.queue_index {
            Some(cur) if cur + 1 < self.queue.len() => {
                self.select_queue_index(
                    cur + 1,
                    PlaybackStatus::Loading,
                    position_secs,
                    duration_secs,
                );
            }
            None => {
                self.select_queue_index(0, PlaybackStatus::Loading, position_secs, duration_secs);
            }
            Some(_) if self.repeat == RepeatMode::All => {
                self.select_queue_index(0, PlaybackStatus::Loading, position_secs, duration_secs);
            }
            Some(_) => {
                *position_secs = *duration_secs;
                self.status = PlaybackStatus::Paused;
            }
        }
    }

    fn sync_shuffle_order(&mut self) {
        if !self.shuffle || self.queue.is_empty() {
            self.shuffle_order.clear();
            self.shuffle_cursor = None;
            return;
        }
        let len = self.queue.len();
        let current = self.queue_index.filter(|&i| i < len);
        let mut indices: Vec<usize> = (0..len).filter(|&i| Some(i) != current).collect();

        // Deterministic Fisher-Yates shuffle seeded from queue length & track IDs.
        let mut seed = (len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA076_1D64_78BD_642F;
        for track in &self.queue {
            for b in track.id.bytes().chain(track.title.bytes()) {
                seed = seed.wrapping_mul(0x517C_C1B7_2722_0A95) ^ u64::from(b);
            }
        }
        let mut i = indices.len();
        while i > 1 {
            i -= 1;
            seed = splitmix64(seed);
            let j = (seed as usize) % (i + 1);
            indices.swap(i, j);
        }

        self.shuffle_order.clear();
        if let Some(cur) = current {
            self.shuffle_order.push(cur);
            self.shuffle_order.extend(indices);
            self.shuffle_cursor = Some(0);
        } else {
            self.shuffle_order = indices;
            self.shuffle_cursor = None;
        }
    }

    fn select_queue_index(
        &mut self,
        index: usize,
        next_status: PlaybackStatus,
        position_secs: &mut f64,
        duration_secs: &mut f64,
    ) {
        if let Some(track) = self.queue.get(index).cloned() {
            let dur = track.duration_secs.map(f64::from).unwrap_or(0.0);
            self.queue_index = Some(index);
            self.current_track = Some(track);
            *position_secs = 0.0;
            *duration_secs = dur;
            self.status = next_status;
        }
    }

    fn clear_playback(&mut self, position_secs: &mut f64, duration_secs: &mut f64) {
        self.queue.clear();
        self.queue_index = None;
        self.current_track = None;
        self.shuffle_order.clear();
        self.shuffle_cursor = None;
        *position_secs = 0.0;
        *duration_secs = 0.0;
        self.status = PlaybackStatus::Idle;
    }

    /// Applies curation mutations (`starred`, `rating`, discovery promotion) to `current_track`
    /// and every matching item in `queue`. Empty `song_id` values are safely ignored.
    pub fn apply_curation_update(
        &mut self,
        song_id: &str,
        starred: Option<bool>,
        rating: Option<u8>,
        promoted_discovery: bool,
    ) {
        if song_id.trim().is_empty() {
            return;
        }
        let update_one = |t: &mut PlayerTrack| {
            let matches = (!t.id.is_empty() && t.id == song_id)
                || t.discovery_track_id
                    .as_deref()
                    .is_some_and(|did| !did.is_empty() && did == song_id);
            if !matches {
                return;
            }
            if let Some(s) = starred {
                t.starred = s;
            }
            if let Some(r) = rating {
                t.rating = r.min(5);
            }
            if promoted_discovery {
                t.is_discovery = false;
                t.discovery_track_id = None;
                t.days_remaining = None;
            }
        };
        if let Some(ref mut cur) = self.current_track {
            update_one(cur);
        }
        for item in &mut self.queue {
            update_one(item);
        }
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Shared Dioxus context exposing reactive playback state, isolated high-frequency clock
/// signals (`position_secs` / `duration_secs`), and command dispatch.
#[derive(Clone, Copy)]
pub struct PlayerContext {
    state: Signal<PlayerState>,
    position_secs: Signal<f64>,
    duration_secs: Signal<f64>,
    dispatcher: Callback<PlayerCommand>,
}

impl PlayerContext {
    pub fn new(
        state: Signal<PlayerState>,
        position_secs: Signal<f64>,
        duration_secs: Signal<f64>,
        dispatcher: Callback<PlayerCommand>,
    ) -> Self {
        Self {
            state,
            position_secs,
            duration_secs,
            dispatcher,
        }
    }

    /// Underlying state signal for platform backends (`web`, `desktop`) to push
    /// track, queue, status, or Connect device updates.
    pub fn state_signal(&self) -> Signal<PlayerState> {
        self.state
    }

    /// Dedicated high-frequency playback position signal (isolated from `PlayerState`
    /// so progress ticks only re-render scrubbers and timestamp labels).
    pub fn position_signal(&self) -> Signal<f64> {
        self.position_secs
    }

    /// Dedicated track duration signal in seconds.
    pub fn duration_signal(&self) -> Signal<f64> {
        self.duration_secs
    }

    /// Updates playback progress clock without notifying `PlayerState` subscribers.
    pub fn set_progress(&mut self, position: f64, duration: f64) {
        if position.is_finite() {
            self.position_secs.set(position.max(0.0));
        }
        if duration.is_finite() {
            self.duration_secs.set(duration.max(0.0));
        }
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

    pub fn queue_index(&self) -> Option<usize> {
        self.state.read().queue_index
    }

    pub fn position_secs(&self) -> f64 {
        *self.position_secs.read()
    }

    pub fn duration_secs(&self) -> f64 {
        *self.duration_secs.read()
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

    pub fn play_track_with_autoplay(&self, track: PlayerTrack, autoplay: bool) {
        self.dispatch(PlayerCommand::LoadQueue {
            tracks: vec![track],
            start_index: 0,
            autoplay,
        });
    }

    pub fn play_queue(&self, tracks: Vec<PlayerTrack>, start_index: usize) {
        self.dispatch(PlayerCommand::PlayQueue {
            tracks,
            start_index,
        });
    }

    pub fn play_queue_with_autoplay(
        &self,
        tracks: Vec<PlayerTrack>,
        start_index: usize,
        autoplay: bool,
    ) {
        self.dispatch(PlayerCommand::LoadQueue {
            tracks,
            start_index,
            autoplay,
        });
    }

    pub fn toggle_play_pause(&self) {
        self.dispatch(PlayerCommand::TogglePlayPause);
    }

    pub fn next(&self) {
        self.dispatch(PlayerCommand::Next);
    }

    pub fn track_ended(&self) {
        self.dispatch(PlayerCommand::TrackEnded);
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
    position_secs: Signal<f64>,
    duration_secs: Signal<f64>,
    on_command: Callback<PlayerCommand>,
    children: Element,
) -> Element {
    use_context_provider(|| PlayerContext::new(state, position_secs, duration_secs, on_command));
    rsx! { {children} }
}

/// Self-contained local provider for previews, tests, and offline/noop shells.
/// Applies all commands deterministically to local reactive state and clock signals.
#[component]
pub fn LocalPlayerProvider(
    #[props(default)] initial_state: Option<PlayerState>,
    children: Element,
) -> Element {
    let initial_dur = initial_state
        .as_ref()
        .and_then(|s| s.current_track.as_ref())
        .and_then(|t| t.duration_secs)
        .map(f64::from)
        .unwrap_or(0.0);

    let mut state = use_signal(move || initial_state.clone().unwrap_or_default());
    let mut position_secs = use_signal(|| 0.0f64);
    let mut duration_secs = use_signal(move || initial_dur);

    let dispatcher = use_callback(move |cmd: PlayerCommand| {
        let mut pos = *position_secs.peek();
        let mut dur = *duration_secs.peek();
        let (prev_pos, prev_dur) = (pos, dur);
        state.write().apply_local_command(&cmd, &mut pos, &mut dur);
        if (pos - prev_pos).abs() > f64::EPSILON {
            position_secs.set(pos);
        }
        if (dur - prev_dur).abs() > f64::EPSILON {
            duration_secs.set(dur);
        }
    });

    use_context_provider(|| PlayerContext::new(state, position_secs, duration_secs, dispatcher));
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
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![track("a", 180), track("b", 240)],
                start_index: 99,
            },
            &mut pos,
            &mut dur,
        );
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.current_track.as_ref().unwrap().id, "b");
        assert_eq!(dur, 240.0);
        assert_eq!(state.status, PlaybackStatus::Loading);
    }

    #[test]
    fn track_ended_obeys_repeat_one_while_explicit_next_advances() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![track("a", 100), track("b", 200)],
                start_index: 0,
            },
            &mut pos,
            &mut dur,
        );
        state.repeat = RepeatMode::One;
        pos = 95.0;

        // TrackEnded restarts current track under RepeatMode::One
        state.apply_local_command(&PlayerCommand::TrackEnded, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(0));
        assert_eq!(state.current_track.as_ref().unwrap().id, "a");
        assert_eq!(pos, 0.0);

        // Explicit Next skips past current track even under RepeatMode::One
        state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.current_track.as_ref().unwrap().id, "b");
    }

    #[test]
    fn next_and_previous_respect_repeat_all_and_3s_restart_rule() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![track("a", 100), track("b", 200)],
                start_index: 0,
            },
            &mut pos,
            &mut dur,
        );

        state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));

        // Past end with Repeat::Off pauses at end of duration
        state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.status, PlaybackStatus::Paused);
        assert_eq!(pos, 200.0);

        // Previous when position > 3s restarts current track instead of skipping back
        state.apply_local_command(&PlayerCommand::Previous, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(pos, 0.0);

        // Previous at 0s moves back to first track
        state.apply_local_command(&PlayerCommand::Previous, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(0));

        // Repeat::All wraps around both directions
        state.apply_local_command(&PlayerCommand::CycleRepeat, &mut pos, &mut dur);
        assert_eq!(state.repeat, RepeatMode::All);
        state.apply_local_command(&PlayerCommand::Previous, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));
        state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(0));
    }

    #[test]
    fn enqueue_on_empty_queue_selects_track_without_autoplaying() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::EnqueueLast(track("a", 150)),
            &mut pos,
            &mut dur,
        );
        assert_eq!(state.queue_index, Some(0));
        assert_eq!(state.current_track.as_ref().unwrap().id, "a");
        assert_eq!(dur, 150.0);
        assert_eq!(state.status, PlaybackStatus::Idle);

        // TogglePlayPause starts the queued track cleanly
        state.apply_local_command(&PlayerCommand::TogglePlayPause, &mut pos, &mut dur);
        assert_eq!(state.status, PlaybackStatus::Loading);
    }

    #[test]
    fn remove_from_queue_preserves_paused_state_and_stops_cleanly_at_end() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![track("a", 100), track("b", 120), track("c", 140)],
                start_index: 1,
            },
            &mut pos,
            &mut dur,
        );
        state.status = PlaybackStatus::Paused;

        // Removing active track while Paused stays Paused (never auto-plays!)
        state.apply_local_command(&PlayerCommand::RemoveFromQueue(1), &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.current_track.as_ref().unwrap().id, "c");
        assert_eq!(state.status, PlaybackStatus::Paused);

        // Removing the last track under Repeat::Off stops cleanly instead of jumping back to "a"
        state.apply_local_command(&PlayerCommand::RemoveFromQueue(1), &mut pos, &mut dur);
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue_index, None);
        assert!(state.current_track.is_none());
        assert_eq!(state.status, PlaybackStatus::Idle);

        // Removing the last track under Repeat::All wraps cleanly to index 0
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![track("a", 100), track("b", 120)],
                start_index: 1,
            },
            &mut pos,
            &mut dur,
        );
        state.status = PlaybackStatus::Playing;
        state.repeat = RepeatMode::All;
        state.apply_local_command(&PlayerCommand::RemoveFromQueue(1), &mut pos, &mut dur);
        assert_eq!(state.queue_index, Some(0));
        assert_eq!(state.current_track.as_ref().unwrap().id, "a");
        assert_eq!(state.status, PlaybackStatus::Loading);
    }

    #[test]
    fn toggle_play_pause_retries_errored_track_and_empty_song_id_is_ignored() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        let mut unindexed = track("", 120);
        unindexed.is_discovery = true;
        state.apply_local_command(&PlayerCommand::PlayTrack(unindexed), &mut pos, &mut dur);
        state.status = PlaybackStatus::Error("network".into());

        // Retrying errored track transitions cleanly back to Loading
        state.apply_local_command(&PlayerCommand::TogglePlayPause, &mut pos, &mut dur);
        assert_eq!(state.status, PlaybackStatus::Loading);

        // Empty song_id never mutates unindexed tracks
        state.apply_curation_update("", Some(true), Some(5), true);
        assert!(state.current_track.as_ref().unwrap().is_discovery);
        assert!(!state.current_track.as_ref().unwrap().starred);
    }

    #[test]
    fn shuffle_visits_every_track_preserving_current_and_supports_previous() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayQueue {
                tracks: vec![
                    track("a", 100),
                    track("b", 100),
                    track("c", 100),
                    track("d", 100),
                    track("e", 100),
                ],
                start_index: 2,
            },
            &mut pos,
            &mut dur,
        );
        state.apply_local_command(&PlayerCommand::ToggleShuffle, &mut pos, &mut dur);
        assert!(state.shuffle);
        assert_eq!(state.current_track.as_ref().unwrap().id, "c");

        let mut visited = vec![state.current_track.as_ref().unwrap().id.clone()];
        for _ in 0..4 {
            state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
            visited.push(state.current_track.as_ref().unwrap().id.clone());
        }
        let unique: std::collections::HashSet<_> = visited.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            5,
            "shuffle must visit all 5 unique tracks: {visited:?}"
        );

        // Next at end of shuffle with Repeat::Off pauses cleanly
        state.apply_local_command(&PlayerCommand::Next, &mut pos, &mut dur);
        assert_eq!(state.status, PlaybackStatus::Paused);

        // Previous walks backwards through the shuffled order
        pos = 0.0;
        state.apply_local_command(&PlayerCommand::Previous, &mut pos, &mut dur);
        assert_eq!(state.current_track.as_ref().unwrap().id, visited[3]);
    }

    #[test]
    fn seek_and_volume_ignore_non_finite_and_clamp_safely() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);
        state.apply_local_command(
            &PlayerCommand::PlayTrack(track("a", 120)),
            &mut pos,
            &mut dur,
        );
        state.apply_local_command(&PlayerCommand::Seek(42.0), &mut pos, &mut dur);
        assert_eq!(pos, 42.0);
        // Non-finite seek is ignored instead of resetting position to 0
        state.apply_local_command(&PlayerCommand::Seek(f64::NAN), &mut pos, &mut dur);
        assert_eq!(pos, 42.0);
        state.apply_local_command(&PlayerCommand::Seek(f64::INFINITY), &mut pos, &mut dur);
        assert_eq!(pos, 42.0);
        state.apply_local_command(&PlayerCommand::Seek(999.0), &mut pos, &mut dur);
        assert_eq!(pos, 120.0);
        state.apply_local_command(&PlayerCommand::Seek(-10.0), &mut pos, &mut dur);
        assert_eq!(pos, 0.0);

        state.apply_local_command(&PlayerCommand::SetVolume(0.4), &mut pos, &mut dur);
        state.apply_local_command(&PlayerCommand::ToggleMute, &mut pos, &mut dur);
        assert!(state.muted);
        // Non-finite volume is ignored without unmuting or resetting to 1.0
        state.apply_local_command(&PlayerCommand::SetVolume(f32::NAN), &mut pos, &mut dur);
        assert_eq!(state.volume, 0.4);
        assert!(state.muted);
        state.apply_local_command(&PlayerCommand::SetVolume(f32::INFINITY), &mut pos, &mut dur);
        assert_eq!(state.volume, 0.4);
        assert!(state.muted);

        // Valid finite volume unmutes and clamps
        state.apply_local_command(&PlayerCommand::SetVolume(2.5), &mut pos, &mut dur);
        assert_eq!(state.volume, 1.0);
        assert!(!state.muted);
    }

    #[test]
    fn load_queue_respects_autoplay_flag() {
        let mut state = PlayerState::default();
        let (mut pos, mut dur) = (0.0, 0.0);

        state.apply_local_command(
            &PlayerCommand::LoadQueue {
                tracks: vec![track("a", 180), track("b", 210)],
                start_index: 1,
                autoplay: false,
            },
            &mut pos,
            &mut dur,
        );
        assert_eq!(state.status, PlaybackStatus::Paused);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.current_track.as_ref().map(|t| t.id.as_str()), Some("b"));
        assert_eq!(dur, 210.0);

        state.apply_local_command(
            &PlayerCommand::LoadQueue {
                tracks: vec![track("a", 180), track("b", 210)],
                start_index: 0,
                autoplay: true,
            },
            &mut pos,
            &mut dur,
        );
        assert_eq!(state.status, PlaybackStatus::Loading);
        assert_eq!(state.queue_index, Some(0));
        assert_eq!(state.current_track.as_ref().map(|t| t.id.as_str()), Some("a"));
    }
}
