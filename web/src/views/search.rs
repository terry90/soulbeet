//! Unified Single-Box Studio Search: live local library & discovery matches
//! alongside Deezer + MusicBrainz unified catalog search, normalized local-vs-remote
//! deduplication, inline artist discographies, expandable album tracklists, 1-tap
//! `Acquire & Play`, auto-download across all enabled backends, and a parallel
//! multi-backend manual source picker (`Download Options`).

use std::collections::{HashMap, HashSet};

use api::models::folder::Folder;
use dioxus::dioxus_core::Task;
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::backends::BackendMeta;
use shared::download::{AcquireAndPlayRequest, AutoDownloadEvent, DownloadQuery};
use shared::metadata::{
    ArtistCatalog, CatalogAlbum, CatalogAlbumDetail, CatalogArtist, CatalogProvider, CatalogTrack,
    UnifiedCatalogSearch,
};
use shared::recommendation::ContextualMixKind;
use shared::search_norm::{
    album_dedup_key, base_song_title_key, canonical_artist_key, classify_track_variant,
    fold_search_text, track_dedup_key, TrackVariantKind,
};
use ui::{
    format_mix_title, friendly_error, run_multi_backend_source_search,
    submit_multi_backend_download, use_player, use_settings, use_system_health, AcquireStageBadge,
    AutoDownloadSignal, DirectSearchEscape, DownloadIcon, DownloadResults, DownloadRowState,
    FallbackToast, FallbackToastData, FolderChip, IconChevronLeft, IconDisc3, IconLoader,
    IconMusic, IconPlay, IconRefreshCw, IconSearch, IconSlidersHorizontal, IconSparkles,
    IconWifiOff, IconX, PlayerTrack, SearchPrefill, SearchReset, SourceChoices, SystemStatus,
    TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{
    fetch_album_tracks, fetch_artist_albums, search_navidrome, NavidromeSearchHits,
    SubsonicAlbumSummary, SubsonicArtistSummary,
};
use crate::web_player::{acquire_key, use_studio, AcquireTrackState, ManualChoiceModalState};

const LOCAL_DEBOUNCE_MS: u32 = 120;
const REMOTE_DEBOUNCE_MS: u32 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    Tracks,
    Albums,
    Artists,
}

impl SearchFilter {
    fn shows_tracks(self) -> bool {
        matches!(self, Self::All | Self::Tracks)
    }

    fn shows_albums(self) -> bool {
        matches!(self, Self::All | Self::Albums)
    }

    fn shows_artists(self) -> bool {
        matches!(self, Self::All | Self::Artists)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ActiveDetail {
    CatalogArtist {
        loading: bool,
        artist_name: String,
        catalog: Option<ArtistCatalog>,
        error: Option<String>,
    },
    CatalogAlbum {
        album_id: String,
        title: String,
        artist: String,
        loading: bool,
        detail: Option<CatalogAlbumDetail>,
        error: Option<String>,
        return_to_artist: Option<(String, String)>,
    },
    LocalArtist {
        artist: SubsonicArtistSummary,
        loading: bool,
        albums: Vec<SubsonicAlbumSummary>,
        error: Option<String>,
    },
}

/// Normalized index of locally owned tracks (Navidrome hits + visible triage tracks)
/// used to hide duplicate studio recordings from the remote catalog section while
/// keeping explicit non-studio variants (Live, Remix, Acoustic, Demo, etc.).
#[derive(Debug, Default)]
struct LocalTrackIndex {
    exact_keys: HashSet<(String, String)>,
    studio_base_keys: HashSet<(String, String)>,
}

impl LocalTrackIndex {
    fn insert(&mut self, artist: &str, title: &str, album: Option<&str>) {
        self.exact_keys.insert(track_dedup_key(artist, title));
        if classify_track_variant(artist, title, album, None, false) == TrackVariantKind::Studio {
            self.studio_base_keys
                .insert((canonical_artist_key(artist), base_song_title_key(title)));
        }
    }

    fn matches_remote(&self, track: &CatalogTrack) -> bool {
        let exact = track_dedup_key(&track.artist, &track.title);
        if self.exact_keys.contains(&exact) {
            return true;
        }
        let is_studio = track.variant_label.is_none()
            && classify_track_variant(
                &track.artist,
                &track.title,
                track.album_title.as_deref(),
                None,
                false,
            ) == TrackVariantKind::Studio;
        if is_studio {
            let base = (
                canonical_artist_key(&track.artist),
                base_song_title_key(&track.title),
            );
            if self.studio_base_keys.contains(&base) {
                return true;
            }
        }
        false
    }
}

fn build_local_album_index(albums: &[SubsonicAlbumSummary]) -> HashSet<(String, String)> {
    albums
        .iter()
        .map(|a| album_dedup_key(&a.artist, &a.name))
        .collect()
}

fn is_remote_album_owned(
    album: &CatalogAlbum,
    local_album_keys: &HashSet<(String, String)>,
) -> bool {
    local_album_keys.contains(&album_dedup_key(&album.artist, &album.title))
}

fn matches_triage_query(query: &str, artist: &str, title: &str, album: &str) -> bool {
    let folded_query = fold_search_text(query);
    let tokens: Vec<&str> = folded_query.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let haystack = format!(
        "{} {} {}",
        fold_search_text(artist),
        fold_search_text(title),
        fold_search_text(album)
    );
    tokens.iter().all(|tok| haystack.contains(tok))
}

fn combine_prefill_query(artist: &str, query: &str) -> String {
    let a = artist.trim();
    let q = query.trim();
    match (a.is_empty(), q.is_empty()) {
        (true, true) => String::new(),
        (false, true) => a.to_string(),
        (true, false) => q.to_string(),
        (false, false) => format!("{a} {q}"),
    }
}

fn artist_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| !w.is_empty()).collect();
    match words.as_slice() {
        [] => "?".to_string(),
        [single] => single
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string()),
        [first, second, ..] => {
            let a = first
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            let b = second
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            format!("{a}{b}")
        }
    }
}

fn format_fan_count(fans: u64) -> String {
    if fans >= 1_000_000 {
        format!("{:.1}M fans", fans as f64 / 1_000_000.0)
    } else if fans >= 1_000 {
        format!("{:.1}K fans", fans as f64 / 1_000.0)
    } else if fans == 1 {
        "1 fan".to_string()
    } else {
        format!("{fans} fans")
    }
}

fn format_catalog_duration(duration: Option<&str>, duration_secs: Option<u32>) -> Option<String> {
    if let Some(d) = duration.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(d.to_string());
    }
    duration_secs
        .filter(|&s| s > 0)
        .map(|s| format!("{}:{:02}", s / 60, s % 60))
}

fn catalog_album_facts(album: &CatalogAlbum) -> Option<String> {
    let year: Option<String> = album
        .first_release_date
        .as_deref()
        .or(album.release_date.as_deref())
        .map(|d| d.trim().chars().take(4).collect::<String>())
        .filter(|y| y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()));
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y);
    }
    if let Some(types) = &album.release_types {
        for ty in types {
            let trimmed = ty.trim();
            if !trimmed.is_empty() && !parts.iter().any(|p| p.eq_ignore_ascii_case(trimmed)) {
                parts.push(trimmed.to_string());
            }
        }
    }
    if let Some(count) = album.track_count.filter(|&c| c > 0) {
        parts.push(if count == 1 {
            "1 track".to_string()
        } else {
            format!("{count} tracks")
        });
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn parse_duration_secs(raw: Option<&str>) -> Option<u32> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<u32>() {
        return Some(secs);
    }
    let parts: Vec<&str> = trimmed.split(':').collect();
    match parts.as_slice() {
        [m, s] => Some(m.parse::<u32>().ok()? * 60 + s.parse::<u32>().ok()?),
        [h, m, s] => Some(
            h.parse::<u32>().ok()? * 3600 + m.parse::<u32>().ok()? * 60 + s.parse::<u32>().ok()?,
        ),
        _ => None,
    }
}

async fn sleep_ms(ms: u32) {
    #[cfg(feature = "web")]
    {
        gloo_timers::future::TimeoutFuture::new(ms).await;
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = ms;
    }
}

