//! Desktop Studio Unified Search View: 0ms local SQLite FTS5 search paired with
//! debounced remote catalog search (Deezer + MusicBrainz deeper catalog fallback),
//! filter pills, 1-tap `Acquire & Play`, and inline Remote Album / Artist drill-down views.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use client_core::models::{
    LocalAlbum, RemoteAlbumDetail, RemoteArtistDetail, RemoteCatalogAlbum, RemoteCatalogArtist,
    RemoteCatalogResults, RemoteCatalogTrack, SearchResults,
};
use client_core::SoulbeetCore;
use dioxus::prelude::*;
use shared::search_norm::canonical_artist_key;
use ui::{
    use_player, AcquireStageBadge, IconCheck, IconChevronLeft, IconChevronRight, IconDisc3,
    IconDownload, IconLoader, IconMusic, IconPlay, IconRefreshCw, IconSearch, IconWifiOff, IconX,
    PlayerTrack, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    desktop_cover_art_url, load_cached_track_ids, load_pending_discovery_map,
    load_tracks_for_album, local_track_to_player_track, remote_album_download_key,
    remote_track_acquire_key, use_desktop_studio, RemoteAlbumDownloadState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    Tracks,
    Albums,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscographyFilter {
    All,
    Albums,
    Eps,
    Singles,
}

#[derive(Debug, Clone, PartialEq)]
enum CatalogDetailView {
    None,
    LocalArtist {
        artist_id: String,
        artist_name: String,
        cover_art_id: Option<String>,
        loading: bool,
        albums: Vec<LocalAlbum>,
        tracks: Vec<PlayerTrack>,
        error: Option<String>,
    },
    Album {
        album_id: String,
        title_hint: String,
        artist_hint: String,
        loading: bool,
        detail: Option<RemoteAlbumDetail>,
        error: Option<String>,
    },
    Artist {
        artist_id: String,
        name: Option<String>,
        loading: bool,
        detail: Option<RemoteArtistDetail>,
        error: Option<String>,
    },
}

impl CatalogDetailView {
    fn breadcrumb_label(&self) -> String {
        match self {
            Self::None => "Search".to_string(),
            Self::LocalArtist { artist_name, .. } => {
                if artist_name.trim().is_empty() {
                    "Artist".to_string()
                } else {
                    artist_name.clone()
                }
            }
            Self::Album {
                title_hint, detail, ..
            } => detail
                .as_ref()
                .map(|d| d.album.title.clone())
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| {
                    if title_hint.trim().is_empty() {
                        "Album".to_string()
                    } else {
                        title_hint.clone()
                    }
                }),
            Self::Artist { name, detail, .. } => detail
                .as_ref()
                .map(|d| d.artist.name.clone())
                .filter(|n| !n.trim().is_empty())
                .or_else(|| name.clone().filter(|n| !n.trim().is_empty()))
                .unwrap_or_else(|| "Artist".to_string()),
        }
    }
}

