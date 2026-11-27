//! Concrete Web HTML5 `<audio>` player host, Soulbeet Connect WebSocket client,
//! Acquire & Play live handoff manager, OpenSubsonic scrobble tracker, and unified
//! curation/discovery state context.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[cfg(feature = "web")]
use std::cell::{Cell, RefCell};

#[cfg(feature = "web")]
use dioxus::logger::tracing::info;
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use futures::channel::mpsc::UnboundedSender;
#[cfg(feature = "web")]
use shared::client::ConnectCommand;
use shared::client::{
    ClientBootstrap, ConnectClientMessage, ConnectDevice, CurationAction, DeleteGuardPreview,
    DropTrackRequest, KeepTrackRequest, NavidromeSessionInfo, PlaybackSnapshot, RateTrackRequest,
    StarTrackRequest, TrackLyrics,
};
#[cfg(feature = "web")]
use shared::download::{AcquireAndPlayEvent, AcquireAndPlayStage};
use shared::download::{
    AcquireAndPlayInitialStatus, AcquireAndPlayRequest, ArchivalUpgradeJob, DownloadProgress,
    DownloadableGroup,
};
use shared::navidrome::DiscoveryTrack;
use shared::recommendation::{
    ContextualMixSummary, ContextualMixTrack, CultureSteering, SeedRadioRequest,
    SteerSeasonalPackRequest, UserCultureProfile,
};
use ui::{
    classify_guard_mode, discovery_days_remaining, format_candidate_reason, friendly_error,
    subsonic_cover_art_url, AcquireStageBadge, Auth, AutoDownloadSignal, DrawerTab, GuardModalMode,
    PendingDropItem, PendingDropQueue, PlaybackStatus, PlayerCommand, PlayerContext,
    PlayerProvider, PlayerState, PlayerTrack,
};

use crate::auth::use_auth;
use crate::navidrome_web::{
    fetch_playlists, fetch_recent_albums, fetch_starred_tracks, SubsonicAlbumSummary,
    SubsonicPlaylistSummary,
};

#[cfg(feature = "web")]
use shared::client::ConnectServerEvent;
#[cfg(feature = "web")]
use shared::download::DownloadEvent;

/// In-flight or terminal state for an `Acquire & Play` request.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquireTrackState {
    pub request_id: String,
    pub job_id: Option<String>,
    pub artist: String,
    pub title: String,
    pub badge: AcquireStageBadge,
    pub manual_choice: Option<(String, Vec<DownloadableGroup>)>,
}

/// Active manual source picker modal opened from `AcquireAndPlayStage::NeedsManualChoice`.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualChoiceModalState {
    pub artist: String,
    pub title: String,
    pub search_id: String,
    pub groups: Vec<DownloadableGroup>,
}

/// Active Multi-Listener Delete Guard confirmation modal state.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardModalState {
    pub track: PlayerTrack,
    pub preview: DeleteGuardPreview,
    pub busy: bool,
}

/// Builds all non-empty deduplicated triage guard keys for a track (`discovery_track_id` and `song_id`)
/// so concurrent actions from different UI surfaces (triage card vs player bar) share the same guard.
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

/// Normalized lookup key for mapping `Acquire & Play` events back to search/catalog rows.
pub fn acquire_key(artist: &str, title: &str) -> String {
    format!(
        "{}::{}",
        artist.trim().to_ascii_lowercase(),
        title.trim().to_ascii_lowercase()
    )
}

/// Deterministic per-play OpenSubsonic scrobble state tracker.
///
/// Enforces Last.fm / OpenSubsonic scrobble rules:
/// - Sends `submission=false` (now playing) at most ONCE per track play.
/// - Accumulates monotonic playback deltas (`0.0 < dt <= 2.0`, ignoring seek jumps).
/// - Sends `submission=true` at most ONCE when `duration >= 30.0` and
///   `accumulated >= min(duration * 0.5, 240.0)`.
#[cfg(any(feature = "web", test))]
#[derive(Debug, Clone, Default)]
pub struct ScrobbleTracker {
    song_id: String,
    last_time_secs: Option<f64>,
    accumulated_secs: f64,
    now_playing_sent: bool,
    scrobbled: bool,
}

#[cfg(any(feature = "web", test))]
impl ScrobbleTracker {
    pub fn reset(&mut self, song_id: String) {
        self.song_id = song_id;
        self.last_time_secs = None;
        self.accumulated_secs = 0.0;
        self.now_playing_sent = false;
        self.scrobbled = false;
    }

    /// Returns `Some(song_id)` the first time playback starts for this track play.
    pub fn on_playing(&mut self) -> Option<String> {
        if self.now_playing_sent || self.song_id.is_empty() {
            return None;
        }
        self.now_playing_sent = true;
        Some(self.song_id.clone())
    }

    /// Advances monotonic listen duration on `timeupdate`. Returns `Some(song_id)` once the
    /// scrobble threshold (`duration >= 30s` and `accumulated >= min(duration / 2, 240s)`) is met.
    pub fn on_timeupdate(&mut self, current_time: f64, duration: f64) -> Option<String> {
        if self.song_id.is_empty() || !current_time.is_finite() || current_time < 0.0 {
            return None;
        }
        if let Some(prev) = self.last_time_secs {
            let dt = current_time - prev;
            if dt > 0.0 && dt <= 2.0 {
                self.accumulated_secs += dt;
            }
        }
        self.last_time_secs = Some(current_time);

        if self.scrobbled || !duration.is_finite() || duration < 30.0 {
            return None;
        }
        let threshold = (duration * 0.5).min(240.0);
        if self.accumulated_secs >= threshold {
            self.scrobbled = true;
            Some(self.song_id.clone())
        } else {
            None
        }
    }
}

/// Reactive per-session star/rating/discovery overrides synced across visible track rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackCurationOverride {
    pub starred: Option<bool>,
    pub rating: Option<u8>,
    pub is_discovery: Option<bool>,
}

/// Shared reactive Studio state & actions provided to every view inside the authenticated shell.
#[derive(Clone, Copy)]
pub struct WebStudioContext {
    pub bootstrap: Signal<Option<ClientBootstrap>>,
    pub connect_devices: Signal<Vec<ConnectDevice>>,
    pub local_device_id: Signal<String>,
    pub connect_outbound: Signal<Option<UnboundedSender<ConnectClientMessage>>>,
    pub downloads: Signal<HashMap<String, DownloadProgress>>,
    pub downloads_drawer_open: Signal<bool>,
    pub downloads_toggle_ref: Signal<Option<Rc<MountedData>>>,
    pub outdated: Signal<bool>,
    pub acquire_states: Signal<HashMap<String, AcquireTrackState>>,
    pub manual_choice_modal: Signal<Option<ManualChoiceModalState>>,
    pub pending_drops: Signal<PendingDropQueue>,
    pub in_flight_triage: Signal<HashSet<String>>,
    pub optimistic_kept: Signal<HashSet<String>>,
    pub guard_modal: Signal<Option<GuardModalState>>,
    pub inspected_track: Signal<Option<PlayerTrack>>,
    pub drawer_tab: Signal<Option<DrawerTab>>,
    pub cmdk_open: Signal<bool>,
    pub toast_notice: Signal<Option<String>>,
    pub playlists: Signal<Vec<SubsonicPlaylistSummary>>,
    pub selected_playlist_id: Signal<Option<String>>,
    pub recent_albums: Signal<Vec<SubsonicAlbumSummary>>,
    pub starred_tracks: Signal<Vec<PlayerTrack>>,
    pub triage_tracks: Signal<Vec<DiscoveryTrack>>,
    pub discovery_lifetimes: Signal<HashMap<String, u32>>,
    pub contextual_mixes: Signal<Vec<ContextualMixSummary>>,
    pub contextual_mixes_gen: Signal<u64>,
    pub culture_profile: Signal<UserCultureProfile>,
    pub curation_overrides: Signal<HashMap<String, TrackCurationOverride>>,
    pub lyrics: Signal<Vec<TrackLyrics>>,
    pub lyrics_song_id: Signal<Option<String>>,
    pub lyrics_loading: Signal<bool>,
    pub lyrics_fetched: Signal<bool>,
    pub lyrics_req_id: Signal<u64>,
    pub lyrics_error: Signal<Option<String>>,
    pub inspector_guard_preview: Signal<Option<DeleteGuardPreview>>,
    pub inspector_guard_loading: Signal<bool>,
    pub guard_preview_req_id: Signal<u64>,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub transfer_gen: Signal<u64>,
    pub search_query_override: Signal<Option<String>>,
    pub archival_upgrades: Signal<Vec<ArchivalUpgradeJob>>,
    pub live_weather_location: Signal<bool>,
}

pub fn use_studio() -> WebStudioContext {
    use_context::<WebStudioContext>()
}