#[component]
pub fn SearchPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let mut settings = use_settings();
    let system_health = use_system_health();
    let search_reset = try_use_context::<SearchReset>();
    let search_prefill = try_use_context::<SearchPrefill>();
    let auto_dl = try_use_context::<AutoDownloadSignal>();

    let mut search = use_signal(String::new);
    let mut filter_tab = use_signal(|| SearchFilter::All);
    let mut has_searched = use_signal(|| false);

    // Independent request generations and debounce handles for local vs remote live search
    let mut local_req_id = use_signal(|| 0u64);
    let mut remote_req_id = use_signal(|| 0u64);
    let mut detail_req_id = use_signal(|| 0u64);
    let mut local_debounce = use_signal(|| None::<Task>);
    let mut remote_debounce = use_signal(|| None::<Task>);

    let mut local_loading = use_signal(|| false);
    let mut remote_loading = use_signal(|| false);
    let mut search_error = use_signal(String::new);

    let mut local_hits = use_signal(NavidromeSearchHits::default);
    let mut remote_results = use_signal(|| None::<UnifiedCatalogSearch>);
    let mut active_detail = use_signal(|| None::<ActiveDetail>);

    let mut expanded_albums = use_signal::<HashSet<String>>(HashSet::new);
    let mut album_cache = use_signal::<HashMap<String, CatalogAlbumDetail>>(HashMap::new);

    // Enabled download backends, folders, auto-download tracking & multi-backend source picker
    let mut enabled_backends = use_signal::<Vec<BackendMeta>>(Vec::new);
    let mut folders = use_signal::<Vec<Folder>>(Vec::new);
    let mut selected_folder_id = use_signal::<Option<String>>(|| None);
    let mut active_menu = use_signal::<Option<String>>(|| None);
    let mut download_states = use_signal::<HashMap<String, DownloadRowState>>(HashMap::new);
    let mut batch_to_item = use_signal::<HashMap<String, String>>(HashMap::new);
    let mut batch_to_name = use_signal::<HashMap<String, String>>(HashMap::new);
    let mut pending_events = use_signal::<Vec<AutoDownloadEvent>>(Vec::new);
    let mut fallback_toasts = use_signal::<Vec<FallbackToastData>>(Vec::new);

    let mut download_options = use_signal::<Option<SourceChoices>>(|| None);
    let mut source_search_gen = use_signal(|| 0u64);
    let mut source_searching = use_signal(|| false);
    let mut is_downloading = use_signal(|| false);
    let mut download_error = use_signal(String::new);

    // Sync default download folder from settings
    use_effect(move || {
        if settings.is_loaded() {
            if let Some(us) = settings.get() {
                if us.default_download_folder_id.is_some() {
                    selected_folder_id.set(us.default_download_folder_id.clone());
                }
            }
        }
    });

    // Fetch user folders
    use_future(move || async move {
        if let Ok(user_folders) = auth.call(api::get_user_folders()).await {
            if user_folders.len() == 1 {
                selected_folder_id.set(Some(user_folders[0].id.clone()));
            }
            folders.set(user_folders);
        }
    });

    // Fetch enabled download backends dynamically
    use_future(move || async move {
        if let Ok(backends) = auth.call(api::get_enabled_download_backends()).await {
            enabled_backends.set(backends);
        }
    });

    let mut cancel_debounces = move || {
        if let Some(task) = local_debounce.write().take() {
            task.cancel();
            local_loading.set(false);
        }
        if let Some(task) = remote_debounce.write().take() {
            task.cancel();
            remote_loading.set(false);
        }
    };

    let mut clear_all_state = move || {
        cancel_debounces();
        let next_local = local_req_id.peek().wrapping_add(1);
        local_req_id.set(next_local);
        let next_remote = remote_req_id.peek().wrapping_add(1);
        remote_req_id.set(next_remote);
        let next_detail = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_detail);
        let next_src = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_src);

        search.set(String::new());
        filter_tab.set(SearchFilter::All);
        has_searched.set(false);
        local_loading.set(false);
        remote_loading.set(false);
        local_hits.set(NavidromeSearchHits::default());
        remote_results.set(None);
        active_detail.set(None);
        search_error.set(String::new());
        download_error.set(String::new());
        download_options.set(None);
        source_searching.set(false);
        expanded_albums.write().clear();
        album_cache.write().clear();
    };

    // Honor SearchReset context
    use_effect(move || {
        if let Some(reset) = search_reset {
            if reset.0() > 0 {
                clear_all_state();
            }
        }
    });

    // Keep an album row and all of its cached track rows in lockstep across auto-download transitions
    let mut set_album_and_tracks_download_state =
        move |item_id: &str, state: DownloadRowState| {
            let cached_track_ids: Vec<String> = album_cache
                .peek()
                .get(item_id)
                .map(|c| c.tracks.iter().map(|t| t.id.clone()).collect())
                .unwrap_or_default();
            let mut states = download_states.write();
            states.insert(item_id.to_string(), state.clone());
            for tid in cached_track_ids {
                states.insert(tid, state.clone());
            }
        };

    // Helper to apply an AutoDownloadEvent once its batch_id is known
    let mut apply_auto_dl_event = move |event: AutoDownloadEvent, item_id: String| match event {
        AutoDownloadEvent::Searching { .. } | AutoDownloadEvent::ScoringResults { .. } => {
            set_album_and_tracks_download_state(&item_id, DownloadRowState::Searching);
        }
        AutoDownloadEvent::PickedSource { .. } | AutoDownloadEvent::Downloading { .. } => {
            let item_id_timer = item_id.clone();
            set_album_and_tracks_download_state(&item_id, DownloadRowState::Done);
            spawn(async move {
                sleep_ms(5000).await;
                let cached_track_ids: Vec<String> = album_cache
                    .peek()
                    .get(&item_id_timer)
                    .map(|c| c.tracks.iter().map(|t| t.id.clone()).collect())
                    .unwrap_or_default();
                let mut states = download_states.write();
                if matches!(states.get(&item_id_timer), Some(DownloadRowState::Done)) {
                    states.insert(item_id_timer, DownloadRowState::Idle);
                }
                for tid in cached_track_ids {
                    if matches!(states.get(&tid), Some(DownloadRowState::Done)) {
                        states.insert(tid, DownloadRowState::Idle);
                    }
                }
            });
        }
        AutoDownloadEvent::FallbackToManual {
            batch_id,
            search_id,
            results,
            reason,
            ..
        } => {
            set_album_and_tracks_download_state(&item_id, DownloadRowState::Idle);
            let track_name = batch_to_name
                .peek()
                .get(&batch_id)
                .cloned()
                .unwrap_or(item_id);
            let toast_id = batch_id.clone();
            fallback_toasts.write().push(FallbackToastData {
                id: batch_id,
                track_name,
                search_id,
                results,
                reason,
            });
            spawn(async move {
                sleep_ms(8000).await;
                fallback_toasts.write().retain(|t| t.id != toast_id);
            });
        }
        AutoDownloadEvent::Failed { failure, .. } => {
            set_album_and_tracks_download_state(
                &item_id,
                DownloadRowState::Failed(failure.message),
            );
        }
    };

    // Route AutoDownloadEvents from WebSocket to per-row download states
    use_effect(move || {
        let Some(mut ctx) = auto_dl else { return };
        let Some(event) = (ctx.0)() else { return };
        (ctx.0).set(None);

        let mut events_to_process = pending_events.peek().clone();
        events_to_process.push(event);
        let mut still_pending = Vec::new();

        for event in events_to_process {
            let batch_id = auto_dl_batch_id(&event).to_string();
            let item_id = batch_to_item.peek().get(&batch_id).cloned();
            let Some(item_id) = item_id else {
                still_pending.push(event);
                continue;
            };
            apply_auto_dl_event(event, item_id);
        }
        pending_events.set(still_pending);
    });

    // Drain buffered AutoDownloadEvents once batch_to_item registers the batch
    use_effect(move || {
        let _trigger = batch_to_item.read().len();
        if pending_events.peek().is_empty() {
            return;
        }
        let events = pending_events.peek().clone();
        let mut still_pending = Vec::new();
        for event in events {
            let batch_id = auto_dl_batch_id(&event).to_string();
            let item_id = batch_to_item.peek().get(&batch_id).cloned();
            let Some(item_id) = item_id else {
                still_pending.push(event);
                continue;
            };
            apply_auto_dl_event(event, item_id);
        }
        pending_events.set(still_pending);
    });

    let mut run_local_search = move |query: String| {
        let trimmed = query.trim().to_string();
        let next_req = local_req_id.peek().wrapping_add(1);
        local_req_id.set(next_req);

        if trimmed.is_empty() || player.offline_only() {
            local_hits.set(NavidromeSearchHits::default());
            local_loading.set(false);
            return;
        }

        let Some(info) = studio.navidrome_info() else {
            local_hits.set(NavidromeSearchHits::default());
            local_loading.set(false);
            return;
        };

        local_loading.set(true);
        spawn(async move {
            match search_navidrome(&info, &trimmed).await {
                Ok(hits) => {
                    if *local_req_id.peek() == next_req {
                        local_hits.set(hits);
                        local_loading.set(false);
                    }
                }
                Err(_) => {
                    if *local_req_id.peek() == next_req {
                        local_hits.set(NavidromeSearchHits::default());
                        local_loading.set(false);
                    }
                }
            }
        });
    };

    let mut run_remote_search = move |query: String, provider: Option<CatalogProvider>| {
        let trimmed = query.trim().to_string();
        let next_req = remote_req_id.peek().wrapping_add(1);
        remote_req_id.set(next_req);

        if trimmed.is_empty() || player.offline_only() {
            remote_results.set(None);
            remote_loading.set(false);
            return;
        }

        remote_results.set(None);
        search_error.set(String::new());
        remote_loading.set(true);
        spawn(async move {
            let res = auth
                .call(api::search_unified(api::UnifiedSearchQuery {
                    query: trimmed,
                    provider,
                    limit: None,
                }))
                .await;

            if *remote_req_id.peek() == next_req {
                match res {
                    Ok(data) => {
                        remote_results.set(Some(data));
                        search_error.set(String::new());
                    }
                    Err(e) => {
                        remote_results.set(None);
                        search_error.set(friendly_error(&e));
                    }
                }
                remote_loading.set(false);
            }
        });
    };

    // React cleanly when Offline Only mode is toggled on or off
    let mut prev_offline_only = use_signal(|| player.offline_only());
    use_effect(move || {
        let now_offline = player.offline_only();
        if now_offline == *prev_offline_only.peek() {
            return;
        }
        prev_offline_only.set(now_offline);

        if now_offline {
            if let Some(task) = remote_debounce.write().take() {
                task.cancel();
            }
            if let Some(task) = local_debounce.write().take() {
                task.cancel();
            }
            let next_remote = remote_req_id.peek().wrapping_add(1);
            remote_req_id.set(next_remote);
            let next_local = local_req_id.peek().wrapping_add(1);
            local_req_id.set(next_local);
            remote_loading.set(false);
            local_loading.set(false);
            remote_results.set(None);
            local_hits.set(NavidromeSearchHits::default());
            search_error.set(String::new());
            if matches!(
                active_detail.peek().as_ref(),
                Some(ActiveDetail::CatalogArtist { .. } | ActiveDetail::CatalogAlbum { .. })
            ) {
                let next_detail = detail_req_id.peek().wrapping_add(1);
                detail_req_id.set(next_detail);
                active_detail.set(None);
            }
        } else {
            let trimmed = search.peek().trim().to_string();
            if !trimmed.is_empty() {
                has_searched.set(true);
                run_local_search(trimmed.clone());
                run_remote_search(trimmed, None);
            }
        }
    });

    let mut trigger_immediate_search = move |raw_query: String| {
        cancel_debounces();
        let trimmed = raw_query.trim().to_string();
        let next_src = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_src);
        download_options.set(None);
        source_searching.set(false);
        download_error.set(String::new());
        active_detail.set(None);

        if trimmed.is_empty() {
            let next_local = local_req_id.peek().wrapping_add(1);
            local_req_id.set(next_local);
            let next_remote = remote_req_id.peek().wrapping_add(1);
            remote_req_id.set(next_remote);
            has_searched.set(false);
            local_hits.set(NavidromeSearchHits::default());
            remote_results.set(None);
            local_loading.set(false);
            remote_loading.set(false);
            search_error.set(String::new());
            return;
        }

        has_searched.set(true);
        run_local_search(trimmed.clone());
        run_remote_search(trimmed, None);
    };

    let mut schedule_live_search = move |raw_query: String| {
        cancel_debounces();
        search.set(raw_query.clone());
        active_detail.set(None);
        download_error.set(String::new());

        let trimmed = raw_query.trim().to_string();
        if trimmed.is_empty() {
            let next_local = local_req_id.peek().wrapping_add(1);
            local_req_id.set(next_local);
            let next_remote = remote_req_id.peek().wrapping_add(1);
            remote_req_id.set(next_remote);
            has_searched.set(false);
            local_hits.set(NavidromeSearchHits::default());
            remote_results.set(None);
            local_loading.set(false);
            remote_loading.set(false);
            search_error.set(String::new());
            return;
        }

        has_searched.set(true);
        if remote_results
            .peek()
            .as_ref()
            .is_some_and(|r| r.query != trimmed)
        {
            remote_results.set(None);
        }

        // Local Navidrome search debounced quickly (120ms)
        let local_q = trimmed.clone();
        let next_local = local_req_id.peek().wrapping_add(1);
        local_req_id.set(next_local);
        if !player.offline_only() && studio.navidrome_info().is_some() {
            let task = spawn(async move {
                sleep_ms(LOCAL_DEBOUNCE_MS).await;
                if *local_req_id.peek() != next_local {
                    return;
                }
                let Some(info) = studio.navidrome_info() else {
                    local_hits.set(NavidromeSearchHits::default());
                    local_loading.set(false);
                    return;
                };
                local_loading.set(true);
                match search_navidrome(&info, &local_q).await {
                    Ok(hits) => {
                        if *local_req_id.peek() == next_local {
                            local_hits.set(hits);
                            local_loading.set(false);
                        }
                    }
                    Err(_) => {
                        if *local_req_id.peek() == next_local {
                            local_hits.set(NavidromeSearchHits::default());
                            local_loading.set(false);
                        }
                    }
                }
            });
            local_debounce.set(Some(task));
        } else {
            local_hits.set(NavidromeSearchHits::default());
            local_loading.set(false);
        }

        // Remote catalog search debounced at 250ms with generation cancellation
        let remote_q = trimmed;
        let next_remote = remote_req_id.peek().wrapping_add(1);
        remote_req_id.set(next_remote);
        if !player.offline_only() {
            let task = spawn(async move {
                sleep_ms(REMOTE_DEBOUNCE_MS).await;
                if *remote_req_id.peek() != next_remote {
                    return;
                }
                search_error.set(String::new());
                remote_loading.set(true);
                let res = auth
                    .call(api::search_unified(api::UnifiedSearchQuery {
                        query: remote_q,
                        provider: None,
                        limit: None,
                    }))
                    .await;
                if *remote_req_id.peek() == next_remote {
                    match res {
                        Ok(data) => {
                            remote_results.set(Some(data));
                            search_error.set(String::new());
                        }
                        Err(e) => {
                            remote_results.set(None);
                            search_error.set(friendly_error(&e));
                        }
                    }
                    remote_loading.set(false);
                }
            });
            remote_debounce.set(Some(task));
        } else {
            remote_results.set(None);
            remote_loading.set(false);
        }
    };

    // Honor Command Palette "Search for <query>" jumps automatically
    use_effect(move || {
        if let Some(incoming) = (studio.search_query_override)() {
            studio.search_query_override.set(None);
            filter_tab.set(SearchFilter::All);
            search.set(incoming.clone());
            trigger_immediate_search(incoming);
        }
    });

    // Honor SearchPrefill context
    use_effect(move || {
        if let Some(mut prefill) = search_prefill {
            if let Some((prefill_artist, prefill_query)) = (prefill.0)() {
                (prefill.0).set(None);
                let combined = combine_prefill_query(&prefill_artist, &prefill_query);
                filter_tab.set(SearchFilter::All);
                search.set(combined.clone());
                trigger_immediate_search(combined);
            }
        }
    });

    let mut open_catalog_artist = move |artist_id: String, artist_name: String| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: catalog lookup is unavailable.");
            return;
        }
        let next_detail = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_detail);
        let display_name = if artist_name.trim().is_empty() {
            "Artist".to_string()
        } else {
            artist_name.clone()
        };
        active_detail.set(Some(ActiveDetail::CatalogArtist {
            loading: true,
            artist_name: display_name.clone(),
            catalog: None,
            error: None,
        }));
        spawn(async move {
            let name_opt = (!artist_name.trim().is_empty()).then_some(artist_name);
            let res = auth
                .call(api::find_catalog_artist(api::ArtistCatalogQuery {
                    id: artist_id,
                    name: name_opt,
                }))
                .await;
            if *detail_req_id.peek() != next_detail {
                return;
            }
            match res {
                Ok(Some(catalog)) => {
                    active_detail.set(Some(ActiveDetail::CatalogArtist {
                        loading: false,
                        artist_name: catalog.artist.name.clone(),
                        catalog: Some(catalog),
                        error: None,
                    }));
                }
                Ok(None) => {
                    active_detail.set(Some(ActiveDetail::CatalogArtist {
                        loading: false,
                        artist_name: display_name,
                        catalog: None,
                        error: Some("No artist discography found in catalog.".to_string()),
                    }));
                }
                Err(e) => {
                    active_detail.set(Some(ActiveDetail::CatalogArtist {
                        loading: false,
                        artist_name: display_name,
                        catalog: None,
                        error: Some(friendly_error(&e)),
                    }));
                }
            }
        });
    };

    let mut open_catalog_album = move |album_id: String,
                                       title: String,
                                       artist: String,
                                       return_to_artist: Option<(String, String)>| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: album lookup is unavailable.");
            return;
        }
        let next_detail = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_detail);
        let display_title = if title.trim().is_empty() {
            "Album".to_string()
        } else {
            title.clone()
        };

        if let Some(cached) = album_cache.peek().get(&album_id).cloned() {
            active_detail.set(Some(ActiveDetail::CatalogAlbum {
                album_id,
                title: cached.album.title.clone(),
                artist: cached.album.artist.clone(),
                loading: false,
                detail: Some(cached),
                error: None,
                return_to_artist,
            }));
            return;
        }

        active_detail.set(Some(ActiveDetail::CatalogAlbum {
            album_id: album_id.clone(),
            title: display_title.clone(),
            artist: artist.clone(),
            loading: true,
            detail: None,
            error: None,
            return_to_artist: return_to_artist.clone(),
        }));

        spawn(async move {
            let res = auth
                .call(api::find_catalog_album(api::AlbumQuery {
                    id: album_id.clone(),
                }))
                .await;
            if *detail_req_id.peek() != next_detail {
                return;
            }
            match res {
                Ok(detail) => {
                    album_cache.write().insert(album_id.clone(), detail.clone());
                    active_detail.set(Some(ActiveDetail::CatalogAlbum {
                        album_id,
                        title: detail.album.title.clone(),
                        artist: detail.album.artist.clone(),
                        loading: false,
                        detail: Some(detail),
                        error: None,
                        return_to_artist,
                    }));
                }
                Err(e) => {
                    active_detail.set(Some(ActiveDetail::CatalogAlbum {
                        album_id,
                        title: display_title,
                        artist,
                        loading: false,
                        detail: None,
                        error: Some(friendly_error(&e)),
                        return_to_artist,
                    }));
                }
            }
        });
    };

    let mut open_local_artist = move |artist: SubsonicArtistSummary| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: library artist lookup is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        let next_detail = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_detail);
        let artist_id = artist.id.clone();
        active_detail.set(Some(ActiveDetail::LocalArtist {
            artist: artist.clone(),
            loading: true,
            albums: Vec::new(),
            error: None,
        }));
        spawn(async move {
            let res = fetch_artist_albums(&info, &artist_id).await;
            if *detail_req_id.peek() != next_detail {
                return;
            }
            match res {
                Ok(albums) => {
                    active_detail.set(Some(ActiveDetail::LocalArtist {
                        artist,
                        loading: false,
                        albums,
                        error: None,
                    }));
                }
                Err(e) => {
                    active_detail.set(Some(ActiveDetail::LocalArtist {
                        artist,
                        loading: false,
                        albums: Vec::new(),
                        error: Some(e),
                    }));
                }
            }
        });
    };

    let mut start_auto_download = move |item_id: String, query: DownloadQuery, folder: Folder| {
        set_album_and_tracks_download_state(&item_id, DownloadRowState::Searching);

        let display_name = query
            .album
            .as_ref()
            .map(|a| a.title.clone())
            .or_else(|| {
                query
                    .tracks
                    .first()
                    .map(|t| format!("{} - {}", t.artist, t.title))
            })
            .unwrap_or_else(|| "Unknown".to_string());

        spawn(async move {
            let result = auth
                .call(api::auto_download(api::AutoDownloadRequest {
                    query,
                    folder_id: folder.id.clone(),
                    folder_path: folder.path.clone(),
                }))
                .await;

            match result {
                Ok(api::AutoDownloadResult::Accepted { batch_id }) => {
                    batch_to_item.write().insert(batch_id.clone(), item_id);
                    batch_to_name.write().insert(batch_id, display_name);
                }
                Ok(api::AutoDownloadResult::Error(failure)) => {
                    set_album_and_tracks_download_state(
                        &item_id,
                        DownloadRowState::Failed(failure.message),
                    );
                }
                Err(e) => {
                    set_album_and_tracks_download_state(
                        &item_id,
                        DownloadRowState::Failed(friendly_error(&e)),
                    );
                }
            }
        });
    };

    let mut handle_auto_download = move |item_id: String, query: DownloadQuery| {
        let folder_id = selected_folder_id();
        let folder = folder_id
            .as_ref()
            .and_then(|fid| folders.read().iter().find(|f| f.id == *fid).cloned())
            .or_else(|| folders.read().first().cloned());
        let Some(folder) = folder else {
            return;
        };
        start_auto_download(item_id, query, folder);
    };

    let mut handle_override_download =
        move |item_id: String, query: DownloadQuery, folder: Folder| {
            start_auto_download(item_id, query, folder);
        };

    let handle_folder_change = move |folder: Folder| {
        selected_folder_id.set(Some(folder.id.clone()));
        spawn(async move {
            let _ = settings
                .update(api::UpdateUserSettings {
                    default_download_folder_id: Some(folder.id),
                    ..Default::default()
                })
                .await;
        });
    };

    let mut open_source_picker = move |query: DownloadQuery| {
        let next_gen = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_gen);
        let backends = enabled_backends.peek().clone();
        spawn(run_multi_backend_source_search(
            auth,
            query,
            backends,
            download_options,
            source_searching,
            download_error,
            source_search_gen,
            next_gen,
        ));
    };

    let mut toggle_expand_album = move |album_id: String| {
        let is_expanded = expanded_albums.read().contains(&album_id);
        if is_expanded {
            expanded_albums.write().remove(&album_id);
        } else {
            expanded_albums.write().insert(album_id.clone());
            let needs_fetch = !album_cache.read().contains_key(&album_id);
            if needs_fetch {
                let aid = album_id.clone();
                spawn(async move {
                    match auth
                        .call(api::find_catalog_album(api::AlbumQuery { id: aid.clone() }))
                        .await
                    {
                        Ok(detail) => {
                            album_cache.write().insert(aid, detail);
                        }
                        Err(e) => {
                            info!("Failed to fetch catalog album details for {}: {:?}", aid, e);
                            expanded_albums.write().remove(&aid);
                        }
                    }
                });
            }
        }
    };

    let mut play_local_album = move |album_id: String| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: album streaming is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            if let Ok(mut tracks) = fetch_album_tracks(&info, &album_id).await {
                for t in &mut tracks {
                    studio.apply_curation_overrides(t);
                }
                if !tracks.is_empty() {
                    player.play_queue(tracks, 0);
                }
            }
        });
    };

    let offline_only = player.offline_only();
    let active_filter = filter_tab();
    let current_query = search();
    let trimmed_query = current_query.trim().to_string();

    // Visible triage tracks (all for dedup index, filtered with normalized multi-word matching)
    let all_visible_triage = studio.visible_triage_tracks();
    let matching_triage: Vec<PlayerTrack> = if trimmed_query.is_empty() {
        Vec::new()
    } else {
        all_visible_triage
            .iter()
            .filter(|t| matches_triage_query(&trimmed_query, &t.artist, &t.title, &t.album))
            .map(|t| studio.track_from_discovery(t))
            .collect()
    };

    // Prepare local Navidrome hits with curation overrides applied and triage IDs deduplicated
    let mut local_snapshot = local_hits.read().clone();
    let triage_song_ids: HashSet<String> = matching_triage
        .iter()
        .filter(|t| !t.id.is_empty())
        .map(|t| t.id.clone())
        .collect();
    local_snapshot
        .songs
        .retain(|s| s.id.is_empty() || !triage_song_ids.contains(&s.id));
    for s in &mut local_snapshot.songs {
        studio.apply_curation_overrides(s);
    }

    // Build normalized local indexes for remote-vs-local deduplication
    let mut local_track_index = LocalTrackIndex::default();
    for s in &local_snapshot.songs {
        local_track_index.insert(&s.artist, &s.title, s.album.as_deref());
    }
    for dt in &all_visible_triage {
        let alb = (!dt.album.trim().is_empty()).then_some(dt.album.as_str());
        local_track_index.insert(&dt.artist, &dt.title, alb);
    }
    let local_album_index = build_local_album_index(&local_snapshot.albums);

    // Deduplicated remote results
    let remote_snapshot = remote_results.read().clone();
    let (deduped_remote_tracks, deduped_remote_albums, remote_artists) = match &remote_snapshot {
        Some(res) => {
            let tracks: Vec<CatalogTrack> = res
                .tracks
                .iter()
                .filter(|t| !local_track_index.matches_remote(t))
                .cloned()
                .collect();
            let albums: Vec<CatalogAlbum> = res
                .albums
                .iter()
                .filter(|a| !is_remote_album_owned(a, &local_album_index))
                .cloned()
                .collect();
            (tracks, albums, res.artists.clone())
        }
        None => (Vec::new(), Vec::new(), Vec::new()),
    };

    let local_tracks_count = matching_triage.len() + local_snapshot.songs.len();
    let total_tracks_count = local_tracks_count + deduped_remote_tracks.len();
    let total_albums_count = local_snapshot.albums.len() + deduped_remote_albums.len();
    let total_artists_count = local_snapshot.artists.len() + remote_artists.len();
    let total_all_count = total_tracks_count + total_albums_count + total_artists_count;

    let acquire_map = studio.acquire_states.read().clone();
    let current_folders = folders.read().clone();
    let current_folder_id = selected_folder_id();
    let has_folders = !current_folders.is_empty();
    let any_loading = local_loading() || remote_loading();

    let mixes: Vec<_> = studio
        .contextual_mixes
        .read()
        .iter()
        .filter(|m| !(m.kind == ContextualMixKind::Seasonal && m.track_count == 0))
        .cloned()
        .collect();
    let starred: Vec<_> = studio
        .starred_tracks
        .read()
        .iter()
        .cloned()
        .map(|mut t| {
            studio.apply_curation_overrides(&mut t);
            t
        })
        .collect();
    let active_backends = enabled_backends.read().clone();
    let backend_summary: String = active_backends
        .iter()
        .map(|b| b.name)
        .collect::<Vec<_>>()
        .join(" · ");

    let detail_snapshot = active_detail.read().clone();

    rsx! {
        div {
            class: "space-y-6",
            onclick: move |_| {
                if active_menu().is_some() {
                    active_menu.set(None);
                }
            },

            if let Some(choices) = download_options.read().clone() {
                if !download_error().is_empty() {
                    p { class: "text-[13px] text-beet-crimson-text bg-beet-crimson-surface border border-beet-crimson-text/30 rounded-[10px] px-4 py-2.5",
                        "{download_error}"
                    }
                }
                DownloadResults {
                    results: choices.groups.clone(),
                    backend_statuses: choices.backend_statuses.clone(),
                    unanswered_note: choices.unanswered_note.clone(),
                    is_searching: source_searching(),
                    is_downloading,
                    on_download: move |(items, folder)| {
                        let choices_c = choices.clone();
                        spawn(async move {
                            download_error.set(String::new());
                            if let Err(err) = submit_multi_backend_download(auth, &choices_c, items, folder).await {
                                download_error.set(err);
                            }
                            is_downloading.set(false);
                        });
                    },
                    on_back: move |_| {
                        let next_gen = source_search_gen.peek().wrapping_add(1);
                        source_search_gen.set(next_gen);
                        download_options.set(None);
                        download_error.set(String::new());
                        source_searching.set(false);
                    },
                }
            } else {
                // Single-Box Live Search Bar + Filter Pills + System Status
                div { class: "space-y-3",
                    form {
                        onsubmit: move |evt| {
                            evt.prevent_default();
                            trigger_immediate_search(search.peek().clone());
                        },
                        div {
                            class: "relative flex items-center gap-2 rounded-[14px] bg-beet-surface border border-beet-border-default focus-within:border-beet-accent/60 focus-within:bg-beet-surface-high px-3.5 min-h-[50px] transition-colors",

                            // Leading search / spinner icon
                            div { class: "text-beet-text-tertiary shrink-0 flex items-center",
                                if any_loading {
                                    IconLoader { size: 18, class: "animate-spin text-beet-leaf-text".to_string() }
                                } else {
                                    IconSearch { size: 18 }
                                }
                            }

                            // Single unified search input
                            input {
                                id: "search-query",
                                r#type: "search",
                                value: "{current_query}",
                                placeholder: if offline_only {
                                    "Search cached library tracks..."
                                } else {
                                    "Search tracks, albums, or artists..."
                                },
                                aria_label: "Search tracks, albums, or artists",
                                autocomplete: "off",
                                spellcheck: "false",
                                class: "w-full min-w-0 bg-transparent text-[15px] text-beet-text-primary placeholder:text-beet-text-disabled focus:outline-none py-2.5",
                                oninput: move |evt| {
                                    schedule_live_search(evt.value());
                                },
                                onkeydown: move |evt| {
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        trigger_immediate_search(search.peek().clone());
                                    } else if evt.key() == Key::Escape && !search.peek().is_empty() {
                                        evt.prevent_default();
                                        clear_all_state();
                                    }
                                },
                            }

                            // Clear button
                            if !current_query.is_empty() {
                                button {
                                    r#type: "button",
                                    aria_label: "Clear search",
                                    title: "Clear search",
                                    class: "p-1.5 rounded-full text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-highest transition-colors cursor-pointer shrink-0",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        clear_all_state();
                                    },
                                    IconX { size: 15 }
                                }
                            }

                            // Submit search button (accessible for keyboard, mouse, and E2E)
                            button {
                                r#type: "submit",
                                aria_label: "Search",
                                title: "Search",
                                class: "inline-flex items-center justify-center h-8 px-3 rounded-full bg-beet-surface-high hover:bg-beet-surface-highest border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer shrink-0",
                                "Search"
                            }

                            // Compact FolderChip when multiple folders exist
                            if current_folders.len() > 1 {
                                div { class: "shrink-0 pl-1 border-l border-beet-border-subtle",
                                    FolderChip {
                                        folders: current_folders.clone(),
                                        selected_folder_id,
                                        on_folder_change: handle_folder_change,
                                        active_menu,
                                    }
                                }
                            }
                        }
                    }

                    // Filter Pills (All | Tracks | Albums | Artists) + active backend badges
                    div { class: "flex flex-wrap items-center justify-between gap-2",
                        div { class: "flex flex-wrap items-center gap-1.5",
                            for (tab, label, count) in [
                                (SearchFilter::All, "All", total_all_count),
                                (SearchFilter::Tracks, "Tracks", total_tracks_count),
                                (SearchFilter::Albums, "Albums", total_albums_count),
                                (SearchFilter::Artists, "Artists", total_artists_count),
                            ] {
                                {
                                    let active = active_filter == tab && detail_snapshot.is_none();
                                    let cls = if active {
                                        "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                                    } else {
                                        "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary"
                                    };
                                    let show_count = has_searched() && count > 0;
                                    rsx! {
                                        button {
                                            key: "filter-{label}",
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full border text-[12px] font-medium transition-colors cursor-pointer {cls}",
                                            onclick: move |_| {
                                                filter_tab.set(tab);
                                                if active_detail.peek().is_some() {
                                                    active_detail.set(None);
                                                }
                                            },
                                            span { "{label}" }
                                            if show_count {
                                                span { class: "tabular-nums-mono text-[10px] opacity-80",
                                                    "{count}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if !offline_only && !active_backends.is_empty() {
                            div { class: "flex flex-wrap items-center gap-1.5",
                                span { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary uppercase tracking-wider",
                                    "Sources"
                                }
                                for backend in active_backends.iter() {
                                    span {
                                        key: "active-backend-{backend.id}",
                                        class: "tabular-nums-mono text-[10px] px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle text-beet-text-secondary",
                                        "{backend.name}"
                                    }
                                }
                            }
                        }
                    }

                    SystemStatus {
                        health: system_health.get(),
                        navidrome_status: auth.navidrome_status(),
                    }
                }

                if !search_error().is_empty() {
                    p { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-4 py-2.5 text-[13px] text-beet-crimson-text",
                        "{search_error}"
                    }
                }
                if !download_error().is_empty() {
                    p { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-4 py-2.5 text-[13px] text-beet-crimson-text",
                        "{download_error}"
                    }
                }

                if let Some(detail) = detail_snapshot {
                    match detail {
                        ActiveDetail::CatalogArtist { loading, artist_name, catalog, error } => {
                            rsx! {
                                section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-5",
                                    div { class: "flex items-center justify-between gap-2",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                                            onclick: move |_| active_detail.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to results" }
                                        }
                                        if let Some(ref cat) = catalog {
                                            span { class: "tabular-nums-mono text-[10px] uppercase tracking-wider px-2.5 py-1 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                                "{cat.provider.display_name()}"
                                            }
                                        }
                                    }

                                    if loading {
                                        div { class: "flex flex-col items-center justify-center py-12 gap-3",
                                            div { class: "size-9 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                                            p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary",
                                                "Loading {artist_name} discography..."
                                            }
                                        }
                                    } else if let Some(err) = error {
                                        div { class: "text-center py-8 space-y-2",
                                            p { class: "text-[14px] font-semibold text-beet-text-primary", "{artist_name}" }
                                            p { class: "text-[13px] text-beet-text-tertiary", "{err}" }
                                        }
                                    } else if let Some(cat) = catalog {
                                        // Artist header
                                        div { class: "flex items-center gap-4 border-b border-beet-border-subtle pb-4",
                                            ArtistAvatar {
                                                key: "detail-art-avatar-{cat.artist.id}",
                                                name: cat.artist.name.clone(),
                                                picture_url: cat.artist.picture_url.clone(),
                                                size_class: "w-16 h-16 text-[18px]".to_string(),
                                            }
                                            div { class: "min-w-0 space-y-1",
                                                h2 { class: "text-[20px] font-semibold text-beet-text-primary truncate",
                                                    "{cat.artist.name}"
                                                }
                                                div { class: "flex flex-wrap items-center gap-2 text-[12px] text-beet-text-secondary",
                                                    if let Some(ref disambig) = cat.artist.disambiguation {
                                                        span { "{disambig}" }
                                                    }
                                                    if let Some(fans) = cat.artist.fan_count.filter(|&f| f > 0) {
                                                        span { class: "tabular-nums-mono text-beet-text-tertiary",
                                                            "{format_fan_count(fans)}"
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // MusicBrainz disambiguation chips ("others")
                                        if !cat.others.is_empty() {
                                            div { class: "space-y-2",
                                                p { class: "text-[11px] tabular-nums-mono uppercase tracking-wider text-beet-text-tertiary",
                                                    "Other artists named {cat.artist.name}"
                                                }
                                                div { class: "flex flex-wrap gap-1.5",
                                                    for other in cat.others.iter() {
                                                        {
                                                            let oid = other.id.clone();
                                                            let oname = other.name.clone();
                                                            let chip_label = match other.disambiguation.as_deref() {
                                                                Some(d) if !d.trim().is_empty() => format!("{} ({})", other.name, d),
                                                                _ => other.name.clone(),
                                                            };
                                                            rsx! {
                                                                button {
                                                                    key: "other-artist-{other.id}",
                                                                    r#type: "button",
                                                                    class: "h-7 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-accent/40 text-[12px] text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                                                                    onclick: move |_| open_catalog_artist(oid.clone(), oname.clone()),
                                                                    "{chip_label}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Artist Top Tracks
                                        if !cat.top_tracks.is_empty() {
                                            div { class: "space-y-2.5",
                                                div { class: "flex items-center justify-between",
                                                    h3 { class: "text-[14px] font-semibold text-beet-text-primary",
                                                        "Top Tracks"
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{cat.top_tracks.len()}"
                                                    }
                                                }
                                                ul { class: "list-none p-0 space-y-2",
                                                    for track in cat.top_tracks.iter() {
                                                        {
                                                            let track_for_src = track.clone();
                                                            let track_for_dl = track.clone();
                                                            let track_for_ov = track.clone();
                                                            let dl_state = download_states.read().get(&track.id).cloned().unwrap_or_default();
                                                            let eff_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                            let key = acquire_key(
                                                                track.mbid.as_deref(),
                                                                Some(&track.id),
                                                                &track.artist,
                                                                &track.title,
                                                            );
                                                            let acq_entry = acquire_map.get(&key).cloned();
                                                            let acq_req = build_acquire_request(track, current_folder_id.clone());
                                                            let acq_track_id = track.id.clone();
                                                            let artist_id_click = track.artist_id.clone().unwrap_or_default();
                                                            let artist_name_click = track.artist.clone();
                                                            let return_artist = (cat.artist.id.clone(), cat.artist.name.clone());
                                                            rsx! {
                                                                li { key: "art-top-{track.id}",
                                                                    CatalogTrackRow {
                                                                        track: track.clone(),
                                                                        acquire_entry: acq_entry,
                                                                        download_state: eff_state,
                                                                        folders: current_folders.clone(),
                                                                        selected_folder_id: current_folder_id.clone(),
                                                                        active_menu,
                                                                        on_acquire_and_play: move |_| {
                                                                            studio.trigger_acquire_and_play(
                                                                                auth,
                                                                                player,
                                                                                acq_req.clone(),
                                                                                Some(acq_track_id.clone()),
                                                                            );
                                                                        },
                                                                        on_pick_acquire_source: move |modal: ManualChoiceModalState| {
                                                                            studio.manual_choice_modal.set(Some(modal));
                                                                        },
                                                                        on_artist_click: move |_| {
                                                                            open_catalog_artist(artist_id_click.clone(), artist_name_click.clone());
                                                                        },
                                                                        on_album_click: {
                                                                            let aid = track.album_id.clone();
                                                                            let atitle = track.album_title.clone().unwrap_or_default();
                                                                            let aartist = track.artist.clone();
                                                                            let ret = return_artist.clone();
                                                                            move |_| {
                                                                                if let Some(ref id) = aid {
                                                                                    open_catalog_album(
                                                                                        id.clone(),
                                                                                        atitle.clone(),
                                                                                        aartist.clone(),
                                                                                        Some(ret.clone()),
                                                                                    );
                                                                                }
                                                                            }
                                                                        },
                                                                        on_search_sources: move |_| {
                                                                            open_source_picker(DownloadQuery::from(track_for_src.to_track()));
                                                                        },
                                                                        on_download: move |_| {
                                                                            handle_auto_download(
                                                                                track_for_dl.id.clone(),
                                                                                DownloadQuery::from(track_for_dl.to_track()),
                                                                            );
                                                                        },
                                                                        on_override_download: move |folder: Folder| {
                                                                            handle_override_download(
                                                                                track_for_ov.id.clone(),
                                                                                DownloadQuery::from(track_for_ov.to_track()),
                                                                                folder,
                                                                            );
                                                                        },
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Artist Albums / EPs / Singles
                                        if !cat.albums.is_empty() {
                                            div { class: "space-y-2.5",
                                                div { class: "flex flex-wrap items-center justify-between gap-2",
                                                    h3 { class: "text-[14px] font-semibold text-beet-text-primary",
                                                        "Albums, EPs & Singles"
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        if !cat.albums_complete || !cat.singles_complete {
                                                            "SHOWING {cat.albums.len()} RELEASES"
                                                        } else {
                                                            "{cat.albums.len()} RELEASES"
                                                        }
                                                    }
                                                }
                                                ul { class: "list-none p-0 space-y-2.5",
                                                    for album in cat.albums.iter() {
                                                        {
                                                            let aid = album.id.clone();
                                                            let is_expanded = expanded_albums.read().contains(&aid);
                                                            let detail = if is_expanded {
                                                                album_cache.read().get(&aid).cloned()
                                                            } else {
                                                                None
                                                            };
                                                            let dl_state = download_states.read().get(&aid).cloned().unwrap_or_default();
                                                            let eff_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                            let alb_src = album.clone();
                                                            let alb_dl = album.clone();
                                                            let alb_ov = album.clone();
                                                            let art_id = album.artist_id.clone().unwrap_or_default();
                                                            let art_name = album.artist.clone();
                                                            rsx! {
                                                                li { key: "art-alb-{album.id}",
                                                                    CatalogAlbumRow {
                                                                        album: album.clone(),
                                                                        is_expanded,
                                                                        detail,
                                                                        download_state: eff_state,
                                                                        download_states,
                                                                        acquire_map: acquire_map.clone(),
                                                                        folders: current_folders.clone(),
                                                                        selected_folder_id: current_folder_id.clone(),
                                                                        active_menu,
                                                                        on_toggle: move |_| toggle_expand_album(aid.clone()),
                                                                        on_artist_click: move |_| open_catalog_artist(art_id.clone(), art_name.clone()),
                                                                        on_search_sources: move |_| {
                                                                            open_source_picker(DownloadQuery::new(vec![]).album(alb_src.to_album()));
                                                                        },
                                                                        on_download: move |_| {
                                                                            handle_auto_download(
                                                                                alb_dl.id.clone(),
                                                                                DownloadQuery::new(vec![]).album(alb_dl.to_album()),
                                                                            );
                                                                        },
                                                                        on_override_download: move |folder: Folder| {
                                                                            handle_override_download(
                                                                                alb_ov.id.clone(),
                                                                                DownloadQuery::new(vec![]).album(alb_ov.to_album()),
                                                                                folder,
                                                                            );
                                                                        },
                                                                        on_track_acquire: move |t: CatalogTrack| {
                                                                            let req = build_acquire_request(&t, selected_folder_id());
                                                                            studio.trigger_acquire_and_play(
                                                                                auth,
                                                                                player,
                                                                                req,
                                                                                Some(t.id.clone()),
                                                                            );
                                                                        },
                                                                        on_track_pick_acquire: move |modal: ManualChoiceModalState| {
                                                                            studio.manual_choice_modal.set(Some(modal));
                                                                        },
                                                                        on_track_search_sources: move |t: CatalogTrack| {
                                                                            open_source_picker(DownloadQuery::from(t.to_track()));
                                                                        },
                                                                        on_track_download: move |t: CatalogTrack| {
                                                                            handle_auto_download(t.id.clone(), DownloadQuery::from(t.to_track()));
                                                                        },
                                                                        on_track_override_download: move |(t, folder): (CatalogTrack, Folder)| {
                                                                            handle_override_download(
                                                                                t.id.clone(),
                                                                                DownloadQuery::from(t.to_track()),
                                                                                folder,
                                                                            );
                                                                        },
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if cat.top_tracks.is_empty() && cat.albums.is_empty() {
                                            p { class: "text-center text-[13px] text-beet-text-tertiary py-8",
                                                "No releases listed for this artist."
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ActiveDetail::CatalogAlbum {
                            album_id: _,
                            title,
                            artist,
                            loading,
                            detail,
                            error,
                            return_to_artist,
                        } => {
                            let back_label = return_to_artist
                                .as_ref()
                                .map(|(_, name)| format!("Back to {name}"))
                                .unwrap_or_else(|| "Back to results".to_string());
                            let return_target = return_to_artist.clone();
                            rsx! {
                                section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-5",
                                    div { class: "flex items-center justify-between gap-2",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                                            onclick: move |_| {
                                                if let Some((ref aid, ref aname)) = return_target {
                                                    open_catalog_artist(aid.clone(), aname.clone());
                                                } else {
                                                    active_detail.set(None);
                                                }
                                            },
                                            IconChevronLeft { size: 14 }
                                            span { "{back_label}" }
                                        }
                                        if let Some(ref det) = detail {
                                            span { class: "tabular-nums-mono text-[10px] uppercase tracking-wider px-2.5 py-1 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                                "{det.album.provider.display_name()}"
                                            }
                                        }
                                    }

                                    if loading {
                                        div { class: "flex flex-col items-center justify-center py-12 gap-3",
                                            div { class: "size-9 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                                            p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary",
                                                "Loading {title}..."
                                            }
                                        }
                                    } else if let Some(err) = error {
                                        div { class: "text-center py-8 space-y-2",
                                            p { class: "text-[14px] font-semibold text-beet-text-primary", "{title}" }
                                            if !artist.trim().is_empty() {
                                                p { class: "text-[12px] text-beet-text-secondary", "{artist}" }
                                            }
                                            p { class: "text-[13px] text-beet-text-tertiary", "{err}" }
                                        }
                                    } else if let Some(det) = detail {
                                        {
                                            let alb = det.album.clone();
                                            let mut alb_for_facts = alb.clone();
                                            if alb_for_facts.track_count.is_none() && !det.tracks.is_empty() {
                                                alb_for_facts.track_count = Some(det.tracks.len() as u32);
                                            }
                                            let facts = catalog_album_facts(&alb_for_facts);
                                            let alb_art_id = alb.artist_id.clone().unwrap_or_default();
                                            let alb_art_name = alb.artist.clone();
                                            let alb_src = alb.clone();
                                            let alb_dl = alb.clone();
                                            let alb_ov = alb.clone();
                                            let alb_dl_state = download_states
                                                .read()
                                                .get(&alb.id)
                                                .cloned()
                                                .unwrap_or_default();
                                            let alb_eff_state = if !has_folders {
                                                DownloadRowState::Disabled
                                            } else {
                                                alb_dl_state
                                            };

                                            rsx! {
                                                // Album header
                                                div { class: "flex flex-col sm:flex-row sm:items-center gap-4 border-b border-beet-border-subtle pb-4",
                                                    AlbumCoverArt {
                                                        key: "detail-alb-cover-{alb.id}",
                                                        cover_url: alb.cover_url.clone(),
                                                        size_class: "w-24 h-24 rounded-[10px]".to_string(),
                                                        icon_size: 28,
                                                    }
                                                    div { class: "flex-grow min-w-0 space-y-1",
                                                        h2 { class: "text-[20px] font-semibold text-beet-text-primary truncate",
                                                            "{alb.title}"
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            class: "text-[14px] text-beet-text-secondary hover:text-beet-leaf-text transition-colors truncate block max-w-full text-left cursor-pointer",
                                                            onclick: move |_| {
                                                                open_catalog_artist(alb_art_id.clone(), alb_art_name.clone());
                                                            },
                                                            "{alb.artist}"
                                                        }
                                                        if let Some(f) = facts {
                                                            p { class: "text-[12px] text-beet-text-tertiary tabular-nums-mono",
                                                                "{f}"
                                                            }
                                                        }
                                                    }
                                                    div { class: "flex items-center gap-1.5 shrink-0",
                                                        button {
                                                            r#type: "button",
                                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                                                            aria_label: "Search sources",
                                                            title: "Search sources",
                                                            onclick: move |evt: MouseEvent| {
                                                                evt.stop_propagation();
                                                                open_source_picker(DownloadQuery::new(vec![]).album(alb_src.to_album()));
                                                            },
                                                            IconSearch { size: 13 }
                                                            span { "Search sources" }
                                                        }
                                                        DownloadIcon {
                                                            item_id: alb.id.clone(),
                                                            state: alb_eff_state,
                                                            folders: current_folders.clone(),
                                                            selected_folder_id: current_folder_id.clone(),
                                                            active_menu,
                                                            on_download: move |_| {
                                                                handle_auto_download(
                                                                    alb_dl.id.clone(),
                                                                    DownloadQuery::new(vec![]).album(alb_dl.to_album()),
                                                                );
                                                            },
                                                            on_override_download: move |folder: Folder| {
                                                                handle_override_download(
                                                                    alb_ov.id.clone(),
                                                                    DownloadQuery::new(vec![]).album(alb_ov.to_album()),
                                                                    folder,
                                                                );
                                                            },
                                                        }
                                                    }
                                                }

                                                // Ordered Album Tracklist
                                                if det.tracks.is_empty() {
                                                    p { class: "text-center text-[13px] text-beet-text-tertiary py-8",
                                                        "No tracklist available for this release."
                                                    }
                                                } else {
                                                    ul { class: "list-none p-0 divide-y divide-beet-border-subtle/60",
                                                        for (idx, track) in det.tracks.iter().enumerate() {
                                                            {
                                                                let track_acq = track.clone();
                                                                let track_src = track.clone();
                                                                let track_dl = track.clone();
                                                                let track_ov = track.clone();
                                                                let dl_state = download_states
                                                                    .read()
                                                                    .get(&track.id)
                                                                    .cloned()
                                                                    .unwrap_or_default();
                                                                let eff_state = if !has_folders {
                                                                    DownloadRowState::Disabled
                                                                } else {
                                                                    dl_state
                                                                };
                                                                let key = acquire_key(
                                                                    track.mbid.as_deref(),
                                                                    Some(&track.id),
                                                                    &track.artist,
                                                                    &track.title,
                                                                );
                                                                let acq_entry = acquire_map.get(&key).cloned();
                                                                let stage = acq_entry.as_ref().map(|e| e.badge.clone());
                                                                let manual_choice = acq_entry.and_then(|e| e.manual_choice);
                                                                let in_flight = stage
                                                                    .as_ref()
                                                                    .is_some_and(AcquireStageBadge::is_in_flight);
                                                                let dur = format_catalog_duration(
                                                                    track.duration.as_deref(),
                                                                    track.duration_secs,
                                                                );

                                                                rsx! {
                                                                    li {
                                                                        key: "det-alb-trk-{track.id}-{idx}",
                                                                        class: "relative flex items-center gap-3 py-2.5 px-2 rounded-[8px] hover:bg-beet-surface/70 transition-colors",
                                                                        span { class: "w-6 text-right tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                                                            "{idx + 1}"
                                                                        }
                                                                        div { class: "flex-grow min-w-0 flex items-center gap-2",
                                                                            span { class: "text-[14px] text-beet-text-primary truncate",
                                                                                "{track.title}"
                                                                            }
                                                                            if let Some(ref variant) = track.variant_label {
                                                                                span { class: "shrink-0 px-1.5 py-0.5 rounded-[4px] bg-beet-surface-highest border border-beet-border-subtle tabular-nums-mono text-[9px] text-beet-text-secondary",
                                                                                    "{variant}"
                                                                                }
                                                                            }
                                                                        }
                                                                        if let Some(d) = dur {
                                                                            span { class: "tabular-nums-mono text-[12px] text-beet-text-tertiary shrink-0 hidden sm:block",
                                                                                "{d}"
                                                                            }
                                                                        }
                                                                        div { class: "flex items-center gap-1 shrink-0",
                                                                            AcquireActionPill {
                                                                                stage,
                                                                                manual_choice,
                                                                                recording_mbid: track.mbid.clone(),
                                                                                catalog_id: Some(track.id.clone()),
                                                                                artist: track.artist.clone(),
                                                                                title: track.title.clone(),
                                                                                compact: true,
                                                                                on_acquire: move |_| {
                                                                                    let req = build_acquire_request(
                                                                                        &track_acq,
                                                                                        selected_folder_id(),
                                                                                    );
                                                                                    studio.trigger_acquire_and_play(
                                                                                        auth,
                                                                                        player,
                                                                                        req,
                                                                                        Some(track_acq.id.clone()),
                                                                                    );
                                                                                },
                                                                                on_pick_source: move |modal: ManualChoiceModalState| {
                                                                                    studio.manual_choice_modal.set(Some(modal));
                                                                                },
                                                                            }
                                                                            button {
                                                                                r#type: "button",
                                                                                class: "p-1.5 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
                                                                                aria_label: "Search sources",
                                                                                title: "Search sources",
                                                                                onclick: move |evt: MouseEvent| {
                                                                                    evt.stop_propagation();
                                                                                    open_source_picker(DownloadQuery::from(track_src.to_track()));
                                                                                },
                                                                                IconSearch {
                                                                                    size: 14,
                                                                                    class: "text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors".to_string(),
                                                                                }
                                                                            }
                                                                            DownloadIcon {
                                                                                item_id: track.id.clone(),
                                                                                state: eff_state,
                                                                                folders: current_folders.clone(),
                                                                                selected_folder_id: current_folder_id.clone(),
                                                                                active_menu,
                                                                                on_download: move |_| {
                                                                                    handle_auto_download(
                                                                                        track_dl.id.clone(),
                                                                                        DownloadQuery::from(track_dl.to_track()),
                                                                                    );
                                                                                },
                                                                                on_override_download: move |folder: Folder| {
                                                                                    handle_override_download(
                                                                                        track_ov.id.clone(),
                                                                                        DownloadQuery::from(track_ov.to_track()),
                                                                                        folder,
                                                                                    );
                                                                                },
                                                                            }
                                                                        }
                                                                        if in_flight {
                                                                            span {
                                                                                class: "beet-seam-light absolute inset-x-2 bottom-0 rounded-full",
                                                                                aria_hidden: "true",
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ActiveDetail::LocalArtist { artist, loading, albums, error } => {
                            let artist_name_for_catalog = artist.name.clone();
                            rsx! {
                                section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-5",
                                    div { class: "flex flex-wrap items-center justify-between gap-2",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                                            onclick: move |_| active_detail.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to results" }
                                        }
                                        if !offline_only {
                                            button {
                                                r#type: "button",
                                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-[12px] font-medium text-beet-accent-text hover:brightness-110 transition-all cursor-pointer",
                                                onclick: move |_| open_catalog_artist(String::new(), artist_name_for_catalog.clone()),
                                                IconSparkles { size: 13 }
                                                span { "Explore full catalog" }
                                            }
                                        }
                                    }

                                    div { class: "flex items-center gap-4 border-b border-beet-border-subtle pb-4",
                                        ArtistAvatar {
                                            key: "local-detail-art-avatar-{artist.id}",
                                            name: artist.name.clone(),
                                            picture_url: artist.cover_art_url.clone(),
                                            size_class: "w-14 h-14 text-[16px]".to_string(),
                                        }
                                        div { class: "min-w-0",
                                            h2 { class: "text-[19px] font-semibold text-beet-text-primary truncate",
                                                "{artist.name}"
                                            }
                                            p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary mt-0.5",
                                                if artist.album_count == 1 {
                                                    "IN YOUR LIBRARY · 1 ALBUM"
                                                } else {
                                                    "IN YOUR LIBRARY · {artist.album_count} ALBUMS"
                                                }
                                            }
                                        }
                                    }

                                    if loading {
                                        div { class: "flex flex-col items-center justify-center py-10 gap-3",
                                            div { class: "size-8 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                                            p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary",
                                                "Loading library albums..."
                                            }
                                        }
                                    } else if let Some(err) = error {
                                        p { class: "text-[13px] text-beet-crimson-text py-4", "{err}" }
                                    } else if albums.is_empty() {
                                        p { class: "text-[13px] text-beet-text-tertiary py-4",
                                            "No local albums found for this artist."
                                        }
                                    } else {
                                        div { class: "grid grid-cols-2 sm:grid-cols-4 gap-3",
                                            for alb in albums.iter() {
                                                {
                                                    let aid = alb.id.clone();
                                                    rsx! {
                                                        button {
                                                            key: "local-art-alb-{alb.id}",
                                                            r#type: "button",
                                                            class: "p-2.5 rounded-[10px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer space-y-2",
                                                            onclick: move |_| play_local_album(aid.clone()),
                                                            div { class: "aspect-square rounded-[8px] overflow-hidden bg-beet-surface-low flex items-center justify-center",
                                                                if let Some(url) = alb.cover_art_url.as_deref() {
                                                                    img { src: "{url}", alt: "", class: "w-full h-full object-cover" }
                                                                } else {
                                                                    IconDisc3 { size: 24, class: "text-beet-text-disabled".to_string() }
                                                                }
                                                            }
                                                            div {
                                                                p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{alb.name}" }
                                                                p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary truncate",
                                                                    if let Some(yr) = alb.year {
                                                                        "{yr} · {alb.song_count} tracks"
                                                                    } else {
                                                                        "{alb.song_count} tracks"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if !has_searched() {
                    // Idle State (Never Blank!)
                    div { class: "space-y-6 pt-1",
                        if !mixes.is_empty() {
                            section { class: "space-y-3",
                                h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                                    "Quick Mood & Seasonal Stations"
                                }
                                div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3",
                                    for mix in mixes.iter().take(6) {
                                        {
                                            let slug = mix.slug.clone();
                                            rsx! {
                                                button {
                                                    key: "srch-mix-{mix.slug}",
                                                    r#type: "button",
                                                    class: "flex items-center justify-between gap-3 p-3.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer",
                                                    onclick: move |_| studio.play_contextual_mix(auth, player, slug.clone()),
                                                    div { class: "min-w-0",
                                                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                            "{format_mix_title(mix)}"
                                                        }
                                                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary mt-0.5",
                                                            "{mix.track_count} TRACKS"
                                                        }
                                                    }
                                                    span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center shrink-0",
                                                        IconPlay { size: 13, filled: true }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if !starred.is_empty() {
                            section { class: "space-y-2",
                                h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                                    "Recently Starred"
                                }
                                div { class: "space-y-1", role: "list",
                                    for (idx, track) in starred.iter().take(6).enumerate() {
                                        {
                                            let queue = starred.clone();
                                            let t_star = track.clone();
                                            let t_rate = track.clone();
                                            let t_inspect = track.clone();
                                            let row_state = if offline_only && !track.is_offline_cached {
                                                TrackRowState::OfflineUnavailable
                                            } else {
                                                TrackRowState::Owned {
                                                    is_offline_cached: track.is_offline_cached,
                                                    starred: track.starred,
                                                    rating: track.rating,
                                                }
                                            };
                                            rsx! {
                                                TrackRow {
                                                    key: "srch-star-{track.id}",
                                                    title: track.title.clone(),
                                                    artist: track.artist.clone(),
                                                    album: track.album.clone(),
                                                    cover_art_url: track.cover_art_url.clone(),
                                                    duration_secs: track.duration_secs,
                                                    is_playing: player.is_track_playing(&track.id),
                                                    state: row_state,
                                                    on_play: move |_| player.play_queue(queue.clone(), idx),
                                                    on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                    on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                    on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if !backend_summary.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle px-4 py-3 flex flex-wrap items-center justify-between gap-2 text-[12px] text-beet-text-secondary",
                                span {
                                    "Type any track, album, or artist to search your library and the remote catalog simultaneously."
                                }
                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    "ACTIVE SOURCES · {backend_summary}"
                                }
                            }
                        }
                    }
                } else {
                    // 1. In Your Library (Local Navidrome Artists, Albums, Tracks + Pending Triage)
                    {
                        let show_local_artists = active_filter.shows_artists() && !local_snapshot.artists.is_empty();
                        let show_local_albums = active_filter.shows_albums() && !local_snapshot.albums.is_empty();
                        let show_local_tracks = active_filter.shows_tracks()
                            && (!matching_triage.is_empty() || !local_snapshot.songs.is_empty());
                        let has_visible_local = show_local_artists || show_local_albums || show_local_tracks;

                        if has_visible_local || offline_only {
                            let visible_local_count = (if show_local_artists { local_snapshot.artists.len() } else { 0 })
                                + (if show_local_albums { local_snapshot.albums.len() } else { 0 })
                                + (if show_local_tracks { local_tracks_count } else { 0 });
                            rsx! {
                                section { class: "space-y-4",
                                    div { class: "flex items-center justify-between",
                                        h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                                            "In Your Library"
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                            "{visible_local_count} MATCHES"
                                        }
                                    }

                                    // Local Artists
                                    if show_local_artists {
                                        div { class: "space-y-2",
                                            p { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                "Artists"
                                            }
                                            div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-2.5",
                                                for art in local_snapshot.artists.iter() {
                                                    {
                                                        let art_click = art.clone();
                                                        rsx! {
                                                            button {
                                                                key: "loc-art-{art.id}",
                                                                r#type: "button",
                                                                class: "flex items-center gap-3 p-2.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer min-w-0",
                                                                onclick: move |_| open_local_artist(art_click.clone()),
                                                                ArtistAvatar {
                                                                    name: art.name.clone(),
                                                                    picture_url: art.cover_art_url.clone(),
                                                                    size_class: "w-10 h-10 text-[13px]".to_string(),
                                                                }
                                                                div { class: "min-w-0",
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate",
                                                                        "{art.name}"
                                                                    }
                                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary truncate",
                                                                        if art.album_count == 1 {
                                                                            "1 album"
                                                                        } else {
                                                                            "{art.album_count} albums"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Local Albums
                                    if show_local_albums {
                                        div { class: "space-y-2",
                                            p { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                "Albums"
                                            }
                                            div { class: "grid grid-cols-2 sm:grid-cols-4 gap-3",
                                                for alb in local_snapshot.albums.iter().take(if active_filter == SearchFilter::Albums { 8 } else { 4 }) {
                                                    {
                                                        let aid = alb.id.clone();
                                                        rsx! {
                                                            button {
                                                                key: "srch-alb-{alb.id}",
                                                                r#type: "button",
                                                                class: "p-2.5 rounded-[10px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer space-y-2",
                                                                onclick: move |_| play_local_album(aid.clone()),
                                                                div { class: "aspect-square rounded-[8px] overflow-hidden bg-beet-surface-low flex items-center justify-center",
                                                                    if let Some(url) = alb.cover_art_url.as_deref() {
                                                                        img { src: "{url}", alt: "", class: "w-full h-full object-cover" }
                                                                    } else {
                                                                        IconDisc3 { size: 24, class: "text-beet-text-disabled".to_string() }
                                                                    }
                                                                }
                                                                div {
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{alb.name}" }
                                                                    p { class: "text-[11px] text-beet-text-secondary truncate", "{alb.artist}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Local Tracks (Triage + Navidrome Songs)
                                    if show_local_tracks {
                                        div { class: "space-y-2",
                                            p { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                "Tracks"
                                            }
                                            div { class: "space-y-1", role: "list", aria_label: "Library matches",
                                                for dt in matching_triage.iter() {
                                                    {
                                                        let t_play = dt.clone();
                                                        let t_keep = dt.clone();
                                                        let t_drop = dt.clone();
                                                        let t_rate = dt.clone();
                                                        let t_inspect = dt.clone();
                                                        rsx! {
                                                            TrackRow {
                                                                key: "srch-disc-{dt.id}",
                                                                title: dt.title.clone(),
                                                                artist: dt.artist.clone(),
                                                                album: dt.album.clone(),
                                                                cover_art_url: dt.cover_art_url.clone(),
                                                                is_playing: !dt.id.is_empty() && player.is_track_playing(&dt.id),
                                                                state: TrackRowState::PendingDiscovery {
                                                                    days_remaining: dt.days_remaining,
                                                                    reason_label: dt.reason_label.clone(),
                                                                    rating: dt.rating,
                                                                },
                                                                on_play: move |_| player.play_track(t_play.clone()),
                                                                on_keep: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                                                on_drop: move |_| studio.drop_track(auth, player, t_drop.clone()),
                                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                                on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                                            }
                                                        }
                                                    }
                                                }
                                                for (idx, song) in local_snapshot.songs.iter().enumerate() {
                                                    {
                                                        let all_songs = local_snapshot.songs.clone();
                                                        let t_star = song.clone();
                                                        let t_rate = song.clone();
                                                        let t_inspect = song.clone();
                                                        let row_state = if offline_only && !song.is_offline_cached {
                                                            TrackRowState::OfflineUnavailable
                                                        } else {
                                                            TrackRowState::Owned {
                                                                is_offline_cached: song.is_offline_cached,
                                                                starred: song.starred,
                                                                rating: song.rating,
                                                            }
                                                        };
                                                        rsx! {
                                                            TrackRow {
                                                                key: "srch-lib-{song.id}",
                                                                title: song.title.clone(),
                                                                artist: song.artist.clone(),
                                                                album: song.album.clone(),
                                                                cover_art_url: song.cover_art_url.clone(),
                                                                duration_secs: song.duration_secs,
                                                                codec_badge: song.codec_badge.clone(),
                                                                is_playing: player.is_track_playing(&song.id),
                                                                state: row_state,
                                                                on_play: move |_| player.play_queue(all_songs.clone(), idx),
                                                                on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                                on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    } else if offline_only && !has_visible_local {
                                        p { class: "text-[13px] text-beet-text-tertiary py-2",
                                            "No matching cached tracks in your library."
                                        }
                                    }
                                }
                            }
                        } else {
                            rsx! {}
                        }
                    }

                    // 2. Catalog Section (Unified Deezer + MusicBrainz)
                    if offline_only {
                        div { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-4 flex items-center gap-3 text-[13px] text-beet-text-secondary",
                            IconWifiOff { size: 17, class: "text-beet-leaf-text".to_string() }
                            span { "Remote catalog search is hidden while Offline Only mode is active." }
                        }
                    } else if remote_loading() {
                        div { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle flex flex-col items-center justify-center py-12 gap-3",
                            div { class: "size-9 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                            p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary",
                                "Searching catalog..."
                            }
                        }
                    } else if let Some(ref res) = remote_snapshot {
                        {
                            let show_remote_artists = active_filter.shows_artists() && !remote_artists.is_empty();
                            let show_remote_tracks = active_filter.shows_tracks() && !deduped_remote_tracks.is_empty();
                            let show_remote_albums = active_filter.shows_albums() && !deduped_remote_albums.is_empty();
                            let has_visible_remote = show_remote_artists || show_remote_tracks || show_remote_albums;
                            let show_direct_escape = active_filter.shows_tracks()
                                && deduped_remote_tracks.is_empty()
                                && !trimmed_query.is_empty();

                            rsx! {
                                section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-5",
                                    // Section Header
                                    div { class: "flex flex-wrap items-center justify-between gap-2 border-b border-beet-border-subtle pb-3",
                                        div { class: "flex items-center gap-2.5",
                                            h2 { class: "text-[17px] font-semibold text-beet-text-primary",
                                                "Catalog"
                                            }
                                            span { class: "tabular-nums-mono text-[10px] uppercase tracking-wider px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                                "{res.provider.display_name()}"
                                            }
                                        }
                                        if !backend_summary.is_empty() {
                                            span { class: "tabular-nums-mono text-[10px] px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-secondary",
                                                "{backend_summary}"
                                            }
                                        }
                                    }

                                    // Subtle fallback notice when MusicBrainz served fallback results
                                    if res.fallback_used {
                                        div { class: "rounded-[10px] bg-beet-surface border border-beet-border-subtle px-3.5 py-2 flex items-center gap-2 text-[12px] text-beet-text-secondary",
                                            IconSparkles { size: 14, class: "text-beet-accent-text".to_string() }
                                            span { "Showing deeper catalog results from MusicBrainz" }
                                        }
                                    }

                                    // Remote Artists strip/grid
                                    if show_remote_artists {
                                        div { class: "space-y-2.5",
                                            div { class: "flex items-center justify-between",
                                                h3 { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                    "Artists"
                                                }
                                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                    "{remote_artists.len()}"
                                                }
                                            }
                                            div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-2.5",
                                                for artist in remote_artists.iter() {
                                                    {
                                                        let aid = artist.id.clone();
                                                        let aname = artist.name.clone();
                                                        let subtitle = artist_subtitle(artist);
                                                        rsx! {
                                                            button {
                                                                key: "cat-art-{artist.id}",
                                                                r#type: "button",
                                                                class: "flex items-center gap-3 p-2.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer min-w-0 group",
                                                                onclick: move |_| open_catalog_artist(aid.clone(), aname.clone()),
                                                                ArtistAvatar {
                                                                    name: artist.name.clone(),
                                                                    picture_url: artist.picture_url.clone(),
                                                                    size_class: "w-11 h-11 text-[13px]".to_string(),
                                                                }
                                                                div { class: "min-w-0",
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate group-hover:text-beet-leaf-text transition-colors",
                                                                        "{artist.name}"
                                                                    }
                                                                    if let Some(sub) = subtitle {
                                                                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary truncate",
                                                                            "{sub}"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Remote Tracks list
                                    if show_remote_tracks {
                                        div { class: "space-y-2.5",
                                            div { class: "flex items-center justify-between",
                                                h3 { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                    "Tracks"
                                                }
                                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                    "{deduped_remote_tracks.len()}"
                                                }
                                            }
                                            ul { class: "list-none p-0 space-y-2",
                                                for track in deduped_remote_tracks.iter() {
                                                    {
                                                        let track_for_src = track.clone();
                                                        let track_for_dl = track.clone();
                                                        let track_for_ov = track.clone();
                                                        let dl_state = download_states.read().get(&track.id).cloned().unwrap_or_default();
                                                        let eff_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                        let key = acquire_key(
                                                            track.mbid.as_deref(),
                                                            Some(&track.id),
                                                            &track.artist,
                                                            &track.title,
                                                        );
                                                        let acq_entry = acquire_map.get(&key).cloned();
                                                        let acq_req = build_acquire_request(track, current_folder_id.clone());
                                                        let acq_track_id = track.id.clone();
                                                        let art_id = track.artist_id.clone().unwrap_or_default();
                                                        let art_name = track.artist.clone();
                                                        let album_id_opt = track.album_id.clone();
                                                        let album_title_click = track.album_title.clone().unwrap_or_default();
                                                        let album_artist_click = track.artist.clone();

                                                        rsx! {
                                                            li { key: "cat-trk-{track.id}",
                                                                CatalogTrackRow {
                                                                    track: track.clone(),
                                                                    acquire_entry: acq_entry,
                                                                    download_state: eff_state,
                                                                    folders: current_folders.clone(),
                                                                    selected_folder_id: current_folder_id.clone(),
                                                                    active_menu,
                                                                    on_acquire_and_play: move |_| {
                                                                        studio.trigger_acquire_and_play(
                                                                            auth,
                                                                            player,
                                                                            acq_req.clone(),
                                                                            Some(acq_track_id.clone()),
                                                                        );
                                                                    },
                                                                    on_pick_acquire_source: move |modal: ManualChoiceModalState| {
                                                                        studio.manual_choice_modal.set(Some(modal));
                                                                    },
                                                                    on_artist_click: move |_| {
                                                                        open_catalog_artist(art_id.clone(), art_name.clone());
                                                                    },
                                                                    on_album_click: move |_| {
                                                                        if let Some(ref aid) = album_id_opt {
                                                                            open_catalog_album(
                                                                                aid.clone(),
                                                                                album_title_click.clone(),
                                                                                album_artist_click.clone(),
                                                                                None,
                                                                            );
                                                                        }
                                                                    },
                                                                    on_search_sources: move |_| {
                                                                        open_source_picker(DownloadQuery::from(track_for_src.to_track()));
                                                                    },
                                                                    on_download: move |_| {
                                                                        handle_auto_download(
                                                                            track_for_dl.id.clone(),
                                                                            DownloadQuery::from(track_for_dl.to_track()),
                                                                        );
                                                                    },
                                                                    on_override_download: move |folder: Folder| {
                                                                        handle_override_download(
                                                                            track_for_ov.id.clone(),
                                                                            DownloadQuery::from(track_for_ov.to_track()),
                                                                            folder,
                                                                        );
                                                                    },
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Remote Albums list
                                    if show_remote_albums {
                                        div { class: "space-y-2.5",
                                            div { class: "flex items-center justify-between",
                                                h3 { class: "tabular-nums-mono text-[11px] uppercase tracking-wider text-beet-text-tertiary",
                                                    "Albums"
                                                }
                                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                    "{deduped_remote_albums.len()}"
                                                }
                                            }
                                            ul { class: "list-none p-0 space-y-2.5",
                                                for album in deduped_remote_albums.iter() {
                                                    {
                                                        let aid = album.id.clone();
                                                        let is_expanded = expanded_albums.read().contains(&aid);
                                                        let detail = if is_expanded {
                                                            album_cache.read().get(&aid).cloned()
                                                        } else {
                                                            None
                                                        };
                                                        let dl_state = download_states.read().get(&aid).cloned().unwrap_or_default();
                                                        let eff_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                        let alb_src = album.clone();
                                                        let alb_dl = album.clone();
                                                        let alb_ov = album.clone();
                                                        let art_id = album.artist_id.clone().unwrap_or_default();
                                                        let art_name = album.artist.clone();

                                                        rsx! {
                                                            li { key: "cat-alb-{album.id}",
                                                                CatalogAlbumRow {
                                                                    album: album.clone(),
                                                                    is_expanded,
                                                                    detail,
                                                                    download_state: eff_state,
                                                                    download_states,
                                                                    acquire_map: acquire_map.clone(),
                                                                    folders: current_folders.clone(),
                                                                    selected_folder_id: current_folder_id.clone(),
                                                                    active_menu,
                                                                    on_toggle: move |_| toggle_expand_album(aid.clone()),
                                                                    on_artist_click: move |_| open_catalog_artist(art_id.clone(), art_name.clone()),
                                                                    on_search_sources: move |_| {
                                                                        open_source_picker(DownloadQuery::new(vec![]).album(alb_src.to_album()));
                                                                    },
                                                                    on_download: move |_| {
                                                                        handle_auto_download(
                                                                            alb_dl.id.clone(),
                                                                            DownloadQuery::new(vec![]).album(alb_dl.to_album()),
                                                                        );
                                                                    },
                                                                    on_override_download: move |folder: Folder| {
                                                                        handle_override_download(
                                                                            alb_ov.id.clone(),
                                                                            DownloadQuery::new(vec![]).album(alb_ov.to_album()),
                                                                            folder,
                                                                        );
                                                                    },
                                                                    on_track_acquire: move |t: CatalogTrack| {
                                                                        let req = build_acquire_request(&t, selected_folder_id());
                                                                        studio.trigger_acquire_and_play(
                                                                            auth,
                                                                            player,
                                                                            req,
                                                                            Some(t.id.clone()),
                                                                        );
                                                                    },
                                                                    on_track_pick_acquire: move |modal: ManualChoiceModalState| {
                                                                        studio.manual_choice_modal.set(Some(modal));
                                                                    },
                                                                    on_track_search_sources: move |t: CatalogTrack| {
                                                                        open_source_picker(DownloadQuery::from(t.to_track()));
                                                                    },
                                                                    on_track_download: move |t: CatalogTrack| {
                                                                        handle_auto_download(t.id.clone(), DownloadQuery::from(t.to_track()));
                                                                    },
                                                                    on_track_override_download: move |(t, folder): (CatalogTrack, Folder)| {
                                                                        handle_override_download(
                                                                            t.id.clone(),
                                                                            DownloadQuery::from(t.to_track()),
                                                                            folder,
                                                                        );
                                                                    },
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Empty state for current filter or whole catalog
                                    if !has_visible_remote {
                                        div { class: "text-center text-beet-text-tertiary py-6 text-[13px]",
                                            if res.tracks.is_empty() && res.albums.is_empty() && res.artists.is_empty() {
                                                p { "No catalog matches for \"{trimmed_query}\"." }
                                            } else if active_filter == SearchFilter::Tracks && !res.tracks.is_empty() {
                                                p { "All matching catalog tracks are already in your library." }
                                            } else if active_filter == SearchFilter::Albums && !res.albums.is_empty() {
                                                p { "All matching catalog albums are already in your library." }
                                            } else {
                                                p { "No additional catalog items in this category." }
                                            }
                                        }
                                    }

                                    // Direct download-source escape when 0 remote tracks remain
                                    if show_direct_escape {
                                        DirectSearchEscape {
                                            query: trimmed_query.clone(),
                                            on_search_sources: move |q: DownloadQuery| open_source_picker(q),
                                        }
                                    }

                                    // Deep Catalog Footer (when Deezer served results)
                                    if res.provider == CatalogProvider::Deezer && !trimmed_query.is_empty() {
                                        div { class: "pt-3 border-t border-beet-border-subtle flex flex-wrap items-center justify-between gap-2 text-[12px] text-beet-text-tertiary",
                                            span { "Didn't find a niche release?" }
                                            button {
                                                r#type: "button",
                                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-default hover:border-beet-accent/40 text-beet-text-secondary hover:text-beet-text-primary font-medium transition-colors cursor-pointer",
                                                onclick: {
                                                    let q = trimmed_query.clone();
                                                    move |_| {
                                                        cancel_debounces();
                                                        run_remote_search(q.clone(), Some(CatalogProvider::Musicbrainz));
                                                    }
                                                },
                                                IconSearch { size: 12 }
                                                span { "Search deeper catalog (MusicBrainz)" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Fallback toasts lifted cleanly above the 84px Studio player bar
            if !fallback_toasts.read().is_empty() {
                div { class: "fixed bottom-24 right-5 flex flex-col-reverse gap-2 z-40 w-80 md:w-96",
                    for toast_data in fallback_toasts.read().iter() {
                        FallbackToast {
                            key: "{toast_data.id}",
                            toast: toast_data.clone(),
                            on_pick_source: move |choices: SourceChoices| {
                                let next_gen = source_search_gen.peek().wrapping_add(1);
                                source_search_gen.set(next_gen);
                                download_options.set(Some(choices));
                                source_searching.set(false);
                            },
                            on_dismiss: move |id: String| {
                                fallback_toasts.write().retain(|t| t.id != id);
                            },
                        }
                    }
                }
            }
        }
    }
}

fn auto_dl_batch_id(event: &AutoDownloadEvent) -> &str {
    match event {
        AutoDownloadEvent::Searching { batch_id, .. }
        | AutoDownloadEvent::ScoringResults { batch_id, .. }
        | AutoDownloadEvent::PickedSource { batch_id, .. }
        | AutoDownloadEvent::Downloading { batch_id }
        | AutoDownloadEvent::FallbackToManual { batch_id, .. }
        | AutoDownloadEvent::Failed { batch_id, .. } => batch_id,
    }
}

fn artist_subtitle(artist: &CatalogArtist) -> Option<String> {
    if let Some(d) = artist
        .disambiguation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(d.to_string());
    }
    artist
        .fan_count
        .filter(|&f| f > 0)
        .map(format_fan_count)
}

fn build_acquire_request(
    track: &CatalogTrack,
    folder_id: Option<String>,
) -> AcquireAndPlayRequest {
    let duration_s = track
        .duration_secs
        .or_else(|| parse_duration_secs(track.duration.as_deref()));
    AcquireAndPlayRequest {
        artist: track.artist.clone(),
        title: track.title.clone(),
        album: track.album_title.clone(),
        recording_mbid: track.mbid.clone(),
        release_mbid: track.release_mbid.clone(),
        duration_s,
        folder_id,
        prefer_fast: true,
    }
}

#[component]
fn ArtistAvatar(name: String, picture_url: Option<String>, size_class: String) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let initials = artist_initials(&name);
    let active_url = picture_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty() && failed_url.read().as_deref() != Some(u.as_str()));

    rsx! {
        div {
            class: "rounded-full overflow-hidden bg-beet-surface-highest border border-beet-border-subtle flex items-center justify-center shrink-0 font-semibold text-beet-text-secondary {size_class}",
            if let Some(url) = active_url {
                {
                    let failed_target = url.clone();
                    rsx! {
                        img {
                            key: "{url}",
                            src: "{url}",
                            alt: "",
                            class: "w-full h-full object-cover",
                            onerror: move |_| failed_url.set(Some(failed_target.clone())),
                        }
                    }
                }
            } else {
                span { "{initials}" }
            }
        }
    }
}

#[component]
fn AlbumCoverArt(cover_url: Option<String>, size_class: String, icon_size: u32) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let active_url = cover_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty() && failed_url.read().as_deref() != Some(u.as_str()));

    rsx! {
        div {
            class: "shrink-0 overflow-hidden bg-beet-surface-low border border-beet-border-subtle flex items-center justify-center {size_class}",
            if let Some(url) = active_url {
                {
                    let failed_target = url.clone();
                    rsx! {
                        img {
                            key: "{url}",
                            src: "{url}",
                            alt: "",
                            class: "w-full h-full object-cover",
                            onerror: move |_| failed_url.set(Some(failed_target.clone())),
                        }
                    }
                }
            } else {
                IconDisc3 { size: icon_size, class: "text-beet-text-disabled".to_string() }
            }
        }
    }
}

#[derive(Props, PartialEq, Clone)]
struct CatalogTrackRowProps {
    track: CatalogTrack,
    acquire_entry: Option<AcquireTrackState>,
    download_state: DownloadRowState,
    folders: Vec<Folder>,
    selected_folder_id: Option<String>,
    active_menu: Signal<Option<String>>,
    on_acquire_and_play: EventHandler<()>,
    on_pick_acquire_source: EventHandler<ManualChoiceModalState>,
    on_artist_click: EventHandler<()>,
    on_album_click: EventHandler<()>,
    on_search_sources: EventHandler<()>,
    on_download: EventHandler<()>,
    on_override_download: EventHandler<Folder>,
}

#[component]
fn CatalogTrackRow(props: CatalogTrackRowProps) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let track = &props.track;
    let stage = props.acquire_entry.as_ref().map(|e| e.badge.clone());
    let manual_choice = props
        .acquire_entry
        .as_ref()
        .and_then(|e| e.manual_choice.clone());
    let in_flight = stage.as_ref().is_some_and(AcquireStageBadge::is_in_flight);
    let duration = format_catalog_duration(track.duration.as_deref(), track.duration_secs);
    let active_cover_url = track
        .cover_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty() && failed_url.read().as_deref() != Some(*u))
        .map(str::to_string);

    let mbid_for_pick = track.mbid.clone();
    let catalog_id_for_pick = Some(track.id.clone());
    let artist_for_pick = track.artist.clone();
    let title_for_pick = track.title.clone();

    let on_art_click = {
        let stage = stage.clone();
        let manual_choice = manual_choice.clone();
        let on_acquire = props.on_acquire_and_play;
        let on_pick = props.on_pick_acquire_source;
        let mbid = mbid_for_pick.clone();
        let catalog_id = catalog_id_for_pick.clone();
        let artist = artist_for_pick.clone();
        let title = title_for_pick.clone();
        move |evt: MouseEvent| {
            evt.stop_propagation();
            match &stage {
                Some(s) if s.is_in_flight() => {}
                Some(AcquireStageBadge::NeedsManualChoice { .. }) => {
                    if let Some((search_id, groups)) = manual_choice.clone() {
                        on_pick.call(ManualChoiceModalState {
                            recording_mbid: mbid.clone(),
                            catalog_id: catalog_id.clone(),
                            artist: artist.clone(),
                            title: title.clone(),
                            search_id,
                            groups,
                        });
                    }
                }
                _ => on_acquire.call(()),
            }
        }
    };

    rsx! {
        div {
            class: "relative bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default rounded-[12px] transition-colors duration-150 group flex items-center gap-3 px-3 py-2.5",

            // Cover art with 1-tap Acquire & Play surface
            button {
                r#type: "button",
                class: "relative w-11 h-11 shrink-0 bg-beet-surface-low border border-beet-border-subtle rounded-[6px] flex items-center justify-center overflow-hidden cursor-pointer disabled:cursor-default",
                aria_label: "Acquire and play {track.title}",
                disabled: in_flight,
                onclick: on_art_click,
                if let Some(url) = active_cover_url {
                    {
                        let failed_target = url.clone();
                        rsx! {
                            img {
                                key: "{url}",
                                src: "{url}",
                                alt: "",
                                class: "w-full h-full object-cover",
                                onerror: move |_| failed_url.set(Some(failed_target.clone())),
                            }
                        }
                    }
                } else {
                    IconMusic { size: 18, class: "text-beet-text-disabled".to_string() }
                }
                if !in_flight {
                    div { class: "absolute inset-0 bg-beet-canvas/65 opacity-0 group-hover:opacity-100 transition-opacity flex items-center justify-center text-beet-leaf-text",
                        IconPlay { size: 15, filled: true }
                    }
                }
            }

            // Track title, variant badge, clickable artist & album
            div { class: "flex-grow min-w-0",
                div { class: "flex items-center gap-2 min-w-0",
                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate group-hover:text-beet-leaf-text transition-colors",
                        "{track.title}"
                    }
                    if let Some(ref variant) = track.variant_label {
                        span { class: "shrink-0 px-1.5 py-0.5 rounded-[5px] bg-beet-surface-highest border border-beet-border-subtle tabular-nums-mono text-[10px] font-medium text-beet-text-secondary",
                            "{variant}"
                        }
                    }
                }
                div { class: "flex items-center gap-1.5 min-w-0 text-[13px] text-beet-text-secondary",
                    button {
                        r#type: "button",
                        class: "truncate hover:text-beet-leaf-text transition-colors cursor-pointer text-left",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            props.on_artist_click.call(());
                        },
                        "{track.artist}"
                    }
                    if let Some(ref album_title) = track.album_title {
                        span { class: "text-beet-text-disabled shrink-0", "·" }
                        if track.album_id.is_some() {
                            button {
                                r#type: "button",
                                class: "text-[12px] text-beet-text-tertiary truncate hover:text-beet-leaf-text transition-colors cursor-pointer text-left",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    props.on_album_click.call(());
                                },
                                "{album_title}"
                            }
                        } else {
                            span { class: "text-[12px] text-beet-text-tertiary truncate",
                                "{album_title}"
                            }
                        }
                    }
                }
            }

            // Duration
            if let Some(dur) = duration {
                span { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary whitespace-nowrap shrink-0 hidden sm:block",
                    "{dur}"
                }
            }

            // Actions: Acquire & Play, Search sources, Auto-download
            div { class: "flex items-center gap-1 shrink-0",
                AcquireActionPill {
                    stage,
                    manual_choice,
                    recording_mbid: mbid_for_pick,
                    catalog_id: catalog_id_for_pick,
                    artist: artist_for_pick,
                    title: title_for_pick,
                    compact: false,
                    on_acquire: props.on_acquire_and_play,
                    on_pick_source: props.on_pick_acquire_source,
                }

                // Search sources button
                button {
                    r#type: "button",
                    class: "p-2 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
                    aria_label: "Search sources",
                    title: "Search sources",
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        props.on_search_sources.call(());
                    },
                    IconSearch {
                        size: 15,
                        class: "text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors".to_string(),
                    }
                }

                // Auto-download button (with folder override menu)
                DownloadIcon {
                    item_id: props.track.id.clone(),
                    state: props.download_state.clone(),
                    folders: props.folders.clone(),
                    selected_folder_id: props.selected_folder_id.clone(),
                    active_menu: props.active_menu,
                    on_download: props.on_download,
                    on_override_download: props.on_override_download,
                }
            }

            // Seam-light while Acquire & Play is active
            if in_flight {
                span {
                    class: "beet-seam-light absolute inset-x-3 bottom-0 rounded-full",
                    aria_hidden: "true",
                }
            }
        }
    }
}

#[derive(Props, PartialEq, Clone)]
struct AcquireActionPillProps {
    stage: Option<AcquireStageBadge>,
    manual_choice: Option<(String, Vec<shared::download::DownloadableGroup>)>,
    recording_mbid: Option<String>,
    catalog_id: Option<String>,
    artist: String,
    title: String,
    compact: bool,
    on_acquire: EventHandler<()>,
    on_pick_source: EventHandler<ManualChoiceModalState>,
}

#[component]
fn AcquireActionPill(props: AcquireActionPillProps) -> Element {
    let h_cls = if props.compact { "h-7 px-2" } else { "h-8 px-2.5" };
    match &props.stage {
        Some(stage) if stage.is_in_flight() => rsx! {
            span {
                class: "inline-flex items-center gap-1.5 {h_cls} rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide shrink-0",
                IconLoader { size: 12, class: "animate-spin".to_string() }
                span { "{stage.badge_label()}" }
            }
        },
        Some(AcquireStageBadge::NeedsManualChoice { message }) => {
            let tip = message
                .clone()
                .unwrap_or_else(|| "Pick a source".to_string());
            let manual_choice = props.manual_choice.clone();
            let mbid = props.recording_mbid.clone();
            let catalog_id = props.catalog_id.clone();
            let artist = props.artist.clone();
            let title = props.title.clone();
            rsx! {
                button {
                    r#type: "button",
                    title: "{tip}",
                    class: "inline-flex items-center gap-1.5 {h_cls} rounded-full bg-beet-accent-surface border border-beet-accent/40 text-beet-accent-text hover:bg-beet-accent-solid hover:text-beet-text-primary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide transition-colors cursor-pointer shrink-0",
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        if let Some((search_id, groups)) = manual_choice.clone() {
                            props.on_pick_source.call(ManualChoiceModalState {
                                recording_mbid: mbid.clone(),
                                catalog_id: catalog_id.clone(),
                                artist: artist.clone(),
                                title: title.clone(),
                                search_id,
                                groups,
                            });
                        }
                    },
                    IconSlidersHorizontal { size: 12 }
                    span { "Pick source" }
                }
            }
        }
        Some(AcquireStageBadge::Failed { message }) => {
            let tip = message
                .clone()
                .unwrap_or_else(|| "Retry Acquire & Play".to_string());
            rsx! {
                button {
                    r#type: "button",
                    title: "{tip}",
                    class: "inline-flex items-center gap-1.5 {h_cls} rounded-full bg-beet-crimson-surface border border-beet-crimson-text/35 text-beet-crimson-text hover:brightness-110 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide transition-colors cursor-pointer shrink-0",
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        props.on_acquire.call(());
                    },
                    IconRefreshCw { size: 12 }
                    span { "Retry" }
                }
            }
        }
        _ => rsx! {
            button {
                r#type: "button",
                aria_label: "Acquire and play",
                title: "Acquire & Play",
                class: "inline-flex items-center gap-1.5 {h_cls} rounded-full bg-beet-leaf-surface border border-beet-leaf/25 text-beet-leaf-text hover:bg-beet-leaf-solid hover:text-beet-leaf-ink text-[11px] font-semibold transition-colors cursor-pointer shrink-0",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    props.on_acquire.call(());
                },
                IconPlay { size: 11, filled: true }
                if !props.compact {
                    span { class: "hidden sm:inline", "Play" }
                }
            }
        },
    }
}

#[derive(Props, PartialEq, Clone)]
struct CatalogAlbumRowProps {
    album: CatalogAlbum,
    is_expanded: bool,
    detail: Option<CatalogAlbumDetail>,
    download_state: DownloadRowState,
    download_states: Signal<HashMap<String, DownloadRowState>>,
    acquire_map: HashMap<String, AcquireTrackState>,
    folders: Vec<Folder>,
    selected_folder_id: Option<String>,
    active_menu: Signal<Option<String>>,
    on_toggle: EventHandler<()>,
    on_artist_click: EventHandler<()>,
    on_search_sources: EventHandler<()>,
    on_download: EventHandler<()>,
    on_override_download: EventHandler<Folder>,
    on_track_acquire: EventHandler<CatalogTrack>,
    on_track_pick_acquire: EventHandler<ManualChoiceModalState>,
    on_track_search_sources: EventHandler<CatalogTrack>,
    on_track_download: EventHandler<CatalogTrack>,
    on_track_override_download: EventHandler<(CatalogTrack, Folder)>,
}

#[component]
fn CatalogAlbumRow(props: CatalogAlbumRowProps) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let album = &props.album;
    let facts = catalog_album_facts(album);
    let has_folders = !props.folders.is_empty();
    let active_cover_url = album
        .cover_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty() && failed_url.read().as_deref() != Some(*u))
        .map(str::to_string);

    rsx! {
        div { class: "space-y-0",
            // Album header row (keep overflow visible for DownloadIcon FolderDropdown)
            div {
                class: "bg-beet-surface hover:bg-beet-surface-high border transition-colors duration-150 flex items-center gap-3 p-3 group cursor-pointer",
                class: if props.is_expanded {
                    "rounded-t-[12px] border-beet-accent/40"
                } else {
                    "rounded-[12px] border-beet-border-subtle hover:border-beet-border-default"
                },
                onclick: move |_| props.on_toggle.call(()),

                // Cover art
                div { class: "w-14 h-14 shrink-0 rounded-[8px] overflow-hidden bg-beet-surface-low border border-beet-border-subtle flex items-center justify-center",
                    if let Some(url) = active_cover_url {
                        {
                            let failed_target = url.clone();
                            rsx! {
                                img {
                                    key: "{url}",
                                    src: "{url}",
                                    alt: "",
                                    class: "w-full h-full object-cover",
                                    onerror: move |_| failed_url.set(Some(failed_target.clone())),
                                }
                            }
                        }
                    } else {
                        IconDisc3 { size: 22, class: "text-beet-text-disabled".to_string() }
                    }
                }

                // Album info
                div { class: "flex-grow min-w-0",
                    p { class: "text-[15px] font-semibold text-beet-text-primary truncate group-hover:text-beet-leaf-text transition-colors",
                        "{album.title}"
                    }
                    button {
                        r#type: "button",
                        class: "text-[13px] text-beet-text-secondary hover:text-beet-leaf-text transition-colors truncate block max-w-full text-left cursor-pointer",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            props.on_artist_click.call(());
                        },
                        "{album.artist}"
                    }
                    if let Some(f) = facts {
                        p { class: "text-[12px] text-beet-text-tertiary tabular-nums-mono mt-0.5 truncate",
                            "{f}"
                        }
                    }
                }

                // Chevron indicator
                svg {
                    class: "w-4 h-4 transition-transform duration-200 shrink-0",
                    class: if props.is_expanded {
                        "rotate-90 text-beet-accent-text"
                    } else {
                        "text-beet-text-tertiary group-hover:text-beet-text-primary"
                    },
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        d: "M8.25 4.5l7.5 7.5-7.5 7.5",
                    }
                }

                // Action buttons: Search sources + Download Album
                div { class: "flex items-center gap-1 shrink-0",
                    button {
                        r#type: "button",
                        class: "p-2 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
                        aria_label: "Search sources",
                        title: "Search sources",
                        onclick: move |evt: MouseEvent| {
                            evt.stop_propagation();
                            props.on_search_sources.call(());
                        },
                        IconSearch {
                            size: 15,
                            class: "text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors".to_string(),
                        }
                    }

                    DownloadIcon {
                        item_id: props.album.id.clone(),
                        state: props.download_state.clone(),
                        folders: props.folders.clone(),
                        selected_folder_id: props.selected_folder_id.clone(),
                        active_menu: props.active_menu,
                        on_download: props.on_download,
                        on_override_download: props.on_override_download,
                    }
                }
            }

            // Expandable Tracklist Panel
            if props.is_expanded {
                div { class: "border-x border-b border-beet-accent/40 rounded-b-[12px] bg-beet-canvas/60 px-3 py-2.5",
                    match &props.detail {
                        None => rsx! {
                            div { class: "flex items-center justify-center py-5 gap-2.5 text-beet-text-tertiary",
                                IconLoader { size: 16, class: "animate-spin text-beet-leaf-text".to_string() }
                                span { class: "tabular-nums-mono text-[11px]", "Loading tracklist..." }
                            }
                        },
                        Some(detail) if detail.tracks.is_empty() => rsx! {
                            p { class: "text-center text-[12px] text-beet-text-tertiary py-4",
                                "No tracklist available for this release."
                            }
                        },
                        Some(detail) => rsx! {
                            ul { class: "list-none p-0 divide-y divide-beet-border-subtle/60",
                                for (idx, track) in detail.tracks.iter().enumerate() {
                                    {
                                        let track_acq = track.clone();
                                        let track_src = track.clone();
                                        let track_dl = track.clone();
                                        let track_ov = track.clone();
                                        let dl_state = props
                                            .download_states
                                            .read()
                                            .get(&track.id)
                                            .cloned()
                                            .unwrap_or_default();
                                        let eff_state = if !has_folders {
                                            DownloadRowState::Disabled
                                        } else {
                                            dl_state
                                        };
                                        let key = acquire_key(
                                            track.mbid.as_deref(),
                                            Some(&track.id),
                                            &track.artist,
                                            &track.title,
                                        );
                                        let acq_entry = props.acquire_map.get(&key).cloned();
                                        let stage = acq_entry.as_ref().map(|e| e.badge.clone());
                                        let manual_choice = acq_entry.and_then(|e| e.manual_choice);
                                        let in_flight = stage.as_ref().is_some_and(AcquireStageBadge::is_in_flight);
                                        let dur = format_catalog_duration(track.duration.as_deref(), track.duration_secs);

                                        rsx! {
                                            li {
                                                key: "alb-trk-{track.id}-{idx}",
                                                class: "relative flex items-center gap-3 py-2 px-1.5 rounded-[8px] hover:bg-beet-surface/70 transition-colors",
                                                span { class: "w-5 text-right tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                                    "{idx + 1}"
                                                }
                                                div { class: "flex-grow min-w-0 flex items-center gap-2",
                                                    span { class: "text-[13px] text-beet-text-primary truncate",
                                                        "{track.title}"
                                                    }
                                                    if let Some(ref variant) = track.variant_label {
                                                        span { class: "shrink-0 px-1.5 py-0.5 rounded-[4px] bg-beet-surface-highest border border-beet-border-subtle tabular-nums-mono text-[9px] text-beet-text-secondary",
                                                            "{variant}"
                                                        }
                                                    }
                                                }
                                                if let Some(d) = dur {
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0 hidden sm:block",
                                                        "{d}"
                                                    }
                                                }
                                                div { class: "flex items-center gap-0.5 shrink-0",
                                                    AcquireActionPill {
                                                        stage,
                                                        manual_choice,
                                                        recording_mbid: track.mbid.clone(),
                                                        catalog_id: Some(track.id.clone()),
                                                        artist: track.artist.clone(),
                                                        title: track.title.clone(),
                                                        compact: true,
                                                        on_acquire: move |_| props.on_track_acquire.call(track_acq.clone()),
                                                        on_pick_source: props.on_track_pick_acquire,
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        class: "p-1.5 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
                                                        aria_label: "Search sources",
                                                        title: "Search sources",
                                                        onclick: move |evt: MouseEvent| {
                                                            evt.stop_propagation();
                                                            props.on_track_search_sources.call(track_src.clone());
                                                        },
                                                        IconSearch {
                                                            size: 13,
                                                            class: "text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors".to_string(),
                                                        }
                                                    }
                                                    DownloadIcon {
                                                        item_id: track.id.clone(),
                                                        state: eff_state,
                                                        folders: props.folders.clone(),
                                                        selected_folder_id: props.selected_folder_id.clone(),
                                                        active_menu: props.active_menu,
                                                        on_download: move |_| props.on_track_download.call(track_dl.clone()),
                                                        on_override_download: move |folder: Folder| {
                                                            props.on_track_override_download.call((track_ov.clone(), folder));
                                                        },
                                                    }
                                                }
                                                if in_flight {
                                                    span {
                                                        class: "beet-seam-light absolute inset-x-2 bottom-0 rounded-full",
                                                        aria_hidden: "true",
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog_track(
        artist: &str,
        title: &str,
        album: Option<&str>,
        variant_label: Option<&str>,
    ) -> CatalogTrack {
        CatalogTrack {
            id: format!("deezer:track:{artist}:{title}"),
            title: title.to_string(),
            artist: artist.to_string(),
            artist_id: None,
            album_id: None,
            album_title: album.map(str::to_string),
            release_date: None,
            duration: Some("3:45".to_string()),
            duration_secs: Some(225),
            mbid: None,
            release_mbid: None,
            cover_url: None,
            variant_label: variant_label.map(str::to_string),
            provider: CatalogProvider::Deezer,
        }
    }

    #[test]
    fn dedup_hides_owned_studio_track_across_accents_and_remaster_tags() {
        let mut idx = LocalTrackIndex::default();
        idx.insert("Blue Oyster Cult", "Don't Fear the Reaper", Some("Agents of Fortune"));

        let remote_remaster = sample_catalog_track(
            "Blue Öyster Cult",
            "(Don’t Fear) The Reaper (Remastered 2001)",
            Some("Agents of Fortune"),
            None,
        );
        assert!(idx.matches_remote(&remote_remaster));
    }

    #[test]
    fn dedup_keeps_non_studio_variants_when_only_studio_is_owned() {
        let mut idx = LocalTrackIndex::default();
        idx.insert("Serj Tankian", "Empty Walls", Some("Elect the Dead"));

        let remote_acoustic = sample_catalog_track(
            "Serj Tankian",
            "Empty Walls (Acoustic)",
            Some("Elect the Dead"),
            Some("Acoustic"),
        );
        assert!(!idx.matches_remote(&remote_acoustic));

        let remote_live = sample_catalog_track(
            "Serj Tankian",
            "Empty Walls (Live)",
            Some("Live in Concert"),
            None,
        );
        assert!(!idx.matches_remote(&remote_live));
    }

    #[test]
    fn dedup_hides_owned_album_with_deluxe_suffix() {
        let local = vec![SubsonicAlbumSummary {
            id: "alb-1".into(),
            name: "Agents of Fortune".into(),
            artist: "Blue Oyster Cult".into(),
            song_count: 10,
            year: Some(1976),
            cover_art_url: None,
        }];
        let keys = build_local_album_index(&local);
        let remote = CatalogAlbum {
            id: "deezer:album:123".into(),
            title: "Agents of Fortune (Deluxe Edition)".into(),
            artist: "Blue Öyster Cult".into(),
            artist_id: None,
            release_date: Some("1976-05-21".into()),
            first_release_date: None,
            cover_url: None,
            mbid: None,
            release_types: Some(vec!["Album".into()]),
            track_count: Some(14),
            provider: CatalogProvider::Deezer,
        };
        assert!(is_remote_album_owned(&remote, &keys));
    }

    #[test]
    fn formats_fan_counts_and_album_facts_cleanly() {
        assert_eq!(format_fan_count(1_250_000), "1.2M fans");
        assert_eq!(format_fan_count(42_500), "42.5K fans");
        assert_eq!(format_fan_count(1), "1 fan");
        assert_eq!(format_fan_count(320), "320 fans");

        let alb = CatalogAlbum {
            id: "deezer:album:1".into(),
            title: "Discovery".into(),
            artist: "Daft Punk".into(),
            artist_id: None,
            release_date: Some("2001-03-12".into()),
            first_release_date: None,
            cover_url: None,
            mbid: None,
            release_types: Some(vec!["Album".into()]),
            track_count: Some(14),
            provider: CatalogProvider::Deezer,
        };
        assert_eq!(
            catalog_album_facts(&alb).as_deref(),
            Some("2001 · Album · 14 tracks")
        );
    }

    #[test]
    fn combines_prefill_query_and_extracts_initials() {
        assert_eq!(combine_prefill_query("Daft Punk", "One More Time"), "Daft Punk One More Time");
        assert_eq!(combine_prefill_query("  Daft Punk ", ""), "Daft Punk");
        assert_eq!(combine_prefill_query("", "Discovery"), "Discovery");
        assert_eq!(artist_initials("Daft Punk"), "DP");
        assert_eq!(artist_initials("Enya"), "E");
    }

    #[test]
    fn triage_query_matches_normalized_multi_word_queries() {
        assert!(matches_triage_query(
            "Blue Oyster Reaper",
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            "Agents of Fortune",
        ));
        assert!(matches_triage_query(
            "don't fear fortune",
            "Blue Öyster Cult",
            "(Don’t Fear) The Reaper",
            "Agents of Fortune",
        ));
        assert!(!matches_triage_query(
            "Blue Oyster Godzilla",
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            "Agents of Fortune",
        ));
        assert!(!matches_triage_query(
            "   ",
            "Blue Öyster Cult",
            "(Don't Fear) The Reaper",
            "Agents of Fortune",
        ));
    }
}
