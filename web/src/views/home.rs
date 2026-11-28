//! Studio Home view: Quick-Resume grid, active Seasonal Hero with live steering,
//! inline Discovery Triage strip, Contextual & Rolling Mixes shelf, and Jump Back In.

use dioxus::prelude::*;
use shared::recommendation::{ContextualMixKind, CultureSteering};
use ui::{
    format_mix_subtitle, format_mix_title, format_mix_window_badge, use_player, IconArrowRight,
    IconCompass, IconDisc3, IconListMusic, IconPlay, IconRadio, IconSparkles, MixSleeveShelves,
    PlaylistCategory, SeasonalHeroCard, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{fetch_album_tracks, fetch_playlist_tracks};
use crate::web_player::use_studio;
use crate::Route;

#[component]
pub fn HomePage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();

    let offline_only = player.offline_only();
    let mixes = studio.contextual_mixes.read().clone();
    let seasonal = mixes
        .iter()
        .find(|m| m.kind == ContextualMixKind::Seasonal && m.track_count > 0)
        .cloned();
    let hero_slug = seasonal.as_ref().map(|h| h.slug.as_str());
    let moment_and_rolling: Vec<_> = mixes
        .iter()
        .filter(|m| {
            Some(m.slug.as_str()) != hero_slug
                && !(m.kind == ContextualMixKind::Seasonal && m.track_count == 0)
        })
        .cloned()
        .collect();

    let culture = studio.culture_profile.read().clone();
    let local_code = culture
        .effective_country()
        .unwrap_or("LOCAL")
        .to_ascii_uppercase();

    let visible_triage = studio.visible_triage_tracks();

    let recent_albums = studio.recent_albums.read().clone();
    let playlists = studio.playlists.read().clone();
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

    let play_album = use_callback(move |(album_id, album_name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: album streaming is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            match fetch_album_tracks(&info, &album_id).await {
                Ok(mut tracks) if !tracks.is_empty() => {
                    for t in &mut tracks {
                        studio.apply_curation_overrides(t);
                    }
                    player.play_queue(tracks, 0);
                }
                _ => {
                    studio.show_toast(format!("No playable tracks found in {album_name}"));
                }
            }
        });
    });

    let play_playlist = use_callback(move |(playlist_id, name): (String, String)| {
        if player.offline_only() {
            studio.show_toast("Offline Only mode is active: playlist streaming is unavailable.");
            return;
        }
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            match fetch_playlist_tracks(&info, &playlist_id).await {
                Ok(mut tracks) => {
                    studio.sync_playlist_count(&playlist_id, tracks.len() as u32);
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

    // Prefer personal / custom smart playlists for Jump Back In so we don't duplicate
    // the "Made for Right Now" or Seasonal Hero mixes shown right below.
    let shown_mix_playlist_ids: Vec<&str> = mixes
        .iter()
        .filter_map(|m| m.navidrome_playlist_id.as_deref())
        .collect();
    let mut jump_playlists: Vec<_> = playlists
        .iter()
        .filter(|pl| !pl.category.is_soulbeet_mix())
        .take(2)
        .cloned()
        .collect();
    if jump_playlists.len() < 2 {
        let needed = 2 - jump_playlists.len();
        jump_playlists.extend(
            playlists
                .iter()
                .filter(|pl| {
                    pl.category.is_soulbeet_mix()
                        && !(pl.category == PlaylistCategory::Seasonal && pl.song_count == 0)
                        && !shown_mix_playlist_ids.contains(&pl.id.as_str())
                })
                .take(needed)
                .cloned(),
        );
    }

    rsx! {
        div { class: "space-y-8",
            // 1. Quick-Resume Bento Grid (Recent Albums + Playlists)
            if !recent_albums.is_empty() || !jump_playlists.is_empty() {
                section { class: "space-y-3",
                    div { class: "flex items-center justify-between",
                        h2 { class: "text-[18px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "Jump Back In"
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1 text-[12px] font-medium text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer",
                            onclick: move |_| {
                                nav.push(Route::LibraryPage {});
                            },
                            span { "Open Library" }
                            IconArrowRight { size: 13 }
                        }
                    }

                    div { class: "grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 gap-3",
                        for alb in recent_albums.iter().take(4) {
                            {
                                let aid = alb.id.clone();
                                let aname = alb.name.clone();
                                rsx! {
                                    button {
                                        key: "home-alb-{alb.id}",
                                        r#type: "button",
                                        class: "group flex items-center gap-3 p-2.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer",
                                        onclick: move |_| play_album.call((aid.clone(), aname.clone())),
                                        div { class: "w-12 h-12 rounded-[8px] overflow-hidden bg-beet-surface-low border border-beet-border-subtle shrink-0 flex items-center justify-center",
                                            if let Some(url) = alb.cover_art_url.as_deref() {
                                                img { src: "{url}", alt: "", class: "w-full h-full object-cover", loading: "lazy" }
                                            } else {
                                                IconDisc3 { size: 20, class: "text-beet-text-disabled".to_string() }
                                            }
                                        }
                                        div { class: "min-w-0 flex-1",
                                            p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                "{alb.name}"
                                            }
                                            p { class: "text-[12px] text-beet-text-secondary truncate",
                                                "{alb.artist}"
                                            }
                                        }
                                        span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 group-hover:scale-105 transition-[opacity,transform] shrink-0",
                                            IconPlay { size: 13, filled: true }
                                        }
                                    }
                                }
                            }
                        }

                        for pl in jump_playlists.iter() {
                            {
                                let pid = pl.id.clone();
                                let pname = pl.name.clone();
                                let category = pl.category;
                                let icon_cls = match category {
                                    PlaylistCategory::Seasonal | PlaylistCategory::Discovery => {
                                        "bg-beet-accent-surface border-beet-accent/25 text-beet-accent-text"
                                    }
                                    PlaylistCategory::MomentMix => {
                                        "bg-beet-leaf-surface border-beet-leaf-solid/25 text-beet-leaf-text"
                                    }
                                    PlaylistCategory::Smart | PlaylistCategory::User => {
                                        "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary"
                                    }
                                };
                                rsx! {
                                    button {
                                        key: "home-pl-{pl.id}",
                                        r#type: "button",
                                        class: "group flex items-center gap-3 p-2.5 rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default text-left transition-colors cursor-pointer",
                                        onclick: move |_| play_playlist.call((pid.clone(), pname.clone())),
                                        div { class: "w-12 h-12 rounded-[8px] border shrink-0 flex items-center justify-center {icon_cls}",
                                            match category {
                                                PlaylistCategory::Seasonal => rsx! { IconSparkles { size: 20 } },
                                                PlaylistCategory::Discovery => rsx! { IconCompass { size: 20 } },
                                                PlaylistCategory::MomentMix => rsx! { IconRadio { size: 20 } },
                                                PlaylistCategory::Smart | PlaylistCategory::User => rsx! { IconListMusic { size: 20 } },
                                            }
                                        }
                                        div { class: "min-w-0 flex-1",
                                            p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                "{pl.name}"
                                            }
                                            div { class: "flex items-center gap-1.5 mt-0.5",
                                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                    "{pl.song_count} TRACKS"
                                                }
                                                if let Some(tag) = category.badge_label() {
                                                    span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[9px] font-semibold tracking-wider text-beet-text-tertiary",
                                                        "{tag}"
                                                    }
                                                }
                                            }
                                        }
                                        span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 group-hover:scale-105 transition-[opacity,transform] shrink-0",
                                            IconPlay { size: 13, filled: true }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 2. Active Seasonal Hero Card (with live Culture Ratio Steering)
            if let Some(pack) = seasonal {
                {
                    let slug_play = pack.slug.clone();
                    let slug_steer = pack.slug.clone();
                    let local_ratio = pack
                        .local_pct
                        .map(|p| f32::from(p) / 100.0)
                        .unwrap_or(culture.local_culture_ratio);
                    let pack_country = pack
                        .country_code
                        .clone()
                        .unwrap_or_else(|| local_code.clone());
                    rsx! {
                        SeasonalHeroCard {
                            title: format_mix_title(&pack),
                            subtitle: format_mix_subtitle(&pack),
                            window_badge: format_mix_window_badge(&pack),
                            mix_slug: Some(pack.slug.clone()),
                            track_count: pack.track_count,
                            local_ratio,
                            local_label: pack_country,
                            steering: pack.steering.unwrap_or(CultureSteering::Balanced),
                            exclude_from_core_taste: pack.exclude_from_core_taste,
                            on_play: move |_| studio.play_contextual_mix(auth, player, slug_play.clone()),
                            on_steer: move |next| studio.steer_seasonal_mix(auth, slug_steer.clone(), next),
                        }
                    }
                }
            }

            // 3. Inline Pending Discovery Triage Callout & Strip
            if !visible_triage.is_empty() {
                section { class: "rounded-[14px] bg-beet-surface-low border border-beet-border-subtle p-4 sm:p-5 space-y-3",
                    div { class: "flex flex-wrap items-center justify-between gap-2",
                        div { class: "flex items-center gap-2",
                            IconSparkles { size: 16, class: "text-beet-accent-text".to_string() }
                            h2 { class: "text-[17px] font-semibold text-beet-text-primary",
                                "Discovery Triage"
                            }
                            span { class: "px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold",
                                "{visible_triage.len()} AWAITING"
                            }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1 text-[12px] font-medium text-beet-accent-text hover:underline cursor-pointer",
                            onclick: move |_| {
                                nav.push(Route::DiscoverPage {});
                            },
                            span { "Open Triage Deck" }
                            IconArrowRight { size: 13 }
                        }
                    }

                    div { class: "space-y-1", role: "list", aria_label: "Pending discovery tracks",
                        for dt in visible_triage.iter().take(4) {
                            {
                                let track = studio.track_from_discovery(dt);
                                let is_playing = !track.id.is_empty() && player.is_track_playing(&track.id);
                                let triage_for_row = visible_triage.clone();
                                let t_play = track.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_rate = track.clone();
                                let t_inspect = track.clone();
                                rsx! {
                                    TrackRow {
                                        key: "home-triage-{dt.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        is_playing,
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
            }

            // 4. Contextual & Rolling Mixes Split-Shelf (Right Now wide + From Your Collection square)
            if !moment_and_rolling.is_empty() {
                MixSleeveShelves {
                    mixes: moment_and_rolling,
                    id_prefix: "home".to_string(),
                    show_owned_pct: true,
                    header_action: Some(rsx! {
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1 text-[12px] font-medium text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer",
                            onclick: move |_| {
                                nav.push(Route::DiscoverPage {});
                            },
                            span { "Explore Discover" }
                            IconArrowRight { size: 13 }
                        }
                    }),
                    on_play: move |slug| studio.play_contextual_mix(auth, player, slug),
                }
            }

            // 5. Starred Favorites / Library Tracks
            if !starred.is_empty() {
                section { class: "space-y-3",
                    h2 { class: "text-[18px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Starred Favorites"
                    }
                    div { class: "space-y-1", role: "list", aria_label: "Starred tracks",
                        for (idx, track) in starred.iter().take(10).enumerate() {
                            {
                                let all_tracks = starred.clone();
                                let is_playing = player.is_track_playing(&track.id);
                                let t_star = track.clone();
                                let t_rate = track.clone();
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
                                        key: "home-star-{track.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        duration_secs: track.duration_secs,
                                        codec_badge: track.codec_badge.clone(),
                                        is_playing,
                                        state: row_state,
                                        on_play: move |_| player.play_queue(all_tracks.clone(), idx),
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

            // Clean Empty / First-Run Onboarding Card when Navidrome & Mixes are not yet populated
            if recent_albums.is_empty() && playlists.is_empty() && visible_triage.is_empty() && mixes.is_empty() {
                section { class: "rounded-[16px] bg-beet-surface border border-beet-border-default p-6 sm:p-8 text-center space-y-4",
                    div { class: "w-12 h-12 rounded-full bg-beet-accent-surface text-beet-accent-text mx-auto flex items-center justify-center",
                        IconCompass { size: 24 }
                    }
                    div { class: "space-y-1.5 max-w-md mx-auto",
                        h2 { class: "text-[20px] font-bold text-beet-text-primary",
                            "Welcome to Soulbeet Studio"
                        }
                        p { class: "text-[14px] text-beet-text-secondary",
                            "Connect Navidrome in Settings to stream your library, or jump straight into Search to Acquire & Play any track from the catalog."
                        }
                    }
                    div { class: "flex flex-wrap items-center justify-center gap-3 pt-1",
                        button {
                            r#type: "button",
                            class: "h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 cursor-pointer",
                            onclick: move |_| {
                                nav.push(Route::SearchPage {});
                            },
                            "Search & Acquire"
                        }
                        button {
                            r#type: "button",
                            class: "h-10 px-5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[14px] font-medium cursor-pointer",
                            onclick: move |_| {
                                nav.push(Route::DiscoverPage {});
                            },
                            "Configure Taste DNA"
                        }
                    }
                }
            }
        }
    }
}
