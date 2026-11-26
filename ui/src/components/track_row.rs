use dioxus::prelude::*;
use shared::download::AcquireAndPlayStage;

use crate::{
    IconCheck, IconInfo, IconLoader, IconMusic, IconPlay, IconPlus, IconRefreshCw,
    IconSlidersHorizontal, IconSparkles, IconStar, IconWifiOff, IconX, StarStrip, StarStripSize,
};

/// Visual acquisition stage badge for an unowned catalog track undergoing
/// 1-tap `Acquire & Play`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireStageBadge {
    Acquiring { label: Option<String> },
    Importing,
    Indexing,
    NeedsManualChoice { message: Option<String> },
    Failed { message: Option<String> },
}

impl AcquireStageBadge {
    /// Maps a backend WebSocket [`AcquireAndPlayStage`] event into a row badge.
    /// Returns `None` once the track reaches `Ready` (the row should transition to `Owned`).
    pub fn from_stage(stage: &AcquireAndPlayStage) -> Option<Self> {
        match stage {
            AcquireAndPlayStage::Acquiring {
                backend_name,
                quality,
                ..
            } => {
                let label = match quality {
                    Some(q) if !q.trim().is_empty() => Some(format!("{backend_name} · {q}")),
                    _ if !backend_name.trim().is_empty() => Some(backend_name.clone()),
                    _ => None,
                };
                Some(Self::Acquiring { label })
            }
            AcquireAndPlayStage::Importing => Some(Self::Importing),
            AcquireAndPlayStage::Indexing => Some(Self::Indexing),
            AcquireAndPlayStage::NeedsManualChoice { .. } => Some(Self::NeedsManualChoice {
                message: Some("Best match was ambiguous — pick a source".to_string()),
            }),
            AcquireAndPlayStage::Failed { failure } => Some(Self::Failed {
                message: Some(failure.message.clone()),
            }),
            AcquireAndPlayStage::Ready { .. } => None,
        }
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(
            self,
            Self::Acquiring { .. } | Self::Importing | Self::Indexing
        )
    }

    pub fn badge_label(&self) -> String {
        match self {
            Self::Acquiring { label: Some(text) } if !text.trim().is_empty() => {
                format!("ACQUIRING · {}", text.trim())
            }
            Self::Acquiring { .. } => "ACQUIRING…".to_string(),
            Self::Importing => "IMPORTING…".to_string(),
            Self::Indexing => "INDEXING…".to_string(),
            Self::NeedsManualChoice { .. } => "PICK SOURCE".to_string(),
            Self::Failed { .. } => "RETRY".to_string(),
        }
    }
}

/// The 4 unified track states in Soulbeet's Refined Midnight Studio UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackRowState {
    /// Permanent library track owned by the user.
    Owned {
        is_offline_cached: bool,
        starred: bool,
        rating: u8,
    },
    /// Staged discovery track awaiting user curation (`Keep` or `Drop`).
    PendingDiscovery {
        days_remaining: Option<u32>,
        reason_label: Option<String>,
    },
    /// Global catalog track not yet in the library; tappable for `Acquire & Play`.
    UnownedCatalog {
        acquiring_stage: Option<AcquireStageBadge>,
    },
    /// Uncached library track while global `Offline Only` mode is active.
    OfflineUnavailable,
}

impl Default for TrackRowState {
    fn default() -> Self {
        Self::Owned {
            is_offline_cached: false,
            starred: false,
            rating: 0,
        }
    }
}

/// Computes whether the row's primary click surface is interactive for `state` and callbacks.
pub fn is_primary_action_enabled(
    state: &TrackRowState,
    has_play: bool,
    has_acquire: bool,
    has_pick_source: bool,
) -> bool {
    match state {
        TrackRowState::OfflineUnavailable => false,
        TrackRowState::UnownedCatalog { acquiring_stage } => match acquiring_stage {
            Some(stage) if stage.is_in_flight() => false,
            Some(AcquireStageBadge::NeedsManualChoice { .. }) => has_pick_source,
            None | Some(AcquireStageBadge::Failed { .. }) => has_acquire,
            Some(_) => false,
        },
        TrackRowState::Owned { .. } | TrackRowState::PendingDiscovery { .. } => has_play,
    }
}

