//! Studio Library view: Playlists browser, Albums grid, Starred list,
//! Pending Discovery tracks, and Library Stats overview.

use std::collections::HashSet;

use dioxus::prelude::*;
use shared::navidrome::{LibraryStats, TopPlayedTrack};
use ui::{
    dashboard::StatsOverview, friendly_error, resolve_playlist_mix_sleeve_slug, use_player,
    web_cover_art_url, ArtFallbackKind, CoverImage, IconListPlus, IconLoader, IconPlay, IconPlus,
    IconRefreshCw, PlayerTrack, PlaylistArtwork, PlaylistCategory, PlaylistImportModal, TrackRow,
    TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{
    fetch_album_list, fetch_album_tracks, fetch_artists, fetch_library_tracks,
    fetch_playlist_tracks, AlbumListSort, SubsonicAlbumSummary, SubsonicArtistSummary,
    SubsonicPlaylistSummary, ALBUM_LIST_PAGE_SIZE,
};
use crate::web_player::use_studio;
use crate::Route;

const TRACKS_PAGE_SIZE: u32 = 100;

/// Merges a fetched `getAlbumList2` page into `existing`, deduplicating album IDs
/// while advancing the server offset by the raw page length returned by Navidrome.
fn merge_album_page(
    existing: &mut Vec<SubsonicAlbumSummary>,
    page: Vec<SubsonicAlbumSummary>,
    offset: u32,
    page_size: u32,
) -> (u32, bool) {
    let raw_len = page.len() as u32;
    let has_more = raw_len >= page_size && page_size > 0;
    let next_offset = offset.saturating_add(raw_len);
    let mut seen: HashSet<String> = existing.iter().map(|a| a.id.clone()).collect();
    for alb in page {
        if seen.insert(alb.id.clone()) {
            existing.push(alb);
        }
    }
    (next_offset, has_more)
}

fn albums_badge_text(count: usize, has_more: bool) -> Option<String> {
    if count == 0 {
        None
    } else if has_more {
        Some(format!("{count}+"))
    } else {
        Some(count.to_string())
    }
}

fn empty_albums_message(has_navidrome: bool, offline_only: bool, sort: AlbumListSort) -> &'static str {
    if offline_only {
        "Offline Only mode is active. Turn it off to browse albums from Navidrome."
    } else if !has_navidrome {
        "Connect Navidrome in Settings to browse your albums."
    } else if sort == AlbumListSort::Recent {
        "No recently played albums yet. Play an album or switch to A-Z to see your full library."
    } else if sort == AlbumListSort::Frequent {
        "No played albums yet. Switch to A-Z or Newest to see your full library."
    } else {
        "No albums found in Navidrome."
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Playlists,
    Albums,
    Artists,
    Tracks,
    Starred,
    Discovery,
    Stats,
}

#[component]
pub fn LibraryPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let navigator = use_navigator();

    let mut active_tab = use_signal(|| LibraryTab::Playlists);
    let mut import_modal_open = use_signal(|| false);
    let on_view_activity = use_callback(move |()| {
        import_modal_open.set(false);
        navigator.push(Route::ActivityPage {});
    });

    let mut album_sort = use_signal(AlbumListSort::default);
    let mut albums_list = use_signal(Vec::<SubsonicAlbumSummary>::new);
    let mut albums_next_offset = use_signal(|| 0u32);
    let mut albums_has_more = use_signal(|| false);
    let mut albums_loading = use_signal(|| false);
    let mut albums_loading_more = use_signal(|| false);
    let mut albums_error = use_signal(|| None::<String>);
    let mut albums_req_id = use_signal(|| 0u64);
    let mut loaded_albums_key = use_signal(|| None::<(String, String, AlbumListSort)>);

    // Artists tab state
    let mut artists_list = use_signal(Vec::<SubsonicArtistSummary>::new);
    let mut artists_loading = use_signal(|| false);
    let mut artists_error = use_signal(|| None::<String>);
    let mut artists_req_id = use_signal(|| 0u64);
    let mut loaded_artists_key = use_signal(|| None::<(String, String)>);

    // Tracks tab state (paginated)
    let mut tracks_list = use_signal(Vec::<PlayerTrack>::new);
    let mut tracks_offset = use_signal(|| 0u32);
    let mut tracks_has_more = use_signal(|| false);
    let mut tracks_loading = use_signal(|| false);
    let mut tracks_loading_more = use_signal(|| false);
    let mut tracks_error = use_signal(|| None::<String>);
    let mut tracks_req_id = use_signal(|| 0u64);
    let mut loaded_tracks_key = use_signal(|| None::<(String, String)>);

    let load_artists = use_callback(move |()| {
        if player.offline_only() {
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        let req_id = artists_req_id.peek().wrapping_add(1);
        artists_req_id.set(req_id);
        loaded_artists_key.set(Some((info.base_url.clone(), info.username.clone())));
        artists_loading.set(true);
        artists_error.set(None);
        spawn(async move {
            let res = fetch_artists(&info).await;
            if *artists_req_id.peek() != req_id {
                return;
            }
            match res {
                Ok(list) => {
                    artists_list.set(list);
                    artists_error.set(None);
                }
                Err(e) => {
                    loaded_artists_key.set(None);
                    artists_error.set(Some(e));
                }
            }
            artists_loading.set(false);
        });
    });

    let load_tracks_page = use_callback(move |append: bool| {
        if player.offline_only() {
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        if append && (*tracks_loading.peek() || *tracks_loading_more.peek()) {
            return;
        }
        let offset = if append { *tracks_offset.peek() } else { 0 };
        let req_id = tracks_req_id.peek().wrapping_add(1);
        tracks_req_id.set(req_id);
        if append {
            tracks_loading_more.set(true);
        } else {
            loaded_tracks_key.set(Some((info.base_url.clone(), info.username.clone())));
            tracks_loading.set(true);
            tracks_loading_more.set(false);
            tracks_error.set(None);
            tracks_list.set(Vec::new());
            tracks_offset.set(0);
            tracks_has_more.set(false);
        }
        spawn(async move {
            let res = fetch_library_tracks(&info, offset, TRACKS_PAGE_SIZE).await;
            if *tracks_req_id.peek() != req_id {
                return;
            }
            match res {
                Ok(page) => {
                    let raw_len = page.len() as u32;
                    let has_more = raw_len >= TRACKS_PAGE_SIZE;
                    let mut next = if append {
                        tracks_list.peek().clone()
                    } else {
                        Vec::new()
                    };
                    let mut seen: HashSet<String> = next.iter().map(|t| t.id.clone()).collect();
                    for t in page {
                        if seen.insert(t.id.clone()) {
                            next.push(t);
                        }
                    }
                    tracks_list.set(next);
                    tracks_offset.set(offset.saturating_add(raw_len));
                    tracks_has_more.set(has_more);
                    tracks_error.set(None);
                }
                Err(e) => {
                    if append {
                        studio.show_toast(format!("Could not load more tracks: {e}"));
                    } else {
                        loaded_tracks_key.set(None);
                        tracks_error.set(Some(e));
                    }
                }
            }
            tracks_loading.set(false);
            tracks_loading_more.set(false);
        });
    });

    // Lazy-load Artists or Tracks and invalidate stale session data on Navidrome session changes.
    use_effect(move || {
        let tab = active_tab();
        let offline = player.offline_only();
        let nav = studio
            .bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone());
        match nav {
            Some(info) if !offline => {
                let session_key = (info.base_url.clone(), info.username.clone());
                if loaded_artists_key
                    .peek()
                    .as_ref()
                    .is_some_and(|k| k != &session_key)
                {
                    let next_req = artists_req_id.peek().wrapping_add(1);
                    artists_req_id.set(next_req);
                    loaded_artists_key.set(None);
                    artists_list.set(Vec::new());
                    artists_error.set(None);
                    artists_loading.set(false);
                }
                if loaded_tracks_key
                    .peek()
                    .as_ref()
                    .is_some_and(|k| k != &session_key)
                {
                    let next_req = tracks_req_id.peek().wrapping_add(1);
                    tracks_req_id.set(next_req);
                    loaded_tracks_key.set(None);
                    tracks_list.set(Vec::new());
                    tracks_offset.set(0);
                    tracks_has_more.set(false);
                    tracks_error.set(None);
                    tracks_loading.set(false);
                    tracks_loading_more.set(false);
                }
                if tab == LibraryTab::Artists
                    && loaded_artists_key.peek().as_ref() != Some(&session_key)
                {
                    load_artists.call(());
                } else if tab == LibraryTab::Tracks
                    && loaded_tracks_key.peek().as_ref() != Some(&session_key)
                {
                    load_tracks_page.call(false);
                }
            }
            None => {
                if loaded_artists_key.peek().is_some()
                    || *artists_loading.peek()
                    || !artists_list.peek().is_empty()
                {
                    let next_req = artists_req_id.peek().wrapping_add(1);
                    artists_req_id.set(next_req);
                    loaded_artists_key.set(None);
                    artists_list.set(Vec::new());
                    artists_error.set(None);
                    artists_loading.set(false);
                }
                if loaded_tracks_key.peek().is_some()
                    || *tracks_loading.peek()
                    || *tracks_loading_more.peek()
                    || !tracks_list.peek().is_empty()
                {
                    let next_req = tracks_req_id.peek().wrapping_add(1);
                    tracks_req_id.set(next_req);
                    loaded_tracks_key.set(None);
                    tracks_list.set(Vec::new());
                    tracks_offset.set(0);
                    tracks_has_more.set(false);
                    tracks_error.set(None);
                    tracks_loading.set(false);
                    tracks_loading_more.set(false);
                }
            }
            Some(_) => {
                if *artists_loading.peek() {
                    let next_req = artists_req_id.peek().wrapping_add(1);
                    artists_req_id.set(next_req);
                    artists_loading.set(false);
                    loaded_artists_key.set(None);
                }
                if *tracks_loading.peek() || *tracks_loading_more.peek() {
                    let was_initial = *tracks_loading.peek();
                    let next_req = tracks_req_id.peek().wrapping_add(1);
                    tracks_req_id.set(next_req);
                    tracks_loading.set(false);
                    tracks_loading_more.set(false);
                    if was_initial {
                        loaded_tracks_key.set(None);
                    }
                }
            }
        }
    });

    let play_album_now = use_callback(move |(aid, name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: album streaming is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            match fetch_album_tracks(&info, &aid).await {
                Ok(mut tracks) if !tracks.is_empty() => {
                    for t in &mut tracks {
                        studio.apply_curation_overrides(t);
                    }
                    player.play_queue(tracks, 0);
                }
                _ => {
                    studio.show_toast(format!("No playable tracks found in {name}"));
                }
            }
        });
    });

    let play_playlist_now = use_callback(move |(pid, name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: playlist streaming is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            match fetch_playlist_tracks(&info, &pid).await {
                Ok(mut tracks) => {
                    studio.sync_playlist_count(&pid, tracks.len() as u32);
                    if tracks.is_empty() {
                        studio.show_toast(format!("Playlist \"{name}\" has no playable tracks yet"));
                    } else {
                        for t in &mut tracks {
                            studio.apply_curation_overrides(t);
                        }
                        player.play_queue(tracks, 0);
                    }
                }
                Err(_) => {
                    studio.show_toast(format!("Playlist \"{name}\" has no playable tracks yet"));
                }
            }
        });
    });

    let load_albums_page = use_callback(move |(sort, append): (AlbumListSort, bool)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: remote albums cannot be loaded.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        if append && (*albums_loading.peek() || *albums_loading_more.peek()) {
            return;
        }
        let offset = if append { *albums_next_offset.peek() } else { 0 };
        let req_id = albums_req_id.peek().wrapping_add(1);
        albums_req_id.set(req_id);
        if append {
            albums_loading_more.set(true);
        } else {
            albums_loading.set(true);
            albums_loading_more.set(false);
            albums_error.set(None);
            albums_list.set(Vec::new());
            albums_next_offset.set(0);
            albums_has_more.set(false);
        }
        spawn(async move {
            let res = fetch_album_list(&info, sort, ALBUM_LIST_PAGE_SIZE, offset).await;
            if *albums_req_id.peek() != req_id {
                return;
            }
            match res {
                Ok(page) => {
                    let mut next = if append {
                        albums_list.peek().clone()
                    } else {
                        Vec::new()
                    };
                    let (next_offset, has_more) =
                        merge_album_page(&mut next, page, offset, ALBUM_LIST_PAGE_SIZE);
                    albums_list.set(next);
                    albums_next_offset.set(next_offset);
                    albums_has_more.set(has_more);
                    albums_error.set(None);
                }
                Err(e) => {
                    if append {
                        studio.show_toast(format!("Could not load more albums: {e}"));
                    } else {
                        loaded_albums_key.set(None);
                        albums_error.set(Some(e));
                    }
                }
            }
            albums_loading.set(false);
            albums_loading_more.set(false);
        });
    });

    // Load the full Navidrome album catalog as soon as bootstrap resolves or sort changes.
    use_effect(move || {
        let sort = album_sort();
        let offline = player.offline_only();
        let nav = studio
            .bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone());
        match nav {
            Some(info) if !offline => {
                let key = (info.base_url.clone(), info.username.clone(), sort);
                if loaded_albums_key.peek().as_ref() != Some(&key) {
                    loaded_albums_key.set(Some(key));
                    load_albums_page.call((sort, false));
                }
            }
            None => {
                if loaded_albums_key.peek().is_some()
                    || *albums_loading.peek()
                    || *albums_loading_more.peek()
                    || !albums_list.peek().is_empty()
                {
                    let next_req = albums_req_id.peek().wrapping_add(1);
                    albums_req_id.set(next_req);
                    loaded_albums_key.set(None);
                    albums_list.set(Vec::new());
                    albums_next_offset.set(0);
                    albums_has_more.set(false);
                    albums_error.set(None);
                    albums_loading.set(false);
                    albums_loading_more.set(false);
                }
            }
            Some(_) => {
                // Offline Only toggled on: cancel any in-flight album fetch cleanly.
                if *albums_loading.peek() || *albums_loading_more.peek() {
                    let was_initial = *albums_loading.peek();
                    let next_req = albums_req_id.peek().wrapping_add(1);
                    albums_req_id.set(next_req);
                    albums_loading.set(false);
                    albums_loading_more.set(false);
                    if was_initial {
                        loaded_albums_key.set(None);
                    }
                }
            }
        }
    });

    let offline_only = player.offline_only();
    let bootstrap_pending = studio.bootstrap.read().is_none();
    let nav_info = studio.navidrome_info();
    let has_navidrome = nav_info.is_some();
    let current_album_key = nav_info
        .as_ref()
        .map(|info| (info.base_url.clone(), info.username.clone(), album_sort()));
    let albums_load_pending = !offline_only
        && albums_error.read().is_none()
        && (bootstrap_pending
            || current_album_key
                .as_ref()
                .is_some_and(|key| loaded_albums_key.read().as_ref() != Some(key)));
    let playlists: Vec<_> = studio
        .playlists
        .read()
        .iter()
        .filter(|p| !(p.category == PlaylistCategory::Seasonal && p.song_count == 0))
        .cloned()
        .collect();
    let albums = albums_list.read().clone();
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
    let triage = studio.visible_triage_tracks();

    // Honor any legacy Left Rail playlist selection by pushing the dedicated route.
    use_effect(move || {
        if let Some(pid) = (studio.selected_playlist_id)() {
            studio.selected_playlist_id.set(None);
            navigator.push(Route::PlaylistDetailPage { id: pid });
        }
    });

    let artists = artists_list.read().clone();
    let library_tracks: Vec<PlayerTrack> = tracks_list
        .read()
        .iter()
        .cloned()
        .map(|mut t| {
            studio.apply_curation_overrides(&mut t);
            t
        })
        .collect();

    rsx! {
        div { class: "space-y-6",
            // Category Tabs
            div { class: "flex flex-wrap items-center justify-between gap-3",
                div { class: "flex flex-wrap items-center gap-2",
                    for (tab, label, badge) in [
                        (
                            LibraryTab::Playlists,
                            "Playlists",
                            (!playlists.is_empty()).then(|| playlists.len().to_string()),
                        ),
                        (
                            LibraryTab::Albums,
                            "Albums",
                            albums_badge_text(albums.len(), albums_has_more()),
                        ),
                        (
                            LibraryTab::Artists,
                            "Artists",
                            (!artists.is_empty()).then(|| artists.len().to_string()),
                        ),
                        (
                            LibraryTab::Tracks,
                            "Tracks",
                            albums_badge_text(library_tracks.len(), tracks_has_more()),
                        ),
                        (
                            LibraryTab::Starred,
                            "Starred",
                            (!starred.is_empty()).then(|| starred.len().to_string()),
                        ),
                        (
                            LibraryTab::Discovery,
                            "Discovery",
                            (!triage.is_empty()).then(|| triage.len().to_string()),
                        ),
                        (LibraryTab::Stats, "Stats", None),
                    ] {
                        {
                            let active = active_tab() == tab;
                            let cls = if active {
                                "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                            } else {
                                "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary"
                            };
                            rsx! {
                                button {
                                    key: "{label}",
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full border text-[13px] font-medium transition-colors cursor-pointer {cls}",
                                    onclick: move |_| {
                                        active_tab.set(tab);
                                    },
                                    span { "{label}" }
                                    if let Some(count) = badge {
                                        span { class: "tabular-nums-mono text-[11px] opacity-75",
                                            "{count}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if active_tab() == LibraryTab::Playlists {
                    div { class: "flex items-center gap-2",
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 transition-[color,background-color,border-color,filter] cursor-pointer",
                            onclick: move |_| studio.open_create_playlist("", Vec::new()),
                            IconPlus { size: 15 }
                            span { "New Playlist" }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-accent-surface hover:bg-beet-accent/25 border border-beet-accent/40 text-beet-accent-text text-[13px] font-medium transition-colors cursor-pointer",
                            onclick: move |_| import_modal_open.set(true),
                            IconListPlus { size: 15 }
                            span { "Import Playlist" }
                        }
                    }
                }
            }

            {
                match active_tab() {
                    LibraryTab::Playlists => rsx! {
                        if playlists.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No Navidrome playlists loaded yet. Connect Navidrome in Settings or create a new playlist above."
                            }
                        } else {
                            {
                                let mixes_snapshot = studio.contextual_mixes.read().clone();
                                let user_playlists: Vec<&SubsonicPlaylistSummary> = playlists
                                    .iter()
                                    .filter(|pl| !pl.category.is_soulbeet_mix())
                                    .collect();
                                let mut mix_playlists: Vec<&SubsonicPlaylistSummary> = playlists
                                    .iter()
                                    .filter(|pl| pl.category.is_soulbeet_mix())
                                    .collect();
                                mix_playlists.sort_by_key(|pl| pl.category.sort_rank());

                                rsx! {
                                    div { class: "space-y-8",
                                        if !user_playlists.is_empty() {
                                            section { class: "space-y-3",
                                                div { class: "flex items-center justify-between gap-2",
                                                    h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                                        "Your Playlists"
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{user_playlists.len()}"
                                                    }
                                                }
                                                div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3.5",
                                                    for pl in user_playlists.iter() {
                                                        {
                                                            let matched_slug = mixes_snapshot
                                                                .iter()
                                                                .find(|m| m.navidrome_playlist_id.as_deref() == Some(pl.id.as_str()))
                                                                .map(|m| m.slug.as_str());
                                                            let mix_slug = resolve_playlist_mix_sleeve_slug(
                                                                matched_slug,
                                                                &pl.name,
                                                                Some(&pl.comment),
                                                            );
                                                            rsx! {
                                                                PlaylistGridCard {
                                                                    key: "lib-pl-{pl.id}",
                                                                    playlist: (*pl).clone(),
                                                                    mix_slug,
                                                                    on_open: move |pid: String| {
                                                                        navigator.push(Route::PlaylistDetailPage { id: pid });
                                                                    },
                                                                    on_play: move |pair| play_playlist_now.call(pair),
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if !mix_playlists.is_empty() {
                                            section { class: "space-y-3",
                                                div { class: "flex items-center justify-between gap-2",
                                                    h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                                        "Discovery & Mixes"
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{mix_playlists.len()}"
                                                    }
                                                }
                                                div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3.5",
                                                    for pl in mix_playlists.iter() {
                                                        {
                                                            let matched_slug = mixes_snapshot
                                                                .iter()
                                                                .find(|m| m.navidrome_playlist_id.as_deref() == Some(pl.id.as_str()))
                                                                .map(|m| m.slug.as_str());
                                                            let mix_slug = resolve_playlist_mix_sleeve_slug(
                                                                matched_slug,
                                                                &pl.name,
                                                                Some(&pl.comment),
                                                            );
                                                            rsx! {
                                                                PlaylistGridCard {
                                                                    key: "lib-mix-{pl.id}",
                                                                    playlist: (*pl).clone(),
                                                                    mix_slug,
                                                                    on_open: move |pid: String| {
                                                                        let mix_slug = studio
                                                                            .contextual_mixes
                                                                            .peek()
                                                                            .iter()
                                                                            .find(|m| m.navidrome_playlist_id.as_deref() == Some(pid.as_str()))
                                                                            .map(|m| m.slug.clone());
                                                                        if let Some(slug) = mix_slug {
                                                                            navigator.push(Route::MixDetailPage { slug });
                                                                        } else {
                                                                            navigator.push(Route::PlaylistDetailPage { id: pid });
                                                                        }
                                                                    },
                                                                    on_play: move |pair| play_playlist_now.call(pair),
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
                    },
                    LibraryTab::Albums => rsx! {
                        div { class: "space-y-4",
                            div { class: "flex flex-wrap items-center justify-between gap-2",
                                div { class: "flex flex-wrap items-center gap-1.5",
                                    for sort in AlbumListSort::ALL {
                                        {
                                            let active = album_sort() == sort;
                                            let cls = if active {
                                                "bg-beet-surface-highest border-beet-border-default text-beet-text-primary font-semibold"
                                            } else {
                                                "bg-beet-surface border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary font-medium"
                                            };
                                            rsx! {
                                                button {
                                                    key: "alb-sort-{sort.as_subsonic_type()}",
                                                    r#type: "button",
                                                    disabled: offline_only,
                                                    class: "h-8 px-3 rounded-full border text-[12px] disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer {cls}",
                                                    onclick: move |_| {
                                                        if album_sort() != sort {
                                                            album_sort.set(sort);
                                                        } else if albums_error().is_some() {
                                                            if let Some(info) = studio.navidrome_info() {
                                                                loaded_albums_key.set(Some((
                                                                    info.base_url.clone(),
                                                                    info.username.clone(),
                                                                    sort,
                                                                )));
                                                            }
                                                            load_albums_page.call((sort, false));
                                                        }
                                                    },
                                                    "{sort.label()}"
                                                }
                                            }
                                        }
                                    }
                                }
                                if has_navidrome {
                                    button {
                                        r#type: "button",
                                        disabled: albums_loading() || albums_loading_more() || offline_only,
                                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if let Some(info) = studio.navidrome_info() {
                                                loaded_albums_key.set(Some((
                                                    info.base_url.clone(),
                                                    info.username.clone(),
                                                    album_sort(),
                                                )));
                                            }
                                            load_albums_page.call((album_sort(), false));
                                        },
                                        IconRefreshCw {
                                            size: 12,
                                            class: if albums_loading() { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                                        }
                                        span { "Refresh" }
                                    }
                                }
                            }

                            if (albums_loading() || albums_load_pending) && albums.is_empty() {
                                div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                                    IconLoader { size: 16 }
                                    span { "Loading albums…" }
                                }
                            } else if let Some(err) = albums_error() {
                                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-3",
                                    p { class: "text-[13px] text-beet-text-secondary",
                                        "Could not load albums from Navidrome: {err}"
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: albums_loading() || offline_only,
                                        class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary hover:bg-beet-surface-highest disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if let Some(info) = studio.navidrome_info() {
                                                loaded_albums_key.set(Some((
                                                    info.base_url.clone(),
                                                    info.username.clone(),
                                                    album_sort(),
                                                )));
                                            }
                                            load_albums_page.call((album_sort(), false));
                                        },
                                        IconRefreshCw { size: 13 }
                                        span { "Retry" }
                                    }
                                }
                            } else if albums.is_empty() {
                                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                    "{empty_albums_message(has_navidrome, offline_only, album_sort())}"
                                }
                            } else {
                                div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4",
                                    for alb in albums.iter() {
                                        {
                                            let aid_open = alb.id.clone();
                                            let aid_key = alb.id.clone();
                                            let aid_play = alb.id.clone();
                                            let name_play = alb.name.clone();
                                            rsx! {
                                                div {
                                                    key: "lib-alb-{alb.id}",
                                                    role: "button",
                                                    tabindex: "0",
                                                    class: "group rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default p-3 text-left transition-colors cursor-pointer space-y-2.5",
                                                    onclick: move |_| {
                                                        navigator.push(Route::AlbumDetailPage { id: aid_open.clone() });
                                                    },
                                                    onkeydown: move |evt: KeyboardEvent| {
                                                        if matches!(evt.key(), Key::Enter) || matches!(evt.key(), Key::Character(ref s) if s == " ") {
                                                            evt.prevent_default();
                                                            navigator.push(Route::AlbumDetailPage { id: aid_key.clone() });
                                                        }
                                                    },
                                                    div { class: "aspect-square rounded-[8px] overflow-hidden bg-beet-surface-low flex items-center justify-center relative",
                                                        CoverImage {
                                                            src: alb.cover_art_url.clone(),
                                                            alt: alb.name.clone(),
                                                            kind: ArtFallbackKind::Album,
                                                            icon_size: 28,
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            aria_label: format!("Play {}", alb.name),
                                                            title: format!("Play {}", alb.name),
                                                            class: "absolute bottom-2 right-2 w-9 h-9 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:scale-105 transition-[opacity,transform] shadow-lg cursor-pointer",
                                                            onclick: move |evt: MouseEvent| {
                                                                evt.stop_propagation();
                                                                play_album_now.call((aid_play.clone(), name_play.clone()));
                                                            },
                                                            onkeydown: move |evt: KeyboardEvent| {
                                                                if matches!(evt.key(), Key::Enter) || matches!(evt.key(), Key::Character(ref s) if s == " ") {
                                                                    evt.stop_propagation();
                                                                }
                                                            },
                                                            IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                                        }
                                                    }
                                                    div {
                                                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate", "{alb.name}" }
                                                        p { class: "text-[12px] text-beet-text-secondary truncate", "{alb.artist}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if albums_has_more() {
                                    div { class: "flex justify-center pt-2",
                                        button {
                                            r#type: "button",
                                            disabled: albums_loading_more() || offline_only,
                                            class: "inline-flex items-center gap-1.5 h-9 px-5 rounded-full bg-beet-surface-high hover:bg-beet-surface-highest border border-beet-border-default text-[13px] font-medium text-beet-text-primary disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                            onclick: move |_| load_albums_page.call((album_sort(), true)),
                                            if albums_loading_more() {
                                                IconLoader { size: 14 }
                                                span { "Loading more…" }
                                            } else {
                                                span { "Load more albums" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Artists => rsx! {
                        if artists_loading() && artists.is_empty() {
                            div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                                IconLoader { size: 16 }
                                span { "Loading artists..." }
                            }
                        } else if let Some(err) = artists_error() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-3",
                                p { class: "text-[13px] text-beet-text-secondary", "Could not load artists: {err}" }
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary cursor-pointer",
                                    onclick: move |_| load_artists.call(()),
                                    IconRefreshCw { size: 13 }
                                    span { "Retry" }
                                }
                            }
                        } else if artists.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No artists found in your Navidrome library."
                            }
                        } else {
                            div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-3.5",
                                for art in artists.iter() {
                                    {
                                        let aid = art.id.clone();
                                        rsx! {
                                            button {
                                                key: "lib-art-{art.id}",
                                                r#type: "button",
                                                class: "group flex items-center gap-3 p-3 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer min-w-0",
                                                onclick: move |_| {
                                                    navigator.push(Route::ArtistDetailPage { id: aid.clone() });
                                                },
                                                div { class: "w-12 h-12 rounded-full overflow-hidden bg-beet-surface-low border border-beet-border-subtle shrink-0 flex items-center justify-center",
                                                    CoverImage {
                                                        src: art.cover_art_url.clone(),
                                                        alt: art.name.clone(),
                                                        kind: ArtFallbackKind::Artist,
                                                        icon_size: 20,
                                                        rounded_full: true,
                                                    }
                                                }
                                                div { class: "min-w-0 flex-1",
                                                    p { class: "text-[14px] font-semibold text-beet-text-primary group-hover:text-beet-leaf-text truncate transition-colors",
                                                        "{art.name}"
                                                    }
                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary truncate",
                                                        if art.album_count == 1 { "1 album" } else { "{art.album_count} albums" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Tracks => rsx! {
                        if tracks_loading() && library_tracks.is_empty() {
                            div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                                IconLoader { size: 16 }
                                span { "Loading tracks..." }
                            }
                        } else if let Some(err) = tracks_error() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-3",
                                p { class: "text-[13px] text-beet-text-secondary", "Could not load tracks: {err}" }
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary cursor-pointer",
                                    onclick: move |_| load_tracks_page.call(false),
                                    IconRefreshCw { size: 13 }
                                    span { "Retry" }
                                }
                            }
                        } else if library_tracks.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No tracks found in your Navidrome library."
                            }
                        } else {
                            div { class: "space-y-4",
                                div { class: "space-y-1", role: "list", aria_label: "Library tracks",
                                    for (idx, track) in library_tracks.iter().enumerate() {
                                        {
                                            let all = library_tracks.clone();
                                            let t_next = track.clone();
                                            let t_queue = track.clone();
                                            let t_pl = track.clone();
                                            let t_star = track.clone();
                                            let t_rate = track.clone();
                                            let t_share = track.clone();
                                            let t_inspect = track.clone();
                                            let state = if offline_only && !track.is_offline_cached {
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
                                                    key: "lib-trk-{track.id}",
                                                    title: track.title.clone(),
                                                    artist: track.artist.clone(),
                                                    album: track.album.clone(),
                                                    cover_art_url: track.cover_art_url.clone(),
                                                    duration_secs: track.duration_secs,
                                                    codec_badge: track.codec_badge.clone(),
                                                    is_playing: player.is_track_playing(&track.id),
                                                    state,
                                                    on_play: move |_| player.play_queue(all.clone(), idx),
                                                    on_play_next: move |_| {
                                                        player.enqueue_next(t_next.clone());
                                                        studio.show_toast(format!("Playing \"{}\" next", t_next.title));
                                                    },
                                                    on_add_to_queue: move |_| {
                                                        player.enqueue_last(t_queue.clone());
                                                        studio.show_toast(format!("Added \"{}\" to queue", t_queue.title));
                                                    },
                                                    on_add_to_playlist: move |_| {
                                                        studio.open_add_to_playlist(t_pl.title.clone(), vec![t_pl.id.clone()]);
                                                    },
                                                    on_artist_click: move |artist| studio.navigate_to_artist(navigator, artist),
                                                    on_album_click: move |album| studio.navigate_to_album(navigator, album),
                                                    on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                    on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                    on_share: move |_| studio.open_share_track(&t_share),
                                                    on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                                }
                                            }
                                        }
                                    }
                                }
                                if tracks_has_more() {
                                    div { class: "flex justify-center pt-2",
                                        button {
                                            r#type: "button",
                                            disabled: tracks_loading_more() || offline_only,
                                            class: "inline-flex items-center gap-1.5 h-9 px-5 rounded-full bg-beet-surface-high hover:bg-beet-surface-highest border border-beet-border-default text-[13px] font-medium text-beet-text-primary disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                            onclick: move |_| load_tracks_page.call(true),
                                            if tracks_loading_more() {
                                                IconLoader { size: 14 }
                                                span { "Loading more..." }
                                            } else {
                                                span { "Load more tracks" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Starred => rsx! {
                        if starred.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No starred tracks yet. Press S on any playing track to star it."
                            }
                        } else {
                            div { class: "space-y-1", role: "list", aria_label: "Starred tracks",
                                for (idx, track) in starred.iter().enumerate() {
                                    {
                                        let all = starred.clone();
                                        let t_next = track.clone();
                                        let t_queue = track.clone();
                                        let t_pl = track.clone();
                                        let t_star = track.clone();
                                        let t_rate = track.clone();
                                        let t_share = track.clone();
                                        let t_inspect = track.clone();
                                        let state = if offline_only && !track.is_offline_cached {
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
                                                key: "lib-star-{track.id}",
                                                title: track.title.clone(),
                                                artist: track.artist.clone(),
                                                album: track.album.clone(),
                                                cover_art_url: track.cover_art_url.clone(),
                                                duration_secs: track.duration_secs,
                                                codec_badge: track.codec_badge.clone(),
                                                is_playing: player.is_track_playing(&track.id),
                                                state,
                                                on_play: move |_| player.play_queue(all.clone(), idx),
                                                on_play_next: move |_| {
                                                    player.enqueue_next(t_next.clone());
                                                    studio.show_toast(format!("Playing \"{}\" next", t_next.title));
                                                },
                                                on_add_to_queue: move |_| {
                                                    player.enqueue_last(t_queue.clone());
                                                    studio.show_toast(format!("Added \"{}\" to queue", t_queue.title));
                                                },
                                                on_add_to_playlist: move |_| {
                                                    studio.open_add_to_playlist(t_pl.title.clone(), vec![t_pl.id.clone()]);
                                                },
                                                on_artist_click: move |artist| studio.navigate_to_artist(navigator, artist),
                                                on_album_click: move |album| studio.navigate_to_album(navigator, album),
                                                on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                on_share: move |_| studio.open_share_track(&t_share),
                                                on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Discovery => rsx! {
                        if triage.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No pending discovery tracks staged right now."
                            }
                        } else {
                            div { class: "space-y-1", role: "list",
                                for dt in triage.iter() {
                                    {
                                        let track = studio.track_from_discovery(dt);
                                        let triage_for_row = triage.clone();
                                        let t_play = track.clone();
                                        let t_next = track.clone();
                                        let t_queue = track.clone();
                                        let t_pl = track.clone();
                                        let t_keep = track.clone();
                                        let t_drop = track.clone();
                                        let t_rate = track.clone();
                                        let t_inspect = track.clone();
                                        rsx! {
                                            TrackRow {
                                                key: "lib-disc-{dt.id}",
                                                title: track.title.clone(),
                                                artist: track.artist.clone(),
                                                album: track.album.clone(),
                                                cover_art_url: track.cover_art_url.clone(),
                                                is_playing: !track.id.is_empty() && player.is_track_playing(&track.id),
                                                state: TrackRowState::PendingDiscovery {
                                                    days_remaining: track.days_remaining,
                                                    reason_label: track.reason_label.clone(),
                                                    rating: track.rating,
                                                },
                                                on_play: move |_| {
                                                    studio.play_or_toggle_triage(
                                                        player,
                                                        &triage_for_row,
                                                        &t_play,
                                                        false,
                                                    )
                                                },
                                                on_play_next: move |_| {
                                                    player.enqueue_next(t_next.clone());
                                                    studio.show_toast(format!("Playing \"{}\" next", t_next.title));
                                                },
                                                on_add_to_queue: move |_| {
                                                    player.enqueue_last(t_queue.clone());
                                                    studio.show_toast(format!("Added \"{}\" to queue", t_queue.title));
                                                },
                                                on_add_to_playlist: move |_| {
                                                    studio.open_add_to_playlist(t_pl.title.clone(), vec![t_pl.id.clone()]);
                                                },
                                                on_artist_click: move |artist| studio.navigate_to_artist(navigator, artist),
                                                on_album_click: move |album| studio.navigate_to_album(navigator, album),
                                                on_keep: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                                on_drop: move |_| studio.drop_track(auth, player, t_drop.clone()),
                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Stats => rsx! {
                        LibraryStatsPanel {}
                    },
                }
            }

            if import_modal_open() {
                PlaylistImportModal {
                    on_close: move |_| import_modal_open.set(false),
                    on_imported: move |resp: shared::playlist_import::CommitPlaylistImportResponse| {
                        studio.show_toast(format!(
                            "Imported \"{}\": {} in library, {} queued.",
                            resp.playlist_name, resp.already_in_library, resp.queued_for_download
                        ));
                        studio.refresh_playlists();
                    },
                    on_view_activity: Some(on_view_activity),
                }
            }
        }
    }
}

#[component]
fn PlaylistGridCard(
    playlist: SubsonicPlaylistSummary,
    #[props(default)] mix_slug: Option<String>,
    on_open: EventHandler<String>,
    on_play: EventHandler<(String, String)>,
) -> Element {
    let pid_open = playlist.id.clone();
    let pid_key = playlist.id.clone();
    let pid_play = playlist.id.clone();
    let name_play = playlist.name.clone();
    let category = playlist.category;
    let track_word = if playlist.song_count == 1 { "TRACK" } else { "TRACKS" };
    let cover_art_url = playlist.cover_art_url(300);

    let tag_cls = match category {
        PlaylistCategory::Seasonal | PlaylistCategory::Discovery => {
            "bg-beet-accent-surface border-beet-accent/30 text-beet-accent-text"
        }
        PlaylistCategory::MomentMix => {
            "bg-beet-leaf-surface border-beet-leaf-solid/30 text-beet-leaf-text"
        }
        PlaylistCategory::Smart | PlaylistCategory::User => {
            "bg-beet-surface-high border-beet-border-subtle text-beet-text-tertiary"
        }
    };

    rsx! {
        div {
            role: "button",
            tabindex: "0",
            class: "group p-4 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer flex items-center justify-between gap-3",
            onclick: move |_| on_open.call(pid_open.clone()),
            onkeydown: move |evt: KeyboardEvent| {
                if matches!(evt.key(), Key::Enter) || matches!(evt.key(), Key::Character(ref s) if s == " ") {
                    evt.prevent_default();
                    on_open.call(pid_key.clone());
                }
            },
            div { class: "flex items-center gap-3 min-w-0",
                div { class: "w-11 h-11 rounded-[8px] overflow-hidden border border-beet-border-subtle flex items-center justify-center shrink-0",
                    PlaylistArtwork {
                        mix_slug,
                        cover_art_url,
                        alt: playlist.name.clone(),
                        svg_id_prefix: format!("lib-pl-{}", playlist.id),
                        icon_size: 19,
                    }
                }
                div { class: "min-w-0 space-y-1",
                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate", "{playlist.name}" }
                    div { class: "flex items-center gap-1.5 flex-wrap",
                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                            "{playlist.song_count} {track_word}"
                        }
                        if let Some(tag) = category.badge_label() {
                            span { class: "px-1.5 py-0.5 rounded border tabular-nums-mono text-[9px] font-semibold tracking-wider {tag_cls}",
                                "{tag}"
                            }
                        }
                    }
                }
            }
            button {
                r#type: "button",
                aria_label: format!("Play {}", playlist.name),
                title: format!("Play {}", playlist.name),
                class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:scale-105 transition-[opacity,transform] shrink-0 cursor-pointer",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    on_play.call((pid_play.clone(), name_play.clone()));
                },
                onkeydown: move |evt: KeyboardEvent| {
                    if matches!(evt.key(), Key::Enter) || matches!(evt.key(), Key::Character(ref s) if s == " ") {
                        evt.stop_propagation();
                    }
                },
                IconPlay { size: 13, filled: true, class: "ml-0.5".to_string() }
            }
        }
    }
}

#[component]
fn LibraryStatsPanel() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();

    let mut refreshing = use_signal(|| false);
    let mut stats_state: Signal<Option<Result<LibraryStats, String>>> = use_signal(|| None);

    let fetch_stats = use_callback(move |()| {
        refreshing.set(true);
        spawn(async move {
            let res = auth
                .call(api::get_library_stats())
                .await
                .map_err(|e| friendly_error(&e));
            stats_state.set(Some(res));
            refreshing.set(false);
        });
    });

    use_effect(move || {
        if stats_state.peek().is_none() {
            fetch_stats.call(());
        }
    });

    let on_play_track = use_callback(move |clicked: TopPlayedTrack| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: remote playback is unavailable.");
            return;
        }
        let nav = studio.navidrome_info();
        let top_tracks: Vec<TopPlayedTrack> = stats_state
            .peek()
            .as_ref()
            .and_then(|res| res.as_ref().ok())
            .map(|s| s.top_played_tracks.clone())
            .unwrap_or_else(|| vec![clicked.clone()]);

        let start_idx = top_tracks
            .iter()
            .position(|t| t.song_id == clicked.song_id)
            .unwrap_or(0);

        let queue: Vec<PlayerTrack> = top_tracks
            .into_iter()
            .map(|t| {
                let cover = nav
                    .as_ref()
                    .map(|_| web_cover_art_url(&t.song_id, 256));
                let mut track = PlayerTrack {
                    id: t.song_id.clone(),
                    title: t.title,
                    artist: t.artist,
                    album: (!t.album.is_empty()).then_some(t.album),
                    cover_art_url: cover,
                    cover_art_id: Some(t.song_id),
                    duration_secs: t.duration,
                    rating: t.rating.unwrap_or(0).min(5),
                    ..Default::default()
                };
                studio.apply_curation_overrides(&mut track);
                track
            })
            .collect();

        if !queue.is_empty() {
            player.play_queue(queue, start_idx);
        }
    });

    let culture = studio.culture_profile.read().clone();
    let pending_triage = studio.visible_triage_tracks().len();
    let binding = stats_state.read();

    match &*binding {
        Some(Ok(stats)) => rsx! {
            StatsOverview {
                stats: stats.clone(),
                culture: Some(culture),
                pending_triage: Some(pending_triage),
                on_play_track,
                on_refresh: move |_| fetch_stats.call(()),
                refreshing: refreshing(),
            }
        },
        Some(Err(err)) => rsx! {
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-3",
                p { class: "text-[13px] text-beet-text-secondary",
                    "Could not load library stats: {err}"
                }
                button {
                    r#type: "button",
                    disabled: refreshing(),
                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary hover:bg-beet-surface-highest disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                    onclick: move |_| fetch_stats.call(()),
                    IconRefreshCw {
                        size: 13,
                        class: if refreshing() { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                    }
                    span { if refreshing() { "Retrying…" } else { "Retry" } }
                }
            }
        },
        None => rsx! {
            div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                IconLoader { size: 16 }
                span { "Loading library stats…" }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, name: &str) -> SubsonicAlbumSummary {
        SubsonicAlbumSummary {
            id: id.to_string(),
            name: name.to_string(),
            artist: "Artist".to_string(),
            song_count: 10,
            year: Some(2024),
            cover_art_url: None,
        }
    }

    #[test]
    fn merge_album_page_deduplicates_and_tracks_pagination() {
        let mut albums = Vec::new();
        let page1 = vec![album("a1", "Alpha"), album("a2", "Beta")];
        let (next, has_more) = merge_album_page(&mut albums, page1, 0, 2);
        assert_eq!(next, 2);
        assert!(has_more);
        assert_eq!(albums.len(), 2);

        // Overlapping duplicate from offset shift still advances offset by raw page size
        let page2 = vec![album("a2", "Beta"), album("a3", "Gamma")];
        let (next, has_more) = merge_album_page(&mut albums, page2, next, 2);
        assert_eq!(next, 4);
        assert!(has_more);
        assert_eq!(
            albums.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["a1", "a2", "a3"]
        );

        // Short page terminates pagination
        let page3 = vec![album("a4", "Delta")];
        let (next, has_more) = merge_album_page(&mut albums, page3, next, 2);
        assert_eq!(next, 5);
        assert!(!has_more);
        assert_eq!(albums.len(), 4);
    }

    #[test]
    fn albums_badge_text_formats_counts_and_more_indicator() {
        assert_eq!(albums_badge_text(0, false), None);
        assert_eq!(albums_badge_text(11, false), Some("11".to_string()));
        assert_eq!(albums_badge_text(200, true), Some("200+".to_string()));
    }

    #[test]
    fn empty_albums_message_guides_user_when_recent_is_empty() {
        assert!(empty_albums_message(true, true, AlbumListSort::AlphabeticalByName)
            .contains("Offline Only"));
        assert!(empty_albums_message(false, false, AlbumListSort::AlphabeticalByName)
            .contains("Connect Navidrome"));
        assert!(empty_albums_message(true, false, AlbumListSort::Recent).contains("A-Z"));
        assert!(empty_albums_message(true, false, AlbumListSort::AlphabeticalByName)
            .contains("No albums found"));
    }
}
