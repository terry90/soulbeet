//! Procedural Midnight Studio mix sleeve cards and split-shelf layout.
//!
//! Renders atmospheric dark-studio gradients, subtle tinted borders, and restrained
//! inline SVG vector artwork for every built-in contextual, seasonal, rolling, and
//! smart library mix. Weather-reactive mixes omit raw temperature/forecast readouts
//! so their appearance feels ambient rather than like a weather widget.

use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;
use shared::recommendation::{ContextualMixKind, ContextualMixSummary};

use crate::{format_mix_subtitle, format_mix_title, IconPlay};

static NEXT_SVG_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_svg_instance_id() -> u64 {
    NEXT_SVG_INSTANCE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Split-shelf destination for curated mix cards on Discover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixShelfBucket {
    /// Live weather, time-of-day, day-of-week, Drive Mix, secondary seasonal, and custom mixes.
    RightNow,
    /// Smart library rules (Forgotten Gems, Fresh Arrivals), decade capsules, and rolling mixes.
    Collection,
}

/// Classifies a contextual mix into either the `RightNow` wide shelf or the `Collection`
/// square sleeve shelf.
pub fn classify_mix_shelf(summary: &ContextualMixSummary) -> MixShelfBucket {
    if summary.kind == ContextualMixKind::Rolling {
        return MixShelfBucket::Collection;
    }
    match normalize_slug(&summary.slug).as_str() {
        "forgotten_gems" | "fresh_arrivals" | "decade_80s" | "decade_90s" | "decade_2000s" => {
            MixShelfBucket::Collection
        }
        _ => MixShelfBucket::RightNow,
    }
}

/// Compact pill tag displayed in the top-left of a mix sleeve card.
///
/// Intentionally never renders temperature readouts or weather telemetry.
pub fn format_mix_sleeve_tag(summary: &ContextualMixSummary) -> &'static str {
    match normalize_slug(&summary.slug).as_str() {
        "decade_80s" => "1980S",
        "decade_90s" => "1990S",
        "decade_2000s" => "2000S",
        "forgotten_gems" => "VAULT",
        "fresh_arrivals" => "NEW",
        "friday_night" => "FRIDAY",
        "sunday_morning" => "SUNDAY",
        _ if summary.offline_priority => "OFFLINE PRIORITY",
        _ => match summary.kind {
            ContextualMixKind::Seasonal => "SEASONAL",
            ContextualMixKind::Rolling => "ROLLING",
            ContextualMixKind::Moment => "MOMENT",
        },
    }
}

/// Color tokens for a procedural Midnight Studio mix sleeve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixSleeveTheme {
    pub background: &'static str,
    pub border: &'static str,
    pub pill_bg: &'static str,
    pub pill_fg: &'static str,
}

/// Card geometry for [`MixSleeveCard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixSleeveLayout {
    Wide,
    Square,
}

fn normalize_slug(slug: &str) -> String {
    let lower = slug.trim().to_ascii_lowercase();
    lower
        .strip_prefix("mix.seasonal.")
        .or_else(|| lower.strip_prefix("mix.moment."))
        .or_else(|| lower.strip_prefix("mix.rolling."))
        .unwrap_or(&lower)
        .to_string()
}

/// Sanitizes an SVG `<defs>` ID prefix to ASCII alphanumeric characters and hyphens.
fn sanitize_svg_id_prefix(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "mix".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Returns the bespoke Midnight Studio surface theme for a mix slug.
pub fn mix_sleeve_theme(slug: &str) -> MixSleeveTheme {
    match normalize_slug(slug).as_str() {
        // --- Weather-reactive moments ---
        "rainy_day" => MixSleeveTheme {
            background: "linear-gradient(135deg, #0e213d 0%, #161836 52%, #110921 100%)",
            border: "rgba(125, 185, 255, 0.26)",
            pill_bg: "rgba(125, 185, 255, 0.14)",
            pill_fg: "#93c5fd",
        },
        "snow_day" => MixSleeveTheme {
            background: "linear-gradient(135deg, #15263a 0%, #1c1a38 56%, #130b24 100%)",
            border: "rgba(186, 230, 253, 0.28)",
            pill_bg: "rgba(224, 242, 254, 0.14)",
            pill_fg: "#bae6fd",
        },
        "storm_night" => MixSleeveTheme {
            background: "linear-gradient(150deg, #0d0a1f 0%, #19133a 48%, #0e172e 100%)",
            border: "rgba(167, 139, 250, 0.30)",
            pill_bg: "rgba(167, 139, 250, 0.16)",
            pill_fg: "#c4b5fd",
        },
        "sunny_day" => MixSleeveTheme {
            background: "linear-gradient(135deg, #3c1e14 0%, #2a1328 56%, #160922 100%)",
            border: "rgba(251, 191, 36, 0.30)",
            pill_bg: "rgba(251, 191, 36, 0.15)",
            pill_fg: "#fcd34d",
        },

        // --- Time-of-day, activity & weekday moments ---
        "drive" => MixSleeveTheme {
            background: "linear-gradient(135deg, #121c2e 0%, #1b0e30 58%, #120720 100%)",
            border: "rgba(51, 255, 176, 0.22)",
            pill_bg: "rgba(0, 255, 157, 0.12)",
            pill_fg: "#33ffb0",
        },
        "morning" => MixSleeveTheme {
            background: "linear-gradient(135deg, #341926 0%, #221030 56%, #120820 100%)",
            border: "rgba(253, 164, 175, 0.26)",
            pill_bg: "rgba(251, 146, 120, 0.14)",
            pill_fg: "#fda4af",
        },
        "focus" => MixSleeveTheme {
            background: "linear-gradient(135deg, #0f2034 0%, #151434 55%, #100820 100%)",
            border: "rgba(45, 212, 191, 0.24)",
            pill_bg: "rgba(45, 212, 191, 0.13)",
            pill_fg: "#5eead4",
        },
        "evening_unwind" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2d1426 0%, #1d0d2d 55%, #120720 100%)",
            border: "rgba(251, 146, 60, 0.24)",
            pill_bg: "rgba(251, 146, 60, 0.14)",
            pill_fg: "#fdba74",
        },
        "dinner_groove" => MixSleeveTheme {
            background: "linear-gradient(135deg, #341322 0%, #220e2e 56%, #130820 100%)",
            border: "rgba(245, 158, 11, 0.26)",
            pill_bg: "rgba(245, 158, 11, 0.14)",
            pill_fg: "#fbbf24",
        },
        "late_night" => MixSleeveTheme {
            background: "linear-gradient(145deg, #230d38 0%, #160a2c 52%, #0c051a 100%)",
            border: "rgba(192, 132, 252, 0.26)",
            pill_bg: "rgba(192, 132, 252, 0.14)",
            pill_fg: "#d8b4fe",
        },
        "friday_night" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2d0d3d 0%, #1d0c35 55%, #110722 100%)",
            border: "rgba(244, 114, 244, 0.28)",
            pill_bg: "rgba(255, 0, 255, 0.14)",
            pill_fg: "#f472f4",
        },
        "sunday_morning" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2e1b16 0%, #1e1129 58%, #120820 100%)",
            border: "rgba(251, 191, 120, 0.25)",
            pill_bg: "rgba(251, 191, 120, 0.14)",
            pill_fg: "#fed7aa",
        },

        // --- Smart library & decade sleeves ---
        "forgotten_gems" => MixSleeveTheme {
            background: "linear-gradient(145deg, #2d1e17 0%, #1c1028 64%, #140920 100%)",
            border: "rgba(255, 209, 102, 0.26)",
            pill_bg: "rgba(255, 209, 102, 0.14)",
            pill_fg: "#ffd166",
        },
        "fresh_arrivals" => MixSleeveTheme {
            background: "linear-gradient(145deg, #0d2825 0%, #15102b 64%, #110820 100%)",
            border: "rgba(51, 255, 176, 0.26)",
            pill_bg: "rgba(0, 255, 157, 0.14)",
            pill_fg: "#33ffb0",
        },
        "decade_80s" => MixSleeveTheme {
            background: "linear-gradient(145deg, #32103d 0%, #1a1038 56%, #0f162e 100%)",
            border: "rgba(244, 114, 244, 0.26)",
            pill_bg: "rgba(244, 114, 244, 0.14)",
            pill_fg: "#f472f4",
        },
        "decade_90s" => MixSleeveTheme {
            background: "linear-gradient(145deg, #2a181e 0%, #1d1228 62%, #13091f 100%)",
            border: "rgba(251, 113, 133, 0.26)",
            pill_bg: "rgba(251, 113, 133, 0.14)",
            pill_fg: "#fda4af",
        },
        "decade_2000s" => MixSleeveTheme {
            background: "linear-gradient(145deg, #13233e 0%, #1a1236 58%, #130922 100%)",
            border: "rgba(125, 211, 252, 0.26)",
            pill_bg: "rgba(56, 189, 248, 0.14)",
            pill_fg: "#7dd3fc",
        },

        // --- Rolling discovery profiles ---
        "conservative" | "comfort" => MixSleeveTheme {
            background: "linear-gradient(145deg, #122826 0%, #16112d 62%, #110820 100%)",
            border: "rgba(51, 255, 176, 0.25)",
            pill_bg: "rgba(0, 255, 157, 0.13)",
            pill_fg: "#33ffb0",
        },
        "balanced" | "weekly_radar" => MixSleeveTheme {
            background: "linear-gradient(145deg, #271238 0%, #190f32 58%, #101628 100%)",
            border: "rgba(244, 114, 244, 0.25)",
            pill_bg: "rgba(244, 114, 244, 0.14)",
            pill_fg: "#f472f4",
        },
        "adventurous" | "deep_cuts" => MixSleeveTheme {
            background: "linear-gradient(145deg, #2f1038 0%, #1c0c34 58%, #110722 100%)",
            border: "rgba(232, 121, 249, 0.28)",
            pill_bg: "rgba(232, 121, 249, 0.15)",
            pill_fg: "#f0abfc",
        },

        // --- Seasonal packs ---
        "winter_holiday" | "noel" | "christmas" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2c1220 0%, #181a2e 55%, #0f1920 100%)",
            border: "rgba(255, 209, 102, 0.28)",
            pill_bg: "rgba(255, 209, 102, 0.14)",
            pill_fg: "#ffd166",
        },
        "new_years_eve" | "nye" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2a1834 0%, #1a0f32 56%, #110720 100%)",
            border: "rgba(253, 224, 71, 0.28)",
            pill_bg: "rgba(253, 224, 71, 0.14)",
            pill_fg: "#fde047",
        },
        "cozy_winter" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2a191a 0%, #1a1530 56%, #110922 100%)",
            border: "rgba(251, 191, 146, 0.26)",
            pill_bg: "rgba(251, 191, 146, 0.14)",
            pill_fg: "#fdba74",
        },
        "lunar_new_year" => MixSleeveTheme {
            background: "linear-gradient(135deg, #3a111c 0%, #240e2a 58%, #13071e 100%)",
            border: "rgba(251, 191, 36, 0.30)",
            pill_bg: "rgba(251, 113, 133, 0.16)",
            pill_fg: "#fda4af",
        },
        "valentines" => MixSleeveTheme {
            background: "linear-gradient(135deg, #381028 0%, #230d2f 56%, #130720 100%)",
            border: "rgba(244, 114, 182, 0.28)",
            pill_bg: "rgba(244, 114, 182, 0.14)",
            pill_fg: "#f9a8d4",
        },
        "carnival" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2d1438 0%, #1b1838 52%, #0f2128 100%)",
            border: "rgba(250, 204, 21, 0.28)",
            pill_bg: "rgba(250, 204, 21, 0.14)",
            pill_fg: "#fde047",
        },
        "celtic_spring" => MixSleeveTheme {
            background: "linear-gradient(135deg, #0d2b22 0%, #131b30 56%, #100920 100%)",
            border: "rgba(52, 211, 153, 0.28)",
            pill_bg: "rgba(52, 211, 153, 0.14)",
            pill_fg: "#6ee7b7",
        },
        "spring" => MixSleeveTheme {
            background: "linear-gradient(135deg, #172a28 0%, #211433 56%, #130822 100%)",
            border: "rgba(110, 231, 183, 0.26)",
            pill_bg: "rgba(110, 231, 183, 0.14)",
            pill_fg: "#6ee7b7",
        },
        "summer" => MixSleeveTheme {
            background: "linear-gradient(135deg, #3a1d16 0%, #26122c 56%, #130922 100%)",
            border: "rgba(251, 146, 60, 0.28)",
            pill_bg: "rgba(251, 146, 60, 0.15)",
            pill_fg: "#fdba74",
        },
        "summer_solstice" => MixSleeveTheme {
            background: "linear-gradient(135deg, #3a2114 0%, #281332 54%, #140822 100%)",
            border: "rgba(253, 224, 71, 0.28)",
            pill_bg: "rgba(253, 224, 71, 0.14)",
            pill_fg: "#fde047",
        },
        "late_summer_reset" => MixSleeveTheme {
            background: "linear-gradient(135deg, #2f1728 0%, #1d1234 56%, #120820 100%)",
            border: "rgba(244, 114, 182, 0.25)",
            pill_bg: "rgba(232, 121, 249, 0.14)",
            pill_fg: "#f0abfc",
        },
        "autumn" => MixSleeveTheme {
            background: "linear-gradient(135deg, #331914 0%, #221128 56%, #130820 100%)",
            border: "rgba(245, 158, 11, 0.28)",
            pill_bg: "rgba(245, 158, 11, 0.14)",
            pill_fg: "#fbbf24",
        },
        "halloween" => MixSleeveTheme {
            background: "linear-gradient(135deg, #250e33 0%, #1a0b28 54%, #170d16 100%)",
            border: "rgba(249, 115, 22, 0.28)",
            pill_bg: "rgba(249, 115, 22, 0.15)",
            pill_fg: "#fb923c",
        },

        // --- Custom / fallback Midnight Studio radial wash ---
        _ => MixSleeveTheme {
            background: "linear-gradient(135deg, #24103a 0%, #190c2e 56%, #110720 100%)",
            border: "rgba(216, 194, 245, 0.22)",
            pill_bg: "rgba(244, 114, 244, 0.14)",
            pill_fg: "#f472f4",
        },
    }
}