/// Formats track duration in seconds as `MM:SS` or `H:MM:SS` for tabular mono display.
pub fn format_duration(total_secs: u32) -> String {
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

/// Formats the compact discovery expiry badge (`DISC · 9D` or `DISC`).
pub fn format_discovery_badge(days_remaining: Option<u32>) -> String {
    match days_remaining {
        Some(days) => format!("DISC · {days}D"),
        None => "DISC".to_string(),
    }
}

/// Unified Refined Midnight Studio track row supporting Owned, Pending Discovery,
/// Unowned Catalog (with `seam-light` acquisition sweep & manual source choice),
/// and Offline Unavailable states.
#[component]
pub fn TrackRow(
    title: String,
    artist: String,
    #[props(default)] album: Option<String>,
    #[props(default)] cover_art_url: Option<String>,
    #[props(default)] duration_secs: Option<u32>,
    #[props(default)] codec_badge: Option<String>,
    #[props(default)] is_playing: bool,
    #[props(default)] state: TrackRowState,
    #[props(default)] track_number: Option<u32>,
    #[props(default)] on_play: Option<EventHandler<()>>,
    #[props(default)] on_keep: Option<EventHandler<()>>,
    #[props(default)] on_drop: Option<EventHandler<()>>,
    #[props(default)] on_star: Option<EventHandler<bool>>,
    #[props(default)] on_rate: Option<EventHandler<u8>>,
    #[props(default)] on_acquire: Option<EventHandler<()>>,
    #[props(default)] on_pick_source: Option<EventHandler<()>>,
    #[props(default)] on_inspect: Option<EventHandler<()>>,
    #[props(default)] class: String,
) -> Element {
    let mut failed_art_url = use_signal(|| None::<String>);

    let is_offline_unavailable = matches!(state, TrackRowState::OfflineUnavailable);
    let is_acquiring = matches!(
        state,
        TrackRowState::UnownedCatalog {
            acquiring_stage: Some(ref stage)
        } if stage.is_in_flight()
    );
    let is_unowned_idle = matches!(
        state,
        TrackRowState::UnownedCatalog {
            acquiring_stage: None
        }
    );

    let can_primary_click = is_primary_action_enabled(
        &state,
        on_play.is_some(),
        on_acquire.is_some(),
        on_pick_source.is_some(),
    );

    let row_surface = if is_offline_unavailable {
        "opacity-[0.42] bg-transparent cursor-not-allowed"
    } else if is_playing {
        "bg-beet-surface-low border-beet-leaf-solid/25"
    } else if is_unowned_idle {
        "opacity-[0.78] hover:opacity-100 hover:bg-beet-surface-low"
    } else {
        "hover:bg-beet-surface-low"
    };

    let title_class = if is_playing {
        "text-beet-leaf-text"
    } else if is_offline_unavailable {
        "text-beet-text-disabled"
    } else {
        "text-beet-text-primary"
    };

    let show_art = cover_art_url
        .as_ref()
        .filter(|url| !url.is_empty() && failed_art_url.read().as_ref() != Some(*url))
        .cloned();

    let subtitle = match album.as_deref().filter(|a| !a.trim().is_empty()) {
        Some(alb) => format!("{artist} · {alb}"),
        None => artist.clone(),
    };

    let play_aria_label = match &state {
        TrackRowState::OfflineUnavailable => {
            format!("{title} by {artist} is not cached offline")
        }
        TrackRowState::UnownedCatalog {
            acquiring_stage: Some(AcquireStageBadge::NeedsManualChoice { .. }),
        } => format!("Pick source for {title} by {artist}"),
        TrackRowState::UnownedCatalog { .. } => {
            format!("Acquire and play {title} by {artist}")
        }
        _ if is_playing => format!("Now playing {title} by {artist}"),
        _ => format!("Play {title} by {artist}"),
    };

    rsx! {
        div {
            role: "listitem",
            aria_disabled: if is_offline_unavailable { "true" } else { "false" },
            title: is_offline_unavailable.then_some("Not cached offline"),
            class: "group relative flex items-center gap-3 px-3 py-2 min-h-[56px] rounded-[8px] border border-transparent transition-[background-color,border-color,opacity] duration-150 {row_surface} {class}",

            // Optional track number column
            if let Some(num) = track_number {
                span { class: "w-6 text-right tabular-nums-mono text-[12px] text-beet-text-tertiary shrink-0",
                    "{num}"
                }
            }

            // Primary clickable area: Artwork + Title/Artist/Provenance.
            // Uses only phrasing-content `<span>` descendants for valid HTML5 inside `<button>`.
            button {
                r#type: "button",
                disabled: !can_primary_click,
                aria_label: "{play_aria_label}",
                class: if can_primary_click {
                    "flex items-center gap-3 flex-1 min-w-0 text-left cursor-pointer rounded-[6px]"
                } else {
                    "flex items-center gap-3 flex-1 min-w-0 text-left cursor-default rounded-[6px]"
                },
                onclick: move |_| {
                    if !can_primary_click {
                        return;
                    }
                    match &state {
                        TrackRowState::UnownedCatalog { acquiring_stage } => match acquiring_stage {
                            Some(AcquireStageBadge::NeedsManualChoice { .. }) => {
                                if let Some(handler) = &on_pick_source {
                                    handler.call(());
                                }
                            }
                            None | Some(AcquireStageBadge::Failed { .. }) => {
                                if let Some(handler) = &on_acquire {
                                    handler.call(());
                                }
                            }
                            Some(_) => {}
                        },
                        TrackRowState::Owned { .. } | TrackRowState::PendingDiscovery { .. } => {
                            if let Some(handler) = &on_play {
                                handler.call(());
                            }
                        }
                        TrackRowState::OfflineUnavailable => {}
                    }
                },

                // 44px Artwork Thumbnail
                span {
                    class: if is_playing {
                        "relative w-11 h-11 rounded-[6px] overflow-hidden bg-beet-surface border border-beet-leaf-solid/40 shrink-0 flex items-center justify-center"
                    } else {
                        "relative w-11 h-11 rounded-[6px] overflow-hidden bg-beet-surface border border-beet-border-subtle shrink-0 flex items-center justify-center"
                    },
                    if let Some(url) = show_art {
                        img {
                            src: "{url}",
                            alt: "Cover for {title}",
                            class: "w-full h-full object-cover",
                            loading: "lazy",
                            decoding: "async",
                            onerror: move |_| failed_art_url.set(Some(url.clone())),
                        }
                    } else {
                        IconMusic { size: 18, class: "text-beet-text-disabled".to_string() }
                    }

                    // Hover play overlay on playable rows
                    if can_primary_click || is_playing {
                        span {
                            class: if is_playing {
                                "absolute inset-0 bg-beet-canvas/60 flex items-center justify-center"
                            } else {
                                "absolute inset-0 bg-beet-canvas/60 opacity-0 group-hover:opacity-100 transition-opacity duration-150 flex items-center justify-center"
                            },
                            if is_playing {
                                // 3-bar studio equalizer
                                span {
                                    class: "flex items-end gap-[2px] h-3.5",
                                    aria_hidden: "true",
                                    span { class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar" }
                                    span {
                                        class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar",
                                        style: "animation-delay: 180ms;",
                                    }
                                    span {
                                        class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar",
                                        style: "animation-delay: 360ms;",
                                    }
                                }
                            } else {
                                IconPlay {
                                    size: 16,
                                    filled: true,
                                    class: "text-beet-text-primary".to_string(),
                                }
                            }
                        }
                    }
                }

                // Track Title, Artist/Album, and Discovery Reason
                span { class: "block flex-1 min-w-0",
                    span { class: "flex items-center gap-2 min-w-0",
                        span { class: "text-[15px] font-medium leading-[1.35] truncate {title_class}",
                            "{title}"
                        }
                        // Owned offline-cached status dot
                        if matches!(state, TrackRowState::Owned { is_offline_cached: true, .. }) {
                            span {
                                class: "w-1.5 h-1.5 rounded-full bg-beet-leaf-glow shrink-0",
                                title: "Available offline",
                                aria_label: "Available offline",
                            }
                        }
                        // Pending Discovery expiry pill
                        if let TrackRowState::PendingDiscovery { days_remaining, .. } = &state {
                            span { class: "inline-flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[10px] font-semibold tracking-wider shrink-0",
                                IconSparkles { size: 11 }
                                "{format_discovery_badge(*days_remaining)}"
                            }
                        }
                    }

                    span { class: "flex items-center gap-2 mt-0.5 min-w-0",
                        span { class: "text-[13px] text-beet-text-secondary truncate",
                            "{subtitle}"
                        }
                        if let TrackRowState::PendingDiscovery {
                            reason_label: Some(reason),
                            ..
                        } = &state
                        {
                            if !reason.trim().is_empty() {
                                span { class: "hidden md:inline-flex items-center gap-1 text-[11px] tabular-nums-mono text-beet-accent-text/90 truncate",
                                    "· {reason}"
                                }
                            }
                        }
                    }
                }
            }

            // Right-side state actions & telemetry
            div { class: "flex items-center gap-2 shrink-0",
                // Optional codec/bitrate telemetry badge
                if let Some(badge) = codec_badge.as_deref().filter(|b| !b.trim().is_empty()) {
                    span { class: "hidden sm:inline-flex items-center px-2 py-0.5 rounded-md bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[11px] font-semibold tracking-wider",
                        "{badge}"
                    }
                }

                // State-specific controls
                match &state {
                    TrackRowState::Owned { starred, rating, .. } => {
                        let is_starred = *starred;
                        let current_rating = *rating;
                        rsx! {
                            if let Some(rate_cb) = on_rate {
                                div { class: "hidden lg:flex items-center",
                                    StarStrip {
                                        rating: current_rating,
                                        size: StarStripSize::Compact,
                                        on_rate: move |next| rate_cb.call(next),
                                        aria_label: format!("Rate {title}"),
                                    }
                                }
                            }
                            if let Some(star_cb) = on_star {
                                button {
                                    r#type: "button",
                                    aria_pressed: if is_starred { "true" } else { "false" },
                                    aria_label: if is_starred {
                                        format!("Unstar {title}")
                                    } else {
                                        format!("Star {title}")
                                    },
                                    title: if is_starred { "Unstar track" } else { "Star track" },
                                    class: if is_starred {
                                        "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-gold hover:bg-beet-gold-surface transition-colors duration-150 cursor-pointer"
                                    } else {
                                        "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-gold hover:bg-beet-surface-high transition-colors duration-150 cursor-pointer"
                                    },
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        star_cb.call(!is_starred);
                                    },
                                    IconStar { size: 17, filled: is_starred }
                                }
                            }
                        }
                    }
                    TrackRowState::PendingDiscovery { .. } => {
                        rsx! {
                            div { class: "flex items-center gap-2",
                                if let Some(keep_cb) = on_keep {
                                    button {
                                        r#type: "button",
                                        aria_label: format!("Keep {title} in library"),
                                        title: "Keep in permanent library",
                                        class: "inline-flex items-center gap-1.5 h-8 px-2.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/35 text-beet-leaf-text hover:bg-beet-leaf-solid hover:text-beet-leaf-ink text-[12px] font-semibold transition-colors duration-150 cursor-pointer",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            keep_cb.call(());
                                        },
                                        IconCheck { size: 14 }
                                        span { "Keep" }
                                    }
                                }
                                if let Some(drop_cb) = on_drop {
                                    button {
                                        r#type: "button",
                                        aria_label: format!("Drop {title} from discovery"),
                                        title: "Drop from discovery",
                                        class: "inline-flex items-center gap-1 h-8 px-2.5 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white text-[12px] font-semibold transition-colors duration-150 cursor-pointer",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            drop_cb.call(());
                                        },
                                        IconX { size: 14 }
                                        span { class: "hidden sm:inline", "Drop" }
                                    }
                                }
                            }
                        }
                    }
                    TrackRowState::UnownedCatalog { acquiring_stage } => {
                        match acquiring_stage {
                            Some(stage) if stage.is_in_flight() => {
                                let label = stage.badge_label();
                                rsx! {
                                    span { class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold tracking-wider",
                                        IconLoader { size: 13 }
                                        "{label}"
                                    }
                                }
                            }
                            Some(AcquireStageBadge::NeedsManualChoice { message }) => {
                                let tip = message
                                    .as_deref()
                                    .unwrap_or("Pick a source manually");
                                rsx! {
                                    if let Some(pick_cb) = on_pick_source {
                                        button {
                                            r#type: "button",
                                            title: "{tip}",
                                            aria_label: format!("Pick source for {title}"),
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-gold-surface border border-beet-gold/40 text-beet-gold hover:brightness-110 tabular-nums-mono text-[11px] font-semibold tracking-wider transition-[filter] duration-150 cursor-pointer",
                                            onclick: move |evt| {
                                                evt.stop_propagation();
                                                pick_cb.call(());
                                            },
                                            IconSlidersHorizontal { size: 13 }
                                            "PICK SOURCE"
                                        }
                                    } else {
                                        span {
                                            title: "{tip}",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-gold-surface border border-beet-gold/40 text-beet-gold tabular-nums-mono text-[11px] font-semibold tracking-wider",
                                            IconSlidersHorizontal { size: 13 }
                                            "PICK SOURCE"
                                        }
                                    }
                                }
                            }
                            Some(AcquireStageBadge::Failed { message }) => {
                                let tip = message
                                    .as_deref()
                                    .unwrap_or("Acquisition failed — click to retry");
                                rsx! {
                                    if let Some(acquire_cb) = on_acquire {
                                        button {
                                            r#type: "button",
                                            title: "{tip}",
                                            aria_label: format!("Retry acquiring {title}"),
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/35 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white tabular-nums-mono text-[11px] font-semibold tracking-wider transition-colors duration-150 cursor-pointer",
                                            onclick: move |evt| {
                                                evt.stop_propagation();
                                                acquire_cb.call(());
                                            },
                                            IconRefreshCw { size: 13 }
                                            "RETRY"
                                        }
                                    } else {
                                        span {
                                            title: "{tip}",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/35 text-beet-crimson-text tabular-nums-mono text-[11px] font-semibold tracking-wider",
                                            "FAILED"
                                        }
                                    }
                                }
                            }
                            _ => {
                                rsx! {
                                    if let Some(acquire_cb) = on_acquire {
                                        button {
                                            r#type: "button",
                                            aria_label: format!("Acquire and play {title}"),
                                            title: "Acquire and play",
                                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-accent-surface hover:border-beet-accent/40 hover:text-beet-accent-text text-[12px] font-medium transition-colors duration-150 cursor-pointer",
                                            onclick: move |evt| {
                                                evt.stop_propagation();
                                                acquire_cb.call(());
                                            },
                                            IconPlus { size: 14 }
                                            span { "Acquire & Play" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    TrackRowState::OfflineUnavailable => {
                        rsx! {
                            span { class: "inline-flex items-center gap-1 text-[11px] tabular-nums-mono text-beet-text-disabled",
                                IconWifiOff { size: 13 }
                                span { class: "hidden sm:inline", "Not cached" }
                            }
                        }
                    }
                }

                // Duration in Commit Mono tabular-nums
                if let Some(secs) = duration_secs {
                    span { class: "w-11 text-right tabular-nums-mono text-[12px] text-beet-text-tertiary shrink-0",
                        "{format_duration(secs)}"
                    }
                }

                // Optional Track Inspector button
                if let Some(inspect_cb) = on_inspect {
                    button {
                        r#type: "button",
                        aria_label: format!("Inspect {title}"),
                        title: "Track inspector",
                        class: "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high transition-colors duration-150 cursor-pointer",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            inspect_cb.call(());
                        },
                        IconInfo { size: 16 }
                    }
                }
            }

            // Signature Soulbeet seam-light sweep while acquiring / importing / indexing
            if is_acquiring {
                div {
                    class: "beet-seam-light absolute inset-x-2 bottom-0 rounded-full pointer-events-none",
                    aria_hidden: "true",
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

    #[test]
    fn formats_durations_in_mm_ss_and_h_mm_ss() {
        assert_eq!(format_duration(0), "00:00");
        assert_eq!(format_duration(65), "01:05");
        assert_eq!(format_duration(222), "03:42");
        assert_eq!(format_duration(3661), "1:01:01");
    }

    #[test]
    fn formats_discovery_badges_consistently() {
        assert_eq!(format_discovery_badge(Some(9)), "DISC · 9D");
        assert_eq!(format_discovery_badge(Some(0)), "DISC · 0D");
        assert_eq!(format_discovery_badge(None), "DISC");
    }

    #[test]
    fn maps_acquire_and_play_stages_including_manual_choice() {
        let acq = AcquireStageBadge::from_stage(&AcquireAndPlayStage::Acquiring {
            backend_id: "slskd".into(),
            backend_name: "Soulseek".into(),
            source: None,
            quality: Some("FLAC".into()),
        })
        .unwrap();
        assert!(acq.is_in_flight());
        assert_eq!(acq.badge_label(), "ACQUIRING · Soulseek · FLAC");

        let imp = AcquireStageBadge::from_stage(&AcquireAndPlayStage::Importing).unwrap();
        assert!(imp.is_in_flight());
        assert_eq!(imp.badge_label(), "IMPORTING…");

        let idx = AcquireStageBadge::from_stage(&AcquireAndPlayStage::Indexing).unwrap();
        assert!(idx.is_in_flight());
        assert_eq!(idx.badge_label(), "INDEXING…");

        let manual = AcquireStageBadge::from_stage(&AcquireAndPlayStage::NeedsManualChoice {
            search_id: "s1".into(),
            results: vec![],
            best_score: 0.4,
            threshold: 0.7,
        })
        .unwrap();
        assert!(!manual.is_in_flight());
        assert!(matches!(
            manual,
            AcquireStageBadge::NeedsManualChoice { .. }
        ));
        assert_eq!(manual.badge_label(), "PICK SOURCE");

        let ready = AcquireStageBadge::from_stage(&AcquireAndPlayStage::Ready {
            navidrome_song_id: Some("song-1".into()),
            codec: Some("FLAC".into()),
            is_lossy: false,
            already_owned: false,
            backend_id: None,
        });
        assert!(ready.is_none());

        let failed = AcquireStageBadge::from_stage(&AcquireAndPlayStage::Failed {
            failure: Failure::new(
                FailureCode::InvalidInput,
                Stage::SourceSearch,
                RetryAdvice::OtherSource,
                "No peer answered",
            ),
        })
        .unwrap();
        assert!(!failed.is_in_flight());
        assert_eq!(failed.badge_label(), "RETRY");
    }

    #[test]
    fn computes_primary_action_enabled_strictly_per_state() {
        // UnownedCatalog never falls back to on_play
        assert!(!is_primary_action_enabled(
            &TrackRowState::UnownedCatalog {
                acquiring_stage: None
            },
            true,
            false,
            false
        ));
        // Acquiring disables primary click even if callbacks exist
        assert!(!is_primary_action_enabled(
            &TrackRowState::UnownedCatalog {
                acquiring_stage: Some(AcquireStageBadge::Importing)
            },
            true,
            true,
            true
        ));
        // NeedsManualChoice enables primary click only with on_pick_source
        assert!(!is_primary_action_enabled(
            &TrackRowState::UnownedCatalog {
                acquiring_stage: Some(AcquireStageBadge::NeedsManualChoice { message: None })
            },
            true,
            true,
            false
        ));
        assert!(is_primary_action_enabled(
            &TrackRowState::UnownedCatalog {
                acquiring_stage: Some(AcquireStageBadge::NeedsManualChoice { message: None })
            },
            false,
            false,
            true
        ));
        // OfflineUnavailable is always disabled
        assert!(!is_primary_action_enabled(
            &TrackRowState::OfflineUnavailable,
            true,
            true,
            true
        ));
    }

    #[test]
    fn renders_valid_listitem_markup_and_non_interactive_status_fallbacks() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                TrackRow {
                    title: "Rare Track".to_string(),
                    artist: "Artist".to_string(),
                    state: TrackRowState::UnownedCatalog {
                        acquiring_stage: Some(AcquireStageBadge::Failed {
                            message: Some("Timed out".to_string()),
                        }),
                    },
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("role=\"listitem\""), "{html}");
        assert!(!html.contains("title=\"\""), "{html}");
        assert!(html.contains(">FAILED</span>"), "{html}");
        // Verify no <div> nested inside the primary <button>
        let btn_start = html.find("<button").unwrap();
        let btn_end = html[btn_start..].find("</button>").unwrap() + btn_start;
        assert!(!html[btn_start..btn_end].contains("<div"), "{html}");

        let mut dom_pick = VirtualDom::new(|| {
            rsx! {
                TrackRow {
                    title: "Ambiguous Track".to_string(),
                    artist: "Artist".to_string(),
                    state: TrackRowState::UnownedCatalog {
                        acquiring_stage: Some(AcquireStageBadge::NeedsManualChoice {
                            message: None,
                        }),
                    },
                }
            }
        });
        dom_pick.rebuild_in_place();
        let html_pick = dioxus_ssr::render(&dom_pick);
        assert!(html_pick.contains("PICK SOURCE</span>"), "{html_pick}");
    }
}
