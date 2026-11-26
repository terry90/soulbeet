use dioxus::prelude::*;
use shared::navidrome::{DeletionReason, DeletionReview, LibraryStats, TopPlayedTrack};
use shared::recommendation::UserCultureProfile;

use crate::{
    IconActivity, IconClock, IconDisc3, IconLibrary, IconMusic, IconPlay, IconRefreshCw,
    IconShieldCheck, IconSparkles, IconStar, TasteDnaBar,
};

#[derive(PartialEq, Clone, Copy, Default)]
pub enum DashboardTab {
    #[default]
    Overview,
    History,
    Discovery,
}

/// Formats a total library duration in seconds into a compact human string
/// (e.g., `"0m"`, `"45s"`, `"18m"`, `"42h 18m"`, `"3d 4h"`).
pub fn format_total_duration(secs: u64) -> String {
    if secs == 0 {
        return "0m".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let total_mins = secs / 60;
    if total_mins < 60 {
        return format!("{total_mins}m");
    }
    let total_hours = secs / 3600;
    let rem_mins = (secs % 3600) / 60;
    if total_hours < 48 {
        if rem_mins == 0 {
            format!("{total_hours}h")
        } else {
            format!("{total_hours}h {rem_mins}m")
        }
    } else {
        let days = total_hours / 24;
        let rem_hours = total_hours % 24;
        if rem_hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {rem_hours}h")
        }
    }
}

/// Formats a single track duration in seconds as `m:ss` (or `h:mm:ss`).
pub fn format_track_duration(secs: u32) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem_secs = secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{rem_secs:02}")
    } else {
        format!("{mins}:{rem_secs:02}")
    }
}