/// Renders minimal Midnight Studio inline SVG artwork for a mix slug.
///
/// Every `<defs>` gradient/pattern/clipPath ID is scoped with `id_prefix` (sanitized to
/// ASCII alphanumeric + `-`) so multiple cards or hero+card on the same page never
/// collide on duplicate DOM SVG IDs.
pub fn render_mix_sleeve_art(slug: &str, id_prefix: &str) -> Element {
    let p = sanitize_svg_id_prefix(id_prefix);
    let key = normalize_slug(slug);

    match key.as_str() {
        "rainy_day" => {
            let rg = format!("ms-{p}-rain-rg");
            let fade = format!("ms-{p}-rain-fade");
            let rg_url = format!("url(#{rg})");
            let fade_url = format!("url(#{fade})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{rg}", cx: "76%", cy: "22%", r: "60%",
                            stop { offset: "0%", stop_color: "#60a5fa", stop_opacity: "0.24" }
                            stop { offset: "60%", stop_color: "#38bdf8", stop_opacity: "0.06" }
                            stop { offset: "100%", stop_color: "#60a5fa", stop_opacity: "0" }
                        }
                        linearGradient { id: "{fade}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#bae6fd", stop_opacity: "0" }
                            stop { offset: "55%", stop_color: "#93c5fd", stop_opacity: "0.52" }
                            stop { offset: "100%", stop_color: "#e0f2fe", stop_opacity: "0.85" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{rg_url}" }
                    ellipse { cx: "322", cy: "178", rx: "44", ry: "7", fill: "none", stroke: "#93c5fd", stroke_opacity: "0.16", stroke_width: "1" }
                    ellipse { cx: "322", cy: "178", rx: "22", ry: "3.5", fill: "none", stroke: "#bae6fd", stroke_opacity: "0.22", stroke_width: "1" }
                    g { stroke: "{fade_url}", stroke_linecap: "round",
                        line { x1: "252", y1: "8", x2: "228", y2: "74", stroke_width: "1.4", opacity: "0.55" }
                        line { x1: "286", y1: "22", x2: "256", y2: "108", stroke_width: "1.2", opacity: "0.48" }
                        line { x1: "316", y1: "-4", x2: "292", y2: "68", stroke_width: "1.6", opacity: "0.62" }
                        line { x1: "348", y1: "30", x2: "318", y2: "120", stroke_width: "1.3", opacity: "0.55" }
                        line { x1: "378", y1: "10", x2: "352", y2: "86", stroke_width: "1.5", opacity: "0.52" }
                        line { x1: "226", y1: "88", x2: "206", y2: "148", stroke_width: "1.0", opacity: "0.35" }
                        line { x1: "298", y1: "98", x2: "274", y2: "168", stroke_width: "1.2", opacity: "0.45" }
                        line { x1: "364", y1: "92", x2: "338", y2: "168", stroke_width: "1.3", opacity: "0.42" }
                    }
                }
            }
        }

        "snow_day" => {
            let sg = format!("ms-{p}-snow-rg");
            let sg_url = format!("url(#{sg})");
            rsx! {
                svg {
                    view_box: "0 0 320 180",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sg}", cx: "76%", cy: "24%", r: "62%",
                            stop { offset: "0%", stop_color: "#e0f2fe", stop_opacity: "0.24" }
                            stop { offset: "55%", stop_color: "#7dd3fc", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#7dd3fc", stop_opacity: "0" }
                        }
                    }
                    rect { width: "320", height: "180", fill: "{sg_url}" }
                    g { stroke: "#e0f2fe", stroke_linecap: "round", fill: "none",
                        g { transform: "translate(238,54) scale(0.95)", opacity: "0.42", stroke_width: "1.2",
                            line { x1: "0", y1: "-14", x2: "0", y2: "14" }
                            line { x1: "-12.1", y1: "-7", x2: "12.1", y2: "7" }
                            line { x1: "-12.1", y1: "7", x2: "12.1", y2: "-7" }
                            path { d: "M-3 -10 L0 -7 L3 -10 M-3 10 L0 7 L3 10", stroke_width: "1" }
                        }
                        g { transform: "translate(278,108) scale(0.68)", opacity: "0.34", stroke_width: "1.3",
                            line { x1: "0", y1: "-13", x2: "0", y2: "13" }
                            line { x1: "-11.3", y1: "-6.5", x2: "11.3", y2: "6.5" }
                            line { x1: "-11.3", y1: "6.5", x2: "11.3", y2: "-6.5" }
                        }
                        g { transform: "translate(206,118) scale(0.45)", opacity: "0.26", stroke_width: "1.4",
                            line { x1: "0", y1: "-12", x2: "0", y2: "12" }
                            line { x1: "-10.4", y1: "-6", x2: "10.4", y2: "6" }
                            line { x1: "-10.4", y1: "6", x2: "10.4", y2: "-6" }
                        }
                    }
                    g { fill: "#f0f9ff",
                        circle { cx: "212", cy: "30", r: "2.2", opacity: "0.42" }
                        circle { cx: "268", cy: "28", r: "1.6", opacity: "0.35" }
                        circle { cx: "294", cy: "68", r: "2.8", opacity: "0.28" }
                        circle { cx: "246", cy: "96", r: "1.8", opacity: "0.32" }
                        circle { cx: "256", cy: "144", r: "2.4", opacity: "0.22" }
                        circle { cx: "192", cy: "76", r: "1.5", opacity: "0.24" }
                    }
                }
            }
        }

        "storm_night" => {
            let st1 = format!("ms-{p}-storm-g1");
            let st2 = format!("ms-{p}-storm-g2");
            let bolt = format!("ms-{p}-storm-bolt");
            let st1_url = format!("url(#{st1})");
            let st2_url = format!("url(#{st2})");
            let bolt_url = format!("url(#{bolt})");
            rsx! {
                svg {
                    view_box: "0 0 320 180",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{st1}", cx: "72%", cy: "38%", r: "54%",
                            stop { offset: "0%", stop_color: "#8b5cf6", stop_opacity: "0.34" }
                            stop { offset: "50%", stop_color: "#38bdf8", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                        radialGradient { id: "{st2}", cx: "88%", cy: "8%", r: "48%",
                            stop { offset: "0%", stop_color: "#090514", stop_opacity: "0.72" }
                            stop { offset: "100%", stop_color: "#090514", stop_opacity: "0" }
                        }
                        linearGradient { id: "{bolt}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#e0e7ff", stop_opacity: "0.12" }
                            stop { offset: "22%", stop_color: "#f5f3ff", stop_opacity: "0.78" }
                            stop { offset: "72%", stop_color: "#a78bfa", stop_opacity: "0.48" }
                            stop { offset: "100%", stop_color: "#38bdf8", stop_opacity: "0" }
                        }
                    }
                    rect { width: "320", height: "180", fill: "{st1_url}" }
                    rect { width: "320", height: "180", fill: "{st2_url}" }
                    path {
                        d: "M242 20 L220 74 L240 74 L210 154",
                        fill: "none",
                        stroke: "#a78bfa",
                        stroke_width: "11",
                        stroke_linejoin: "round",
                        stroke_linecap: "round",
                        opacity: "0.11",
                    }
                    path {
                        d: "M242 20 L220 74 L240 74 L210 154",
                        fill: "none",
                        stroke: "#c4b5fd",
                        stroke_width: "4.5",
                        stroke_linejoin: "round",
                        stroke_linecap: "round",
                        opacity: "0.20",
                    }
                    path {
                        d: "M242 20 L220 74 L240 74 L210 154",
                        fill: "none",
                        stroke: "{bolt_url}",
                        stroke_width: "1.8",
                        stroke_linejoin: "round",
                        stroke_linecap: "round",
                    }
                }
            }
        }

        "sunny_day" => {
            let core = format!("ms-{p}-sun-core");
            let rays = format!("ms-{p}-sun-rays");
            let core_url = format!("url(#{core})");
            let rays_url = format!("url(#{rays})");
            rsx! {
                svg {
                    view_box: "0 0 320 180",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{core}", cx: "76%", cy: "38%", r: "64%",
                            stop { offset: "0%", stop_color: "#fde68a", stop_opacity: "0.40" }
                            stop { offset: "30%", stop_color: "#fbbf24", stop_opacity: "0.22" }
                            stop { offset: "65%", stop_color: "#fb923c", stop_opacity: "0.09" }
                            stop { offset: "100%", stop_color: "#fb923c", stop_opacity: "0" }
                        }
                        linearGradient { id: "{rays}", x1: "1", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0.46" }
                            stop { offset: "100%", stop_color: "#fb923c", stop_opacity: "0" }
                        }
                    }
                    rect { width: "320", height: "180", fill: "{core_url}" }
                    g { fill: "{rays_url}",
                        polygon { points: "236,64 128,18 144,4 238,60", opacity: "0.26" }
                        polygon { points: "236,66 112,72 116,54 236,63", opacity: "0.34" }
                        polygon { points: "236,68 134,126 124,108 235,66", opacity: "0.36" }
                        polygon { points: "238,70 176,176 158,164 236,68", opacity: "0.32" }
                        polygon { points: "240,70 232,180 212,180 238,69", opacity: "0.24" }
                        polygon { points: "242,68 286,180 266,180 240,68", opacity: "0.20" }
                    }
                    circle { cx: "238", cy: "66", r: "18", fill: "#fef08a", opacity: "0.20" }
                    circle { cx: "238", cy: "66", r: "10", fill: "#fde68a", opacity: "0.34" }
                }
            }
        }

        "drive" => {
            let drg = format!("ms-{p}-drive-rg");
            let drg_url = format!("url(#{drg})");
            rsx! {
                svg {
                    view_box: "0 0 400 220",
                    preserve_aspect_ratio: "none",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{drg}", cx: "78%", cy: "48%", r: "55%",
                            stop { offset: "0%", stop_color: "#00ff9d", stop_opacity: "0.14" }
                            stop { offset: "50%", stop_color: "#ff00ff", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "220", fill: "{drg_url}" }
                    g { stroke: "#33ffb0", stroke_opacity: "0.18", stroke_width: "1",
                        line { x1: "180", y1: "125", x2: "400", y2: "125" }
                        line { x1: "295", y1: "125", x2: "195", y2: "220" }
                        line { x1: "295", y1: "125", x2: "265", y2: "220" }
                        line { x1: "295", y1: "125", x2: "345", y2: "220" }
                        line { x1: "295", y1: "125", x2: "420", y2: "220" }
                        line { x1: "230", y1: "152", x2: "400", y2: "152", opacity: "0.55" }
                        line { x1: "205", y1: "184", x2: "400", y2: "184", opacity: "0.4" }
                    }
                }
            }
        }

        "evening_unwind" => {
            let evg = format!("ms-{p}-ev-rg");
            let band = format!("ms-{p}-ev-band");
            let evg_url = format!("url(#{evg})");
            let band_url = format!("url(#{band})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{evg}", cx: "78%", cy: "46%", r: "58%",
                            stop { offset: "0%", stop_color: "#f97316", stop_opacity: "0.24" }
                            stop { offset: "48%", stop_color: "#e879f9", stop_opacity: "0.09" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                        linearGradient { id: "{band}", x1: "0", y1: "0", x2: "1", y2: "0",
                            stop { offset: "0%", stop_color: "#fdba74", stop_opacity: "0" }
                            stop { offset: "55%", stop_color: "#fdba74", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#f472b6", stop_opacity: "0.06" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{evg_url}" }
                    path { d: "M312 62 A24 24 0 1 0 338 94 A19 19 0 1 1 312 62 Z", fill: "#fed7aa", opacity: "0.28" }
                    g { fill: "#fde68a",
                        circle { cx: "268", cy: "54", r: "1.4", opacity: "0.45" }
                        circle { cx: "352", cy: "118", r: "1.2", opacity: "0.35" }
                        circle { cx: "284", cy: "126", r: "1.1", opacity: "0.30" }
                        path { d: "M250 86 l1.4 3.4 3.4 1.4-3.4 1.4-1.4 3.4-1.4-3.4-3.4-1.4 3.4-1.4Z", opacity: "0.38" }
                    }
                    rect { x: "210", y: "150", width: "170", height: "1.2", rx: "1", fill: "{band_url}" }
                    rect { x: "242", y: "164", width: "138", height: "1.2", rx: "1", fill: "{band_url}", opacity: "0.72" }
                    rect { x: "276", y: "178", width: "104", height: "1.2", rx: "1", fill: "{band_url}", opacity: "0.48" }
                }
            }
        }

        // A3-2: Pure 4-Point Starlight Glints (no squares, no disco ball)
        "friday_night" => {
            let sbase = format!("ms-{p}-fri-base");
            let sbase_url = format!("url(#{sbase})");
            rsx! {
                svg {
                    view_box: "0 0 400 184",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sbase}", cx: "84%", cy: "32%", r: "60%",
                            stop { offset: "0%", stop_color: "#f472f4", stop_opacity: "0.30" }
                            stop { offset: "52%", stop_color: "#38bdf8", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "184", fill: "{sbase_url}" }
                    path { d: "M318 76 l2.8 8.2 8.2 2.8-8.2 2.8-2.8 8.2-2.8-8.2-8.2-2.8 8.2-2.8Z", fill: "#f5d0fe", opacity: "0.62" }
                    path { d: "M276 112 l1.8 5.2 5.2 1.8-5.2 1.8-1.8 5.2-1.8-5.2-5.2-1.8 5.2-1.8Z", fill: "#33ffb0", opacity: "0.46" }
                    path { d: "M352 124 l1.6 4.4 4.4 1.6-4.4 1.6-1.6 4.4-1.6-4.4-4.4-1.6 4.4-1.6Z", fill: "#f472f4", opacity: "0.52" }
                    path { d: "M286 58 l1.1 3.0 3.0 1.1-3.0 1.1-1.1 3.0-1.1-3.0-3.0-1.1 3.0-1.1Z", fill: "#7dd3fc", opacity: "0.44" }
                    circle { cx: "338", cy: "60", r: "1.4", fill: "#ffffff", opacity: "0.45" }
                    circle { cx: "308", cy: "142", r: "1.3", fill: "#f5d0fe", opacity: "0.38" }
                    circle { cx: "252", cy: "86", r: "1.2", fill: "#f472f4", opacity: "0.34" }
                }
            }
        }

        "forgotten_gems" => {
            let fgg = format!("ms-{p}-fg-rg");
            let fgg_url = format!("url(#{fgg})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{fgg}", cx: "84%", cy: "32%", r: "64%",
                            stop { offset: "0%", stop_color: "#ffd166", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#ffd166", stop_opacity: "0" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{fgg_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke: "#ffd166",
                        circle { cx: "222", cy: "86", r: "24", stroke_opacity: "0.26", stroke_width: "1.2" }
                        circle { cx: "222", cy: "86", r: "40", stroke_opacity: "0.20", stroke_width: "1" }
                        circle { cx: "222", cy: "86", r: "56", stroke_opacity: "0.16", stroke_width: "1" }
                        circle { cx: "222", cy: "86", r: "72", stroke_opacity: "0.12", stroke_width: "0.9" }
                        circle { cx: "222", cy: "86", r: "88", stroke_opacity: "0.08", stroke_width: "0.9" }
                        circle { cx: "222", cy: "86", r: "10", fill: "#ffd166", fill_opacity: "0.22", stroke_opacity: "0.35" }
                        circle { cx: "222", cy: "86", r: "2.8", fill: "#1c1028", stroke: "none" }
                    }
                }
            }
        }

        "fresh_arrivals" => {
            let fag = format!("ms-{p}-fa-rg");
            let fag_url = format!("url(#{fag})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{fag}", cx: "80%", cy: "32%", r: "64%",
                            stop { offset: "0%", stop_color: "#00ff9d", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#00ff9d", stop_opacity: "0" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{fag_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "#33ffb0", opacity: "0.28",
                        rect { x: "132", y: "78", width: "4", height: "22", rx: "2" }
                        rect { x: "142", y: "66", width: "4", height: "46", rx: "2" }
                        rect { x: "152", y: "82", width: "4", height: "14", rx: "2" }
                        rect { x: "162", y: "56", width: "4", height: "66", rx: "2" }
                        rect { x: "172", y: "70", width: "4", height: "38", rx: "2" }
                        rect { x: "182", y: "52", width: "4", height: "74", rx: "2" }
                        rect { x: "192", y: "68", width: "4", height: "42", rx: "2" }
                        rect { x: "202", y: "60", width: "4", height: "58", rx: "2" }
                        rect { x: "212", y: "74", width: "4", height: "30", rx: "2" }
                        rect { x: "222", y: "80", width: "4", height: "18", rx: "2" }
                    }
                }
            }
        }

        "decade_80s" => {
            let g80 = format!("ms-{p}-d80-lg");
            let g80_url = format!("url(#{g80})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{g80}", x1: "0", y1: "0", x2: "1", y2: "1",
                            stop { offset: "0%", stop_color: "#ff00ff", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#22d3ee", stop_opacity: "0.16" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{g80_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { stroke: "#22d3ee", stroke_opacity: "0.22", stroke_width: "1",
                        line { x1: "0", y1: "196", x2: "260", y2: "196" }
                        line { x1: "0", y1: "216", x2: "260", y2: "216" }
                        line { x1: "0", y1: "238", x2: "260", y2: "238" }
                    }
                }
                div {
                    "aria-hidden": "true",
                    style: "position:absolute;right:12px;top:56px;font-family:var(--beet-font-display);font-weight:700;font-size:68px;line-height:1;letter-spacing:-0.045em;font-variant-numeric:lining-nums tabular-nums;pointer-events:none;user-select:none;color:rgba(244,114,244,0.20);",
                    "80s"
                }
            }
        }

        "decade_90s" => {
            let dots = format!("ms-{p}-d90-dots");
            let dots_url = format!("url(#{dots})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        pattern { id: "{dots}", width: "11", height: "11", pattern_units: "userSpaceOnUse",
                            circle { cx: "4", cy: "4", r: "1.15", fill: "#fda4af", fill_opacity: "0.16" }
                        }
                    }
                    rect { x: "96", y: "0", width: "164", height: "156", fill: "{dots_url}" }
                }
                div {
                    "aria-hidden": "true",
                    style: "position:absolute;right:12px;top:56px;font-family:var(--beet-font-display);font-weight:700;font-size:68px;line-height:1;letter-spacing:-0.045em;font-variant-numeric:lining-nums tabular-nums;pointer-events:none;user-select:none;color:rgba(253,164,175,0.20);",
                    "90s"
                }
            }
        }

        "decade_2000s" => {
            let g00 = format!("ms-{p}-d00-lg");
            let g00_url = format!("url(#{g00})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{g00}", x1: "0", y1: "0", x2: "1", y2: "1",
                            stop { offset: "0%", stop_color: "#38bdf8", stop_opacity: "0.24" }
                            stop { offset: "52%", stop_color: "#c084fc", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    polygon { points: "88,0 260,0 260,172", fill: "{g00_url}" }
                }
                div {
                    "aria-hidden": "true",
                    style: "position:absolute;right:12px;top:56px;font-family:var(--beet-font-display);font-weight:700;font-size:68px;line-height:1;letter-spacing:-0.045em;font-variant-numeric:lining-nums tabular-nums;pointer-events:none;user-select:none;color:rgba(125,211,252,0.20);",
                    "00s"
                }
            }
        }

        // --- Additional Time-of-Day & Activity Moments ---
        "morning" => {
            let mg = format!("ms-{p}-morn-rg");
            let mg_url = format!("url(#{mg})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{mg}", cx: "78%", cy: "76%", r: "56%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.30" }
                            stop { offset: "45%", stop_color: "#fda4af", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{mg_url}" }
                    path { d: "M272 152 A40 40 0 0 1 352 152", fill: "#fed7aa", opacity: "0.18" }
                    g { stroke: "#fda4af", stroke_linecap: "round", stroke_width: "1.1", opacity: "0.34",
                        line { x1: "234", y1: "152", x2: "390", y2: "152" }
                        line { x1: "312", y1: "100", x2: "312", y2: "68" }
                        line { x1: "282", y1: "110", x2: "260", y2: "86" }
                        line { x1: "342", y1: "110", x2: "364", y2: "86" }
                        line { x1: "266", y1: "132", x2: "238", y2: "120" }
                        line { x1: "358", y1: "132", x2: "386", y2: "120" }
                    }
                }
            }
        }

        "focus" => {
            let fg = format!("ms-{p}-focus-rg");
            let pat = format!("ms-{p}-focus-grid");
            let fg_url = format!("url(#{fg})");
            let pat_url = format!("url(#{pat})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{fg}", cx: "76%", cy: "54%", r: "52%",
                            stop { offset: "0%", stop_color: "#2dd4bf", stop_opacity: "0.22" }
                            stop { offset: "55%", stop_color: "#38bdf8", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                        pattern { id: "{pat}", width: "22", height: "22", pattern_units: "userSpaceOnUse",
                            path { d: "M 22 0 L 0 0 0 22", fill: "none", stroke: "#5eead4", stroke_opacity: "0.10", stroke_width: "0.8" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{fg_url}" }
                    rect { x: "200", y: "0", width: "200", height: "200", fill: "{pat_url}" }
                    g { fill: "none", stroke: "#5eead4",
                        circle { cx: "312", cy: "112", r: "34", stroke_opacity: "0.24", stroke_width: "1" }
                        circle { cx: "312", cy: "112", r: "18", stroke_opacity: "0.34", stroke_width: "1" }
                    }
                    circle { cx: "312", cy: "112", r: "4", fill: "#5eead4", opacity: "0.62" }
                }
            }
        }

        "dinner_groove" => {
            let dg = format!("ms-{p}-din-rg");
            let dg_url = format!("url(#{dg})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{dg}", cx: "78%", cy: "56%", r: "56%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.26" }
                            stop { offset: "52%", stop_color: "#fb7185", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{dg_url}" }
                    g { fill: "none", stroke: "#fbbf24",
                        circle { cx: "320", cy: "116", r: "26", stroke_opacity: "0.30", stroke_width: "1.1" }
                        circle { cx: "320", cy: "116", r: "46", stroke_opacity: "0.20", stroke_width: "1" }
                        circle { cx: "320", cy: "116", r: "68", stroke_opacity: "0.12", stroke_width: "0.9" }
                    }
                    g { fill: "none", stroke: "#fda4af",
                        circle { cx: "272", cy: "134", r: "36", stroke_opacity: "0.18", stroke_width: "1" }
                    }
                    circle { cx: "320", cy: "116", r: "5", fill: "#fde68a", opacity: "0.42" }
                }
            }
        }

        // Inspired by B from generate_friday.py: Nocturnal City Skyline & Neon Glow
        "late_night" => {
            let sky = format!("ms-{p}-ln-sky");
            let bldg = format!("ms-{p}-ln-bldg");
            let sky_url = format!("url(#{sky})");
            let bldg_url = format!("url(#{bldg})");
            rsx! {
                svg {
                    view_box: "0 0 600 184",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sky}", cx: "76%", cy: "82%", r: "55%",
                            stop { offset: "0%", stop_color: "#ff00ff", stop_opacity: "0.30" }
                            stop { offset: "48%", stop_color: "#38bdf8", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                        linearGradient { id: "{bldg}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#2a1246", stop_opacity: "0.88" }
                            stop { offset: "100%", stop_color: "#110621", stop_opacity: "0.95" }
                        }
                    }
                    rect { width: "600", height: "184", fill: "{sky_url}" }
                    g { fill: "#220f3b", opacity: "0.65",
                        rect { x: "330", y: "106", width: "24", height: "78" }
                        rect { x: "386", y: "76", width: "28", height: "108" }
                        rect { x: "442", y: "64", width: "32", height: "120" }
                        rect { x: "498", y: "88", width: "26", height: "96" }
                        rect { x: "546", y: "72", width: "34", height: "112" }
                    }
                    g { fill: "{bldg_url}", stroke: "rgba(244,114,244,0.22)", stroke_width: "0.9",
                        rect { x: "352", y: "122", width: "28", height: "62" }
                        rect { x: "404", y: "92", width: "34", height: "92" }
                        rect { x: "466", y: "78", width: "36", height: "106" }
                        rect { x: "518", y: "104", width: "30", height: "80" }
                    }
                    line { x1: "484", y1: "54", x2: "484", y2: "78", stroke: "#f472f4", stroke_opacity: "0.45", stroke_width: "1.2" }
                    circle { cx: "484", cy: "54", r: "2", fill: "#ff00ff", opacity: "0.8" }
                    g {
                        rect { x: "411", y: "102", width: "5", height: "3", fill: "#fde68a", opacity: "0.65" }
                        rect { x: "421", y: "102", width: "5", height: "3", fill: "#f472f4", opacity: "0.55" }
                        rect { x: "411", y: "112", width: "5", height: "3", fill: "#fde68a", opacity: "0.45" }
                        rect { x: "421", y: "122", width: "5", height: "3", fill: "#33ffb0", opacity: "0.65" }
                        rect { x: "411", y: "136", width: "5", height: "3", fill: "#fde68a", opacity: "0.55" }
                        rect { x: "474", y: "90", width: "6", height: "3", fill: "#33ffb0", opacity: "0.65" }
                        rect { x: "486", y: "90", width: "6", height: "3", fill: "#fde68a", opacity: "0.55" }
                        rect { x: "474", y: "102", width: "6", height: "3", fill: "#f472f4", opacity: "0.65" }
                        rect { x: "486", y: "114", width: "6", height: "3", fill: "#fde68a", opacity: "0.70" }
                        rect { x: "474", y: "128", width: "6", height: "3", fill: "#38bdf8", opacity: "0.55" }
                        rect { x: "486", y: "142", width: "6", height: "3", fill: "#f472f4", opacity: "0.50" }
                        rect { x: "525", y: "114", width: "5", height: "3", fill: "#fde68a", opacity: "0.60" }
                        rect { x: "535", y: "126", width: "5", height: "3", fill: "#33ffb0", opacity: "0.55" }
                        rect { x: "360", y: "134", width: "5", height: "3", fill: "#f472f4", opacity: "0.50" }
                        rect { x: "360", y: "148", width: "5", height: "3", fill: "#fde68a", opacity: "0.50" }
                    }
                }
            }
        }

        "sunday_morning" => {
            let smg = format!("ms-{p}-sunm-rg");
            let steam = format!("ms-{p}-sunm-steam");
            let smg_url = format!("url(#{smg})");
            let steam_url = format!("url(#{steam})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{smg}", cx: "76%", cy: "52%", r: "56%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.24" }
                            stop { offset: "55%", stop_color: "#fed7aa", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                        linearGradient { id: "{steam}", x1: "0", y1: "1", x2: "0", y2: "0",
                            stop { offset: "0%", stop_color: "#fed7aa", stop_opacity: "0.42" }
                            stop { offset: "100%", stop_color: "#fed7aa", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{smg_url}" }
                    g { fill: "none", stroke: "{steam_url}", stroke_width: "1.5", stroke_linecap: "round",
                        path { d: "M294 148 C286 126 304 108 294 84 C288 70 296 58 294 46" }
                        path { d: "M314 144 C306 122 324 102 314 78 C308 64 316 52 314 38", opacity: "0.85" }
                        path { d: "M334 150 C326 130 342 112 334 90 C328 76 336 66 334 56", opacity: "0.65" }
                    }
                    ellipse { cx: "314", cy: "162", rx: "38", ry: "7", fill: "none", stroke: "#fed7aa", stroke_opacity: "0.22", stroke_width: "1.1" }
                }
            }
        }

        // --- Rolling Discovery Mixes ---
        "conservative" | "comfort" => {
            let cg = format!("ms-{p}-cons-rg");
            let cg_url = format!("url(#{cg})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cg}", cx: "80%", cy: "32%", r: "62%",
                            stop { offset: "0%", stop_color: "#33ffb0", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{cg_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke: "#33ffb0", stroke_linecap: "round",
                        path { d: "M104 92 Q154 64 204 92 T260 92", stroke_opacity: "0.30", stroke_width: "1.3" }
                        path { d: "M92 108 Q148 80 204 108 T260 108", stroke_opacity: "0.22", stroke_width: "1.1" }
                        path { d: "M116 124 Q166 98 216 124 T260 124", stroke_opacity: "0.14", stroke_width: "1" }
                    }
                }
            }
        }

        "balanced" | "weekly_radar" => {
            let bg = format!("ms-{p}-bal-rg");
            let bg_url = format!("url(#{bg})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{bg}", cx: "78%", cy: "34%", r: "62%",
                            stop { offset: "0%", stop_color: "#f472f4", stop_opacity: "0.24" }
                            stop { offset: "55%", stop_color: "#33ffb0", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{bg_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke: "#f472f4",
                        circle { cx: "204", cy: "94", r: "22", stroke_opacity: "0.30", stroke_width: "1.1" }
                        circle { cx: "204", cy: "94", r: "42", stroke_opacity: "0.20", stroke_width: "1", stroke_dasharray: "18 10" }
                        circle { cx: "204", cy: "94", r: "64", stroke_opacity: "0.13", stroke_width: "0.9" }
                    }
                    line { x1: "204", y1: "94", x2: "242", y2: "68", stroke: "#33ffb0", stroke_opacity: "0.42", stroke_width: "1.2", stroke_linecap: "round" }
                    circle { cx: "204", cy: "94", r: "3.2", fill: "#33ffb0", opacity: "0.65" }
                }
            }
        }

        "adventurous" | "deep_cuts" => {
            let ag = format!("ms-{p}-adv-rg");
            let ag_url = format!("url(#{ag})");
            rsx! {
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{ag}", cx: "80%", cy: "34%", r: "60%",
                            stop { offset: "0%", stop_color: "#e879f9", stop_opacity: "0.26" }
                            stop { offset: "55%", stop_color: "#6366f1", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "260", height: "260", fill: "{ag_url}" }
                }
                svg {
                    view_box: "0 0 260 260",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke: "#f0abfc",
                        circle { cx: "202", cy: "94", r: "48", stroke_opacity: "0.20", stroke_width: "1", stroke_dasharray: "6 6" }
                        circle { cx: "202", cy: "94", r: "28", stroke_opacity: "0.28", stroke_width: "1" }
                    }
                    path { d: "M202 68 L206 90 L228 94 L206 98 L202 120 L198 98 L176 94 L198 90 Z", fill: "#f0abfc", opacity: "0.34" }
                }
            }
        }

        // --- Seasonal Mixes ---
        "winter_holiday" | "noel" | "christmas" => {
            let wh = format!("ms-{p}-wh-rg");
            let wh_url = format!("url(#{wh})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{wh}", cx: "78%", cy: "38%", r: "58%",
                            stop { offset: "0%", stop_color: "#ffd166", stop_opacity: "0.26" }
                            stop { offset: "52%", stop_color: "#34d399", stop_opacity: "0.09" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{wh_url}" }
                    g { stroke: "#ffd166", stroke_linecap: "round", fill: "none",
                        line { x1: "312", y1: "48", x2: "312", y2: "104", stroke_opacity: "0.42", stroke_width: "1.2" }
                        line { x1: "284", y1: "76", x2: "340", y2: "76", stroke_opacity: "0.42", stroke_width: "1.2" }
                        line { x1: "294", y1: "58", x2: "330", y2: "94", stroke_opacity: "0.26", stroke_width: "1" }
                        line { x1: "330", y1: "58", x2: "294", y2: "94", stroke_opacity: "0.26", stroke_width: "1" }
                    }
                    path { d: "M270 156 L312 118 L354 156 M284 176 L312 148 L340 176", fill: "none", stroke: "#6ee7b7", stroke_opacity: "0.22", stroke_width: "1.1", stroke_linecap: "round", stroke_linejoin: "round" }
                    circle { cx: "312", cy: "76", r: "3.5", fill: "#fef08a", opacity: "0.70" }
                }
            }
        }

        "new_years_eve" | "nye" => {
            let nye = format!("ms-{p}-nye-rg");
            let nye_url = format!("url(#{nye})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{nye}", cx: "78%", cy: "42%", r: "56%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.28" }
                            stop { offset: "50%", stop_color: "#f472f4", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{nye_url}" }
                    g { stroke: "#fde047", stroke_linecap: "round", stroke_width: "1.1", opacity: "0.34",
                        line { x1: "316", y1: "92", x2: "316", y2: "46" }
                        line { x1: "316", y1: "92", x2: "354", y2: "64" }
                        line { x1: "316", y1: "92", x2: "360", y2: "104" }
                        line { x1: "316", y1: "92", x2: "340", y2: "136" }
                        line { x1: "316", y1: "92", x2: "278", y2: "126" }
                        line { x1: "316", y1: "92", x2: "272", y2: "80" }
                    }
                    circle { cx: "256", cy: "148", r: "1.8", fill: "#fde047", opacity: "0.45" }
                    circle { cx: "286", cy: "54", r: "1.5", fill: "#f472f4", opacity: "0.50" }
                    circle { cx: "368", cy: "132", r: "2", fill: "#33ffb0", opacity: "0.40" }
                }
            }
        }

        "cozy_winter" => {
            let cw = format!("ms-{p}-cw-rg");
            let cw_url = format!("url(#{cw})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cw}", cx: "76%", cy: "62%", r: "58%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.26" }
                            stop { offset: "52%", stop_color: "#93c5fd", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{cw_url}" }
                    g { fill: "none", stroke: "#fdba74",
                        path { d: "M246 176 A66 66 0 0 1 378 176", stroke_opacity: "0.24", stroke_width: "1.2" }
                        path { d: "M268 176 A44 44 0 0 1 356 176", stroke_opacity: "0.30", stroke_width: "1.2" }
                        path { d: "M290 176 A22 22 0 0 1 334 176", stroke_opacity: "0.38", stroke_width: "1.2" }
                    }
                    g { stroke: "#bae6fd", stroke_linecap: "round", stroke_width: "1.1", opacity: "0.34",
                        line { x1: "272", y1: "54", x2: "272", y2: "70" }
                        line { x1: "265", y1: "58", x2: "279", y2: "66" }
                        line { x1: "265", y1: "66", x2: "279", y2: "58" }
                    }
                }
            }
        }

        "lunar_new_year" => {
            let lny = format!("ms-{p}-lny-rg");
            let lny_url = format!("url(#{lny})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{lny}", cx: "78%", cy: "48%", r: "56%",
                            stop { offset: "0%", stop_color: "#fb7185", stop_opacity: "0.28" }
                            stop { offset: "50%", stop_color: "#fbbf24", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{lny_url}" }
                    line { x1: "314", y1: "0", x2: "314", y2: "56", stroke: "#fbbf24", stroke_opacity: "0.36", stroke_width: "1.1" }
                    g { fill: "none", stroke: "#fbbf24",
                        ellipse { cx: "314", cy: "98", rx: "34", ry: "38", stroke_opacity: "0.30", stroke_width: "1.2" }
                        ellipse { cx: "314", cy: "98", rx: "18", ry: "38", stroke_opacity: "0.22", stroke_width: "1" }
                        line { x1: "314", y1: "60", x2: "314", y2: "136", stroke_opacity: "0.22", stroke_width: "1" }
                    }
                    line { x1: "314", y1: "138", x2: "314", y2: "168", stroke: "#fda4af", stroke_opacity: "0.36", stroke_width: "1.2" }
                }
            }
        }

        "valentines" => {
            let val = format!("ms-{p}-val-rg");
            let val_url = format!("url(#{val})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{val}", cx: "78%", cy: "48%", r: "56%",
                            stop { offset: "0%", stop_color: "#f472b6", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#c084fc", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{val_url}" }
                    g { fill: "none", stroke: "#f9a8d4",
                        circle { cx: "294", cy: "104", r: "34", stroke_opacity: "0.28", stroke_width: "1.1" }
                        circle { cx: "330", cy: "104", r: "34", stroke_opacity: "0.22", stroke_width: "1.1" }
                    }
                    circle { cx: "262", cy: "62", r: "2", fill: "#fbcfe8", opacity: "0.42" }
                    circle { cx: "354", cy: "148", r: "1.6", fill: "#f472b6", opacity: "0.45" }
                }
            }
        }

        "carnival" => {
            let crn = format!("ms-{p}-crn-rg");
            let crn_url = format!("url(#{crn})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{crn}", cx: "80%", cy: "44%", r: "58%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.24" }
                            stop { offset: "48%", stop_color: "#33ffb0", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{crn_url}" }
                    path { d: "M212 164 C264 106 318 134 392 54", fill: "none", stroke: "#fde047", stroke_opacity: "0.34", stroke_width: "1.4" }
                    path { d: "M232 180 C286 126 336 150 404 78", fill: "none", stroke: "#33ffb0", stroke_opacity: "0.28", stroke_width: "1.2" }
                    polygon { points: "312,62 320,72 312,82 304,72", fill: "#f472f4", opacity: "0.42" }
                    polygon { points: "276,104 282,111 276,118 270,111", fill: "#fde047", opacity: "0.46" }
                }
            }
        }

        "celtic_spring" => {
            let cs = format!("ms-{p}-celt-rg");
            let cs_url = format!("url(#{cs})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cs}", cx: "78%", cy: "50%", r: "56%",
                            stop { offset: "0%", stop_color: "#34d399", stop_opacity: "0.26" }
                            stop { offset: "55%", stop_color: "#10b981", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{cs_url}" }
                    g { fill: "none", stroke: "#6ee7b7", stroke_width: "1.2",
                        circle { cx: "312", cy: "82", r: "22", stroke_opacity: "0.30" }
                        circle { cx: "294", cy: "114", r: "22", stroke_opacity: "0.26" }
                        circle { cx: "330", cy: "114", r: "22", stroke_opacity: "0.26" }
                    }
                }
            }
        }

        "spring" => {
            let sp = format!("ms-{p}-spr-rg");
            let sp_url = format!("url(#{sp})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sp}", cx: "78%", cy: "44%", r: "58%",
                            stop { offset: "0%", stop_color: "#6ee7b7", stop_opacity: "0.24" }
                            stop { offset: "52%", stop_color: "#f9a8d4", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{sp_url}" }
                    g { fill: "none", stroke: "#6ee7b7", stroke_width: "1.1",
                        path { d: "M272 152 C288 110 318 82 362 66", stroke_opacity: "0.30" }
                        path { d: "M304 114 C308 92 324 80 342 82 C338 100 324 112 304 114 Z", stroke_opacity: "0.26" }
                        path { d: "M318 122 C338 118 352 128 354 144 C336 146 322 138 318 122 Z", stroke_opacity: "0.22" }
                    }
                    circle { cx: "282", cy: "76", r: "1.8", fill: "#f9a8d4", opacity: "0.45" }
                    circle { cx: "354", cy: "104", r: "1.5", fill: "#6ee7b7", opacity: "0.45" }
                }
            }
        }

        "summer" => {
            let sm = format!("ms-{p}-sum-rg");
            let sm_url = format!("url(#{sm})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sm}", cx: "78%", cy: "42%", r: "58%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.30" }
                            stop { offset: "50%", stop_color: "#fde047", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{sm_url}" }
                    circle { cx: "314", cy: "88", r: "26", fill: "#fdba74", opacity: "0.20" }
                    g { fill: "none", stroke: "#fdba74", stroke_linecap: "round",
                        path { d: "M220 140 Q262 128 304 140 T388 140", stroke_opacity: "0.28", stroke_width: "1.2" }
                        path { d: "M236 158 Q276 146 316 158 T396 158", stroke_opacity: "0.20", stroke_width: "1.1" }
                    }
                }
            }
        }

        "summer_solstice" => {
            let ss = format!("ms-{p}-sol-rg");
            let ss_url = format!("url(#{ss})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{ss}", cx: "78%", cy: "44%", r: "58%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.30" }
                            stop { offset: "48%", stop_color: "#f472b6", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{ss_url}" }
                    g { fill: "none", stroke: "#fde047",
                        circle { cx: "312", cy: "98", r: "22", stroke_opacity: "0.34", stroke_width: "1.2" }
                        circle { cx: "312", cy: "98", r: "42", stroke_opacity: "0.22", stroke_width: "1" }
                        line { x1: "252", y1: "98", x2: "372", y2: "98", stroke_opacity: "0.20", stroke_width: "1" }
                        line { x1: "312", y1: "38", x2: "312", y2: "158", stroke_opacity: "0.20", stroke_width: "1" }
                    }
                    circle { cx: "312", cy: "98", r: "8", fill: "#fef08a", opacity: "0.34" }
                }
            }
        }

        "late_summer_reset" => {
            let lsr = format!("ms-{p}-lsr-rg");
            let lsr_url = format!("url(#{lsr})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{lsr}", cx: "78%", cy: "46%", r: "56%",
                            stop { offset: "0%", stop_color: "#f0abfc", stop_opacity: "0.24" }
                            stop { offset: "50%", stop_color: "#fb923c", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{lsr_url}" }
                    path { d: "M272 132 A40 40 0 0 1 352 132", fill: "none", stroke: "#f0abfc", stroke_opacity: "0.28", stroke_width: "1.2" }
                    g { fill: "none", stroke: "#f0abfc", stroke_linecap: "round",
                        line { x1: "230", y1: "136", x2: "388", y2: "136", stroke_opacity: "0.24", stroke_width: "1.1" }
                        line { x1: "252", y1: "152", x2: "384", y2: "152", stroke_opacity: "0.18", stroke_width: "1" }
                        line { x1: "276", y1: "168", x2: "378", y2: "168", stroke_opacity: "0.12", stroke_width: "1" }
                    }
                }
            }
        }

        "autumn" => {
            let aut = format!("ms-{p}-aut-rg");
            let aut_url = format!("url(#{aut})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{aut}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.26" }
                            stop { offset: "52%", stop_color: "#ea580c", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{aut_url}" }
                    g { fill: "none", stroke: "#fbbf24", stroke_linecap: "round",
                        path { d: "M232 146 C276 122 316 140 374 108", stroke_opacity: "0.24", stroke_width: "1.1" }
                        path { d: "M296 76 C318 68 334 78 338 96 C318 102 302 94 296 76 Z", stroke_opacity: "0.32", stroke_width: "1.1" }
                        path { d: "M264 114 C278 108 290 114 292 126 C278 130 268 124 264 114 Z", stroke_opacity: "0.24", stroke_width: "1" }
                    }
                }
            }
        }

        "halloween" => {
            let hal = format!("ms-{p}-hal-rg");
            let hal_url = format!("url(#{hal})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{hal}", cx: "78%", cy: "42%", r: "56%",
                            stop { offset: "0%", stop_color: "#f97316", stop_opacity: "0.28" }
                            stop { offset: "50%", stop_color: "#a855f7", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{hal_url}" }
                    circle { cx: "316", cy: "82", r: "28", fill: "none", stroke: "#fb923c", stroke_opacity: "0.34", stroke_width: "1.3" }
                    circle { cx: "322", cy: "80", r: "24", fill: "#1a0b28", opacity: "0.78" }
                    polyline { points: "224,168 268,132 306,154 348,122 396,168", fill: "none", stroke: "#c084fc", stroke_opacity: "0.22", stroke_width: "1.1", stroke_linejoin: "round" }
                }
            }
        }

        // --- Default fallback: Midnight Studio violet-magenta radial wash ---
        _ => {
            let def = format!("ms-{p}-def-rg");
            let def_url = format!("url(#{def})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "xMaxYMid slice",
                    class: "w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{def}", cx: "80%", cy: "36%", r: "60%",
                            stop { offset: "0%", stop_color: "#f472f4", stop_opacity: "0.24" }
                            stop { offset: "52%", stop_color: "#a855f7", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{def_url}" }
                    g { fill: "none", stroke: "#f472f4",
                        circle { cx: "324", cy: "98", r: "34", stroke_opacity: "0.18", stroke_width: "1" }
                        circle { cx: "324", cy: "98", r: "60", stroke_opacity: "0.11", stroke_width: "0.9" }
                    }
                }
            }
        }
    }
}

