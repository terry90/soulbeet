use dioxus::prelude::*;

use crate::IconWifiOff;

/// Compact global toggle pill for Soulbeet's Offline Only mode.
///
/// When enabled, highlights with `beet-leaf` tonal surface and status indicator dot,
/// signaling that Library/Home/Search are filtered to locally cached items.
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
        "bg-beet-leaf-surface border-beet-leaf-solid/40 text-beet-leaf-text"
    } else {
        "bg-beet-surface border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary"
    };

    let dot_class = if offline_only {
        "bg-beet-leaf-glow"
    } else {
        "bg-beet-text-disabled"
    };

    let telemetry = if compact {
        None
    } else {
        match (cached_tracks, cache_size_label.as_deref()) {
            (Some(count), Some(size)) => Some(format!("{count} · {size}")),
            (Some(count), None) => Some(format!("{count} cached")),
            (None, Some(size)) => Some(size.to_string()),
            (None, None) => None,
        }
    };

    let aria_label = if offline_only {
        "Offline only mode enabled. Click to go back online."
    } else {
        "Offline only mode disabled. Click to filter to cached tracks."
    };

    rsx! {
        button {
            r#type: "button",
            role: "switch",
            aria_checked: if offline_only { "true" } else { "false" },
            aria_label: "{aria_label}",
            title: "{aria_label}",
            class: "inline-flex items-center gap-2 rounded-full border px-3 h-9 text-[13px] font-medium transition-[color,background-color,border-color] duration-150 cursor-pointer shrink-0 {surface_class} {class}",
            onclick: move |_| on_toggle.call(!offline_only),
            span { class: "w-2 h-2 rounded-full shrink-0 {dot_class}", aria_hidden: "true" }
            IconWifiOff { size: 15 }
            span { "Offline" }
            if let Some(meta) = telemetry {
                span { class: "tabular-nums-mono text-[11px] font-semibold tracking-wider opacity-80 pl-1 border-l border-current/20",
                    "{meta}"
                }
            }
        }
    }
}
