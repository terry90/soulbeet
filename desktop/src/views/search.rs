//! Desktop Studio Search View — 0ms local SQLite FTS5 search + online MusicBrainz catalog
//! with live 1-tap `Acquire & Play` (`POST /api/acquire-and-play`, automatically hidden when
//! global `Offline Only` mode is active).

use client_core::models::SearchResults;
use dioxus::prelude::*;
use ui::{use_player, IconMusic, IconPlay, IconSearch, IconWifiOff, TrackRow, TrackRowState};

use crate::desktop_state::{
    desktop_cover_art_url, load_cached_track_ids, load_pending_discovery_map,
    load_tracks_for_album, local_track_to_player_track, use_desktop_studio,
};

#[component]
pub fn SearchView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();

    let mut query = use_signal(move || studio.search_seed_query.read().clone());

    // Sync query if Command Palette seeded a new search term
    use_effect(move || {
        let seeded = studio.search_seed_query.read().clone();
        if !seeded.is_empty() && seeded != *query.peek() {
            query.set(seeded);
        }
    });

    // All reactive reads happen INSIDE `use_resource` so search re-runs on query, core init,
    // offline toggle, or sync updates.
    let results_res = use_resource(move || {
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
            match c.search(&q, 40).await {
                Ok(res) => Some((res, cached, disc)),
                Err(err) => {
                    tracing::warn!("local search failed: {err}");
                    None
                }
            }
        }
    });

    let acquiring_map = studio.acquiring_tracks.read().clone();
    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    rsx! {
        div { class: "p-6 space-y-6 max-w-6xl mx-auto",
            // Sticky Search Bar
            div { class: "flex items-center gap-3 px-4 h-12 rounded-[12px] bg-beet-surface border border-beet-border-default focus-within:border-beet-leaf-solid transition-colors",
                IconSearch { size: 18, class: "text-beet-text-tertiary".to_string() }
                input {
                    r#type: "text",
                    value: "{query}",
                    placeholder: if offline_only {
                        "Search cached offline library (0ms SQLite FTS5)…"
                    } else {
                        "Search local library (0ms FTS5) & MusicBrainz catalog…"
                    },
                    class: "flex-1 bg-transparent text-[15px] text-beet-text-primary placeholder:text-beet-text-tertiary outline-none",
                    oninput: move |evt| query.set(evt.value()),
                }
                if offline_only {
                    span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-leaf-surface text-beet-leaf-text tabular-nums-mono text-[10px] font-semibold",
                        IconWifiOff { size: 11 }
                        "OFFLINE"
                    }
                }
            }

            if query().trim().is_empty() {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-2",
                    IconSearch { size: 28, class: "mx-auto text-beet-text-disabled".to_string() }
                    p { class: "text-[15px] font-medium text-beet-text-secondary",
                        "Instant local FTS5 search"
                    }
                    p { class: "text-[13px] text-beet-text-tertiary",
                        "Type any artist, track, album, or genre for sub-millisecond offline results."
                    }
                }
            } else if let Some(Some((res, cached, disc))) = results_res.read().as_ref() {
                {
                    let res: &SearchResults = res;
                    let local_tracks: Vec<_> = res
                        .local_tracks
                        .iter()
                        .map(|t| local_track_to_player_track(t, cached, disc))
                        .collect();

                    rsx! {
                        div { class: "space-y-6",
                            p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                "{local_tracks.len()} TRACKS · {res.local_albums.len()} ALBUMS · {res.elapsed_ms} MS"
                            }

                            // Matching Local Tracks
                            if !local_tracks.is_empty() {
                                section { class: "space-y-2",
                                    h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                                        "In Your Library"
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

                            // Matching Local Albums
                            if !res.local_albums.is_empty() {
                                section { class: "space-y-2",
                                    h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                                        "Albums"
                                    }
                                    div { class: "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3",
                                        for album in res.local_albums.iter() {
                                            {
                                                let aid = album.id.clone();
                                                let aname = album.name.clone();
                                                let art = desktop_cover_art_url(album.cover_art_id.as_deref());
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
                                                            if let Some(u) = art {
                                                                img { src: "{u}", alt: "{album.name}", class: "w-full h-full object-cover" }
                                                            } else {
                                                                IconMusic { size: 22, class: "text-beet-text-disabled".to_string() }
                                                            }
                                                            span { class: "absolute bottom-2 right-2 w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity",
                                                                IconPlay { size: 13, filled: true }
                                                            }
                                                        }
                                                        p { class: "text-[13px] font-semibold text-beet-text-primary truncate", "{album.name}" }
                                                        p { class: "text-[12px] text-beet-text-secondary truncate", "{album.artist_name}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Remote MusicBrainz Catalog (1-tap Acquire & Play; hidden when offline_only)
                            if !offline_only && !res.remote_tracks.is_empty() {
                                section { class: "space-y-2",
                                    h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                                        "Global MusicBrainz Catalog"
                                    }
                                    div { class: "space-y-1", role: "list",
                                        for remote in res.remote_tracks.iter() {
                                            {
                                                let remote_clone = remote.clone();
                                                let row_key = format!("{}:{}", remote.artist, remote.title);
                                                let stage = acquiring_map.get(&row_key).cloned();
                                                rsx! {
                                                    TrackRow {
                                                        key: "rem-{remote.artist}-{remote.title}",
                                                        title: remote.title.clone(),
                                                        artist: remote.artist.clone(),
                                                        album: remote.album.clone(),
                                                        duration_secs: remote.duration_secs.and_then(|d| u32::try_from(d).ok()),
                                                        codec_badge: Some("MUSICBRAINZ".to_string()),
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

                            if local_tracks.is_empty() && res.local_albums.is_empty() && res.remote_tracks.is_empty() {
                                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-tertiary",
                                    "No matches found for “{res.query}”."
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
