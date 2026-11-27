pub mod album;
pub mod context;
pub mod track;

pub use album::AlbumResult;
pub use context::{AutoDownloadSignal, SearchPrefill, SearchReset};
pub use track::TrackResult;

mod download_icon;
pub use download_icon::{DownloadIcon, DownloadRowState};

mod download_options_menu;

mod inline_track_panel;
mod inline_track_row;

mod folder_chip;
pub use folder_chip::FolderChip;

mod search_bar;
pub use search_bar::SearchBar;

mod discography_heading;
pub use discography_heading::DiscographyHeading;

mod direct_escape;
pub use direct_escape::{direct_track_query, split_artist_title, DirectSearchEscape};

mod download_results;
pub use download_results::{
    group_backend_id, sort_merged_groups, BackendPollState, BackendSearchStatus, DownloadResults,
};

mod toast;
pub use toast::{FallbackToast, FallbackToastData};

mod search_type_toggle;
pub use search_type_toggle::{SearchType, SearchTypeToggle};

use api::models::folder::Folder;
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::backends::{BackendMeta, SLSKD};
use shared::download::{
    AutoDownloadEvent, DownloadQuery, DownloadableGroup, DownloadableItem,
    SearchState as DownloadSearchState,
};
use shared::metadata::{
    asks_no_title, Album, AlbumWithTracks, ArtistSummary, Discography, SearchResult, Track,
};
use std::collections::{HashMap, HashSet};

use crate::settings_context::use_settings;
use crate::{friendly_error, use_auth, use_system_health, Auth, SystemStatus};

/// The sources a search found, to pick files from, and the search IDs a pick
/// names (one fallback `search_id` plus per-backend IDs for multi-backend searches).
#[derive(Clone, PartialEq, Debug)]
pub struct SourceChoices {
    /// Fallback search_id (e.g. from `FallbackToast` or single-backend search).
    pub search_id: String,
    /// Per-backend search IDs (`backend_id -> search_id`) for multi-backend manual searches.
    pub search_ids_by_backend: HashMap<String, String>,
    pub groups: Vec<DownloadableGroup>,
    /// Why the search may have found nothing, as the backend says it (`SearchStats`).
    pub unanswered_note: Option<String>,
    /// Live per-backend polling status (populated for multi-backend searches).
    pub backend_statuses: Vec<BackendSearchStatus>,
}

impl SourceChoices {
    /// Wraps a single stored search (such as `AutoDownloadEvent::FallbackToManual`).
    pub fn single(
        search_id: String,
        mut groups: Vec<DownloadableGroup>,
        unanswered_note: Option<String>,
    ) -> Self {
        sort_merged_groups(&mut groups);
        Self {
            search_id,
            search_ids_by_backend: HashMap::new(),
            groups,
            unanswered_note,
            backend_statuses: Vec::new(),
        }
    }

    /// Resolves the stored `search_id` that owns items from `backend_id`.
    pub fn search_id_for_backend(&self, backend_id: &str) -> String {
        self.search_ids_by_backend
            .get(backend_id)
            .cloned()
            .unwrap_or_else(|| self.search_id.clone())
    }

    /// Merges a backend's poll results into `self.groups`, ensuring every item carries
    /// its backend ID, deduplicating per `(backend, source, group_id)`, and keeping
    /// `Delivery::AsFound` groups sorted ahead of `Delivery::LossyOnly` by score desc.
    pub fn merge_backend_groups(
        &mut self,
        backend_id: &str,
        incoming: Vec<DownloadableGroup>,
        unanswered_note: Option<String>,
    ) {
        if self.unanswered_note.is_none() {
            self.unanswered_note = unanswered_note;
        }
        for mut new_group in incoming {
            for item in &mut new_group.items {
                if item.backend.is_empty() {
                    item.backend = backend_id.to_string();
                }
            }
            if let Some(pos) = self.groups.iter().position(|x| {
                group_backend_id(x) == Some(backend_id)
                    && x.source == new_group.source
                    && x.group_id == new_group.group_id
            }) {
                self.groups[pos] = new_group;
            } else {
                self.groups.push(new_group);
            }
        }
        sort_merged_groups(&mut self.groups);
        let count = self
            .groups
            .iter()
            .filter(|g| group_backend_id(g) == Some(backend_id))
            .count();
        if let Some(st) = self
            .backend_statuses
            .iter_mut()
            .find(|s| s.id == backend_id)
        {
            st.group_count = count;
        }
    }
}

