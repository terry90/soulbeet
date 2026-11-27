use dioxus::prelude::*;
use shared::recommendation::CultureSteering;

use crate::{
    mix_sleeve_theme, render_mix_sleeve_art, IconCloudCheck, IconDownload, IconPlay, IconSparkles,
};

/// Formats the cultural mix ratio pill (e.g. `45% FR · 55% INTL`).
pub fn format_culture_split(local_ratio: f32, local_label: &str) -> String {
    let clamped = if local_ratio.is_finite() {
        local_ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let local_pct = (clamped * 100.0).round() as u8;
    let intl_pct = 100u8.saturating_sub(local_pct);
    let label = local_label.trim();
    let code = if label.is_empty() {
        "LOCAL".to_string()
    } else {
        label.to_ascii_uppercase()
    };
    format!("{local_pct}% {code} · {intl_pct}% INTL")
}

/// Rich dark studio hero card for Seasonal & Contextual Discovery packs with adaptive
/// OLED-aware gradient scrim, live Culture Steering segmented chips, offline pin toggle,
/// and anti-taste-pollution disclosure pill.
#[component]
pub fn SeasonalHeroCard(
    title: String,
    subtitle: String,
    #[props(default)] window_badge: Option<String>,
    #[props(default)] artwork_url: Option<String>,
    #[props(default)] mix_slug: Option<String>,
    track_count: u32,
    /// Current effective local-culture share (`0.0..=1.0`).
    local_ratio: f32,
    /// Short region label shown in the ratio pill and steering chips (e.g. `"FR"` or `"LOCAL"`).
    #[props(default = "LOCAL".to_string())]
    local_label: String,
    #[props(default)] steering: CultureSteering,
    #[props(default)] is_pinned_offline: bool,
    #[props(default = true)] exclude_from_core_taste: bool,
    on_play: EventHandler<()>,
    #[props(default)] on_pin_offline: Option<EventHandler<()>>,
    #[props(default)] on_steer: Option<EventHandler<CultureSteering>>,
    #[props(default)] class: String,
) -> Element {
    let mut failed_art_url = use_signal(|| None::<String>);
    let hero_instance_id = use_hook(super::mix_sleeve_card::next_svg_instance_id);

    let show_art = artwork_url
        .as_ref()
        .filter(|url| !url.is_empty() && failed_art_url.read().as_ref() != Some(*url))
        .cloned();
    let procedural_slug = mix_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let procedural_theme = procedural_slug.as_deref().map(mix_sleeve_theme);
    let section_style = if show_art.is_none() {
        procedural_theme
            .map(|t| format!("border-color:{};", t.border))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let clean_code = local_label.trim().to_ascii_uppercase();
    let more_local_label = if clean_code.is_empty() || clean_code == "LOCAL" {
        "More Local".to_string()
    } else {
        format!("More {clean_code}")
    };

    let split_label = format_culture_split(local_ratio, &local_label);
    let track_word = if track_count == 1 { "TRACK" } else { "TRACKS" };

    let steering_options = [
        (CultureSteering::MoreLocal, more_local_label),
        (CultureSteering::Balanced, "Balanced".to_string()),
        (CultureSteering::MoreInternational, "More Intl".to_string()),
    ];

    rsx! {
        section {
            aria_label: "{title}",
            class: "relative overflow-hidden rounded-[16px] border border-beet-border-default bg-beet-surface min-h-[220px] {class}",
            style: "{section_style}",

            // Full-bleed artwork backdrop, procedural mix sleeve art, or subtle midnight-violet aura
            if let Some(url) = show_art {
                img {
                    src: "{url}",
                    alt: "",
                    aria_hidden: "true",
                    class: "absolute inset-0 w-full h-full object-cover",
                    loading: "lazy",
                    decoding: "async",
                    onerror: move |_| failed_art_url.set(Some(url.clone())),
                }
            } else if let (Some(slug), Some(theme)) = (procedural_slug.as_deref(), procedural_theme) {
                div {
                    class: "absolute inset-0 pointer-events-none",
                    aria_hidden: "true",
                    style: "background:{theme.background};",
                    {render_mix_sleeve_art(slug, &format!("hero-{hero_instance_id}-{slug}"))}
                }
            } else {
                div {
                    class: "absolute inset-0 pointer-events-none",
                    aria_hidden: "true",
                    style: "background-image: radial-gradient(circle at 82% 24%, rgba(168, 27, 168, 0.32), transparent 56%), radial-gradient(circle at 18% 85%, rgba(0, 229, 141, 0.16), transparent 52%);",
                }
            }

            // Adaptive OLED-aware dark gradient scrim preserving WCAG AAA readability
            div {
                class: "absolute inset-0 pointer-events-none",
                aria_hidden: "true",
                style: "background: linear-gradient(95deg, color-mix(in srgb, var(--beet-canvas) 94%, transparent) 0%, color-mix(in srgb, var(--beet-canvas) 80%, transparent) 56%, color-mix(in srgb, var(--beet-canvas) 48%, transparent) 100%), linear-gradient(0deg, color-mix(in srgb, var(--beet-canvas) 92%, transparent) 0%, transparent 55%);",
            }

            // Content layer
            div { class: "relative z-10 p-5 sm:p-6 flex flex-col justify-between gap-5",
                // Top metadata badges
                div { class: "flex flex-wrap items-center gap-2",
                    if let Some(badge) = window_badge.as_deref().filter(|b| !b.trim().is_empty()) {
                        span { class: "inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider",
                            IconSparkles { size: 12 }
                            "{badge}"
                        }
                    }
                    span { class: "inline-flex items-center px-2.5 py-1 rounded-full bg-beet-surface-high/90 border border-beet-border-subtle text-beet-text-secondary tabular-nums-mono text-[11px] font-semibold tracking-wider",
                        "{split_label}"
                    }
                    span { class: "inline-flex items-center px-2.5 py-1 rounded-full bg-beet-surface-high/80 border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[11px] font-semibold tracking-wider",
                        "{track_count} {track_word}"
                    }
                }

                // Display title & subtitle
                div { class: "max-w-2xl space-y-1.5",
                    h2 { class: "text-2xl sm:text-[32px] font-bold leading-[1.12] tracking-[-0.025em] text-beet-text-primary",
                        "{title}"
                    }
                    p { class: "text-sm text-beet-text-secondary leading-[1.45]",
                        "{subtitle}"
                    }
                }

                // Controls row: Play CTA, optional Pin Offline, and Culture Steering segmented chips
                div { class: "flex flex-wrap items-center justify-between gap-3 pt-1",
                    div { class: "flex flex-wrap items-center gap-2.5",
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[15px] font-semibold hover:brightness-105 active:scale-[0.98] transition-[filter,transform] duration-150 cursor-pointer",
                            onclick: move |_| on_play.call(()),
                            IconPlay { size: 16, filled: true }
                            span { "Play Mix" }
                        }

                        if let Some(pin_cb) = on_pin_offline {
                            button {
                                r#type: "button",
                                aria_pressed: if is_pinned_offline { "true" } else { "false" },
                                class: if is_pinned_offline {
                                    "inline-flex items-center gap-2 h-10 px-4 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/40 text-beet-leaf-text text-[13px] font-medium transition-colors duration-150 cursor-pointer"
                                } else {
                                    "inline-flex items-center gap-2 h-10 px-4 rounded-full bg-beet-surface-high/90 border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium transition-colors duration-150 cursor-pointer"
                                },
                                onclick: move |_| pin_cb.call(()),
                                if is_pinned_offline {
                                    IconCloudCheck { size: 16 }
                                    span { "Pinned Offline" }
                                } else {
                                    IconDownload { size: 16 }
                                    span { "Pin Offline" }
                                }
                            }
                        }
                    }

                    if let Some(steer_cb) = on_steer {
                        div {
                            role: "group",
                            aria_label: "Culture mix steering",
                            class: "inline-flex items-center gap-1 p-1 rounded-full bg-beet-sunken/80 border border-beet-border-subtle",
                            for (option, label) in steering_options {
                                {
                                    let active = steering == option;
                                    let chip_class = if active {
                                        "bg-beet-accent-surface text-beet-accent-text border-beet-accent/40"
                                    } else {
                                        "bg-transparent text-beet-text-secondary border-transparent hover:text-beet-text-primary hover:bg-beet-surface-high/70"
                                    };
                                    rsx! {
                                        button {
                                            key: "{option.as_str()}",
                                            r#type: "button",
                                            aria_pressed: if active { "true" } else { "false" },
                                            class: "h-8 px-3 rounded-full border text-[12px] font-medium transition-colors duration-150 cursor-pointer {chip_class}",
                                            onclick: move |_| steer_cb.call(option),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Anti-taste-pollution trust disclosure pill
                if exclude_from_core_taste {
                    div { class: "flex items-center gap-1.5 text-[12px] text-beet-text-tertiary",
                        IconSparkles { size: 13, class: "text-beet-accent-text".to_string() }
                        span { "Excluded from core taste until kept" }
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
    fn formats_culture_split_cleanly() {
        assert_eq!(format_culture_split(0.45, "fr"), "45% FR · 55% INTL");
        assert_eq!(format_culture_split(1.2, "jp"), "100% JP · 0% INTL");
        assert_eq!(format_culture_split(-0.5, ""), "0% LOCAL · 100% INTL");
        assert_eq!(format_culture_split(f32::NAN, "de"), "50% DE · 50% INTL");
    }

    #[test]
    fn renders_group_steering_chips_and_oled_aware_scrim() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                SeasonalHeroCard {
                    title: "Christmas Mix".to_string(),
                    subtitle: "Festive mix".to_string(),
                    track_count: 24,
                    local_ratio: 0.45,
                    local_label: "FR".to_string(),
                    steering: CultureSteering::Balanced,
                    on_play: |_| {},
                    on_steer: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("role=\"group\""), "{html}");
        assert!(html.contains("aria-pressed=\"true\""), "{html}");
        assert!(html.contains("var(--beet-canvas)"), "{html}");
        assert!(!html.contains("role=\"radiogroup\""), "{html}");
    }

    #[test]
    fn renders_procedural_mix_sleeve_art_when_mix_slug_provided_without_artwork() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                SeasonalHeroCard {
                    title: "Christmas Mix".to_string(),
                    subtitle: "Festive mix".to_string(),
                    mix_slug: Some("winter_holiday".to_string()),
                    track_count: 25,
                    local_ratio: 0.45,
                    local_label: "FR".to_string(),
                    on_play: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("id=\"ms-hero-"), "{html}");
        assert!(html.contains("-winter-holiday-wh-rg\""), "{html}");
        assert!(html.contains("linear-gradient(135deg, #2c1220"), "{html}");
    }
}
