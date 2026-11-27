//! Discover 2.0 view: Clean Discover header with on-demand refresh & progress,
//! Active Seasonal Hero with live steering, interactive Fast Triage Deck
//! (Card Stack & List modes with 5-second Undo), Contextual & Rolling Mixes,
//! Seed Radio starter, and Taste DNA editor (`UserCultureProfile`).

use dioxus::prelude::*;
use shared::navidrome::{DiscoveryProgress, GenerationStatus, ProfilePhase};
use shared::recommendation::{
    ContextualMixKind, CultureSteering, SeedRadioRequest, UpdateCultureProfileRequest,
};
use shared::system::NavidromeStatus;
use ui::{
    format_discovery_badge, format_mix_subtitle, format_mix_title, format_mix_window_badge,
    friendly_error, subsonic_cover_art_url, use_player, IconCheck, IconHeart, IconLoader,
    IconMusic, IconPause, IconPlay, IconRadio, IconRefreshCw, IconSettings, IconSparkles,
    IconThumbsDown, PlayerTrack, SeasonalHeroCard, StarStrip, StarStripSize, TasteDnaBar,
    TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::{local_browser_hour, local_browser_weekday, use_studio};
use crate::Route;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriageLayout {
    Deck,
    List,
}

fn format_relative_time(ts: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
            .map(|naive| naive.and_utc().fixed_offset())
    }) else {
        return ts.to_string();
    };
    let delta = chrono::Utc::now().signed_duration_since(parsed);
    let mins = delta.num_minutes();
    if mins < 1 {
        return "just now".to_string();
    }
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = delta.num_days();
    format!("{days}d ago")
}

fn discovery_progress_summary(progress: &DiscoveryProgress) -> (String, u32) {
    if progress.profiles.is_empty() {
        return ("Preparing recommendations...".to_string(), 8);
    }
    let total_profiles = progress.profiles.len() as f64;
    let mut completed_units = 0.0f64;
    let mut active_label: Option<String> = None;

    for pp in &progress.profiles {
        match pp.phase {
            ProfilePhase::Done | ProfilePhase::Skipped => {
                completed_units += 1.0;
            }
            ProfilePhase::Waiting => {}
            _ => {
                let frac = if pp.total > 0 {
                    (pp.current as f64 / pp.total as f64).clamp(0.0, 1.0)
                } else {
                    0.25
                };
                completed_units += frac;
                if active_label.is_none() {
                    if pp.total > 0 {
                        active_label =
                            Some(format!("{}: {} ({}/{})", pp.profile, pp.phase, pp.current, pp.total));
                    } else {
                        active_label = Some(format!("{}: {}", pp.profile, pp.phase));
                    }
                }
            }
        }
    }

    let pct = ((completed_units / total_profiles) * 100.0)
        .round()
        .clamp(6.0, 98.0) as u32;
    let label = active_label.unwrap_or_else(|| "Generating fresh discovery tracks...".to_string());
    (label, pct)
}