/// Starts parallel manual source searches across every enabled download backend,
/// polling concurrently and streaming merged groups into `download_options`.
///
/// Falls back to a single default-backend search (`backend: None`) when the
/// enabled backend list is empty or cannot be fetched.
#[allow(clippy::too_many_arguments)]
pub async fn run_multi_backend_source_search(
    auth: Auth,
    query: DownloadQuery,
    enabled_backends: Vec<BackendMeta>,
    mut download_options: Signal<Option<SourceChoices>>,
    mut source_searching: Signal<bool>,
    mut download_error: Signal<String>,
    gen_signal: Signal<u64>,
    expected_gen: u64,
) {
    source_searching.set(true);
    download_error.set(String::new());

    let resolved_backends = if !enabled_backends.is_empty() {
        enabled_backends
    } else {
        auth.call(api::get_enabled_download_backends())
            .await
            .ok()
            .unwrap_or_default()
    };

    if *gen_signal.peek() != expected_gen {
        return;
    }

    // (explicit backend field for DownloadQuery, metadata for display/tracking)
    let targets: Vec<(Option<String>, BackendMeta)> = if resolved_backends.is_empty() {
        vec![(None, SLSKD)]
    } else {
        resolved_backends
            .into_iter()
            .map(|m| (Some(m.id.to_string()), m))
            .collect()
    };

    download_options.set(Some(SourceChoices {
        search_id: String::new(),
        search_ids_by_backend: HashMap::new(),
        groups: vec![],
        unanswered_note: None,
        backend_statuses: targets
            .iter()
            .map(|(_, meta)| BackendSearchStatus {
                id: meta.id.to_string(),
                name: meta.name.to_string(),
                delivery: meta.delivery,
                state: BackendPollState::Searching,
                group_count: 0,
                warning: None,
            })
            .collect(),
    }));

    let tasks = targets.into_iter().map(|(explicit_backend, meta)| {
        let q = DownloadQuery {
            backend: explicit_backend,
            ..query.clone()
        };
        async move {
            let search_id = match auth.call(api::start_download_search(q)).await {
                Ok(id) => id,
                Err(e) => {
                    warn!("Failed to start {} download search: {:?}", meta.id, e);
                    if *gen_signal.peek() == expected_gen {
                        let msg = friendly_error(&e);
                        download_options.with_mut(|current| {
                            if let Some(choices) = current.as_mut() {
                                if let Some(st) =
                                    choices.backend_statuses.iter_mut().find(|s| s.id == meta.id)
                                {
                                    st.state = BackendPollState::Failed;
                                    st.warning = Some(msg);
                                }
                            }
                        });
                    }
                    return;
                }
            };

            if *gen_signal.peek() != expected_gen {
                return;
            }

            download_options.with_mut(|current| {
                if let Some(choices) = current.as_mut() {
                    if choices.search_id.is_empty() {
                        choices.search_id = search_id.clone();
                    }
                    choices
                        .search_ids_by_backend
                        .insert(meta.id.to_string(), search_id.clone());
                }
            });

            loop {
                if *gen_signal.peek() != expected_gen {
                    break;
                }
                match auth
                    .call(api::poll_download_search(api::PollQuery {
                        search_id: search_id.clone(),
                    }))
                    .await
                {
                    Ok(response) => {
                        if *gen_signal.peek() != expected_gen {
                            break;
                        }
                        let done = response.state != DownloadSearchState::InProgress;
                        download_options.with_mut(|current| {
                            if let Some(choices) = current.as_mut() {
                                choices.merge_backend_groups(
                                    meta.id,
                                    response.groups,
                                    response.stats.unanswered_note,
                                );
                                if done {
                                    if let Some(st) = choices
                                        .backend_statuses
                                        .iter_mut()
                                        .find(|s| s.id == meta.id)
                                    {
                                        st.state = BackendPollState::Done;
                                    }
                                }
                            }
                        });
                        if done {
                            break;
                        }
                        gloo_timers::future::TimeoutFuture::new(350).await;
                    }
                    Err(e) => {
                        info!("Failed to poll {} search: {:?}", meta.id, e);
                        if *gen_signal.peek() == expected_gen {
                            let msg = friendly_error(&e);
                            download_options.with_mut(|current| {
                                if let Some(choices) = current.as_mut() {
                                    if let Some(st) = choices
                                        .backend_statuses
                                        .iter_mut()
                                        .find(|s| s.id == meta.id)
                                    {
                                        st.state = BackendPollState::Failed;
                                        st.warning = Some(msg);
                                    }
                                }
                            });
                        }
                        break;
                    }
                }
            }
        }
    });

    futures::future::join_all(tasks).await;

    if *gen_signal.peek() == expected_gen {
        source_searching.set(false);
    }
}

