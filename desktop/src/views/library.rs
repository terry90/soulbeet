//! Desktop Studio Library & Offline Cache View: cache budget meter,
//! Pin/Unpin offline playlists & albums, Playlist CRUD (create/rename/delete/add/remove),
//! Album & Artist Detail views, local track browser, and Sync Now / Outbox controls.

use client_core::models::PinTarget;
use dioxus::prelude::*;
use ui::{
    classify_local_playlist_category, format_duration, use_player, ConfirmModal, IconChevronLeft,
    IconCloudCheck, IconCompass, IconDownload, IconListMusic, IconListPlus, IconMusic, IconPlay,
    IconPlus, IconRadio, IconRefreshCw, IconShuffle, IconSlidersHorizontal, IconSparkles,
    IconTrash2, IconX, Modal, PlaylistCategory, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    desktop_cover_art_url, load_album_detail, load_artist_detail, load_library_albums,
    load_library_artists, load_library_tracks, load_tracks_for_album, load_tracks_for_playlist,
    use_desktop_studio, DesktopPlaylist,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Playlists,
    Albums,
    Artists,
    Tracks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaylistFormMode {
    Create,
    Edit {
        playlist_id: String,
        initial_name: String,
        initial_comment: String,
    },
}

#[component]
pub fn LibraryView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();

    let mut active_tab = use_signal(move || {
        if studio.selected_album_id.peek().is_some() {
            LibraryTab::Albums
        } else if studio.selected_artist_id.peek().is_some() {
            LibraryTab::Artists
        } else {
            LibraryTab::Playlists
        }
    });

    // Switch to Playlists immediately whenever a playlist is selected (e.g. from Sidebar).
    use_effect(move || {
        if studio.selected_playlist_id.read().is_some() {
            active_tab.set(LibraryTab::Playlists);
        }
    });

    // Switch to Albums immediately whenever an album is opened (e.g. from Home or Artist Detail).
    use_effect(move || {
        if studio.selected_album_id.read().is_some() {
            active_tab.set(LibraryTab::Albums);
        }
    });

    // Switch to Artists immediately whenever an artist is opened.
    use_effect(move || {
        if studio.selected_artist_id.read().is_some() {
            active_tab.set(LibraryTab::Artists);
        }
    });

    let mut playlist_form = use_signal(|| None::<PlaylistFormMode>);
    let mut delete_confirm = use_signal(|| None::<(String, String)>);

    let selected_playlist = studio.selected_playlist_id.read().clone();
    let selected_album = studio.selected_album_id.read().clone();
    let selected_artist = studio.selected_artist_id.read().clone();

    let has_server = studio
        .config
        .read()
        .soulbeet_base_url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty());
    let playlist_mutating = (studio.playlist_mutating)();
    let can_edit_playlists = has_server && !offline_only && !playlist_mutating;

    // All reactive reads happen INSIDE each `use_resource` closure so Dioxus tracks them!
    let albums_res = use_resource(move || {
        let core = studio.core.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match core {
                Some(c) => load_library_albums(&c, offline).await,
                None => Vec::new(),
            }
        }
    });

    let artists_res = use_resource(move || {
        let core = studio.core.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match core {
                Some(c) => load_library_artists(&c, offline).await,
                None => Vec::new(),
            }
        }
    });

    let tracks_res = use_resource(move || {
        let core = studio.core.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match core {
                Some(c) => load_library_tracks(&c, offline).await,
                None => Vec::new(),
            }
        }
    });

    let playlist_tracks_res = use_resource(move || {
        let core = studio.core.read().clone();
        let pid = studio.selected_playlist_id.read().clone();
        let _rev = (studio.library_revision)();
        async move {
            match (core, pid) {
                (Some(c), Some(id)) => load_tracks_for_playlist(&c, &id).await,
                _ => Vec::new(),
            }
        }
    });

    let album_detail_res = use_resource(move || {
        let core = studio.core.read().clone();
        let aid = studio.selected_album_id.read().clone();
        let _rev = (studio.library_revision)();
        async move {
            match (core, aid) {
                (Some(c), Some(id)) => load_album_detail(&c, &id).await,
                _ => None,
            }
        }
    });

    let artist_detail_res = use_resource(move || {
        let core = studio.core.read().clone();
        let artist_id = studio.selected_artist_id.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match (core, artist_id) {
                (Some(c), Some(id)) => load_artist_detail(&c, &id, offline).await,
                _ => None,
            }
        }
    });

    let playlists = studio.playlists.read().clone();
    let stats = studio.cache_stats.read().clone();
    let sync_st = studio.sync_status.read().clone();
    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    let selected_playlist_item = selected_playlist
        .as_deref()
        .and_then(|id| playlists.iter().find(|p| p.playlist.id == id))
        .cloned();

    let (used_mb, max_mb, pct, cached_count, pinned_count) = if let Some(s) = stats {
        let used = s.total_audio_bytes / (1024 * 1024);
        let max = (s.max_audio_bytes / (1024 * 1024)).max(1);
        let p = ((s.total_audio_bytes as f64 / s.max_audio_bytes.max(1) as f64) * 100.0)
            .clamp(0.0, 100.0);
        (used, max, p, s.cached_track_count, s.pinned_track_count)
    } else {
        (0, 8192, 0.0, 0, 0)
    };

    rsx! {
        div { class: "p-6 space-y-6 max-w-6xl mx-auto",
            // Offline Cache Budget & Sync Header Card
            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-4 sm:p-5 space-y-4",
                div { class: "flex flex-wrap items-center justify-between gap-3",
                    div {
                        h2 { class: "text-[18px] font-bold text-beet-text-primary",
                            "Library & Offline Cache"
                        }
                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary mt-0.5",
                            "{cached_count} CACHED ({pinned_count} PINNED) · {used_mb} MB / {max_mb} MB · {sync_st.pending_outbox_count} QUEUED OUTBOX"
                        }
                    }

                    div { class: "flex flex-wrap items-center gap-2",
                        button {
                            r#type: "button",
                            disabled: sync_st.syncing || offline_only,
                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none text-[12px] font-medium cursor-pointer",
                            onclick: move |_| studio.sync_now(false),
                            IconRefreshCw { size: 13 }
                            span { if sync_st.syncing { "Syncing…" } else { "Sync Now" } }
                        }
                    }
                }

                // Cache Budget Meter Bar
                div { class: "h-2 w-full rounded-full bg-beet-sunken overflow-hidden",
                    div {
                        class: "h-full rounded-full bg-beet-leaf-solid transition-[width] duration-300",
                        style: "width: {pct:.1}%;",
                    }
                }
            }

            // Segmented Tabs + Contextual Action
            div { class: "flex flex-wrap items-center justify-between gap-3",
                div { class: "flex items-center gap-1.5 p-1 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle w-fit",
                    for (tab, label) in [
                        (LibraryTab::Playlists, "Playlists"),
                        (LibraryTab::Albums, "Albums"),
                        (LibraryTab::Artists, "Artists"),
                        (LibraryTab::Tracks, "Tracks"),
                    ] {
                        {
                            let active = active_tab() == tab;
                            rsx! {
                                button {
                                    key: "{label}",
                                    r#type: "button",
                                    class: if active {
                                        "h-8 px-3.5 rounded-[7px] bg-beet-surface-high border border-beet-border-default text-beet-text-primary text-[13px] font-semibold cursor-pointer"
                                    } else {
                                        "h-8 px-3.5 rounded-[7px] border border-transparent text-beet-text-secondary hover:text-beet-text-primary text-[13px] font-medium cursor-pointer"
                                    },
                                    onclick: move |_| active_tab.set(tab),
                                    "{label}"
                                }
                            }
                        }
                    }
                }

                if active_tab() == LibraryTab::Playlists {
                    button {
                        r#type: "button",
                        disabled: !can_edit_playlists,
                        title: if !has_server {
                            "Connect a Soulbeet server in Settings to create playlists"
                        } else if offline_only {
                            "Disable Offline Only mode to create playlists"
                        } else {
                            "Create a new playlist"
                        },
                        class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold hover:brightness-105 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none cursor-pointer transition-[filter]",
                        onclick: move |_| playlist_form.set(Some(PlaylistFormMode::Create)),
                        IconPlus { size: 13 }
                        span { "New Playlist" }
                    }
                }
            }

            // Tab Content
            match active_tab() {
                LibraryTab::Playlists => rsx! {
                    div { class: "grid grid-cols-1 lg:grid-cols-3 gap-5",
                        // Left: Grouped Playlists List with 1-tap Offline Pinning
                        div { class: "space-y-4",
                            if playlists.is_empty() {
                                p { class: "text-[13px] text-beet-text-tertiary py-4",
                                    "No playlists mirrored yet. Click Sync Now or create a new playlist above."
                                }
                            } else {
                                {
                                    let classified: Vec<(&DesktopPlaylist, PlaylistCategory)> = playlists
                                        .iter()
                                        .map(|item| {
                                            let cat = classify_local_playlist_category(
                                                &item.playlist.kind,
                                                item.playlist.comment.as_deref(),
                                                item.playlist.is_smart,
                                            );
                                            (item, cat)
                                        })
                                        .collect();
                                    let user_items: Vec<(&DesktopPlaylist, PlaylistCategory)> = classified
                                        .iter()
                                        .copied()
                                        .filter(|(_, cat)| !cat.is_soulbeet_mix())
                                        .collect();
                                    let mut mix_items: Vec<(&DesktopPlaylist, PlaylistCategory)> = classified
                                        .iter()
                                        .copied()
                                        .filter(|(item, cat)| {
                                            cat.is_soulbeet_mix()
                                                && !(*cat == PlaylistCategory::Seasonal
                                                    && item.playlist.song_count <= 0)
                                        })
                                        .collect();
                                    mix_items.sort_by_key(|(_, cat)| cat.sort_rank());

                                    rsx! {
                                        if !user_items.is_empty() {
                                            div { class: "space-y-2",
                                                p { class: "px-1 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Your Playlists"
                                                }
                                                for (item, category) in user_items.iter() {
                                                    DesktopPlaylistRow {
                                                        key: "pl-user-{item.playlist.id}",
                                                        item: (*item).clone(),
                                                        category: *category,
                                                        is_selected: selected_playlist.as_deref() == Some(item.playlist.id.as_str()),
                                                        on_select: move |pid| studio.selected_playlist_id.set(Some(pid)),
                                                        on_toggle_pin: move |(pid, pname, pinned)| {
                                                            if pinned {
                                                                studio.unpin_target_offline(PinTarget::Playlist(pid), pname);
                                                            } else {
                                                                studio.pin_target_offline(PinTarget::Playlist(pid), pname);
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }

                                        if !mix_items.is_empty() {
                                            div { class: "space-y-2",
                                                p { class: "px-1 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                    "Discovery & Mixes"
                                                }
                                                for (item, category) in mix_items.iter() {
                                                    DesktopPlaylistRow {
                                                        key: "pl-mix-{item.playlist.id}",
                                                        item: (*item).clone(),
                                                        category: *category,
                                                        is_selected: selected_playlist.as_deref() == Some(item.playlist.id.as_str()),
                                                        on_select: move |pid| studio.selected_playlist_id.set(Some(pid)),
                                                        on_toggle_pin: move |(pid, pname, pinned)| {
                                                            if pinned {
                                                                studio.unpin_target_offline(PinTarget::Playlist(pid), pname);
                                                            } else {
                                                                studio.pin_target_offline(PinTarget::Playlist(pid), pname);
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Right: Selected Playlist Detail & Tracks
                        div { class: "lg:col-span-2 space-y-3",
                            {
                                let pl_tracks = playlist_tracks_res.read().clone().unwrap_or_default();
                                if let Some(pl_item) = selected_playlist_item.clone() {
                                    let category = classify_local_playlist_category(
                                        &pl_item.playlist.kind,
                                        pl_item.playlist.comment.as_deref(),
                                        pl_item.playlist.is_smart,
                                    );
                                    let is_editable = !pl_item.playlist.is_smart && !category.is_soulbeet_mix();
                                    let pl_id = pl_item.playlist.id.clone();
                                    let pl_name = pl_item.playlist.name.clone();
                                    let pl_comment = pl_item
                                        .playlist
                                        .comment
                                        .clone()
                                        .unwrap_or_default();
                                    let visible_comment = {
                                        let trimmed = pl_comment.trim();
                                        if trimmed.is_empty()
                                            || trimmed.to_ascii_lowercase().starts_with("soulbeet:")
                                        {
                                            None
                                        } else {
                                            Some(trimmed.to_string())
                                        }
                                    };
                                    let play_all = pl_tracks.clone();
                                    let shuffle_all = pl_tracks.clone();
                                    let pl_label_play = pl_name.clone();
                                    let pl_label_shuffle = pl_name.clone();
                                    let edit_id = pl_id.clone();
                                    let edit_name = pl_name.clone();
                                    let edit_comment = pl_comment.clone();
                                    let del_id = pl_id.clone();
                                    let del_name = pl_name.clone();
                                    let total_secs: u32 = pl_tracks
                                        .iter()
                                        .filter_map(|t| t.duration_secs)
                                        .sum();

                                    rsx! {
                                        // Playlist Header Card
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 space-y-3",
                                            div { class: "flex flex-wrap items-start justify-between gap-3",
                                                div { class: "min-w-0 space-y-1",
                                                    div { class: "flex items-center gap-2 flex-wrap",
                                                        h3 { class: "text-[18px] font-bold text-beet-text-primary truncate",
                                                            "{pl_name}"
                                                        }
                                                        if let Some(tag) = category.badge_label() {
                                                            span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[10px] font-semibold tracking-wider text-beet-text-tertiary",
                                                                "{tag}"
                                                            }
                                                        }
                                                    }
                                                    {
                                                        let track_word = if pl_tracks.len() == 1 {
                                                            "TRACK"
                                                        } else {
                                                            "TRACKS"
                                                        };
                                                        rsx! {
                                                            p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                                if total_secs > 0 {
                                                                    "{pl_tracks.len()} {track_word} · {format_duration(total_secs)}"
                                                                } else {
                                                                    "{pl_tracks.len()} {track_word}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if let Some(ref desc) = visible_comment {
                                                        p { class: "text-[12px] text-beet-text-secondary",
                                                            "{desc}"
                                                        }
                                                    }
                                                }

                                                div { class: "flex flex-wrap items-center gap-2 shrink-0",
                                                    if !pl_tracks.is_empty() {
                                                        button {
                                                            r#type: "button",
                                                            class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold hover:brightness-105 cursor-pointer",
                                                            onclick: move |_| studio.play_collection(player, play_all.clone(), 0, &pl_label_play),
                                                            IconPlay { size: 12, filled: true }
                                                            span { "Play" }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid/40 text-[12px] font-medium cursor-pointer",
                                                            onclick: move |_| studio.shuffle_collection(player, shuffle_all.clone(), &pl_label_shuffle),
                                                            IconShuffle { size: 12 }
                                                            span { "Shuffle" }
                                                        }
                                                    }
                                                    if is_editable {
                                                        button {
                                                            r#type: "button",
                                                            disabled: !can_edit_playlists,
                                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none text-[12px] font-medium cursor-pointer",
                                                            onclick: move |_| {
                                                                playlist_form.set(Some(PlaylistFormMode::Edit {
                                                                    playlist_id: edit_id.clone(),
                                                                    initial_name: edit_name.clone(),
                                                                    initial_comment: edit_comment.clone(),
                                                                }));
                                                            },
                                                            IconSlidersHorizontal { size: 12 }
                                                            span { "Edit" }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            disabled: !can_edit_playlists,
                                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none text-[12px] font-medium cursor-pointer transition-colors",
                                                            onclick: move |_| {
                                                                delete_confirm.set(Some((del_id.clone(), del_name.clone())));
                                                            },
                                                            IconTrash2 { size: 12 }
                                                            span { "Delete" }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if pl_tracks.is_empty() {
                                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                                "No tracks in this playlist yet. Use the Add to Playlist button on any track to populate it."
                                            }
                                        } else {
                                            div { class: "space-y-1", role: "list",
                                                for (idx, track) in pl_tracks.iter().enumerate() {
                                                    {
                                                        let queue = pl_tracks.clone();
                                                        let t_star = track.clone();
                                                        let t_rate = track.clone();
                                                        let t_inspect = track.clone();
                                                        let t_add_pl = track.clone();
                                                        let remove_pid = pl_id.clone();
                                                        let remove_tid = track.id.clone();
                                                        let remove_title = track.title.clone();
                                                        let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
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
                                                            div {
                                                                key: "plt-{idx}-{track.id}",
                                                                class: "flex items-center gap-1",
                                                                div { class: "flex-1 min-w-0",
                                                                    TrackRow {
                                                                        title: track.title.clone(),
                                                                        artist: track.artist.clone(),
                                                                        album: track.album.clone(),
                                                                        cover_art_url: track.cover_art_url.clone(),
                                                                        duration_secs: track.duration_secs,
                                                                        codec_badge: track.codec_badge.clone(),
                                                                        is_playing: row_playing,
                                                                        state: row_state,
                                                                        on_play: move |_| player.play_queue(queue.clone(), idx),
                                                                        on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                                                        on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                                                        on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                                                    }
                                                                }
                                                                button {
                                                                    r#type: "button",
                                                                    title: "Add to playlist",
                                                                    aria_label: format!("Add {} to playlist", track.title),
                                                                    class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-surface-high shrink-0 cursor-pointer transition-colors",
                                                                    onclick: move |_| studio.open_add_to_playlist(t_add_pl.clone()),
                                                                    IconListPlus { size: 14 }
                                                                }
                                                                if is_editable {
                                                                    button {
                                                                        r#type: "button",
                                                                        disabled: !can_edit_playlists,
                                                                        title: "Remove from playlist",
                                                                        aria_label: format!("Remove {} from playlist", track.title),
                                                                        class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-crimson-text hover:bg-beet-crimson-surface disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none shrink-0 cursor-pointer transition-colors",
                                                                        onclick: move |_| {
                                                                            studio.remove_track_from_playlist(
                                                                                remove_pid.clone(),
                                                                                remove_tid.clone(),
                                                                                remove_title.clone(),
                                                                            );
                                                                        },
                                                                        IconX { size: 13 }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    rsx! {
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                            "Select a playlist to inspect its tracks, edit it, or pin it for offline listening."
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                LibraryTab::Albums => rsx! {
                    if selected_album.is_some() {
                        {
                            let detail = album_detail_res.read().clone().flatten();
                            if let Some((album, is_pinned, album_tracks)) = detail {
                                let art = desktop_cover_art_url(album.cover_art_id.as_deref());
                                let aid_pin = album.id.clone();
                                let aname_pin = album.name.clone();
                                let aname_play = album.name.clone();
                                let aname_shuffle = album.name.clone();
                                let play_tracks = album_tracks.clone();
                                let shuffle_tracks = album_tracks.clone();
                                let total_secs: u32 = album_tracks
                                    .iter()
                                    .filter_map(|t| t.duration_secs)
                                    .sum();
                                let year_label = album.year.filter(|y| *y > 0).map(|y| y.to_string());
                                let artist_id_opt = album
                                    .artist_id
                                    .clone()
                                    .filter(|s| !s.trim().is_empty());

                                rsx! {
                                    div { class: "space-y-5",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer transition-colors",
                                            onclick: move |_| studio.selected_album_id.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to Albums" }
                                        }

                                        // Album Header Card
                                        div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 flex flex-col sm:flex-row items-start sm:items-center gap-5",
                                            div { class: "w-32 h-32 rounded-[10px] overflow-hidden bg-beet-surface-high border border-beet-border-subtle shrink-0 flex items-center justify-center",
                                                if let Some(u) = art {
                                                    img { src: "{u}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                                } else {
                                                    IconMusic { size: 36, class: "text-beet-text-disabled".to_string() }
                                                }
                                            }
                                            div { class: "flex-1 min-w-0 space-y-2.5",
                                                div { class: "space-y-1",
                                                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                        "ALBUM"
                                                    }
                                                    h3 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight truncate",
                                                        "{album.name}"
                                                    }
                                                    div { class: "flex items-center gap-2 flex-wrap text-[13px] text-beet-text-secondary",
                                                        if let Some(arid) = artist_id_opt {
                                                            button {
                                                                r#type: "button",
                                                                class: "font-medium text-beet-text-primary hover:text-beet-leaf-text hover:underline cursor-pointer",
                                                                onclick: move |_| studio.open_artist_detail(arid.clone()),
                                                                "{album.artist_name}"
                                                            }
                                                        } else {
                                                            span { class: "font-medium text-beet-text-primary", "{album.artist_name}" }
                                                        }
                                                        if let Some(yr) = year_label {
                                                            span { "· {yr}" }
                                                        }
                                                        span { "· {album_tracks.len()} tracks" }
                                                        if total_secs > 0 {
                                                            span { "· {format_duration(total_secs)}" }
                                                        }
                                                    }
                                                }

                                                div { class: "flex flex-wrap items-center gap-2",
                                                    button {
                                                        r#type: "button",
                                                        disabled: album_tracks.is_empty(),
                                                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none cursor-pointer",
                                                        onclick: move |_| studio.play_collection(player, play_tracks.clone(), 0, &aname_play),
                                                        IconPlay { size: 13, filled: true }
                                                        span { "Play Album" }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        disabled: album_tracks.is_empty(),
                                                        class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid/40 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none text-[12px] font-medium cursor-pointer",
                                                        onclick: move |_| studio.shuffle_collection(player, shuffle_tracks.clone(), &aname_shuffle),
                                                        IconShuffle { size: 13 }
                                                        span { "Shuffle" }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        class: if is_pinned {
                                                            "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/35 text-beet-leaf-text text-[12px] font-semibold cursor-pointer"
                                                        } else {
                                                            "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-leaf-text text-[12px] font-medium cursor-pointer"
                                                        },
                                                        onclick: move |_| {
                                                            if is_pinned {
                                                                studio.unpin_target_offline(PinTarget::Album(aid_pin.clone()), aname_pin.clone());
                                                            } else {
                                                                studio.pin_target_offline(PinTarget::Album(aid_pin.clone()), aname_pin.clone());
                                                            }
                                                        },
                                                        if is_pinned {
                                                            IconCloudCheck { size: 13 }
                                                            span { "Pinned Offline" }
                                                        } else {
                                                            IconDownload { size: 13 }
                                                            span { "Pin Offline" }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Album Tracklist
                                        if album_tracks.is_empty() {
                                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                                "No tracks mirrored for this album yet."
                                            }
                                        } else {
                                            div { class: "space-y-1", role: "list",
                                                for (idx, track) in album_tracks.iter().enumerate() {
                                                    {
                                                        let queue = album_tracks.clone();
                                                        let t_star = track.clone();
                                                        let t_rate = track.clone();
                                                        let t_inspect = track.clone();
                                                        let t_pl = track.clone();
                                                        let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
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
                                                            div {
                                                                key: "albt-{idx}-{track.id}",
                                                                class: "flex items-center gap-1",
                                                                div { class: "flex-1 min-w-0",
                                                                    TrackRow {
                                                                        title: track.title.clone(),
                                                                        artist: track.artist.clone(),
                                                                        album: track.album.clone(),
                                                                        cover_art_url: track.cover_art_url.clone(),
                                                                        duration_secs: track.duration_secs,
                                                                        codec_badge: track.codec_badge.clone(),
                                                                        track_number: Some((idx + 1) as u32),
                                                                        is_playing: row_playing,
                                                                        state: row_state,
                                                                        on_play: move |_| player.play_queue(queue.clone(), idx),
                                                                        on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                                                        on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                                                        on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                                                    }
                                                                }
                                                                button {
                                                                    r#type: "button",
                                                                    title: "Add to playlist",
                                                                    aria_label: format!("Add {} to playlist", track.title),
                                                                    class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-surface-high shrink-0 cursor-pointer transition-colors",
                                                                    onclick: move |_| studio.open_add_to_playlist(t_pl.clone()),
                                                                    IconListPlus { size: 14 }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                rsx! {
                                    div { class: "space-y-4",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer",
                                            onclick: move |_| studio.selected_album_id.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to Albums" }
                                        }
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                            "Album not found in local mirror."
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        {
                            let albums = albums_res.read().clone().unwrap_or_default();
                            rsx! {
                                div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-5 gap-3",
                                    for (album, is_pinned) in albums.iter() {
                                        {
                                            let aid_open = album.id.clone();
                                            let aid_play = album.id.clone();
                                            let aid_pin = album.id.clone();
                                            let aname = album.name.clone();
                                            let aname_play = album.name.clone();
                                            let pinned = *is_pinned;
                                            let art = desktop_cover_art_url(album.cover_art_id.as_deref());
                                            rsx! {
                                                div {
                                                    key: "la-{album.id}",
                                                    class: "group rounded-[12px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 flex flex-col gap-2 cursor-pointer transition-colors",
                                                    onclick: move |_| studio.selected_album_id.set(Some(aid_open.clone())),
                                                    div { class: "relative aspect-square w-full rounded-[8px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                                        if let Some(u) = art {
                                                            img { src: "{u}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                                        } else {
                                                            IconMusic { size: 26, class: "text-beet-text-disabled".to_string() }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            title: format!("Play {}", album.name),
                                                            aria_label: format!("Play {}", album.name),
                                                            class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 focus-visible:opacity-100 transition-opacity shadow-lg cursor-pointer hover:brightness-105",
                                                            onclick: move |evt| {
                                                                evt.stop_propagation();
                                                                let id = aid_play.clone();
                                                                let label = aname_play.clone();
                                                                if let Some(core) = studio.core.peek().clone() {
                                                                    spawn(async move {
                                                                        let tracks = load_tracks_for_album(&core, &id).await;
                                                                        studio.play_collection(player, tracks, 0, &label);
                                                                    });
                                                                }
                                                            },
                                                            IconPlay { size: 13, filled: true, class: "ml-0.5".to_string() }
                                                        }
                                                    }
                                                    div { class: "flex items-start justify-between gap-1",
                                                        div { class: "min-w-0",
                                                            p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{album.name}" }
                                                            p { class: "text-[12px] text-beet-text-secondary truncate", "{album.artist_name}" }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            title: if pinned { "Unpin album" } else { "Pin album offline" },
                                                            class: if pinned {
                                                                "w-7 h-7 rounded-md inline-flex items-center justify-center bg-beet-leaf-surface text-beet-leaf-text cursor-pointer shrink-0"
                                                            } else {
                                                                "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-surface-high cursor-pointer shrink-0"
                                                            },
                                                            onclick: move |evt| {
                                                                evt.stop_propagation();
                                                                if pinned {
                                                                    studio.unpin_target_offline(PinTarget::Album(aid_pin.clone()), aname.clone());
                                                                } else {
                                                                    studio.pin_target_offline(PinTarget::Album(aid_pin.clone()), aname.clone());
                                                                }
                                                            },
                                                            if pinned {
                                                                IconCloudCheck { size: 13 }
                                                            } else {
                                                                IconDownload { size: 13 }
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
                LibraryTab::Artists => rsx! {
                    if selected_artist.is_some() {
                        {
                            let detail = artist_detail_res.read().clone().flatten();
                            if let Some((artist, artist_albums, artist_tracks)) = detail {
                                let art = desktop_cover_art_url(artist.cover_art_id.as_deref());
                                let play_tracks = artist_tracks.clone();
                                let shuffle_tracks = artist_tracks.clone();
                                let play_name = artist.name.clone();
                                let shuffle_name = artist.name.clone();
                                rsx! {
                                    div { class: "space-y-5",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer transition-colors",
                                            onclick: move |_| studio.selected_artist_id.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to Artists" }
                                        }

                                        div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 flex flex-col sm:flex-row items-start sm:items-center gap-5",
                                            div { class: "w-24 h-24 rounded-full overflow-hidden bg-beet-surface-high border border-beet-border-subtle shrink-0 flex items-center justify-center",
                                                if let Some(u) = art {
                                                    img { src: "{u}", alt: "{artist.name}", class: "w-full h-full object-cover" }
                                                } else {
                                                    IconMusic { size: 30, class: "text-beet-text-disabled".to_string() }
                                                }
                                            }
                                            div { class: "flex-1 min-w-0 space-y-2.5",
                                                div {
                                                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                        "ARTIST"
                                                    }
                                                    h3 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary truncate",
                                                        "{artist.name}"
                                                    }
                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{artist_albums.len()} ALBUMS · {artist_tracks.len()} TRACKS"
                                                    }
                                                }
                                                div { class: "flex flex-wrap items-center gap-2",
                                                    button {
                                                        r#type: "button",
                                                        disabled: artist_tracks.is_empty(),
                                                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none cursor-pointer",
                                                        onclick: move |_| studio.play_collection(player, play_tracks.clone(), 0, &play_name),
                                                        IconPlay { size: 13, filled: true }
                                                        span { "Play All" }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        disabled: artist_tracks.is_empty(),
                                                        class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid/40 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none text-[12px] font-medium cursor-pointer",
                                                        onclick: move |_| studio.shuffle_collection(player, shuffle_tracks.clone(), &shuffle_name),
                                                        IconShuffle { size: 13 }
                                                        span { "Shuffle" }
                                                    }
                                                }
                                            }
                                        }

                                        if !artist_albums.is_empty() {
                                            section { class: "space-y-3",
                                                h4 { class: "text-[16px] font-semibold text-beet-text-primary",
                                                    "Albums ({artist_albums.len()})"
                                                }
                                                div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-5 gap-3",
                                                    for (album, _) in artist_albums.iter() {
                                                        {
                                                            let aid_open = album.id.clone();
                                                            let aid_play = album.id.clone();
                                                            let aname_play = album.name.clone();
                                                            let alb_art = desktop_cover_art_url(album.cover_art_id.as_deref());
                                                            let yr = album.year.filter(|y| *y > 0);
                                                            rsx! {
                                                                div {
                                                                    key: "art-alb-{album.id}",
                                                                    class: "group rounded-[12px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 flex flex-col gap-2 cursor-pointer transition-colors",
                                                                    onclick: move |_| studio.open_album_detail(aid_open.clone()),
                                                                    div { class: "relative aspect-square w-full rounded-[8px] overflow-hidden bg-beet-surface-high flex items-center justify-center",
                                                                        if let Some(u) = alb_art {
                                                                            img { src: "{u}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                                                        } else {
                                                                            IconMusic { size: 24, class: "text-beet-text-disabled".to_string() }
                                                                        }
                                                                        button {
                                                                            r#type: "button",
                                                                            title: format!("Play {}", album.name),
                                                                            aria_label: format!("Play {}", album.name),
                                                                            class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 focus-visible:opacity-100 transition-opacity shadow-lg cursor-pointer hover:brightness-105",
                                                                            onclick: move |evt| {
                                                                                evt.stop_propagation();
                                                                                let id = aid_play.clone();
                                                                                let label = aname_play.clone();
                                                                                if let Some(core) = studio.core.peek().clone() {
                                                                                    spawn(async move {
                                                                                        let tracks = load_tracks_for_album(&core, &id).await;
                                                                                        studio.play_collection(player, tracks, 0, &label);
                                                                                    });
                                                                                }
                                                                            },
                                                                            IconPlay { size: 13, filled: true, class: "ml-0.5".to_string() }
                                                                        }
                                                                    }
                                                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{album.name}" }
                                                                    if let Some(y) = yr {
                                                                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary", "{y}" }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if !artist_tracks.is_empty() {
                                            section { class: "space-y-2",
                                                h4 { class: "text-[16px] font-semibold text-beet-text-primary",
                                                    "Tracks ({artist_tracks.len()})"
                                                }
                                                div { class: "space-y-1", role: "list",
                                                    for (idx, track) in artist_tracks.iter().enumerate() {
                                                        {
                                                            let queue = artist_tracks.clone();
                                                            let t_star = track.clone();
                                                            let t_rate = track.clone();
                                                            let t_inspect = track.clone();
                                                            let t_pl = track.clone();
                                                            let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
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
                                                                div {
                                                                    key: "art-tr-{idx}-{track.id}",
                                                                    class: "flex items-center gap-1",
                                                                    div { class: "flex-1 min-w-0",
                                                                        TrackRow {
                                                                            title: track.title.clone(),
                                                                            artist: track.artist.clone(),
                                                                            album: track.album.clone(),
                                                                            cover_art_url: track.cover_art_url.clone(),
                                                                            duration_secs: track.duration_secs,
                                                                            codec_badge: track.codec_badge.clone(),
                                                                            is_playing: row_playing,
                                                                            state: row_state,
                                                                            on_play: move |_| player.play_queue(queue.clone(), idx),
                                                                            on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                                                            on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                                                            on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                                                        }
                                                                    }
                                                                    button {
                                                                        r#type: "button",
                                                                        title: "Add to playlist",
                                                                        aria_label: format!("Add {} to playlist", track.title),
                                                                        class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-surface-high shrink-0 cursor-pointer transition-colors",
                                                                        onclick: move |_| studio.open_add_to_playlist(t_pl.clone()),
                                                                        IconListPlus { size: 14 }
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
                            } else {
                                rsx! {
                                    div { class: "space-y-4",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer",
                                            onclick: move |_| studio.selected_artist_id.set(None),
                                            IconChevronLeft { size: 14 }
                                            span { "Back to Artists" }
                                        }
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                            "Artist not found in local mirror."
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        {
                            let artists = artists_res.read().clone().unwrap_or_default();
                            if artists.is_empty() {
                                rsx! {
                                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                        "No artists mirrored yet. Click Sync Now to load your library."
                                    }
                                }
                            } else {
                                rsx! {
                                    div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-5 gap-3",
                                        for artist in artists.iter() {
                                            {
                                                let arid = artist.id.clone();
                                                let art = desktop_cover_art_url(artist.cover_art_id.as_deref());
                                                let album_word = if artist.album_count == 1 { "ALBUM" } else { "ALBUMS" };
                                                rsx! {
                                                    button {
                                                        key: "lar-{artist.id}",
                                                        r#type: "button",
                                                        class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-3 flex flex-col items-center text-center gap-2.5 cursor-pointer transition-colors",
                                                        onclick: move |_| studio.selected_artist_id.set(Some(arid.clone())),
                                                        div { class: "w-20 h-20 rounded-full overflow-hidden bg-beet-surface-high border border-beet-border-subtle flex items-center justify-center",
                                                            if let Some(u) = art {
                                                                img { src: "{u}", alt: "{artist.name}", class: "w-full h-full object-cover" }
                                                            } else {
                                                                IconMusic { size: 24, class: "text-beet-text-disabled".to_string() }
                                                            }
                                                        }
                                                        div { class: "w-full min-w-0",
                                                            p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{artist.name}" }
                                                            p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary mt-0.5",
                                                                "{artist.album_count} {album_word}"
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
                LibraryTab::Tracks => rsx! {
                    {
                        let tracks = tracks_res.read().clone().unwrap_or_default();
                        rsx! {
                            div { class: "space-y-1", role: "list",
                                for (idx, track) in tracks.iter().enumerate() {
                                    {
                                        let queue = tracks.clone();
                                        let t_star = track.clone();
                                        let t_rate = track.clone();
                                        let t_inspect = track.clone();
                                        let t_pl = track.clone();
                                        let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
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
                                            div {
                                                key: "lt-{track.id}",
                                                class: "flex items-center gap-1",
                                                div { class: "flex-1 min-w-0",
                                                    TrackRow {
                                                        title: track.title.clone(),
                                                        artist: track.artist.clone(),
                                                        album: track.album.clone(),
                                                        cover_art_url: track.cover_art_url.clone(),
                                                        duration_secs: track.duration_secs,
                                                        codec_badge: track.codec_badge.clone(),
                                                        is_playing: row_playing,
                                                        state: row_state,
                                                        on_play: move |_| player.play_queue(queue.clone(), idx),
                                                        on_star: move |next| studio.star_track(player, t_star.clone(), next),
                                                        on_rate: move |r| studio.rate_track(player, t_rate.clone(), r),
                                                        on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                                    }
                                                }
                                                button {
                                                    r#type: "button",
                                                    title: "Add to playlist",
                                                    aria_label: format!("Add {} to playlist", track.title),
                                                    class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-surface-high shrink-0 cursor-pointer transition-colors",
                                                    onclick: move |_| studio.open_add_to_playlist(t_pl.clone()),
                                                    IconListPlus { size: 14 }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }

            // Create / Edit Playlist Modal
            if let Some(mode) = playlist_form() {
                PlaylistEditorModal {
                    mode,
                    busy: playlist_mutating,
                    on_close: move |_| playlist_form.set(None),
                    on_save: move |(mode_out, name, comment, public): (PlaylistFormMode, String, String, Option<bool>)| {
                        let comment_opt = if comment.trim().is_empty() {
                            None
                        } else {
                            Some(comment.trim().to_string())
                        };
                        let close_editor = EventHandler::new(move |_| playlist_form.set(None));
                        match mode_out {
                            PlaylistFormMode::Create => {
                                studio.create_playlist(
                                    name,
                                    comment_opt,
                                    public,
                                    None,
                                    true,
                                    Some(close_editor),
                                );
                            }
                            PlaylistFormMode::Edit { playlist_id, .. } => {
                                studio.update_playlist(
                                    playlist_id,
                                    Some(name),
                                    Some(comment.trim().to_string()),
                                    public,
                                    Some(close_editor),
                                );
                            }
                        }
                    },
                }
            }

            // Delete Playlist Confirmation Modal
            if let Some((del_id, del_name)) = delete_confirm() {
                ConfirmModal {
                    message: format!("Delete playlist \"{del_name}\"? Your library tracks will not be removed."),
                    confirm_label: if playlist_mutating {
                        "Deleting...".to_string()
                    } else {
                        "Delete Playlist".to_string()
                    },
                    danger: true,
                    on_confirm: move |_| {
                        let close_confirm = EventHandler::new(move |_| delete_confirm.set(None));
                        studio.delete_playlist(del_id.clone(), del_name.clone(), Some(close_confirm));
                    },
                    on_cancel: move |_| delete_confirm.set(None),
                }
            }
        }
    }
}

#[component]
fn PlaylistEditorModal(
    mode: PlaylistFormMode,
    busy: bool,
    on_close: EventHandler<()>,
    on_save: EventHandler<(PlaylistFormMode, String, String, Option<bool>)>,
) -> Element {
    let (heading, submit_label, init_name, init_comment, is_create_mode) = match &mode {
        PlaylistFormMode::Create => (
            "New Playlist",
            "Create Playlist",
            String::new(),
            String::new(),
            true,
        ),
        PlaylistFormMode::Edit {
            initial_name,
            initial_comment,
            ..
        } => (
            "Edit Playlist",
            "Save Changes",
            initial_name.clone(),
            initial_comment.clone(),
            false,
        ),
    };

    let mut name = use_signal(move || init_name);
    let mut comment = use_signal(move || init_comment);
    let mut is_public = use_signal(|| false);
    let mut edit_public_choice = use_signal(|| None::<bool>);

    let mode_for_submit = mode.clone();
    let can_submit = !busy && !name().trim().is_empty();

    rsx! {
        Modal {
            on_close: move |_| on_close.call(()),
            header: rsx! {
                h2 { class: "text-base font-semibold text-beet-text-primary", "{heading}" }
            },
            div { class: "space-y-4",
                label { class: "block space-y-1.5",
                    span { class: "text-[12px] text-beet-text-secondary", "Playlist Name" }
                    input {
                        r#type: "text",
                        value: "{name}",
                        placeholder: "Late Night Synth",
                        disabled: busy,
                        class: "w-full h-10 px-3 rounded-[8px] bg-beet-surface border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none disabled:opacity-50",
                        oninput: move |evt| name.set(evt.value()),
                    }
                }
                label { class: "block space-y-1.5",
                    span { class: "text-[12px] text-beet-text-secondary", "Description (optional)" }
                    textarea {
                        rows: "3",
                        value: "{comment}",
                        placeholder: "Mood, notes, or curation theme...",
                        disabled: busy,
                        class: "w-full px-3 py-2 rounded-[8px] bg-beet-surface border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none resize-none disabled:opacity-50",
                        oninput: move |evt| comment.set(evt.value()),
                    }
                }
                if is_create_mode {
                    label { class: "inline-flex items-center gap-2.5 text-[13px] text-beet-text-secondary cursor-pointer",
                        input {
                            r#type: "checkbox",
                            checked: is_public(),
                            disabled: busy,
                            class: "accent-beet-leaf-solid cursor-pointer",
                            onchange: move |evt| is_public.set(evt.checked()),
                        }
                        span { "Public playlist (visible to other household listeners)" }
                    }
                } else {
                    div { class: "space-y-1.5",
                        span { class: "block text-[12px] text-beet-text-secondary", "Visibility" }
                        div {
                            role: "group",
                            aria_label: "Playlist visibility",
                            class: "inline-flex items-center gap-1 p-1 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle",
                            for (choice, label) in [
                                (None, "Keep current"),
                                (Some(false), "Private"),
                                (Some(true), "Public"),
                            ] {
                                {
                                    let active = edit_public_choice() == choice;
                                    let cls = if active {
                                        "bg-beet-surface-high border-beet-border-default text-beet-text-primary font-semibold"
                                    } else {
                                        "border-transparent text-beet-text-secondary hover:text-beet-text-primary font-medium"
                                    };
                                    rsx! {
                                        button {
                                            key: "{label}",
                                            r#type: "button",
                                            disabled: busy,
                                            aria_pressed: if active { "true" } else { "false" },
                                            class: "h-7 px-3 rounded-[7px] border text-[12px] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none {cls}",
                                            onclick: move |_| edit_public_choice.set(choice),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "flex justify-end gap-2.5 pt-2",
                    button {
                        r#type: "button",
                        class: "px-4 py-2 text-sm font-medium rounded-[10px] bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary transition-colors cursor-pointer",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        r#type: "button",
                        disabled: !can_submit,
                        class: "px-4 py-2 text-sm font-semibold rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none transition-[filter] cursor-pointer",
                        onclick: move |_| {
                            let trimmed = name().trim().to_string();
                            if !trimmed.is_empty() {
                                let public_val = if is_create_mode {
                                    Some(is_public())
                                } else {
                                    edit_public_choice()
                                };
                                on_save.call((
                                    mode_for_submit.clone(),
                                    trimmed,
                                    comment(),
                                    public_val,
                                ));
                            }
                        },
                        if busy { "Saving..." } else { "{submit_label}" }
                    }
                }
            }
        }
    }
}

#[component]
fn DesktopPlaylistRow(
    item: DesktopPlaylist,
    category: PlaylistCategory,
    is_selected: bool,
    on_select: EventHandler<String>,
    on_toggle_pin: EventHandler<(String, String, bool)>,
) -> Element {
    let pid_select = item.playlist.id.clone();
    let pid_pin = item.playlist.id.clone();
    let pname = item.playlist.name.clone();
    let is_pinned = item.is_pinned;

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
            class: if is_selected {
                "flex items-center justify-between gap-2 p-2.5 rounded-[10px] bg-beet-surface-high border border-beet-leaf-solid/40"
            } else {
                "flex items-center justify-between gap-2 p-2.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default"
            },
            button {
                r#type: "button",
                class: "flex items-center gap-2.5 flex-1 min-w-0 text-left cursor-pointer",
                onclick: move |_| on_select.call(pid_select.clone()),
                match category {
                    PlaylistCategory::Seasonal => rsx! {
                        IconSparkles { size: 15, class: "text-beet-accent-text shrink-0".to_string() }
                    },
                    PlaylistCategory::Discovery => rsx! {
                        IconCompass { size: 15, class: "text-beet-accent-text shrink-0".to_string() }
                    },
                    PlaylistCategory::MomentMix => rsx! {
                        IconRadio { size: 15, class: "text-beet-leaf-text shrink-0".to_string() }
                    },
                    PlaylistCategory::Smart | PlaylistCategory::User => rsx! {
                        IconListMusic { size: 15, class: "text-beet-text-tertiary shrink-0".to_string() }
                    },
                }
                div { class: "min-w-0 space-y-0.5",
                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{item.playlist.name}" }
                    div { class: "flex items-center gap-1.5 flex-wrap",
                        span { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary", "{item.playlist.song_count} TRACKS" }
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
                title: if is_pinned { "Unpin offline" } else { "Pin playlist offline" },
                class: if is_pinned {
                    "inline-flex items-center gap-1 h-7 px-2.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/35 text-beet-leaf-text text-[11px] font-semibold cursor-pointer shrink-0"
                } else {
                    "inline-flex items-center gap-1 h-7 px-2.5 rounded-full bg-beet-surface-high border border-beet-border-subtle text-beet-text-secondary hover:text-beet-leaf-text text-[11px] font-medium cursor-pointer shrink-0"
                },
                onclick: move |_| on_toggle_pin.call((pid_pin.clone(), pname.clone(), is_pinned)),
                if is_pinned {
                    IconCloudCheck { size: 12 }
                    span { "Pinned" }
                } else {
                    IconDownload { size: 12 }
                    span { "Pin" }
                }
            }
        }
    }
}