/// Formats an integer with ASCII comma thousands separators (e.g. `2384` -> `"2,384"`).
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Computes a rounded whole percentage `0..=100`, returning `0` when `whole == 0`.
pub fn percentage_u32(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    ((part as f64 / whole as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u32
}

/// Computes a horizontal bar fill width percentage scaled against `max_value`.
/// Non-zero values receive at least a 2% visible sliver so small buckets remain legible.
pub fn bar_width_pct(value: u64, max_value: u64) -> f64 {
    if value == 0 || max_value == 0 {
        return 0.0;
    }
    ((value as f64 / max_value as f64) * 100.0).clamp(2.0, 100.0)
}

#[component]
pub fn DashboardTabs(active: DashboardTab, on_change: EventHandler<DashboardTab>) -> Element {
    let render_tab = move |label: &'static str, tab: DashboardTab| {
        let is_active = active == tab;
        let state_class = if is_active {
            "bg-beet-surface-high text-beet-text-primary border-beet-border-default"
        } else {
            "text-beet-text-secondary border-transparent hover:text-beet-text-primary hover:bg-beet-surface/60"
        };
        rsx! {
            button {
                r#type: "button",
                aria_current: is_active.then_some("page"),
                class: "inline-flex items-center gap-2 h-9 px-3.5 rounded-[8px] border text-[13px] font-medium transition-colors cursor-pointer shrink-0 {state_class}",
                onclick: move |_| on_change.call(tab),
                match tab {
                    DashboardTab::Overview => rsx! { IconActivity { size: 15 } },
                    DashboardTab::History => rsx! { IconClock { size: 15 } },
                    DashboardTab::Discovery => rsx! { IconSparkles { size: 15 } },
                }
                span { "{label}" }
            }
        }
    };

    rsx! {
        nav {
            aria_label: "Dashboard sections",
            class: "inline-flex items-center gap-1 p-1 rounded-[12px] bg-beet-surface-low border border-beet-border-subtle max-w-full overflow-x-auto no-scrollbar",
            {render_tab("Overview", DashboardTab::Overview)}
            {render_tab("History", DashboardTab::History)}
            {render_tab("Discovery", DashboardTab::Discovery)}
        }
    }
}

#[component]
pub fn StatsOverview(
    stats: LibraryStats,
    #[props(default)] culture: Option<UserCultureProfile>,
    #[props(default)] pending_triage: Option<usize>,
    #[props(default)] on_play_track: Option<EventHandler<TopPlayedTrack>>,
    #[props(default)] on_refresh: Option<EventHandler<()>>,
    #[props(default)] refreshing: bool,
) -> Element {
    if stats.total_tracks == 0 {
        return rsx! {
            section {
                aria_label: "Library overview",
                class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 sm:p-10 text-center space-y-3",
                div { class: "w-12 h-12 rounded-[12px] bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary flex items-center justify-center mx-auto",
                    IconLibrary { size: 22 }
                }
                h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                    "Your library has no indexed tracks yet"
                }
                p { class: "text-[13px] text-beet-text-secondary max-w-md mx-auto leading-relaxed",
                    "Connect Navidrome in Settings or search the catalog to acquire music and unlock listening insights."
                }
                if let Some(refresh) = on_refresh {
                    div { class: "pt-2",
                        button {
                            r#type: "button",
                            disabled: refreshing,
                            class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary hover:bg-beet-surface-highest disabled:opacity-50 transition-colors cursor-pointer",
                            onclick: move |_| refresh.call(()),
                            IconRefreshCw {
                                size: 14,
                                class: if refreshing { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                            }
                            span { if refreshing { "Refreshing…" } else { "Refresh stats" } }
                        }
                    }
                }
            }
        };
    }

    let rated_pct = percentage_u32(u64::from(stats.rated_tracks), u64::from(stats.total_tracks));
    let max_rating_count = stats.rating_distribution.iter().copied().max().unwrap_or(0);
    let max_genre_count = stats
        .genres
        .iter()
        .take(10)
        .map(|(_, c)| *c)
        .max()
        .unwrap_or(0);
    let max_artist_plays = stats
        .top_played_artists
        .iter()
        .take(8)
        .map(|(_, p)| *p)
        .max()
        .unwrap_or(0);

    let show_listening_insights = stats.total_plays > 0
        || !stats.top_played_artists.is_empty()
        || !stats.top_played_tracks.is_empty();

    let top_genre_names: Vec<String> = stats
        .genres
        .iter()
        .take(6)
        .map(|(name, _)| name.clone())
        .collect();

    let duration_label = format_total_duration(stats.total_duration_secs);
    let top_artist_subtitle = stats
        .top_played_artists
        .first()
        .map(|(artist, _)| format!("Top: {artist}"))
        .unwrap_or_else(|| "No scrobbles yet".to_string());
    let top_artist_plays_chip = stats.top_played_artists.first().map(|(_, plays)| {
        if *plays == 1 {
            "1 play".to_string()
        } else {
            format!("{} plays", format_count(*plays))
        }
    });

    rsx! {
        div { class: "space-y-4",
            // Optional Studio action bar when refresh is wired
            if let Some(refresh) = on_refresh {
                div { class: "flex flex-wrap items-center justify-between gap-2",
                    div { class: "flex items-center gap-2 min-w-0",
                        h2 { class: "text-[16px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "Library Insights"
                        }
                        if stats.total_duration_secs > 0 {
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle",
                                "{duration_label}"
                            }
                        }
                    }
                    button {
                        r#type: "button",
                        disabled: refreshing,
                        aria_label: "Refresh library stats",
                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface border border-beet-border-default text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-50 transition-colors cursor-pointer",
                        onclick: move |_| refresh.call(()),
                        IconRefreshCw {
                            size: 13,
                            class: if refreshing { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                        }
                        span { if refreshing { "Refreshing…" } else { "Refresh" } }
                    }
                }
            }

            // 1. Top KPI Strip — Balanced 4 Studio Bento Cards
            div { class: "grid grid-cols-2 lg:grid-cols-4 gap-3.5",
                // Card 1: Library
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-2.5",
                    div { class: "flex items-center justify-between gap-2",
                        span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "Library"
                        }
                        IconLibrary { size: 15, class: "text-beet-text-tertiary".to_string() }
                    }
                    div { class: "space-y-1",
                        p { class: "tabular-nums-mono text-[26px] font-semibold text-beet-text-primary leading-none tracking-tight",
                            "{format_count(u64::from(stats.total_tracks))}"
                        }
                        p { class: "text-[12px] text-beet-text-secondary truncate",
                            "{format_count(u64::from(stats.total_albums))} albums · {format_count(u64::from(stats.total_artists))} artists"
                        }
                    }
                    div { class: "flex items-center gap-1.5 pt-0.5",
                        span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-tertiary",
                            IconClock { size: 11, class: "text-beet-text-tertiary".to_string() }
                            span {
                                if stats.total_duration_secs > 0 {
                                    "{duration_label} runtime"
                                } else {
                                    "Indexed tracks"
                                }
                            }
                        }
                    }
                }

                // Card 2: Rating Health
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-2.5",
                    div { class: "flex items-center justify-between gap-2",
                        span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "Rating Health"
                        }
                        span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-gold-surface text-beet-gold tabular-nums-mono text-[11px] font-semibold",
                            IconStar { size: 11, filled: true }
                            span {
                                if stats.rated_tracks > 0 {
                                    "{stats.average_rating:.1}"
                                } else {
                                    "—"
                                }
                            }
                        }
                    }
                    div { class: "space-y-2",
                        p { class: "tabular-nums-mono text-[26px] font-semibold text-beet-text-primary leading-none tracking-tight",
                            "{rated_pct}%"
                        }
                        div {
                            class: "h-1.5 w-full rounded-full bg-beet-surface-high overflow-hidden",
                            role: "progressbar",
                            aria_label: "Rated library percentage",
                            "aria-valuenow": "{rated_pct}",
                            "aria-valuemin": "0",
                            "aria-valuemax": "100",
                            div {
                                class: "h-full rounded-full bg-beet-gold transition-all duration-300",
                                style: "width: {rated_pct}%",
                            }
                        }
                    }
                    p { class: "text-[12px] text-beet-text-secondary truncate",
                        "{format_count(u64::from(stats.rated_tracks))} rated · {format_count(u64::from(stats.unrated_tracks))} unrated"
                    }
                }

                // Card 3: Total Plays
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-2.5",
                    div { class: "flex items-center justify-between gap-2",
                        span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "Total Plays"
                        }
                        IconActivity { size: 15, class: "text-beet-leaf-text".to_string() }
                    }
                    div { class: "space-y-1 min-w-0",
                        p { class: "tabular-nums-mono text-[26px] font-semibold text-beet-text-primary leading-none tracking-tight",
                            "{format_count(stats.total_plays)}"
                        }
                        p { class: "text-[12px] text-beet-text-secondary truncate",
                            "{top_artist_subtitle}"
                        }
                    }
                    div { class: "flex items-center gap-1.5 pt-0.5",
                        span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-tertiary truncate max-w-full",
                            if let Some(chip) = top_artist_plays_chip {
                                "{chip} on #1 artist"
                            } else {
                                "Scrobble activity"
                            }
                        }
                    }
                }

                // Card 4: Curation Pipeline (when pending_triage is provided) or Distinct Genres
                if let Some(pending) = pending_triage {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-2.5",
                        div { class: "flex items-center justify-between gap-2",
                            span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                "Curation"
                            }
                            IconSparkles { size: 15, class: "text-beet-accent-text".to_string() }
                        }
                        div { class: "space-y-1",
                            p {
                                class: if pending > 0 {
                                    "tabular-nums-mono text-[26px] font-semibold text-beet-accent-text leading-none tracking-tight"
                                } else {
                                    "tabular-nums-mono text-[26px] font-semibold text-beet-leaf-text leading-none tracking-tight"
                                },
                                "{format_count(pending as u64)}"
                            }
                            p { class: "text-[12px] text-beet-text-secondary truncate",
                                if pending > 0 {
                                    "Awaiting Keep / Drop verdict"
                                } else {
                                    "Discovery queue clear"
                                }
                            }
                        }
                        div { class: "flex items-center gap-1.5 pt-0.5",
                            span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-tertiary truncate max-w-full",
                                if let Some((top_genre, _)) = stats.genres.first() {
                                    "{stats.genres.len()} genres · {top_genre}"
                                } else {
                                    "{stats.genres.len()} genres"
                                }
                            }
                        }
                    }
                } else {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 flex flex-col justify-between gap-2.5",
                        div { class: "flex items-center justify-between gap-2",
                            span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                "Genres"
                            }
                            IconDisc3 { size: 15, class: "text-beet-leaf-text".to_string() }
                        }
                        div { class: "space-y-1 min-w-0",
                            p { class: "tabular-nums-mono text-[26px] font-semibold text-beet-text-primary leading-none tracking-tight",
                                "{format_count(stats.genres.len() as u64)}"
                            }
                            p { class: "text-[12px] text-beet-text-secondary truncate",
                                if let Some((top_genre, _)) = stats.genres.first() {
                                    "Top genre: {top_genre}"
                                } else {
                                    "No genre tags yet"
                                }
                            }
                        }
                        div { class: "flex items-center gap-1.5 pt-0.5",
                            span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                if let Some((_, count)) = stats.genres.first() {
                                    "{format_count(u64::from(*count))} tracks ({percentage_u32(u64::from(*count), u64::from(stats.total_tracks))}%)"
                                } else {
                                    "Distinct tags"
                                }
                            }
                        }
                    }
                }
            }

            // 2. Taste DNA Strip (when culture profile is available)
            if let Some(ref profile) = culture {
                TasteDnaBar {
                    profile: profile.clone(),
                    top_genres: top_genre_names,
                }
            }

            // 3. Ratings & Genre Spectrum Bento Row
            div { class: "grid grid-cols-1 lg:grid-cols-12 gap-4",
                // Rating Distribution (lg:col-span-5)
                section {
                    aria_label: "Rating Distribution",
                    class: "lg:col-span-5 rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                    div { class: "flex items-center justify-between gap-2",
                        h3 { class: "text-[15px] font-semibold text-beet-text-primary",
                            "Rating Distribution"
                        }
                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                            "{format_count(u64::from(stats.rated_tracks))} RATED"
                        }
                    }

                    div { class: "space-y-3",
                        for stars in [5usize, 4, 3, 2, 1] {
                            {
                                let count = stats.rating_distribution[stars - 1];
                                let width = bar_width_pct(u64::from(count), u64::from(max_rating_count));
                                let share_pct = percentage_u32(u64::from(count), u64::from(stats.rated_tracks));
                                let (star_color, bar_color) = match stars {
                                    5 => ("text-beet-gold", "bg-beet-gold"),
                                    4 => ("text-beet-gold", "bg-beet-gold/75"),
                                    3 => ("text-beet-accent-text", "bg-beet-accent-solid/75"),
                                    2 => ("text-beet-text-tertiary", "bg-beet-text-tertiary/60"),
                                    _ => ("text-beet-crimson-text", "bg-beet-crimson-solid"),
                                };
                                rsx! {
                                    div { key: "rating-{stars}", class: "space-y-1",
                                        div { class: "flex items-center gap-3",
                                            div { class: "flex items-center gap-1.5 w-11 shrink-0",
                                                span { class: "tabular-nums-mono text-[12px] font-semibold text-beet-text-primary w-3",
                                                    "{stars}"
                                                }
                                                IconStar { size: 12, filled: true, class: star_color.to_string() }
                                            }
                                            div { class: "flex-1 h-2.5 rounded-full bg-beet-surface-low overflow-hidden",
                                                div {
                                                    class: "h-full rounded-full transition-all duration-300 {bar_color}",
                                                    style: "width: {width:.1}%",
                                                }
                                            }
                                            div { class: "flex items-center justify-end gap-2 w-20 shrink-0 tabular-nums-mono text-[11px]",
                                                span { class: "text-beet-text-primary font-medium",
                                                    "{format_count(u64::from(count))}"
                                                }
                                                span { class: "text-beet-text-tertiary w-8 text-right",
                                                    "{share_pct}%"
                                                }
                                            }
                                        }
                                        if stars == 1 {
                                            div { class: "pl-14 flex items-center gap-1.5",
                                                span { class: "px-1.5 py-0.5 rounded bg-beet-crimson-surface text-beet-crimson-text tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                                    "Auto-delete"
                                                }
                                                span { class: "text-[11px] text-beet-text-tertiary",
                                                    "Flagged by rating sync guard"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Top Genres (lg:col-span-7)
                section {
                    aria_label: "Top Genres",
                    class: "lg:col-span-7 rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                    div { class: "flex items-center justify-between gap-2",
                        h3 { class: "text-[15px] font-semibold text-beet-text-primary",
                            "Top Genres"
                        }
                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                            "{stats.genres.len()} GENRES"
                        }
                    }

                    if stats.genres.is_empty() {
                        div { class: "py-6 text-center text-[13px] text-beet-text-tertiary",
                            "No genre tags indexed yet."
                        }
                    } else {
                        div { class: "space-y-2.5",
                            for (idx, (genre, count)) in stats.genres.iter().take(10).enumerate() {
                                {
                                    let rank = idx + 1;
                                    let width = bar_width_pct(u64::from(*count), u64::from(max_genre_count));
                                    let lib_pct = percentage_u32(u64::from(*count), u64::from(stats.total_tracks));
                                    rsx! {
                                        div { key: "genre-{rank}-{genre}", class: "flex items-center gap-2.5",
                                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary w-6 shrink-0",
                                                "#{rank}"
                                            }
                                            span { class: "text-[13px] font-medium text-beet-text-primary truncate w-28 sm:w-36 shrink-0",
                                                "{genre}"
                                            }
                                            div { class: "flex-1 h-2 rounded-full bg-beet-surface-low overflow-hidden",
                                                div {
                                                    class: "h-full rounded-full bg-beet-leaf-solid/70 transition-all duration-300",
                                                    style: "width: {width:.1}%",
                                                }
                                            }
                                            div { class: "flex items-center justify-end gap-2 w-20 shrink-0 tabular-nums-mono text-[11px]",
                                                span { class: "text-beet-text-primary font-medium",
                                                    "{format_count(u64::from(*count))}"
                                                }
                                                span { class: "text-beet-text-tertiary w-8 text-right",
                                                    "{lib_pct}%"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if stats.genres.len() > 10 {
                            div { class: "pt-3 border-t border-beet-border-subtle flex flex-wrap gap-1.5",
                                for (genre, count) in stats.genres.iter().skip(10).take(10) {
                                    span {
                                        key: "genre-more-{genre}",
                                        class: "inline-flex items-center gap-1.5 px-2.5 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle text-[11px] text-beet-text-secondary",
                                        span { "{genre}" }
                                        span { class: "tabular-nums-mono text-beet-text-tertiary", "{count}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 4. Listening Insights Bento Row
            if show_listening_insights {
                div { class: "grid grid-cols-1 lg:grid-cols-12 gap-4",
                    // Top Played Artists (lg:col-span-5)
                    section {
                        aria_label: "Top Played Artists",
                        class: "lg:col-span-5 rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                        div { class: "flex items-center justify-between gap-2",
                            h3 { class: "text-[15px] font-semibold text-beet-text-primary",
                                "Top Played Artists"
                            }
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                "{stats.top_played_artists.len().min(8)} ARTISTS"
                            }
                        }

                        if stats.top_played_artists.is_empty() {
                            div { class: "py-6 text-center text-[13px] text-beet-text-tertiary",
                                "No artist play history recorded yet."
                            }
                        } else {
                            div { class: "space-y-3",
                                for (idx, (artist, plays)) in stats.top_played_artists.iter().take(8).enumerate() {
                                    {
                                        let rank = idx + 1;
                                        let width = bar_width_pct(*plays, max_artist_plays);
                                        let plays_label = if *plays == 1 {
                                            "1 play".to_string()
                                        } else {
                                            format!("{} plays", format_count(*plays))
                                        };
                                        rsx! {
                                            div { key: "top-artist-{rank}-{artist}", class: "space-y-1",
                                                div { class: "flex items-center justify-between gap-2",
                                                    div { class: "flex items-center gap-2 min-w-0",
                                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary w-5 shrink-0",
                                                            "#{rank}"
                                                        }
                                                        span { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                            "{artist}"
                                                        }
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-secondary shrink-0",
                                                        "{plays_label}"
                                                    }
                                                }
                                                div { class: "pl-7",
                                                    div { class: "h-1.5 rounded-full bg-beet-surface-low overflow-hidden",
                                                        div {
                                                            class: "h-full rounded-full bg-beet-accent-solid/70 transition-all duration-300",
                                                            style: "width: {width:.1}%",
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

                    // Most Played Tracks (lg:col-span-7)
                    section {
                        aria_label: "Most Played Tracks",
                        class: "lg:col-span-7 rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                        div { class: "flex items-center justify-between gap-2",
                            div { class: "flex items-center gap-2",
                                IconMusic { size: 15, class: "text-beet-leaf-text".to_string() }
                                h3 { class: "text-[15px] font-semibold text-beet-text-primary",
                                    "Most Played Tracks"
                                }
                            }
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                if on_play_track.is_some() {
                                    "CLICK TO PLAY"
                                } else {
                                    "{stats.top_played_tracks.len().min(8)} TRACKS"
                                }
                            }
                        }

                        if stats.top_played_tracks.is_empty() {
                            div { class: "py-6 text-center text-[13px] text-beet-text-tertiary",
                                "No track play history recorded yet."
                            }
                        } else {
                            div { class: "space-y-1", role: "list",
                                for (idx, track) in stats.top_played_tracks.iter().take(8).enumerate() {
                                    {
                                        let rank = idx + 1;
                                        let play_handler = on_play_track;
                                        let track_for_play = track.clone();
                                        let plays_label = if track.play_count == 1 {
                                            "1 play".to_string()
                                        } else {
                                            format!("{} plays", format_count(track.play_count))
                                        };
                                        let subtitle = if track.album.is_empty() {
                                            track.artist.clone()
                                        } else {
                                            format!("{} · {}", track.artist, track.album)
                                        };
                                        let rating_chip = track.rating.filter(|&r| r > 0);
                                        let dur_label = track.duration.filter(|&d| d > 0).map(format_track_duration);

                                        if let Some(handler) = play_handler {
                                            rsx! {
                                                button {
                                                    key: "top-track-{rank}-{track.song_id}",
                                                    r#type: "button",
                                                    role: "listitem",
                                                    aria_label: format!("Play {} by {}", track.title, track.artist),
                                                    class: "group w-full flex items-center gap-3 px-2.5 py-2 rounded-[8px] hover:bg-beet-surface-high/80 text-left transition-colors cursor-pointer",
                                                    onclick: move |_| handler.call(track_for_play.clone()),
                                                    div { class: "w-7 h-7 flex items-center justify-center shrink-0 relative",
                                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary group-hover:opacity-0 transition-opacity",
                                                            "#{rank}"
                                                        }
                                                        span { class: "absolute inset-0 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity",
                                                            IconPlay { size: 12, filled: true }
                                                        }
                                                    }
                                                    div { class: "flex-1 min-w-0",
                                                        p { class: "text-[13px] font-medium text-beet-text-primary group-hover:text-beet-leaf-text truncate transition-colors",
                                                            "{track.title}"
                                                        }
                                                        p { class: "text-[12px] text-beet-text-secondary truncate",
                                                            "{subtitle}"
                                                        }
                                                    }
                                                    div { class: "flex items-center gap-2 shrink-0",
                                                        if let Some(r) = rating_chip {
                                                            span { class: "inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-beet-gold-surface text-beet-gold tabular-nums-mono text-[10px] font-semibold",
                                                                IconStar { size: 10, filled: true }
                                                                span { "{r}" }
                                                            }
                                                        }
                                                        if let Some(dur) = dur_label {
                                                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary hidden sm:inline",
                                                                "{dur}"
                                                            }
                                                        }
                                                        span { class: "px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-secondary",
                                                            "{plays_label}"
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            rsx! {
                                                div {
                                                    key: "top-track-{rank}-{track.song_id}",
                                                    role: "listitem",
                                                    class: "flex items-center gap-3 px-2.5 py-2 rounded-[8px] bg-beet-surface-low/50",
                                                    span { class: "w-7 text-center tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                                        "#{rank}"
                                                    }
                                                    div { class: "flex-1 min-w-0",
                                                        p { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                            "{track.title}"
                                                        }
                                                        p { class: "text-[12px] text-beet-text-secondary truncate",
                                                            "{subtitle}"
                                                        }
                                                    }
                                                    div { class: "flex items-center gap-2 shrink-0",
                                                        if let Some(r) = rating_chip {
                                                            span { class: "inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-beet-gold-surface text-beet-gold tabular-nums-mono text-[10px] font-semibold",
                                                                IconStar { size: 10, filled: true }
                                                                span { "{r}" }
                                                            }
                                                        }
                                                        if let Some(dur) = dur_label {
                                                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary hidden sm:inline",
                                                                "{dur}"
                                                            }
                                                        }
                                                        span { class: "px-2 py-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] text-beet-text-secondary",
                                                            "{plays_label}"
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

/// "Replaced" for a replaced copy, else "Deleted"; then " via {client}" when
/// a connected app did it.
fn deletion_label(review: &DeletionReview) -> String {
    let what = if review.reason == DeletionReason::Replaced {
        "Replaced"
    } else {
        "Deleted"
    };
    match &review.client {
        Some(client) => format!("{what} via {client}"),
        None => what.to_string(),
    }
}

#[component]
pub fn DeletionHistoryTab() -> Element {
    let history = use_resource(|| async { api::get_deletion_history().await });

    let items = match &*history.read() {
        Some(Ok(items)) => items.clone(),
        _ => vec![],
    };

    rsx! {
        section {
            aria_label: "Deletion History",
            class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
            div { class: "flex flex-wrap items-center justify-between gap-2",
                div { class: "flex items-center gap-2",
                    IconShieldCheck { size: 16, class: "text-beet-leaf-text".to_string() }
                    h3 { class: "text-[15px] font-semibold text-beet-text-primary",
                        "Deletion History"
                    }
                }
                p { class: "text-[12px] text-beet-text-tertiary",
                    "Configure auto-delete in Settings → Library"
                }
            }

            if items.is_empty() {
                div { class: "rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-secondary",
                    "No deletion history yet."
                }
            } else {
                div { class: "space-y-1.5 max-h-96 overflow-y-auto studio-scrollbar pr-1",
                    for item in items {
                        {
                            let badge_class = if item.reason == DeletionReason::Replaced {
                                "bg-beet-accent-surface text-beet-accent-text"
                            } else {
                                "bg-beet-crimson-surface text-beet-crimson-text"
                            };
                            rsx! {
                                div {
                                    key: "del-{item.id}",
                                    class: "flex items-center justify-between gap-3 px-3 py-2.5 rounded-[8px] bg-beet-surface-low border border-beet-border-subtle text-[13px]",
                                    div { class: "flex-1 min-w-0",
                                        span { class: "font-medium text-beet-text-primary truncate", "{item.title}" }
                                        span { class: "text-beet-text-disabled mx-2", "·" }
                                        span { class: "text-beet-text-secondary truncate", "{item.artist}" }
                                    }
                                    span { class: "px-2 py-0.5 rounded-full tabular-nums-mono text-[11px] font-semibold shrink-0 {badge_class}",
                                        {deletion_label(&item)}
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

    fn review(reason: DeletionReason, client: Option<&str>) -> DeletionReview {
        DeletionReview {
            id: "r1".into(),
            song_id: "s1".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            path: None,
            rating: None,
            reason,
            created_at: "2026-11-16 10:00:00".into(),
            origin: "mcp".into(),
            client: client.map(Into::into),
        }
    }

    #[test]
    fn a_deletion_by_an_app_names_it() {
        assert_eq!(
            deletion_label(&review(DeletionReason::Cleanup, Some("claude.ai"))),
            "Deleted via claude.ai"
        );
        assert_eq!(
            deletion_label(&review(DeletionReason::Replaced, None)),
            "Replaced"
        );
        assert_eq!(
            deletion_label(&review(DeletionReason::Disliked, None)),
            "Deleted"
        );
    }

    #[test]
    fn format_total_duration_handles_all_scales() {
        assert_eq!(format_total_duration(0), "0m");
        assert_eq!(format_total_duration(45), "45s");
        assert_eq!(format_total_duration(60), "1m");
        assert_eq!(format_total_duration(18 * 60 + 20), "18m");
        assert_eq!(format_total_duration(3600), "1h");
        assert_eq!(format_total_duration(42 * 3600 + 18 * 60), "42h 18m");
        assert_eq!(format_total_duration(48 * 3600), "2d");
        assert_eq!(format_total_duration(76 * 3600), "3d 4h");
    }

    #[test]
    fn format_track_duration_and_count_format_cleanly() {
        assert_eq!(format_track_duration(0), "0:00");
        assert_eq!(format_track_duration(222), "3:42");
        assert_eq!(format_track_duration(3661), "1:01:01");

        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(2384), "2,384");
        assert_eq!(format_count(1_250_000), "1,250,000");
    }

    #[test]
    fn rating_and_genre_percentage_math_handles_zero_and_bounds() {
        assert_eq!(percentage_u32(0, 0), 0);
        assert_eq!(percentage_u32(5, 0), 0);
        assert_eq!(percentage_u32(612, 2384), 26);
        assert_eq!(percentage_u32(241, 612), 39);
        assert_eq!(percentage_u32(500, 200), 100);

        assert_eq!(bar_width_pct(0, 100), 0.0);
        assert_eq!(bar_width_pct(10, 0), 0.0);
        assert_eq!(bar_width_pct(1, 1000), 2.0);
        assert_eq!(bar_width_pct(50, 100), 50.0);
        assert_eq!(bar_width_pct(100, 100), 100.0);
    }

    #[test]
    fn stats_overview_renders_empty_and_populated_bento_states() {
        let mut empty_dom = VirtualDom::new(|| {
            rsx! { StatsOverview { stats: LibraryStats::default() } }
        });
        empty_dom.rebuild_in_place();
        let empty_html = dioxus_ssr::render(&empty_dom);
        assert!(empty_html.contains("Your library has no indexed tracks yet"));

        let populated = LibraryStats {
            total_tracks: 2384,
            rated_tracks: 612,
            unrated_tracks: 1772,
            average_rating: 3.7,
            rating_distribution: [18, 47, 166, 241, 140],
            total_albums: 214,
            total_artists: 131,
            genres: vec![("Indie Rock".into(), 412), ("Dream Pop".into(), 288)],
            total_plays: 1420,
            total_duration_secs: 42 * 3600 + 18 * 60,
            top_played_artists: vec![("Daft Punk".into(), 180)],
            top_played_tracks: vec![TopPlayedTrack {
                song_id: "s1".into(),
                title: "Digital Love".into(),
                artist: "Daft Punk".into(),
                album: "Discovery".into(),
                play_count: 42,
                rating: Some(5),
                duration: Some(301),
            }],
        };

        let mut dom = VirtualDom::new_with_props(
            |stats: LibraryStats| {
                rsx! {
                    StatsOverview {
                        stats,
                        pending_triage: Some(7),
                    }
                }
            },
            populated,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("2,384"));
        assert!(html.contains("42h 18m"));
        assert!(html.contains("Rating Distribution"));
        assert!(html.contains("Auto-delete"));
        assert!(html.contains("Top Genres"));
        assert!(html.contains("Top Played Artists"));
        assert!(html.contains("Most Played Tracks"));
        assert!(html.contains("Digital Love"));
        assert!(html.contains("Awaiting Keep / Drop verdict"));
    }
}
