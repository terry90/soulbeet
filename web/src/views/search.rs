//! Unified Multi-Backend Studio Search: instant local library & discovery matches
//! alongside MusicBrainz track/album/discography search, 1-tap `Acquire & Play`,
//! auto-download across all enabled backends, and a parallel multi-backend manual
//! source picker (`Download Options`).

use std::collections::{HashMap, HashSet};

use api::models::folder::Folder;
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::backends::BackendMeta;
use shared::download::{AcquireAndPlayRequest, AutoDownloadEvent, DownloadQuery};
use shared::metadata::{AlbumWithTracks, SearchResult, Track};
use shared::recommendation::ContextualMixKind;
use ui::{
    album_query, discography_artist, empty_message, format_mix_title, friendly_error, listed_from,
    run_multi_backend_source_search, submit_multi_backend_download, use_player, use_settings,
    use_system_health, AlbumResult, AutoDownloadSignal, DirectSearchEscape, DiscographyHeading,
    DownloadResults, DownloadRowState, FallbackToast, FallbackToastData, IconDisc3, IconPlay,
    IconWifiOff, Listed, PlayerTrack, SearchBar, SearchPrefill, SearchReset, SearchType,
    SourceChoices, SystemStatus, TrackResult, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{fetch_album_tracks, search_navidrome, NavidromeSearchHits};
use crate::web_player::{acquire_key, use_studio, ManualChoiceModalState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchScope {
    All,
    MyLibrary,
    FindAndDownload,
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

    let mut scope = use_signal(|| SearchScope::All);
    let mut search = use_signal(String::new);
    let mut artist = use_signal::<Option<String>>(|| None);
    let mut artist_open = use_signal(|| false);
    let mut search_type = use_signal(|| settings.last_search_type());
    let mut loading = use_signal(|| false);
    let mut search_req_id = use_signal(|| 0u64);
    let mut has_searched = use_signal(|| false);
    let mut search_error = use_signal(String::new);

    let mut local_hits = use_signal(NavidromeSearchHits::default);
    let mut catalog_results = use_signal::<Option<Vec<SearchResult>>>(|| None);
    let mut listed = use_signal::<Option<Listed>>(|| None);
    let mut expanded_albums = use_signal::<HashSet<String>>(HashSet::new);
    let mut album_cache = use_signal::<HashMap<String, AlbumWithTracks>>(HashMap::new);

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

    let mut synced = use_signal(|| false);

    // Sync last search type from user settings once loaded
    use_effect(move || {
        if settings.is_loaded() && !synced() {
            search_type.set(settings.last_search_type());
            synced.set(true);
        }
    });

    // Persist search type changes to user settings
    use_effect(move || {
        let current_type = search_type();
        if synced() {
            spawn(async move {
                let _ = settings.set_last_search_type(current_type).await;
            });
        }
    });

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

    // Honor SearchReset context
    use_effect(move || {
        if let Some(reset) = search_reset {
            if reset.0() > 0 {
                let next_req = search_req_id.peek().wrapping_add(1);
                search_req_id.set(next_req);
                let next_gen = source_search_gen.peek().wrapping_add(1);
                source_search_gen.set(next_gen);
                has_searched.set(false);
                catalog_results.set(None);
                local_hits.set(NavidromeSearchHits::default());
                listed.set(None);
                search_error.set(String::new());
                download_error.set(String::new());
                download_options.set(None);
                search.set(String::new());
                artist.set(None);
                artist_open.set(false);
                expanded_albums.write().clear();
                album_cache.write().clear();
                loading.set(false);
                source_searching.set(false);
            }
        }
    });

    // Route AutoDownloadEvents from WebSocket to per-row download states
    use_effect(move || {
        let Some(mut ctx) = auto_dl else { return };
        let Some(event) = (ctx.0)() else { return };
        (ctx.0).set(None);

        let mut events_to_process = pending_events.peek().clone();
        events_to_process.push(event);
        let mut still_pending = Vec::new();

        for event in events_to_process {
            let batch_id = match &event {
                AutoDownloadEvent::Searching { batch_id, .. }
                | AutoDownloadEvent::ScoringResults { batch_id, .. }
                | AutoDownloadEvent::PickedSource { batch_id, .. }
                | AutoDownloadEvent::Downloading { batch_id }
                | AutoDownloadEvent::FallbackToManual { batch_id, .. }
                | AutoDownloadEvent::Failed { batch_id, .. } => batch_id.clone(),
            };

            let item_id = batch_to_item.peek().get(&batch_id).cloned();
            let Some(item_id) = item_id else {
                still_pending.push(event);
                continue;
            };

            match event {
                AutoDownloadEvent::Searching { .. } | AutoDownloadEvent::ScoringResults { .. } => {
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Searching);
                    if let Some(cached) = album_cache.peek().get(&item_id) {
                        let mut states = download_states.write();
                        for track in &cached.tracks {
                            states.insert(track.id.clone(), DownloadRowState::Searching);
                        }
                    }
                }
                AutoDownloadEvent::PickedSource { .. } | AutoDownloadEvent::Downloading { .. } => {
                    let item_id_timer = item_id.clone();
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Done);
                    if let Some(cached) = album_cache.peek().get(&item_id) {
                        let mut states = download_states.write();
                        for track in &cached.tracks {
                            states.insert(track.id.clone(), DownloadRowState::Done);
                        }
                    }
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(5000).await;
                        let current = download_states.peek().get(&item_id_timer).cloned();
                        if matches!(current, Some(DownloadRowState::Done)) {
                            download_states
                                .write()
                                .insert(item_id_timer, DownloadRowState::Idle);
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
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Idle);
                    let track_name = batch_to_name
                        .peek()
                        .get(&batch_id)
                        .cloned()
                        .unwrap_or(item_id.clone());
                    let toast_id = batch_id.clone();
                    fallback_toasts.write().push(FallbackToastData {
                        id: batch_id,
                        track_name,
                        search_id,
                        results,
                        reason,
                    });
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(8000).await;
                        fallback_toasts.write().retain(|t| t.id != toast_id);
                    });
                }
                AutoDownloadEvent::Failed { failure, .. } => {
                    download_states
                        .write()
                        .insert(item_id, DownloadRowState::Failed(failure.message));
                }
            }
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
            let batch_id = match &event {
                AutoDownloadEvent::Searching { batch_id, .. }
                | AutoDownloadEvent::ScoringResults { batch_id, .. }
                | AutoDownloadEvent::PickedSource { batch_id, .. }
                | AutoDownloadEvent::Downloading { batch_id }
                | AutoDownloadEvent::FallbackToManual { batch_id, .. }
                | AutoDownloadEvent::Failed { batch_id, .. } => batch_id.clone(),
            };
            let item_id = batch_to_item.peek().get(&batch_id).cloned();
            let Some(item_id) = item_id else {
                still_pending.push(event);
                continue;
            };
            match event {
                AutoDownloadEvent::Searching { .. } | AutoDownloadEvent::ScoringResults { .. } => {
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Searching);
                    if let Some(cached) = album_cache.peek().get(&item_id) {
                        let mut states = download_states.write();
                        for track in &cached.tracks {
                            states.insert(track.id.clone(), DownloadRowState::Searching);
                        }
                    }
                }
                AutoDownloadEvent::PickedSource { .. } | AutoDownloadEvent::Downloading { .. } => {
                    let item_id_timer = item_id.clone();
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Done);
                    if let Some(cached) = album_cache.peek().get(&item_id) {
                        let mut states = download_states.write();
                        for track in &cached.tracks {
                            states.insert(track.id.clone(), DownloadRowState::Done);
                        }
                    }
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(5000).await;
                        let current = download_states.peek().get(&item_id_timer).cloned();
                        if matches!(current, Some(DownloadRowState::Done)) {
                            download_states
                                .write()
                                .insert(item_id_timer, DownloadRowState::Idle);
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
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Idle);
                    let track_name = batch_to_name
                        .peek()
                        .get(&batch_id)
                        .cloned()
                        .unwrap_or(item_id.clone());
                    let toast_id = batch_id.clone();
                    fallback_toasts.write().push(FallbackToastData {
                        id: batch_id,
                        track_name,
                        search_id,
                        results,
                        reason,
                    });
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(8000).await;
                        fallback_toasts.write().retain(|t| t.id != toast_id);
                    });
                }
                AutoDownloadEvent::Failed { failure, .. } => {
                    download_states
                        .write()
                        .insert(item_id, DownloadRowState::Failed(failure.message));
                }
            }
        }
        pending_events.set(still_pending);
    });

    let list_discography = move |name: String, chosen: Option<String>| async move {
        let next_req = search_req_id.peek().wrapping_add(1);
        search_req_id.set(next_req);
        let next_gen = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_gen);

        download_error.set(String::new());
        download_options.set(None);
        source_searching.set(false);
        search_error.set(String::new());
        has_searched.set(true);
        loading.set(true);

        let is_offline = player.offline_only();
        let nav_info = studio.navidrome_info();

        if is_offline {
            local_hits.set(NavidromeSearchHits::default());
            catalog_results.set(None);
            listed.set(None);
            if *search_req_id.peek() == next_req {
                loading.set(false);
            }
            return;
        }

        if let Some(info) = nav_info {
            if let Ok(hits) = search_navidrome(&info, &name).await {
                if *search_req_id.peek() == next_req {
                    local_hits.set(hits);
                }
            }
        } else if *search_req_id.peek() == next_req {
            local_hits.set(NavidromeSearchHits::default());
        }

        let found = auth
            .call(api::artist_discography(api::DiscographyQuery {
                artist: name.clone(),
                mbid: chosen,
            }))
            .await;

        if *search_req_id.peek() == next_req {
            match found {
                Ok(found) => {
                    let (listing, rows) = listed_from(name, found);
                    listed.set(Some(listing));
                    catalog_results.set(Some(rows));
                }
                Err(e) => search_error.set(friendly_error(&e)),
            }
            loading.set(false);
        }
    };

    let perform_search = move || async move {
        let q = search.peek().clone();
        let art = artist.peek().clone();
        if let Some(name) = discography_artist(art.clone(), &q) {
            return list_discography(name, None).await;
        }

        let trimmed_q = q.trim().to_string();
        if trimmed_q.is_empty() {
            has_searched.set(false);
            loading.set(false);
            local_hits.set(NavidromeSearchHits::default());
            catalog_results.set(None);
            listed.set(None);
            search_error.set(String::new());
            return;
        }

        let next_req = search_req_id.peek().wrapping_add(1);
        search_req_id.set(next_req);
        let next_gen = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_gen);

        download_error.set(String::new());
        download_options.set(None);
        source_searching.set(false);
        search_error.set(String::new());
        has_searched.set(true);
        loading.set(true);

        let is_offline = player.offline_only();
        if is_offline {
            local_hits.set(NavidromeSearchHits::default());
            catalog_results.set(None);
            listed.set(None);
            if *search_req_id.peek() == next_req {
                loading.set(false);
            }
            return;
        }

        let nav_info = studio.navidrome_info();
        let active_scope = *scope.peek();
        let local_query = match art.as_deref() {
            Some(a) if !a.trim().is_empty() => format!("{} {trimmed_q}", a.trim()),
            _ => trimmed_q.clone(),
        };

        // 1. Local Navidrome search3
        if let Some(info) = nav_info {
            match search_navidrome(&info, &local_query).await {
                Ok(hits) => {
                    if *search_req_id.peek() == next_req {
                        local_hits.set(hits);
                    }
                }
                Err(e) => {
                    if *search_req_id.peek() == next_req {
                        local_hits.set(NavidromeSearchHits::default());
                        if active_scope == SearchScope::MyLibrary {
                            search_error.set(e);
                        }
                    }
                }
            }
        } else if *search_req_id.peek() == next_req {
            local_hits.set(NavidromeSearchHits::default());
        }

        // 2. MusicBrainz catalog search (respects ALBUM / TRACK radio)
        let query_data = api::SearchQuery {
            artist: art,
            query: q,
        };
        let selected_type = *search_type.peek();
        let mb_result = match selected_type {
            SearchType::Album => auth.call(api::search_album(query_data)).await,
            SearchType::Track => auth.call(api::search_track(query_data)).await,
        };

        if *search_req_id.peek() == next_req {
            match mb_result {
                Ok(data) => {
                    listed.set(None);
                    catalog_results.set(Some(data));
                }
                Err(e) => search_error.set(friendly_error(&e)),
            }
            loading.set(false);
        }
    };

    // Honor Command Palette "Search for <query>" jumps automatically
    use_effect(move || {
        if let Some(incoming) = (studio.search_query_override)() {
            studio.search_query_override.set(None);
            artist.set(None);
            artist_open.set(false);
            search.set(incoming);
            scope.set(SearchScope::All);
            spawn(perform_search());
        }
    });

    // Honor SearchPrefill context
    use_effect(move || {
        if let Some(mut prefill) = search_prefill {
            if let Some((prefill_artist, prefill_query)) = (prefill.0)() {
                artist.set(Some(prefill_artist));
                search.set(prefill_query);
                (prefill.0).set(None);
                spawn(perform_search());
            }
        }
    });

    let mut start_auto_download = move |item_id: String, query: DownloadQuery, folder: Folder| {
        download_states
            .write()
            .insert(item_id.clone(), DownloadRowState::Searching);

        if expanded_albums.read().contains(&item_id) {
            if let Some(cached) = album_cache.read().get(&item_id) {
                let mut states = download_states.write();
                for track in &cached.tracks {
                    states.insert(track.id.clone(), DownloadRowState::Searching);
                }
            }
        }

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
                    download_states
                        .write()
                        .insert(item_id, DownloadRowState::Failed(failure.message));
                }
                Err(e) => {
                    download_states
                        .write()
                        .insert(item_id, DownloadRowState::Failed(e.to_string()));
                }
            }
        });
    };

    let mut handle_auto_download = move |item_id: String, query: DownloadQuery| {
        let folder_id = selected_folder_id();
        let folder = folder_id
            .as_ref()
            .and_then(|fid| folders.read().iter().find(|f| f.id == *fid).cloned());
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

    let mut toggle_expand = move |album_id: String| {
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
                        .call(api::find_album(api::AlbumQuery { id: aid.clone() }))
                        .await
                    {
                        Ok(album_data) => {
                            album_cache.write().insert(aid, album_data);
                        }
                        Err(e) => {
                            info!("Failed to fetch album details for {}: {:?}", aid, e);
                            expanded_albums.write().remove(&aid);
                        }
                    }
                });
            }
        }
    };

    let mut handle_track_download = move |track: Track| {
        let folder_id = selected_folder_id();
        let folder = folder_id
            .as_ref()
            .and_then(|fid| folders.read().iter().find(|f| f.id == *fid).cloned());
        let Some(folder) = folder else {
            return;
        };
        start_auto_download(track.id.clone(), DownloadQuery::from(track), folder);
    };

    let mut handle_track_override_download = move |(track, folder): (Track, Folder)| {
        start_auto_download(track.id.clone(), DownloadQuery::from(track), folder);
    };

    let offline_only = player.offline_only();
    let effective_scope = if offline_only && scope() == SearchScope::FindAndDownload {
        SearchScope::MyLibrary
    } else {
        scope()
    };

    let q_lower = {
        let q = search();
        let trimmed = q.trim();
        if !trimmed.is_empty() {
            trimmed.to_ascii_lowercase()
        } else {
            artist()
                .map(|a| a.trim().to_ascii_lowercase())
                .unwrap_or_default()
        }
    };
    let matching_triage: Vec<PlayerTrack> = if q_lower.is_empty() {
        Vec::new()
    } else {
        studio
            .visible_triage_tracks()
            .iter()
            .filter(|t| {
                t.title.to_ascii_lowercase().contains(&q_lower)
                    || t.artist.to_ascii_lowercase().contains(&q_lower)
                    || t.album.to_ascii_lowercase().contains(&q_lower)
            })
            .map(|t| studio.track_from_discovery(t))
            .collect()
    };

    let acquire_map = studio.acquire_states.read().clone();
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
                // Unified Search Bar + Controls Strip + System Status
                div { class: "space-y-3",
                    SearchBar {
                        search,
                        artist,
                        artist_open,
                        search_type,
                        loading: loading(),
                        folders: folders.read().clone(),
                        selected_folder_id,
                        on_folder_change: handle_folder_change,
                        active_menu,
                        show_type_toggle: !offline_only
                            && matches!(effective_scope, SearchScope::All | SearchScope::FindAndDownload),
                        placeholder_override: offline_only.then(|| "Search cached library tracks…".to_string()),
                        on_search: move |_| {
                            spawn(perform_search());
                        },
                    }

                    // Scope pills + active download backend pills
                    div { class: "flex flex-wrap items-center justify-between gap-2",
                        div { class: "flex flex-wrap items-center gap-2",
                            for (tab, label) in [
                                (SearchScope::All, "All"),
                                (SearchScope::MyLibrary, "My Library"),
                                (SearchScope::FindAndDownload, "Find & Download"),
                            ] {
                                {
                                    let active = effective_scope == tab;
                                    let disabled_offline = offline_only && tab == SearchScope::FindAndDownload;
                                    let cls = if active {
                                        "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                                    } else {
                                        "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary"
                                    };
                                    rsx! {
                                        button {
                                            key: "{label}",
                                            r#type: "button",
                                            disabled: disabled_offline,
                                            class: "h-8 px-3.5 rounded-full border text-[12px] font-medium transition-colors disabled:opacity-40 cursor-pointer {cls}",
                                            onclick: move |_| scope.set(tab),
                                            "{label}"
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

                if !has_searched() {
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
                                    "Search albums, tracks, or full artist discographies to play instantly or download."
                                }
                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    "ACTIVE SOURCES · {backend_summary}"
                                }
                            }
                        }
                    }
                } else {
                    // 1. In Library & Discovery Matches
                    if matches!(effective_scope, SearchScope::All | SearchScope::MyLibrary) {
                        {
                            let mut hits = local_hits.read().clone();
                            for s in &mut hits.songs {
                                studio.apply_curation_overrides(s);
                            }
                            let has_local = !hits.songs.is_empty()
                                || !hits.albums.is_empty()
                                || !matching_triage.is_empty();
                            let show_section = has_local || effective_scope == SearchScope::MyLibrary || offline_only;
                            if show_section {
                                rsx! {
                                    section { class: "space-y-3",
                                        div { class: "flex items-center justify-between",
                                            h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                                                "In Your Library"
                                            }
                                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                "{hits.songs.len() + matching_triage.len()} MATCHES"
                                            }
                                        }

                                        if !hits.albums.is_empty() {
                                            div { class: "grid grid-cols-2 sm:grid-cols-4 gap-3",
                                                for alb in hits.albums.iter().take(4) {
                                                    {
                                                        let aid = alb.id.clone();
                                                        rsx! {
                                                            button {
                                                                key: "srch-alb-{alb.id}",
                                                                r#type: "button",
                                                                class: "p-2.5 rounded-[10px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer space-y-2",
                                                                onclick: move |_| {
                                                                    if player.offline_only() {
                                                                        studio.show_toast("Offline Only mode is active: album streaming is unavailable.");
                                                                        return;
                                                                    }
                                                                    let Some(info) = studio.navidrome_info() else {
                                                                        return;
                                                                    };
                                                                    let aid = aid.clone();
                                                                    spawn(async move {
                                                                        if let Ok(mut tracks) = fetch_album_tracks(&info, &aid).await {
                                                                            for t in &mut tracks {
                                                                                studio.apply_curation_overrides(t);
                                                                            }
                                                                            if !tracks.is_empty() {
                                                                                player.play_queue(tracks, 0);
                                                                            }
                                                                        }
                                                                    });
                                                                },
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

                                        if hits.songs.is_empty() && matching_triage.is_empty() {
                                            p { class: "text-[13px] text-beet-text-tertiary py-2",
                                                "No matching tracks in your library yet."
                                            }
                                        } else {
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
                                                for (idx, song) in hits.songs.iter().enumerate() {
                                                    {
                                                        let all_songs = hits.songs.clone();
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
                                    }
                                }
                            } else {
                                rsx! {}
                            }
                        }
                    }

                    // 2. Find & Download (MusicBrainz Track / Album / Discography)
                    if matches!(effective_scope, SearchScope::All | SearchScope::FindAndDownload) {
                        if offline_only {
                            div { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-4 flex items-center gap-3 text-[13px] text-beet-text-secondary",
                                IconWifiOff { size: 17, class: "text-beet-leaf-text".to_string() }
                                span { "Find & Download is hidden while Offline Only mode is active." }
                            }
                        } else if loading() {
                            div { class: "flex flex-col items-center justify-center py-10 gap-3",
                                div { class: "size-9 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                                p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary",
                                    "Searching MusicBrainz…"
                                }
                            }
                        } else {
                            match (&*catalog_results.read(), listed()) {
                                (Some(ref data), listing) if !data.is_empty() || listing.as_ref().is_some_and(|l| l.artist.is_some()) => {
                                    rsx! {
                                        section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-4",
                                            if let Some(Listed { name, artist: Some(found), others, albums_complete, singles_complete, shown }) = listing.clone() {
                                                DiscographyHeading {
                                                    name: name.clone(),
                                                    artist: found,
                                                    others,
                                                    albums_complete,
                                                    singles_complete,
                                                    shown,
                                                    on_pick: move |mbid: String| {
                                                        spawn(list_discography(name.clone(), Some(mbid)));
                                                    },
                                                }
                                            } else {
                                                div { class: "flex flex-wrap items-center justify-between gap-2 border-b border-beet-border-subtle pb-3",
                                                    h2 { class: "text-[17px] font-semibold text-beet-text-primary",
                                                        "Search Results"
                                                    }
                                                    div { class: "flex flex-wrap items-center gap-2",
                                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                            "MUSICBRAINZ · {data.len()}"
                                                        }
                                                        if !backend_summary.is_empty() {
                                                            span { class: "tabular-nums-mono text-[10px] px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-secondary",
                                                                "{backend_summary}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            if data.is_empty() {
                                                p { class: "text-center text-beet-text-tertiary py-8 text-[13px]",
                                                    "{empty_message(listing.as_ref())}"
                                                }
                                            } else {
                                                ul { class: "list-none p-0 space-y-2.5",
                                                    for item in data.iter() {
                                                        match item {
                                                            SearchResult::Track(ref track) => {
                                                                let track_for_sources = track.clone();
                                                                let track_for_album = track.clone();
                                                                let track_for_dl = track.clone();
                                                                let track_for_override = track.clone();
                                                                let track_id = track.id.clone();
                                                                let dl_state = download_states.read().get(&track_id).cloned().unwrap_or_default();
                                                                let has_folders = !folders.read().is_empty();
                                                                let effective_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                                let current_folders = folders.read().clone();
                                                                let current_folder_id = selected_folder_id();

                                                                let key = acquire_key(&track.artist, &track.title);
                                                                let acq_entry = acquire_map.get(&key).cloned();
                                                                let acquiring_stage = acq_entry.as_ref().map(|e| e.badge.clone());
                                                                let manual_choice = acq_entry.and_then(|e| e.manual_choice);
                                                                let duration_secs = parse_duration_secs(track.duration.as_deref());
                                                                let acq_req = AcquireAndPlayRequest {
                                                                    artist: track.artist.clone(),
                                                                    title: track.title.clone(),
                                                                    album: track.album_title.clone(),
                                                                    recording_mbid: track.mbid.clone(),
                                                                    release_mbid: track.release_mbid.clone(),
                                                                    duration_s: duration_secs,
                                                                    folder_id: current_folder_id.clone(),
                                                                    prefer_fast: true,
                                                                };
                                                                let artist_for_pick = track.artist.clone();
                                                                let title_for_pick = track.title.clone();

                                                                rsx! {
                                                                    li { key: "{track.id}",
                                                                        TrackResult {
                                                                            track: track.clone(),
                                                                            acquiring_stage,
                                                                            on_acquire_and_play: move |_| {
                                                                                studio.trigger_acquire_and_play(auth, player, acq_req.clone());
                                                                            },
                                                                            on_pick_acquire_source: move |_| {
                                                                                if let Some((search_id, groups)) = manual_choice.clone() {
                                                                                    studio.manual_choice_modal.set(Some(ManualChoiceModalState {
                                                                                        artist: artist_for_pick.clone(),
                                                                                        title: title_for_pick.clone(),
                                                                                        search_id,
                                                                                        groups,
                                                                                    }));
                                                                                }
                                                                            },
                                                                            on_search_sources: move || {
                                                                                open_source_picker(DownloadQuery::from(track_for_sources.clone()));
                                                                            },
                                                                            on_album_click: {
                                                                                let album_id_for_expand = track_for_album.album_id.clone();
                                                                                move || {
                                                                                    if let Some(ref aid) = album_id_for_expand {
                                                                                        toggle_expand(aid.clone());
                                                                                    }
                                                                                }
                                                                            },
                                                                            download_state: effective_state,
                                                                            folders: current_folders,
                                                                            selected_folder_id: current_folder_id.clone(),
                                                                            active_menu,
                                                                            on_download: move |_| {
                                                                                handle_auto_download(
                                                                                    track_for_dl.id.clone(),
                                                                                    DownloadQuery::from(track_for_dl.clone()),
                                                                                );
                                                                            },
                                                                            on_override_download: move |folder: Folder| {
                                                                                handle_override_download(
                                                                                    track_for_override.id.clone(),
                                                                                    DownloadQuery::from(track_for_override.clone()),
                                                                                    folder,
                                                                                );
                                                                            },
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            SearchResult::Album(ref album) => {
                                                                let album_clone = album.clone();
                                                                let album_for_dl = album.clone();
                                                                let album_for_override = album.clone();
                                                                let album_id = album.id.clone();
                                                                let dl_state = download_states.read().get(&album_id).cloned().unwrap_or_default();
                                                                let has_folders = !folders.read().is_empty();
                                                                let effective_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                                                let current_folders = folders.read().clone();
                                                                let current_folder_id = selected_folder_id();

                                                                let is_expanded = expanded_albums.read().contains(&album_id);
                                                                let cached = album_cache.read().get(&album_id).cloned();
                                                                let tracks_for_panel: Option<Vec<Track>> = if is_expanded {
                                                                    cached.map(|awt| awt.tracks)
                                                                } else {
                                                                    Some(vec![])
                                                                };

                                                                rsx! {
                                                                    li { key: "{album.id}",
                                                                        AlbumResult {
                                                                            is_expanded,
                                                                            on_toggle: {
                                                                                let id = album_clone.id.clone();
                                                                                move || toggle_expand(id.clone())
                                                                            },
                                                                            on_search_sources: {
                                                                                let album_for_search = album.clone();
                                                                                move || open_source_picker(album_query(&album_for_search))
                                                                            },
                                                                            album: album.clone(),
                                                                            download_state: effective_state,
                                                                            folders: current_folders,
                                                                            selected_folder_id: current_folder_id.clone(),
                                                                            active_menu,
                                                                            on_download: move |_| {
                                                                                handle_auto_download(
                                                                                    album_for_dl.id.clone(),
                                                                                    album_query(&album_for_dl),
                                                                                );
                                                                            },
                                                                            on_override_download: move |folder: Folder| {
                                                                                handle_override_download(
                                                                                    album_for_override.id.clone(),
                                                                                    album_query(&album_for_override),
                                                                                    folder,
                                                                                );
                                                                            },
                                                                            tracks: tracks_for_panel,
                                                                            download_states,
                                                                            on_track_download: move |track: Track| {
                                                                                handle_track_download(track);
                                                                            },
                                                                            on_track_override_download: move |(track, folder): (Track, Folder)| {
                                                                                handle_track_override_download((track, folder));
                                                                            },
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
                                (Some(_), listing) => rsx! {
                                    div { class: "text-center text-beet-text-tertiary py-10 text-[13px]",
                                        p { "{empty_message(listing.as_ref())}" }
                                        if search_type() == SearchType::Track && !search().trim().is_empty() && listing.is_none() {
                                            DirectSearchEscape {
                                                query: search(),
                                                artist_hint: artist(),
                                                on_search_sources: move |q: DownloadQuery| open_source_picker(q),
                                            }
                                        }
                                    }
                                },
                                (None, _) => rsx! {},
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