impl WebStudioContext {
    pub fn navidrome_info(&self) -> Option<NavidromeSessionInfo> {
        self.bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone())
    }

    pub fn set_live_weather_location(&self, enabled: bool) {
        let mut sig = self.live_weather_location;
        sig.set(enabled);
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            if enabled {
                let _ = storage.set_item(LIVE_WEATHER_LOCATION_KEY, "1");
            } else {
                let _ = storage.remove_item(LIVE_WEATHER_LOCATION_KEY);
            }
        }
    }

    pub fn next_contextual_mixes_gen(&self) -> u64 {
        let mut gen_sig = self.contextual_mixes_gen;
        let next = (*gen_sig.peek()).wrapping_add(1);
        gen_sig.set(next);
        next
    }

    pub fn apply_contextual_mixes_if_current(
        &self,
        gen: u64,
        mixes: Vec<ContextualMixSummary>,
    ) -> bool {
        if *self.contextual_mixes_gen.peek() != gen {
            return false;
        }
        let mut sig = self.contextual_mixes;
        sig.set(mixes);
        let mut this = *self;
        this.reclassify_playlists();
        true
    }

    pub fn has_active_acquisitions(&self) -> bool {
        self.acquire_states
            .read()
            .values()
            .any(|s| s.badge.is_in_flight())
    }

    pub fn active_transfers_count(&self) -> usize {
        let dl_count = self
            .downloads
            .read()
            .values()
            .filter(|d| d.in_flight())
            .count();
        let acq_count = self
            .acquire_states
            .read()
            .values()
            .filter(|s| s.badge.is_in_flight())
            .count();
        dl_count + acq_count
    }

    pub fn discovery_lifetime_for(&self, profile: &str) -> u32 {
        self.discovery_lifetimes
            .read()
            .get(profile)
            .copied()
            .unwrap_or(14)
    }

    /// Pending discovery tracks excluding any items optimistically hidden in the 5s Undo queue
    /// or optimistically kept while awaiting server confirmation.
    pub fn visible_triage_tracks(&self) -> Vec<DiscoveryTrack> {
        let pending = self.pending_drops.read();
        let kept = self.optimistic_kept.read();
        self.triage_tracks
            .read()
            .iter()
            .filter(|t| {
                !pending.is_hidden(&t.id)
                    && !kept.contains(&t.id)
                    && t.song_id
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .is_none_or(|sid| !pending.is_hidden(sid) && !kept.contains(sid))
            })
            .cloned()
            .collect()
    }

    /// Applies reactive star/rating/discovery overrides from this session onto a track before rendering.
    pub fn apply_curation_overrides(&self, track: &mut PlayerTrack) {
        if track.id.is_empty() {
            return;
        }
        if let Some(ov) = self.curation_overrides.read().get(&track.id).copied() {
            if let Some(s) = ov.starred {
                track.starred = s;
            }
            if let Some(r) = ov.rating {
                track.rating = r.min(5);
            }
            if let Some(is_disc) = ov.is_discovery {
                track.is_discovery = is_disc;
                if !is_disc {
                    track.discovery_track_id = None;
                    track.days_remaining = None;
                }
            }
        }
    }

    fn record_curation_override(
        &mut self,
        song_id: &str,
        starred: Option<bool>,
        rating: Option<u8>,
    ) {
        self.record_curation_override_with_discovery(song_id, starred, rating, None);
    }

    fn record_curation_override_with_discovery(
        &mut self,
        song_id: &str,
        starred: Option<bool>,
        rating: Option<u8>,
        is_discovery: Option<bool>,
    ) {
        if song_id.is_empty() {
            return;
        }
        let mut map = self.curation_overrides.write();
        let entry = map.entry(song_id.to_string()).or_default();
        if let Some(s) = starred {
            entry.starred = Some(s);
        }
        if let Some(r) = rating {
            entry.rating = Some(r.min(5));
        }
        if let Some(disc) = is_discovery {
            entry.is_discovery = Some(disc);
        }
        drop(map);

        // Keep `inspected_track` synchronized immediately
        let mut inspected = self.inspected_track.write();
        if let Some(t) = inspected.as_mut() {
            if !t.id.is_empty() && t.id == song_id {
                if let Some(s) = starred {
                    t.starred = s;
                }
                if let Some(r) = rating {
                    t.rating = r.min(5);
                }
                if let Some(disc) = is_discovery {
                    t.is_discovery = disc;
                    if !disc {
                        t.days_remaining = None;
                    }
                }
            }
        }
    }

    fn finalize_inspected_promoted_discovery(
        &mut self,
        effective_id: &str,
        orig_song_id: &str,
        discovery_track_id: Option<&str>,
    ) {
        let mut inspected = self.inspected_track.write();
        if let Some(t) = inspected.as_mut() {
            let matches = (!effective_id.is_empty() && t.id == effective_id)
                || (!orig_song_id.is_empty() && t.id == orig_song_id)
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

    fn set_triage_rating(
        &mut self,
        song_id: &str,
        discovery_track_id: Option<&str>,
        rating: u8,
    ) {
        let clamped = rating.min(5);
        let stored = (clamped > 0).then_some(clamped);
        let mut triage = self.triage_tracks.write();
        for dt in triage.iter_mut() {
            let match_song = !song_id.is_empty() && dt.song_id.as_deref() == Some(song_id);
            let match_disc = discovery_track_id
                .filter(|d| !d.is_empty())
                .is_some_and(|did| dt.id == did);
            if match_song || match_disc {
                dt.rating = stored;
            }
        }
    }

    pub fn show_toast(&mut self, msg: impl Into<String>) {
        let text = msg.into();
        self.toast_notice.set(Some(text.clone()));
        let mut sig = self.toast_notice;
        spawn(async move {
            gloo_timers::future::TimeoutFuture::new(3500).await;
            if sig.peek().as_deref() == Some(text.as_str()) {
                sig.set(None);
            }
        });
    }

    pub fn send_connect(&self, msg: ConnectClientMessage) {
        if let Some(tx) = self.connect_outbound.peek().as_ref() {
            let _ = tx.unbounded_send(msg);
        }
    }

    pub fn toggle_drawer(&mut self, tab: DrawerTab) {
        let current = *self.drawer_tab.peek();
        if current == Some(tab) {
            self.drawer_tab.set(None);
        } else {
            self.drawer_tab.set(Some(tab));
        }
    }

    pub fn inspect_track(&mut self, auth: Auth, mut track: PlayerTrack) {
        self.apply_curation_overrides(&mut track);
        let song_id = track.id.clone();
        self.inspected_track.set(Some(track));
        self.drawer_tab.set(Some(DrawerTab::Inspector));
        if !song_id.is_empty() {
            self.fetch_guard_preview(auth, song_id);
        }
    }

    pub fn track_from_discovery(&self, dt: &DiscoveryTrack) -> PlayerTrack {
        let lifetime_days = self.discovery_lifetime_for(&dt.profile);
        let nav = self.navidrome_info();
        let song_id = dt.song_id.clone().unwrap_or_default();
        let cover_art_url = nav
            .as_ref()
            .filter(|_| !song_id.is_empty())
            .map(|info| subsonic_cover_art_url(info, &song_id, 256));
        let reason_label = dt
            .reason
            .as_deref()
            .map(format_candidate_reason)
            .or_else(|| Some(format!("{} discovery", dt.profile)));
        let mut track = PlayerTrack {
            id: song_id.clone(),
            title: dt.title.clone(),
            artist: dt.artist.clone(),
            album: (!dt.album.is_empty()).then(|| dt.album.clone()),
            cover_art_url,
            cover_art_id: (!song_id.is_empty()).then_some(song_id),
            duration_secs: None,
            is_discovery: true,
            discovery_track_id: Some(dt.id.clone()),
            days_remaining: discovery_days_remaining(&dt.created_at, lifetime_days),
            starred: false,
            rating: dt.rating.unwrap_or(0).min(5),
            codec_badge: None,
            is_offline_cached: false,
            reason_label,
        };
        self.apply_curation_overrides(&mut track);
        track
    }

    /// Maps all currently visible pending discovery tracks with a playable Navidrome song ID
    /// into a continuous triage playback queue.
    pub fn triage_queue_tracks(&self, visible: &[DiscoveryTrack]) -> Vec<PlayerTrack> {
        visible
            .iter()
            .map(|dt| self.track_from_discovery(dt))
            .filter(|t| !t.id.is_empty())
            .collect()
    }

    /// Enqueues the full pending triage list starting at `selected` (or toggles play/pause when
    /// `selected` is already active inside the triage queue so pausing/resuming never restarts).
    pub fn play_or_toggle_triage(
        &self,
        player: PlayerContext,
        visible: &[DiscoveryTrack],
        selected: &PlayerTrack,
        allow_toggle_pause: bool,
    ) {
        if selected.id.is_empty() {
            player.play_track(selected.clone());
            return;
        }
        let queue = self.triage_queue_tracks(visible);
        if allow_toggle_pause {
            if player.is_track_playing(&selected.id) {
                player.toggle_play_pause();
                return;
            }
            let is_paused_in_triage_queue = {
                let st_sig = player.state_signal();
                let st = st_sig.read();
                st.current_track
                    .as_ref()
                    .is_some_and(|t| t.id == selected.id)
                    && st.queue_index.is_some_and(|idx| {
                        let upcoming = st.queue.get(idx..).unwrap_or(&[]);
                        let all_upcoming_in_triage = !upcoming.is_empty()
                            && upcoming
                                .iter()
                                .all(|q| queue.iter().any(|tq| tq.id == q.id));
                        all_upcoming_in_triage && (st.queue.len() > 1 || queue.len() <= 1)
                    })
            };
            if is_paused_in_triage_queue {
                player.toggle_play_pause();
                return;
            }
        }
        if let Some(start_index) = queue.iter().position(|t| t.id == selected.id) {
            player.play_queue(queue, start_index);
        } else {
            player.play_track(selected.clone());
        }
    }

    pub fn track_from_contextual(&self, ct: &ContextualMixTrack) -> PlayerTrack {
        let nav = self.navidrome_info();
        let cover_art_url = nav
            .as_ref()
            .filter(|_| !ct.song_id.is_empty())
            .map(|info| subsonic_cover_art_url(info, &ct.song_id, 256));
        let mut track = PlayerTrack {
            id: ct.song_id.clone(),
            title: ct.title.clone(),
            artist: ct.artist.clone(),
            album: ct.album.clone(),
            cover_art_url,
            cover_art_id: (!ct.song_id.is_empty()).then(|| ct.song_id.clone()),
            duration_secs: ct.duration_s,
            is_discovery: !ct.owned || ct.discovery_track_id.is_some(),
            discovery_track_id: ct.discovery_track_id.clone(),
            days_remaining: None,
            starred: ct.starred,
            rating: ct.rating.unwrap_or(0).min(5),
            codec_badge: None,
            is_offline_cached: false,
            reason_label: ct.reason.as_ref().map(format_candidate_reason),
        };
        self.apply_curation_overrides(&mut track);
        track
    }

    pub fn sync_playlist_count(&mut self, playlist_id: &str, count: u32) {
        if playlist_id.is_empty() {
            return;
        }
        let mut list = self.playlists.write();
        if let Some(pl) = list.iter_mut().find(|p| p.id == playlist_id) {
            pl.song_count = count;
        }
    }

    pub fn reclassify_playlists(&mut self) {
        let mixes = self.contextual_mixes.peek().clone();
        let mut list = self.playlists.write();
        for pl in list.iter_mut() {
            pl.category = ui::classify_playlist_category(
                &pl.id,
                Some(&pl.comment),
                pl.is_smart,
                &mixes,
            );
        }
    }

    pub fn fetch_lyrics_for(&mut self, auth: Auth, track: PlayerTrack) {
        let next_req = self.lyrics_req_id.peek().wrapping_add(1);
        self.lyrics_req_id.set(next_req);
        if track.id.is_empty() {
            self.lyrics.set(Vec::new());
            self.lyrics_song_id.set(None);
            self.lyrics_loading.set(false);
            self.lyrics_fetched.set(false);
            self.lyrics_error.set(None);
            return;
        }
        let song_id = track.id.clone();
        let artist = Some(track.artist.clone());
        let title = Some(track.title.clone());
        self.lyrics.set(Vec::new());
        self.lyrics_song_id.set(Some(song_id.clone()));
        self.lyrics_loading.set(true);
        self.lyrics_fetched.set(false);
        self.lyrics_error.set(None);

        let mut studio = *self;
        spawn(async move {
            match auth
                .call(api::get_track_lyrics(song_id.clone(), artist, title))
                .await
            {
                Ok(entries) => {
                    if *studio.lyrics_req_id.peek() == next_req
                        && studio.lyrics_song_id.peek().as_deref() == Some(song_id.as_str())
                    {
                        studio.lyrics.set(entries);
                        studio.lyrics_fetched.set(true);
                        studio.lyrics_loading.set(false);
                    }
                }
                Err(e) => {
                    if *studio.lyrics_req_id.peek() == next_req
                        && studio.lyrics_song_id.peek().as_deref() == Some(song_id.as_str())
                    {
                        studio.lyrics_error.set(Some(friendly_error(&e)));
                        studio.lyrics_fetched.set(true);
                        studio.lyrics_loading.set(false);
                    }
                }
            }
        });
    }

    pub fn fetch_guard_preview(&mut self, auth: Auth, song_id: String) {
        if song_id.is_empty() {
            return;
        }
        let next_req = self.guard_preview_req_id.peek().wrapping_add(1);
        self.guard_preview_req_id.set(next_req);
        self.inspector_guard_loading.set(true);
        let mut studio = *self;
        spawn(async move {
            let result = auth.call(api::curation_guard_preview(song_id)).await;
            if *studio.guard_preview_req_id.peek() != next_req {
                return;
            }
            match result {
                Ok(preview) => {
                    studio.inspector_guard_preview.set(Some(preview));
                }
                Err(e) => {
                    warn!("Guard preview failed: {e}");
                }
            }
            studio.inspector_guard_loading.set(false);
        });
    }

    pub fn keep_track(&mut self, auth: Auth, player: PlayerContext, track: PlayerTrack) {
        let mut studio = *self;
        let title = track.title.clone();
        let disc_id = track.discovery_track_id.clone();
        let orig_song_id = track.id.clone();
        let prev_is_discovery = track.is_discovery;
        let prev_days_remaining = track.days_remaining;
        let guard_keys = triage_guard_keys(disc_id.as_deref(), &orig_song_id);

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

        // Immediate optimistic UI update (0ms card removal; never stamps fake star ratings!)
        // Preserve `discovery_track_id` until server confirmation so rollback can always match.
        if !orig_song_id.is_empty() {
            player.state_signal().write().apply_curation_update(
                &orig_song_id,
                None,
                None,
                true,
            );
            self.record_curation_override_with_discovery(
                &orig_song_id,
                None,
                None,
                Some(false),
            );
        }
        if let Some(ref did) = disc_id {
            if !did.is_empty() && *did != orig_song_id {
                player
                    .state_signal()
                    .write()
                    .apply_curation_update(did, None, None, true);
            }
        }
        {
            let mut inspected = self.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!orig_song_id.is_empty() && t.id == orig_song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.is_discovery = false;
                    t.days_remaining = None;
                }
            }
        }

        let req = KeepTrackRequest {
            song_id: orig_song_id.clone(),
            discovery_track_id: disc_id.clone(),
            target_folder_id: None,
        };

        spawn(async move {
            match auth.call(api::curation_keep(req)).await {
                Ok(res) => {
                    let effective_id = if !res.song_id.is_empty() {
                        res.song_id.clone()
                    } else if !orig_song_id.is_empty() {
                        orig_song_id.clone()
                    } else {
                        disc_id.clone().unwrap_or_default()
                    };
                    if !effective_id.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            &effective_id,
                            None,
                            None,
                            true,
                        );
                        studio.record_curation_override_with_discovery(
                            &effective_id,
                            None,
                            None,
                            Some(false),
                        );
                    }
                    {
                        let mut st = player.state_signal();
                        let mut w = st.write();
                        w.finalize_promoted_discovery(&effective_id, disc_id.as_deref());
                        if !orig_song_id.is_empty() && orig_song_id != effective_id {
                            w.finalize_promoted_discovery(&orig_song_id, disc_id.as_deref());
                        }
                    }
                    studio.finalize_inspected_promoted_discovery(
                        &effective_id,
                        &orig_song_id,
                        disc_id.as_deref(),
                    );
                    studio.prune_triage_entry(&effective_id, disc_id.as_deref());
                    {
                        let mut kept = studio.optimistic_kept.write();
                        for key in &guard_keys {
                            kept.remove(key);
                        }
                        kept.remove(&effective_id);
                    }
                    if !guard_keys.is_empty() {
                        let mut in_flight = studio.in_flight_triage.write();
                        for key in &guard_keys {
                            in_flight.remove(key);
                        }
                    }
                    if res.archival_upgrade_queued {
                        studio.show_toast(format!(
                            "Kept \"{title}\" in library · archival FLAC upgrade queued"
                        ));
                    } else {
                        studio.show_toast(format!("Kept \"{title}\" in permanent library"));
                    }
                }
                Err(e) => {
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
                        if !orig_song_id.is_empty() {
                            studio.record_curation_override_with_discovery(
                                &orig_song_id,
                                None,
                                None,
                                Some(true),
                            );
                        }
                        player.state_signal().write().restore_discovery_state(
                            &orig_song_id,
                            disc_id.as_deref(),
                            prev_days_remaining,
                        );
                        let mut inspected = studio.inspected_track.write();
                        if let Some(t) = inspected.as_mut() {
                            let matches = (!orig_song_id.is_empty() && t.id == orig_song_id)
                                || (disc_id.is_some() && t.discovery_track_id == disc_id);
                            if matches {
                                t.is_discovery = true;
                                t.discovery_track_id = disc_id.clone();
                                t.days_remaining = prev_days_remaining;
                            }
                        }
                    }
                    studio.show_toast(format!("Keep failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn star_track(
        &mut self,
        auth: Auth,
        player: PlayerContext,
        track: PlayerTrack,
        starred: bool,
    ) {
        let mut studio = *self;
        let song_id = track.id.clone();
        let disc_id = track.discovery_track_id.clone();
        let title = track.title.clone();
        let track_for_list = track.clone();
        let is_disc_promote = starred && track.is_discovery;
        let prev_days_remaining = track.days_remaining;
        let guard_keys = triage_guard_keys(disc_id.as_deref(), &song_id);

        if is_disc_promote {
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

        let req = StarTrackRequest {
            song_id: song_id.clone(),
            starred,
            discovery_track_id: disc_id.clone(),
        };

        // Optimistic UI update across player + visible TrackRows + triage deck (never stamps fake stars!)
        // Preserve `discovery_track_id` until server confirmation so error rollback always matches.
        if !song_id.is_empty() {
            player.state_signal().write().apply_curation_update(
                &song_id,
                Some(starred),
                None,
                is_disc_promote,
            );
            studio.record_curation_override_with_discovery(
                &song_id,
                Some(starred),
                None,
                is_disc_promote.then_some(false),
            );
        }
        if is_disc_promote {
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
            let mut inspected = self.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!song_id.is_empty() && t.id == song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.starred = starred;
                    t.is_discovery = false;
                    t.days_remaining = None;
                }
            }
        }

        spawn(async move {
            match auth.call(api::curation_star(req)).await {
                Ok(res) => {
                    let final_starred = res.starred.unwrap_or(starred);
                    let effective_id = if !res.song_id.is_empty() {
                        res.song_id.clone()
                    } else {
                        song_id.clone()
                    };
                    let promoted = res.promoted_discovery || is_disc_promote;
                    if !effective_id.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            &effective_id,
                            Some(final_starred),
                            None,
                            promoted,
                        );
                        studio.record_curation_override_with_discovery(
                            &effective_id,
                            Some(final_starred),
                            None,
                            promoted.then_some(false),
                        );
                        studio.sync_starred_list(
                            &track_for_list,
                            &effective_id,
                            final_starred,
                            None,
                        );
                    }
                    if promoted {
                        {
                            let mut st = player.state_signal();
                            let mut w = st.write();
                            w.finalize_promoted_discovery(&effective_id, disc_id.as_deref());
                            if !song_id.is_empty() && song_id != effective_id {
                                w.finalize_promoted_discovery(&song_id, disc_id.as_deref());
                            }
                        }
                        studio.finalize_inspected_promoted_discovery(
                            &effective_id,
                            &song_id,
                            disc_id.as_deref(),
                        );
                        studio.prune_triage_entry(&effective_id, disc_id.as_deref());
                        studio.show_toast(format!(
                            "Favorited & kept \"{title}\" in permanent library"
                        ));
                    }
                    if is_disc_promote {
                        {
                            let mut kept = studio.optimistic_kept.write();
                            for key in &guard_keys {
                                kept.remove(key);
                            }
                            kept.remove(&effective_id);
                        }
                        if !guard_keys.is_empty() {
                            let mut in_flight = studio.in_flight_triage.write();
                            for key in &guard_keys {
                                in_flight.remove(key);
                            }
                        }
                    }
                }
                Err(e) => {
                    if is_disc_promote {
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
                        if song_id.is_empty() {
                            if let Some(ref did) = disc_id {
                                if !did.is_empty() {
                                    player.state_signal().write().apply_curation_update(
                                        did,
                                        Some(!starred),
                                        None,
                                        false,
                                    );
                                }
                            }
                        }
                        player.state_signal().write().restore_discovery_state(
                            &song_id,
                            disc_id.as_deref(),
                            prev_days_remaining,
                        );
                        let mut inspected = studio.inspected_track.write();
                        if let Some(t) = inspected.as_mut() {
                            let matches = (!song_id.is_empty() && t.id == song_id)
                                || (disc_id.is_some() && t.discovery_track_id == disc_id);
                            if matches {
                                t.starred = !starred;
                                t.is_discovery = true;
                                t.discovery_track_id = disc_id.clone();
                                t.days_remaining = prev_days_remaining;
                            }
                        }
                    }
                    if !song_id.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            &song_id,
                            Some(!starred),
                            None,
                            false,
                        );
                        studio.record_curation_override_with_discovery(
                            &song_id,
                            Some(!starred),
                            None,
                            is_disc_promote.then_some(true),
                        );
                    }
                    studio.show_toast(format!("Star failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn rate_track(
        &mut self,
        auth: Auth,
        player: PlayerContext,
        track: PlayerTrack,
        rating: u8,
    ) {
        let mut studio = *self;
        let song_id = track.id.clone();
        let disc_id = track.discovery_track_id.clone();
        let prev_rating = track.rating;
        let clamped_rating = rating.min(5);
        let req = RateTrackRequest {
            song_id: song_id.clone(),
            rating: clamped_rating,
            discovery_track_id: disc_id.clone(),
        };

        // Pure 1-5 star rating everywhere (never promotes or drops discovery tracks!)
        if !song_id.is_empty() {
            player.state_signal().write().apply_curation_update(
                &song_id,
                None,
                Some(clamped_rating),
                false,
            );
            studio.record_curation_override(&song_id, None, Some(clamped_rating));
        } else if let Some(ref did) = disc_id {
            if !did.is_empty() {
                player.state_signal().write().apply_curation_update(
                    did,
                    None,
                    Some(clamped_rating),
                    false,
                );
            }
        }
        studio.set_triage_rating(&song_id, disc_id.as_deref(), clamped_rating);
        {
            let mut stars = studio.starred_tracks.write();
            if let Some(slot) = stars
                .iter_mut()
                .find(|t| !song_id.is_empty() && t.id == song_id)
            {
                slot.rating = clamped_rating;
            }
        }
        {
            let mut inspected = studio.inspected_track.write();
            if let Some(t) = inspected.as_mut() {
                let matches = (!song_id.is_empty() && t.id == song_id)
                    || (disc_id.is_some() && t.discovery_track_id == disc_id);
                if matches {
                    t.rating = clamped_rating;
                }
            }
        }

        spawn(async move {
            match auth.call(api::curation_rate(req)).await {
                Ok(res) => {
                    let effective_id = if !res.song_id.is_empty() {
                        res.song_id.clone()
                    } else {
                        song_id.clone()
                    };
                    let final_rating = res.rating.unwrap_or(clamped_rating).min(5);
                    if !effective_id.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            &effective_id,
                            res.starred,
                            Some(final_rating),
                            false,
                        );
                        studio.record_curation_override(
                            &effective_id,
                            res.starred,
                            Some(final_rating),
                        );
                        let mut stars = studio.starred_tracks.write();
                        if let Some(slot) = stars.iter_mut().find(|t| t.id == effective_id) {
                            slot.rating = final_rating;
                            if let Some(s) = res.starred {
                                slot.starred = s;
                            }
                        }
                    }
                    studio.set_triage_rating(&effective_id, disc_id.as_deref(), final_rating);
                }
                Err(e) => {
                    if !song_id.is_empty() {
                        player.state_signal().write().apply_curation_update(
                            &song_id,
                            None,
                            Some(prev_rating),
                            false,
                        );
                        studio.record_curation_override(&song_id, None, Some(prev_rating));
                        let mut stars = studio.starred_tracks.write();
                        if let Some(slot) = stars.iter_mut().find(|t| t.id == song_id) {
                            slot.rating = prev_rating;
                        }
                    } else if let Some(ref did) = disc_id {
                        if !did.is_empty() {
                            player.state_signal().write().apply_curation_update(
                                did,
                                None,
                                Some(prev_rating),
                                false,
                            );
                        }
                    }
                    studio.set_triage_rating(&song_id, disc_id.as_deref(), prev_rating);
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
                    studio.show_toast(format!("Rating failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    fn sync_starred_list(
        &mut self,
        track: &PlayerTrack,
        effective_id: &str,
        starred: bool,
        rating: Option<u8>,
    ) {
        if effective_id.is_empty() {
            return;
        }
        let mut list = self.starred_tracks.write();
        if starred {
            if let Some(existing) = list.iter_mut().find(|t| t.id == effective_id) {
                existing.starred = true;
                if let Some(r) = rating {
                    existing.rating = r.min(5);
                }
            } else {
                let mut added = track.clone();
                added.id = effective_id.to_string();
                added.starred = true;
                added.is_discovery = false;
                added.discovery_track_id = None;
                if let Some(r) = rating {
                    added.rating = r.min(5);
                }
                list.insert(0, added);
            }
        } else {
            list.retain(|t| t.id != effective_id);
        }
    }

    fn prune_triage_entry(&mut self, song_id: &str, discovery_track_id: Option<&str>) {
        self.triage_tracks.write().retain(|t| {
            let match_song = !song_id.is_empty() && t.song_id.as_deref() == Some(song_id);
            let match_disc = discovery_track_id
                .filter(|d| !d.is_empty())
                .is_some_and(|did| t.id == did);
            !(match_song || match_disc)
        });
    }

    /// Context-aware drop/remove:
    /// - Pending Discovery: skips immediately if playing, hides optimistically, and shows 5s Undo snackbar.
    /// - Permanent Library: loads `curation_guard_preview` and opens `MultiListenerGuardModal`.
    pub fn drop_track(&mut self, auth: Auth, player: PlayerContext, track: PlayerTrack) {
        if track.is_discovery {
            if !track.id.is_empty() {
                player.remove_track_from_queue(&track.id);
            }
            let displaced = self.pending_drops.write().stage_drop(
                track.id.clone(),
                track.discovery_track_id.clone(),
                track.title.clone(),
                track.artist.clone(),
            );
            if let Some(prev) = displaced {
                self.commit_pending_drop(auth, prev);
            }
            return;
        }

        if track.id.is_empty() {
            return;
        }
        let mut studio = *self;
        let song_id = track.id.clone();
        spawn(async move {
            match auth.call(api::curation_guard_preview(song_id)).await {
                Ok(preview) => {
                    studio.guard_modal.set(Some(GuardModalState {
                        track,
                        preview,
                        busy: false,
                    }));
                }
                Err(e) => {
                    studio.show_toast(format!("Guard check failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn undo_pending_drop(&mut self) {
        let restored = self.pending_drops.write().undo_active();
        if let Some(restored) = restored {
            self.show_toast(format!("Restored \"{}\"", restored.title));
        }
    }

    /// Commits the active pending drop ONLY if its toast key matches `key`, preventing a
    /// stale timer from committing a subsequent drop.
    pub fn dismiss_and_commit_active_drop(&mut self, auth: Auth, key: String) {
        let item = self.pending_drops.write().commit_active_if_key(&key);
        if let Some(item) = item {
            self.commit_pending_drop(auth, item);
        }
    }

    /// Awaits any staged pending-discovery drop right before sign-out so the user's drop
    /// is never silently discarded when their session ends.
    pub async fn flush_pending_drops_now(&mut self, auth: Auth) {
        let item = self.pending_drops.write().drain_active();
        if let Some(item) = item {
            let req = DropTrackRequest {
                song_id: item.song_id.clone(),
                discovery_track_id: item.discovery_track_id.clone(),
                confirm_delete: false,
                reason: None,
            };
            let _ = auth.call(api::curation_drop(req)).await;
            self.pending_drops.write().finish_committed(&item);
        }
    }

    pub fn commit_pending_drop(&mut self, auth: Auth, item: PendingDropItem) {
        let mut studio = *self;
        let req = DropTrackRequest {
            song_id: item.song_id.clone(),
            discovery_track_id: item.discovery_track_id.clone(),
            confirm_delete: false,
            reason: None,
        };
        spawn(async move {
            match auth.call(api::curation_drop(req)).await {
                Ok(_) => {
                    studio.prune_triage_entry(&item.song_id, item.discovery_track_id.as_deref());
                    studio.pending_drops.write().finish_committed(&item);
                }
                Err(e) => {
                    studio.pending_drops.write().restore_failed(&item);
                    studio.show_toast(format!(
                        "Could not drop \"{}\": {}",
                        item.title,
                        friendly_error(&e)
                    ));
                }
            }
        });
    }

    /// Executes the Multi-Listener Delete Guard modal confirmation cleanly per mode:
    /// - `ListenerProtected`: directly calls `curation_star(false)` + `curation_rate(1)` so
    ///   Navidrome persists the unstar+1★ AND there is zero TOCTOU disk-deletion hazard.
    /// - `PermanentDelete`: calls `curation_drop(confirm_delete: true)` and handles
    ///   `ConfirmationRequired` / `DeletedFromLibrary` / `ProtectedAndUnstarred`.
    /// - `PendingDiscovery`: calls `curation_drop(confirm_delete: false)` and prunes triage.
    /// - `HardBlocked`: closes cleanly with zero mutation.
    pub fn confirm_guard_modal(&mut self, auth: Auth, player: PlayerContext) {
        let Some(current_modal) = self.guard_modal.peek().clone() else {
            return;
        };
        if current_modal.busy {
            return;
        }

        let mode = classify_guard_mode(&current_modal.preview);
        if mode == GuardModalMode::HardBlocked {
            self.guard_modal.set(None);
            return;
        }

        if let Some(m) = self.guard_modal.write().as_mut() {
            m.busy = true;
        }

        let mut studio = *self;
        let track = current_modal.track;

        spawn(async move {
            match mode {
                GuardModalMode::ListenerProtected => {
                    let unstar = auth
                        .call(api::curation_star(StarTrackRequest {
                            song_id: track.id.clone(),
                            starred: false,
                            discovery_track_id: None,
                        }))
                        .await;
                    let rate_down = auth
                        .call(api::curation_rate(RateTrackRequest {
                            song_id: track.id.clone(),
                            rating: 1,
                            discovery_track_id: None,
                        }))
                        .await;
                    match (unstar, rate_down) {
                        (Ok(_), Ok(_)) => {
                            studio.guard_modal.set(None);
                            if !track.id.is_empty() {
                                player.state_signal().write().apply_curation_update(
                                    &track.id,
                                    Some(false),
                                    Some(1),
                                    false,
                                );
                                studio.record_curation_override(&track.id, Some(false), Some(1));
                                studio.starred_tracks.write().retain(|t| t.id != track.id);
                                if player.is_track_current(&track.id) {
                                    player.next();
                                }
                            }
                            studio.show_toast(
                                "File kept for other listeners: unstarred & hidden from your mixes",
                            );
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            if let Some(m) = studio.guard_modal.write().as_mut() {
                                m.busy = false;
                            }
                            studio.show_toast(format!(
                                "Could not hide track: {}",
                                friendly_error(&e)
                            ));
                        }
                    }
                }
                GuardModalMode::PermanentDelete | GuardModalMode::PendingDiscovery => {
                    let req = DropTrackRequest {
                        song_id: track.id.clone(),
                        discovery_track_id: track.discovery_track_id.clone(),
                        confirm_delete: mode == GuardModalMode::PermanentDelete,
                        reason: None,
                    };
                    match auth.call(api::curation_drop(req)).await {
                        Ok(res) => match res.action {
                            CurationAction::ConfirmationRequired => {
                                if let Some(m) = studio.guard_modal.write().as_mut() {
                                    m.busy = false;
                                    if let Some(updated_preview) = res.guard_preview {
                                        m.preview = updated_preview;
                                    }
                                }
                                studio.show_toast(res.message);
                            }
                            CurationAction::DroppedDiscovery => {
                                studio.guard_modal.set(None);
                                studio.prune_triage_entry(
                                    &track.id,
                                    track.discovery_track_id.as_deref(),
                                );
                                if !track.id.is_empty() {
                                    player.remove_track_from_queue(&track.id);
                                }
                                studio.show_toast(res.message);
                            }
                            CurationAction::DeletedFromLibrary => {
                                studio.guard_modal.set(None);
                                if !track.id.is_empty() {
                                    studio.starred_tracks.write().retain(|t| t.id != track.id);
                                    player.remove_track_from_queue(&track.id);
                                }
                                studio.show_toast(res.message);
                            }
                            CurationAction::ProtectedAndUnstarred => {
                                studio.guard_modal.set(None);
                                if !track.id.is_empty() {
                                    player.state_signal().write().apply_curation_update(
                                        &track.id,
                                        Some(false),
                                        Some(1),
                                        false,
                                    );
                                    studio.record_curation_override(
                                        &track.id,
                                        Some(false),
                                        Some(1),
                                    );
                                    studio.starred_tracks.write().retain(|t| t.id != track.id);
                                    if player.is_track_current(&track.id) {
                                        player.next();
                                    }
                                }
                                studio.show_toast(res.message);
                            }
                            _ => {
                                studio.guard_modal.set(None);
                                studio.show_toast(res.message);
                            }
                        },
                        Err(e) => {
                            if let Some(m) = studio.guard_modal.write().as_mut() {
                                m.busy = false;
                            }
                            studio.show_toast(format!("Action failed: {}", friendly_error(&e)));
                        }
                    }
                }
                GuardModalMode::HardBlocked => {
                    studio.guard_modal.set(None);
                }
            }
        });
    }

    pub fn start_radio(&mut self, auth: Auth, player: PlayerContext, track: PlayerTrack) {
        if player.offline_only() {
            self.show_toast("Offline Only mode is active: Seed Radio requires a network connection.");
            return;
        }
        let mut studio = *self;
        let req = SeedRadioRequest {
            seed_song_id: (!track.id.is_empty()).then(|| track.id.clone()),
            seed_artist: Some(track.artist.clone()),
            seed_track: Some(track.title.clone()),
            seed_artist_mbid: None,
            limit: Some(25),
        };
        studio.show_toast(format!("Building radio from {}…", track.artist));
        spawn(async move {
            match auth.call(api::start_seed_radio(req)).await {
                Ok(res) => {
                    let nav = studio.navidrome_info();
                    let tracks: Vec<PlayerTrack> = res
                        .owned_tracks
                        .into_iter()
                        .map(|t| {
                            let cover = nav
                                .as_ref()
                                .map(|i| subsonic_cover_art_url(i, &t.song_id, 256));
                            let mut pt = PlayerTrack {
                                id: t.song_id.clone(),
                                title: t.title,
                                artist: t.artist,
                                album: t.album,
                                cover_art_url: cover,
                                cover_art_id: Some(t.song_id),
                                duration_secs: t.duration,
                                starred: t.starred,
                                rating: t.rating.unwrap_or(0).min(5),
                                reason_label: Some(format_candidate_reason(&t.reason)),
                                ..Default::default()
                            };
                            studio.apply_curation_overrides(&mut pt);
                            pt
                        })
                        .collect();
                    if tracks.is_empty() {
                        studio.show_toast("No matching library tracks found for radio seed.");
                    } else {
                        let count = tracks.len();
                        player.play_queue(tracks, 0);
                        studio.show_toast(format!(
                            "Started {} radio ({count} tracks)",
                            res.seed_label
                        ));
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Radio failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn play_contextual_mix(&mut self, auth: Auth, player: PlayerContext, slug: String) {
        if player.offline_only() {
            self.show_toast("Offline Only mode is active: contextual mixes require a network connection.");
            return;
        }
        let mut studio = *self;
        spawn(async move {
            match auth
                .call(api::get_contextual_mix_playlist(slug.clone(), Some(false)))
                .await
            {
                Ok(pl) => {
                    {
                        let mut mixes = studio.contextual_mixes.write();
                        if let Some(slot) = mixes.iter_mut().find(|m| m.slug == slug) {
                            *slot = pl.summary.clone();
                        }
                    }
                    let tracks: Vec<PlayerTrack> = pl
                        .tracks
                        .iter()
                        .map(|t| studio.track_from_contextual(t))
                        .collect();
                    if tracks.is_empty() {
                        studio.show_toast(
                            "No ready tracks in this mix yet. Generate Discovery or add library tracks.",
                        );
                    } else {
                        let title = ui::format_mix_title(&pl.summary);
                        let count = tracks.len();
                        player.play_queue(tracks, 0);
                        studio.show_toast(format!("Playing {title} ({count} tracks)"));
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Could not load mix: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn steer_seasonal_mix(&mut self, auth: Auth, slug: String, steering: CultureSteering) {
        let mut studio = *self;
        spawn(async move {
            match auth
                .call(api::steer_seasonal_pack(SteerSeasonalPackRequest {
                    slug: slug.clone(),
                    steering,
                }))
                .await
            {
                Ok(updated) => {
                    let mut mixes = studio.contextual_mixes.write();
                    if let Some(slot) = mixes.iter_mut().find(|m| m.slug == slug) {
                        *slot = updated;
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Steering failed: {}", friendly_error(&e)));
                }
            }
        });
    }

    pub fn trigger_acquire_and_play(
        &mut self,
        auth: Auth,
        player: PlayerContext,
        req: AcquireAndPlayRequest,
    ) {
        if player.offline_only() {
            self.show_toast("Disable Offline Only mode to acquire catalog tracks.");
            return;
        }
        let mut studio = *self;
        let key = acquire_key(&req.artist, &req.title);
        let artist = req.artist.clone();
        let title = req.title.clone();
        let album = req.album.clone();
        let duration_s = req.duration_s;

        studio.acquire_states.write().insert(
            key.clone(),
            AcquireTrackState {
                request_id: String::new(),
                job_id: None,
                artist: artist.clone(),
                title: title.clone(),
                badge: AcquireStageBadge::Acquiring { label: None },
                manual_choice: None,
            },
        );

        spawn(async move {
            match auth.call(api::acquire_and_play(req)).await {
                Ok(resp) => match resp.status {
                    AcquireAndPlayInitialStatus::AlreadyOwned {
                        navidrome_song_id,
                        codec,
                        ..
                    } => {
                        studio.acquire_states.write().remove(&key);
                        let cover = studio
                            .navidrome_info()
                            .map(|info| subsonic_cover_art_url(&info, &navidrome_song_id, 256));
                        let mut pt = PlayerTrack {
                            id: navidrome_song_id.clone(),
                            title: title.clone(),
                            artist: artist.clone(),
                            album,
                            cover_art_url: cover,
                            cover_art_id: Some(navidrome_song_id),
                            duration_secs: duration_s,
                            codec_badge: codec,
                            ..Default::default()
                        };
                        studio.apply_curation_overrides(&mut pt);
                        player.play_track(pt);
                    }
                    AcquireAndPlayInitialStatus::Started { job_id, .. } => {
                        if let Some(slot) = studio.acquire_states.write().get_mut(&key) {
                            slot.request_id = resp.request_id;
                            slot.job_id = Some(job_id);
                        }
                    }
                },
                Err(e) => {
                    studio.acquire_states.write().insert(
                        key,
                        AcquireTrackState {
                            request_id: String::new(),
                            job_id: None,
                            artist,
                            title,
                            badge: AcquireStageBadge::Failed {
                                message: Some(friendly_error(&e)),
                            },
                            manual_choice: None,
                        },
                    );
                }
            }
        });
    }
}

/// Applies an incoming `DownloadEvent::AcquireAndPlay` to `studio.acquire_states`
/// and auto-starts playback once `Ready { navidrome_song_id: Some(id), already_owned: false }`
/// arrives (already-owned tracks already started playback with full metadata via HTTP response).
#[cfg(feature = "web")]
pub fn handle_acquire_event(
    mut studio: WebStudioContext,
    player: PlayerContext,
    ev: AcquireAndPlayEvent,
) {
    let key = acquire_key(&ev.artist, &ev.title);
    let existing_job_id = studio
        .acquire_states
        .read()
        .get(&key)
        .and_then(|s| s.job_id.clone());
    match ev.stage {
        AcquireAndPlayStage::Ready {
            navidrome_song_id,
            codec,
            already_owned,
            ..
        } => {
            studio.acquire_states.write().remove(&key);
            if already_owned {
                return;
            }
            if let Some(song_id) = navidrome_song_id {
                let cover = studio
                    .navidrome_info()
                    .map(|info| subsonic_cover_art_url(&info, &song_id, 256));
                let mut pt = PlayerTrack {
                    id: song_id.clone(),
                    title: ev.title.clone(),
                    artist: ev.artist.clone(),
                    cover_art_url: cover,
                    cover_art_id: Some(song_id),
                    codec_badge: codec,
                    ..Default::default()
                };
                studio.apply_curation_overrides(&mut pt);
                player.play_track(pt);
                studio.show_toast(format!("Ready: playing \"{}\"", ev.title));
            } else {
                studio.show_toast(format!("Acquired \"{}\"", ev.title));
            }
        }
        AcquireAndPlayStage::NeedsManualChoice {
            search_id,
            results,
            best_score,
            threshold,
        } => {
            let badge = AcquireStageBadge::NeedsManualChoice {
                message: Some(format!(
                    "Best match {:.0}% below {:.0}%: pick a source",
                    best_score * 100.0,
                    threshold * 100.0
                )),
            };
            studio.acquire_states.write().insert(
                key,
                AcquireTrackState {
                    request_id: ev.request_id,
                    job_id: existing_job_id,
                    artist: ev.artist,
                    title: ev.title,
                    badge,
                    manual_choice: Some((search_id, results)),
                },
            );
        }
        other => {
            if let Some(badge) = AcquireStageBadge::from_stage(&other) {
                studio.acquire_states.write().insert(
                    key,
                    AcquireTrackState {
                        request_id: ev.request_id,
                        job_id: existing_job_id,
                        artist: ev.artist,
                        title: ev.title,
                        badge,
                        manual_choice: None,
                    },
                );
            }
        }
    }
}

/// Builds a `PlaybackSnapshot` from current player signals for `WS /api/ws/connect`.
pub fn build_connect_snapshot(player: PlayerContext) -> PlaybackSnapshot {
    let s = player.state_signal().peek().clone();
    let pos_ms = (*player.position_signal().peek() * 1000.0).max(0.0) as u64;
    let dur_ms = (*player.duration_signal().peek() * 1000.0).max(0.0) as u64;
    PlaybackSnapshot {
        song_id: s.current_track.as_ref().map(|t| t.id.clone()),
        title: s.current_track.as_ref().map(|t| t.title.clone()),
        artist: s.current_track.as_ref().map(|t| t.artist.clone()),
        album: s.current_track.as_ref().and_then(|t| t.album.clone()),
        cover_art_id: s
            .current_track
            .as_ref()
            .and_then(|t| t.cover_art_id.clone()),
        is_playing: s.status.is_playing(),
        position_ms: pos_ms,
        duration_ms: dur_ms,
        queue_song_ids: s.queue.iter().map(|t| t.id.clone()).collect(),
        queue_index: s.queue_index.unwrap_or(0),
        volume: s.volume,
        is_pending_discovery: s.current_track.as_ref().is_some_and(|t| t.is_discovery),
        updated_at_unix: chrono::Utc::now().timestamp(),
    }
}

#[cfg(feature = "web")]
type AudioEventListener = (
    &'static str,
    wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
);

/// Persistent HTML5 `<audio>` engine handle (browser-only).
/// Removes EVERY registered DOM event listener BEFORE calling `pause()` / `set_src("")` in `Drop`
/// so unmounting on sign-out can never fire a callback into a dropped WASM closure.
#[cfg(feature = "web")]
struct WebAudioEngine {
    audio: web_sys::HtmlAudioElement,
    load_gen: Rc<Cell<u64>>,
    loaded_track_id: Rc<RefCell<Option<String>>>,
    pending_seek_secs: Rc<Cell<Option<f64>>>,
    scrobble: Rc<RefCell<ScrobbleTracker>>,
    listeners: Vec<AudioEventListener>,
}

#[cfg(feature = "web")]
impl Drop for WebAudioEngine {
    fn drop(&mut self) {
        use wasm_bindgen::JsCast;
        // Detach all listeners first so pause()/set_src("") cannot invoke dropped closures.
        for (name, cb) in &self.listeners {
            let _ = self
                .audio
                .remove_event_listener_with_callback(name, cb.as_ref().unchecked_ref());
        }
        let _ = self.audio.pause();
        self.audio.set_src("");
    }
}

#[cfg(feature = "web")]
struct EndedListenerGuard {
    audio: web_sys::HtmlAudioElement,
    cb: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
}

#[cfg(feature = "web")]
impl Drop for EndedListenerGuard {
    fn drop(&mut self) {
        use wasm_bindgen::JsCast;
        let _ = self
            .audio
            .remove_event_listener_with_callback("ended", self.cb.as_ref().unchecked_ref());
    }
}

/// Host provider mounting `PlayerContext`, `WebStudioContext`, HTML5 `<audio>`,
/// `WS /api/downloads/updates`, `WS /api/ws/connect`, and initial bootstrap/discovery data.
#[component]
pub fn WebStudioProvider(children: Element) -> Element {
    let auth = use_auth();
    let mut player_state = use_signal(PlayerState::default);
    let mut position_secs = use_signal(|| 0.0f64);
    let mut duration_secs = use_signal(|| 0.0f64);

    let bootstrap = use_signal(|| None::<ClientBootstrap>);
    let connect_devices = use_signal(Vec::<ConnectDevice>::new);
    let local_device_id = use_signal(load_or_create_device_id);
    let connect_outbound = use_signal(|| None::<UnboundedSender<ConnectClientMessage>>);
    let downloads = use_signal::<HashMap<String, DownloadProgress>>(HashMap::new);
    let downloads_drawer_open = use_signal(|| false);
    let downloads_toggle_ref = use_signal(|| None::<Rc<MountedData>>);
    let outdated = use_signal(|| false);
    let acquire_states = use_signal::<HashMap<String, AcquireTrackState>>(HashMap::new);
    let manual_choice_modal = use_signal(|| None::<ManualChoiceModalState>);
    let pending_drops = use_signal(PendingDropQueue::new);
    let in_flight_triage = use_signal(HashSet::<String>::new);
    let optimistic_kept = use_signal(HashSet::<String>::new);
    let guard_modal = use_signal(|| None::<GuardModalState>);
    let inspected_track = use_signal(|| None::<PlayerTrack>);
    let drawer_tab = use_signal(|| None::<DrawerTab>);
    let cmdk_open = use_signal(|| false);
    let toast_notice = use_signal(|| None::<String>);
    let playlists = use_signal(Vec::<SubsonicPlaylistSummary>::new);
    let selected_playlist_id = use_signal(|| None::<String>);
    let recent_albums = use_signal(Vec::<SubsonicAlbumSummary>::new);
    let starred_tracks = use_signal(Vec::<PlayerTrack>::new);
    let triage_tracks = use_signal(Vec::<DiscoveryTrack>::new);
    let discovery_lifetimes = use_signal::<HashMap<String, u32>>(HashMap::new);
    let contextual_mixes = use_signal(Vec::<ContextualMixSummary>::new);
    let contextual_mixes_gen = use_signal(|| 0u64);
    let culture_profile = use_signal(UserCultureProfile::default);
    let curation_overrides = use_signal::<HashMap<String, TrackCurationOverride>>(HashMap::new);
    let lyrics = use_signal(Vec::<TrackLyrics>::new);
    let lyrics_song_id = use_signal(|| None::<String>);
    let lyrics_loading = use_signal(|| false);
    let lyrics_fetched = use_signal(|| false);
    let lyrics_req_id = use_signal(|| 0u64);
    let lyrics_error = use_signal(|| None::<String>);
    let mut seek_revision = use_signal(|| 0u64);
    let inspector_guard_preview = use_signal(|| None::<DeleteGuardPreview>);
    let inspector_guard_loading = use_signal(|| false);
    let guard_preview_req_id = use_signal(|| 0u64);
    let mut transfer_gen = use_signal(|| 0u64);
    let search_query_override = use_signal(|| None::<String>);
    let archival_upgrades = use_signal(Vec::<ArchivalUpgradeJob>::new);
    let live_weather_location = use_signal(load_live_weather_location);

    #[allow(unused_mut)]
    let mut auto_download_signal = use_signal(|| None::<shared::download::AutoDownloadEvent>);
    use_context_provider(|| AutoDownloadSignal(auto_download_signal));

    let studio = use_context_provider(|| WebStudioContext {
        bootstrap,
        connect_devices,
        local_device_id,
        connect_outbound,
        downloads,
        downloads_drawer_open,
        downloads_toggle_ref,
        outdated,
        acquire_states,
        manual_choice_modal,
        pending_drops,
        in_flight_triage,
        optimistic_kept,
        guard_modal,
        inspected_track,
        drawer_tab,
        cmdk_open,
        toast_notice,
        playlists,
        selected_playlist_id,
        recent_albums,
        starred_tracks,
        triage_tracks,
        discovery_lifetimes,
        contextual_mixes,
        contextual_mixes_gen,
        culture_profile,
        curation_overrides,
        lyrics,
        lyrics_song_id,
        lyrics_loading,
        lyrics_fetched,
        lyrics_req_id,
        lyrics_error,
        inspector_guard_preview,
        inspector_guard_loading,
        guard_preview_req_id,
        transfer_gen,
        search_query_override,
        archival_upgrades,
        live_weather_location,
    });

    // Browser HTML5 <audio> engine
    #[cfg(feature = "web")]
    let audio_handle: Rc<Option<WebAudioEngine>> = use_hook(|| {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        let Ok(audio) = web_sys::HtmlAudioElement::new() else {
            return Rc::new(None);
        };
        audio.set_preload("auto");

        let runtime = dioxus::core::Runtime::current();
        let scope_id = dioxus::core::current_scope_id();
        let load_gen = Rc::new(Cell::new(0u64));
        let loaded_track_id = Rc::new(RefCell::new(None::<String>));
        let pending_seek_secs = Rc::new(Cell::new(None::<f64>));
        let scrobble = Rc::new(RefCell::new(ScrobbleTracker::default()));
        let mut listeners: Vec<AudioEventListener> = Vec::new();

        // timeupdate -> isolated progress signals + monotonic scrobble submission
        {
            let audio_ref = audio.clone();
            let loaded_ref = Rc::clone(&loaded_track_id);
            let scrobble_ref = Rc::clone(&scrobble);
            let rt = Rc::clone(&runtime);
            let boot_sig = bootstrap;
            let mut pos_sig = position_secs;
            let mut dur_sig = duration_secs;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded {
                    return;
                }
                let ct = audio_ref.current_time();
                let dur = audio_ref.duration();
                let scrobble_song_id = scrobble_ref.borrow_mut().on_timeupdate(ct, dur);

                let nav_for_scrobble = rt.in_scope(scope_id, || {
                    if ct.is_finite() {
                        pos_sig.set(ct.max(0.0));
                    }
                    if dur.is_finite() && dur > 0.0 {
                        dur_sig.set(dur);
                    }
                    scrobble_song_id.as_ref().and_then(|_| {
                        boot_sig.peek().as_ref().and_then(|b| b.navidrome.clone())
                    })
                });

                if let (Some(song_id), Some(nav)) = (scrobble_song_id, nav_for_scrobble) {
                    wasm_bindgen_futures::spawn_local(async move {
                        crate::navidrome_web::send_scrobble(&nav, &song_id, true).await;
                    });
                }
            });
            let _ = audio
                .add_event_listener_with_callback("timeupdate", cb.as_ref().unchecked_ref());
            listeners.push(("timeupdate", cb));
        }

        // loadedmetadata -> accurate duration + deferred Connect handoff seek
        {
            let audio_ref = audio.clone();
            let loaded_ref = Rc::clone(&loaded_track_id);
            let pending_seek = Rc::clone(&pending_seek_secs);
            let rt = Rc::clone(&runtime);
            let mut pos_sig = position_secs;
            let mut dur_sig = duration_secs;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded {
                    return;
                }
                let dur = audio_ref.duration();
                let applied_seek = pending_seek.take().map(|target_secs| {
                    let clamped = if dur.is_finite() && dur > 0.0 {
                        target_secs.clamp(0.0, dur)
                    } else {
                        target_secs.max(0.0)
                    };
                    audio_ref.set_current_time(clamped);
                    clamped
                });
                rt.in_scope(scope_id, || {
                    if dur.is_finite() && dur > 0.0 {
                        dur_sig.set(dur);
                    }
                    if let Some(clamped) = applied_seek {
                        pos_sig.set(clamped);
                    }
                });
            });
            let _ = audio
                .add_event_listener_with_callback("loadedmetadata", cb.as_ref().unchecked_ref());
            listeners.push(("loadedmetadata", cb));
        }

        // playing -> mark Playing & send OpenSubsonic now-playing scrobble ONCE per play
        {
            let loaded_ref = Rc::clone(&loaded_track_id);
            let scrobble_ref = Rc::clone(&scrobble);
            let rt = Rc::clone(&runtime);
            let mut state_sig = player_state;
            let boot_sig = bootstrap;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded {
                    return;
                }
                // Release RefMut immediately before touching any Dioxus signal or async task.
                let now_playing_song_id = scrobble_ref.borrow_mut().on_playing();

                let nav_for_scrobble = rt.in_scope(scope_id, || {
                    state_sig.write().status = PlaybackStatus::Playing;
                    now_playing_song_id.as_ref().and_then(|_| {
                        boot_sig.peek().as_ref().and_then(|b| b.navidrome.clone())
                    })
                });

                if let (Some(song_id), Some(nav)) = (now_playing_song_id, nav_for_scrobble) {
                    wasm_bindgen_futures::spawn_local(async move {
                        crate::navidrome_web::send_scrobble(&nav, &song_id, false).await;
                    });
                }
            });
            let _ =
                audio.add_event_listener_with_callback("playing", cb.as_ref().unchecked_ref());
            listeners.push(("playing", cb));
        }

        // waiting -> Buffering
        {
            let loaded_ref = Rc::clone(&loaded_track_id);
            let rt = Rc::clone(&runtime);
            let mut state_sig = player_state;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded {
                    return;
                }
                rt.in_scope(scope_id, || {
                    if state_sig.peek().status.is_active() {
                        state_sig.write().status = PlaybackStatus::Buffering;
                    }
                });
            });
            let _ =
                audio.add_event_listener_with_callback("waiting", cb.as_ref().unchecked_ref());
            listeners.push(("waiting", cb));
        }

        // pause -> Paused (if not already ended/cleared)
        {
            let audio_ref = audio.clone();
            let loaded_ref = Rc::clone(&loaded_track_id);
            let rt = Rc::clone(&runtime);
            let mut state_sig = player_state;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded || audio_ref.ended() {
                    return;
                }
                rt.in_scope(scope_id, || {
                    if state_sig.peek().status == PlaybackStatus::Playing {
                        state_sig.write().status = PlaybackStatus::Paused;
                    }
                });
            });
            let _ = audio.add_event_listener_with_callback("pause", cb.as_ref().unchecked_ref());
            listeners.push(("pause", cb));
        }

        // error -> PlaybackStatus::Error (only for actively loaded track)
        {
            let audio_ref = audio.clone();
            let loaded_ref = Rc::clone(&loaded_track_id);
            let rt = Rc::clone(&runtime);
            let mut state_sig = player_state;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded || audio_ref.src().is_empty() {
                    return;
                }
                let msg = match audio_ref.error().map(|e| e.code()) {
                    Some(2) => "Network error while streaming track".to_string(),
                    Some(3) => "Audio decoding error".to_string(),
                    Some(4) => "Unsupported audio source or unreachable Navidrome".to_string(),
                    _ => "Playback error".to_string(),
                };
                rt.in_scope(scope_id, || {
                    state_sig.write().status = PlaybackStatus::Error(msg);
                });
            });
            let _ = audio.add_event_listener_with_callback("error", cb.as_ref().unchecked_ref());
            listeners.push(("error", cb));
        }

        Rc::new(Some(WebAudioEngine {
            audio,
            load_gen,
            loaded_track_id,
            pending_seek_secs,
            scrobble,
            listeners,
        }))
    });

    // Command dispatcher driving PlayerState + HTML5 <audio>
    #[cfg(feature = "web")]
    let audio_for_cmd = Rc::clone(&audio_handle);
    let dispatcher = use_callback(move |cmd: PlayerCommand| {
        #[cfg(feature = "web")]
        let (prev_track_id, prev_queue_index) = {
            let st = player_state.peek();
            (
                st.current_track.as_ref().map(|t| t.id.clone()),
                st.queue_index,
            )
        };

        let mut pos = *position_secs.peek();
        let mut dur = *duration_secs.peek();
        let (prev_pos, prev_dur) = (pos, dur);

        player_state
            .write()
            .apply_local_command(&cmd, &mut pos, &mut dur);

        if (pos - prev_pos).abs() > f64::EPSILON {
            position_secs.set(pos);
        }
        if (dur - prev_dur).abs() > f64::EPSILON {
            duration_secs.set(dur);
        }
        if matches!(cmd, PlayerCommand::Seek(_)) {
            let next = seek_revision.peek().wrapping_add(1);
            seek_revision.set(next);
        }
        if matches!(
            cmd,
            PlayerCommand::PlayTrack(_)
                | PlayerCommand::PlayQueue { .. }
                | PlayerCommand::LoadQueue { .. }
                | PlayerCommand::ClearQueue
                | PlayerCommand::RemoveFromQueue(_)
                | PlayerCommand::RemoveTrackFromQueue(_)
                | PlayerCommand::EnqueueNext(_)
                | PlayerCommand::EnqueueLast(_)
        ) {
            let next = transfer_gen.peek().wrapping_add(1);
            transfer_gen.set(next);
        }

        #[cfg(feature = "web")]
        if let Some(engine) = audio_for_cmd.as_ref() {
            let audio = &engine.audio;
            let next_state = player_state.peek().clone();
            let next_track_id = next_state.current_track.as_ref().map(|t| t.id.clone());
            let active_slot_removed = matches!(
                &cmd,
                PlayerCommand::RemoveFromQueue(idx) if prev_queue_index == Some(*idx)
            ) || matches!(
                &cmd,
                PlayerCommand::RemoveTrackFromQueue(id)
                    if !id.is_empty() && prev_track_id.as_deref() == Some(id.as_str())
            );
            let track_changed = next_track_id != prev_track_id
                || active_slot_removed
                || matches!(
                    cmd,
                    PlayerCommand::PlayTrack(_)
                        | PlayerCommand::PlayQueue { .. }
                        | PlayerCommand::LoadQueue { .. }
                        | PlayerCommand::TrackEnded
                );

            match &cmd {
                PlayerCommand::SetVolume(_) | PlayerCommand::ToggleMute => {
                    audio.set_volume(f64::from(next_state.volume));
                    audio.set_muted(next_state.muted);
                }
                PlayerCommand::Seek(secs) => {
                    if secs.is_finite() {
                        if audio.ready_state() >= 1 {
                            engine.pending_seek_secs.set(None);
                            audio.set_current_time(pos);
                        } else {
                            engine.pending_seek_secs.set(Some(pos));
                        }
                    }
                }
                PlayerCommand::ClearQueue => {
                    clear_web_audio(engine);
                }
                PlayerCommand::LoadQueue { autoplay, .. } => {
                    if next_state.current_track.is_none() {
                        clear_web_audio(engine);
                    } else if let Some(track) = next_state.current_track.as_ref() {
                        start_web_audio_track(
                            engine,
                            track,
                            bootstrap.peek().as_ref().and_then(|b| b.navidrome.as_ref()),
                            &mut player_state,
                            *autoplay,
                        );
                    }
                }
                PlayerCommand::TogglePlayPause => {
                    if next_state.status == PlaybackStatus::Paused {
                        let _ = audio.pause();
                    } else if next_state.status == PlaybackStatus::Playing {
                        let loaded_matches = engine
                            .loaded_track_id
                            .borrow()
                            .as_deref()
                            .is_some_and(|id| Some(id) == next_track_id.as_deref());
                        if !loaded_matches {
                            if let Some(track) = next_state.current_track.as_ref() {
                                start_web_audio_track(
                                    engine,
                                    track,
                                    bootstrap.peek().as_ref().and_then(|b| b.navidrome.as_ref()),
                                    &mut player_state,
                                    true,
                                );
                            }
                        } else {
                            let expected_gen = engine.load_gen.get();
                            let gen_ref = Rc::clone(&engine.load_gen);
                            if let Ok(promise) = audio.play() {
                                let mut st = player_state;
                                spawn(async move {
                                    if wasm_bindgen_futures::JsFuture::from(promise).await.is_err()
                                        && gen_ref.get() == expected_gen
                                        && st.peek().status == PlaybackStatus::Playing
                                    {
                                        st.write().status = PlaybackStatus::Paused;
                                    }
                                });
                            }
                        }
                    } else if next_state.status == PlaybackStatus::Loading {
                        if let Some(track) = next_state.current_track.as_ref() {
                            start_web_audio_track(
                                engine,
                                track,
                                bootstrap.peek().as_ref().and_then(|b| b.navidrome.as_ref()),
                                &mut player_state,
                                true,
                            );
                        }
                    }
                }
                _ => {
                    if next_state.current_track.is_none() {
                        clear_web_audio(engine);
                    } else if track_changed && next_state.status == PlaybackStatus::Paused {
                        // Track changed while Paused (e.g. removed active track while paused):
                        // clear stale audio source so resuming later loads the new track cleanly.
                        clear_web_audio(engine);
                    } else if track_changed && next_state.status == PlaybackStatus::Loading {
                        if let Some(track) = next_state.current_track.as_ref() {
                            start_web_audio_track(
                                engine,
                                track,
                                bootstrap.peek().as_ref().and_then(|b| b.navidrome.as_ref()),
                                &mut player_state,
                                true,
                            );
                        }
                    } else if matches!(cmd, PlayerCommand::Previous) && !track_changed {
                        audio.set_current_time(pos);
                    } else if next_state.status == PlaybackStatus::Paused {
                        let _ = audio.pause();
                    }
                }
            }
        }
    });

    // Attach `ended` listener with explicit Drop guard and active Dioxus Runtime scope.
    #[cfg(feature = "web")]
    {
        let audio_for_ended = Rc::clone(&audio_handle);
        let _ended_guard = use_hook(move || {
            use wasm_bindgen::closure::Closure;
            use wasm_bindgen::JsCast;
            let engine = audio_for_ended.as_ref().as_ref()?;
            let runtime = dioxus::core::Runtime::current();
            let scope_id = dioxus::core::current_scope_id();
            let loaded_ref = Rc::clone(&engine.loaded_track_id);
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let is_loaded = loaded_ref.borrow().is_some();
                if !is_loaded {
                    return;
                }
                runtime.in_scope(scope_id, || {
                    dispatcher.call(PlayerCommand::TrackEnded);
                });
            });
            let _ = engine
                .audio
                .add_event_listener_with_callback("ended", cb.as_ref().unchecked_ref());
            Some(Rc::new(EndedListenerGuard {
                audio: engine.audio.clone(),
                cb,
            }))
        });
    }

    let player_ctx = PlayerContext::new(player_state, position_secs, duration_secs, dispatcher);

    // Fetch bootstrap + initial Discovery 2.0 & Navidrome library data on mount
    use_future(move || {
        let mut studio = studio;
        async move {
            // 1. Bootstrap (OpenSubsonic coordinates + capabilities)
            if let Ok(boot) = auth.call(api::get_client_bootstrap()).await {
                let nav_opt = boot.navidrome.clone();
                studio.bootstrap.set(Some(boot));
                if let Some(nav) = nav_opt {
                    let (pl_res, alb_res, star_res) = futures::future::join3(
                        fetch_playlists(&nav),
                        fetch_recent_albums(&nav, 12),
                        fetch_starred_tracks(&nav),
                    )
                    .await;
                    let mut nav_err: Option<String> = None;
                    match pl_res {
                        Ok(pls) => studio.playlists.set(pls),
                        Err(e) => {
                            warn!("Navidrome getPlaylists failed: {e}");
                            nav_err.get_or_insert(e);
                        }
                    }
                    match alb_res {
                        Ok(albs) => studio.recent_albums.set(albs),
                        Err(e) => {
                            warn!("Navidrome getAlbumList2 failed: {e}");
                            nav_err.get_or_insert(e);
                        }
                    }
                    match star_res {
                        Ok(stars) => studio.starred_tracks.set(stars),
                        Err(e) => {
                            warn!("Navidrome getStarred2 failed: {e}");
                            nav_err.get_or_insert(e);
                        }
                    }
                    if let Some(err) = nav_err {
                        studio.show_toast(format!("Navidrome browse warning: {err}"));
                    }
                }
            }

            // 2. Discovery 2.0 parallel bootstrap (spawn contextual mixes independently so slow
            // browser geolocation never blocks triage, culture, archival upgrades, or config).
            #[cfg_attr(not(feature = "web"), allow(unused_mut))]
            let mut studio_mix = studio;
            let auth_mix = auth;
            spawn(async move {
                let gen = studio_mix.next_contextual_mixes_gen();
                let live_location = *studio_mix.live_weather_location.peek();
                let (mix_res, perm_denied) =
                    fetch_contextual_mixes_with_context(&auth_mix, live_location).await;
                if perm_denied {
                    studio_mix.set_live_weather_location(false);
                }
                if let Ok(mixes) = mix_res {
                    if studio_mix.apply_contextual_mixes_if_current(gen, mixes) {
                        // If user has > 25 playlists, also resolve live counts for any matched
                        // contextual mix playlists that weren't resolved during initial `fetch_playlists`.
                        #[cfg(feature = "web")]
                        if studio_mix.playlists.peek().len() > 25 {
                            if let Some(nav) = studio_mix.navidrome_info() {
                                let mix_ids: Vec<String> = studio_mix
                                    .playlists
                                    .peek()
                                    .iter()
                                    .filter(|p| p.category.is_soulbeet_mix() && !p.is_smart)
                                    .map(|p| p.id.clone())
                                    .collect();
                                if !mix_ids.is_empty() {
                                    use futures::stream::{self, StreamExt};
                                    let resolved: Vec<(String, Option<u32>)> =
                                        stream::iter(mix_ids.into_iter().map(|pid| {
                                            let nav = nav.clone();
                                            async move {
                                                let cnt =
                                                    crate::navidrome_web::fetch_playlist_live_count(
                                                        &nav, &pid,
                                                    )
                                                    .await;
                                                (pid, cnt)
                                            }
                                        }))
                                        .buffer_unordered(6)
                                        .collect()
                                        .await;
                                    for (pid, cnt) in resolved {
                                        if let Some(count) = cnt {
                                            studio_mix.sync_playlist_count(&pid, count);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            });

            let (triage_res, culture_res, archival_res, cfg_res) =
                futures::future::join4(
                    auth.call(api::list_triage_queue()),
                    auth.call(api::get_culture_profile()),
                    auth.call(api::list_archival_upgrades()),
                    auth.call(api::get_discovery_config()),
                )
                .await;
            if let Ok(triage) = triage_res {
                studio.triage_tracks.set(triage);
            }
            if let Ok(profile) = culture_res {
                studio.culture_profile.set(profile);
            }
            if let Ok(upgrades) = archival_res {
                studio.archival_upgrades.set(upgrades);
            }
            if let Ok(cfg) = cfg_res {
                studio.discovery_lifetimes.set(cfg.lifetime_days);
            }
        }
    });

    // Clear stale lyrics immediately on track change (even when Lyrics tab is closed),
    // and auto-fetch when the Lyrics tab is open.
    use_effect(move || {
        let mut studio = studio;
        let tab = (studio.drawer_tab)();
        let current = player_ctx.current_track();
        let current_id = current
            .as_ref()
            .filter(|t| !t.id.is_empty())
            .map(|t| t.id.clone());

        if *studio.lyrics_song_id.peek() != current_id {
            let next_req = studio.lyrics_req_id.peek().wrapping_add(1);
            studio.lyrics_req_id.set(next_req);
            studio.lyrics.set(Vec::new());
            studio.lyrics_error.set(None);
            studio.lyrics_loading.set(false);
            studio.lyrics_fetched.set(false);
            studio.lyrics_song_id.set(None);
        }

        if tab == Some(DrawerTab::Lyrics) {
            if let Some(track) = current {
                if !track.id.is_empty()
                    && studio.lyrics_song_id.peek().as_deref() != Some(track.id.as_str())
                {
                    studio.fetch_lyrics_for(auth, track);
                }
            }
        }
    });

    // Push state snapshots to Soulbeet Connect ONLY when track, play/pause status, or seek changes
    // (memoized so unrelated `PlayerState` updates such as volume/mute/curation/keepalive never
    // emit redundant `UpdateState` frames).
    let mut last_pushed_connect_key =
        use_signal(|| None::<(Option<String>, PlaybackStatus, u64)>);
    use_effect(move || {
        let track_id = player_ctx.current_track().map(|t| t.id);
        let status = player_ctx.status();
        let seek_rev = seek_revision();
        let next_key = (track_id, status, seek_rev);
        if last_pushed_connect_key.peek().as_ref() == Some(&next_key) {
            return;
        }
        last_pushed_connect_key.set(Some(next_key));
        let snap = build_connect_snapshot(player_ctx);
        studio.send_connect(ConnectClientMessage::UpdateState { snapshot: snap });
    });

    // Download & Acquire-and-Play WebSocket (`/api/downloads/updates`)
    #[cfg(feature = "web")]
    {
        let mut dl_sig = downloads;
        let mut outdated_sig = outdated;
        crate::websocket::use_resilient_websocket(
            || {
                api::download_updates_ws(
                    Some(true),
                    Some(true),
                    dioxus::fullstack::WebSocketOptions::new(),
                )
            },
            move |event: DownloadEvent| {
                if *outdated_sig.peek() {
                    return;
                }
                match event {
                    DownloadEvent::Progress(data) => {
                        let mut map = dl_sig.write();
                        for file in data {
                            map.insert(file.item.clone(), file);
                        }
                    }
                    DownloadEvent::Snapshot(rows) => {
                        shared::download::apply_snapshot(&mut dl_sig.write(), rows);
                    }
                    DownloadEvent::Replaced {
                        previous_item,
                        entry,
                    } => {
                        let mut map = dl_sig.write();
                        map.remove(&previous_item);
                        map.insert(entry.item.clone(), *entry);
                    }
                    DownloadEvent::AutoDownload(auto_event) => {
                        auto_download_signal.set(Some(auto_event));
                    }
                    DownloadEvent::AcquireAndPlay(acquire_event) => {
                        handle_acquire_event(studio, player_ctx, acquire_event);
                    }
                    DownloadEvent::Build(server_build) => {
                        if !api::build_id::is_current(&server_build) {
                            outdated_sig.set(true);
                        }
                    }
                }
            },
        );
    }

    // Soulbeet Connect bidirectional WebSocket (`/api/ws/connect`)
    #[cfg(feature = "web")]
    {
        let audio_for_connect = Rc::clone(&audio_handle);
        use_future(move || {
            let mut studio = studio;
            let audio_for_connect = Rc::clone(&audio_for_connect);
            async move {
                use futures::StreamExt;
                let device_id = studio.local_device_id.peek().clone();
                let device_name = "Web Studio".to_string();
                let mut retry_count = 0u32;

                loop {
                    // Query parameters register this device on upgrade (no duplicate Register frame).
                    let conn = api::connect_ws(
                        Some(device_id.clone()),
                        Some(device_name.clone()),
                        Some("web".to_string()),
                        dioxus::fullstack::WebSocketOptions::new(),
                    )
                    .await;

                    match conn {
                        Ok(socket) => {
                            info!("Connected to Soulbeet Connect WS as {device_id}");
                            let (tx, rx) =
                                futures::channel::mpsc::unbounded::<ConnectClientMessage>();
                            studio.connect_outbound.set(Some(tx));

                            // Immediately publish our current snapshot on connect / reconnect.
                            // If this fails, fall through to cleanup + exponential backoff (never spin).
                            let initial_snap = build_connect_snapshot(player_ctx);
                            if socket
                                .send(ConnectClientMessage::UpdateState {
                                    snapshot: initial_snap,
                                })
                                .await
                                .is_ok()
                            {
                                // Persistent intervals live OUTSIDE the loop so incoming frames
                                // never starve heartbeats or active playback position sync.
                                let mut heartbeat =
                                    gloo_timers::future::IntervalStream::new(25_000).fuse();
                                let mut position_sync =
                                    gloo_timers::future::IntervalStream::new(5_000).fuse();
                                let mut rx = rx.fuse();
                                let mut confirmed_exchange = false;

                                loop {
                                    let recv_fut = socket.recv();
                                    futures::pin_mut!(recv_fut);

                                    futures::select! {
                                        _ = heartbeat.select_next_some() => {
                                            if socket.send(ConnectClientMessage::Heartbeat).await.is_err() {
                                                break;
                                            }
                                        }
                                        _ = position_sync.select_next_some() => {
                                            if player_ctx.is_playing() {
                                                let snap = build_connect_snapshot(player_ctx);
                                                if socket
                                                    .send(ConnectClientMessage::UpdateState { snapshot: snap })
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        }
                                        outgoing = rx.next() => {
                                            if let Some(msg) = outgoing {
                                                if socket.send(msg).await.is_err() {
                                                    break;
                                                }
                                            } else {
                                                break;
                                            }
                                        }
                                        incoming = futures::FutureExt::fuse(recv_fut) => {
                                            match incoming {
                                                Ok(event) => {
                                                    if !confirmed_exchange {
                                                        confirmed_exchange = true;
                                                        retry_count = 0;
                                                    }
                                                    match event {
                                                        ConnectServerEvent::DeviceList { devices, .. } => {
                                                            let active_name = devices
                                                                .iter()
                                                                .find(|d| d.is_active)
                                                                .map(|d| d.name.clone());
                                                            if player_ctx.state_signal().peek().active_device_name != active_name {
                                                                player_ctx.state_signal().write().active_device_name = active_name;
                                                            }
                                                            if !connect_devices_visibly_equal(&studio.connect_devices.peek(), &devices) {
                                                                studio.connect_devices.set(devices);
                                                            }
                                                        }
                                                        ConnectServerEvent::PlaybackUpdated { device_id: dev, snapshot } => {
                                                            let mut list = studio.connect_devices.write();
                                                            if let Some(item) = list.iter_mut().find(|d| d.device_id == dev) {
                                                                item.snapshot = Some(snapshot);
                                                            }
                                                        }
                                                        ConnectServerEvent::Command { command, .. } => {
                                                            apply_remote_connect_command(
                                                                studio,
                                                                player_ctx,
                                                                audio_for_connect.as_ref().as_ref(),
                                                                command,
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!("Connect WS closed: {e:?}");
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            studio.connect_outbound.set(None);
                            if !studio.connect_devices.peek().is_empty() {
                                studio.connect_devices.set(Vec::new());
                            }
                            if player_ctx.state_signal().peek().active_device_name.is_some() {
                                player_ctx.state_signal().write().active_device_name = None;
                            }
                        }
                        Err(e) => {
                            warn!("Connect WS failed: {e:?}");
                            studio.connect_outbound.set(None);
                            if !studio.connect_devices.peek().is_empty() {
                                studio.connect_devices.set(Vec::new());
                            }
                            if player_ctx.state_signal().peek().active_device_name.is_some() {
                                player_ctx.state_signal().write().active_device_name = None;
                            }
                        }
                    }

                    retry_count = retry_count.saturating_add(1);
                    let delay = (1_500u32
                        .saturating_mul(2u32.saturating_pow(retry_count.min(4))))
                    .min(30_000);
                    gloo_timers::future::TimeoutFuture::new(delay).await;
                }
            }
        });
    }

    rsx! {
        PlayerProvider {
            state: player_state,
            position_secs,
            duration_secs,
            on_command: dispatcher,
            {children}
        }
    }
}

#[cfg(feature = "web")]
fn clear_web_audio(engine: &WebAudioEngine) {
    engine.load_gen.set(engine.load_gen.get().wrapping_add(1));
    *engine.loaded_track_id.borrow_mut() = None;
    engine.pending_seek_secs.set(None);
    engine.scrobble.borrow_mut().reset(String::new());
    let _ = engine.audio.pause();
    engine.audio.set_src("");
}

#[cfg(feature = "web")]
fn start_web_audio_track(
    engine: &WebAudioEngine,
    track: &PlayerTrack,
    nav: Option<&NavidromeSessionInfo>,
    player_state: &mut Signal<PlayerState>,
    autoplay: bool,
) {
    let state_snap = player_state.peek().clone();
    if state_snap.offline_only && !track.is_offline_cached {
        clear_web_audio(engine);
        player_state.write().status = PlaybackStatus::Error(
            "Offline Only mode is active: this track is not cached offline.".to_string(),
        );
        return;
    }
    let Some(info) = nav else {
        clear_web_audio(engine);
        player_state.write().status =
            PlaybackStatus::Error("Connect Navidrome in Settings to stream audio.".to_string());
        return;
    };
    if track.id.is_empty() {
        clear_web_audio(engine);
        player_state.write().status =
            PlaybackStatus::Error("Track is not indexed in Navidrome yet.".to_string());
        return;
    }

    let next_gen = engine.load_gen.get().wrapping_add(1);
    engine.load_gen.set(next_gen);
    *engine.loaded_track_id.borrow_mut() = Some(track.id.clone());
    engine.scrobble.borrow_mut().reset(track.id.clone());

    let url = ui::subsonic_stream_url(info, &track.id);
    engine.audio.set_src(&url);
    engine.audio.set_volume(f64::from(state_snap.volume));
    engine.audio.set_muted(state_snap.muted);

    if !autoplay {
        let _ = engine.audio.pause();
        player_state.write().status = PlaybackStatus::Paused;
        return;
    }

    if let Ok(promise) = engine.audio.play() {
        let mut st = *player_state;
        let gen_ref = Rc::clone(&engine.load_gen);
        spawn(async move {
            if wasm_bindgen_futures::JsFuture::from(promise).await.is_err()
                && gen_ref.get() == next_gen
                && st.peek().status == PlaybackStatus::Loading
            {
                st.write().status = PlaybackStatus::Paused;
            }
        });
    }
}

/// Compares two Connect device slices ignoring volatile keepalive timestamps (`last_seen_unix`
/// and snapshot `updated_at_unix`) so periodic 25s heartbeats do not cause redundant re-renders.
#[cfg(any(feature = "web", test))]
fn connect_devices_visibly_equal(a: &[ConnectDevice], b: &[ConnectDevice]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.device_id == y.device_id
                && x.name == y.name
                && x.platform == y.platform
                && x.is_active == y.is_active
                && snapshots_visibly_equal(x.snapshot.as_ref(), y.snapshot.as_ref())
        })
}

#[cfg(any(feature = "web", test))]
fn snapshots_visibly_equal(a: Option<&PlaybackSnapshot>, b: Option<&PlaybackSnapshot>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.song_id == y.song_id
                && x.title == y.title
                && x.artist == y.artist
                && x.album == y.album
                && x.cover_art_id == y.cover_art_id
                && x.is_playing == y.is_playing
                && x.position_ms == y.position_ms
                && x.duration_ms == y.duration_ms
                && x.queue_song_ids == y.queue_song_ids
                && x.queue_index == y.queue_index
                && x.volume.to_bits() == y.volume.to_bits()
                && x.is_pending_discovery == y.is_pending_discovery
        }
        _ => false,
    }
}

#[cfg(feature = "web")]
fn apply_remote_connect_command(
    mut studio: WebStudioContext,
    player: PlayerContext,
    engine: Option<&WebAudioEngine>,
    command: ConnectCommand,
) {
    match command {
        ConnectCommand::Play => {
            if !player.is_playing() {
                player.toggle_play_pause();
            }
        }
        ConnectCommand::Pause => {
            if player.is_playing() {
                player.toggle_play_pause();
            }
        }
        ConnectCommand::TogglePlay => player.toggle_play_pause(),
        ConnectCommand::Next => player.next(),
        ConnectCommand::Previous => player.previous(),
        ConnectCommand::Seek { position_ms } => player.seek((position_ms as f64) / 1000.0),
        ConnectCommand::SetVolume { volume } => player.set_volume(volume),
        ConnectCommand::TransferPlayback { snapshot, .. } => {
            use futures::stream::{self, StreamExt};
            let target_secs = (snapshot.position_ms as f64) / 1000.0;
            let queue_ids = snapshot.queue_song_ids;
            let requested_index = snapshot.queue_index;
            let should_play = snapshot.is_playing;

            if let Some(song_id) = snapshot.song_id {
                if let Some(eng) = engine {
                    if target_secs > 0.0 {
                        eng.pending_seek_secs.set(Some(target_secs));
                    }
                }
                let nav_opt = studio.navidrome_info();
                let cover = nav_opt
                    .as_ref()
                    .map(|info| subsonic_cover_art_url(info, &song_id, 256));
                let mut pt = PlayerTrack {
                    id: song_id.clone(),
                    title: snapshot
                        .title
                        .unwrap_or_else(|| "Transferred Track".to_string()),
                    artist: snapshot
                        .artist
                        .unwrap_or_else(|| "Unknown Artist".to_string()),
                    album: snapshot.album,
                    cover_art_url: cover,
                    cover_art_id: snapshot.cover_art_id.or_else(|| Some(song_id.clone())),
                    duration_secs: Some((snapshot.duration_ms / 1000) as u32),
                    is_discovery: snapshot.is_pending_discovery,
                    ..Default::default()
                };
                studio.apply_curation_overrides(&mut pt);
                // Respect paused handoff natively via `autoplay: should_play` (never triggers audio.play when false).
                player.play_track_with_autoplay(pt.clone(), should_play);
                if target_secs > 0.0 {
                    player.seek(target_secs);
                }

                // Capture monotonic transfer generation AFTER `play_track_with_autoplay` bumped it so
                // any newer queue command invalidates this async hydration task.
                let my_gen = *studio.transfer_gen.peek();

                // Hydrate the full transferred queue with bounded concurrency without interrupting playback.
                if queue_ids.len() > 1 {
                    if let Some(nav) = nav_opt {
                        let active_song_id = song_id.clone();
                        let active_track = pt;
                        spawn(async move {
                            let mut indexed: Vec<(usize, PlayerTrack)> = stream::iter(
                                queue_ids.into_iter().take(50).enumerate().map(|(pos, sid)| {
                                    let nav = nav.clone();
                                    let active_id = active_song_id.clone();
                                    let active_pt = active_track.clone();
                                    async move {
                                        if sid == active_id {
                                            return Some((pos, active_pt));
                                        }
                                        match crate::navidrome_web::fetch_song(&nav, &sid).await {
                                            Ok(Some(mut t)) => {
                                                studio.apply_curation_overrides(&mut t);
                                                Some((pos, t))
                                            }
                                            _ => None,
                                        }
                                    }
                                }),
                            )
                            .buffer_unordered(6)
                            .collect::<Vec<_>>()
                            .await
                            .into_iter()
                            .flatten()
                            .collect();
                            indexed.sort_by_key(|(pos, _)| *pos);
                            let tracks: Vec<PlayerTrack> =
                                indexed.into_iter().map(|(_, t)| t).collect();

                            if *studio.transfer_gen.peek() != my_gen {
                                return;
                            }
                            if tracks.len() > 1 {
                                let mut state_sig = player.state_signal();
                                let mut state = state_sig.write();
                                if state.current_track.as_ref().map(|t| t.id.as_str())
                                    == Some(active_song_id.as_str())
                                {
                                    let resolved_idx = tracks
                                        .get(requested_index)
                                        .filter(|t| t.id == active_song_id)
                                        .map(|_| requested_index)
                                        .or_else(|| {
                                            tracks.iter().position(|t| t.id == active_song_id)
                                        })
                                        .unwrap_or(0);
                                    state.queue = tracks;
                                    state.queue_index = Some(resolved_idx);
                                }
                            }
                        });
                    }
                }
            } else if !queue_ids.is_empty() {
                if let Some(nav) = studio.navidrome_info() {
                    let my_gen = studio.transfer_gen.peek().wrapping_add(1);
                    studio.transfer_gen.set(my_gen);
                    if let Some(eng) = engine {
                        if target_secs > 0.0 {
                            eng.pending_seek_secs.set(Some(target_secs));
                        }
                    }
                    spawn(async move {
                        let mut indexed: Vec<(usize, PlayerTrack)> = stream::iter(
                            queue_ids.into_iter().take(50).enumerate().map(|(pos, sid)| {
                                let nav = nav.clone();
                                async move {
                                    match crate::navidrome_web::fetch_song(&nav, &sid).await {
                                        Ok(Some(mut t)) => {
                                            studio.apply_curation_overrides(&mut t);
                                            Some((pos, t))
                                        }
                                        _ => None,
                                    }
                                }
                            }),
                        )
                        .buffer_unordered(6)
                        .collect::<Vec<_>>()
                        .await
                        .into_iter()
                        .flatten()
                        .collect();
                        indexed.sort_by_key(|(pos, _)| *pos);
                        let tracks: Vec<PlayerTrack> =
                            indexed.into_iter().map(|(_, t)| t).collect();

                        if tracks.is_empty() || *studio.transfer_gen.peek() != my_gen {
                            return;
                        }
                        let idx = requested_index.min(tracks.len() - 1);
                        player.play_queue_with_autoplay(tracks, idx, should_play);
                        if target_secs > 0.0 {
                            player.seek(target_secs);
                        }
                    });
                }
            }
        }
        ConnectCommand::LoadQueue {
            song_ids,
            start_index,
            start_position_ms,
            autoplay,
        } => {
            use futures::stream::{self, StreamExt};
            if song_ids.is_empty() {
                return;
            }
            let Some(nav) = studio.navidrome_info() else {
                return;
            };
            let my_gen = studio.transfer_gen.peek().wrapping_add(1);
            studio.transfer_gen.set(my_gen);
            let target_secs = (start_position_ms as f64) / 1000.0;
            if let Some(eng) = engine {
                if target_secs > 0.0 {
                    eng.pending_seek_secs.set(Some(target_secs));
                }
            }
            spawn(async move {
                let mut indexed: Vec<(usize, PlayerTrack)> = stream::iter(
                    song_ids.into_iter().take(50).enumerate().map(|(pos, sid)| {
                        let nav = nav.clone();
                        async move {
                            match crate::navidrome_web::fetch_song(&nav, &sid).await {
                                Ok(Some(mut t)) => {
                                    studio.apply_curation_overrides(&mut t);
                                    Some((pos, t))
                                }
                                _ => None,
                            }
                        }
                    }),
                )
                .buffer_unordered(6)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .flatten()
                .collect();
                indexed.sort_by_key(|(pos, _)| *pos);
                let tracks: Vec<PlayerTrack> = indexed.into_iter().map(|(_, t)| t).collect();
                if tracks.is_empty() || *studio.transfer_gen.peek() != my_gen {
                    return;
                }
                player.play_queue_with_autoplay(tracks, start_index, autoplay);
                if target_secs > 0.0 {
                    player.seek(target_secs);
                }
            });
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn js_day_to_iso_weekday(js_day: u32) -> u8 {
    if js_day == 0 {
        7
    } else {
        js_day as u8
    }
}

/// Returns the browser's current local hour (`0..=23`) on WASM, or `None` on non-WASM targets.
pub fn local_browser_hour() -> Option<u8> {
    #[cfg(target_arch = "wasm32")]
    {
        Some(js_sys::Date::new_0().get_hours() as u8)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// Returns the browser's current local ISO weekday (`1` = Monday .. `7` = Sunday) on WASM,
/// or `None` on non-WASM targets.
pub fn local_browser_weekday() -> Option<u8> {
    #[cfg(target_arch = "wasm32")]
    {
        Some(js_day_to_iso_weekday(js_sys::Date::new_0().get_day()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

#[cfg(target_arch = "wasm32")]
const LIVE_WEATHER_LOCATION_KEY: &str = "soulbeet_live_weather_location";

fn load_live_weather_location() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(LIVE_WEATHER_LOCATION_KEY).ok().flatten())
            .is_some_and(|v| v == "1")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Rounds a coordinate to `0.1` degrees (~11 km city-scale accuracy) and normalizes `-0.0` to `0.0`.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
pub fn round_coord_tenth(v: f64) -> f64 {
    let rounded = (v * 10.0).round() / 10.0;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

/// Validates raw browser coordinates (rejecting NaN, Infinity, out-of-range, and Null Island
/// `(0.0, 0.0)`) and rounds them to `0.1` degrees before network transmission.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
pub fn sanitize_browser_coords(lat: f64, lon: f64) -> Option<(f64, f64)> {
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
        || (lat == 0.0 && lon == 0.0)
    {
        return None;
    }
    Some((round_coord_tenth(lat), round_coord_tenth(lon)))
}

/// Result of a coarse browser geolocation request.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub enum BrowserGeoResult {
    Coords(f64, f64),
    PermissionDenied,
    Unavailable,
}

/// Requests coarse (`0.1` degree) browser coordinates via `navigator.geolocation`.
///
/// Closure lifetimes are retained in a self-releasing `Rc<RefCell<...>>` holder until the browser
/// actually invokes either callback (then dropped on the next microtask via `spawn_local`), so a
/// timeout or cancelled future while the user reads the permission prompt NEVER causes a
/// `wasm-bindgen` dropped-closure JS exception.
pub async fn fetch_browser_coarse_coords(interactive_prompt: bool) -> BrowserGeoResult {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        type GeoClosures = (
            Closure<dyn FnMut(web_sys::Position)>,
            Closure<dyn FnMut(web_sys::PositionError)>,
        );

        let Some(window) = web_sys::window() else {
            return BrowserGeoResult::Unavailable;
        };
        let Ok(geolocation) = window.navigator().geolocation() else {
            return BrowserGeoResult::Unavailable;
        };
        let options = web_sys::PositionOptions::new();
        options.set_enable_high_accuracy(false);
        options.set_maximum_age(600_000);
        let geo_timeout_ms: u32 = if interactive_prompt { 10_000 } else { 2_500 };
        options.set_timeout(geo_timeout_ms);

        let (tx, rx) = futures::channel::oneshot::channel::<BrowserGeoResult>();
        let tx_cell = Rc::new(std::cell::RefCell::new(Some(tx)));
        let closures_holder: Rc<std::cell::RefCell<Option<GeoClosures>>> =
            Rc::new(std::cell::RefCell::new(None));

        let tx_ok = Rc::clone(&tx_cell);
        let holder_ok = Rc::clone(&closures_holder);
        let on_success =
            Closure::<dyn FnMut(web_sys::Position)>::new(move |pos: web_sys::Position| {
                let coords = pos.coords();
                let res = match sanitize_browser_coords(coords.latitude(), coords.longitude()) {
                    Some((lat, lon)) => BrowserGeoResult::Coords(lat, lon),
                    None => BrowserGeoResult::Unavailable,
                };
                if let Some(sender) = tx_ok.borrow_mut().take() {
                    let _ = sender.send(res);
                }
                if let Some(taken) = holder_ok.borrow_mut().take() {
                    wasm_bindgen_futures::spawn_local(async move {
                        drop(taken);
                    });
                }
            });

        let tx_err = Rc::clone(&tx_cell);
        let holder_err = Rc::clone(&closures_holder);
        let on_error =
            Closure::<dyn FnMut(web_sys::PositionError)>::new(move |err: web_sys::PositionError| {
                let res = if err.code() == web_sys::PositionError::PERMISSION_DENIED {
                    BrowserGeoResult::PermissionDenied
                } else {
                    BrowserGeoResult::Unavailable
                };
                if let Some(sender) = tx_err.borrow_mut().take() {
                    let _ = sender.send(res);
                }
                if let Some(taken) = holder_err.borrow_mut().take() {
                    wasm_bindgen_futures::spawn_local(async move {
                        drop(taken);
                    });
                }
            });

        let call_res = geolocation.get_current_position_with_error_callback_and_options(
            on_success.as_ref().unchecked_ref(),
            Some(on_error.as_ref().unchecked_ref()),
            &options,
        );
        if call_res.is_err() {
            return BrowserGeoResult::Unavailable;
        }
        // Keep closures alive until the browser invokes either callback.
        *closures_holder.borrow_mut() = Some((on_success, on_error));

        let wait_ms: u32 = if interactive_prompt { 30_000 } else { 2_500 };
        let timeout = gloo_timers::future::TimeoutFuture::new(wait_ms);
        futures::pin_mut!(rx);
        futures::pin_mut!(timeout);
        match futures::future::select(rx, timeout).await {
            futures::future::Either::Left((Ok(res), _)) => res,
            _ => BrowserGeoResult::Unavailable,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = interactive_prompt;
        BrowserGeoResult::Unavailable
    }
}

/// Fetches active contextual mixes with the browser's local clock and optional coarse location
/// when the user has opted into live weather location.
/// Returns `(result, permission_was_denied)` so callers can automatically clear the persisted
/// live-location preference if the user revoked browser geolocation permission.
pub async fn fetch_contextual_mixes_with_context(
    auth: &Auth,
    live_location: bool,
) -> (Result<Vec<ContextualMixSummary>, ServerFnError>, bool) {
    let local_hour = local_browser_hour();
    let local_weekday = local_browser_weekday();
    let (lat, lon, perm_denied) = if live_location {
        match fetch_browser_coarse_coords(false).await {
            BrowserGeoResult::Coords(la, lo) => (Some(la), Some(lo), false),
            BrowserGeoResult::PermissionDenied => (None, None, true),
            BrowserGeoResult::Unavailable => (None, None, false),
        }
    } else {
        (None, None, false)
    };
    let res = auth
        .call(api::list_active_contextual_mixes(
            local_hour,
            local_weekday,
            lat,
            lon,
        ))
        .await;
    (res, perm_denied)
}

fn load_or_create_device_id() -> String {
    #[cfg(feature = "web")]
    {
        // Use per-tab sessionStorage so opening/closing a second browser tab never
        // collides with or unregisters the active tab's Connect device ID.
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            if let Ok(Some(existing)) = storage.get_item("soulbeet.connect.device_id") {
                if !existing.trim().is_empty() {
                    return existing;
                }
            }
            let generated = format!("web-{}", js_sys::Math::random().to_bits());
            let _ = storage.set_item("soulbeet.connect.device_id", &generated);
            return generated;
        }
        // Private browsing or blocked storage: still generate a unique per-tab ID.
        format!("web-{}", js_sys::Math::random().to_bits())
    }
    #[cfg(not(feature = "web"))]
    {
        "web-studio".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrobble_tracker_sends_now_playing_once_and_ignores_seek_jumps() {
        let mut tracker = ScrobbleTracker::default();
        tracker.reset("song-42".into());

        // Now-playing fires exactly once per track play
        assert_eq!(tracker.on_playing().as_deref(), Some("song-42"));
        assert_eq!(tracker.on_playing(), None);

        // Short tracks (< 30s) never scrobble
        for sec in 0..25 {
            assert_eq!(tracker.on_timeupdate(f64::from(sec), 25.0), None);
        }

        // Seeking straight to 95s on a 100s track does NOT count as 95s listened
        tracker.reset("song-99".into());
        assert_eq!(tracker.on_timeupdate(0.0, 100.0), None);
        assert_eq!(tracker.on_timeupdate(95.0, 100.0), None);
        assert_eq!(tracker.on_timeupdate(96.0, 100.0), None);

        // Listening monotonically past 50% (50s of 100s) triggers scrobble exactly once
        tracker.reset("song-100".into());
        let mut triggered = 0;
        for step in 0..=60 {
            if tracker.on_timeupdate(f64::from(step), 100.0).is_some() {
                triggered += 1;
            }
        }
        assert_eq!(triggered, 1);

        // Long tracks (> 480s) cap scrobble threshold at 240s
        tracker.reset("song-long".into());
        let mut scrobble_at = None;
        for step in 0..=300 {
            if tracker.on_timeupdate(f64::from(step), 600.0).is_some() {
                scrobble_at = Some(step);
            }
        }
        assert_eq!(scrobble_at, Some(240));
    }

    #[test]
    fn connect_device_visible_equality_ignores_keepalive_timestamps() {
        let base = ConnectDevice {
            device_id: "d1".into(),
            name: "Web Studio".into(),
            platform: shared::client::DevicePlatform::Web,
            is_active: true,
            last_seen_unix: 100,
            snapshot: Some(PlaybackSnapshot {
                song_id: Some("s1".into()),
                title: Some("Song".into()),
                artist: Some("Artist".into()),
                album: None,
                cover_art_id: None,
                is_playing: true,
                position_ms: 10_000,
                duration_ms: 180_000,
                queue_song_ids: vec!["s1".into()],
                queue_index: 0,
                volume: 1.0,
                is_pending_discovery: false,
                updated_at_unix: 100,
            }),
        };
        let mut heartbeat_update = base.clone();
        heartbeat_update.last_seen_unix = 125;
        if let Some(ref mut snap) = heartbeat_update.snapshot {
            snap.updated_at_unix = 125;
        }
        assert!(connect_devices_visibly_equal(
            std::slice::from_ref(&base),
            &[heartbeat_update]
        ));

        let mut active_changed = base.clone();
        active_changed.is_active = false;
        assert!(!connect_devices_visibly_equal(&[base], &[active_changed]));
    }

    #[test]
    fn js_day_to_iso_weekday_maps_sunday_to_seven_and_mon_to_sat_directly() {
        assert_eq!(js_day_to_iso_weekday(0), 7);
        assert_eq!(js_day_to_iso_weekday(1), 1);
        assert_eq!(js_day_to_iso_weekday(2), 2);
        assert_eq!(js_day_to_iso_weekday(3), 3);
        assert_eq!(js_day_to_iso_weekday(4), 4);
        assert_eq!(js_day_to_iso_weekday(5), 5);
        assert_eq!(js_day_to_iso_weekday(6), 6);
    }

    #[test]
    fn round_coord_tenth_rounds_to_nearest_tenth_degree() {
        assert!((round_coord_tenth(48.8582) - 48.9).abs() < 1e-9);
        assert!((round_coord_tenth(2.3387) - 2.3).abs() < 1e-9);
        assert!((round_coord_tenth(-33.8688) - (-33.9)).abs() < 1e-9);
        assert!((round_coord_tenth(0.04) - 0.0).abs() < 1e-9);
        assert!((round_coord_tenth(45.75) - 45.8).abs() < 1e-9);
        assert!(!round_coord_tenth(-0.04).is_sign_negative());
        assert!(!round_coord_tenth(-0.0).is_sign_negative());
        assert_eq!(sanitize_browser_coords(0.0, 0.0), None);
        assert_eq!(sanitize_browser_coords(f64::NAN, 2.3), None);
        assert_eq!(sanitize_browser_coords(91.0, 2.3), None);
        assert_eq!(sanitize_browser_coords(45.75, 4.84), Some((45.8, 4.8)));
    }
}
