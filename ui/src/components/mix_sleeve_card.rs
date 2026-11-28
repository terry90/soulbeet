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
            let sun = format!("ms-{p}-morn-sun");
            let bar = format!("ms-{p}-morn-bar");
            let mg_url = format!("url(#{mg})");
            let sun_url = format!("url(#{sun})");
            let bar_url = format!("url(#{bar})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{mg}", cx: "80%", cy: "62%", r: "58%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.28" }
                            stop { offset: "48%", stop_color: "#fda4af", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{mg_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{sun}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#fde68a", stop_opacity: "0.38" }
                            stop { offset: "100%", stop_color: "#fda4af", stop_opacity: "0.08" }
                        }
                        linearGradient { id: "{bar}", x1: "0", y1: "0", x2: "1", y2: "0",
                            stop { offset: "0%", stop_color: "#fda4af", stop_opacity: "0" }
                            stop { offset: "50%", stop_color: "#fed7aa", stop_opacity: "0.46" }
                            stop { offset: "100%", stop_color: "#fda4af", stop_opacity: "0.08" }
                        }
                    }
                    path { d: "M110 114 A42 42 0 0 1 194 114 Z", fill: "{sun_url}" }
                    path {
                        d: "M96 114 A56 56 0 0 1 208 114",
                        fill: "none",
                        stroke: "#fda4af",
                        stroke_opacity: "0.24",
                        stroke_width: "1",
                    }
                    rect { x: "42", y: "114", width: "178", height: "1.2", rx: "0.6", fill: "{bar_url}" }
                    rect { x: "86", y: "128", width: "130", height: "1.1", rx: "0.5", fill: "{bar_url}", opacity: "0.78" }
                    rect { x: "112", y: "142", width: "92", height: "1.0", rx: "0.5", fill: "{bar_url}", opacity: "0.52" }
                    rect { x: "134", y: "156", width: "54", height: "0.9", rx: "0.5", fill: "{bar_url}", opacity: "0.32" }
                }
            }
        }

        "focus" => {
            let fg = format!("ms-{p}-focus-rg");
            let fg_url = format!("url(#{fg})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{fg}", cx: "78%", cy: "50%", r: "56%",
                            stop { offset: "0%", stop_color: "#2dd4bf", stop_opacity: "0.24" }
                            stop { offset: "55%", stop_color: "#38bdf8", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{fg_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "#5eead4", opacity: "0.18",
                        circle { cx: "102", cy: "52", r: "1.1" }
                        circle { cx: "126", cy: "52", r: "1.1" }
                        circle { cx: "174", cy: "52", r: "1.1" }
                        circle { cx: "198", cy: "52", r: "1.1" }
                        circle { cx: "102", cy: "76", r: "1.1" }
                        circle { cx: "198", cy: "76", r: "1.1" }
                        circle { cx: "102", cy: "124", r: "1.1" }
                        circle { cx: "198", cy: "124", r: "1.1" }
                        circle { cx: "102", cy: "148", r: "1.1" }
                        circle { cx: "126", cy: "148", r: "1.1" }
                        circle { cx: "174", cy: "148", r: "1.1" }
                        circle { cx: "198", cy: "148", r: "1.1" }
                    }
                    g { fill: "none", stroke: "#5eead4",
                        circle { cx: "150", cy: "100", r: "48", stroke_opacity: "0.22", stroke_width: "1" }
                        circle { cx: "150", cy: "100", r: "26", stroke_opacity: "0.36", stroke_width: "1.1" }
                        line { x1: "150", y1: "38", x2: "150", y2: "46", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "150", y1: "154", x2: "150", y2: "162", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "88", y1: "100", x2: "96", y2: "100", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "204", y1: "100", x2: "212", y2: "100", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                    }
                    circle { cx: "150", cy: "100", r: "4.2", fill: "#5eead4", opacity: "0.74" }
                }
            }
        }

        // Warm candlelight vertical glow pillars & subtle table reflection hairline
        "dinner_groove" => {
            let dg = format!("ms-{p}-din-rg");
            let col = format!("ms-{p}-din-col");
            let col_warm = format!("ms-{p}-din-colw");
            let dg_url = format!("url(#{dg})");
            let col_url = format!("url(#{col})");
            let col_warm_url = format!("url(#{col_warm})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{dg}", cx: "80%", cy: "58%", r: "58%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#fb7185", stop_opacity: "0.11" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{dg_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{col}", x1: "0", y1: "1", x2: "0", y2: "0",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.02" }
                            stop { offset: "26%", stop_color: "#fbbf24", stop_opacity: "0.28" }
                            stop { offset: "72%", stop_color: "#fda4af", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#fda4af", stop_opacity: "0" }
                        }
                        linearGradient { id: "{col_warm}", x1: "0", y1: "1", x2: "0", y2: "0",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.04" }
                            stop { offset: "30%", stop_color: "#fde68a", stop_opacity: "0.36" }
                            stop { offset: "75%", stop_color: "#fbbf24", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#fbbf24", stop_opacity: "0" }
                        }
                    }
                    rect { x: "108", y: "88", width: "10", height: "66", rx: "5", fill: "{col_url}" }
                    rect { x: "128", y: "62", width: "12", height: "92", rx: "6", fill: "{col_warm_url}" }
                    rect { x: "150", y: "48", width: "14", height: "106", rx: "7", fill: "{col_warm_url}" }
                    rect { x: "174", y: "72", width: "11", height: "82", rx: "5.5", fill: "{col_url}" }
                    rect { x: "194", y: "98", width: "9", height: "56", rx: "4.5", fill: "{col_url}", opacity: "0.75" }
                    circle { cx: "134", cy: "78", r: "2.0", fill: "#fef3c7", opacity: "0.62" }
                    circle { cx: "157", cy: "64", r: "2.4", fill: "#fef3c7", opacity: "0.76" }
                    circle { cx: "179.5", cy: "86", r: "1.6", fill: "#fde68a", opacity: "0.55" }
                    line { x1: "86", y1: "156", x2: "218", y2: "156", stroke: "#fbbf24", stroke_opacity: "0.22", stroke_width: "1", stroke_linecap: "round" }
                }
            }
        }

        // Nocturnal frequency: sleek after-hours sine ribbon and sparse starlight pinpoints
        "late_night" => {
            let sky = format!("ms-{p}-ln-sky");
            let wave = format!("ms-{p}-ln-wave");
            let sky_url = format!("url(#{sky})");
            let wave_url = format!("url(#{wave})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sky}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#a855f7", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#38bdf8", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{sky_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{wave}", x1: "0", y1: "0", x2: "1", y2: "0",
                            stop { offset: "0%", stop_color: "#c084fc", stop_opacity: "0" }
                            stop { offset: "30%", stop_color: "#d8b4fe", stop_opacity: "0.68" }
                            stop { offset: "76%", stop_color: "#7dd3fc", stop_opacity: "0.58" }
                            stop { offset: "100%", stop_color: "#38bdf8", stop_opacity: "0.14" }
                        }
                    }
                    line { x1: "48", y1: "106", x2: "226", y2: "106", stroke: "#d8b4fe", stroke_opacity: "0.12", stroke_width: "0.9" }
                    path {
                        d: "M24 106 C64 62, 108 148, 152 94 C182 58, 208 82, 232 98",
                        fill: "none",
                        stroke: "#c084fc",
                        stroke_width: "8",
                        stroke_linecap: "round",
                        opacity: "0.12",
                    }
                    path {
                        d: "M24 106 C64 62, 108 148, 152 94 C182 58, 208 82, 232 98",
                        fill: "none",
                        stroke: "{wave_url}",
                        stroke_width: "1.6",
                        stroke_linecap: "round",
                    }
                    path {
                        d: "M38 118 C80 86, 122 152, 168 112 C196 86, 214 96, 232 108",
                        fill: "none",
                        stroke: "{wave_url}",
                        stroke_width: "1.0",
                        stroke_linecap: "round",
                        opacity: "0.42",
                    }
                    circle { cx: "116", cy: "56", r: "1.4", fill: "#e9d5ff", opacity: "0.48" }
                    circle { cx: "176", cy: "46", r: "1.8", fill: "#bae6fd", opacity: "0.54" }
                    circle { cx: "204", cy: "138", r: "1.3", fill: "#d8b4fe", opacity: "0.38" }
                }
            }
        }

        // Slow sunlit slats pouring diagonally across a warm studio room
        "sunday_morning" => {
            let smg = format!("ms-{p}-sunm-rg");
            let slat = format!("ms-{p}-sunm-slat");
            let smg_url = format!("url(#{smg})");
            let slat_url = format!("url(#{slat})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{smg}", cx: "80%", cy: "38%", r: "60%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.26" }
                            stop { offset: "55%", stop_color: "#fed7aa", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{smg_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{slat}", x1: "1", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#fef3c7", stop_opacity: "0.34" }
                            stop { offset: "55%", stop_color: "#fdba74", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#fdba74", stop_opacity: "0" }
                        }
                    }
                    g { fill: "{slat_url}",
                        polygon { points: "152,0 178,0 82,200 56,200", opacity: "0.42" }
                        polygon { points: "186,0 208,0 112,200 90,200", opacity: "0.68" }
                        polygon { points: "216,0 232,0 140,200 124,200", opacity: "0.52" }
                        polygon { points: "240,18 240,56 174,200 156,200", opacity: "0.34" }
                    }
                    circle { cx: "142", cy: "72", r: "1.8", fill: "#fef3c7", opacity: "0.56" }
                    circle { cx: "174", cy: "108", r: "1.4", fill: "#fed7aa", opacity: "0.46" }
                    circle { cx: "122", cy: "132", r: "1.2", fill: "#fde68a", opacity: "0.38" }
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
                        radialGradient { id: "{cg}", cx: "80%", cy: "34%", r: "62%",
                            stop { offset: "0%", stop_color: "#33ffb0", stop_opacity: "0.24" }
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
                    circle { cx: "194", cy: "94", r: "26", fill: "none", stroke: "#33ffb0", stroke_opacity: "0.18", stroke_width: "1" }
                    g { fill: "none", stroke: "#33ffb0", stroke_linecap: "round",
                        path { d: "M96 86 Q146 60 196 86 T260 86", stroke_opacity: "0.34", stroke_width: "1.3" }
                        path { d: "M84 102 Q140 76 196 102 T260 102", stroke_opacity: "0.24", stroke_width: "1.1" }
                        path { d: "M108 118 Q158 94 208 118 T260 118", stroke_opacity: "0.15", stroke_width: "1" }
                    }
                    circle { cx: "194", cy: "94", r: "3.5", fill: "#33ffb0", opacity: "0.68" }
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
                    path { d: "M204 94 L242 68 A46 46 0 0 1 248 106 Z", fill: "#33ffb0", opacity: "0.08" }
                    g { fill: "none", stroke: "#f472f4",
                        circle { cx: "204", cy: "94", r: "22", stroke_opacity: "0.30", stroke_width: "1.1" }
                        circle { cx: "204", cy: "94", r: "42", stroke_opacity: "0.20", stroke_width: "1", stroke_dasharray: "18 10" }
                        circle { cx: "204", cy: "94", r: "64", stroke_opacity: "0.13", stroke_width: "0.9" }
                    }
                    line { x1: "204", y1: "94", x2: "242", y2: "68", stroke: "#33ffb0", stroke_opacity: "0.46", stroke_width: "1.2", stroke_linecap: "round" }
                    circle { cx: "204", cy: "94", r: "3.2", fill: "#33ffb0", opacity: "0.70" }
                    circle { cx: "234", cy: "74", r: "2.0", fill: "#f472f4", opacity: "0.58" }
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
                        line { x1: "202", y1: "38", x2: "202", y2: "44", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "202", y1: "144", x2: "202", y2: "150", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "146", y1: "94", x2: "152", y2: "94", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                        line { x1: "252", y1: "94", x2: "258", y2: "94", stroke_opacity: "0.34", stroke_width: "1", stroke_linecap: "round" }
                    }
                    path {
                        d: "M202 64 Q204 92 232 94 Q204 96 202 124 Q200 96 172 94 Q200 92 202 64 Z",
                        fill: "#f0abfc",
                        opacity: "0.42",
                    }
                }
            }
        }

        // --- Seasonal Mixes ---
        // Minimalist anamorphic winter starlight & soft warm holiday bokeh spheres
        "winter_holiday" | "noel" | "christmas" => {
            let wh = format!("ms-{p}-wh-rg");
            let bokeh = format!("ms-{p}-wh-bk");
            let vbeam = format!("ms-{p}-wh-vb");
            let hbeam = format!("ms-{p}-wh-hb");
            let wh_url = format!("url(#{wh})");
            let bokeh_url = format!("url(#{bokeh})");
            let vbeam_url = format!("url(#{vbeam})");
            let hbeam_url = format!("url(#{hbeam})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{wh}", cx: "78%", cy: "44%", r: "58%",
                            stop { offset: "0%", stop_color: "#ffd166", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#34d399", stop_opacity: "0.11" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{wh_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{bokeh}", cx: "50%", cy: "50%", r: "50%",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0.34" }
                            stop { offset: "60%", stop_color: "#ffd166", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#ffd166", stop_opacity: "0" }
                        }
                        linearGradient { id: "{vbeam}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0" }
                            stop { offset: "50%", stop_color: "#fffbeb", stop_opacity: "0.85" }
                            stop { offset: "100%", stop_color: "#fef08a", stop_opacity: "0" }
                        }
                        linearGradient { id: "{hbeam}", x1: "0", y1: "0", x2: "1", y2: "0",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0" }
                            stop { offset: "50%", stop_color: "#fffbeb", stop_opacity: "0.85" }
                            stop { offset: "100%", stop_color: "#fef08a", stop_opacity: "0" }
                        }
                    }
                    circle { cx: "110", cy: "126", r: "22", fill: "{bokeh_url}", opacity: "0.62" }
                    circle { cx: "194", cy: "118", r: "15", fill: "{bokeh_url}", opacity: "0.52" }
                    circle { cx: "156", cy: "82", r: "34", fill: "{bokeh_url}", opacity: "0.85" }
                    rect { x: "155.3", y: "26", width: "1.4", height: "112", rx: "0.7", fill: "{vbeam_url}" }
                    rect { x: "106", y: "81.3", width: "100", height: "1.4", rx: "0.7", fill: "{hbeam_url}" }
                    line { x1: "136", y1: "62", x2: "176", y2: "102", stroke: "#fef08a", stroke_opacity: "0.24", stroke_width: "0.9", stroke_linecap: "round" }
                    line { x1: "176", y1: "62", x2: "136", y2: "102", stroke: "#fef08a", stroke_opacity: "0.24", stroke_width: "0.9", stroke_linecap: "round" }
                    circle { cx: "156", cy: "82", r: "3.2", fill: "#fffbeb", opacity: "0.94" }
                    circle { cx: "102", cy: "54", r: "1.5", fill: "#6ee7b7", opacity: "0.52" }
                    circle { cx: "204", cy: "140", r: "1.6", fill: "#ffd166", opacity: "0.54" }
                }
            }
        }

        // Champagne long-exposure radial light burst & soft bokeh
        "new_years_eve" | "nye" => {
            let nye = format!("ms-{p}-nye-rg");
            let bk = format!("ms-{p}-nye-bk");
            let nye_url = format!("url(#{nye})");
            let bk_url = format!("url(#{bk})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{nye}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#f472f4", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{nye_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{bk}", cx: "50%", cy: "50%", r: "50%",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0.42" }
                            stop { offset: "65%", stop_color: "#fde047", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#fde047", stop_opacity: "0" }
                        }
                    }
                    circle { cx: "152", cy: "98", r: "34", fill: "{bk_url}", opacity: "0.75" }
                    circle { cx: "106", cy: "136", r: "14", fill: "{bk_url}", opacity: "0.52" }
                    circle { cx: "194", cy: "138", r: "10", fill: "{bk_url}", opacity: "0.42" }
                    g { stroke: "#fef08a", stroke_linecap: "round",
                        line { x1: "152", y1: "84", x2: "152", y2: "42", stroke_width: "1.2", opacity: "0.56" }
                        line { x1: "160", y1: "87", x2: "184", y2: "54", stroke_width: "1.1", opacity: "0.46" }
                        line { x1: "165", y1: "94", x2: "206", y2: "80", stroke_width: "1.2", opacity: "0.52" }
                        line { x1: "165", y1: "103", x2: "200", y2: "118", stroke_width: "1.0", opacity: "0.40" }
                        line { x1: "159", y1: "110", x2: "178", y2: "144", stroke_width: "1.1", opacity: "0.44" }
                        line { x1: "148", y1: "112", x2: "140", y2: "150", stroke_width: "1.0", opacity: "0.38" }
                        line { x1: "140", y1: "106", x2: "106", y2: "128", stroke_width: "1.2", opacity: "0.48" }
                        line { x1: "138", y1: "97", x2: "98", y2: "94", stroke_width: "1.1", opacity: "0.42" }
                        line { x1: "143", y1: "88", x2: "116", y2: "62", stroke_width: "1.0", opacity: "0.44" }
                    }
                    circle { cx: "152", cy: "98", r: "3.4", fill: "#fffbeb", opacity: "0.88" }
                    circle { cx: "152", cy: "40", r: "1.6", fill: "#fde047", opacity: "0.70" }
                    circle { cx: "208", cy: "79", r: "1.6", fill: "#f472f4", opacity: "0.66" }
                    circle { cx: "104", cy: "129", r: "1.5", fill: "#fef08a", opacity: "0.64" }
                    circle { cx: "114", cy: "60", r: "1.4", fill: "#f472f4", opacity: "0.58" }
                }
            }
        }

        // Soft horizontal hearth-light bands meeting quiet drifting frost specks
        "cozy_winter" => {
            let cw = format!("ms-{p}-cw-rg");
            let band = format!("ms-{p}-cw-band");
            let cw_url = format!("url(#{cw})");
            let band_url = format!("url(#{band})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cw}", cx: "78%", cy: "66%", r: "60%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.30" }
                            stop { offset: "50%", stop_color: "#93c5fd", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{cw_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{band}", x1: "0", y1: "0", x2: "1", y2: "0",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0" }
                            stop { offset: "52%", stop_color: "#fdba74", stop_opacity: "0.46" }
                            stop { offset: "100%", stop_color: "#fde68a", stop_opacity: "0.06" }
                        }
                    }
                    rect { x: "66", y: "112", width: "148", height: "10", rx: "5", fill: "{band_url}", opacity: "0.28" }
                    rect { x: "52", y: "130", width: "166", height: "12", rx: "6", fill: "{band_url}", opacity: "0.48" }
                    rect { x: "78", y: "150", width: "136", height: "8", rx: "4", fill: "{band_url}", opacity: "0.34" }
                    rect { x: "104", y: "166", width: "92", height: "2", rx: "1", fill: "{band_url}", opacity: "0.52" }
                    circle { cx: "116", cy: "62", r: "1.6", fill: "#bae6fd", opacity: "0.46" }
                    circle { cx: "152", cy: "48", r: "2.2", fill: "#e0f2fe", opacity: "0.42" }
                    circle { cx: "184", cy: "70", r: "1.4", fill: "#bae6fd", opacity: "0.48" }
                    circle { cx: "138", cy: "86", r: "1.3", fill: "#fed7aa", opacity: "0.44" }
                }
            }
        }

        // Imperial moon-gate & burnished gold disc
        "lunar_new_year" => {
            let lny = format!("ms-{p}-lny-rg");
            let disc = format!("ms-{p}-lny-disc");
            let lny_url = format!("url(#{lny})");
            let disc_url = format!("url(#{disc})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{lny}", cx: "78%", cy: "50%", r: "58%",
                            stop { offset: "0%", stop_color: "#fb7185", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#fbbf24", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{lny_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{disc}", cx: "50%", cy: "50%", r: "50%",
                            stop { offset: "0%", stop_color: "#fde68a", stop_opacity: "0.42" }
                            stop { offset: "72%", stop_color: "#fbbf24", stop_opacity: "0.16" }
                            stop { offset: "100%", stop_color: "#fb7185", stop_opacity: "0.04" }
                        }
                    }
                    circle { cx: "154", cy: "100", r: "34", fill: "{disc_url}" }
                    circle { cx: "154", cy: "100", r: "48", fill: "none", stroke: "#fbbf24", stroke_opacity: "0.38", stroke_width: "1.2" }
                    path { d: "M108 56 A64 64 0 0 0 108 144", fill: "none", stroke: "#fbbf24", stroke_opacity: "0.22", stroke_width: "1", stroke_linecap: "round" }
                    path { d: "M200 56 A64 64 0 0 1 200 144", fill: "none", stroke: "#fbbf24", stroke_opacity: "0.22", stroke_width: "1", stroke_linecap: "round" }
                    line { x1: "154", y1: "22", x2: "154", y2: "42", stroke: "#fbbf24", stroke_opacity: "0.38", stroke_width: "1.1", stroke_linecap: "round" }
                    line { x1: "154", y1: "158", x2: "154", y2: "178", stroke: "#fda4af", stroke_opacity: "0.42", stroke_width: "1.1", stroke_linecap: "round" }
                    circle { cx: "154", cy: "178", r: "2.0", fill: "#fda4af", opacity: "0.68" }
                }
            }
        }

        // Velvet cinema bokeh duet: two softly glowing light spheres with delicate rim gleam
        "valentines" => {
            let val = format!("ms-{p}-val-rg");
            let orb_a = format!("ms-{p}-val-oa");
            let orb_b = format!("ms-{p}-val-ob");
            let val_url = format!("url(#{val})");
            let orb_a_url = format!("url(#{orb_a})");
            let orb_b_url = format!("url(#{orb_b})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{val}", cx: "78%", cy: "50%", r: "56%",
                            stop { offset: "0%", stop_color: "#f472b6", stop_opacity: "0.26" }
                            stop { offset: "54%", stop_color: "#c084fc", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{val_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{orb_a}", cx: "38%", cy: "38%", r: "62%",
                            stop { offset: "0%", stop_color: "#fbcfe8", stop_opacity: "0.44" }
                            stop { offset: "58%", stop_color: "#f472b6", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#f472b6", stop_opacity: "0" }
                        }
                        radialGradient { id: "{orb_b}", cx: "62%", cy: "60%", r: "62%",
                            stop { offset: "0%", stop_color: "#e9d5ff", stop_opacity: "0.34" }
                            stop { offset: "58%", stop_color: "#c084fc", stop_opacity: "0.16" }
                            stop { offset: "100%", stop_color: "#c084fc", stop_opacity: "0" }
                        }
                    }
                    circle { cx: "136", cy: "108", r: "48", fill: "{orb_a_url}" }
                    circle { cx: "174", cy: "92", r: "42", fill: "{orb_b_url}" }
                    path {
                        d: "M108 82 A38 38 0 0 1 154 74",
                        fill: "none",
                        stroke: "#fbcfe8",
                        stroke_opacity: "0.36",
                        stroke_width: "1.1",
                        stroke_linecap: "round",
                    }
                    circle { cx: "198", cy: "136", r: "1.6", fill: "#fbcfe8", opacity: "0.52" }
                    circle { cx: "98", cy: "62", r: "1.4", fill: "#f472b6", opacity: "0.46" }
                }
            }
        }

        // Syncopated prism cadence ribbons & crisp geometric diamonds
        "carnival" => {
            let crn = format!("ms-{p}-crn-rg");
            let crn_url = format!("url(#{crn})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{crn}", cx: "80%", cy: "46%", r: "58%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.24" }
                            stop { offset: "48%", stop_color: "#33ffb0", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{crn_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke_linecap: "round",
                        path { d: "M48 154 C98 102, 152 128, 226 48", stroke: "#fde047", stroke_opacity: "0.44", stroke_width: "1.5" }
                        path { d: "M62 168 C112 118, 166 142, 232 68", stroke: "#33ffb0", stroke_opacity: "0.36", stroke_width: "1.3" }
                        path { d: "M78 180 C126 134, 178 156, 234 90", stroke: "#f472f4", stroke_opacity: "0.28", stroke_width: "1.1" }
                    }
                    polygon { points: "142,56 149,64 142,72 135,64", fill: "#fde047", opacity: "0.56" }
                    polygon { points: "106,96 111,102 106,108 101,102", fill: "#f472f4", opacity: "0.52" }
                    polygon { points: "192,126 198,133 192,140 186,133", fill: "#33ffb0", opacity: "0.50" }
                }
            }
        }

        // Minimalist symmetric geometric triquetra
        "celtic_spring" => {
            let cs = format!("ms-{p}-celt-rg");
            let cs_url = format!("url(#{cs})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cs}", cx: "78%", cy: "50%", r: "56%",
                            stop { offset: "0%", stop_color: "#34d399", stop_opacity: "0.26" }
                            stop { offset: "55%", stop_color: "#10b981", stop_opacity: "0.08" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{cs_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    circle { cx: "154", cy: "100", r: "56", fill: "none", stroke: "#6ee7b7", stroke_opacity: "0.18", stroke_width: "1" }
                    circle { cx: "154", cy: "100", r: "26", fill: "none", stroke: "#34d399", stroke_opacity: "0.26", stroke_width: "1.1" }
                    g { fill: "#34d399", fill_opacity: "0.06", stroke: "#6ee7b7", stroke_opacity: "0.46", stroke_width: "1.35", stroke_linejoin: "round",
                        path { d: "M154 100 Q130 74 154 48 Q178 74 154 100 Z" }
                        path { d: "M154 100 Q188.5 92.2 199 126 Q164.5 133.8 154 100 Z" }
                        path { d: "M154 100 Q143.5 133.8 109 126 Q119.5 92.2 154 100 Z" }
                    }
                    circle { cx: "154", cy: "100", r: "3.0", fill: "#a7f3d0", opacity: "0.72" }
                }
            }
        }

        // Vernal geometric blossom: 6 radial vesica petals with luminous center
        "spring" => {
            let sp = format!("ms-{p}-spr-rg");
            let sp_url = format!("url(#{sp})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sp}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#6ee7b7", stop_opacity: "0.24" }
                            stop { offset: "52%", stop_color: "#f9a8d4", stop_opacity: "0.11" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{sp_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { transform: "translate(154 100)",
                        g { fill: "#6ee7b7", fill_opacity: "0.08", stroke: "#a7f3d0", stroke_opacity: "0.38", stroke_width: "1.1", stroke_linejoin: "round",
                            path { d: "M0 -8 Q-12 -26 0 -46 Q12 -26 0 -8 Z" }
                            path { d: "M0 -8 Q-12 -26 0 -46 Q12 -26 0 -8 Z", transform: "rotate(120)" }
                            path { d: "M0 -8 Q-12 -26 0 -46 Q12 -26 0 -8 Z", transform: "rotate(240)" }
                        }
                        g { fill: "#f9a8d4", fill_opacity: "0.07", stroke: "#f9a8d4", stroke_opacity: "0.32", stroke_width: "1.0", stroke_linejoin: "round",
                            path { d: "M0 -8 Q-11 -24 0 -42 Q11 -24 0 -8 Z", transform: "rotate(60)" }
                            path { d: "M0 -8 Q-11 -24 0 -42 Q11 -24 0 -8 Z", transform: "rotate(180)" }
                            path { d: "M0 -8 Q-11 -24 0 -42 Q11 -24 0 -8 Z", transform: "rotate(300)" }
                        }
                        circle { cx: "0", cy: "0", r: "4.0", fill: "#fbcfe8", opacity: "0.82" }
                    }
                    circle { cx: "104", cy: "64", r: "1.4", fill: "#f9a8d4", opacity: "0.46" }
                    circle { cx: "204", cy: "136", r: "1.5", fill: "#6ee7b7", opacity: "0.46" }
                }
            }
        }

        // Golden coast solar disc & tapering shimmer horizon
        "summer" => {
            let sm = format!("ms-{p}-sum-rg");
            let sun = format!("ms-{p}-sum-sun");
            let sm_url = format!("url(#{sm})");
            let sun_url = format!("url(#{sun})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{sm}", cx: "78%", cy: "44%", r: "58%",
                            stop { offset: "0%", stop_color: "#fb923c", stop_opacity: "0.30" }
                            stop { offset: "50%", stop_color: "#fde047", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{sm_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{sun}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#fef08a", stop_opacity: "0.52" }
                            stop { offset: "65%", stop_color: "#fb923c", stop_opacity: "0.24" }
                            stop { offset: "100%", stop_color: "#f472b6", stop_opacity: "0.06" }
                        }
                    }
                    circle { cx: "154", cy: "82", r: "34", fill: "{sun_url}" }
                    circle { cx: "154", cy: "82", r: "43", fill: "none", stroke: "#fed7aa", stroke_opacity: "0.20", stroke_width: "1" }
                    rect { x: "66", y: "128", width: "156", height: "1.3", rx: "0.6", fill: "#fed7aa", opacity: "0.44" }
                    rect { x: "94", y: "140", width: "120", height: "1.3", rx: "0.6", fill: "#fdba74", opacity: "0.34" }
                    rect { x: "112", y: "151", width: "84", height: "1.2", rx: "0.6", fill: "#fdba74", opacity: "0.26" }
                    rect { x: "128", y: "162", width: "52", height: "1.1", rx: "0.5", fill: "#fde047", opacity: "0.18" }
                    rect { x: "140", y: "173", width: "28", height: "1.0", rx: "0.5", fill: "#fde047", opacity: "0.12" }
                }
            }
        }

        // Celestial zenith corona & tilted ecliptic orbit (no crosshair)
        "summer_solstice" => {
            let ss = format!("ms-{p}-sol-rg");
            let ss_url = format!("url(#{ss})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{ss}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#fde047", stop_opacity: "0.28" }
                            stop { offset: "48%", stop_color: "#f472b6", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{ss_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    circle {
                        cx: "154",
                        cy: "100",
                        r: "48",
                        fill: "none",
                        stroke: "#fde047",
                        stroke_opacity: "0.36",
                        stroke_width: "2.2",
                        stroke_dasharray: "1.8 10.76",
                    }
                    circle { cx: "154", cy: "100", r: "28", fill: "none", stroke: "#fef08a", stroke_opacity: "0.28", stroke_width: "1" }
                    ellipse {
                        cx: "154",
                        cy: "100",
                        rx: "68",
                        ry: "20",
                        transform: "rotate(-22 154 100)",
                        fill: "none",
                        stroke: "#f9a8d4",
                        stroke_opacity: "0.36",
                        stroke_width: "1.1",
                    }
                    circle { cx: "154", cy: "100", r: "13", fill: "#fef08a", opacity: "0.30" }
                    circle { cx: "154", cy: "100", r: "5.2", fill: "#fffbeb", opacity: "0.78" }
                    circle { cx: "217", cy: "75", r: "2.4", fill: "#fde047", opacity: "0.78" }
                }
            }
        }

        // Equinox dusk horizon & mirrored half-arc reflection
        "late_summer_reset" => {
            let lsr = format!("ms-{p}-lsr-rg");
            let dusk = format!("ms-{p}-lsr-dusk");
            let lsr_url = format!("url(#{lsr})");
            let dusk_url = format!("url(#{dusk})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{lsr}", cx: "78%", cy: "48%", r: "56%",
                            stop { offset: "0%", stop_color: "#f0abfc", stop_opacity: "0.26" }
                            stop { offset: "50%", stop_color: "#fb923c", stop_opacity: "0.12" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{lsr_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        linearGradient { id: "{dusk}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "#f0abfc", stop_opacity: "0.38" }
                            stop { offset: "100%", stop_color: "#fb923c", stop_opacity: "0.14" }
                        }
                    }
                    path { d: "M118 106 A36 36 0 0 1 190 106 Z", fill: "{dusk_url}" }
                    path {
                        d: "M118 106 A36 36 0 0 0 190 106",
                        fill: "none",
                        stroke: "#fdba74",
                        stroke_opacity: "0.28",
                        stroke_width: "1.1",
                        stroke_dasharray: "4 5",
                    }
                    line { x1: "58", y1: "106", x2: "222", y2: "106", stroke: "#f0abfc", stroke_opacity: "0.40", stroke_width: "1.1", stroke_linecap: "round" }
                    line { x1: "94", y1: "152", x2: "214", y2: "152", stroke: "#f0abfc", stroke_opacity: "0.18", stroke_width: "1", stroke_linecap: "round" }
                    line { x1: "118", y1: "166", x2: "190", y2: "166", stroke: "#fdba74", stroke_opacity: "0.14", stroke_width: "1", stroke_linecap: "round" }
                }
            }
        }

        // Geometric amber drift facets along a crisp breeze curve
        "autumn" => {
            let aut = format!("ms-{p}-aut-rg");
            let aut_url = format!("url(#{aut})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{aut}", cx: "78%", cy: "48%", r: "58%",
                            stop { offset: "0%", stop_color: "#f59e0b", stop_opacity: "0.26" }
                            stop { offset: "52%", stop_color: "#ea580c", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{aut_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    path {
                        d: "M48 156 Q132 126 226 62",
                        fill: "none",
                        stroke: "#fbbf24",
                        stroke_opacity: "0.20",
                        stroke_width: "1",
                        stroke_linecap: "round",
                    }
                    g { transform: "translate(168 78) rotate(-24)",
                        polygon { points: "0,-24 13,0 0,24 -13,0", fill: "#fbbf24", fill_opacity: "0.13", stroke: "#fbbf24", stroke_opacity: "0.46", stroke_width: "1.1" }
                        line { x1: "0", y1: "-24", x2: "0", y2: "24", stroke: "#fde68a", stroke_opacity: "0.34", stroke_width: "0.9" }
                    }
                    g { transform: "translate(122 116) rotate(16) scale(0.76)",
                        polygon { points: "0,-24 13,0 0,24 -13,0", fill: "#fb923c", fill_opacity: "0.11", stroke: "#fdba74", stroke_opacity: "0.36", stroke_width: "1.1" }
                        line { x1: "0", y1: "-24", x2: "0", y2: "24", stroke: "#fed7aa", stroke_opacity: "0.28", stroke_width: "0.9" }
                    }
                    g { transform: "translate(202 128) rotate(-38) scale(0.56)",
                        polygon { points: "0,-24 13,0 0,24 -13,0", fill: "#f59e0b", fill_opacity: "0.10", stroke: "#fbbf24", stroke_opacity: "0.28", stroke_width: "1.1" }
                        line { x1: "0", y1: "-24", x2: "0", y2: "24", stroke: "#fde68a", stroke_opacity: "0.22", stroke_width: "0.9" }
                    }
                }
            }
        }

        // Minimalist blood-moon eclipse (pure corona + occulted dark disc, no orbital ring)
        "halloween" => {
            let hal = format!("ms-{p}-hal-rg");
            let cor = format!("ms-{p}-hal-cor");
            let hal_url = format!("url(#{hal})");
            let cor_url = format!("url(#{cor})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{hal}", cx: "78%", cy: "48%", r: "56%",
                            stop { offset: "0%", stop_color: "#f97316", stop_opacity: "0.28" }
                            stop { offset: "52%", stop_color: "#a855f7", stop_opacity: "0.14" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{hal_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{cor}", cx: "50%", cy: "50%", r: "50%",
                            stop { offset: "50%", stop_color: "#fb923c", stop_opacity: "0.52" }
                            stop { offset: "72%", stop_color: "#f97316", stop_opacity: "0.22" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    circle { cx: "156", cy: "98", r: "56", fill: "{cor_url}" }
                    circle { cx: "156", cy: "98", r: "34", fill: "none", stroke: "#f97316", stroke_opacity: "0.24", stroke_width: "4" }
                    circle { cx: "156", cy: "98", r: "32", fill: "none", stroke: "#fb923c", stroke_opacity: "0.72", stroke_width: "1.5" }
                    circle { cx: "161", cy: "95", r: "30", fill: "#160822" }
                    circle { cx: "104", cy: "60", r: "1.5", fill: "#c084fc", opacity: "0.48" }
                    circle { cx: "206", cy: "138", r: "1.4", fill: "#fb923c", opacity: "0.50" }
                }
            }
        }

        // --- Default fallback: Midnight Studio acoustic pulse & 4-point spark ---
        _ => {
            let def = format!("ms-{p}-def-rg");
            let def_url = format!("url(#{def})");
            rsx! {
                svg {
                    view_box: "0 0 400 200",
                    preserve_aspect_ratio: "none",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    defs {
                        radialGradient { id: "{def}", cx: "78%", cy: "46%", r: "58%",
                            stop { offset: "0%", stop_color: "#f472f4", stop_opacity: "0.24" }
                            stop { offset: "52%", stop_color: "#a855f7", stop_opacity: "0.10" }
                            stop { offset: "100%", stop_color: "#000000", stop_opacity: "0" }
                        }
                    }
                    rect { width: "400", height: "200", fill: "{def_url}" }
                }
                svg {
                    view_box: "0 0 240 200",
                    preserve_aspect_ratio: "xMaxYMid meet",
                    class: "absolute inset-0 w-full h-full",
                    "aria-hidden": "true",
                    g { fill: "none", stroke: "#f472f4",
                        circle { cx: "154", cy: "100", r: "26", stroke_opacity: "0.28", stroke_width: "1.1" }
                        circle { cx: "154", cy: "100", r: "46", stroke_opacity: "0.18", stroke_width: "1", stroke_dasharray: "8 6" }
                        circle { cx: "154", cy: "100", r: "66", stroke_opacity: "0.11", stroke_width: "0.9" }
                    }
                    path {
                        d: "M154 84 Q155.5 98.5 170 100 Q155.5 101.5 154 116 Q152.5 101.5 138 100 Q152.5 98.5 154 84 Z",
                        fill: "#f5d0fe",
                        opacity: "0.56",
                    }
                    circle { cx: "186", cy: "67", r: "2.2", fill: "#33ffb0", opacity: "0.62" }
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
            class: "group relative w-full h-full overflow-hidden rounded-[14px] border text-left flex flex-col justify-between transition-[transform,filter] duration-150 hover:-translate-y-0.5 hover:brightness-110 active:scale-[0.99] cursor-pointer {card_padding}",
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
                    div { class: "mt-4", style: "max-width:68%;",
                        h3 { class: "text-[20px] sm:text-[21px] font-bold tracking-[-0.02em] leading-[1.2] text-beet-text-primary",
                            "{title}"
                        }
                        p {
                            class: "text-[13px] leading-[1.42] mt-1 line-clamp-2",
                            style: "color:rgba(235,228,246,0.80);min-height:2.84em;",
                            "{subtitle}"
                        }
                    }
                    div {
                        class: "mt-3.5 flex flex-wrap items-center gap-1.5 tabular-nums-mono text-[11px] tracking-[0.04em]",
                        style: "color:rgba(216,194,245,0.68);",
                        span { class: "whitespace-nowrap", "{mix.track_count} {track_word}" }
                        if show_owned_pct && mix.owned_pct > 0 {
                            span { class: "whitespace-nowrap", "· {mix.owned_pct}% LIBRARY" }
                        }
                    }
                } else {
                    div { class: "mt-auto pt-11",
                        h3 { class: "text-[17px] font-bold tracking-[-0.02em] leading-[1.2] text-beet-text-primary",
                            "{title}"
                        }
                        p {
                            class: "text-[12px] leading-[1.42] mt-1 line-clamp-2",
                            style: "color:rgba(235,228,246,0.78);min-height:2.84em;",
                            "{subtitle}"
                        }
                        div {
                            class: "mt-2.5 flex items-center gap-1.5 tabular-nums-mono text-[11px] tracking-[0.04em]",
                            style: "color:rgba(216,194,245,0.68);",
                            span { class: "whitespace-nowrap", "{mix.track_count} {track_word}" }
                            if show_owned_pct && mix.owned_pct > 0 {
                                span { class: "whitespace-nowrap", "· {mix.owned_pct}% LIBRARY" }
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
    #[props(default)] show_owned_pct: bool,
    #[props(default)] header_action: Option<Element>,
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

    let action_on_right_now = !right_now.is_empty();
    let mut right_now_action = None;
    let mut collection_action = None;
    if action_on_right_now {
        right_now_action = header_action;
    } else {
        collection_action = header_action;
    }

    rsx! {
        div { class: "space-y-7",
            if !right_now.is_empty() {
                section { class: "space-y-3.5",
                    div { class: "flex flex-wrap items-baseline justify-between gap-2",
                        div { class: "flex flex-wrap items-baseline gap-2.5",
                            h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                "Right Now"
                            }
                            span { class: "tabular-nums-mono text-[11px] uppercase tracking-[0.06em] text-beet-text-tertiary",
                                "LIVE WEATHER & TIME OF DAY"
                            }
                        }
                        if let Some(action) = right_now_action {
                            {action}
                        }
                    }
                    div { class: "grid grid-cols-1 md:grid-cols-2 gap-4",
                        for (idx, mix) in right_now.into_iter().enumerate() {
                            MixSleeveCard {
                                key: "{id_prefix}-rn-{idx}-{mix.slug}",
                                mix,
                                layout: MixSleeveLayout::Wide,
                                id_prefix: format!("{id_prefix}-rn-{idx}"),
                                show_owned_pct,
                                on_play,
                            }
                        }
                    }
                }
            }

            if !collection.is_empty() {
                section { class: "space-y-3.5",
                    div { class: "flex flex-wrap items-baseline justify-between gap-2",
                        div { class: "flex flex-wrap items-baseline gap-2.5",
                            h2 { class: "text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary",
                                "From Your Collection"
                            }
                            span { class: "tabular-nums-mono text-[11px] uppercase tracking-[0.06em] text-beet-text-tertiary",
                                "SMART LIBRARY & DECADE SLEEVES"
                            }
                        }
                        if let Some(action) = collection_action {
                            {action}
                        }
                    }
                    div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3.5",
                        for (idx, mix) in collection.into_iter().enumerate() {
                            MixSleeveCard {
                                key: "{id_prefix}-col-{idx}-{mix.slug}",
                                mix,
                                layout: MixSleeveLayout::Square,
                                id_prefix: format!("{id_prefix}-col-{idx}"),
                                show_owned_pct: false,
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

    #[test]
    fn seasonal_and_redesigned_wide_motifs_use_meet_to_prevent_hero_cropping() {
        for slug in [
            "morning",
            "focus",
            "dinner_groove",
            "late_night",
            "sunday_morning",
            "winter_holiday",
            "new_years_eve",
            "cozy_winter",
            "lunar_new_year",
            "valentines",
            "carnival",
            "celtic_spring",
            "spring",
            "summer",
            "summer_solstice",
            "late_summer_reset",
            "autumn",
            "halloween",
            "custom_unknown_slug",
        ] {
            let slug_owned = slug.to_string();
            let mut dom = VirtualDom::new_with_props(
                |s: String| render_mix_sleeve_art(&s, "hero-fit"),
                slug_owned,
            );
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);

            assert!(
                html.contains("preserveAspectRatio=\"xMaxYMid meet\""),
                "expected xMaxYMid meet for wide/seasonal motif `{slug}`: {html}"
            );
            assert!(
                !html.contains("preserveAspectRatio=\"xMaxYMid slice\""),
                "wide/seasonal motif `{slug}` must not use slice: {html}"
            );
        }
    }
}