/// Returns true when `progress` is a terminal snapshot belonging to a newly triggered run
/// (never a stale `Complete`/`Error` left over from before the user clicked Refresh Discovery).
fn is_fresh_terminal_progress(
    progress: &DiscoveryProgress,
    observed_running: bool,
    awaiting_new_run: bool,
    pre_click_started_at: Option<&str>,
) -> bool {
    if !progress.is_terminal() {
        return false;
    }
    observed_running || (awaiting_new_run && progress.started_at.as_deref() != pre_click_started_at)
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
    let mut weather_city_input = use_signal(String::new);
    let mut ratio_pct = use_signal(|| 45u8);
    let mut saving_culture = use_signal(|| false);

    // Discover header refresh + progress polling
    let mut discovery_cfg =
        use_resource(move || async move { auth.call(api::get_discovery_config()).await });
    let mut poll_tick = use_signal(|| 0u32);
    let progress_res = use_resource(move || {
        let _ = poll_tick();
        async move { auth.call(api::get_discovery_progress()).await.ok().flatten() }
    });
    let mut starting_refresh = use_signal(|| false);
    let mut awaiting_new_run = use_signal(|| false);
    let mut pre_click_started_at = use_signal(|| None::<String>);
    let mut observed_running = use_signal(|| false);
    let mut refresh_error = use_signal(|| None::<String>);

    // Poll every 2.5s while a generation is active (also catches an already-running job on mount)
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(2500).await;
            let server_running = progress_res
                .peek()
                .as_ref()
                .and_then(|p| p.as_ref())
                .is_some_and(|p| p.status == GenerationStatus::Running);
            if server_running || *starting_refresh.peek() || *awaiting_new_run.peek() {
                poll_tick += 1;
            }
        }
    });

    // React to server progress transitions (Running -> Complete / Error), ignoring any stale
    // terminal snapshot cached from a prior run before Refresh Discovery was clicked.
    use_effect(move || {
        let current = progress_res.read().as_ref().and_then(|p| p.clone());
        let fresh_terminal = current.as_ref().is_some_and(|p| {
            is_fresh_terminal_progress(
                p,
                *observed_running.peek(),
                *awaiting_new_run.peek(),
                pre_click_started_at.peek().as_deref(),
            )
        });
        match current.as_ref().map(|p| &p.status) {
            Some(GenerationStatus::Running) => {
                if !*observed_running.peek() {
                    observed_running.set(true);
                }
                if *awaiting_new_run.peek() {
                    awaiting_new_run.set(false);
                }
                if *starting_refresh.peek() {
                    starting_refresh.set(false);
                }
            }
            Some(GenerationStatus::Complete) if fresh_terminal => {
                observed_running.set(false);
                awaiting_new_run.set(false);
                starting_refresh.set(false);
                let total_new = current
                    .as_ref()
                    .and_then(|p| p.result.as_ref())
                    .map(|r| r.total_imported)
                    .unwrap_or(0);
                discovery_cfg.restart();
                spawn(async move {
                    let local_hour = local_browser_hour();
                    let local_weekday = local_browser_weekday();
                    let (triage_res, mixes_res) = futures::future::join(
                        auth.call(api::list_triage_queue()),
                        auth.call(api::list_active_contextual_mixes(local_hour, local_weekday)),
                    )
                    .await;
                    if let Ok(triage) = triage_res {
                        studio.triage_tracks.set(triage);
                    }
                    if let Ok(refreshed_mixes) = mixes_res {
                        studio.contextual_mixes.set(refreshed_mixes);
                        studio.reclassify_playlists();
                    }
                    let noun = if total_new == 1 { "track" } else { "tracks" };
                    studio.show_toast(format!("Discovery refreshed: {total_new} new {noun}"));
                });
            }
            Some(GenerationStatus::Error) if fresh_terminal => {
                observed_running.set(false);
                awaiting_new_run.set(false);
                starting_refresh.set(false);
                let msg = current
                    .as_ref()
                    .and_then(|p| p.failure.as_ref())
                    .map(|f| f.message.clone())
                    .unwrap_or_else(|| "Discovery generation failed".to_string());
                refresh_error.set(Some(msg));
            }
            _ => {}
        }
    });

    // Seed Radio form
    let mut radio_artist = use_signal(String::new);
    let mut radio_loading = use_signal(|| false);

    let cfg_info = match &*discovery_cfg.read() {
        Some(Ok(c)) => Some(c.clone()),
        _ => None,
    };
    let current_progress = progress_res.read().as_ref().and_then(|p| p.clone());
    let is_generating = starting_refresh()
        || awaiting_new_run()
        || current_progress
            .as_ref()
            .is_some_and(|p| p.status == GenerationStatus::Running);
    let nav_status = auth.navidrome_status();
    let nav_unlinked = matches!(
        nav_status,
        NavidromeStatus::NotLinked | NavidromeStatus::InvalidCredentials | NavidromeStatus::Unknown
    );
    let discovery_disabled = cfg_info.as_ref().is_some_and(|c| !c.enabled);
    let last_updated_label = cfg_info
        .as_ref()
        .and_then(|c| c.last_generated_at.as_deref())
        .map(|ts| format!("Updated {}", format_relative_time(ts)));

    let handle_refresh_discovery = move |_| {
        if is_generating || discovery_disabled || nav_unlinked {
            return;
        }
        let prev_started = progress_res
            .peek()
            .as_ref()
            .and_then(|p| p.as_ref())
            .and_then(|p| p.started_at.clone());
        pre_click_started_at.set(prev_started);
        refresh_error.set(None);
        starting_refresh.set(true);
        awaiting_new_run.set(true);
        spawn(async move {
            match auth.call(api::start_discovery_generation()).await {
                Ok(()) => {
                    poll_tick += 1;
                }
                Err(e) => {
                    starting_refresh.set(false);
                    awaiting_new_run.set(false);
                    refresh_error.set(Some(friendly_error(&e)));
                    poll_tick += 1;
                }
            }
        });
    };

    let mixes = studio.contextual_mixes.read().clone();
    let seasonal = mixes
        .iter()
        .find(|m| m.kind == ContextualMixKind::Seasonal && m.track_count > 0)
        .cloned();
    let hero_slug = seasonal.as_ref().map(|h| h.slug.as_str());
    let other_mixes: Vec<_> = mixes
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

    let open_culture_editor = {
        let culture = culture.clone();
        move |_| {
            country_input.set(culture.home_country.clone().unwrap_or_default());
            langs_input.set(culture.preferred_languages.join(", "));
            weather_city_input.set(culture.weather_city.clone().unwrap_or_default());
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
        let weather_city_trimmed = weather_city_input().trim().to_string();
        let existing_city = studio
            .culture_profile
            .peek()
            .weather_city
            .clone()
            .unwrap_or_default();
        let weather_city = if weather_city_trimmed.eq_ignore_ascii_case(existing_city.trim()) {
            None
        } else {
            Some(weather_city_trimmed)
        };
        let req = UpdateCultureProfileRequest {
            home_country: (!country.is_empty()).then_some(country),
            preferred_languages: Some(languages),
            local_culture_ratio: Some(ratio),
            weather_city,
        };
        spawn(async move {
            match auth.call(api::update_culture_profile(req)).await {
                Ok(updated) => {
                    studio.culture_profile.set(updated);
                    editing_culture.set(false);
                    studio.show_toast("Taste DNA updated");
                    let local_hour = local_browser_hour();
                    let local_weekday = local_browser_weekday();
                    if let Ok(refreshed) = auth
                        .call(api::list_active_contextual_mixes(local_hour, local_weekday))
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
                            let mut pt = PlayerTrack {
                                id: t.song_id.clone(),
                                title: t.title,
                                artist: t.artist,
                                album: t.album,
                                cover_art_url: cover,
                                cover_art_id: Some(t.song_id),
                                duration_secs: t.duration,
                                starred: t.starred,
                                rating: t.rating.unwrap_or(0).min(5),
                                reason_label: Some(ui::format_candidate_reason(&t.reason)),
                                ..Default::default()
                            };
                            studio.apply_curation_overrides(&mut pt);
                            pt
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
            // 0. Discover Header (Refresh Discovery + Last Updated + Settings + Inline Progress)
            header { class: "space-y-3",
                div { class: "flex flex-wrap items-center justify-between gap-3",
                    div { class: "flex flex-wrap items-baseline gap-2.5",
                        h1 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary",
                            "Discover"
                        }
                        if let Some(ref updated) = last_updated_label {
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                "{updated}"
                            }
                        }
                    }

                    div { class: "flex items-center gap-2",
                        button {
                            r#type: "button",
                            disabled: is_generating || discovery_disabled || nav_unlinked,
                            class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed cursor-pointer transition-[filter]",
                            onclick: handle_refresh_discovery,
                            IconRefreshCw {
                                size: 14,
                                class: if is_generating { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                            }
                            span {
                                if is_generating { "Refreshing..." } else { "Refresh Discovery" }
                            }
                        }
                        Link {
                            to: Route::SettingsPage {},
                            class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium transition-colors",
                            IconSettings { size: 14 }
                            span { "Settings" }
                        }
                    }
                }

                // Inline setup prompt when Navidrome is not linked or Discovery is disabled
                if nav_unlinked {
                    div { class: "flex flex-wrap items-center justify-between gap-2 rounded-[10px] bg-beet-accent-surface border border-beet-accent/30 px-3.5 py-2.5 text-[12px] text-beet-accent-text",
                        span { "Connect your Navidrome account to refresh Discovery playlists and sync your library." }
                        Link {
                            to: Route::SettingsPage {},
                            class: "font-semibold underline underline-offset-2 hover:brightness-110 shrink-0",
                            "Open Settings"
                        }
                    }
                } else if discovery_disabled {
                    div { class: "flex flex-wrap items-center justify-between gap-2 rounded-[10px] bg-beet-surface border border-beet-border-subtle px-3.5 py-2.5 text-[12px] text-beet-text-secondary",
                        span { "Discovery generation is currently turned off in your library preferences." }
                        Link {
                            to: Route::SettingsPage {},
                            class: "font-semibold text-beet-leaf-text hover:underline shrink-0",
                            "Enable in Settings"
                        }
                    }
                }

                // Compact inline progress bar while discovery generation is running
                if is_generating {
                    {
                        let (status_label, pct) = current_progress
                            .as_ref()
                            .filter(|p| p.status == GenerationStatus::Running)
                            .map(discovery_progress_summary)
                            .unwrap_or_else(|| ("Starting discovery refresh...".to_string(), 8));
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

                if let Some(ref err) = refresh_error() {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3.5 py-2 text-[12px] text-beet-crimson-text",
                        "{err}"
                    }
                }
            }

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
                            "All staged discovery tracks have been reviewed. Click Refresh Discovery above whenever you are ready for a fresh batch."
                        }
                    }
                } else if triage_layout() == TriageLayout::Deck {
                    {
                        let active_track_id = player.current_track().map(|t| t.id);
                        let active_dt = active_track_id
                            .as_deref()
                            .filter(|id| !id.is_empty())
                            .and_then(|id| {
                                visible_triage
                                    .iter()
                                    .find(|dt| dt.song_id.as_deref() == Some(id))
                            })
                            .unwrap_or(&visible_triage[0])
                            .clone();
                        let track = studio.track_from_discovery(&active_dt);
                        let is_playing = !track.id.is_empty() && player.is_track_playing(&track.id);
                        let triage_for_play = visible_triage.clone();
                        let t_play = track.clone();
                        let t_keep = track.clone();
                        let t_star = track.clone();
                        let t_drop = track.clone();
                        let t_rate = track.clone();
                        let t_inspect = track.clone();
                        rsx! {
                            div { class: "rounded-[16px] bg-beet-surface border border-beet-border-default p-5 sm:p-6 flex flex-col md:flex-row items-center gap-6",
                                // Artwork + Play/Pause Trigger (enqueues full pending triage list)
                                button {
                                    r#type: "button",
                                    aria_label: if is_playing { "Pause triage track" } else { "Play triage queue" },
                                    class: "relative w-36 h-36 sm:w-44 sm:h-44 rounded-[12px] overflow-hidden bg-beet-surface-low border border-beet-border-subtle hover:border-beet-border-default shrink-0 flex items-center justify-center group cursor-pointer transition-colors",
                                    onclick: move |_| {
                                        studio.play_or_toggle_triage(
                                            player,
                                            &triage_for_play,
                                            &t_play,
                                            true,
                                        )
                                    },
                                    if let Some(url) = track.cover_art_url.as_deref() {
                                        img { src: "{url}", alt: "", class: "w-full h-full object-cover" }
                                    } else {
                                        IconMusic { size: 36, class: "text-beet-text-disabled".to_string() }
                                    }
                                    span { class: "absolute inset-0 bg-beet-canvas/55 flex items-center justify-center opacity-90 group-hover:opacity-100 transition-opacity",
                                        span { class: "w-12 h-12 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center shadow-lg group-hover:scale-105 transition-transform",
                                            if is_playing {
                                                IconPause { size: 20 }
                                            } else {
                                                IconPlay { size: 20, filled: true, class: "ml-0.5".to_string() }
                                            }
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

                                    div { class: "flex flex-wrap items-center justify-center md:justify-start gap-2.5",
                                        span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                            "Rate"
                                        }
                                        StarStrip {
                                            rating: track.rating,
                                            size: StarStripSize::Compact,
                                            on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                            aria_label: format!("Rate {}", track.title),
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
                                            class: "inline-flex items-center gap-2 h-10 px-4 rounded-full bg-beet-accent-surface border border-beet-accent/40 text-beet-accent-text hover:brightness-110 text-[13px] font-semibold cursor-pointer transition-[filter]",
                                            onclick: move |_| studio.star_track(auth, player, t_star.clone(), true),
                                            IconHeart { size: 15, filled: true }
                                            span { "Favorite & Keep" }
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
                                let triage_for_row = visible_triage.clone();
                                let t_play = track.clone();
                                let t_keep = track.clone();
                                let t_drop = track.clone();
                                let t_rate = track.clone();
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

                            label { class: "space-y-1 block",
                                span { class: "text-[11px] tabular-nums-mono uppercase tracking-wider text-beet-text-tertiary",
                                    "Weather City (optional)"
                                }
                                input {
                                    r#type: "text",
                                    maxlength: "100",
                                    placeholder: "Auto (by IP) or city, e.g. Lyon",
                                    value: "{weather_city_input}",
                                    class: "w-full h-9 px-3 rounded-[8px] bg-beet-sunken border border-beet-border-default text-[13px] text-beet-text-primary outline-none",
                                    oninput: move |evt| weather_city_input.set(evt.value()),
                                }
                                p { class: "text-[11px] text-beet-text-tertiary",
                                    "Used for Rainy / Snow / Storm / Sunny mixes. Leave blank for automatic IP weather."
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
                        "Curated Mixes"
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
                                        class: "group rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default p-4 text-left transition-colors cursor-pointer flex flex-col justify-between gap-3",
                                        onclick: move |_| studio.play_contextual_mix(auth, player, slug.clone()),
                                        div { class: "space-y-1.5",
                                            div { class: "flex items-center justify-between gap-2",
                                                span { class: "px-2 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[10px] font-semibold text-beet-text-tertiary",
                                                    "{badge}"
                                                }
                                                span { class: "w-8 h-8 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center group-hover:scale-105 transition-transform",
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_terminal_progress_is_ignored_until_new_run_is_observed() {
        let stale_complete = DiscoveryProgress {
            status: GenerationStatus::Complete,
            started_at: Some("2026-11-27T08:00:00Z".into()),
            ..Default::default()
        };
        // Prior Complete cached before click must NOT fire when awaiting new run.
        assert!(!is_fresh_terminal_progress(
            &stale_complete,
            false,
            true,
            Some("2026-11-27T08:00:00Z"),
        ));

        let stale_error = DiscoveryProgress {
            status: GenerationStatus::Error,
            started_at: Some("2026-11-27T08:00:00Z".into()),
            ..Default::default()
        };
        assert!(!is_fresh_terminal_progress(
            &stale_error,
            false,
            true,
            Some("2026-11-27T08:00:00Z"),
        ));

        // Instant finish of a NEW run (different started_at) IS accepted while awaiting_new_run.
        let instant_complete = DiscoveryProgress {
            status: GenerationStatus::Complete,
            started_at: Some("2026-11-27T08:02:00Z".into()),
            ..Default::default()
        };
        assert!(is_fresh_terminal_progress(
            &instant_complete,
            false,
            true,
            Some("2026-11-27T08:00:00Z"),
        ));

        // Terminal after observing Running is always accepted.
        assert!(is_fresh_terminal_progress(
            &stale_complete,
            true,
            false,
            Some("2026-11-27T08:00:00Z"),
        ));
    }
}
