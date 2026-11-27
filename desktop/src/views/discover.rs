//! Desktop Studio Discover & Triage View: Pending Discovery queue with 1-click Keep / Drop
//! (5-second Undo via `DesktopStudio`), Seasonal Hero card with real server steering, and
//! Taste DNA populated from `ClientBootstrap` + local SQLite top genres fallback.

use client_core::models::PinTarget;
use dioxus::prelude::*;
use shared::recommendation::{CultureSteering, UserCultureProfile};
use ui::{
    use_player, IconLoader, IconPlay, IconRefreshCw, IconSettings, IconSparkles, SeasonalHeroCard,
    StudioNavSection, TasteDnaBar, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    load_local_top_genres, load_pending_discovery_tracks, load_tracks_for_playlist,
    use_desktop_studio,
};

#[component]
pub fn DiscoverView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();
    let sync_st = studio.sync_status.read().clone();
    let is_generating = (studio.discovery_generating)();
    let progress_label = studio.discovery_progress_label.read().clone();
    let progress_pct = (studio.discovery_progress_pct)();
    let refresh_error = studio.discovery_refresh_error.read().clone();
    let mut steering = use_signal(|| CultureSteering::Balanced);

    // Reactive reads INSIDE `use_resource` so triage & genres refresh automatically on sync/curation.
    let pending_res = use_resource(move || {
        let core = studio.core.read().clone();
        let _rev = (studio.library_revision)();
        let offline = player.offline_only();
        async move {
            match core {
                Some(c) => {
                    let tracks = load_pending_discovery_tracks(&c, offline).await;
                    let genres = load_local_top_genres(&c).await;
                    (tracks, genres)
                }
                None => (Vec::new(), Vec::new()),
            }
        }
    });

    let (raw_tracks, local_genres) = pending_res.read().clone().unwrap_or_default();

    let drops_queue = studio.pending_drops.read();
    let kept_set = studio.optimistic_kept.read();
    let visible_tracks: Vec<_> = raw_tracks
        .into_iter()
        .filter(|t| {
            !drops_queue.is_hidden(&t.id)
                && !kept_set.contains(&t.id)
                && !t
                    .discovery_track_id
                    .as_deref()
                    .is_some_and(|did| drops_queue.is_hidden(did) || kept_set.contains(did))
        })
        .collect();
    drop(kept_set);
    drop(drops_queue);

    let playlists = studio.playlists.read().clone();
    let seasonal_pl = playlists
        .iter()
        .find(|p| p.playlist.kind == "seasonal" && p.playlist.song_count > 0)
        .cloned();
    let weekly_radar_pl = playlists
        .iter()
        .find(|p| p.playlist.kind == "discovery" && p.playlist.song_count > 0)
        .cloned();

    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    let boot_culture = studio
        .bootstrap
        .read()
        .as_ref()
        .and_then(|b| b.active_culture_profile.clone());
    let base_ratio = boot_culture
        .as_ref()
        .map(|c| c.local_culture_ratio)
        .unwrap_or(0.45);
    let local_code = boot_culture
        .as_ref()
        .map(|c| c.home_country.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "LOCAL".to_string());
    let effective_ratio = match steering() {
        CultureSteering::MoreLocal => (base_ratio + 0.25).min(0.85),
        CultureSteering::Balanced => base_ratio,
        CultureSteering::MoreInternational => (base_ratio - 0.25).max(0.15),
    };

    let taste_profile = UserCultureProfile {
        home_country: Some(local_code.clone()),
        preferred_languages: boot_culture
            .as_ref()
            .map(|c| c.primary_languages.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["EN".into()]),
        local_culture_ratio: effective_ratio,
        inferred_area_weights: Vec::new(),
        inferred_local_tags: Vec::new(),
        weather_city: boot_culture.as_ref().and_then(|c| c.weather_city.clone()),
        ..Default::default()
    };

    rsx! {
        div { class: "p-6 space-y-8 max-w-6xl mx-auto",
            // Discover Header + Inline Discovery Refresh Progress
            header { class: "space-y-3",
                div { class: "flex flex-wrap items-center justify-between gap-3",
                    div {
                        h1 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary",
                            "Discover"
                        }
                        p { class: "text-[12px] text-beet-text-tertiary mt-0.5",
                            "Review staged recommendations, rate tracks, and steer seasonal packs."
                        }
                    }
                    div { class: "flex items-center gap-2",
                        button {
                            r#type: "button",
                            disabled: is_generating || sync_st.syncing || offline_only,
                            class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed cursor-pointer transition-[filter]",
                            onclick: move |_| studio.refresh_discovery(),
                            IconRefreshCw {
                                size: 14,
                                class: if is_generating || sync_st.syncing { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                            }
                            span {
                                if is_generating {
                                    "Refreshing..."
                                } else if sync_st.syncing {
                                    "Syncing..."
                                } else {
                                    "Refresh Discovery"
                                }
                            }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer transition-colors",
                            onclick: move |_| studio.active_section.set(StudioNavSection::Settings),
                            IconSettings { size: 14 }
                            span { "Settings" }
                        }
                    }
                }

                if is_generating {
                    {
                        let status_label = progress_label
                            .unwrap_or_else(|| "Starting discovery refresh...".to_string());
                        let pct = progress_pct.unwrap_or(8);
                        rsx! {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-accent/30 px-4 py-3 space-y-2",
                                div { class: "flex items-center justify-between gap-3 text-[12px]",
                                    span { class: "inline-flex items-center gap-2 font-medium text-beet-text-primary truncate",
                                        IconLoader { size: 13 }
                                        span { class: "truncate", "{status_label}" }
                                    }
                                    span { class: "tabular-nums-mono text-[11px] font-semibold text-beet-accent-text shrink-0",
                                        "{pct}%"
                                    }
                                }
                                div { class: "h-1.5 w-full rounded-full bg-beet-surface-high overflow-hidden",
                                    div {
                                        class: "h-full rounded-full bg-beet-leaf-solid transition-[width] duration-300",
                                        style: "width: {pct}%;",
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some(ref err) = refresh_error {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3.5 py-2 text-[12px] text-beet-crimson-text",
                        "{err}"
                    }
                }
            }

            // Seasonal / Discovery Hero Card (never rendered when empty!)
            if let Some(seasonal) = seasonal_pl {
                {
                    let pl_id = seasonal.playlist.id.clone();
                    let slug = seasonal
                        .playlist
                        .slug
                        .clone()
                        .unwrap_or_else(|| seasonal.playlist.id.clone());
                    let slug_steer = slug.clone();
                    let title = if seasonal.playlist.name.starts_with("Soulbeet: ") {
                        seasonal
                            .playlist
                            .slug
                            .as_deref()
                            .map(|s| shared::recommendation::canonical_mix_title(s, None, None))
                            .unwrap_or_else(|| seasonal.playlist.name.clone())
                    } else {
                        seasonal.playlist.name.clone()
                    };
                    let title_play = title.clone();
                    let title_pin = title.clone();
                    let is_pinned = seasonal.is_pinned;
                    let count = u32::try_from(seasonal.playlist.song_count.max(0)).unwrap_or(0);
                    let subtitle = seasonal
                        .playlist
                        .comment
                        .as_deref()
                        .map(str::trim)
                        .filter(|c| {
                            !c.is_empty() && !c.to_ascii_lowercase().starts_with("soulbeet:")
                        })
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            "Curated seasonal discovery pack mirrored offline.".to_string()
                        });
                    rsx! {
                        SeasonalHeroCard {
                            title,
                            subtitle,
                            window_badge: Some("SEASONAL PACK · AUTO-EXPIRES".to_string()),
                            track_count: count,
                            local_ratio: effective_ratio,
                            local_label: local_code.clone(),
                            steering: steering(),
                            is_pinned_offline: is_pinned,
                            exclude_from_core_taste: true,
                            on_play: move |_| {
                                let pid = pl_id.clone();
                                let label = title_play.clone();
                                if let Some(core) = studio.core.peek().clone() {
                                    spawn(async move {
                                        let tracks = load_tracks_for_playlist(&core, &pid).await;
                                        studio.play_collection(player, tracks, 0, &label);
                                    });
                                }
                            },
                            on_pin_offline: move |_| {
                                if is_pinned {
                                    studio.unpin_target_offline(
                                        PinTarget::SeasonalPack(slug.clone()),
                                        title_pin.clone(),
                                    );
                                } else {
                                    studio.pin_target_offline(
                                        PinTarget::SeasonalPack(slug.clone()),
                                        title_pin.clone(),
                                    );
                                }
                            },
                            on_steer: move |next| {
                                steering.set(next);
                                studio.steer_seasonal_pack(slug_steer.clone(), next);
                            },
                        }
                    }
                }
            } else if !visible_tracks.is_empty() || weekly_radar_pl.is_some() {
                {
                    let radar_id = weekly_radar_pl.as_ref().map(|p| p.playlist.id.clone());
                    let is_pinned = weekly_radar_pl.as_ref().is_some_and(|p| p.is_pinned);
                    let tracks_play = visible_tracks.clone();
                    let radar_count = if !visible_tracks.is_empty() {
                        visible_tracks.len() as u32
                    } else {
                        weekly_radar_pl
                            .as_ref()
                            .map(|p| u32::try_from(p.playlist.song_count.max(0)).unwrap_or(0))
                            .unwrap_or(0)
                    };
                    rsx! {
                        SeasonalHeroCard {
                            title: "Fresh Picks".to_string(),
                            subtitle: "Keep your favorites in permanent lossless storage or drop them with 1 click (5-second Undo safety).".to_string(),
                            window_badge: Some("DISCOVERY TRIAGE · OFFLINE READY".to_string()),
                            track_count: radar_count,
                            local_ratio: effective_ratio,
                            local_label: local_code.clone(),
                            steering: steering(),
                            is_pinned_offline: is_pinned,
                            exclude_from_core_taste: false,
                            on_play: move |_| {
                                if !tracks_play.is_empty() {
                                    studio.play_collection(player, tracks_play.clone(), 0, "Fresh Picks");
                                } else if let (Some(pid), Some(core)) = (radar_id.clone(), studio.core.peek().clone()) {
                                    spawn(async move {
                                        let tracks = load_tracks_for_playlist(&core, &pid).await;
                                        studio.play_collection(player, tracks, 0, "Fresh Picks");
                                    });
                                }
                            },
                            on_pin_offline: move |_| {
                                if is_pinned {
                                    studio.unpin_target_offline(PinTarget::WeeklyRadar, "Fresh Picks".into());
                                } else {
                                    studio.pin_target_offline(PinTarget::WeeklyRadar, "Fresh Picks".into());
                                }
                            },
                            on_steer: move |next| steering.set(next),
                        }
                    }
                }
            }

            // Pending Discovery Triage Queue
            section { class: "space-y-3",
                div { class: "flex items-center justify-between gap-3",
                    div {
                        h3 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary flex items-center gap-2",
                            IconSparkles { size: 16, class: "text-beet-accent-text".to_string() }
                            span { "Pending Discovery Triage" }
                        }
                        p { class: "text-[12px] text-beet-text-secondary mt-0.5",
                            "Press P to Keep, X to Drop (5s Undo), or S to Favorite & Keep. Works 100% offline via mutation outbox."
                        }
                    }
                    if !visible_tracks.is_empty() {
                        {
                            let play_all = visible_tracks.clone();
                            rsx! {
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 cursor-pointer",
                                    onclick: move |_| studio.play_collection(player, play_all.clone(), 0, "Discovery Triage"),
                                    IconPlay { size: 13, filled: true }
                                    span { "Triage All ({visible_tracks.len()})" }
                                }
                            }
                        }
                    }
                }

                if visible_tracks.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-2",
                        p { class: "text-[14px] font-medium text-beet-text-secondary",
                            "All caught up: no pending discovery tracks awaiting review."
                        }
                        p { class: "text-[12px] text-beet-text-tertiary",
                            "Click Refresh Discovery above after Soulbeet generates your next Fresh Picks."
                        }
                    }
                } else {
                    div { class: "space-y-1", role: "list", aria_label: "Pending discovery triage queue",
                        for (idx, track) in visible_tracks.iter().enumerate() {
                            {
                                let queue_all = visible_tracks.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_rate = track.clone();
                                let t_inspect = track.clone();
                                let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
                                let row_state = if offline_only && !track.is_offline_cached {
                                    TrackRowState::OfflineUnavailable
                                } else {
                                    TrackRowState::PendingDiscovery {
                                        days_remaining: track.days_remaining,
                                        reason_label: track.reason_label.clone(),
                                        rating: track.rating,
                                    }
                                };
                                rsx! {
                                    TrackRow {
                                        key: "disc-{track.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        duration_secs: track.duration_secs,
                                        codec_badge: track.codec_badge.clone(),
                                        is_playing: row_playing,
                                        state: row_state,
                                        on_play: move |_| player.play_queue(queue_all.clone(), idx),
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

            // Taste DNA Bar (populated from ClientBootstrap + local SQLite top genres)
            TasteDnaBar {
                profile: taste_profile,
                top_genres: local_genres,
            }
        }
    }
}
