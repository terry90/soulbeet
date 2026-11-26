//! Desktop Studio Discover & Triage View — Pending Discovery queue with 1-click Keep / Drop
//! (5-second Undo via `DesktopStudio`), Seasonal Hero card, and Taste DNA summary.

use client_core::models::PinTarget;
use dioxus::prelude::*;
use shared::recommendation::{CultureSteering, UserCultureProfile};
use ui::{
    use_player, IconPlay, IconSparkles, SeasonalHeroCard, TasteDnaBar, TrackRow, TrackRowState,
};

use crate::desktop_state::{
    load_pending_discovery_tracks, load_tracks_for_playlist, use_desktop_studio,
};

#[component]
pub fn DiscoverView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();
    let rev = (studio.library_revision)();
    let core_opt = studio.core.read().clone();
    let mut steering = use_signal(|| CultureSteering::Balanced);

    let pending_res = use_resource(move || {
        let core = core_opt.clone();
        let _rev = rev;
        async move {
            match core {
                Some(c) => load_pending_discovery_tracks(&c, offline_only).await,
                None => Vec::new(),
            }
        }
    });

    let drops_queue = studio.pending_drops.read();
    let visible_tracks: Vec<_> = pending_res
        .read()
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            !drops_queue.is_hidden(&t.id)
                && !t
                    .discovery_track_id
                    .as_deref()
                    .is_some_and(|did| drops_queue.is_hidden(did))
        })
        .collect();
    drop(drops_queue);

    let playlists = studio.playlists.read().clone();
    let seasonal_pl = playlists
        .iter()
        .find(|p| p.playlist.kind == "seasonal")
        .cloned();
    let weekly_radar_pl = playlists
        .iter()
        .find(|p| p.playlist.kind == "discovery")
        .cloned();

    let current_id = player.current_track().map(|t| t.id);
    let is_playing = player.is_playing();

    let effective_ratio = match steering() {
        CultureSteering::MoreLocal => 0.70,
        CultureSteering::Balanced => 0.45,
        CultureSteering::MoreInternational => 0.20,
    };

    let taste_profile = UserCultureProfile {
        home_country: Some("LOCAL".into()),
        preferred_languages: vec!["EN".into()],
        local_culture_ratio: effective_ratio,
        inferred_area_weights: Vec::new(),
        inferred_local_tags: vec!["Discovery Radar".into(), "Offline Studio".into()],
    };

    rsx! {
        div { class: "p-6 space-y-8 max-w-6xl mx-auto",
            // Seasonal / Discovery Hero Card
            if let Some(seasonal) = seasonal_pl {
                {
                    let pl_id = seasonal.playlist.id.clone();
                    let slug = seasonal
                        .playlist
                        .slug
                        .clone()
                        .unwrap_or_else(|| seasonal.playlist.id.clone());
                    let title = seasonal.playlist.name.clone();
                    let title_pin = title.clone();
                    let is_pinned = seasonal.is_pinned;
                    let count = u32::try_from(seasonal.playlist.song_count.max(0)).unwrap_or(0);
                    rsx! {
                        SeasonalHeroCard {
                            title,
                            subtitle: seasonal.playlist.comment.clone().unwrap_or_else(|| "Curated seasonal discovery pack mirrored offline.".to_string()),
                            window_badge: Some("SEASONAL PACK · AUTO-EXPIRES".to_string()),
                            track_count: count,
                            local_ratio: effective_ratio,
                            local_label: "LOCAL".to_string(),
                            steering: steering(),
                            is_pinned_offline: is_pinned,
                            exclude_from_core_taste: true,
                            on_play: move |_| {
                                let pid = pl_id.clone();
                                if let Some(core) = studio.core.peek().clone() {
                                    spawn(async move {
                                        let tracks = load_tracks_for_playlist(&core, &pid).await;
                                        if !tracks.is_empty() {
                                            player.play_queue(tracks, 0);
                                        }
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
                            on_steer: move |next| steering.set(next),
                        }
                    }
                }
            } else {
                {
                    let radar_id = weekly_radar_pl.as_ref().map(|p| p.playlist.id.clone());
                    let is_pinned = weekly_radar_pl.as_ref().is_some_and(|p| p.is_pinned);
                    let tracks_play = visible_tracks.clone();
                    rsx! {
                        SeasonalHeroCard {
                            title: "Weekly Discovery Radar".to_string(),
                            subtitle: "Keep your favorites in permanent lossless storage or drop them with 1 click (5-second Undo safety).".to_string(),
                            window_badge: Some("DISCOVERY TRIAGE · OFFLINE READY".to_string()),
                            track_count: visible_tracks.len() as u32,
                            local_ratio: effective_ratio,
                            local_label: "LOCAL".to_string(),
                            steering: steering(),
                            is_pinned_offline: is_pinned,
                            exclude_from_core_taste: false,
                            on_play: move |_| {
                                if !tracks_play.is_empty() {
                                    player.play_queue(tracks_play.clone(), 0);
                                } else if let (Some(pid), Some(core)) = (radar_id.clone(), studio.core.peek().clone()) {
                                    spawn(async move {
                                        let tracks = load_tracks_for_playlist(&core, &pid).await;
                                        if !tracks.is_empty() {
                                            player.play_queue(tracks, 0);
                                        }
                                    });
                                }
                            },
                            on_pin_offline: move |_| {
                                if is_pinned {
                                    studio.unpin_target_offline(PinTarget::WeeklyRadar, "Weekly Radar".into());
                                } else {
                                    studio.pin_target_offline(PinTarget::WeeklyRadar, "Weekly Radar".into());
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
                            "Press P to Keep, X to Drop (5s Undo), or S to Star & auto-promote. Works 100% offline via mutation outbox."
                        }
                    }
                    if !visible_tracks.is_empty() {
                        {
                            let play_all = visible_tracks.clone();
                            rsx! {
                                button {
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 cursor-pointer",
                                    onclick: move |_| player.play_queue(play_all.clone(), 0),
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
                            "All caught up — no pending discovery tracks awaiting review."
                        }
                        p { class: "text-[12px] text-beet-text-tertiary",
                            "Run Sync Now from Home or Library after Soulbeet generates your next Weekly Radar."
                        }
                    }
                } else {
                    div { class: "space-y-1", role: "list", aria_label: "Pending discovery triage queue",
                        for (idx, track) in visible_tracks.iter().enumerate() {
                            {
                                let queue_all = visible_tracks.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_inspect = track.clone();
                                let row_playing = current_id.as_deref() == Some(track.id.as_str()) && is_playing;
                                let row_state = if offline_only && !track.is_offline_cached {
                                    TrackRowState::OfflineUnavailable
                                } else {
                                    TrackRowState::PendingDiscovery {
                                        days_remaining: track.days_remaining,
                                        reason_label: track.reason_label.clone(),
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
                                        on_inspect: move |_| studio.inspect_track(t_inspect.clone()),
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Taste DNA Bar
            TasteDnaBar {
                profile: taste_profile,
                top_genres: vec!["Lossless FLAC".into(), "Gapless Studio".into()],
            }
        }
    }
}