/// Procedural dark-studio sleeve card for contextual, smart library, decade, and rolling mixes.
#[component]
pub fn MixSleeveCard(
    mix: ContextualMixSummary,
    layout: MixSleeveLayout,
    #[props(default)] id_prefix: String,
    #[props(default)] show_owned_pct: bool,
    on_play: EventHandler<String>,
) -> Element {
    let instance_id = use_hook(next_svg_instance_id);
    let theme = mix_sleeve_theme(&mix.slug);
    let tag = format_mix_sleeve_tag(&mix);
    let title = format_mix_title(&mix);
    let subtitle = format_mix_subtitle(&mix);
    let track_word = if mix.track_count == 1 {
        "TRACK"
    } else {
        "TRACKS"
    };
    let base_prefix = if id_prefix.trim().is_empty() {
        "card"
    } else {
        id_prefix.trim()
    };
    let art_prefix = format!("{base_prefix}-{instance_id}-{}", mix.slug);
    let slug_for_click = mix.slug.clone();

    let (card_padding, card_extra_style, play_size, play_icon_size, scrim_gradient) = match layout {
        MixSleeveLayout::Wide => (
            "p-5",
            "min-height:172px;",
            "w-[34px] h-[34px]",
            13u32,
            "linear-gradient(95deg, rgba(11, 4, 20, 0.56) 0%, rgba(11, 4, 20, 0.24) 56%, rgba(11, 4, 20, 0.04) 100%)",
        ),
        MixSleeveLayout::Square => (
            "p-4",
            "aspect-ratio:1/1.08;",
            "w-[30px] h-[30px]",
            11u32,
            "linear-gradient(180deg, rgba(11, 4, 20, 0.08) 0%, rgba(11, 4, 20, 0.22) 48%, rgba(11, 4, 20, 0.68) 100%)",
        ),
    };

    rsx! {
        button {
            r#type: "button",
            class: "group relative overflow-hidden rounded-[14px] border text-left flex flex-col justify-between transition-[transform,filter] duration-150 hover:-translate-y-0.5 hover:brightness-110 active:scale-[0.99] cursor-pointer {card_padding}",
            style: "background:{theme.background};border-color:{theme.border};{card_extra_style}",
            onclick: move |_| on_play.call(slug_for_click.clone()),

            // Procedural vector sleeve layer
            div {
                class: "absolute inset-0 pointer-events-none",
                "aria-hidden": "true",
                {render_mix_sleeve_art(&mix.slug, &art_prefix)}
            }

            // Subtle directional contrast scrim preserving readability over bright sleeves
            div {
                class: "absolute inset-0 pointer-events-none",
                aria_hidden: "true",
                style: "background:{scrim_gradient};",
            }

            // Foreground copy & play trigger
            div { class: "relative z-10 flex flex-col justify-between h-full w-full",
                div { class: "flex items-center justify-between gap-3",
                    span {
                        class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-[0.06em] px-2.5 py-[3px] rounded-full inline-flex items-center border",
                        style: "background:{theme.pill_bg};color:{theme.pill_fg};border-color:{theme.border};",
                        "{tag}"
                    }
                    span {
                        class: "{play_size} rounded-full bg-beet-leaf-solid text-beet-leaf-ink inline-flex items-center justify-center shrink-0 group-hover:scale-105 transition-transform duration-150",
                        style: "box-shadow:0 4px 14px rgba(0,229,141,0.24);",
                        IconPlay { size: play_icon_size, filled: true, class: "ml-0.5".to_string() }
                    }
                }

                if layout == MixSleeveLayout::Wide {
                    div { class: "mt-4 max-w-[78%]",
                        h3 { class: "text-[20px] sm:text-[21px] font-bold tracking-[-0.02em] leading-[1.2] text-beet-text-primary",
                            "{title}"
                        }
                        p {
                            class: "text-[13px] leading-[1.42] mt-1 line-clamp-2",
                            style: "color:rgba(235,228,246,0.80);",
                            "{subtitle}"
                        }
                    }
                    div {
                        class: "mt-3.5 flex items-center gap-1.5 tabular-nums-mono text-[11px] tracking-[0.04em]",
                        style: "color:rgba(216,194,245,0.68);",
                        span { "{mix.track_count} {track_word}" }
                        if show_owned_pct && mix.owned_pct > 0 {
                            span { "· {mix.owned_pct}% LIBRARY" }
                        }
                    }
                } else {
                    div { class: "mt-auto pt-11",
                        h3 { class: "text-[17px] font-bold tracking-[-0.02em] leading-[1.2] text-beet-text-primary",
                            "{title}"
                        }
                        p {
                            class: "text-[12px] leading-[1.42] mt-1 line-clamp-2",
                            style: "color:rgba(235,228,246,0.78);",
                            "{subtitle}"
                        }
                        div {
                            class: "mt-2.5 flex items-center gap-1.5 tabular-nums-mono text-[11px] tracking-[0.04em]",
                            style: "color:rgba(216,194,245,0.68);",
                            span { "{mix.track_count} {track_word}" }
                            if show_owned_pct && mix.owned_pct > 0 {
                                span { "· {mix.owned_pct}% LIBRARY" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Split-shelf layout separating live moment mixes ("Right Now", wide 2-col) from
/// smart library, decade, and rolling sleeves ("From Your Collection", square 5-col).
#[component]
pub fn MixSleeveShelves(
    mixes: Vec<ContextualMixSummary>,
    #[props(default = "disc".to_string())] id_prefix: String,
    on_play: EventHandler<String>,
) -> Element {
    let mut right_now = Vec::new();
    let mut collection = Vec::new();

    for mix in mixes {
        match classify_mix_shelf(&mix) {
            MixShelfBucket::RightNow => right_now.push(mix),
            MixShelfBucket::Collection => collection.push(mix),
        }
    }

    if right_now.is_empty() && collection.is_empty() {
        return rsx! {};
    }

    rsx! {
        div { class: "space-y-7",
            if !right_now.is_empty() {
                section { class: "space-y-3.5",
                    div { class: "flex flex-wrap items-baseline justify-between gap-2",
                        h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "Right Now"
                        }
                        span { class: "tabular-nums-mono text-[11px] uppercase tracking-[0.06em] text-beet-text-tertiary",
                            "LIVE WEATHER & TIME OF DAY"
                        }
                    }
                    div { class: "grid grid-cols-1 md:grid-cols-2 gap-4",
                        for (idx, mix) in right_now.into_iter().enumerate() {
                            MixSleeveCard {
                                key: "{id_prefix}-rn-{idx}-{mix.slug}",
                                mix,
                                layout: MixSleeveLayout::Wide,
                                id_prefix: format!("{id_prefix}-rn-{idx}"),
                                on_play,
                            }
                        }
                    }
                }
            }

            if !collection.is_empty() {
                section { class: "space-y-3.5",
                    div { class: "flex flex-wrap items-baseline justify-between gap-2",
                        h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                            "From Your Collection"
                        }
                        span { class: "tabular-nums-mono text-[11px] uppercase tracking-[0.06em] text-beet-text-tertiary",
                            "SMART LIBRARY & DECADE SLEEVES"
                        }
                    }
                    div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3.5",
                        for (idx, mix) in collection.into_iter().enumerate() {
                            MixSleeveCard {
                                key: "{id_prefix}-col-{idx}-{mix.slug}",
                                mix,
                                layout: MixSleeveLayout::Square,
                                id_prefix: format!("{id_prefix}-col-{idx}"),
                                on_play,
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
    use shared::recommendation::{builtin_contextual_mixes, WeatherCondition, WeatherSnapshot};
    use std::collections::HashSet;

    fn make_summary(
        slug: &str,
        kind: ContextualMixKind,
        offline_priority: bool,
    ) -> ContextualMixSummary {
        ContextualMixSummary {
            slug: slug.to_string(),
            kind,
            i18n_key: format!("mix.{}.{}", kind.as_str(), slug),
            custom_name: None,
            custom_description: None,
            built_in: true,
            country_code: None,
            local_pct: None,
            owned_pct: 75,
            offline_priority,
            exclude_from_core_taste: kind == ContextualMixKind::Seasonal,
            steering: None,
            active_from: None,
            active_until: None,
            expires_at: None,
            navidrome_playlist_id: None,
            weather_badge: None,
            track_count: 25,
            target_tracks: 25,
        }
    }

    fn extract_html_ids(html: &str) -> Vec<String> {
        let mut ids = Vec::new();
        let mut rest = html;
        while let Some(pos) = rest.find("id=\"") {
            let after = &rest[pos + 4..];
            if let Some(end) = after.find('"') {
                ids.push(after[..end].to_string());
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
        ids
    }

    #[test]
    fn classifies_right_now_vs_collection_shelves() {
        for slug in [
            "rainy_day",
            "snow_day",
            "storm_night",
            "sunny_day",
            "morning",
            "focus",
            "evening_unwind",
            "dinner_groove",
            "late_night",
            "friday_night",
            "sunday_morning",
            "drive",
            "custom_vibe",
        ] {
            let s = make_summary(slug, ContextualMixKind::Moment, slug == "drive");
            assert_eq!(
                classify_mix_shelf(&s),
                MixShelfBucket::RightNow,
                "slug={slug}"
            );
        }

        let secondary_seasonal = make_summary("cozy_winter", ContextualMixKind::Seasonal, false);
        assert_eq!(
            classify_mix_shelf(&secondary_seasonal),
            MixShelfBucket::RightNow
        );

        for slug in [
            "forgotten_gems",
            "fresh_arrivals",
            "decade_80s",
            "decade_90s",
            "decade_2000s",
        ] {
            let s = make_summary(slug, ContextualMixKind::Moment, false);
            assert_eq!(
                classify_mix_shelf(&s),
                MixShelfBucket::Collection,
                "slug={slug}"
            );
        }

        for slug in [
            "conservative",
            "balanced",
            "adventurous",
            "comfort",
            "weekly_radar",
            "deep_cuts",
        ] {
            let s = make_summary(slug, ContextualMixKind::Rolling, false);
            assert_eq!(
                classify_mix_shelf(&s),
                MixShelfBucket::Collection,
                "rolling slug={slug}"
            );
        }
    }

    #[test]
    fn sleeve_tags_never_include_weather_telemetry() {
        for (slug, cond, temp) in [
            ("rainy_day", WeatherCondition::Rainy, 11.4),
            ("snow_day", WeatherCondition::Snowy, -2.5),
            ("storm_night", WeatherCondition::Stormy, 17.0),
            ("sunny_day", WeatherCondition::Sunny, 27.3),
        ] {
            let mut s = make_summary(slug, ContextualMixKind::Moment, false);
            s.weather_badge = Some(WeatherSnapshot {
                condition: cond,
                temperature_c: temp,
                weather_code: 61,
                is_day: true,
                location_label: Some("Lyon, FR".into()),
            });
            let tag = format_mix_sleeve_tag(&s);
            assert_eq!(tag, "MOMENT");
            assert!(!tag.contains('°'));
            assert!(!tag.contains("RAIN"));
            assert!(!tag.contains("SNOW"));
            assert!(!tag.contains("STORM"));
            assert!(!tag.contains("SUN"));
        }

        assert_eq!(
            format_mix_sleeve_tag(&make_summary("decade_80s", ContextualMixKind::Moment, false)),
            "1980S"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary("decade_90s", ContextualMixKind::Moment, false)),
            "1990S"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "decade_2000s",
                ContextualMixKind::Moment,
                false
            )),
            "2000S"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "forgotten_gems",
                ContextualMixKind::Moment,
                false
            )),
            "VAULT"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "fresh_arrivals",
                ContextualMixKind::Moment,
                false
            )),
            "NEW"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "friday_night",
                ContextualMixKind::Moment,
                false
            )),
            "FRIDAY"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "sunday_morning",
                ContextualMixKind::Moment,
                false
            )),
            "SUNDAY"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary("drive", ContextualMixKind::Moment, true)),
            "OFFLINE PRIORITY"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary(
                "winter_holiday",
                ContextualMixKind::Seasonal,
                false
            )),
            "SEASONAL"
        );
        assert_eq!(
            format_mix_sleeve_tag(&make_summary("balanced", ContextualMixKind::Rolling, false)),
            "ROLLING"
        );
    }

    #[test]
    fn renders_all_builtin_and_rolling_sleeves_with_unique_svg_ids() {
        let mut mixes: Vec<ContextualMixSummary> = builtin_contextual_mixes()
            .into_iter()
            .map(|def| make_summary(&def.slug, def.kind, def.offline_priority))
            .collect();

        for rolling_slug in ["conservative", "balanced", "adventurous"] {
            mixes.push(make_summary(rolling_slug, ContextualMixKind::Rolling, false));
        }

        // Verify all themes resolve to non-empty styling
        for m in &mixes {
            let theme = mix_sleeve_theme(&m.slug);
            assert!(!theme.background.is_empty());
            assert!(!theme.border.is_empty());
            assert!(!theme.pill_bg.is_empty());
            assert!(!theme.pill_fg.is_empty());
        }

        let mut dom = VirtualDom::new_with_props(
            |mixes: Vec<ContextualMixSummary>| {
                rsx! {
                    MixSleeveShelves {
                        mixes,
                        id_prefix: "test".to_string(),
                        on_play: |_| {},
                    }
                }
            },
            mixes,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);

        assert!(html.contains("Right Now"));
        assert!(html.contains("From Your Collection"));
        assert!(html.contains("80s"));
        assert!(html.contains("90s"));
        assert!(html.contains("00s"));

        let ids = extract_html_ids(&html);
        assert!(!ids.is_empty(), "expected SVG <defs> IDs in rendered HTML");
        let unique: HashSet<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "found duplicate SVG IDs in rendered shelves: {ids:?}"
        );
    }

    #[test]
    fn colliding_slugs_and_repeated_heroes_produce_zero_duplicate_svg_ids() {
        let mix_a = make_summary("my_mix", ContextualMixKind::Moment, false);
        let mix_b = make_summary("my-mix", ContextualMixKind::Moment, false);
        let dup_a = make_summary("rainy_day", ContextualMixKind::Moment, false);
        let dup_b = make_summary("rainy_day", ContextualMixKind::Moment, false);

        let mut dom = VirtualDom::new_with_props(
            |(mix_a, mix_b, dup_a, dup_b): (
                ContextualMixSummary,
                ContextualMixSummary,
                ContextualMixSummary,
                ContextualMixSummary,
            )| {
                rsx! {
                    div {
                        MixSleeveCard {
                            mix: mix_a,
                            layout: MixSleeveLayout::Wide,
                            on_play: |_| {},
                        }
                        MixSleeveCard {
                            mix: mix_b,
                            layout: MixSleeveLayout::Square,
                            on_play: |_| {},
                        }
                        MixSleeveCard {
                            mix: dup_a,
                            layout: MixSleeveLayout::Wide,
                            id_prefix: "same".to_string(),
                            on_play: |_| {},
                        }
                        MixSleeveCard {
                            mix: dup_b,
                            layout: MixSleeveLayout::Wide,
                            id_prefix: "same".to_string(),
                            on_play: |_| {},
                        }
                        crate::SeasonalHeroCard {
                            title: "Christmas Mix A".to_string(),
                            subtitle: "Festive mix".to_string(),
                            mix_slug: Some("winter_holiday".to_string()),
                            track_count: 25,
                            local_ratio: 0.45,
                            local_label: "FR".to_string(),
                            on_play: |_| {},
                        }
                        crate::SeasonalHeroCard {
                            title: "Christmas Mix B".to_string(),
                            subtitle: "Festive mix".to_string(),
                            mix_slug: Some("winter_holiday".to_string()),
                            track_count: 25,
                            local_ratio: 0.45,
                            local_label: "FR".to_string(),
                            on_play: |_| {},
                        }
                    }
                }
            },
            (mix_a, mix_b, dup_a, dup_b),
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);

        let ids = extract_html_ids(&html);
        assert!(
            ids.len() >= 8,
            "expected at least 8 SVG <defs> IDs across 4 cards and 2 heroes, got {ids:?}"
        );
        let unique: HashSet<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "found duplicate SVG IDs across colliding slugs / heroes: {ids:?}"
        );

        // Verify directional contrast scrims for both Wide and Square layouts
        assert!(html.contains("rgba(11, 4, 20, 0.56)"), "{html}");
        assert!(html.contains("rgba(11, 4, 20, 0.68)"), "{html}");
    }

    #[test]
    fn collection_and_rolling_260x260_motifs_use_meet_without_vertical_cropping() {
        for slug in [
            "forgotten_gems",
            "fresh_arrivals",
            "decade_80s",
            "decade_90s",
            "decade_2000s",
            "conservative",
            "balanced",
            "adventurous",
        ] {
            let slug_owned = slug.to_string();
            let mut dom = VirtualDom::new_with_props(
                |s: String| render_mix_sleeve_art(&s, "aspect"),
                slug_owned,
            );
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);

            assert!(
                html.contains("preserveAspectRatio=\"xMaxYMid meet\""),
                "expected xMaxYMid meet for 260x260 motif `{slug}`: {html}"
            );
            assert!(
                !html.contains("preserveAspectRatio=\"xMaxYMid slice\""),
                "260x260 motif `{slug}` must not use slice: {html}"
            );
        }
    }
}
