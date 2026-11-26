//! Unified Studio Search view: instant local library + discovery results alongside
//! global MusicBrainz catalog results with 1-click `Acquire & Play` (`beet-seam-light`),
//! `Offline Only` filtering, and full access to Soulbeet's Advanced Soulseek Search.

use dioxus::prelude::*;
use shared::download::AcquireAndPlayRequest;
use shared::metadata::{SearchResult, Track};
use ui::{
    format_mix_title, friendly_error, use_player, IconDisc3, IconLoader, IconPlay, IconSearch,
    IconSparkles, IconWifiOff, PlayerTrack, Search as LegacySoulseekSearch, TrackRow,
    TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{fetch_album_tracks, search_navidrome, NavidromeSearchHits};
use crate::web_player::{acquire_key, use_studio, ManualChoiceModalState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    InLibrary,
    Catalog,
    Soulseek,
}

#[component]
pub fn SearchPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();

    let mut filter = use_signal(|| SearchFilter::All);
    let mut query = use_signal(String::new);
    let mut searching = use_signal(|| false);
    let mut search_error = use_signal(|| None::<String>);
    let mut local_hits = use_signal(NavidromeSearchHits::default);
    let mut catalog_tracks = use_signal(Vec::<Track>::new);
    let mut has_searched = use_signal(|| false);

    let offline_only = player.offline_only();

    let run_search = use_callback(move |raw: String| {
        let q = raw.trim().to_string();
        if q.is_empty() {
            has_searched.set(false);
            local_hits.set(NavidromeSearchHits::default());
            catalog_tracks.set(Vec::new());
            search_error.set(None);
            return;
        }
        searching.set(true);
        has_searched.set(true);
        search_error.set(None);
        let nav_info = studio.navidrome_info();
        let is_offline = player.offline_only();

        spawn(async move {
            // 1. Local Navidrome search3
            if let Some(info) = nav_info {
                if let Ok(hits) = search_navidrome(&info, &q).await {
                    local_hits.set(hits);
                }
            } else {
                local_hits.set(NavidromeSearchHits::default());
            }

            // 2. Global MusicBrainz catalog search (strictly skipped when Offline Only is active)
            if is_offline {
                catalog_tracks.set(Vec::new());
            } else {
                match auth
                    .call(api::search_track(api::SearchQuery {
                        artist: None,
                        query: q.clone(),
                    }))
                    .await
                {
                    Ok(results) => {
                        let tracks: Vec<Track> = results
                            .into_iter()
                            .filter_map(|r| match r {
                                SearchResult::Track(t) => Some(t),
                                SearchResult::Album(_) => None,
                            })
                            .collect();
                        catalog_tracks.set(tracks);
                    }
                    Err(e) => {
                        search_error.set(Some(friendly_error(&e)));
                    }
                }
            }
            searching.set(false);
        });
    });

    // Honor Command Palette "Search for <query>" jumps automatically
    use_effect(move || {
        if let Some(incoming) = (studio.search_query_override)() {
            studio.search_query_override.set(None);
            query.set(incoming.clone());
            filter.set(SearchFilter::All);
            run_search.call(incoming);
        }
    });

    let q_lower = query().trim().to_ascii_lowercase();
    let matching_triage: Vec<PlayerTrack> = if q_lower.is_empty() {
        Vec::new()
    } else {
        studio
            .triage_tracks
            .read()
            .iter()
            .filter(|t| {
                t.title.to_ascii_lowercase().contains(&q_lower)
                    || t.artist.to_ascii_lowercase().contains(&q_lower)
                    || t.album.to_ascii_lowercase().contains(&q_lower)
            })
            .map(|t| studio.track_from_discovery(t, 14))
            .collect()
    };

    let acquire_map = studio.acquire_states.read().clone();
    let mixes = studio.contextual_mixes.read().clone();
    let starred = studio.starred_tracks.read().clone();

    rsx! {
        div { class: "space-y-6",
            // Sticky Search Bar + Filter Chips
            div { class: "space-y-3",
                if filter() != SearchFilter::Soulseek {
                    form {
                        class: "flex items-center gap-2",
                        onsubmit: move |evt| {
                            evt.prevent_default();
                            run_search.call(query());
                        },
                        div { class: "relative flex-1",
                            div { class: "absolute left-3.5 top-1/2 -translate-y-1/2 text-beet-text-tertiary pointer-events-none",
                                IconSearch { size: 17 }
                            }
                            input {
                                r#type: "search",
                                value: "{query}",
                                placeholder: if offline_only {
                                    "Search cached library tracks…"
                                } else {
                                    "Search your library or any track on MusicBrainz to Acquire & Play…"
                                },
                                class: "w-full h-11 pl-10 pr-4 rounded-[12px] bg-beet-surface border border-beet-border-default focus:border-beet-accent/50 text-[15px] text-beet-text-primary placeholder:text-beet-text-tertiary outline-none transition-colors",
                                oninput: move |evt| query.set(evt.value()),
                            }
                        }
                        button {
                            r#type: "submit",
                            disabled: searching() || query().trim().is_empty(),
                            class: "inline-flex items-center gap-2 h-11 px-5 rounded-[12px] bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-45 cursor-pointer shrink-0",
                            if searching() {
                                IconLoader { size: 16 }
                                span { "Searching…" }
                            } else {
                                span { "Search" }
                            }
                        }
                    }
                }

                // Filter Pills
                div { class: "flex flex-wrap items-center gap-2",
                    for (tab, label) in [
                        (SearchFilter::All, "All"),
                        (SearchFilter::InLibrary, "In Library"),
                        (SearchFilter::Catalog, "Global Catalog"),
                        (SearchFilter::Soulseek, "Advanced Soulseek"),
                    ] {
                        {
                            let active = filter() == tab;
                            let disabled_offline = offline_only
                                && matches!(tab, SearchFilter::Catalog | SearchFilter::Soulseek);
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
                                    onclick: move |_| filter.set(tab),
                                    "{label}"
                                }
                            }
                        }
                    }
                }
            }

            // Advanced Soulseek Power-User Mode (preserves 100% of existing ui::Search!)
            if filter() == SearchFilter::Soulseek {
                LegacySoulseekSearch {}
            } else if !has_searched() {
                // Zero-Query State (Never Blank!)
                div { class: "space-y-6 pt-2",
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
                                                class: "flex items-center justify-between gap-3 p-3.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-left transition-colors cursor-pointer",
                                                onclick: move |_| studio.play_contextual_mix(auth, player, slug.clone()),
                                                div { class: "min-w-0",
                                                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                        "{format_mix_title(mix)}"
                                                    }
                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
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
                                "Quick Play from Starred"
                            }
                            div { class: "space-y-1", role: "list",
                                for (idx, track) in starred.iter().take(6).enumerate() {
                                    {
                                        let queue = starred.clone();
                                        let t_star = track.clone();
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
                // Active Search Results
                if let Some(err) = search_error() {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-4 py-3 text-[13px] text-beet-crimson-text",
                        "{err}"
                    }
                }

                // 1. In Library & Discovery Matches
                if matches!(filter(), SearchFilter::All | SearchFilter::InLibrary) {
                    {
                        let hits = local_hits.read().clone();
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
                                                        class: "p-2.5 rounded-[10px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-left transition-colors cursor-pointer space-y-2",
                                                        onclick: move |_| {
                                                            let Some(info) = studio.navidrome_info() else {
                                                                return;
                                                            };
                                                            let aid = aid.clone();
                                                            spawn(async move {
                                                                if let Ok(tracks) = fetch_album_tracks(&info, &aid).await {
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
                                                        },
                                                        on_play: move |_| player.play_track(t_play.clone()),
                                                        on_keep: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                                        on_drop: move |_| studio.drop_track(auth, player, t_drop.clone()),
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
                    }
                }

                // 2. Global MusicBrainz Catalog ("Not in Library — Acquire & Play")
                if matches!(filter(), SearchFilter::All | SearchFilter::Catalog) {
                    if offline_only {
                        div { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-4 flex items-center gap-3 text-[13px] text-beet-text-secondary",
                            IconWifiOff { size: 17, class: "text-beet-leaf-text".to_string() }
                            span { "Global MusicBrainz catalog is hidden while Offline Only mode is active." }
                        }
                    } else {
                        {
                            let tracks = catalog_tracks.read().clone();
                            rsx! {
                                section { class: "space-y-3",
                                    div { class: "flex items-center justify-between",
                                        div { class: "flex items-center gap-2",
                                            IconSparkles { size: 15, class: "text-beet-accent-text".to_string() }
                                            h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                                                "Global Catalog — Tap to Acquire & Play"
                                            }
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                            "MUSICBRAINZ · {tracks.len()}"
                                        }
                                    }

                                    if tracks.is_empty() && !searching() {
                                        p { class: "text-[13px] text-beet-text-tertiary py-2",
                                            "No catalog tracks found for this query."
                                        }
                                    } else {
                                        div { class: "space-y-1", role: "list", aria_label: "MusicBrainz catalog tracks",
                                            for track in tracks.iter() {
                                                {
                                                    let key = acquire_key(&track.artist, &track.title);
                                                    let acq_entry = acquire_map.get(&key).cloned();
                                                    let acquiring_stage = acq_entry.as_ref().map(|e| e.badge.clone());
                                                    let manual_choice = acq_entry.and_then(|e| e.manual_choice);
                                                    let duration_secs = parse_duration_secs(track.duration.as_deref());
                                                    let req = AcquireAndPlayRequest {
                                                        artist: track.artist.clone(),
                                                        title: track.title.clone(),
                                                        album: track.album_title.clone(),
                                                        recording_mbid: track.mbid.clone(),
                                                        release_mbid: track.release_mbid.clone(),
                                                        duration_s: duration_secs,
                                                        folder_id: None,
                                                        prefer_fast: true,
                                                    };
                                                    let artist_for_pick = track.artist.clone();
                                                    let title_for_pick = track.title.clone();
                                                    rsx! {
                                                        TrackRow {
                                                            key: "srch-mb-{track.id}",
                                                            title: track.title.clone(),
                                                            artist: track.artist.clone(),
                                                            album: track.album_title.clone(),
                                                            duration_secs,
                                                            codec_badge: Some("MUSICBRAINZ".to_string()),
                                                            state: TrackRowState::UnownedCatalog { acquiring_stage },
                                                            on_acquire: move |_| {
                                                                studio.trigger_acquire_and_play(auth, player, req.clone());
                                                            },
                                                            on_pick_source: move |_| {
                                                                if let Some((search_id, groups)) = manual_choice.clone() {
                                                                    studio.manual_choice_modal.set(Some(ManualChoiceModalState {
                                                                        artist: artist_for_pick.clone(),
                                                                        title: title_for_pick.clone(),
                                                                        search_id,
                                                                        groups,
                                                                    }));
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
