//! Studio Library view: Global Offline filter, Playlists browser, Albums grid, Starred list,
//! Pending Discovery tracks, and Library Stats overview.

use dioxus::prelude::*;
use ui::{
    dashboard::StatsOverview, use_player, IconDisc3, IconListMusic, IconLoader, IconPlay,
    OfflineTogglePill, PlayerTrack, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{fetch_album_tracks, fetch_playlist_tracks};
use crate::web_player::use_studio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Playlists,
    Albums,
    Starred,
    Discovery,
    Stats,
}

#[component]
pub fn LibraryPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();

    let mut active_tab = use_signal(|| LibraryTab::Playlists);
    let mut detail_title = use_signal(|| None::<String>);
    let mut detail_tracks = use_signal(Vec::<PlayerTrack>::new);
    let mut detail_loading = use_signal(|| false);
    let mut detail_req_id = use_signal(|| 0u64);

    let offline_only = player.offline_only();
    let playlists = studio.playlists.read().clone();
    let albums = studio.recent_albums.read().clone();
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

    let load_playlist = use_callback(move |(pid, name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active — remote playlists cannot be loaded.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        let next_req = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_req);
        detail_title.set(Some(name));
        detail_loading.set(true);
        spawn(async move {
            let res = fetch_playlist_tracks(&info, &pid).await;
            if *detail_req_id.peek() != next_req {
                return;
            }
            match res {
                Ok(tracks) => detail_tracks.set(tracks),
                Err(e) => {
                    detail_tracks.set(Vec::new());
                    studio.show_toast(format!("Could not load playlist: {e}"));
                }
            }
            detail_loading.set(false);
        });
    });

    let load_album = use_callback(move |(aid, name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active — remote albums cannot be loaded.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        let next_req = detail_req_id.peek().wrapping_add(1);
        detail_req_id.set(next_req);
        detail_title.set(Some(name));
        detail_loading.set(true);
        spawn(async move {
            let res = fetch_album_tracks(&info, &aid).await;
            if *detail_req_id.peek() != next_req {
                return;
            }
            match res {
                Ok(tracks) => detail_tracks.set(tracks),
                Err(e) => {
                    detail_tracks.set(Vec::new());
                    studio.show_toast(format!("Could not load album: {e}"));
                }
            }
            detail_loading.set(false);
        });
    });

    // Honor Left Rail playlist click
    use_effect(move || {
        if let Some(pid) = (studio.selected_playlist_id)() {
            studio.selected_playlist_id.set(None);
            active_tab.set(LibraryTab::Playlists);
            let name = studio
                .playlists
                .peek()
                .iter()
                .find(|p| p.id == pid)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Playlist".to_string());
            load_playlist.call((pid, name));
        }
    });

    rsx! {
        div { class: "space-y-6",
            // Header + Offline Switch + Category Tabs
            div { class: "flex flex-wrap items-center justify-between gap-3",
                div { class: "flex flex-wrap items-center gap-2",
                    for (tab, label) in [
                        (LibraryTab::Playlists, "Playlists"),
                        (LibraryTab::Albums, "Albums"),
                        (LibraryTab::Starred, "Starred"),
                        (LibraryTab::Discovery, "Discovery"),
                        (LibraryTab::Stats, "Stats"),
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
                                    class: "h-9 px-4 rounded-full border text-[13px] font-medium transition-colors cursor-pointer {cls}",
                                    onclick: move |_| {
                                        active_tab.set(tab);
                                        detail_title.set(None);
                                    },
                                    "{label}"
                                }
                            }
                        }
                    }
                }

                OfflineTogglePill {
                    offline_only,
                    on_toggle: move |next| player.set_offline_only(next),
                }
            }

            // Detail Drill-Down (when a Playlist or Album is selected)
            if let Some(title) = detail_title() {
                section { class: "rounded-[14px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-4",
                    div { class: "flex flex-wrap items-center justify-between gap-3",
                        div {
                            h2 { class: "text-[18px] font-bold text-beet-text-primary", "{title}" }
                            p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary mt-0.5",
                                "{detail_tracks.read().len()} TRACKS"
                            }
                        }
                        div { class: "flex items-center gap-2",
                            if !detail_tracks.read().is_empty() {
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold cursor-pointer",
                                    onclick: move |_| player.play_queue(detail_tracks.read().clone(), 0),
                                    IconPlay { size: 14, filled: true }
                                    span { "Play All" }
                                }
                            }
                            button {
                                r#type: "button",
                                class: "h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer",
                                onclick: move |_| detail_title.set(None),
                                "Back"
                            }
                        }
                    }

                    if detail_loading() {
                        div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                            IconLoader { size: 16 }
                            span { "Loading tracks…" }
                        }
                    } else if detail_tracks.read().is_empty() {
                        p { class: "text-[13px] text-beet-text-tertiary py-4 text-center",
                            "No tracks found."
                        }
                    } else {
                        {
                            let list: Vec<_> = detail_tracks
                                .read()
                                .iter()
                                .cloned()
                                .map(|mut t| {
                                    studio.apply_curation_overrides(&mut t);
                                    t
                                })
                                .collect();
                            rsx! {
                                div { class: "space-y-1", role: "list",
                                    for (idx, track) in list.iter().enumerate() {
                                        {
                                            let all = list.clone();
                                            let t_star = track.clone();
                                            let t_rate = track.clone();
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
                                                    key: "lib-det-{idx}-{track.id}",
                                                    track_number: Some((idx + 1) as u32),
                                                    title: track.title.clone(),
                                                    artist: track.artist.clone(),
                                                    album: track.album.clone(),
                                                    cover_art_url: track.cover_art_url.clone(),
                                                    duration_secs: track.duration_secs,
                                                    codec_badge: track.codec_badge.clone(),
                                                    is_playing: player.is_track_playing(&track.id),
                                                    state,
                                                    on_play: move |_| player.play_queue(all.clone(), idx),
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
                }
            } else {
                match active_tab() {
                    LibraryTab::Playlists => rsx! {
                        if playlists.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No Navidrome playlists loaded yet. Connect Navidrome in Settings to browse your playlists."
                            }
                        } else {
                            div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3.5",
                                for pl in playlists.iter() {
                                    {
                                        let pid = pl.id.clone();
                                        let name = pl.name.clone();
                                        rsx! {
                                            button {
                                                key: "lib-pl-{pl.id}",
                                                r#type: "button",
                                                class: "group p-4 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-left transition-colors cursor-pointer flex items-center justify-between gap-3",
                                                onclick: move |_| load_playlist.call((pid.clone(), name.clone())),
                                                div { class: "flex items-center gap-3 min-w-0",
                                                    div { class: "w-11 h-11 rounded-[8px] bg-beet-accent-surface text-beet-accent-text flex items-center justify-center shrink-0",
                                                        IconListMusic { size: 19 }
                                                    }
                                                    div { class: "min-w-0",
                                                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate", "{pl.name}" }
                                                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                            "{pl.song_count} TRACKS"
                                                            if pl.is_smart { " · SMART" }
                                                        }
                                                    }
                                                }
                                                span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity shrink-0",
                                                    IconPlay { size: 13, filled: true }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    LibraryTab::Albums => rsx! {
                        if albums.is_empty() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No albums loaded from Navidrome yet."
                            }
                        } else {
                            div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4",
                                for alb in albums.iter() {
                                    {
                                        let aid = alb.id.clone();
                                        let name = alb.name.clone();
                                        rsx! {
                                            button {
                                                key: "lib-alb-{alb.id}",
                                                r#type: "button",
                                                class: "group rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle p-3 text-left transition-colors cursor-pointer space-y-2.5",
                                                onclick: move |_| load_album.call((aid.clone(), name.clone())),
                                                div { class: "aspect-square rounded-[8px] overflow-hidden bg-beet-surface-low flex items-center justify-center relative",
                                                    if let Some(url) = alb.cover_art_url.as_deref() {
                                                        img { src: "{url}", alt: "", class: "w-full h-full object-cover", loading: "lazy" }
                                                    } else {
                                                        IconDisc3 { size: 28, class: "text-beet-text-disabled".to_string() }
                                                    }
                                                    span { class: "absolute bottom-2 right-2 w-9 h-9 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity shadow-lg",
                                                        IconPlay { size: 15, filled: true }
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
                                        let t_star = track.clone();
                                        let t_rate = track.clone();
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
                                                on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
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
                                        let t_play = track.clone();
                                        let t_keep = track.clone();
                                        let t_drop = track.clone();
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
                                                },
                                                on_play: move |_| player.play_track(t_play.clone()),
                                                on_keep: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                                on_drop: move |_| studio.drop_track(auth, player, t_drop.clone()),
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
        }
    }
}

#[component]
fn LibraryStatsPanel() -> Element {
    let auth = use_auth();
    let stats_resource = use_resource(move || async move { auth.call(api::get_library_stats()).await });
    let binding = stats_resource.read();

    match &*binding {
        Some(Ok(stats)) => rsx! { StatsOverview { stats: stats.clone() } },
        Some(Err(_)) => rsx! {
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-secondary",
                "Could not load library stats. Check Navidrome connection in Settings."
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