#[component]
pub fn SearchView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();

    let mut query = use_signal(move || studio.search_seed_query.peek().clone());
    let mut active_filter = use_signal(|| SearchFilter::All);

    // Debounced remote search state (kept strictly decoupled from 0ms local search)
    let mut remote_results = use_signal(|| None::<RemoteCatalogResults>);
    let mut remote_loading = use_signal(|| false);
    let mut remote_error = use_signal(|| None::<String>);
    let remote_gen = use_signal(|| Arc::new(AtomicU64::new(0)));

    // Drill-down navigation stack supporting Search -> Artist -> Album -> Back to Artist -> Back to Search
    let mut detail_stack = use_signal(Vec::<CatalogDetailView>::new);
    let detail_gen = use_signal(|| Arc::new(AtomicU64::new(0)));

    // Sync query if Command Palette seeded a new search term
    use_effect(move || {
        let seeded = studio.search_seed_query.read().clone();
        if !seeded.is_empty() {
            if seeded != *query.peek() {
                query.set(seeded);
            }
            detail_stack.write().clear();
            studio.search_seed_query.set(String::new());
        }
    });

    // 1. Instant 0ms local SQLite FTS5 + albums + artists search (zero HTTP)
    let local_res = use_resource(move || {
        let q = query();
        let core = studio.core.read().clone();
        let _offline = player.offline_only();
        let _rev = (studio.library_revision)();
        async move {
            let c = core?;
            if q.trim().is_empty() {
                return None;
            }
            let cached = load_cached_track_ids(&c).await;
            let disc = load_pending_discovery_map(&c).await;
            match c.search_local(&q, 40).await {
                Ok(res) => Some((res, cached, disc)),
                Err(err) => {
                    tracing::warn!("local search failed: {err}");
                    None
                }
            }
        }
    });

    // 2. Debounced (250ms) remote catalog search with monotonic request generation counter
    use_effect(move || {
        let raw_q = query();
        let core_opt = studio.core.read().clone();
        let is_offline = player.offline_only();
        let gen_arc = remote_gen.peek().clone();
        let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;

        let trimmed = raw_q.trim().to_string();
        if is_offline {
            detail_stack
                .write()
                .retain(|v| matches!(v, CatalogDetailView::LocalArtist { .. }));
        }
        if is_offline || trimmed.is_empty() {
            remote_loading.set(false);
            remote_error.set(None);
            remote_results.set(None);
            return;
        }
        let Some(core) = core_opt else {
            remote_loading.set(false);
            remote_error.set(None);
            remote_results.set(None);
            return;
        };

        if remote_results
            .peek()
            .as_ref()
            .is_some_and(|r| !r.query.trim().eq_ignore_ascii_case(&trimmed))
        {
            remote_results.set(None);
        }
        remote_error.set(None);
        remote_loading.set(true);

        spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if gen_arc.load(Ordering::SeqCst) != req_gen || core.is_offline_only() {
                return;
            }
            match core.search_remote(&trimmed, None, 20).await {
                Ok(res) => {
                    if gen_arc.load(Ordering::SeqCst) != req_gen || core.is_offline_only() {
                        return;
                    }
                    remote_results.set(Some(res));
                    remote_error.set(None);
                    remote_loading.set(false);
                }
                Err(err) => {
                    if gen_arc.load(Ordering::SeqCst) != req_gen || core.is_offline_only() {
                        return;
                    }
                    tracing::warn!("remote catalog search failed: {err}");
                    remote_results.set(None);
                    remote_error.set(Some(err.to_string()));
                    remote_loading.set(false);
                }
            }
        });
    });

    // Helper closure to execute a remote search immediately (Enter key or deeper catalog button)
    let trigger_remote_now = move |provider: Option<&'static str>| {
        if player.offline_only() {
            return;
        }
        let trimmed = query.peek().trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let Some(core) = studio.core.peek().clone() else {
            return;
        };
        let gen_arc = remote_gen.peek().clone();
        let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;
        let mut remote_results = remote_results;
        let mut remote_loading = remote_loading;
        let mut remote_error = remote_error;
        if remote_results
            .peek()
            .as_ref()
            .is_some_and(|r| !r.query.trim().eq_ignore_ascii_case(&trimmed))
        {
            remote_results.set(None);
        }
        remote_error.set(None);
        remote_loading.set(true);

        spawn(async move {
            match core.search_remote(&trimmed, provider, 20).await {
                Ok(res) => {
                    if gen_arc.load(Ordering::SeqCst) != req_gen || core.is_offline_only() {
                        return;
                    }
                    remote_results.set(Some(res));
                    remote_error.set(None);
                    remote_loading.set(false);
                }
                Err(err) => {
                    if gen_arc.load(Ordering::SeqCst) != req_gen || core.is_offline_only() {
                        return;
                    }
                    tracing::warn!("immediate remote catalog search failed: {err}");
                    remote_results.set(None);
                    remote_error.set(Some(err.to_string()));
                    remote_loading.set(false);
                }
            }
        });
    };

    // Helper closure to open Local Artist Detail View (works offline and online)
    let open_local_artist =
        move |artist_id: String, artist_name: String, cover_art_id: Option<String>| {
            let mut detail_stack = detail_stack;
            let Some(core) = studio.core.peek().clone() else {
                return;
            };
            let gen_arc = detail_gen.peek().clone();
            let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;

            detail_stack.write().push(CatalogDetailView::LocalArtist {
                artist_id: artist_id.clone(),
                artist_name: artist_name.clone(),
                cover_art_id,
                loading: true,
                albums: Vec::new(),
                tracks: Vec::new(),
                error: None,
            });

            spawn_local_artist_fetch(core, artist_id, artist_name, gen_arc, req_gen, detail_stack);
        };

    // Helper closure to open Remote Album Detail View (pushes onto breadcrumb stack)
    let open_remote_album = move |album_id: String, title_hint: String, artist_hint: String| {
        let mut studio = studio;
        let mut detail_stack = detail_stack;
        if player.offline_only() {
            studio
                .status_toast
                .set(Some("Disable Offline Only mode to browse remote albums.".into()));
            return;
        }
        let Some(core) = studio.core.peek().clone() else {
            return;
        };
        let gen_arc = detail_gen.peek().clone();
        let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;

        detail_stack.write().push(CatalogDetailView::Album {
            album_id: album_id.clone(),
            title_hint,
            artist_hint,
            loading: true,
            detail: None,
            error: None,
        });

        spawn_album_detail_fetch(core, album_id, gen_arc, req_gen, detail_stack);
    };

    // Helper closure to open Remote Artist Detail View (pushes onto breadcrumb stack)
    let open_remote_artist = move |artist_id: String, name: Option<String>, replace_top: bool| {
        let mut studio = studio;
        let mut detail_stack = detail_stack;
        if player.offline_only() {
            studio
                .status_toast
                .set(Some("Disable Offline Only mode to browse remote artists.".into()));
            return;
        }
        let Some(core) = studio.core.peek().clone() else {
            return;
        };
        let gen_arc = detail_gen.peek().clone();
        let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;

        let entry = CatalogDetailView::Artist {
            artist_id: artist_id.clone(),
            name: name.clone(),
            loading: true,
            detail: None,
            error: None,
        };
        {
            let mut stack = detail_stack.write();
            if replace_top && !stack.is_empty() {
                stack.pop();
            }
            stack.push(entry);
        }

        spawn_artist_detail_fetch(core, artist_id, name, gen_arc, req_gen, detail_stack);
    };

    let stack_snapshot = detail_stack.read().clone();
    let active_detail = stack_snapshot
        .last()
        .cloned()
        .unwrap_or(CatalogDetailView::None);
    let acquiring_map = studio.acquiring_tracks.read().clone();
    let downloading_albums = studio.downloading_albums.read().clone();
    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    rsx! {
        div { class: "p-6 space-y-6 max-w-6xl mx-auto",
            // Sticky Unified Search Bar
            div { class: "flex items-center gap-3 px-4 h-12 rounded-[12px] bg-beet-surface border border-beet-border-default focus-within:border-beet-leaf-solid transition-colors",
                IconSearch { size: 18, class: "text-beet-text-tertiary shrink-0".to_string() }
                input {
                    r#type: "text",
                    value: "{query}",
                    placeholder: if offline_only {
                        "Search cached offline library (0ms SQLite FTS5)..."
                    } else {
                        "Search library & global catalog (artists, albums, tracks)..."
                    },
                    class: "flex-1 bg-transparent text-[15px] text-beet-text-primary placeholder:text-beet-text-tertiary outline-none",
                    oninput: move |evt| {
                        let next = evt.value();
                        query.set(next);
                        if !detail_stack.peek().is_empty() {
                            detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                            detail_stack.write().clear();
                        }
                    },
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter {
                            evt.prevent_default();
                            if !detail_stack.peek().is_empty() {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().clear();
                            }
                            trigger_remote_now(None);
                        }
                    },
                }
                if !query().is_empty() {
                    button {
                        r#type: "button",
                        aria_label: "Clear search",
                        title: "Clear search",
                        class: "w-7 h-7 rounded-full inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high transition-colors cursor-pointer",
                        onclick: move |_| {
                            remote_gen.peek().fetch_add(1, Ordering::SeqCst);
                            detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                            query.set(String::new());
                            detail_stack.write().clear();
                            remote_results.set(None);
                            remote_error.set(None);
                            remote_loading.set(false);
                        },
                        IconX { size: 14 }
                    }
                }
                if offline_only {
                    span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-leaf-surface text-beet-leaf-text tabular-nums-mono text-[10px] font-semibold shrink-0",
                        IconWifiOff { size: 11 }
                        "OFFLINE"
                    }
                }
            }

            // Active Drill-Down Detail View OR Unified Search Results
            match active_detail {
                CatalogDetailView::LocalArtist {
                    artist_id,
                    artist_name,
                    cover_art_id,
                    loading,
                    albums,
                    tracks,
                    error,
                } => {
                    let prev_label = if stack_snapshot.len() > 1 {
                        Some(stack_snapshot[stack_snapshot.len() - 2].breadcrumb_label())
                    } else {
                        None
                    };
                    let crumbs: Vec<String> = stack_snapshot
                        .iter()
                        .map(CatalogDetailView::breadcrumb_label)
                        .collect();
                    let retry_id = artist_id.clone();
                    let retry_name = artist_name.clone();
                    let explore_name = artist_name.clone();
                    rsx! {
                        DetailNavHeader {
                            prev_label: prev_label,
                            breadcrumbs: crumbs,
                            on_back_one: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().pop();
                            },
                            on_back_to_search: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().clear();
                            },
                        }

                        if loading {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 flex items-center justify-center gap-3 text-beet-text-secondary",
                                IconLoader { size: 18, class: "animate-spin text-beet-leaf-text".to_string() }
                                span { class: "text-[14px]", "Loading {artist_name} from your library..." }
                            }
                        } else if let Some(err_msg) = error {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-crimson-text/30 p-6 space-y-3",
                                p { class: "text-[14px] font-semibold text-beet-crimson-text",
                                    "Could not load local artist"
                                }
                                p { class: "text-[13px] text-beet-text-secondary", "{err_msg}" }
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid text-[12px] font-semibold cursor-pointer",
                                    onclick: move |_| {
                                        if let Some(core) = studio.core.peek().clone() {
                                            let gen_arc = detail_gen.peek().clone();
                                            let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;
                                            if let Some(CatalogDetailView::LocalArtist { loading, error, .. }) =
                                                detail_stack.write().last_mut()
                                            {
                                                *loading = true;
                                                *error = None;
                                            }
                                            spawn_local_artist_fetch(
                                                core,
                                                retry_id.clone(),
                                                retry_name.clone(),
                                                gen_arc,
                                                req_gen,
                                                detail_stack,
                                            );
                                        }
                                    },
                                    IconRefreshCw { size: 13 }
                                    "Retry"
                                }
                            }
                        } else {
                            LocalArtistDetailPanel {
                                artist_name: artist_name,
                                cover_art_id: cover_art_id,
                                albums: albums,
                                tracks: tracks,
                                offline_only: offline_only,
                                current_id: current_id.clone(),
                                is_playing: is_playing,
                                on_explore_catalog: move |_| {
                                    open_remote_artist(String::new(), Some(explore_name.clone()), false);
                                },
                            }
                        }
                    }
                }
                CatalogDetailView::Album {
                    album_id,
                    title_hint,
                    artist_hint,
                    loading,
                    detail,
                    error,
                } => {
                    let prev_label = if stack_snapshot.len() > 1 {
                        Some(stack_snapshot[stack_snapshot.len() - 2].breadcrumb_label())
                    } else {
                        None
                    };
                    let crumbs: Vec<String> = stack_snapshot
                        .iter()
                        .map(CatalogDetailView::breadcrumb_label)
                        .collect();
                    let retry_id = album_id.clone();
                    rsx! {
                        DetailNavHeader {
                            prev_label: prev_label,
                            breadcrumbs: crumbs,
                            on_back_one: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().pop();
                            },
                            on_back_to_search: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().clear();
                            },
                        }

                        if loading {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 flex items-center justify-center gap-3 text-beet-text-secondary",
                                IconLoader { size: 18, class: "animate-spin text-beet-leaf-text".to_string() }
                                span { class: "text-[14px]",
                                    if title_hint.is_empty() {
                                        "Loading album details..."
                                    } else {
                                        "Loading \"{title_hint}\"..."
                                    }
                                }
                            }
                        } else if let Some(err_msg) = error {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-crimson-text/30 p-6 space-y-3",
                                p { class: "text-[14px] font-semibold text-beet-crimson-text",
                                    "Could not load album details"
                                }
                                p { class: "text-[13px] text-beet-text-secondary", "{err_msg}" }
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid text-[12px] font-semibold cursor-pointer",
                                    onclick: move |_| {
                                        if let Some(core) = studio.core.peek().clone() {
                                            let gen_arc = detail_gen.peek().clone();
                                            let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;
                                            if let Some(CatalogDetailView::Album { loading, error, .. }) =
                                                detail_stack.write().last_mut()
                                            {
                                                *loading = true;
                                                *error = None;
                                            }
                                            spawn_album_detail_fetch(
                                                core,
                                                retry_id.clone(),
                                                gen_arc,
                                                req_gen,
                                                detail_stack,
                                            );
                                        }
                                    },
                                    IconRefreshCw { size: 13 }
                                    "Retry"
                                }
                            }
                        } else if let Some(album_detail) = detail {
                            RemoteAlbumDetailPanel {
                                detail: album_detail,
                                acquiring_map: acquiring_map.clone(),
                                downloading_albums: downloading_albums.clone(),
                                on_open_artist: move |(aid, aname): (Option<String>, String)| {
                                    if let Some(id) = aid.filter(|s| !s.trim().is_empty()) {
                                        open_remote_artist(id, Some(aname), false);
                                    } else if !aname.trim().is_empty() {
                                        open_remote_artist(String::new(), Some(aname), false);
                                    }
                                },
                            }
                        } else {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-6 text-[13px] text-beet-text-tertiary",
                                if artist_hint.is_empty() {
                                    "No album details available."
                                } else {
                                    "No album details available for {artist_hint}."
                                }
                            }
                        }
                    }
                }
                CatalogDetailView::Artist {
                    artist_id,
                    name,
                    loading,
                    detail,
                    error,
                } => {
                    let prev_label = if stack_snapshot.len() > 1 {
                        Some(stack_snapshot[stack_snapshot.len() - 2].breadcrumb_label())
                    } else {
                        None
                    };
                    let crumbs: Vec<String> = stack_snapshot
                        .iter()
                        .map(CatalogDetailView::breadcrumb_label)
                        .collect();
                    let retry_id = artist_id.clone();
                    let retry_name = name.clone();
                    rsx! {
                        DetailNavHeader {
                            prev_label: prev_label,
                            breadcrumbs: crumbs,
                            on_back_one: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().pop();
                            },
                            on_back_to_search: move |_| {
                                detail_gen.peek().fetch_add(1, Ordering::SeqCst);
                                detail_stack.write().clear();
                            },
                        }

                        if loading {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 flex items-center justify-center gap-3 text-beet-text-secondary",
                                IconLoader { size: 18, class: "animate-spin text-beet-leaf-text".to_string() }
                                span { class: "text-[14px]",
                                    if let Some(n) = name.as_deref().filter(|s| !s.is_empty()) {
                                        "Loading {n}..."
                                    } else {
                                        "Loading artist catalog..."
                                    }
                                }
                            }
                        } else if let Some(err_msg) = error {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-crimson-text/30 p-6 space-y-3",
                                p { class: "text-[14px] font-semibold text-beet-crimson-text",
                                    "Could not load artist catalog"
                                }
                                p { class: "text-[13px] text-beet-text-secondary", "{err_msg}" }
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid text-[12px] font-semibold cursor-pointer",
                                    onclick: move |_| {
                                        if let Some(core) = studio.core.peek().clone() {
                                            let gen_arc = detail_gen.peek().clone();
                                            let req_gen = gen_arc.fetch_add(1, Ordering::SeqCst) + 1;
                                            if let Some(CatalogDetailView::Artist { loading, error, .. }) =
                                                detail_stack.write().last_mut()
                                            {
                                                *loading = true;
                                                *error = None;
                                            }
                                            spawn_artist_detail_fetch(
                                                core,
                                                retry_id.clone(),
                                                retry_name.clone(),
                                                gen_arc,
                                                req_gen,
                                                detail_stack,
                                            );
                                        }
                                    },
                                    IconRefreshCw { size: 13 }
                                    "Retry"
                                }
                            }
                        } else if let Some(artist_detail) = detail {
                            RemoteArtistDetailPanel {
                                key: "{artist_detail.artist.id}",
                                detail: artist_detail,
                                acquiring_map: acquiring_map.clone(),
                                on_select_other_artist: move |other: RemoteCatalogArtist| {
                                    open_remote_artist(other.id, Some(other.name), true);
                                },
                                on_open_album: move |album: RemoteCatalogAlbum| {
                                    open_remote_album(album.id, album.title, album.artist);
                                },
                            }
                        } else {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-6 text-[13px] text-beet-text-tertiary",
                                "No artist details available."
                            }
                        }
                    }
                }
                CatalogDetailView::None => {
                    let trimmed_q = query().trim().to_string();
                    if trimmed_q.is_empty() {
                        rsx! {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-2",
                                IconSearch { size: 28, class: "mx-auto text-beet-text-disabled".to_string() }
                                p { class: "text-[15px] font-medium text-beet-text-secondary",
                                    "Instant local FTS5 + global catalog search"
                                }
                                p { class: "text-[13px] text-beet-text-tertiary max-w-md mx-auto",
                                    if offline_only {
                                        "Type any artist, track, album, or genre for instant offline results."
                                    } else {
                                        "Local library results appear in 0ms while remote artists, albums, and tracks stream in right below."
                                    }
                                }
                            }
                        }
                    } else {
                        let local_binding = local_res.read();
                        let local_data = local_binding.as_ref().and_then(|opt| opt.as_ref());

                        let (local_tracks, local_albums, local_artists, elapsed_ms) = match local_data {
                            Some((res, cached, disc)) => {
                                let res: &SearchResults = res;
                                let tracks: Vec<_> = res
                                    .local_tracks
                                    .iter()
                                    .map(|t| local_track_to_player_track(t, cached, disc))
                                    .collect();
                                (
                                    tracks,
                                    res.local_albums.clone(),
                                    res.local_artists.clone(),
                                    res.elapsed_ms,
                                )
                            }
                            None => (Vec::new(), Vec::new(), Vec::new(), 0),
                        };

                        let remote_data = if offline_only {
                            None
                        } else {
                            remote_results.read().clone()
                        };
                        let is_remote_loading = !offline_only && remote_loading();
                        let remote_err = if offline_only {
                            None
                        } else {
                            remote_error.read().clone()
                        };

                        let remote_tracks = remote_data
                            .as_ref()
                            .map(|r| r.tracks.clone())
                            .unwrap_or_default();
                        let remote_albums = remote_data
                            .as_ref()
                            .map(|r| r.albums.clone())
                            .unwrap_or_default();
                        let remote_artists = remote_data
                            .as_ref()
                            .map(|r| r.artists.clone())
                            .unwrap_or_default();

                        let total_tracks = local_tracks.len() + remote_tracks.len();
                        let total_albums = local_albums.len() + remote_albums.len();
                        let total_artists = local_artists.len() + remote_artists.len();
                        let total_all = total_tracks + total_albums + total_artists;

                        let filter = active_filter();
                        let show_tracks = matches!(filter, SearchFilter::All | SearchFilter::Tracks);
                        let show_albums = matches!(filter, SearchFilter::All | SearchFilter::Albums);
                        let show_artists = matches!(filter, SearchFilter::All | SearchFilter::Artists);

                        let has_visible_local = (show_artists && !local_artists.is_empty())
                            || (show_tracks && !local_tracks.is_empty())
                            || (show_albums && !local_albums.is_empty());
                        let has_visible_remote = (show_artists && !remote_artists.is_empty())
                            || (show_tracks && !remote_tracks.is_empty())
                            || (show_albums && !remote_albums.is_empty());

                        let remote_provider = remote_data
                            .as_ref()
                            .map(|r| r.provider.clone())
                            .unwrap_or_default();
                        let remote_fallback_used =
                            remote_data.as_ref().is_some_and(|r| r.fallback_used);

                        rsx! {
                            div { class: "space-y-6",
                                // Filter Pills + Timing Telemetry Row
                                div { class: "flex flex-wrap items-center justify-between gap-3",
                                    div { class: "flex flex-wrap items-center gap-1.5",
                                        for (pill, label, count) in [
                                            (SearchFilter::All, "All", total_all),
                                            (SearchFilter::Tracks, "Tracks", total_tracks),
                                            (SearchFilter::Albums, "Albums", total_albums),
                                            (SearchFilter::Artists, "Artists", total_artists),
                                        ] {
                                            {
                                                let active = filter == pill;
                                                rsx! {
                                                    button {
                                                        key: "filter-{label}",
                                                        r#type: "button",
                                                        aria_pressed: if active { "true" } else { "false" },
                                                        class: if active {
                                                            "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/45 text-beet-leaf-text text-[12px] font-semibold cursor-pointer transition-colors"
                                                        } else {
                                                            "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary hover:border-beet-border-default text-[12px] font-medium cursor-pointer transition-colors"
                                                        },
                                                        onclick: move |_| active_filter.set(pill),
                                                        span { "{label}" }
                                                        span { class: "tabular-nums-mono text-[10px] opacity-80",
                                                            "{count}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                        "{local_tracks.len()} LOCAL TRACKS · {local_albums.len()} ALBUMS · {local_artists.len()} ARTISTS · {elapsed_ms} MS"
                                    }
                                }

                                // SECTION 1: IN YOUR LIBRARY (0ms SQLite)
                                if has_visible_local {
                                    section { class: "space-y-5",
                                        div { class: "flex items-center justify-between",
                                            h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                                "In Your Library"
                                            }
                                        }

                                        // Local Artists
                                        if show_artists && !local_artists.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Artists ({local_artists.len()})"
                                                }
                                                div { class: "flex flex-wrap gap-2.5",
                                                    for artist in local_artists.iter() {
                                                        {
                                                            let artist_id = artist.id.clone();
                                                            let artist_name = artist.name.clone();
                                                            let cover_art_id = artist.cover_art_id.clone();
                                                            let art_url = desktop_cover_art_url(
                                                                artist.cover_art_id.as_deref(),
                                                            );
                                                            let album_label = if artist.album_count == 1 {
                                                                "1 album".to_string()
                                                            } else {
                                                                format!("{} albums", artist.album_count)
                                                            };
                                                            rsx! {
                                                                button {
                                                                    key: "la-{artist.id}",
                                                                    r#type: "button",
                                                                    class: "inline-flex items-center gap-2.5 pl-1.5 pr-3.5 py-1.5 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default hover:bg-beet-surface-high text-left cursor-pointer transition-colors",
                                                                    onclick: move |_| {
                                                                        open_local_artist(
                                                                            artist_id.clone(),
                                                                            artist_name.clone(),
                                                                            cover_art_id.clone(),
                                                                        );
                                                                    },
                                                                    RemoteArtistAvatar {
                                                                        url: art_url,
                                                                        name: artist.name.clone(),
                                                                        size_class: "w-8 h-8 text-[12px]".to_string(),
                                                                    }
                                                                    span { class: "min-w-0",
                                                                        span { class: "block text-[13px] font-semibold text-beet-text-primary truncate max-w-[180px]",
                                                                            "{artist.name}"
                                                                        }
                                                                        span { class: "block tabular-nums-mono text-[10px] text-beet-text-tertiary",
                                                                            "{album_label}"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Local Tracks
                                        if show_tracks && !local_tracks.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Tracks ({local_tracks.len()})"
                                                }
                                                div { class: "space-y-1", role: "list",
                                                    for (idx, track) in local_tracks.iter().enumerate() {
                                                        {
                                                            let queue = local_tracks.clone();
                                                            let t_star = track.clone();
                                                            let t_rate = track.clone();
                                                            let t_keep = track.clone();
                                                            let t_drop = track.clone();
                                                            let t_inspect = track.clone();
                                                            let row_playing = current_id.as_deref()
                                                                == Some(track.id.as_str())
                                                                && is_playing;
                                                            let row_state = if offline_only && !track.is_offline_cached {
                                                                TrackRowState::OfflineUnavailable
                                                            } else if track.is_discovery {
                                                                TrackRowState::PendingDiscovery {
                                                                    days_remaining: track.days_remaining,
                                                                    reason_label: track.reason_label.clone(),
                                                                    rating: track.rating,
                                                                }
                                                            } else {
                                                                TrackRowState::Owned {
                                                                    is_offline_cached: track.is_offline_cached,
                                                                    starred: track.starred,
                                                                    rating: track.rating,
                                                                }
                                                            };
                                                            rsx! {
                                                                TrackRow {
                                                                    key: "sr-{track.id}",
                                                                    title: track.title.clone(),
                                                                    artist: track.artist.clone(),
                                                                    album: track.album.clone(),
                                                                    cover_art_url: track.cover_art_url.clone(),
                                                                    duration_secs: track.duration_secs,
                                                                    codec_badge: track.codec_badge.clone(),
                                                                    is_playing: row_playing,
                                                                    state: row_state,
                                                                    on_play: move |_| player.play_queue(queue.clone(), idx),
                                                                    on_keep: move |_| studio.keep_track(player, t_keep.clone()),
                                                                    on_drop: move |_| studio.drop_track(player, t_drop.clone()),
                                                                    on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                                                    on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                                                    on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Local Albums
                                        if show_albums && !local_albums.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Albums ({local_albums.len()})"
                                                }
                                                div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3",
                                                    for album in local_albums.iter() {
                                                        {
                                                            let aid = album.id.clone();
                                                            let aname = album.name.clone();
                                                            let art = desktop_cover_art_url(
                                                                album.cover_art_id.as_deref(),
                                                            );
                                                            let year_text = album
                                                                .year
                                                                .filter(|y| *y > 0)
                                                                .map(|y| y.to_string());
                                                            rsx! {
                                                                button {
                                                                    key: "sa-{album.id}",
                                                                    r#type: "button",
                                                                    class: "group rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 text-left space-y-2 cursor-pointer transition-colors",
                                                                    onclick: move |_| {
                                                                        let id = aid.clone();
                                                                        let label = aname.clone();
                                                                        if let Some(core) = studio.core.peek().clone() {
                                                                            spawn(async move {
                                                                                let tracks = load_tracks_for_album(&core, &id).await;
                                                                                studio.play_collection(player, tracks, 0, &label);
                                                                            });
                                                                        }
                                                                    },
                                                                    div { class: "relative aspect-square w-full rounded-[6px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                                                        RemoteCoverImage {
                                                                            url: art,
                                                                            alt: album.name.clone(),
                                                                            icon_size: 22,
                                                                        }
                                                                        span { class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity",
                                                                            IconPlay { size: 13, filled: true }
                                                                        }
                                                                    }
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{album.name}" }
                                                                    p { class: "text-[12px] text-beet-text-secondary truncate",
                                                                        if let Some(yr) = year_text {
                                                                            "{album.artist_name} · {yr}"
                                                                        } else {
                                                                            "{album.artist_name}"
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

                                // SECTION 2: REMOTE CATALOG (when online)
                                if !offline_only {
                                    section { class: "space-y-5",
                                        div { class: "flex flex-wrap items-center justify-between gap-2",
                                            div { class: "flex items-center gap-2.5",
                                                h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                                    "Catalog"
                                                }
                                                if !remote_provider.is_empty() {
                                                    span { class: "px-2 py-0.5 rounded-md bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                                        "{remote_provider}"
                                                    }
                                                }
                                            }
                                            if is_remote_loading {
                                                span { class: "inline-flex items-center gap-1.5 text-[12px] text-beet-text-tertiary",
                                                    IconLoader { size: 13, class: "animate-spin text-beet-leaf-text".to_string() }
                                                    "Searching catalog..."
                                                }
                                            }
                                        }

                                        if let Some(err_msg) = remote_err.as_ref() {
                                            div { class: "rounded-[12px] bg-beet-crimson-surface/40 border border-beet-crimson-text/30 px-4 py-3 flex flex-wrap items-center justify-between gap-3",
                                                span { class: "text-[13px] text-beet-crimson-text",
                                                    "Catalog search failed: {err_msg}"
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "inline-flex items-center gap-1.5 h-7 px-3 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-primary text-[12px] font-medium cursor-pointer",
                                                    onclick: move |_| trigger_remote_now(None),
                                                    IconRefreshCw { size: 12 }
                                                    "Retry"
                                                }
                                            }
                                        }

                                        // Remote Artists
                                        if show_artists && !remote_artists.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Artists ({remote_artists.len()})"
                                                }
                                                div { class: "grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 gap-3",
                                                    for artist in remote_artists.iter() {
                                                        {
                                                            let aid = artist.id.clone();
                                                            let aname = artist.name.clone();
                                                            let subtitle = artist_subtitle(artist);
                                                            rsx! {
                                                                button {
                                                                    key: "ra-{artist.id}",
                                                                    r#type: "button",
                                                                    class: "group flex items-center gap-3 p-3 rounded-[12px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default hover:bg-beet-surface-high text-left cursor-pointer transition-colors",
                                                                    onclick: move |_| {
                                                                        open_remote_artist(aid.clone(), Some(aname.clone()), false);
                                                                    },
                                                                    RemoteArtistAvatar {
                                                                        url: artist.picture_url.clone(),
                                                                        name: artist.name.clone(),
                                                                        size_class: "w-12 h-12 text-[16px]".to_string(),
                                                                    }
                                                                    div { class: "flex-1 min-w-0",
                                                                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                                            "{artist.name}"
                                                                        }
                                                                        if let Some(sub) = subtitle {
                                                                            p { class: "text-[12px] text-beet-text-secondary truncate mt-0.5",
                                                                                "{sub}"
                                                                            }
                                                                        }
                                                                    }
                                                                    IconChevronRight {
                                                                        size: 16,
                                                                        class: "text-beet-text-tertiary group-hover:text-beet-text-primary shrink-0 transition-colors".to_string(),
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Remote Tracks (1-tap Acquire & Play)
                                        if show_tracks && !remote_tracks.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Tracks ({remote_tracks.len()})"
                                                }
                                                div { class: "space-y-1", role: "list",
                                                    for (idx, remote) in remote_tracks.iter().enumerate() {
                                                        {
                                                            let remote_clone = remote.clone();
                                                            let row_key = remote_track_acquire_key(remote);
                                                            let stage = acquiring_map.get(&row_key).cloned();
                                                            let badge = format_remote_track_badge(remote);
                                                            let album_opt = (!remote.album.trim().is_empty())
                                                                .then(|| remote.album.clone());
                                                            let duration_opt = (remote.duration_secs > 0)
                                                                .then_some(remote.duration_secs);
                                                            rsx! {
                                                                TrackRow {
                                                                    key: "rem-{idx}-{row_key}",
                                                                    title: remote.title.clone(),
                                                                    artist: remote.artist.clone(),
                                                                    album: album_opt,
                                                                    cover_art_url: remote.cover_url.clone(),
                                                                    duration_secs: duration_opt,
                                                                    codec_badge: badge,
                                                                    state: TrackRowState::UnownedCatalog { acquiring_stage: stage },
                                                                    on_acquire: move |_| {
                                                                        studio.acquire_and_play_remote(player, remote_clone.clone());
                                                                    },
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Remote Albums (clicking opens Remote Album Detail View)
                                        if show_albums && !remote_albums.is_empty() {
                                            div { class: "space-y-2",
                                                h4 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Albums ({remote_albums.len()})"
                                                }
                                                div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3",
                                                    for album in remote_albums.iter() {
                                                        {
                                                            let aid = album.id.clone();
                                                            let atitle = album.title.clone();
                                                            let aartist = album.artist.clone();
                                                            let meta_line = format_album_meta(album);
                                                            rsx! {
                                                                button {
                                                                    key: "ralb-{album.id}",
                                                                    r#type: "button",
                                                                    class: "group rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 text-left space-y-2 cursor-pointer transition-colors",
                                                                    onclick: move |_| {
                                                                        open_remote_album(
                                                                            aid.clone(),
                                                                            atitle.clone(),
                                                                            aartist.clone(),
                                                                        );
                                                                    },
                                                                    div { class: "relative aspect-square w-full rounded-[6px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                                                        RemoteCoverImage {
                                                                            url: album.cover_url.clone(),
                                                                            alt: album.title.clone(),
                                                                            icon_size: 22,
                                                                        }
                                                                    }
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate",
                                                                        "{album.title}"
                                                                    }
                                                                    p { class: "text-[12px] text-beet-text-secondary truncate",
                                                                        "{album.artist}"
                                                                    }
                                                                    if !meta_line.is_empty() {
                                                                        p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary truncate",
                                                                            "{meta_line}"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Deep Catalog Footer
                                        if remote_fallback_used || remote_provider == "deezer" || remote_provider == "musicbrainz" {
                                            div { class: "pt-2 border-t border-beet-border-subtle flex flex-wrap items-center justify-between gap-3",
                                                if remote_fallback_used {
                                                    p { class: "text-[12px] text-beet-text-tertiary",
                                                        "Showing deeper catalog results from MusicBrainz"
                                                    }
                                                } else if remote_provider == "deezer" {
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_remote_loading,
                                                        class: "inline-flex items-center gap-2 text-[12px] font-medium text-beet-text-secondary hover:text-beet-leaf-text disabled:opacity-50 cursor-pointer transition-colors",
                                                        onclick: move |_| trigger_remote_now(Some("musicbrainz")),
                                                        IconDisc3 { size: 14 }
                                                        span { "Didn't find a niche release? Search deeper catalog (MusicBrainz)" }
                                                    }
                                                } else if remote_provider == "musicbrainz" {
                                                    div { class: "flex flex-wrap items-center gap-3",
                                                        p { class: "text-[12px] text-beet-text-tertiary",
                                                            "Showing deeper catalog results from MusicBrainz"
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            disabled: is_remote_loading,
                                                            class: "text-[12px] font-medium text-beet-leaf-text hover:underline disabled:opacity-50 cursor-pointer",
                                                            onclick: move |_| trigger_remote_now(None),
                                                            "Return to default catalog"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Empty States
                                if !has_visible_local && !has_visible_remote && !is_remote_loading && remote_err.is_none() {
                                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-2.5",
                                        if filter != SearchFilter::All && total_all > 0 {
                                            p { class: "text-[13px] text-beet-text-secondary",
                                                "No matching {filter_plural_label(filter)} for \"{trimmed_q}\"."
                                            }
                                            button {
                                                r#type: "button",
                                                class: "inline-flex items-center h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary text-[12px] font-medium cursor-pointer",
                                                onclick: move |_| active_filter.set(SearchFilter::All),
                                                "Show all {total_all} results"
                                            }
                                        } else {
                                            p { class: "text-[13px] text-beet-text-tertiary",
                                                "No matches found for \"{trimmed_q}\"."
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

fn local_artist_matches(
    item_artist_id: Option<&str>,
    item_artist_name: &str,
    target_artist_id: &str,
    target_artist_key: &str,
) -> bool {
    if !target_artist_id.trim().is_empty() && item_artist_id == Some(target_artist_id) {
        return true;
    }
    !target_artist_key.is_empty() && canonical_artist_key(item_artist_name) == target_artist_key
}

fn spawn_local_artist_fetch(
    core: Arc<SoulbeetCore>,
    artist_id: String,
    artist_name: String,
    gen_arc: Arc<AtomicU64>,
    req_gen: u64,
    mut detail_stack: Signal<Vec<CatalogDetailView>>,
) {
    spawn(async move {
        let cached = load_cached_track_ids(&core).await;
        let disc = load_pending_discovery_map(&core).await;
        let result = core.search_local(&artist_name, 100).await;
        if gen_arc.load(Ordering::SeqCst) != req_gen {
            return;
        }
        let target_key = canonical_artist_key(&artist_name);
        if let Some(CatalogDetailView::LocalArtist {
            artist_id: active_id,
            artist_name: active_name,
            loading,
            albums,
            tracks,
            error,
            ..
        }) = detail_stack.write().last_mut()
        {
            if *active_id == artist_id && *active_name == artist_name {
                *loading = false;
                match result {
                    Ok(res) => {
                        *albums = res
                            .local_albums
                            .into_iter()
                            .filter(|a| {
                                local_artist_matches(
                                    a.artist_id.as_deref(),
                                    &a.artist_name,
                                    &artist_id,
                                    &target_key,
                                )
                            })
                            .collect();
                        *tracks = res
                            .local_tracks
                            .into_iter()
                            .filter(|t| {
                                local_artist_matches(
                                    t.artist_id.as_deref(),
                                    &t.artist_name,
                                    &artist_id,
                                    &target_key,
                                )
                            })
                            .map(|t| local_track_to_player_track(&t, &cached, &disc))
                            .collect();
                        *error = None;
                    }
                    Err(err) => {
                        *error = Some(err.to_string());
                    }
                }
            }
        }
    });
}

fn spawn_album_detail_fetch(
    core: Arc<SoulbeetCore>,
    album_id: String,
    gen_arc: Arc<AtomicU64>,
    req_gen: u64,
    mut detail_stack: Signal<Vec<CatalogDetailView>>,
) {
    spawn(async move {
        let result = core.get_remote_album(&album_id).await;
        if gen_arc.load(Ordering::SeqCst) != req_gen {
            return;
        }
        if let Some(CatalogDetailView::Album {
            album_id: active_id,
            loading,
            detail,
            error,
            ..
        }) = detail_stack.write().last_mut()
        {
            if *active_id == album_id {
                *loading = false;
                match result {
                    Ok(data) => {
                        *detail = Some(data);
                        *error = None;
                    }
                    Err(err) => {
                        *error = Some(err.to_string());
                    }
                }
            }
        }
    });
}

fn spawn_artist_detail_fetch(
    core: Arc<SoulbeetCore>,
    artist_id: String,
    name: Option<String>,
    gen_arc: Arc<AtomicU64>,
    req_gen: u64,
    mut detail_stack: Signal<Vec<CatalogDetailView>>,
) {
    spawn(async move {
        let result = core.get_remote_artist(&artist_id, name.as_deref()).await;
        if gen_arc.load(Ordering::SeqCst) != req_gen {
            return;
        }
        if let Some(CatalogDetailView::Artist {
            artist_id: active_id,
            loading,
            detail,
            error,
            ..
        }) = detail_stack.write().last_mut()
        {
            if *active_id == artist_id {
                *loading = false;
                match result {
                    Ok(Some(data)) => {
                        *detail = Some(data);
                        *error = None;
                    }
                    Ok(None) => {
                        *error = Some("Artist details were not found in the catalog.".to_string());
                    }
                    Err(err) => {
                        *error = Some(err.to_string());
                    }
                }
            }
        }
    });
}

#[component]
fn DetailNavHeader(
    prev_label: Option<String>,
    breadcrumbs: Vec<String>,
    on_back_one: EventHandler<()>,
    on_back_to_search: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "flex flex-wrap items-center justify-between gap-3",
            div { class: "flex flex-wrap items-center gap-2",
                if let Some(prev) = prev_label {
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-high text-[12px] font-semibold cursor-pointer transition-colors",
                        onclick: move |_| on_back_one.call(()),
                        IconChevronLeft { size: 14 }
                        span { "Back to {prev}" }
                    }
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1 h-8 px-3 rounded-full bg-beet-surface-low border border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| on_back_to_search.call(()),
                        "Back to search"
                    }
                } else {
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-high text-[12px] font-semibold cursor-pointer transition-colors",
                        onclick: move |_| on_back_to_search.call(()),
                        IconChevronLeft { size: 14 }
                        span { "Back to search" }
                    }
                }
            }

            if !breadcrumbs.is_empty() {
                div { class: "hidden sm:flex items-center gap-1.5 text-[12px] text-beet-text-tertiary min-w-0",
                    span { "Search" }
                    for (idx, crumb) in breadcrumbs.iter().enumerate() {
                        IconChevronRight { size: 12, class: "shrink-0".to_string() }
                        span {
                            class: if idx + 1 == breadcrumbs.len() {
                                "text-beet-text-primary font-medium truncate max-w-[200px]"
                            } else {
                                "truncate max-w-[160px]"
                            },
                            "{crumb}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn LocalArtistDetailPanel(
    artist_name: String,
    cover_art_id: Option<String>,
    albums: Vec<LocalAlbum>,
    tracks: Vec<PlayerTrack>,
    offline_only: bool,
    current_id: Option<String>,
    is_playing: bool,
    on_explore_catalog: EventHandler<()>,
) -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let art_url = desktop_cover_art_url(cover_art_id.as_deref());
    let album_label = if albums.len() == 1 {
        "1 ALBUM".to_string()
    } else {
        format!("{} ALBUMS", albums.len())
    };
    let track_label = if tracks.len() == 1 {
        "1 TRACK".to_string()
    } else {
        format!("{} TRACKS", tracks.len())
    };
    let play_tracks = tracks.clone();
    let play_label = artist_name.clone();

    rsx! {
        div { class: "space-y-6",
            // Local Artist Hero Card
            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 flex flex-col sm:flex-row gap-5 items-start sm:items-center",
                RemoteArtistAvatar {
                    url: art_url,
                    name: artist_name.clone(),
                    size_class: "w-24 h-24 sm:w-28 sm:h-28 text-[30px]".to_string(),
                }

                div { class: "flex-1 min-w-0 space-y-3",
                    div { class: "space-y-1",
                        span { class: "inline-block px-2 py-0.5 rounded-md bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                            "In Your Library"
                        }
                        h2 { class: "text-[24px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight",
                            "{artist_name}"
                        }
                        p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "{album_label} · {track_label}"
                        }
                    }

                    div { class: "flex flex-wrap items-center gap-2.5 pt-0.5",
                        if !tracks.is_empty() {
                            button {
                                r#type: "button",
                                class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 text-[13px] font-semibold cursor-pointer transition-[filter]",
                                onclick: move |_| {
                                    studio.play_collection(player, play_tracks.clone(), 0, &play_label);
                                },
                                IconPlay { size: 13, filled: true }
                                span { "Play All" }
                            }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default hover:border-beet-leaf-solid/50 text-beet-text-primary text-[13px] font-semibold cursor-pointer transition-colors",
                            onclick: move |_| on_explore_catalog.call(()),
                            IconDisc3 { size: 14 }
                            span { "Explore full catalog" }
                        }
                    }
                }
            }

            // Local Albums
            if !albums.is_empty() {
                section { class: "space-y-2",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Albums ({albums.len()})"
                    }
                    div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3",
                        for album in albums.iter() {
                            {
                                let aid = album.id.clone();
                                let aname = album.name.clone();
                                let art = desktop_cover_art_url(album.cover_art_id.as_deref());
                                let year_text = album
                                    .year
                                    .filter(|y| *y > 0)
                                    .map(|y| y.to_string());
                                rsx! {
                                    button {
                                        key: "la-alb-{album.id}",
                                        r#type: "button",
                                        class: "group rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 text-left space-y-2 cursor-pointer transition-colors",
                                        onclick: move |_| {
                                            let id = aid.clone();
                                            let label = aname.clone();
                                            if let Some(core) = studio.core.peek().clone() {
                                                spawn(async move {
                                                    let album_tracks = load_tracks_for_album(&core, &id).await;
                                                    studio.play_collection(player, album_tracks, 0, &label);
                                                });
                                            }
                                        },
                                        div { class: "relative aspect-square w-full rounded-[6px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                            RemoteCoverImage {
                                                url: art,
                                                alt: album.name.clone(),
                                                icon_size: 22,
                                            }
                                            span { class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity",
                                                IconPlay { size: 13, filled: true }
                                            }
                                        }
                                        p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{album.name}" }
                                        p { class: "text-[12px] text-beet-text-secondary truncate",
                                            if let Some(yr) = year_text {
                                                "{album.artist_name} · {yr}"
                                            } else {
                                                "{album.artist_name}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Local Tracks
            if !tracks.is_empty() {
                section { class: "space-y-2",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Tracks ({tracks.len()})"
                    }
                    div { class: "space-y-1", role: "list",
                        for (idx, track) in tracks.iter().enumerate() {
                            {
                                let queue = tracks.clone();
                                let row_artist_label = artist_name.clone();
                                let t_star = track.clone();
                                let t_rate = track.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_inspect = track.clone();
                                let row_playing = current_id.as_deref()
                                    == Some(track.id.as_str())
                                    && is_playing;
                                let row_state = if offline_only && !track.is_offline_cached {
                                    TrackRowState::OfflineUnavailable
                                } else if track.is_discovery {
                                    TrackRowState::PendingDiscovery {
                                        days_remaining: track.days_remaining,
                                        reason_label: track.reason_label.clone(),
                                        rating: track.rating,
                                    }
                                } else {
                                    TrackRowState::Owned {
                                        is_offline_cached: track.is_offline_cached,
                                        starred: track.starred,
                                        rating: track.rating,
                                    }
                                };
                                rsx! {
                                    TrackRow {
                                        key: "la-trk-{track.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        duration_secs: track.duration_secs,
                                        codec_badge: track.codec_badge.clone(),
                                        is_playing: row_playing,
                                        state: row_state,
                                        on_play: move |_| {
                                            studio.play_collection(player, queue.clone(), idx, &row_artist_label);
                                        },
                                        on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                        on_keep: move |_| studio.keep_track(player, t_keep.clone()),
                                        on_drop: move |_| studio.drop_track(player, t_drop.clone()),
                                        on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                        on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if albums.is_empty() && tracks.is_empty() {
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-tertiary",
                    "No local albums or tracks found for {artist_name}."
                }
            }
        }
    }
}

#[component]
fn RemoteAlbumDetailPanel(
    detail: RemoteAlbumDetail,
    acquiring_map: HashMap<String, AcquireStageBadge>,
    downloading_albums: HashMap<String, RemoteAlbumDownloadState>,
    on_open_artist: EventHandler<(Option<String>, String)>,
) -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let album = detail.album.clone();
    let album_key = remote_album_download_key(&album);
    let download_state = downloading_albums.get(&album_key).cloned();
    let track_count = album
        .track_count
        .map(|c| c as usize)
        .unwrap_or(detail.tracks.len());
    let release_display = album
        .release_date
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let album_for_download = album.clone();
    let artist_id_click = album
        .artist_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            detail
                .tracks
                .iter()
                .find_map(|t| t.artist_id.clone().filter(|s| !s.trim().is_empty()))
        });
    let artist_name_click = album.artist.clone();

    rsx! {
        div { class: "space-y-6",
            // Album Hero Card
            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 flex flex-col sm:flex-row gap-5 items-start sm:items-center",
                div { class: "w-36 h-36 sm:w-44 sm:h-44 rounded-[10px] overflow-hidden bg-beet-surface-high border border-beet-border-subtle shrink-0 flex items-center justify-center",
                    RemoteCoverImage {
                        url: album.cover_url.clone(),
                        alt: album.title.clone(),
                        icon_size: 36,
                    }
                }

                div { class: "flex-1 min-w-0 space-y-3",
                    div { class: "space-y-1",
                        div { class: "flex flex-wrap items-center gap-1.5",
                            if !album.provider.trim().is_empty() {
                                span { class: "px-2 py-0.5 rounded-md bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                    "{album.provider}"
                                }
                            }
                            for rtype in album.release_types.iter() {
                                span {
                                    key: "rt-{rtype}",
                                    class: "px-2 py-0.5 rounded-md bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                    "{rtype}"
                                }
                            }
                        }

                        h2 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight",
                            "{album.title}"
                        }

                        button {
                            r#type: "button",
                            class: "text-[14px] font-semibold text-beet-leaf-text hover:underline cursor-pointer text-left",
                            onclick: move |_| {
                                on_open_artist.call((
                                    artist_id_click.clone(),
                                    artist_name_click.clone(),
                                ));
                            },
                            "{album.artist}"
                        }
                    }

                    p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                        if let Some(ref date) = release_display {
                            "{date} · {track_count} TRACKS"
                        } else {
                            "{track_count} TRACKS"
                        }
                    }

                    // Primary Download Album Action + Live Feedback
                    div { class: "flex flex-wrap items-center gap-3 pt-1",
                        match download_state {
                            Some(RemoteAlbumDownloadState::Downloading) => rsx! {
                                button {
                                    r#type: "button",
                                    disabled: true,
                                    class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/40 text-beet-leaf-text text-[13px] font-semibold cursor-wait",
                                    IconLoader { size: 14, class: "animate-spin".to_string() }
                                    span { "Downloading..." }
                                }
                            },
                            Some(RemoteAlbumDownloadState::Queued { .. }) => rsx! {
                                span { class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/45 text-beet-leaf-text text-[13px] font-semibold",
                                    IconCheck { size: 14 }
                                    span { "Queued in Activity" }
                                }
                            },
                            Some(RemoteAlbumDownloadState::Failed(ref err_msg)) => rsx! {
                                div { class: "flex flex-wrap items-center gap-2.5",
                                    button {
                                        r#type: "button",
                                        class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/35 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white text-[13px] font-semibold cursor-pointer transition-colors",
                                        onclick: move |_| {
                                            studio.download_remote_album(album_for_download.clone());
                                        },
                                        IconRefreshCw { size: 14 }
                                        span { "Retry Download" }
                                    }
                                    span { class: "text-[12px] text-beet-crimson-text",
                                        "{err_msg}"
                                    }
                                }
                            },
                            None => rsx! {
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 text-[13px] font-semibold cursor-pointer transition-[filter]",
                                    onclick: move |_| {
                                        studio.download_remote_album(album_for_download.clone());
                                    },
                                    IconDownload { size: 14 }
                                    span { "Download Album" }
                                }
                            },
                        }
                    }
                }
            }

            // Ordered Tracklist with per-track 1-tap Acquire & Play
            section { class: "space-y-2",
                h3 { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                    "Tracklist ({detail.tracks.len()})"
                }
                if detail.tracks.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-tertiary",
                        "No tracklist available from this catalog provider."
                    }
                } else {
                    div { class: "space-y-1", role: "list",
                        for (idx, track) in detail.tracks.iter().enumerate() {
                            {
                                let mut track_for_acquire = track.clone();
                                if track_for_acquire.album.trim().is_empty() {
                                    track_for_acquire.album = album.title.clone();
                                }
                                if track_for_acquire.cover_url.is_none() {
                                    track_for_acquire.cover_url = album.cover_url.clone();
                                }
                                let row_key = remote_track_acquire_key(&track_for_acquire);
                                let stage = acquiring_map.get(&row_key).cloned();
                                let badge = format_remote_track_badge(&track_for_acquire);
                                let duration_opt = (track_for_acquire.duration_secs > 0)
                                    .then_some(track_for_acquire.duration_secs);
                                let album_opt = (!track_for_acquire.album.trim().is_empty())
                                    .then(|| track_for_acquire.album.clone());
                                rsx! {
                                    TrackRow {
                                        key: "alb-trk-{idx}-{row_key}",
                                        track_number: Some((idx + 1) as u32),
                                        title: track_for_acquire.title.clone(),
                                        artist: track_for_acquire.artist.clone(),
                                        album: album_opt,
                                        cover_art_url: track_for_acquire.cover_url.clone(),
                                        duration_secs: duration_opt,
                                        codec_badge: badge,
                                        state: TrackRowState::UnownedCatalog { acquiring_stage: stage },
                                        on_acquire: move |_| {
                                            studio.acquire_and_play_remote(player, track_for_acquire.clone());
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

#[component]
fn RemoteArtistDetailPanel(
    detail: RemoteArtistDetail,
    acquiring_map: HashMap<String, AcquireStageBadge>,
    on_select_other_artist: EventHandler<RemoteCatalogArtist>,
    on_open_album: EventHandler<RemoteCatalogAlbum>,
) -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let mut disc_filter = use_signal(|| DiscographyFilter::All);
    let artist_id_key = detail.artist.id.clone();
    use_effect(use_reactive(&artist_id_key, move |_| {
        disc_filter.set(DiscographyFilter::All);
    }));
    let artist = detail.artist.clone();

    let albums_count = detail
        .albums
        .iter()
        .filter(|a| classify_discography_group(&a.release_types) == DiscographyFilter::Albums)
        .count();
    let eps_count = detail
        .albums
        .iter()
        .filter(|a| classify_discography_group(&a.release_types) == DiscographyFilter::Eps)
        .count();
    let singles_count = detail
        .albums
        .iter()
        .filter(|a| classify_discography_group(&a.release_types) == DiscographyFilter::Singles)
        .count();

    let active_disc_filter = disc_filter();
    let filtered_albums: Vec<RemoteCatalogAlbum> = detail
        .albums
        .iter()
        .filter(|a| {
            active_disc_filter == DiscographyFilter::All
                || classify_discography_group(&a.release_types) == active_disc_filter
        })
        .cloned()
        .collect();

    rsx! {
        div { class: "space-y-6",
            // Artist Hero Card
            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                div { class: "flex flex-col sm:flex-row gap-5 items-start sm:items-center",
                    RemoteArtistAvatar {
                        url: artist.picture_url.clone(),
                        name: artist.name.clone(),
                        size_class: "w-24 h-24 sm:w-28 sm:h-28 text-[30px]".to_string(),
                    }

                    div { class: "flex-1 min-w-0 space-y-2",
                        div { class: "flex flex-wrap items-center gap-2",
                            if !detail.provider.trim().is_empty() {
                                span { class: "px-2 py-0.5 rounded-md bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                    "{detail.provider}"
                                }
                            }
                            if let Some(fans) = artist.fan_count.filter(|f| *f > 0) {
                                span { class: "tabular-nums-mono text-[11px] font-semibold text-beet-text-tertiary",
                                    "{format_fan_count(fans)}"
                                }
                            }
                        }

                        h2 { class: "text-[24px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight",
                            "{artist.name}"
                        }

                        if let Some(disambig) = artist
                            .disambiguation
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            p { class: "text-[13px] text-beet-text-secondary",
                                "{disambig}"
                            }
                        }
                    }
                }

                // MusicBrainz disambiguation chips ("others")
                if !detail.others.is_empty() {
                    div { class: "pt-3 border-t border-beet-border-subtle space-y-2",
                        p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "Other matching artists"
                        }
                        div { class: "flex flex-wrap gap-2",
                            for other in detail.others.iter() {
                                {
                                    let other_clone = other.clone();
                                    let label = match other
                                        .disambiguation
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                    {
                                        Some(d) => format!("{} ({})", other.name, d),
                                        None => other.name.clone(),
                                    };
                                    rsx! {
                                        button {
                                            key: "other-{other.id}",
                                            r#type: "button",
                                            class: "inline-flex items-center h-7 px-3 rounded-full bg-beet-surface-high border border-beet-border-subtle hover:border-beet-leaf-solid/50 text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer transition-colors",
                                            onclick: move |_| on_select_other_artist.call(other_clone.clone()),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Top Tracks
            if !detail.top_tracks.is_empty() {
                section { class: "space-y-2",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Top Tracks"
                    }
                    div { class: "space-y-1", role: "list",
                        for (idx, track) in detail.top_tracks.iter().enumerate() {
                            {
                                let track_clone = track.clone();
                                let row_key = remote_track_acquire_key(track);
                                let stage = acquiring_map.get(&row_key).cloned();
                                let badge = format_remote_track_badge(track);
                                let duration_opt = (track.duration_secs > 0)
                                    .then_some(track.duration_secs);
                                let album_opt = (!track.album.trim().is_empty())
                                    .then(|| track.album.clone());
                                rsx! {
                                    TrackRow {
                                        key: "art-top-{idx}-{row_key}",
                                        track_number: Some((idx + 1) as u32),
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: album_opt,
                                        cover_art_url: track.cover_url.clone(),
                                        duration_secs: duration_opt,
                                        codec_badge: badge,
                                        state: TrackRowState::UnownedCatalog { acquiring_stage: stage },
                                        on_acquire: move |_| {
                                            studio.acquire_and_play_remote(player, track_clone.clone());
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Discography (Albums, EPs, Singles)
            section { class: "space-y-3",
                div { class: "flex flex-wrap items-center justify-between gap-3",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Discography"
                    }

                    if !detail.albums.is_empty() {
                        div { class: "flex flex-wrap items-center gap-1.5",
                            for (pill, label, count) in [
                                (DiscographyFilter::All, "All", detail.albums.len()),
                                (DiscographyFilter::Albums, "Albums", albums_count),
                                (DiscographyFilter::Eps, "EPs", eps_count),
                                (DiscographyFilter::Singles, "Singles", singles_count),
                            ] {
                                if pill == DiscographyFilter::All || count > 0 {
                                    {
                                        let active = active_disc_filter == pill;
                                        rsx! {
                                            button {
                                                key: "df-{label}",
                                                r#type: "button",
                                                class: if active {
                                                    "inline-flex items-center gap-1.5 h-7 px-2.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/45 text-beet-leaf-text text-[11px] font-semibold cursor-pointer"
                                                } else {
                                                    "inline-flex items-center gap-1.5 h-7 px-2.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary text-[11px] font-medium cursor-pointer"
                                                },
                                                onclick: move |_| disc_filter.set(pill),
                                                span { "{label}" }
                                                span { class: "tabular-nums-mono text-[10px] opacity-80", "{count}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if filtered_albums.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-tertiary",
                        "No releases found in this category."
                    }
                } else {
                    div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3",
                        for album in filtered_albums.iter() {
                            {
                                let album_click = album.clone();
                                let meta_line = format_album_meta(album);
                                rsx! {
                                    button {
                                        key: "disc-{album.id}",
                                        r#type: "button",
                                        class: "group rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 text-left space-y-2 cursor-pointer transition-colors",
                                        onclick: move |_| on_open_album.call(album_click.clone()),
                                        div { class: "relative aspect-square w-full rounded-[6px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                            RemoteCoverImage {
                                                url: album.cover_url.clone(),
                                                alt: album.title.clone(),
                                                icon_size: 22,
                                            }
                                        }
                                        p { class: "text-[13px] font-semibold text-beet-text-primary truncate",
                                            "{album.title}"
                                        }
                                        if !meta_line.is_empty() {
                                            p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary truncate",
                                                "{meta_line}"
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

#[component]
fn RemoteCoverImage(url: Option<String>, alt: String, icon_size: u32) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let active_url = url
        .as_ref()
        .filter(|u| !u.trim().is_empty() && failed_url.read().as_ref() != Some(*u))
        .cloned();

    if let Some(u) = active_url {
        let u_err = u.clone();
        rsx! {
            img {
                src: "{u}",
                alt: "{alt}",
                class: "w-full h-full object-cover",
                loading: "lazy",
                decoding: "async",
                onerror: move |_| failed_url.set(Some(u_err.clone())),
            }
        }
    } else {
        rsx! {
            IconMusic { size: icon_size, class: "text-beet-text-disabled".to_string() }
        }
    }
}

#[component]
fn RemoteArtistAvatar(url: Option<String>, name: String, size_class: String) -> Element {
    let mut failed_url = use_signal(|| None::<String>);
    let active_url = url
        .as_ref()
        .filter(|u| !u.trim().is_empty() && failed_url.read().as_ref() != Some(*u))
        .cloned();
    let initial = artist_initial(&name);

    rsx! {
        div { class: "rounded-full overflow-hidden bg-beet-surface-high border border-beet-border-subtle shrink-0 flex items-center justify-center font-semibold text-beet-text-secondary {size_class}",
            if let Some(u) = active_url {
                {
                    let u_err = u.clone();
                    rsx! {
                        img {
                            src: "{u}",
                            alt: "{name}",
                            class: "w-full h-full object-cover",
                            loading: "lazy",
                            decoding: "async",
                            onerror: move |_| failed_url.set(Some(u_err.clone())),
                        }
                    }
                }
            } else {
                span { "{initial}" }
            }
        }
    }
}

fn filter_plural_label(filter: SearchFilter) -> &'static str {
    match filter {
        SearchFilter::All => "results",
        SearchFilter::Tracks => "tracks",
        SearchFilter::Albums => "albums",
        SearchFilter::Artists => "artists",
    }
}

fn artist_initial(name: &str) -> String {
    name.trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn format_fan_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M fans", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K fans", count as f64 / 1_000.0)
    } else if count == 1 {
        "1 fan".to_string()
    } else {
        format!("{count} fans")
    }
}

fn artist_subtitle(artist: &RemoteCatalogArtist) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(fans) = artist.fan_count.filter(|f| *f > 0) {
        parts.push(format_fan_count(fans));
    }
    if let Some(d) = artist
        .disambiguation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(d.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn extract_year(release_date: Option<&str>) -> Option<&str> {
    let candidate = release_date?.trim().get(..4)?;
    if candidate.bytes().all(|b| b.is_ascii_digit()) {
        Some(candidate)
    } else {
        None
    }
}

fn format_album_meta(album: &RemoteCatalogAlbum) -> String {
    let mut parts = Vec::new();
    if let Some(yr) = extract_year(album.release_date.as_deref()) {
        parts.push(yr.to_string());
    }
    if let Some(primary_type) = album
        .release_types
        .first()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        parts.push(primary_type.to_string());
    }
    if let Some(count) = album.track_count.filter(|c| *c > 0) {
        if count == 1 {
            parts.push("1 track".to_string());
        } else {
            parts.push(format!("{count} tracks"));
        }
    }
    parts.join(" · ")
}

fn format_remote_track_badge(track: &RemoteCatalogTrack) -> Option<String> {
    let variant = track
        .variant_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase);
    let provider = {
        let trimmed = track.provider.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_uppercase())
        }
    };
    match (variant, provider) {
        (Some(v), Some(p)) => Some(format!("{v} · {p}")),
        (Some(v), None) => Some(v),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    }
}

fn classify_discography_group(release_types: &[String]) -> DiscographyFilter {
    for rt in release_types {
        let lower = rt.trim().to_ascii_lowercase();
        if lower == "single" {
            return DiscographyFilter::Singles;
        }
        if lower == "ep" {
            return DiscographyFilter::Eps;
        }
        if lower == "album" {
            return DiscographyFilter::Albums;
        }
    }
    DiscographyFilter::Albums
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_fan_counts_and_artist_initials() {
        assert_eq!(format_fan_count(1), "1 fan");
        assert_eq!(format_fan_count(820), "820 fans");
        assert_eq!(format_fan_count(45_200), "45.2K fans");
        assert_eq!(format_fan_count(1_260_000), "1.3M fans");
        assert_eq!(artist_initial("  Édith Piaf"), "É");
        assert_eq!(artist_initial("..."), "?");
    }

    #[test]
    fn formats_remote_track_badges_with_variant_and_provider() {
        let mut track = RemoteCatalogTrack {
            id: "deezer:track:1".into(),
            mbid: String::new(),
            release_mbid: None,
            title: "Song (Live)".into(),
            artist: "Artist".into(),
            artist_id: None,
            album: "Album".into(),
            album_id: None,
            duration_secs: 200,
            year: Some(2020),
            cover_url: None,
            variant_label: Some("Live".into()),
            provider: "deezer".into(),
        };
        assert_eq!(
            format_remote_track_badge(&track).as_deref(),
            Some("LIVE · DEEZER")
        );

        track.variant_label = None;
        track.provider = "musicbrainz".into();
        assert_eq!(
            format_remote_track_badge(&track).as_deref(),
            Some("MUSICBRAINZ")
        );
    }

    #[test]
    fn extracts_release_year_and_formats_album_meta() {
        assert_eq!(extract_year(Some("1976-05-21")), Some("1976"));
        assert_eq!(extract_year(Some("abcd")), None);
        assert_eq!(extract_year(None), None);

        let album = RemoteCatalogAlbum {
            id: "deezer:album:10".into(),
            title: "Agents of Fortune".into(),
            artist: "Blue Öyster Cult".into(),
            artist_id: Some("deezer:artist:1".into()),
            release_date: Some("1976-05-21".into()),
            cover_url: None,
            mbid: None,
            release_types: vec!["Album".into()],
            track_count: Some(10),
            provider: "deezer".into(),
        };
        assert_eq!(format_album_meta(&album), "1976 · Album · 10 tracks");
        assert_eq!(
            classify_discography_group(&[String::from("EP")]),
            DiscographyFilter::Eps
        );
        assert_eq!(
            classify_discography_group(&[String::from("Single")]),
            DiscographyFilter::Singles
        );
    }

    #[test]
    fn local_artist_matches_by_id_or_canonical_unicode_key() {
        let key = canonical_artist_key("Blue Öyster Cult");
        assert!(local_artist_matches(
            Some("ar-1"),
            "Blue Oyster Cult",
            "ar-1",
            &key,
        ));
        assert!(local_artist_matches(
            None,
            "Blue Oyster Cult feat. Guest",
            "ar-1",
            &key,
        ));
        assert!(!local_artist_matches(
            Some("ar-2"),
            "Boards of Canada",
            "ar-1",
            &key,
        ));
    }
}
