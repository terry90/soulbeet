use dioxus::prelude::*;
use shared::recommendation::UserCultureProfile;

use crate::IconSlidersHorizontal;

/// Normalized regional segment rendered in [`TasteDnaBar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasteSegment {
    pub label: String,
    pub percent: u8,
}

/// Normalizes a [`UserCultureProfile`]'s `inferred_area_weights` into at most 4 segments
/// (top 3 regions + optional `"INTL"` remainder) whose percentages sum strictly to `100`.
/// Returns an empty list if there are no positive finite area weights.
pub fn normalize_taste_segments(profile: &UserCultureProfile) -> Vec<TasteSegment> {
    let mut valid: Vec<(String, f32)> = profile
        .inferred_area_weights
        .iter()
        .filter(|(code, w)| !code.trim().is_empty() && w.is_finite() && *w > 0.0)
        .map(|(code, w)| (code.trim().to_ascii_uppercase(), *w))
        .collect();

    if valid.is_empty() {
        return Vec::new();
    }

    valid.sort_by(|a, b| b.1.total_cmp(&a.1));

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

/// Visualizes a user's [`UserCultureProfile`] as a crisp Refined Midnight Studio
/// segmented bar (`Local Culture` vs `International`), optional inferred area breakdown,
/// and top language/region/genre chips.
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

    let mut tag_chips: Vec<String> = profile
        .inferred_local_tags
        .iter()
        .chain(top_genres.iter())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    tag_chips.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    tag_chips.truncate(8);

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

            // Segmented horizontal studio bar (Local Culture vs International)
            div {
                class: "h-2.5 w-full rounded-full overflow-hidden flex gap-0.5 bg-beet-sunken",
                role: "img",
                aria_label: "{local_pct} percent {local_code} and {intl_pct} percent International",
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

            // Inferred regional area distribution (when available)
            if !area_segments.is_empty() {
                div { class: "flex flex-wrap items-center gap-3 text-[11px] tabular-nums-mono text-beet-text-secondary",
                    for (idx, seg) in area_segments.iter().enumerate() {
                        div {
                            key: "{seg.label}",
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

            // Language & genre chips
            if !profile.preferred_languages.is_empty() || !tag_chips.is_empty() {
                div { class: "flex flex-wrap items-center gap-1.5 pt-0.5",
                    for lang in profile.preferred_languages.iter().filter(|l| !l.trim().is_empty()) {
                        span {
                            key: "lang-{lang}",
                            class: "px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/25 text-beet-accent-text tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider",
                            "{lang.trim().to_ascii_uppercase()}"
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
    fn normalizes_area_weights_to_exact_hundred_percent() {
        let profile = UserCultureProfile {
            home_country: Some("FR".into()),
            preferred_languages: vec!["fr".into(), "en".into()],
            local_culture_ratio: 0.45,
            inferred_area_weights: vec![("fr".into(), 1.0), ("us".into(), 1.0), ("jp".into(), 1.0)],
            inferred_local_tags: vec!["french house".into()],
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
    fn handles_empty_and_out_of_range_profiles_safely() {
        let profile = UserCultureProfile {
            local_culture_ratio: f32::NAN,
            ..Default::default()
        };
        assert!(normalize_taste_segments(&profile).is_empty());
        assert_eq!(local_vs_international_split(&profile), (45, 55));
    }
}
