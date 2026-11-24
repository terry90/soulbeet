pub mod album;
pub mod context;
pub mod track;

pub use context::{AutoDownloadSignal, SearchPrefill, SearchReset};

mod download_icon;
pub use download_icon::{DownloadIcon, DownloadRowState};

mod download_options_menu;

mod inline_track_panel;
mod inline_track_row;

mod folder_chip;

mod search_bar;
use search_bar::SearchBar;

use api::models::folder::Folder;
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::download::{
    AutoDownloadEvent, DownloadQuery, DownloadableGroup, DownloadableItem,
    SearchState as DownloadSearchState,
};
use shared::metadata::{Album, AlbumWithTracks, SearchResult, Track};
use std::collections::{HashMap, HashSet};

use track::TrackResult;

use crate::search::album::AlbumResult;
use crate::settings_context::use_settings;
use crate::{use_auth, use_system_health, BackgroundGlows, GlowColor, GlowSpread, SystemStatus};

mod download_results;
use download_results::DownloadResults;

mod toast;
use toast::{FallbackToast, FallbackToastData};

mod search_type_toggle;
use search_type_toggle::SearchType;

/// The sources a search found, to pick files from, and the search a pick
/// names.
#[derive(Clone, PartialEq, Debug)]
pub struct SourceChoices {
    /// Empty while the search is starting, when there is nothing to pick.
    pub search_id: String,
    pub groups: Vec<DownloadableGroup>,
    /// Why the search may have found nothing, as the backend says it (`SearchStats`).
    pub unanswered_note: Option<String>,
}

/// A download query for `album`.
fn album_query(album: &Album) -> DownloadQuery {
    DownloadQuery::new(vec![]).album(album.clone())
}

