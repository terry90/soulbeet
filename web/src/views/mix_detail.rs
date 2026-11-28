//! Dedicated Mix Detail route (`/mix/:slug`): procedural sleeve header, Play, Shuffle,
//! Refresh Mix, Save as Playlist, Seasonal Culture Steering pills, and full TrackRow list.

use dioxus::prelude::*;
use shared::recommendation::{
    ContextualMixKind, ContextualMixPlaylist, CultureSteering, SteerSeasonalPackRequest,
};
use ui::{
    format_duration, format_mix_sleeve_tag, format_mix_subtitle, format_mix_title, friendly_error,
    mix_sleeve_theme, render_mix_sleeve_art, use_player, IconChevronLeft, IconListPlus, IconLoader,
    IconPlay, IconRefreshCw, IconShuffle, PlayerTrack, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::use_studio;
use crate::Route;

#[component]
pub fn MixDetailPage(slug: String) -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();

    let mut target_slug = use_signal(|| slug.clone());
    if *target_slug.peek() != slug {
        target_slug.set(slug.clone());
    }

    let mut playlist = use_signal(|| None::<ContextualMixPlaylist>);
    let mut loading = use_signal(|| true);
    let mut refreshing = use_signal(|| false);
    let mut steering_busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut req_id = use_signal(|| 0u64);
    let mut reload_tick = use_signal(|| 0u64);

    use_effect(move || {
        let target = target_slug();
        let _tick = reload_tick();
        let next_req = req_id.peek().wrapping_add(1);
        req_id.set(next_req);
        playlist.set(None);
        error.set(None);
        refreshing.set(false);
        steering_busy.set(false);
        loading.set(true);
        spawn(async move {
            match auth
                .call(api::get_contextual_mix_playlist(target.clone(), Some(false)))
                .await
            {
                Ok(data) => {
                    if *req_id.peek() == next_req {
                        {
                            let mut mixes = studio.contextual_mixes.write();
                            if let Some(slot) = mixes.iter_mut().find(|m| m.slug == target) {
                                *slot = data.summary.clone();
                            }
                        }
                        playlist.set(Some(data));
                        loading.set(false);
                    }
                }
                Err(err) => {
                    if *req_id.peek() == next_req {
                        error.set(Some(friendly_error(&err)));
                        loading.set(false);
                    }
                }
            }
        });
    });

    let handle_refresh = move |_| {
        if refreshing() {
            return;
        }
        let target = target_slug.peek().clone();
        let next_req = req_id.peek().wrapping_add(1);
        req_id.set(next_req);
        refreshing.set(true);
        steering_busy.set(false);
        error.set(None);
        spawn(async move {
            match auth.call(api::refresh_contextual_mix(target.clone())).await {
                Ok(data) => {
                    {
                        let mut mixes = studio.contextual_mixes.write();
                        if let Some(slot) = mixes.iter_mut().find(|m| m.slug == target) {
                            *slot = data.summary.clone();
                        }
                    }
                    if *req_id.peek() == next_req {
                        playlist.set(Some(data));
                        studio.show_toast("Mix refreshed");
                        refreshing.set(false);
                    }
                }
                Err(err) => {
                    if *req_id.peek() == next_req {
                        studio.show_toast(format!("Refresh failed: {}", friendly_error(&err)));
                        refreshing.set(false);
                    }
                }
            }
        });
    };

    let offline_only = player.offline_only();
    let culture = studio.culture_profile.read().clone();
    let local_code = culture
        .effective_country()
        .unwrap_or("LOCAL")
        .to_ascii_uppercase();

    rsx! {
        div { class: "space-y-6",
            // Back bar
            div { class: "flex items-center justify-between gap-2",
                button {
                    r#type: "button",
                    class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                    onclick: move |_| {
                        nav.push(Route::DiscoverPage {});
                    },
                    IconChevronLeft { size: 14 }
                    span { "Discover" }
                }
            }

            if loading() {
                div { class: "py-16 flex items-center justify-center gap-2.5 text-beet-text-tertiary text-[13px]",
                    IconLoader { size: 16, class: "animate-spin text-beet-leaf-text".to_string() }
                    span { "Loading mix tracklist..." }
                }
            } else if let Some(err) = error() {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-3",
                    p { class: "text-[14px] font-semibold text-beet-text-primary",
                        "Could not load mix"
                    }
                    p { class: "text-[13px] text-beet-text-secondary", "{err}" }
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-[13px] font-medium text-beet-text-primary hover:bg-beet-surface-highest cursor-pointer",
                        onclick: move |_| {
                            error.set(None);
                            loading.set(true);
                            reload_tick += 1;
                        },
                        IconRefreshCw { size: 14 }
                        span { "Try Again" }
                    }
                }
            } else if let Some(data) = playlist.read().clone() {
                {
                    let summary = &data.summary;
                    let theme = mix_sleeve_theme(&summary.slug);
                    let title = format_mix_title(summary);
                    let subtitle = format_mix_subtitle(summary);
                    let tag = format_mix_sleeve_tag(summary);
                    let tracks: Vec<PlayerTrack> = data
                        .tracks
                        .iter()
                        .map(|t| studio.track_from_contextual(t))
                        .collect();
                    let playable_ids: Vec<String> = tracks
                        .iter()
                        .map(|t| t.id.clone())
                        .filter(|id| !id.trim().is_empty())
                        .collect();
                    let total_secs: u32 = tracks.iter().filter_map(|t| t.duration_secs).sum();
                    let track_count = tracks.len();
                    let tracks_for_play = tracks.clone();
                    let tracks_for_shuffle = tracks.clone();
                    let save_title = title.clone();
                    let save_ids = playable_ids.clone();
                    let is_seasonal = summary.kind == ContextualMixKind::Seasonal;
                    let cur_steering = summary.steering.unwrap_or(CultureSteering::Balanced);
                    let country_code = summary
                        .country_code
                        .clone()
                        .unwrap_or_else(|| local_code.clone())
                        .to_ascii_uppercase();
                    let more_local = if country_code.is_empty() || country_code == "LOCAL" {
                        "More Local".to_string()
                    } else {
                        format!("More {country_code}")
                    };
                    let steering_opts = [
                        (CultureSteering::MoreLocal, more_local),
                        (CultureSteering::Balanced, "Balanced".to_string()),
                        (CultureSteering::MoreInternational, "More Intl".to_string()),
                    ];

                    rsx! {
                        // Mix Header Card
                        section {
                            class: "relative overflow-hidden rounded-[16px] border p-5 sm:p-6 flex flex-col sm:flex-row items-start sm:items-end gap-5",
                            style: "background:{theme.background};border-color:{theme.border};",
                            div {
                                class: "absolute inset-0 pointer-events-none opacity-45",
                                aria_hidden: "true",
                                {render_mix_sleeve_art(&summary.slug, &format!("mix-hdr-{}", summary.slug))}
                            }
                            div {
                                class: "absolute inset-0 pointer-events-none",
                                aria_hidden: "true",
                                style: "background:linear-gradient(95deg, color-mix(in srgb, var(--beet-canvas) 82%, transparent) 0%, color-mix(in srgb, var(--beet-canvas) 54%, transparent) 60%, color-mix(in srgb, var(--beet-canvas) 22%, transparent) 100%);",
                            }

                            // Sleeve square badge
                            div {
                                class: "relative z-10 w-32 h-32 sm:w-40 sm:h-40 rounded-[12px] overflow-hidden border shrink-0 shadow-xl",
                                style: "background:{theme.background};border-color:{theme.border};",
                                {render_mix_sleeve_art(&summary.slug, &format!("mix-sq-{}", summary.slug))}
                            }

                            div { class: "relative z-10 flex-1 min-w-0 space-y-3",
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full border tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                    style: "background:{theme.pill_bg};color:{theme.pill_fg};border-color:{theme.border};",
                                    "{tag}"
                                }
                                div { class: "space-y-1",
                                    h1 { class: "text-2xl sm:text-[32px] font-bold tracking-[-0.025em] leading-[1.12] text-beet-text-primary",
                                        "{title}"
                                    }
                                    p { class: "text-[14px] text-beet-text-secondary max-w-2xl",
                                        "{subtitle}"
                                    }
                                }
                                div { class: "flex flex-wrap items-center gap-2 tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    span {
                                        if track_count == 1 {
                                            "1 TRACK"
                                        } else {
                                            "{track_count} TRACKS"
                                        }
                                    }
                                    if total_secs > 0 {
                                        span { "· {format_duration(total_secs)}" }
                                    }
                                    if summary.owned_pct > 0 {
                                        span { "· {summary.owned_pct}% IN LIBRARY" }
                                    }
                                }

                                // Primary & secondary actions
                                div { class: "flex flex-wrap items-center gap-2.5 pt-1",
                                    button {
                                        r#type: "button",
                                        disabled: tracks.is_empty(),
                                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_play.is_empty() {
                                                player.play_queue(tracks_for_play.clone(), 0);
                                            }
                                        },
                                        IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                        span { "Play" }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: tracks.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high/90 border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_shuffle.is_empty() {
                                                if !player.snapshot().shuffle {
                                                    player.toggle_shuffle();
                                                }
                                                player.play_queue(tracks_for_shuffle.clone(), 0);
                                            }
                                        },
                                        IconShuffle { size: 14 }
                                        span { "Shuffle" }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: refreshing(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high/90 border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: handle_refresh,
                                        IconRefreshCw {
                                            size: 14,
                                            class: if refreshing() { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                                        }
                                        span { if refreshing() { "Refreshing..." } else { "Refresh Mix" } }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: save_ids.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high/90 border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            studio.open_create_playlist(save_title.clone(), save_ids.clone());
                                        },
                                        IconListPlus { size: 14 }
                                        span { "Save as Playlist" }
                                    }
                                }

                                // Seasonal Culture Steering Pills
                                if is_seasonal {
                                    div {
                                        role: "group",
                                        aria_label: "Culture mix steering",
                                        class: "inline-flex flex-wrap items-center gap-1 p-1 rounded-full bg-beet-sunken/80 border border-beet-border-subtle mt-1",
                                        for (opt, label) in steering_opts {
                                            {
                                                let active = cur_steering == opt;
                                                let cls = if active {
                                                    "bg-beet-accent-surface text-beet-accent-text border-beet-accent/40"
                                                } else {
                                                    "bg-transparent text-beet-text-secondary border-transparent hover:text-beet-text-primary hover:bg-beet-surface-high/70"
                                                };
                                                let steer_slug = summary.slug.clone();
                                                rsx! {
                                                    button {
                                                        key: "{opt.as_str()}",
                                                        r#type: "button",
                                                        disabled: steering_busy(),
                                                        aria_pressed: if active { "true" } else { "false" },
                                                        class: "h-7 px-3 rounded-full border text-[12px] font-medium transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none {cls}",
                                                        onclick: move |_| {
                                                            if steering_busy() {
                                                                return;
                                                            }
                                                            let next_req = req_id.peek().wrapping_add(1);
                                                            req_id.set(next_req);
                                                            steering_busy.set(true);
                                                            refreshing.set(false);
                                                            let target = steer_slug.clone();
                                                            spawn(async move {
                                                                match auth
                                                                    .call(api::steer_seasonal_pack(SteerSeasonalPackRequest {
                                                                        slug: target.clone(),
                                                                        steering: opt,
                                                                    }))
                                                                    .await
                                                                {
                                                                    Ok(updated) => {
                                                                        {
                                                                            let mut mixes = studio.contextual_mixes.write();
                                                                            if let Some(slot) = mixes.iter_mut().find(|m| m.slug == target) {
                                                                                *slot = updated;
                                                                            }
                                                                        }
                                                                        if *req_id.peek() == next_req {
                                                                            if let Ok(fresh) = auth
                                                                                .call(api::get_contextual_mix_playlist(target, Some(false)))
                                                                                .await
                                                                            {
                                                                                if *req_id.peek() == next_req {
                                                                                    playlist.set(Some(fresh));
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                    Err(err) => {
                                                                        if *req_id.peek() == next_req {
                                                                            studio.show_toast(format!("Steering failed: {}", friendly_error(&err)));
                                                                        }
                                                                    }
                                                                }
                                                                if *req_id.peek() == next_req {
                                                                    steering_busy.set(false);
                                                                }
                                                            });
                                                        },
                                                        "{label}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Tracklist
                        if tracks.is_empty() {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No ready tracks in this mix yet. Try Refresh Mix or generate Discovery tracks."
                            }
                        } else {
                            div { class: "space-y-1", role: "list", aria_label: "Mix tracks",
                                for (idx, track) in tracks.iter().enumerate() {
                                    {
                                        let all = tracks.clone();
                                        let t_next = track.clone();
                                        let t_queue = track.clone();
                                        let t_pl = track.clone();
                                        let t_star = track.clone();
                                        let t_keep = track.clone();
                                        let t_drop = track.clone();
                                        let t_rate = track.clone();
                                        let t_inspect = track.clone();
                                        let row_state = if offline_only && !track.is_offline_cached {
                                            TrackRowState::OfflineUnavailable
                                        } else if track.is_discovery {
                                            TrackRowState::PendingDiscovery {
                                                days_remaining: track.days_remaining,
                                                reason_label: track.reason_label.clone(),
                                                rating: track.rating,
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
                                                key: "mix-trk-{idx}-{track.id}",
                                                track_number: Some((idx + 1) as u32),
                                                title: track.title.clone(),
                                                artist: track.artist.clone(),
                                                album: track.album.clone(),
                                                cover_art_url: track.cover_art_url.clone(),
                                                duration_secs: track.duration_secs,
                                                codec_badge: track.codec_badge.clone(),
                                                is_playing: !track.id.is_empty() && player.is_track_playing(&track.id),
                                                state: row_state,
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
                                                    studio.open_add_to_playlist(
                                                        t_pl.title.clone(),
                                                        vec![t_pl.id.clone()],
                                                    );
                                                },
                                                on_artist_click: move |artist| studio.navigate_to_artist(nav, artist),
                                                on_album_click: move |album| studio.navigate_to_album(nav, album),
                                                on_keep: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                                on_drop: move |_| studio.drop_track(auth, player, t_drop.clone()),
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
    }
}
