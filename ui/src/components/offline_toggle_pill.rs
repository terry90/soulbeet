use dioxus::prelude::*;

use crate::IconWifiOff;

/// Formats the optional cached telemetry suffix for [`OfflineTogglePill`].
pub fn format_offline_telemetry(
    cached_tracks: Option<u32>,
    cache_size_label: Option<&str>,
    compact: bool,
) -> Option<String> {
    if compact {
        return None;
    }
    let clean_size = cache_size_label.map(str::trim).filter(|s| !s.is_empty());
    match (cached_tracks, clean_size) {
        (Some(count), Some(size)) => Some(format!("{count} · {size}")),
        (Some(count), None) => Some(format!("{count} cached")),
        (None, Some(size)) => Some(size.to_string()),
        (None, None) => None,
    }
}

/// Compact global connectivity toggle pill for Desktop's Online / Offline Only mode.
///
/// Clearly states the active mode so users never confuse "Online" with "Offline":
/// - Online (`offline_only == false`): green indicator dot + `"Online"`.
/// - Offline Only (`offline_only == true`): amber/gold `IconWifiOff` + `"Offline Only"`.
#[component]
pub fn OfflineTogglePill(
    offline_only: bool,
    on_toggle: EventHandler<bool>,
    #[props(default)] cached_tracks: Option<u32>,
    #[props(default)] cache_size_label: Option<String>,
    #[props(default)] compact: bool,
    #[props(default)] class: String,
) -> Element {
    let surface_class = if offline_only {
        "bg-beet-gold-surface border-beet-gold/45 text-beet-gold hover:brightness-110"
    } else {
        "bg-beet-surface border-beet-border-default text-beet-text-primary hover:bg-beet-surface-high"
    };

    let telemetry = format_offline_telemetry(cached_tracks, cache_size_label.as_deref(), compact);

    let tooltip = if offline_only {
        "Offline Only mode active - click to go back Online"
    } else {
        "Online mode - click to switch to Offline Only"
    };

    rsx! {
        button {
            r#type: "button",
            role: "switch",
            aria_checked: if offline_only { "true" } else { "false" },
            aria_label: "{tooltip}",
            title: "{tooltip}",
            class: "inline-flex items-center gap-2 rounded-full border px-3 h-9 text-[13px] font-medium transition-[color,background-color,border-color] duration-150 cursor-pointer shrink-0 {surface_class} {class}",
            onclick: move |_| on_toggle.call(!offline_only),
            if offline_only {
                IconWifiOff { size: 15 }
                span { "Offline Only" }
            } else {
                span { class: "w-2 h-2 rounded-full shrink-0 bg-beet-leaf-glow", aria_hidden: "true" }
                span { "Online" }
            }
            if let Some(meta) = telemetry {
                span { class: "tabular-nums-mono text-[11px] font-semibold tracking-wider opacity-80 pl-1 border-l border-current/20",
                    "{meta}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_whitespace_only_cache_size_label() {
        assert_eq!(
            format_offline_telemetry(Some(42), Some("   "), false),
            Some("42 cached".to_string())
        );
        assert_eq!(
            format_offline_telemetry(Some(42), Some(" 4.2 GB "), false),
            Some("42 · 4.2 GB".to_string())
        );
        assert_eq!(format_offline_telemetry(None, Some("   "), false), None);
        assert_eq!(
            format_offline_telemetry(Some(42), Some("4.2 GB"), true),
            None
        );
    }

    #[test]
    fn renders_clear_online_and_offline_only_states() {
        let mut online_dom = VirtualDom::new(|| {
            rsx! {
                OfflineTogglePill {
                    offline_only: false,
                    on_toggle: |_| {},
                }
            }
        });
        online_dom.rebuild_in_place();
        let online_html = dioxus_ssr::render(&online_dom);
        assert!(online_html.contains(">Online</span>"), "{online_html}");
        assert!(online_html.contains("bg-beet-leaf-glow"), "{online_html}");
        assert!(!online_html.contains(">Offline</span>"), "{online_html}");
        assert!(!online_html.contains("Offline Only</span>"), "{online_html}");

        let mut offline_dom = VirtualDom::new(|| {
            rsx! {
                OfflineTogglePill {
                    offline_only: true,
                    on_toggle: |_| {},
                }
            }
        });
        offline_dom.rebuild_in_place();
        let offline_html = dioxus_ssr::render(&offline_dom);
        assert!(offline_html.contains(">Offline Only</span>"), "{offline_html}");
        assert!(offline_html.contains("bg-beet-gold-surface"), "{offline_html}");
        assert!(offline_html.contains("text-beet-gold"), "{offline_html}");
        assert!(!offline_html.contains("amber-"), "{offline_html}");
    }
}