#[component]
pub fn Search() -> Element {
    let auth = use_auth();
    let mut settings = use_settings();
    let mut search_results = use_signal::<Option<Vec<SearchResult>>>(|| None);
    let mut search = use_signal(String::new);
    let mut artist = use_signal::<Option<String>>(|| None);
    let mut artist_open = use_signal(|| false);
    let mut search_type = use_signal(|| settings.last_search_type());
    let mut loading = use_signal(|| false);
    let mut expanded_albums = use_signal::<HashSet<String>>(HashSet::new);
    let mut album_cache = use_signal::<HashMap<String, AlbumWithTracks>>(HashMap::new);
    let mut download_options = use_signal::<Option<SourceChoices>>(|| None);
    let mut is_downloading = use_signal(|| false);
    let mut download_error = use_signal(String::new);
    let search_reset = try_use_context::<SearchReset>();
    let search_prefill = try_use_context::<SearchPrefill>();

    let system_health = use_system_health();

    // Download state tracking
    let mut download_states = use_signal::<HashMap<String, DownloadRowState>>(HashMap::new);
    let mut batch_to_item = use_signal::<HashMap<String, String>>(HashMap::new);
    let mut folders = use_signal::<Vec<Folder>>(Vec::new);
    let mut selected_folder_id = use_signal::<Option<String>>(|| None);
    let mut fallback_toasts = use_signal::<Vec<FallbackToastData>>(Vec::new);
    let mut batch_to_name = use_signal::<HashMap<String, String>>(HashMap::new);
    let mut active_menu = use_signal::<Option<String>>(|| None);

    // Track if we've synced search_type from settings (to avoid saving on initial load)
    let mut synced = use_signal(|| false);

    // Sync search type from settings once loaded
    use_effect(move || {
        if settings.is_loaded() && !synced() {
            search_type.set(settings.last_search_type());
            synced.set(true);
        }
    });

    // Persist search type changes to settings
    use_effect(move || {
        let current_type = search_type();
        if synced() {
            spawn(async move {
                let _ = settings.set_last_search_type(current_type).await;
            });
        }
    });

    // Sync selected_folder_id from settings
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

    use_effect(move || {
        if let Some(reset) = search_reset {
            if reset.0() > 0 {
                search_results.set(None);
                search.set(String::new());
                artist.set(None);
                artist_open.set(false);
                expanded_albums.write().clear();
                album_cache.write().clear();
                loading.set(false);
            }
        }
    });

    // Buffer for events that arrive before batch_to_item is populated (race condition)
    let mut pending_events = use_signal::<Vec<AutoDownloadEvent>>(Vec::new);

    // Route AutoDownloadEvents from WebSocket to per-row download states
    let auto_dl = try_use_context::<AutoDownloadSignal>();
    use_effect(move || {
        let Some(mut ctx) = auto_dl else { return };
        // Subscribe only to ctx.0 (WS event slot). Reading other signals with
        // .read() would subscribe to them too, and writing the same signal
        // later in this effect would self-retrigger because Dioxus 0.7
        // Signal::set has no PartialEq dedupe (dioxus-signals/src/write.rs:345).
        let Some(event) = (ctx.0)() else { return };
        (ctx.0).set(None);

        // Collect the new event plus any buffered ones — peek, no subscribe
        let mut events_to_process = pending_events.peek().clone();
        events_to_process.push(event);
        let mut still_pending = Vec::new();

        for event in events_to_process {
            let batch_id = match &event {
                AutoDownloadEvent::Searching { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::ScoringResults { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::PickedSource { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::Downloading { batch_id } => batch_id.clone(),
                AutoDownloadEvent::FallbackToManual { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::Failed { batch_id, .. } => batch_id.clone(),
            };

            let item_id = batch_to_item.peek().get(&batch_id).cloned();
            let Some(item_id) = item_id else {
                // batch_id not yet registered, buffer for retry
                still_pending.push(event);
                continue;
            };

            match event {
                AutoDownloadEvent::Searching { .. } | AutoDownloadEvent::ScoringResults { .. } => {
                    download_states
                        .write()
                        .insert(item_id.clone(), DownloadRowState::Searching);
                    // D-09: propagate to expanded track rows
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
                    // D-09: propagate to expanded track rows
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

        // Safe to always set: neither this effect nor the drain effect
        // below subscribe to `pending_events` (peek throughout), and the
        // render does not read it either, so this is a leaf write.
        pending_events.set(still_pending);
    });

    // Drain pending events when batch_to_item changes (new batch registered).
    // Subscribe ONLY to batch_to_item via the `_trigger` read; every other
    // signal is peeked. Without this, `pending_events.set(...)` below would
    // self-retrigger the effect with the same value (Dioxus 0.7 Signal::set
    // has no PartialEq dedupe) and the race between the WS Searching event
    // and the auto_download HTTP response would freeze the browser.
    use_effect(move || {
        let _trigger = batch_to_item.read().len();
        if pending_events.peek().is_empty() {
            return;
        }

        let events = pending_events.peek().clone();
        let mut still_pending = Vec::new();

        for event in events {
            let batch_id = match &event {
                AutoDownloadEvent::Searching { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::ScoringResults { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::PickedSource { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::Downloading { batch_id } => batch_id.clone(),
                AutoDownloadEvent::FallbackToManual { batch_id, .. } => batch_id.clone(),
                AutoDownloadEvent::Failed { batch_id, .. } => batch_id.clone(),
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
                    // D-09: propagate to expanded track rows
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
                    // D-09: propagate to expanded track rows
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

    if !auth.is_logged_in() {
        info!("User not logged in");
        return rsx! {};
    }

    // Start an auto_download for a specific folder
    let mut start_auto_download = move |item_id: String, query: DownloadQuery, folder: Folder| {
        download_states
            .write()
            .insert(item_id.clone(), DownloadRowState::Searching);

        // Propagate to expanded track rows (D-09)
        if expanded_albums.read().contains(&item_id) {
            if let Some(cached) = album_cache.read().get(&item_id) {
                let mut states = download_states.write();
                for track in &cached.tracks {
                    states.insert(track.id.clone(), DownloadRowState::Searching);
                }
            }
        }

        // Extract display name before query is moved into the request
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

    // Auto-download with default folder resolution
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

    // Override download to a specific folder (D-02, does not change default)
    let mut handle_override_download =
        move |item_id: String, query: DownloadQuery, folder: Folder| {
            start_auto_download(item_id, query, folder);
        };

    // Handle folder change from FolderChip
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

    let download = move |query: DownloadQuery| async move {
        loading.set(true);
        download_error.set(String::new());
        download_options.set(Some(SourceChoices {
            search_id: String::new(),
            groups: vec![],
            unanswered_note: None,
        }));

        let search_id = match auth.call(api::start_download_search(query)).await {
            Ok(id) => id,
            Err(e) => {
                warn!("Failed to start download search: {:?}", e);
                loading.set(false);
                return;
            }
        };
        // Unless the panel was closed or shows other choices meanwhile.
        download_options.with_mut(|current| {
            if let Some(choices) = current.as_mut().filter(|c| c.search_id.is_empty()) {
                choices.search_id = search_id.clone();
            }
        });

        loop {
            match auth
                .call(api::poll_download_search(api::PollQuery {
                    search_id: search_id.clone(),
                }))
                .await
            {
                Ok(response) => {
                    download_options.with_mut(|current| {
                        if let Some(SourceChoices {
                            groups: list,
                            unanswered_note,
                            ..
                        }) = current.as_mut().filter(|c| c.search_id == search_id)
                        {
                            *unanswered_note = response.stats.unanswered_note;
                            for new_group in response.groups {
                                if let Some(pos) = list.iter().position(|x| {
                                    x.source == new_group.source && x.group_id == new_group.group_id
                                }) {
                                    // Safeguard against incomplete albums
                                    list[pos] = new_group;
                                } else {
                                    list.push(new_group);
                                }
                            }

                            // Resort new results by score
                            list.sort_by(|a, b| {
                                b.score
                                    .partial_cmp(&a.score)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        }
                    });

                    if response.state != DownloadSearchState::InProgress {
                        break;
                    }
                }
                Err(e) => {
                    info!("Failed to poll search: {:?}", e);
                    break;
                }
            }
        }
        loading.set(false);
    };

    let download_tracks =
        move |search_id: String, (items, folder): (Vec<DownloadableItem>, String)| async move {
            download_error.set(String::new());
            match auth
                .call(api::download(api::DownloadRequest {
                    items,
                    target_folder: folder,
                    search_id,
                }))
                .await
            {
                Ok(_res) => info!("Downloads started"),
                Err(e) => {
                    warn!("Failed to start downloads: {:?}", e);
                    download_error.set(crate::friendly_error(&e));
                }
            }
            is_downloading.set(false);
        };

    let perform_search = move || async move {
        loading.set(true);
        download_error.set(String::new());
        download_options.set(None);

        let query_data = api::SearchQuery {
            artist: artist(),
            query: search(),
        };

        let result = match search_type() {
            SearchType::Album => auth.call(api::search_album(query_data)).await,
            SearchType::Track => auth.call(api::search_track(query_data)).await,
        };

        if let Ok(data) = result {
            search_results.set(Some(data));
        }
        loading.set(false);
    };

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

    rsx! {
      // bg decorations
      BackgroundGlows { left: GlowColor::Accent, spread: GlowSpread::Wide }

      div {
        class: "w-full max-w-3xl space-y-8 z-10 mx-auto flex flex-col items-center mt-20",
        onclick: move |_| {
            if active_menu().is_some() {
                active_menu.set(None);
            }
        },

        // Title Area
        div { class: "text-center space-y-2",
          h2 { class: "text-4xl md:text-6xl font-bold tracking-tight",
            span { class: "text-white", "Harvest" }
            " "
            span { class: "text-beet-leaf font-light italic", "Music." }
          }
          p { class: "text-gray-400 font-mono text-sm",
            "Search & Download // Manage Your Library"
          }
        }

        div { class: "w-full",
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
            on_search: move |_| {
                spawn(perform_search());
            },
          }
        }

        SystemStatus { health: system_health.get(), navidrome_status: auth.navidrome_status() }

        if !download_error().is_empty() {
          p { class: "text-sm font-mono text-red-400 mt-4", "{download_error}" }
        }

        // Results
        if let Some(choices) = download_options.read().clone() {
          DownloadResults {
            results: choices.groups,
            unanswered_note: choices.unanswered_note,
            is_searching: loading(),
            is_downloading,
            on_download: move |data| {
                spawn(download_tracks(choices.search_id.clone(), data));
            },
            on_back: move |_| {
                download_options.set(None);
                download_error.set(String::new());
                loading.set(false);
            },
          }
        } else if loading() {
          div { class: "flex flex-col justify-center items-center py-10",
            div { class: "animate-spin rounded-full h-16 w-16 border-t-4 border-b-4 border-beet-accent" }
          }
        } else {
          match &*search_results.read() {
              Some(ref data) if !data.is_empty() => {
                rsx! {
                  div { class: "w-full bg-beet-panel/50 border border-white/5 p-6 backdrop-blur-sm mt-8 rounded-lg",
                    h5 { class: "text-xl font-display font-bold mb-4 border-b border-white/10 pb-2 text-white",
                      "Search Results"
                    }
                    ul { class: "list-none p-0 space-y-4",
                      for item in data.iter() {
                        match item {
                            SearchResult::Track(ref track) => {
                                let track_clone = track.clone();
                                let track_clone_2 = track.clone();
                                let track_for_dl = track.clone();
                                let track_for_override = track.clone();
                                let track_id = track.id.clone();
                                let dl_state = download_states.read().get(&track_id).cloned().unwrap_or_default();
                                let has_folders = !folders.read().is_empty();
                                let effective_state = if !has_folders { DownloadRowState::Disabled } else { dl_state };
                                let current_folders = folders.read().clone();
                                let current_folder_id = selected_folder_id();
                                rsx! {
                                  li { key: "{track.id}",
                                    TrackResult {
                                      on_search_sources: move || {
                                          spawn(download(DownloadQuery::from(track_clone.clone())));
                                      },
                                      on_album_click: {
                                          let album_id_for_expand = track_clone_2.album_id.clone();
                                          move || {
                                              if let Some(ref aid) = album_id_for_expand {
                                                  toggle_expand(aid.clone());
                                              }
                                          }
                                      },
                                      track: track.clone(),
                                      download_state: effective_state,
                                      folders: current_folders,
                                      selected_folder_id: current_folder_id.clone(),
                                      active_menu,
                                      on_download: move |_| {
                                          handle_auto_download(track_for_dl.id.clone(), DownloadQuery::from(track_for_dl.clone()));
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
                                          move || {
                                              toggle_expand(id.clone());
                                          }
                                      },
                                      on_search_sources: {
                                          let album_for_search = album.clone();
                                          move || {
                                              let query = album_query(&album_for_search);
                                              spawn(download(query));
                                          }
                                      },
                                      album: album.clone(),
                                      download_state: effective_state,
                                      folders: current_folders,
                                      selected_folder_id: current_folder_id.clone(),
                                      active_menu,
                                      on_download: move |_| {
                                          let query = album_query(&album_for_dl);
                                          handle_auto_download(album_for_dl.id.clone(), query);
                                      },
                                      on_override_download: move |folder: Folder| {
                                          let query = album_query(&album_for_override);
                                          handle_override_download(album_for_override.id.clone(), query, folder);
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
              },
              Some(_) => rsx! {
                div { class: "text-center text-gray-500 py-10 font-mono", "No signals found in the ether." }
              },
              None => rsx! {},
          }
        }

        // Fallback toasts (D-14 through D-17)
        if !fallback_toasts.read().is_empty() {
          div { class: "fixed bottom-4 right-4 flex flex-col-reverse gap-2 z-40 w-80 md:w-96",
            for toast_data in fallback_toasts.read().iter() {
              FallbackToast {
                key: "{toast_data.id}",
                toast: toast_data.clone(),
                on_pick_source: move |choices: SourceChoices| {
                    download_options.set(Some(choices));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_album_query_carries_the_album() {
        let album = Album {
            id: "8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f".to_string(),
            title: "Winter Songs".to_string(),
            artist: "Marrow Choir".to_string(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
        };
        let query = album_query(&album);
        assert!(query.tracks.is_empty());
        assert_eq!(query.album, Some(album));
    }
}