/// Submits hand-picked `items` to `api::download`, grouping by `item.backend` so
/// each request honors the server's single-backend-per-request contract and passes
/// the matching backend `search_id`.
pub async fn submit_multi_backend_download(
    auth: Auth,
    choices: &SourceChoices,
    items: Vec<DownloadableItem>,
    target_folder: String,
) -> Result<(), String> {
    let mut by_backend: Vec<(String, Vec<DownloadableItem>)> = Vec::new();
    for item in items {
        let key = item.backend.clone();
        if let Some((_, bucket)) = by_backend.iter_mut().find(|(b, _)| *b == key) {
            bucket.push(item);
        } else {
            by_backend.push((key, vec![item]));
        }
    }

    let mut errors = Vec::new();
    for (backend_id, batch) in by_backend {
        let search_id = choices.search_id_for_backend(&backend_id);
        match auth
            .call(api::download(api::DownloadRequest {
                items: batch,
                target_folder: target_folder.clone(),
                search_id,
            }))
            .await
        {
            Ok(_) => info!("Downloads started on backend {}", backend_id),
            Err(e) => {
                warn!("Failed to start downloads on {}: {:?}", backend_id, e);
                errors.push(friendly_error(&e));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join(" · "))
    }
}

/// A download query for `album`.
pub fn album_query(album: &Album) -> DownloadQuery {
    DownloadQuery::new(vec![]).album(album.clone())
}

/// The artist whose albums a search lists: one given with no title.
pub fn discography_artist(artist: Option<String>, query: &str) -> Option<String> {
    artist.filter(|_| asks_no_title(query))
}

/// What an artist-only search listed.
#[derive(Clone, PartialEq, Debug)]
pub struct Listed {
    /// The name searched for.
    pub name: String,
    /// The artist whose albums are listed; None when no artist carries the name.
    pub artist: Option<ArtistSummary>,
    /// The other artists carrying the name.
    pub others: Vec<ArtistSummary>,
    pub albums_complete: bool,
    pub singles_complete: bool,
    pub shown: usize,
}

/// The listing of the search for `name` and its rows, from what it found.
pub fn listed_from(name: String, found: Option<Discography>) -> (Listed, Vec<SearchResult>) {
    let Some(found) = found else {
        let listed = Listed {
            name,
            artist: None,
            others: vec![],
            albums_complete: true,
            singles_complete: true,
            shown: 0,
        };
        return (listed, vec![]);
    };
    let listed = Listed {
        name,
        artist: Some(found.artist),
        others: found.others,
        albums_complete: found.albums_complete,
        singles_complete: found.singles_complete,
        shown: found.albums.len(),
    };
    let rows = found.albums.into_iter().map(SearchResult::Album).collect();
    (listed, rows)
}

/// What a search that found nothing says.
pub fn empty_message(listed: Option<&Listed>) -> String {
    match listed {
        Some(Listed {
            artist: Some(artist),
            ..
        }) => format!(
            "No albums, EPs or singles by {} on MusicBrainz.",
            artist.name
        ),
        Some(Listed { name, .. }) => format!("No artist named “{name}” on MusicBrainz."),
        None => "No signals found in the ether.".to_string(),
    }
}

/// Standalone Studio multi-backend search component (reusable outside the web shell).
#[component]
pub fn Search() -> Element {
    let auth = use_auth();
    let mut settings = use_settings();
    let mut search_results = use_signal::<Option<Vec<SearchResult>>>(|| None);
    // Set while the results are an artist's albums.
    let mut listed = use_signal::<Option<Listed>>(|| None);
    let mut search = use_signal(String::new);
    let mut artist = use_signal::<Option<String>>(|| None);
    let mut artist_open = use_signal(|| false);
    let mut search_type = use_signal(|| settings.last_search_type());
    let mut loading = use_signal(|| false);
    let mut source_searching = use_signal(|| false);
    let mut source_search_gen = use_signal(|| 0u64);
    let mut expanded_albums = use_signal::<HashSet<String>>(HashSet::new);
    let mut album_cache = use_signal::<HashMap<String, AlbumWithTracks>>(HashMap::new);
    let mut download_options = use_signal::<Option<SourceChoices>>(|| None);
    let mut is_downloading = use_signal(|| false);
    let mut download_error = use_signal(String::new);
    let mut search_error = use_signal(String::new);
    let mut enabled_backends = use_signal::<Vec<BackendMeta>>(Vec::new);
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

    let mut synced = use_signal(|| false);

    use_effect(move || {
        if settings.is_loaded() && !synced() {
            search_type.set(settings.last_search_type());
            synced.set(true);
        }
    });

    use_effect(move || {
        let current_type = search_type();
        if synced() {
            spawn(async move {
                let _ = settings.set_last_search_type(current_type).await;
            });
        }
    });

    use_effect(move || {
        if settings.is_loaded() {
            if let Some(us) = settings.get() {
                if us.default_download_folder_id.is_some() {
                    selected_folder_id.set(us.default_download_folder_id.clone());
                }
            }
        }
    });

    use_future(move || async move {
        if let Ok(user_folders) = auth.call(api::get_user_folders()).await {
            if user_folders.len() == 1 {
                selected_folder_id.set(Some(user_folders[0].id.clone()));
            }
            folders.set(user_folders);
        }
    });

    use_future(move || async move {
        if let Ok(list) = auth.call(api::get_enabled_download_backends()).await {
            enabled_backends.set(list);
        }
    });

    use_effect(move || {
        if let Some(reset) = search_reset {
            if reset.0() > 0 {
                let next_gen = source_search_gen.peek().wrapping_add(1);
                source_search_gen.set(next_gen);
                search_results.set(None);
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

    let mut pending_events = use_signal::<Vec<AutoDownloadEvent>>(Vec::new);
    let auto_dl = try_use_context::<AutoDownloadSignal>();
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

    if !auth.is_logged_in() {
        info!("User not logged in");
        return rsx! {};
    }

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

    let mut start_search = move || {
        download_error.set(String::new());
        let next_gen = source_search_gen.peek().wrapping_add(1);
        source_search_gen.set(next_gen);
        download_options.set(None);
        source_searching.set(false);
        search_error.set(String::new());
        loading.set(true);
    };

    let list_discography = move |name: String, chosen: Option<String>| async move {
        start_search();
        let found = auth
            .call(api::artist_discography(api::DiscographyQuery {
                artist: name.clone(),
                mbid: chosen,
            }))
            .await;
        match found {
            Ok(found) => {
                let (listing, rows) = listed_from(name, found);
                listed.set(Some(listing));
                search_results.set(Some(rows));
            }
            Err(e) => search_error.set(friendly_error(&e)),
        }
        loading.set(false);
    };

    let perform_search = move || async move {
        if let Some(name) = discography_artist(artist(), &search()) {
            return list_discography(name, None).await;
        }
        start_search();

        let query_data = api::SearchQuery {
            artist: artist(),
            query: search(),
        };

        let result = match search_type() {
            SearchType::Album => auth.call(api::search_album(query_data)).await,
            SearchType::Track => auth.call(api::search_track(query_data)).await,
        };

        match result {
            Ok(data) => {
                listed.set(None);
                search_results.set(Some(data));
            }
            Err(e) => search_error.set(friendly_error(&e)),
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
      div {
        class: "w-full max-w-4xl space-y-6 mx-auto",
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
          div { class: "w-full space-y-2",
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

            SystemStatus { health: system_health.get(), navidrome_status: auth.navidrome_status() }
          }

          if !search_error().is_empty() {
            p { class: "text-[13px] text-beet-crimson-text bg-beet-crimson-surface border border-beet-crimson-text/30 rounded-[10px] px-4 py-2.5",
              "{search_error}"
            }
          }
          if !download_error().is_empty() {
            p { class: "text-[13px] text-beet-crimson-text bg-beet-crimson-surface border border-beet-crimson-text/30 rounded-[10px] px-4 py-2.5",
              "{download_error}"
            }
          }

          if loading() {
            div { class: "flex flex-col justify-center items-center py-10 gap-3",
              div { class: "animate-spin rounded-full h-10 w-10 border-2 border-beet-border-default border-t-beet-leaf-text" }
            }
          } else {
            match (&*search_results.read(), listed()) {
                (Some(ref data), listing) if !data.is_empty() || listing.as_ref().is_some_and(|l| l.artist.is_some()) => {
                  rsx! {
                    div { class: "w-full bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 rounded-[16px]",
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
                        h5 { class: "text-[17px] font-display font-semibold mb-4 border-b border-beet-border-subtle pb-2.5 text-beet-text-primary",
                          "Search Results"
                        }
                      }
                      if data.is_empty() {
                        p { class: "text-center text-beet-text-tertiary py-8 text-[13px]", "{empty_message(listing.as_ref())}" }
                      } else {
                        ul { class: "list-none p-0 space-y-2.5",
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
                                              open_source_picker(DownloadQuery::from(track_clone.clone()));
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
                                                  open_source_picker(album_query(&album_for_search));
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
                  }
                },
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
            first_release_date: None,
        };
        let query = album_query(&album);
        assert!(query.tracks.is_empty());
        assert_eq!(query.album, Some(album));
    }

    #[test]
    fn a_search_with_an_artist_and_no_title_lists_the_discography() {
        let chevelle = || Some("Chevelle".to_string());
        assert_eq!(discography_artist(chevelle(), ""), chevelle());
        assert_eq!(discography_artist(chevelle(), " * "), chevelle());
        assert_eq!(discography_artist(chevelle(), "Vena Sera"), None);
        assert_eq!(discography_artist(None, ""), None);

        let (nobody, rows) = listed_from("Nirvana".into(), None);
        assert_eq!((nobody.artist.as_ref(), rows.len()), (None, 0));
        assert_eq!(
            empty_message(Some(&nobody)),
            "No artist named “Nirvana” on MusicBrainz."
        );

        let album = |id: &str, title: &str| Album {
            id: id.into(),
            title: title.into(),
            artist: "Chevelle".into(),
            release_date: None,
            mbid: Some(id.into()),
            cover_url: None,
            release_types: Some(vec!["Album".into()]),
            first_release_date: None,
        };
        let artist = ArtistSummary {
            mbid: "8456e9f7-debf-4579-a86c-33a325a35d2d".into(),
            name: "Chevelle".into(),
            disambiguation: None,
        };
        let (listed, rows) = listed_from(
            "chevelle".into(),
            Some(Discography {
                artist: artist.clone(),
                others: vec![],
                albums: vec![
                    album("c9a3fe11-45a8-4045-9d15-1b80b8333f81", "Point #1"),
                    album("accde5f7-04da-4c66-bb8e-7bb3d39ad326", "Wonder What’s Next"),
                ],
                albums_complete: true,
                singles_complete: false,
            }),
        );
        let titles: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                SearchResult::Album(a) => a.title.as_str(),
                SearchResult::Track(t) => panic!("a track row: {t:?}"),
            })
            .collect();
        assert_eq!(titles, ["Point #1", "Wonder What’s Next"]);
        assert_eq!(
            (
                listed.artist,
                listed.shown,
                listed.albums_complete,
                listed.singles_complete
            ),
            (Some(artist.clone()), 2, true, false)
        );
        let (none_listed, _) = listed_from(
            "Chevelle".into(),
            Some(Discography {
                artist,
                others: vec![],
                albums: vec![],
                albums_complete: true,
                singles_complete: true,
            }),
        );
        assert_eq!(
            empty_message(Some(&none_listed)),
            "No albums, EPs or singles by Chevelle on MusicBrainz."
        );
        assert_eq!(empty_message(None), "No signals found in the ether.");
    }
}
