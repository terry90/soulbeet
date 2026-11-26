//! Desktop Studio Home View — powered directly by local SQLite `SoulbeetCore` (0ms offline).

use client_core::models::PinTarget;
use dioxus::prelude::*;
use ui::{
    use_player, IconCloudCheck, IconCompass, IconDownload, IconMusic, IconPlay, IconRefreshCw,
    IconSparkles, StudioNavSection, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    desktop_cover_art_url, load_home_snapshot, load_tracks_for_album, use_desktop_studio,
    HomeSnapshot,
};

#[component]
pub fn HomeView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();

    // All reactive reads happen INSIDE `use_resource` so Dioxus tracks `core`,
    // `library_revision`, and `offline_only` automatically!
    let snapshot = use_resource(move || {
        let core = studio.core.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match core {
                Some(c) => load_home_snapshot(&c, offline).await,
                None => HomeSnapshot::default(),
            }
        }
    });

    let data = snapshot.read().clone().unwrap_or_default();
    let sync_st = studio.sync_status.read().clone();
    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    rsx! {
        div { class: "p-6 space-y-8 max-w-6xl mx-auto",
            // Quick Stats + Discovery Triage Callout
            div { class: "flex flex-wrap items-center justify-between gap-4",
                div {
                    h2 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary",
                        "Jump Back In"
                    }
                    p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary mt-1",
                        "{data.total_track_count} IN LOCAL MIRROR · {data.cached_track_count} CACHED OFFLINE"
                    }
                }
                div { class: "flex items-center gap-2",
                    if data.pending_discovery_count > 0 {
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text hover:brightness-110 text-[13px] font-semibold cursor-pointer transition-[filter]",
                            onclick: move |_| studio.active_section.set(StudioNavSection::Discover),
                            IconSparkles { size: 14 }
                            span { "{data.pending_discovery_count} Pending Discovery → Triage" }
                        }
                    }
                    button {
                        r#type: "button",
                        disabled: sync_st.syncing || offline_only,
                        class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-40 text-[12px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| studio.sync_now(false),
                        IconRefreshCw { size: 14 }
                        span { if sync_st.syncing { "Syncing…" } else { "Sync Now" } }
                    }
                }
            }

            // Empty Library Prompt
            if data.total_track_count == 0 {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-3",
                    IconCompass { size: 32, class: "mx-auto text-beet-accent-text".to_string() }
                    h3 { class: "text-[17px] font-semibold text-beet-text-primary",
                        "Your local SQLite library is empty"
                    }
                    p { class: "text-[13px] text-beet-text-secondary max-w-md mx-auto",
                        "Run a sync to mirror your Navidrome artists, albums, playlists, and Soulbeet Discovery triage queue for 0ms offline browsing."
                    }
                    button {
                        r#type: "button",
                        disabled: sync_st.syncing || offline_only,
                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-40 cursor-pointer",
                        onclick: move |_| studio.sync_now(true),
                        IconRefreshCw { size: 15 }
                        span { "Run Full Sync" }
                    }
                }
            }

            // Recent Albums Bento Grid
            if !data.recent_albums.is_empty() {
                section { class: "space-y-3",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Recent Albums"
                    }
                    div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-3",
                        for album in data.recent_albums.iter() {
                            {
                                let album_id = album.id.clone();
                                let album_id_pin = album.id.clone();
                                let album_name = album.name.clone();
                                let album_name_play = album.name.clone();
                                let art_url = desktop_cover_art_url(album.cover_art_id.as_deref());
                                rsx! {
                                    div {
                                        key: "{album.id}",
                                        class: "group rounded-[12px] bg-beet-surface border border-beet-border-subtle hover:border-beet-border-default p-2.5 flex flex-col gap-2 transition-colors",
                                        button {
                                            r#type: "button",
                                            class: "relative aspect-square w-full rounded-[8px] overflow-hidden bg-beet-surface-high flex items-center justify-center cursor-pointer",
                                            onclick: move |_| {
                                                let aid = album_id.clone();
                                                let label = album_name_play.clone();
                                                if let Some(core) = studio.core.peek().clone() {
                                                    spawn(async move {
                                                        let tracks = load_tracks_for_album(&core, &aid).await;
                                                        studio.play_collection(player, tracks, 0, &label);
                                                    });
                                                }
                                            },
                                            if let Some(url) = art_url {
                                                img { src: "{url}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                            } else {
                                                IconMusic { size: 28, class: "text-beet-text-disabled".to_string() }
                                            }
                                            span { class: "absolute bottom-2 right-2 w-9 h-9 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity shadow-lg",
                                                IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                            }
                                        }
                                        div { class: "min-w-0 flex items-start justify-between gap-1",
                                            div { class: "min-w-0",
                                                p { class: "text-[13px] font-semibold text-beet-text-primary truncate",
                                                    "{album.name}"
                                                }
                                                p { class: "text-[12px] text-beet-text-secondary truncate",
                                                    "{album.artist_name}"
                                                }
                                            }
                                            button {
                                                r#type: "button",
                                                title: "Pin album offline",
                                                aria_label: format!("Pin {} offline", album.name),
                                                class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-leaf-text hover:bg-beet-leaf-surface shrink-0 cursor-pointer transition-colors",
                                                onclick: move |_| {
                                                    studio.pin_target_offline(
                                                        PinTarget::Album(album_id_pin.clone()),
                                                        album_name.clone(),
                                                    );
                                                },
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

            // Starred Favorites
            if !data.starred_tracks.is_empty() {
                section { class: "space-y-3",
                    div { class: "flex items-center justify-between gap-2",
                        h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "Starred Favorites"
                        }
                        div { class: "flex items-center gap-2",
                            button {
                                r#type: "button",
                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-leaf-text text-[12px] font-medium cursor-pointer",
                                onclick: move |_| studio.pin_target_offline(PinTarget::Starred, "Starred".into()),
                                IconCloudCheck { size: 13 }
                                span { "Pin Starred" }
                            }
                            {
                                let tracks_play = data.starred_tracks.clone();
                                rsx! {
                                    button {
                                        r#type: "button",
                                        class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold cursor-pointer",
                                        onclick: move |_| studio.play_collection(player, tracks_play.clone(), 0, "Starred Favorites"),
                                        IconPlay { size: 12, filled: true }
                                        span { "Play All" }
                                    }
                                }
                            }
                        }
                    }
                    div { class: "space-y-1", role: "list",
                        for (idx, track) in data.starred_tracks.iter().take(10).enumerate() {
                            {
                                let all = data.starred_tracks.clone();
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
                                        key: "star-{track.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        duration_secs: track.duration_secs,
                                        codec_badge: track.codec_badge.clone(),
                                        is_playing: row_playing,
                                        state: row_state,
                                        on_play: move |_| player.play_queue(all.clone(), idx),
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

            // Recently Played / Updated Tracks
            if !data.recent_tracks.is_empty() {
                section { class: "space-y-3",
                    h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Recently Played & Added"
                    }
                    div { class: "space-y-1", role: "list",
                        for (idx, track) in data.recent_tracks.iter().take(12).enumerate() {
                            {
                                let all = data.recent_tracks.clone();
                                let t_star = track.clone();
                                let t_rate = track.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_inspect = track.clone();
                                let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
                                let row_state = if offline_only && !track.is_offline_cached {
                                    TrackRowState::OfflineUnavailable
                                } else if track.is_discovery {
                                    TrackRowState::PendingDiscovery {
                                        days_remaining: track.days_remaining,
                                        reason_label: track.reason_label.clone(),
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
                                        key: "rec-{track.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        duration_secs: track.duration_secs,
                                        codec_badge: track.codec_badge.clone(),
                                        is_playing: row_playing,
                                        state: row_state,
                                        on_play: move |_| player.play_queue(all.clone(), idx),
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
        }
    }
}
