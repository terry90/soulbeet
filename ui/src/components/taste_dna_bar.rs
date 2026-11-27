use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use shared::recommendation::UserCultureProfile;

use crate::IconSlidersHorizontal;

/// Normalized regional segment rendered in [`TasteDnaBar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasteSegment {
    pub label: String,
    pub percent: u8,
}

/// Normalizes a [`UserCultureProfile`]'s `inferred_area_weights` into at most 4 unique
/// segments (top 3 regions + optional `"OTHER"` remainder) whose percentages sum strictly
/// to `100`. Duplicate country codes (case-insensitive) are aggregated before grouping.
/// Returns an empty list if there are no positive finite area weights.
pub fn normalize_taste_segments(profile: &UserCultureProfile) -> Vec<TasteSegment> {
    let mut totals: HashMap<String, f32> = HashMap::new();
    for (code, w) in &profile.inferred_area_weights {
        let trimmed = code.trim();
        if !trimmed.is_empty() && w.is_finite() && *w > 0.0 {
            let upper = trimmed.to_ascii_uppercase();
            *totals.entry(upper).or_insert(0.0) += *w;
        }
    }

    if totals.is_empty() {
        return Vec::new();
    }

    let mut valid: Vec<(String, f32)> = totals.into_iter().collect();
    valid.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut grouped: Vec<(String, f32)> = Vec::new();
    if valid.len() <= 4 {
        grouped = valid;
    } else {
        grouped.extend(valid.iter().take(3).cloned());
        let rest_sum: f32 = valid.iter().skip(3).map(|(_, w)| *w).sum();
        if rest_sum > 0.0 {
            grouped.push(("OTHER".to_string(), rest_sum));
        }
    }

    let total: f32 = grouped.iter().map(|(_, w)| *w).sum();
    if total <= 0.0 || !total.is_finite() {
        return Vec::new();
    }

    // Largest-remainder rounding so percentages always sum to exactly 100.
    let mut exact: Vec<(String, u8, f32)> = grouped
        .into_iter()
        .map(|(label, w)| {
            let pct = (w / total) * 100.0;
            let floored = pct.floor() as u8;
            let remainder = pct - f32::from(floored);
            (label, floored, remainder)
        })
        .collect();

    let sum: u16 = exact.iter().map(|(_, f, _)| u16::from(*f)).sum();
    let mut leftover = 100u16.saturating_sub(sum) as usize;

    let mut order: Vec<usize> = (0..exact.len()).collect();
    order.sort_by(|&a, &b| exact[b].2.total_cmp(&exact[a].2));
    for idx in order {
        if leftover == 0 {
            break;
        }
        exact[idx].1 = exact[idx].1.saturating_add(1);
        leftover -= 1;
    }

    exact
        .into_iter()
        .filter(|(_, pct, _)| *pct > 0)
        .map(|(label, percent, _)| TasteSegment { label, percent })
        .collect()
}

/// Computes the binary `(local_percent, international_percent)` split summing to `100`.
pub fn local_vs_international_split(profile: &UserCultureProfile) -> (u8, u8) {
    let ratio = if profile.local_culture_ratio.is_finite() {
        profile.local_culture_ratio.clamp(0.0, 1.0)
    } else {
        0.45
    };
    let local = (ratio * 100.0).round() as u8;
    (local, 100u8.saturating_sub(local))
}

