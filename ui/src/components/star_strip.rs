use dioxus::prelude::*;

use crate::IconStar;

/// Sizing variant for [`StarStrip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StarStripSize {
    Compact,
    #[default]
    Standard,
}

/// Computes the next rating value when a star (`1..=5`) is clicked.
/// If `allow_clear` is enabled and the user clicks the currently active rating,
/// clears the rating back to `0`.
pub fn next_star_rating(current: u8, clicked: u8, allow_clear: bool) -> u8 {
    let current = current.min(5);
    let clicked = clicked.clamp(1, 5);
    if allow_clear && current == clicked {
        0
    } else {
        clicked
    }
}

/// Computes keyboard-adjusted star rating for arrow/home/end/delete keys.
pub fn keyboard_star_rating(current: u8, key: &Key, allow_clear: bool) -> Option<u8> {
    let current = current.min(5);
    let min_val = if allow_clear { 0 } else { 1 };
    match key {
        Key::ArrowRight | Key::ArrowUp => Some((current.saturating_add(1)).min(5)),
        Key::ArrowLeft | Key::ArrowDown => Some(current.saturating_sub(1).max(min_val)),
        Key::Home => Some(1),
        Key::End => Some(5),
        Key::Backspace | Key::Delete if allow_clear => Some(0),
        _ => None,
    }
}

/// Returns the star (`1..=5`) that receives `tabindex="0"` for roving focus in the radiogroup.
pub fn roving_tab_star(rating: u8) -> u8 {
    let clamped = rating.min(5);
    if clamped == 0 {
        1
    } else {
        clamped
    }
}

/// Accessible 1–5★ interactive rating control with pointer-only gold hover preview,
/// click-to-clear, roving tabindex, and keyboard arrow navigation.
#[component]
pub fn StarStrip(
    /// Current rating (`0` = unrated, `1..=5` = star rating).
    rating: u8,
    /// Emits `0..=5` when the user selects or clears a rating.
    on_rate: EventHandler<u8>,
    #[props(default = true)] allow_clear: bool,
    #[props(default)] size: StarStripSize,
    #[props(default)] disabled: bool,
    #[props(default = "Track rating".to_string())] aria_label: String,
    #[props(default)] class: String,
) -> Element {
    let mut hover_rating = use_signal(|| None::<u8>);
    let clamped = rating.min(5);
    let effective_rating = if disabled {
        clamped
    } else {
        hover_rating().unwrap_or(clamped)
    };
    let active_tab_star = roving_tab_star(clamped);

    let (icon_size, btn_class) = match size {
        StarStripSize::Compact => (14u32, "w-6 h-6"),
        StarStripSize::Standard => (18u32, "w-8 h-8"),
    };

    rsx! {
        div {
            class: "inline-flex items-center gap-0.5 {class}",
            role: "radiogroup",
            aria_label: "{aria_label}",
            onmouseleave: move |_| hover_rating.set(None),
            onkeydown: move |evt: KeyboardEvent| {
                if disabled {
                    return;
                }
                if let Some(next) = keyboard_star_rating(clamped, &evt.key(), allow_clear) {
                    evt.prevent_default();
                    evt.stop_propagation();
                    hover_rating.set(None);
                    on_rate.call(next);
                }
            },
            for star in 1u8..=5u8 {
                {
                    let is_filled = star <= effective_rating;
                    let is_selected = clamped == star;
                    let tab_index = if disabled || star != active_tab_star {
                        "-1"
                    } else {
                        "0"
                    };
                    let label = if allow_clear && is_selected {
                        format!("Clear {star}-star rating")
                    } else if star == 1 {
                        "Rate 1 out of 5 stars".to_string()
                    } else {
                        format!("Rate {star} out of 5 stars")
                    };
                    let color_class = if is_filled {
                        "text-beet-gold"
                    } else {
                        "text-beet-text-tertiary hover:text-beet-gold/80"
                    };
                    let cursor_class = if disabled {
                        "opacity-45 cursor-not-allowed"
                    } else {
                        "cursor-pointer hover:scale-110 active:scale-95"
                    };
                    rsx! {
                        button {
                            key: "{star}",
                            r#type: "button",
                            role: "radio",
                            tabindex: "{tab_index}",
                            disabled: disabled,
                            aria_checked: if is_selected { "true" } else { "false" },
                            aria_label: "{label}",
                            title: "{label}",
                            class: "inline-flex items-center justify-center rounded-md transition-[color,transform] duration-150 {btn_class} {color_class} {cursor_class}",
                            onmouseenter: move |_| {
                                if !disabled {
                                    hover_rating.set(Some(star));
                                }
                            },
                            onclick: move |evt| {
                                evt.stop_propagation();
                                if !disabled {
                                    hover_rating.set(None);
                                    on_rate.call(next_star_rating(clamped, star, allow_clear));
                                }
                            },
                            IconStar { size: icon_size, filled: is_filled }
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
    fn clicking_same_star_clears_when_allowed() {
        assert_eq!(next_star_rating(4, 4, true), 0);
        assert_eq!(next_star_rating(4, 4, false), 4);
        assert_eq!(next_star_rating(0, 3, true), 3);
        assert_eq!(next_star_rating(2, 5, true), 5);
        // Out-of-range ratings clamp to 5 before checking clear
        assert_eq!(next_star_rating(9, 5, true), 0);
    }

    #[test]
    fn keyboard_arrows_clamp_within_bounds() {
        assert_eq!(keyboard_star_rating(0, &Key::ArrowRight, true), Some(1));
        assert_eq!(keyboard_star_rating(5, &Key::ArrowRight, true), Some(5));
        assert_eq!(keyboard_star_rating(1, &Key::ArrowLeft, true), Some(0));
        assert_eq!(keyboard_star_rating(1, &Key::ArrowLeft, false), Some(1));
        assert_eq!(keyboard_star_rating(3, &Key::Home, true), Some(1));
        assert_eq!(keyboard_star_rating(3, &Key::End, true), Some(5));
        assert_eq!(keyboard_star_rating(3, &Key::Backspace, true), Some(0));
        assert_eq!(keyboard_star_rating(3, &Key::Backspace, false), None);
    }

    #[test]
    fn roving_tabindex_highlights_single_star_and_clamps_out_of_range() {
        assert_eq!(roving_tab_star(0), 1);
        assert_eq!(roving_tab_star(3), 3);
        assert_eq!(roving_tab_star(9), 5);

        let mut dom = VirtualDom::new(|| {
            rsx! {
                StarStrip {
                    rating: 9,
                    on_rate: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(html.matches("tabindex=\"0\"").count(), 1, "{html}");
        assert_eq!(html.matches("tabindex=\"-1\"").count(), 4, "{html}");
        assert!(html.contains("Clear 5-star rating"), "{html}");
        assert!(html.contains("aria-checked=\"true\""), "{html}");
    }
}
