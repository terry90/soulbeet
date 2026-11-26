//! Desktop Studio Library & Offline Cache View — Offline-Only pill, cache budget meter,
//! Pin/Unpin offline playlists & albums, local track browser, and Sync Now / Outbox controls.

use client_core::models::PinTarget;
use dioxus::prelude::*;
use ui::{
    use_player, IconCloudCheck, IconDownload, IconListMusic, IconMusic, IconPlay, IconRefreshCw,
    OfflineTogglePill, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    desktop_cover_art_url, load_library_albums, load_library_tracks, load_tracks_for_album,
    load_tracks_for_playlist, use_desktop_studio,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Playlists,
    Albums,
    Tracks,
}

#[component]
pub fn LibraryView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();

    let mut active_tab = use_signal(|| LibraryTab::Playlists);
    let selected_playlist = studio.selected_playlist_id.read().clone();

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

    let playlists = studio.playlists.read().clone();
    let stats = studio.cache_stats.read().clone();
    let sync_st = studio.sync_status.read().clone();
    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    let selected_playlist_name = selected_playlist
        .as_deref()
        .and_then(|id| playlists.iter().find(|p| p.playlist.id == id))
        .map(|p| p.playlist.name.clone())
        .unwrap_or_else(|| "Playlist".to_string());

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
                        OfflineTogglePill {
                            offline_only,
                            on_toggle: move |next| player.set_offline_only(next),
                            compact: true,
                        }
                        button {
                            r#type: "button",
                            disabled: sync_st.syncing || offline_only,
                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary disabled:opacity-40 text-[12px] font-medium cursor-pointer",
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

            // Segmented Tabs
            div { class: "flex items-center gap-1.5 p-1 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle w-fit",
                for (tab, label) in [
                    (LibraryTab::Playlists, "Playlists"),
                    (LibraryTab::Albums, "Albums"),
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
                                    "h-8 px-3.5 rounded-[7px] text-beet-text-secondary hover:text-beet-text-primary text-[13px] font-medium cursor-pointer"
                                },
                                onclick: move |_| active_tab.set(tab),
                                "{label}"
                            }
                        }
                    }
                }
            }

            // Tab Content
            match active_tab() {
                LibraryTab::Playlists => rsx! {
                    div { class: "grid grid-cols-1 lg:grid-cols-3 gap-5",
                        // Left: Playlists List with 1-tap Offline Pinning
                        div { class: "space-y-2",
                            if playlists.is_empty() {
                                p { class: "text-[13px] text-beet-text-tertiary py-4",
                                    "No playlists mirrored yet. Click Sync Now to load playlists from Navidrome."
                                }
                            } else {
                                for item in playlists.iter() {
                                    {
                                        let pid = item.playlist.id.clone();
                                        let pid_select = pid.clone();
                                        let pid_pin = pid.clone();
                                        let pname = item.playlist.name.clone();
                                        let is_pinned = item.is_pinned;
                                        let is_selected = selected_playlist.as_deref() == Some(pid.as_str());
                                        rsx! {
                                            div {
                                                key: "pl-{pid}",
                                                class: if is_selected {
                                                    "flex items-center justify-between gap-2 p-2.5 rounded-[10px] bg-beet-surface-high border border-beet-leaf-solid/40"
                                                } else {
                                                    "flex items-center justify-between gap-2 p-2.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default"
                                                },
                                                button {
                                                    r#type: "button",
                                                    class: "flex items-center gap-2.5 flex-1 min-w-0 text-left cursor-pointer",
                                                    onclick: move |_| studio.selected_playlist_id.set(Some(pid_select.clone())),
                                                    IconListMusic { size: 15, class: "text-beet-text-tertiary shrink-0".to_string() }
                                                    div { class: "min-w-0",
                                                        p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{item.playlist.name}" }
                                                        p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary", "{item.playlist.song_count} TRACKS" }
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
                                                    onclick: move |_| {
                                                        if is_pinned {
                                                            studio.unpin_target_offline(PinTarget::Playlist(pid_pin.clone()), pname.clone());
                                                        } else {
                                                            studio.pin_target_offline(PinTarget::Playlist(pid_pin.clone()), pname.clone());
                                                        }
                                                    },
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
                                }
                            }
                        }

                        // Right: Selected Playlist Tracks
                        div { class: "lg:col-span-2 space-y-2",
                            {
                                let pl_tracks = playlist_tracks_res.read().clone().unwrap_or_default();
                                if selected_playlist.is_none() {
                                    rsx! {
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                            "Select a playlist to inspect its tracks or pin it for offline listening."
                                        }
                                    }
                                } else if pl_tracks.is_empty() {
                                    rsx! {
                                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-tertiary",
                                            "No tracks in this playlist."
                                        }
                                    }
                                } else {
                                    let play_all = pl_tracks.clone();
                                    let pl_label = selected_playlist_name.clone();
                                    rsx! {
                                        div { class: "flex items-center justify-between pb-1",
                                            span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                                "{pl_tracks.len()} TRACKS"
                                            }
                                            button {
                                                r#type: "button",
                                                class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold cursor-pointer",
                                                onclick: move |_| studio.play_collection(player, play_all.clone(), 0, &pl_label),
                                                IconPlay { size: 12, filled: true }
                                                span { "Play Playlist" }
                                            }
                                        }
                                        div { class: "space-y-1", role: "list",
                                            for (idx, track) in pl_tracks.iter().enumerate() {
                                                {
                                                    let queue = pl_tracks.clone();
                                                    let t_star = track.clone();
                                                    let t_rate = track.clone();
                                                    let t_inspect = track.clone();
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
                                                        TrackRow {
                                                            key: "plt-{idx}-{track.id}",
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
                    {
                        let albums = albums_res.read().clone().unwrap_or_default();
                        rsx! {
                            div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-5 gap-3",
                                for (album, is_pinned) in albums.iter() {
                                    {
                                        let aid = album.id.clone();
                                        let aid_pin = album.id.clone();
                                        let aname = album.name.clone();
                                        let aname_play = album.name.clone();
                                        let pinned = *is_pinned;
                                        let art = desktop_cover_art_url(album.cover_art_id.as_deref());
                                        rsx! {
                                            div {
                                                key: "la-{album.id}",
                                                class: "group rounded-[12px] bg-beet-surface border border-beet-border-subtle p-2.5 flex flex-col gap-2",
                                                button {
                                                    r#type: "button",
                                                    class: "relative aspect-square w-full rounded-[8px] overflow-hidden bg-beet-surface-high flex items-center justify-center cursor-pointer",
                                                    onclick: move |_| {
                                                        let id = aid.clone();
                                                        let label = aname_play.clone();
                                                        if let Some(core) = studio.core.peek().clone() {
                                                            spawn(async move {
                                                                let tracks = load_tracks_for_album(&core, &id).await;
                                                                studio.play_collection(player, tracks, 0, &label);
                                                            });
                                                        }
                                                    },
                                                    if let Some(u) = art {
                                                        img { src: "{u}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                                    } else {
                                                        IconMusic { size: 26, class: "text-beet-text-disabled".to_string() }
                                                    }
                                                    span { class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity",
                                                        IconPlay { size: 13, filled: true }
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
                                                        onclick: move |_| {
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
                                            TrackRow {
                                                key: "lt-{track.id}",
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
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}
