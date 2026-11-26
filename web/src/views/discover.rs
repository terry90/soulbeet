//! Discover 2.0 view: Active Seasonal Hero with live steering, interactive Fast Triage Deck
//! (Card Stack & List modes with 5-second Undo), Contextual & Rolling Mixes, Seed Radio starter,
//! Taste DNA editor (`UserCultureProfile`), and Discovery playlist generation controls.

use dioxus::prelude::*;
use shared::recommendation::{
    ContextualMixKind, CultureSteering, SeedRadioRequest, UpdateCultureProfileRequest,
};
use ui::{
    discovery::DiscoveryOverview, format_discovery_badge, format_mix_subtitle, format_mix_title,
    format_mix_window_badge, friendly_error, subsonic_cover_art_url, use_player, IconCheck,
    IconLoader, IconMusic, IconPlay, IconRadio, IconSparkles, IconStar, IconThumbsDown,
    PlayerTrack, SeasonalHeroCard, TasteDnaBar, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::use_studio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriageLayout {
    Deck,
    List,
}

#[component]
pub fn DiscoverPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();

    let mut triage_layout = use_signal(|| TriageLayout::Deck);
    let mut editing_culture = use_signal(|| false);
    let mut country_input = use_signal(String::new);
    let mut langs_input = use_signal(String::new);
    let mut ratio_pct = use_signal(|| 45u8);
    let mut saving_culture = use_signal(|| false);
    let mut show_engine_controls = use_signal(|| false);

    // Seed Radio form
    let mut radio_artist = use_signal(String::new);
    let mut radio_loading = use_signal(|| false);

    let mixes = studio.contextual_mixes.read().clone();
    let seasonal = mixes
        .iter()
        .find(|m| m.kind == ContextualMixKind::Seasonal)
        .cloned();
    let other_mixes: Vec<_> = mixes
        .iter()
        .filter(|m| m.kind != ContextualMixKind::Seasonal)
        .cloned()
        .collect();

    let culture = studio.culture_profile.read().clone();
    let local_code = culture
        .effective_country()
        .unwrap_or("LOCAL")
        .to_ascii_uppercase();

    let visible_triage = studio.visible_triage_tracks();

    let open_culture_editor = {
        let culture = culture.clone();
        move |_| {
            country_input.set(culture.home_country.clone().unwrap_or_default());
            langs_input.set(culture.preferred_languages.join(", "));
            ratio_pct.set((culture.local_culture_ratio.clamp(0.0, 1.0) * 100.0).round() as u8);
            editing_culture.set(true);
        }
    };

    let save_culture = move |_| {
        saving_culture.set(true);
        let country = country_input().trim().to_ascii_uppercase();
        let languages: Vec<String> = langs_input()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let ratio = f32::from(ratio_pct()) / 100.0;
        let req = UpdateCultureProfileRequest {
            home_country: (!country.is_empty()).then_some(country),
            preferred_languages: Some(languages),
            local_culture_ratio: Some(ratio),
        };
        spawn(async move {
            match auth.call(api::update_culture_profile(req)).await {
                Ok(updated) => {
                    studio.culture_profile.set(updated);
                    editing_culture.set(false);
                    studio.show_toast("Taste DNA updated");
                    let local_hour = Some(
                        chrono::Local::now()
                            .format("%H")
                            .to_string()
                            .parse::<u8>()
                            .unwrap_or(12),
                    );
                    if let Ok(refreshed) = auth
                        .call(api::list_active_contextual_mixes(local_hour))
                        .await
                    {
                        studio.contextual_mixes.set(refreshed);
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Could not save Taste DNA: {}", friendly_error(&e)));
                }
            }
            saving_culture.set(false);
        });
    };

    let launch_seed_radio = move |evt: FormEvent| {
        evt.prevent_default();
        let artist = radio_artist().trim().to_string();
        if artist.is_empty() || radio_loading() {
            return;
        }
        radio_loading.set(true);
        let req = SeedRadioRequest {
            seed_song_id: None,
            seed_artist: Some(artist.clone()),
            seed_track: None,
            seed_artist_mbid: None,
            limit: Some(25),
        };
        spawn(async move {
            match auth.call(api::start_seed_radio(req)).await {
                Ok(res) => {
                    let nav = studio.navidrome_info();
                    let tracks: Vec<PlayerTrack> = res
                        .owned_tracks
                        .into_iter()
                        .map(|t| {
                            let cover = nav
                                .as_ref()
                                .map(|i| subsonic_cover_art_url(i, &t.song_id, 256));
                            PlayerTrack {
                                id: t.song_id.clone(),
                                title: t.title,
                                artist: t.artist,
                                album: t.album,
                                cover_art_url: cover,
                                cover_art_id: Some(t.song_id),
                                duration_secs: t.duration,
                                reason_label: Some(ui::format_candidate_reason(&t.reason)),
                                ..Default::default()
                            }
                        })
                        .collect();
                    if tracks.is_empty() {
                        studio.show_toast(format!("No owned tracks matched radio seed “{artist}”"));
                    } else {
                        let count = tracks.len();
                        player.play_queue(tracks, 0);
                        studio.show_toast(format!("Playing {} radio ({count} tracks)", res.seed_label));
                        radio_artist.set(String::new());
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Seed radio failed: {}", friendly_error(&e)));
                }
            }
            radio_loading.set(false);
        });
    };

    rsx! {
        div { class: "space-y-8",
            // 1. Seasonal Hero Card
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

            // 2. Fast Discovery Triage Deck (Card Stack & List views)
            section { class: "space-y-4",
                div { class: "flex flex-wrap items-center justify-between gap-3",
                    div { class: "flex items-center gap-2.5",
                        h2 { class: "text-[18px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "Fast Triage Deck"
                        }
                        span { class: "px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold",
                            "{visible_triage.len()} PENDING"
                        }
                    }

                    div { class: "inline-flex items-center gap-1 p-1 rounded-full bg-beet-surface-low border border-beet-border-subtle",
                        button {
                            r#type: "button",
                            class: if triage_layout() == TriageLayout::Deck {
                                "h-7 px-3 rounded-full bg-beet-surface-high text-beet-text-primary text-[12px] font-medium cursor-pointer"
                            } else {
                                "h-7 px-3 rounded-full text-beet-text-tertiary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer"
                            },
                            onclick: move |_| triage_layout.set(TriageLayout::Deck),
                            "Card Deck"
                        }
                        button {
                            r#type: "button",
                            class: if triage_layout() == TriageLayout::List {
                                "h-7 px-3 rounded-full bg-beet-surface-high text-beet-text-primary text-[12px] font-medium cursor-pointer"
                            } else {
                                "h-7 px-3 rounded-full text-beet-text-tertiary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer"
                            },
                            onclick: move |_| triage_layout.set(TriageLayout::List),
                            "List"
                        }
                    }
                }

                if visible_triage.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-2",
                        IconSparkles { size: 22, class: "text-beet-text-disabled mx-auto".to_string() }
                        p { class: "text-[14px] font-medium text-beet-text-primary",
                            "Triage queue is clear"
                        }
                        p { class: "text-[13px] text-beet-text-secondary",
                            "All staged discovery tracks have been reviewed. Generate a fresh batch below whenever you're ready."
                        }
                    }
                } else if triage_layout() == TriageLayout::Deck {
                    {
                        let first_dt = visible_triage[0].clone();
                        let track = studio.track_from_discovery(&first_dt);
                        let is_playing = !track.id.is_empty() && player.is_track_playing(&track.id);
                        let t_play = track.clone();
                        let t_keep = track.clone();
                        let t_star = track.clone();
                        let t_drop = track.clone();
                        let t_inspect = track.clone();
                        rsx! {
                            div { class: "rounded-[16px] bg-beet-surface border border-beet-border-default p-5 sm:p-6 flex flex-col md:flex-row items-center gap-6",
                                // Artwork + Play Trigger
                                button {
                                    r#type: "button",
                                    class: "relative w-36 h-36 sm:w-44 sm:h-44 rounded-[12px] overflow-hidden bg-beet-surface-low border border-beet-border-subtle shrink-0 flex items-center justify-center group cursor-pointer",
                                    onclick: move |_| player.play_track(t_play.clone()),
                                    if let Some(url) = track.cover_art_url.as_deref() {
                                        img { src: "{url}", alt: "", class: "w-full h-full object-cover" }
                                    } else {
                                        IconMusic { size: 36, class: "text-beet-text-disabled".to_string() }
                                    }
                                    span { class: "absolute inset-0 bg-beet-canvas/55 flex items-center justify-center opacity-90 group-hover:opacity-100 transition-opacity",
                                        span { class: "w-12 h-12 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center shadow-lg",
                                            IconPlay { size: 20, filled: true, class: "ml-0.5".to_string() }
                                        }
                                    }
                                }

                                // Track Details & 1-Tap Verdict Row
                                div { class: "flex-1 min-w-0 space-y-4 text-center md:text-left",
                                    div { class: "flex flex-wrap items-center justify-center md:justify-start gap-2",
                                        span { class: "inline-flex items-center gap-1 px-2.5 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold",
                                            IconSparkles { size: 12 }
                                            "{format_discovery_badge(track.days_remaining)}"
                                        }
                                        if let Some(ref reason) = track.reason_label {
                                            span { class: "px-2.5 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-subtle text-beet-text-secondary text-[12px]",
                                                "{reason}"
                                            }
                                        }
                                        if is_playing {
                                            span { class: "tabular-nums-mono text-[11px] font-semibold text-beet-leaf-text",
                                                "NOW PLAYING"
                                            }
                                        }
                                    }

                                    div { class: "space-y-1",
                                        h3 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight",
                                            "{track.title}"
                                        }
                                        p { class: "text-[15px] text-beet-text-secondary",
                                            "{track.artist}"
                                            if let Some(ref alb) = track.album {
                                                span { class: "text-beet-text-tertiary", " · {alb}" }
                                            }
                                        }
                                    }

                                    // 1-Tap Verdict Controls (with >= 8px gap between Drop and Keep!)
                                    div { class: "flex flex-wrap items-center justify-center md:justify-start gap-3 pt-1",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-2 h-10 px-4 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white text-[13px] font-semibold cursor-pointer transition-colors",
                                            onclick: move |_| studio.drop_track(auth, player, t_drop.clone()),
                                            IconThumbsDown { size: 15 }
                                            span { "Drop (5s Undo)" }
                                        }
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-2 h-10 px-4 rounded-full bg-beet-gold-surface border border-beet-gold/40 text-beet-gold hover:brightness-110 text-[13px] font-semibold cursor-pointer transition-[filter]",
                                            onclick: move |_| studio.star_track(auth, player, t_star.clone(), true),
                                            IconStar { size: 15, filled: true }
                                            span { "Star & Keep" }
                                        }
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 text-[14px] font-semibold cursor-pointer transition-[filter]",
                                            onclick: move |_| studio.keep_track(auth, player, t_keep.clone()),
                                            IconCheck { size: 16 }
                                            span { "Keep in Library" }
                                        }
                                        button {
                                            r#type: "button",
                                            class: "h-10 px-3 rounded-full text-[12px] text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer",
                                            onclick: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                            "Inspect"
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    div { class: "space-y-1", role: "list", aria_label: "Pending discovery triage list",
                        for dt in visible_triage.iter() {
                            {
                                let track = studio.track_from_discovery(dt);
                                let is_playing = !track.id.is_empty() && player.is_track_playing(&track.id);
                                let t_play = track.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_inspect = track.clone();
                                rsx! {
                                    TrackRow {
                                        key: "disc-triage-{dt.id}",
                                        title: track.title.clone(),
                                        artist: track.artist.clone(),
                                        album: track.album.clone(),
                                        cover_art_url: track.cover_art_url.clone(),
                                        is_playing,
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
            }

            // 3. Taste DNA + Seed Radio (2-column desktop studio grid)
            div { class: "grid grid-cols-1 lg:grid-cols-2 gap-4",
                // Taste DNA Card + Interactive Culture Profile Editor
                div { class: "space-y-3",
                    TasteDnaBar {
                        profile: culture.clone(),
                        on_edit: open_culture_editor,
                    }

                    if editing_culture() {
                        div { class: "rounded-[12px] bg-beet-surface-high border border-beet-border-default p-4 space-y-4",
                            div { class: "flex items-center justify-between",
                                h3 { class: "text-[14px] font-semibold text-beet-text-primary",
                                    "Tune Cultural Profile"
                                }
                                button {
                                    r#type: "button",
                                    class: "text-[12px] text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer",
                                    onclick: move |_| editing_culture.set(false),
                                    "Cancel"
                                }
                            }

                            div { class: "grid grid-cols-1 sm:grid-cols-2 gap-3",
                                label { class: "space-y-1 block",
                                    span { class: "text-[11px] tabular-nums-mono uppercase tracking-wider text-beet-text-tertiary",
                                        "Home Country (ISO-2)"
                                    }
                                    input {
                                        r#type: "text",
                                        maxlength: "2",
                                        placeholder: "FR, US, JP…",
                                        value: "{country_input}",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-sunken border border-beet-border-default text-[13px] text-beet-text-primary uppercase outline-none",
                                        oninput: move |evt| country_input.set(evt.value()),
                                    }
                                }
                                label { class: "space-y-1 block",
                                    span { class: "text-[11px] tabular-nums-mono uppercase tracking-wider text-beet-text-tertiary",
                                        "Languages (comma-separated)"
                                    }
                                    input {
                                        r#type: "text",
                                        placeholder: "fr, en",
                                        value: "{langs_input}",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-sunken border border-beet-border-default text-[13px] text-beet-text-primary outline-none",
                                        oninput: move |evt| langs_input.set(evt.value()),
                                    }
                                }
                            }

                            div { class: "space-y-1.5",
                                div { class: "flex items-center justify-between text-[12px]",
                                    span { class: "text-beet-text-secondary", "Local Culture Share" }
                                    span { class: "tabular-nums-mono font-semibold text-beet-accent-text",
                                        "{ratio_pct}% LOCAL · {100u8.saturating_sub(ratio_pct())}% INTL"
                                    }
                                }
                                input {
                                    r#type: "range",
                                    min: "0",
                                    max: "100",
                                    value: "{ratio_pct}",
                                    class: "w-full accent-beet-accent-solid cursor-pointer",
                                    oninput: move |evt| {
                                        if let Ok(v) = evt.value().parse::<u8>() {
                                            ratio_pct.set(v.min(100));
                                        }
                                    },
                                }
                            }

                            div { class: "flex justify-end",
                                button {
                                    r#type: "button",
                                    disabled: saving_culture(),
                                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-50 cursor-pointer",
                                    onclick: save_culture,
                                    if saving_culture() {
                                        IconLoader { size: 14 }
                                        span { "Saving…" }
                                    } else {
                                        span { "Save Taste DNA" }
                                    }
                                }
                            }
                        }
                    }
                }

                // On-Demand Seed Radio Starter
                section { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-4",
                    div { class: "space-y-1.5",
                        div { class: "flex items-center gap-2 text-beet-accent-text",
                            IconRadio { size: 17 }
                            h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                                "Instant Seed Radio"
                            }
                        }
                        p { class: "text-[13px] text-beet-text-secondary",
                            "Enter any artist to launch an instant radio station blending owned library matches with similar-artist neighbors."
                        }
                    }

                    form {
                        class: "flex items-center gap-2",
                        onsubmit: launch_seed_radio,
                        input {
                            r#type: "text",
                            value: "{radio_artist}",
                            placeholder: "Artist name (e.g. Daft Punk, Khruangbin…)",
                            class: "flex-1 h-10 px-3.5 rounded-full bg-beet-sunken border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-tertiary outline-none",
                            oninput: move |evt| radio_artist.set(evt.value()),
                        }
                        button {
                            r#type: "submit",
                            disabled: radio_loading() || radio_artist().trim().is_empty(),
                            class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-45 cursor-pointer shrink-0",
                            if radio_loading() {
                                IconLoader { size: 14 }
                                span { "Starting…" }
                            } else {
                                IconPlay { size: 14, filled: true }
                                span { "Start Radio" }
                            }
                        }
                    }
                }
            }

            // 4. Contextual & Rolling Mixes Shelf
            if !other_mixes.is_empty() {
                section { class: "space-y-3",
                    h2 { class: "text-[18px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                        "Moment & Rolling Mixes"
                    }
                    div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3.5",
                        for mix in other_mixes.iter() {
                            {
                                let slug = mix.slug.clone();
                                let badge = format_mix_window_badge(mix)
                                    .unwrap_or_else(|| "MIX".to_string());
                                rsx! {
                                    button {
                                        key: "disc-mix-{mix.slug}",
                                        r#type: "button",
                                        class: "group rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle p-4 text-left transition-colors cursor-pointer flex flex-col justify-between gap-3",
                                        onclick: move |_| studio.play_contextual_mix(auth, player, slug.clone()),
                                        div { class: "space-y-1.5",
                                            div { class: "flex items-center justify-between gap-2",
                                                span { class: "px-2 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[10px] font-semibold text-beet-text-tertiary",
                                                    "{badge}"
                                                }
                                                span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center",
                                                    IconPlay { size: 13, filled: true }
                                                }
                                            }
                                            h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                                                "{format_mix_title(mix)}"
                                            }
                                            p { class: "text-[13px] text-beet-text-secondary line-clamp-2",
                                                "{format_mix_subtitle(mix)}"
                                            }
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                            "{mix.track_count} TRACKS"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 5. Classic Discovery Engine Generation Controls (collapsible to keep Discover calm)
            section { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-4 space-y-3",
                div { class: "flex items-center justify-between gap-3",
                    div {
                        h2 { class: "text-[15px] font-semibold text-beet-text-primary",
                            "Discovery Playlist Engine"
                        }
                        p { class: "text-[12px] text-beet-text-tertiary",
                            "Trigger full Last.fm + ListenBrainz playlist generation and inspect run telemetry."
                        }
                    }
                    button {
                        r#type: "button",
                        class: "h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer",
                        onclick: move |_| show_engine_controls.toggle(),
                        if show_engine_controls() { "Hide Engine" } else { "Show Engine" }
                    }
                }
                if show_engine_controls() {
                    div { class: "pt-2 border-t border-beet-border-subtle",
                        DiscoveryOverview {}
                    }
                }
            }
        }
    }
}