/// Deduplicates strings case-insensitively while preserving first-seen order.
pub fn dedup_case_insensitive<'a>(
    items: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in items {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_ascii_lowercase()) {
            out.push(trimmed.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Visualizes a user's [`UserCultureProfile`] as a crisp Refined Midnight Studio
/// segmented bar matching its regional legend 1:1 (falling back to the binary Local vs.
/// International split when no area weights exist) plus language and genre chips.
#[component]
pub fn TasteDnaBar(
    profile: UserCultureProfile,
    #[props(default)] top_genres: Vec<String>,
    #[props(default)] on_edit: Option<EventHandler<()>>,
    #[props(default)] class: String,
) -> Element {
    let (local_pct, intl_pct) = local_vs_international_split(&profile);
    let local_code = profile
        .effective_country()
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "LOCAL".to_string());
    let area_segments = normalize_taste_segments(&profile);

    let segment_colors = [
        "bg-beet-accent-solid",
        "bg-beet-leaf-solid",
        "bg-beet-gold",
        "bg-beet-border-strong",
    ];

    let languages: Vec<String> =
        dedup_case_insensitive(profile.preferred_languages.iter().map(String::as_str), 6)
            .into_iter()
            .map(|l| l.to_ascii_uppercase())
            .collect();

    let tag_chips = dedup_case_insensitive(
        profile
            .inferred_local_tags
            .iter()
            .chain(top_genres.iter())
            .map(String::as_str),
        8,
    );

    let weather_label = profile
        .weather_city
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|city| format!("Weather: {city}"))
        .unwrap_or_else(|| "Weather: Auto (IP)".to_string());

    let bar_aria_label = if !area_segments.is_empty() {
        area_segments
            .iter()
            .map(|s| format!("{} {} percent", s.label, s.percent))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("{local_pct} percent {local_code} and {intl_pct} percent International")
    };

    rsx! {
        section {
            aria_label: "Your Taste DNA",
            class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 space-y-3.5 {class}",

            // Header row
            div { class: "flex items-center justify-between gap-3",
                div { class: "min-w-0",
                    h3 { class: "text-[16px] font-semibold text-beet-text-primary leading-tight",
                        "Your Taste DNA"
                    }
                    p { class: "tabular-nums-mono text-[11px] font-semibold tracking-wider text-beet-text-tertiary mt-0.5",
                        "{local_pct}% {local_code} · {intl_pct}% INTL"
                    }
                    p { class: "text-[11px] text-beet-text-tertiary mt-0.5",
                        "{weather_label}"
                    }
                }

                if let Some(edit_cb) = on_edit {
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-highest text-[12px] font-medium transition-colors duration-150 cursor-pointer shrink-0",
                        onclick: move |_| edit_cb.call(()),
                        IconSlidersHorizontal { size: 14 }
                        span { "Tune Mix" }
                    }
                }
            }

            // Segmented horizontal studio bar: mirrors `area_segments` 1:1 when present,
            // falling back to the 2-segment Local vs International split otherwise.
            div {
                class: "h-2.5 w-full rounded-full overflow-hidden flex gap-0.5 bg-beet-sunken",
                role: "img",
                aria_label: "{bar_aria_label}",
                if !area_segments.is_empty() {
                    for (idx, seg) in area_segments.iter().enumerate() {
                        div {
                            key: "bar-{seg.label}",
                            class: "h-full transition-[width] duration-300 {segment_colors[idx % segment_colors.len()]}",
                            style: "width: {seg.percent}%;",
                        }
                    }
                } else {
                    if local_pct > 0 {
                        div {
                            class: "h-full bg-beet-accent-solid transition-[width] duration-300",
                            style: "width: {local_pct}%;",
                        }
                    }
                    if intl_pct > 0 {
                        div {
                            class: "h-full bg-beet-leaf-solid transition-[width] duration-300",
                            style: "width: {intl_pct}%;",
                        }
                    }
                }
            }

            // Inferred regional area distribution legend (matches bar segments 1:1)
            if !area_segments.is_empty() {
                div { class: "flex flex-wrap items-center gap-3 text-[11px] tabular-nums-mono text-beet-text-secondary",
                    for (idx, seg) in area_segments.iter().enumerate() {
                        div {
                            key: "legend-{seg.label}",
                            class: "inline-flex items-center gap-1.5",
                            span {
                                class: "w-2 h-2 rounded-full {segment_colors[idx % segment_colors.len()]}",
                                aria_hidden: "true",
                            }
                            span { "{seg.label} {seg.percent}%" }
                        }
                    }
                }
            }

            // Deduplicated language & genre chips
            if !languages.is_empty() || !tag_chips.is_empty() {
                div { class: "flex flex-wrap items-center gap-1.5 pt-0.5",
                    for lang in languages.iter() {
                        span {
                            key: "lang-{lang}",
                            class: "px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/25 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider",
                            "{lang}"
                        }
                    }
                    for tag in tag_chips.iter() {
                        span {
                            key: "tag-{tag}",
                            class: "px-2.5 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-subtle text-beet-text-secondary text-[12px]",
                            "{tag}"
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
    fn aggregates_duplicate_country_codes_case_insensitively() {
        let profile = UserCultureProfile {
            home_country: Some("FR".into()),
            inferred_area_weights: vec![
                ("fr".into(), 20.0),
                (" FR ".into(), 30.0),
                ("us".into(), 50.0),
            ],
            ..Default::default()
        };
        let segs = normalize_taste_segments(&profile);
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0],
            TasteSegment {
                label: "FR".into(),
                percent: 50
            }
        );
        assert_eq!(
            segs[1],
            TasteSegment {
                label: "US".into(),
                percent: 50
            }
        );
    }

    #[test]
    fn normalizes_area_weights_to_exact_hundred_percent() {
        let profile = UserCultureProfile {
            home_country: Some("FR".into()),
            preferred_languages: vec!["fr".into(), "en".into()],
            local_culture_ratio: 0.45,
            inferred_area_weights: vec![("fr".into(), 1.0), ("us".into(), 1.0), ("jp".into(), 1.0)],
            inferred_local_tags: vec!["french house".into()],
            ..Default::default()
        };
        let segs = normalize_taste_segments(&profile);
        assert_eq!(segs.len(), 3);
        let sum: u8 = segs.iter().map(|s| s.percent).sum();
        assert_eq!(sum, 100);
        assert_eq!(local_vs_international_split(&profile), (45, 55));
    }

    #[test]
    fn groups_long_tail_regions_into_other_and_ignores_invalid_weights() {
        let profile = UserCultureProfile {
            inferred_area_weights: vec![
                ("US".into(), 50.0),
                ("FR".into(), 30.0),
                ("GB".into(), 10.0),
                ("JP".into(), 6.0),
                ("DE".into(), 4.0),
                ("XX".into(), -5.0),
                ("YY".into(), f32::NAN),
                ("".into(), 99.0),
            ],
            ..Default::default()
        };
        let segs = normalize_taste_segments(&profile);
        assert_eq!(segs.len(), 4);
        assert_eq!(
            segs[0],
            TasteSegment {
                label: "US".into(),
                percent: 50
            }
        );
        assert_eq!(
            segs[1],
            TasteSegment {
                label: "FR".into(),
                percent: 30
            }
        );
        assert_eq!(
            segs[2],
            TasteSegment {
                label: "GB".into(),
                percent: 10
            }
        );
        assert_eq!(
            segs[3],
            TasteSegment {
                label: "OTHER".into(),
                percent: 10
            }
        );
    }

    #[test]
    fn deduplicates_non_consecutive_languages_and_tags_case_insensitively() {
        let out = dedup_case_insensitive(["fr", "en", "FR", "  ", "En", "jp"], 6);
        assert_eq!(out, vec!["fr", "en", "jp"]);
    }

    #[test]
    fn renders_area_segments_matching_legend_colors_in_ssr() {
        let profile = UserCultureProfile {
            home_country: Some("FR".into()),
            preferred_languages: vec!["fr".into(), "en".into(), "FR".into()],
            local_culture_ratio: 0.45,
            inferred_area_weights: vec![
                ("US".into(), 50.0),
                ("FR".into(), 30.0),
                ("JP".into(), 20.0),
            ],
            inferred_local_tags: vec!["French House".into(), "Ambient".into()],
            ..Default::default()
        };
        let mut dom = VirtualDom::new_with_props(
            |profile: UserCultureProfile| {
                rsx! {
                    TasteDnaBar {
                        profile,
                        top_genres: vec!["french house".to_string(), "Synthpop".to_string()],
                    }
                }
            },
            profile,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("width: 50%;"), "{html}");
        assert!(html.contains("width: 30%;"), "{html}");
        assert!(html.contains("width: 20%;"), "{html}");
        assert!(html.contains("bg-beet-gold"), "{html}");
        assert!(html.contains("Weather: Auto (IP)"), "{html}");
        assert_eq!(html.matches(">FR</span>").count(), 1, "{html}");
    }

    #[test]
    fn renders_explicit_weather_city_in_taste_dna_card() {
        let profile = UserCultureProfile {
            home_country: Some("FR".into()),
            weather_city: Some("Lyon, FR".into()),
            ..Default::default()
        };
        let mut dom = VirtualDom::new_with_props(
            |profile: UserCultureProfile| rsx! { TasteDnaBar { profile } },
            profile,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Weather: Lyon, FR"), "{html}");
    }

    #[test]
    fn handles_empty_and_out_of_range_profiles_safely() {
        let profile = UserCultureProfile {
            local_culture_ratio: f32::NAN,
            ..Default::default()
        };
        assert!(normalize_taste_segments(&profile).is_empty());
        assert_eq!(local_vs_international_split(&profile), (45, 55));
    }
}
