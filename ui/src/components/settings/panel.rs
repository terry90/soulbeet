//! Markup every settings panel shares.

use dioxus::prelude::*;

pub(super) const LABEL_CLASS: &str = "block text-xs font-medium text-beet-text-secondary mb-1.5";
/// A primary positive action button inside settings panels.
pub(super) const PRIMARY_BUTTON_CLASS: &str = "inline-flex items-center justify-center gap-2 px-4 h-10 text-sm font-semibold rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 transition-[color,background-color,border-color,filter] cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none";
/// A compact primary positive action button (no height/padding class collisions).
pub(super) const COMPACT_PRIMARY_BUTTON_CLASS: &str = "inline-flex items-center justify-center gap-1.5 px-3 h-8 text-xs font-semibold rounded-[8px] bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 transition-[color,background-color,border-color,filter] cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none";
/// A secondary action button inside settings panels.
pub(super) const SECONDARY_BUTTON_CLASS: &str = "inline-flex items-center justify-center gap-2 px-3.5 h-9 text-xs font-medium rounded-[10px] bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none";
/// A compact secondary action button (no height/padding class collisions).
pub(super) const COMPACT_SECONDARY_BUTTON_CLASS: &str = "inline-flex items-center justify-center gap-1.5 px-3 h-8 text-xs font-medium rounded-[8px] bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none";
/// A small text button that removes something.
pub(super) const REMOVE_BUTTON_CLASS: &str = "text-xs font-medium text-beet-text-tertiary hover:text-beet-crimson-text transition-colors cursor-pointer";

/// The unix time `at` in UTC, written with the chrono `format`; the bare
/// number when chrono cannot represent it.
pub(super) fn utc_time(at: i64, format: &str) -> String {
    match chrono::DateTime::from_timestamp(at, 0) {
        Some(time) => time.format(format).to_string(),
        None => at.to_string(),
    }
}

/// A settings card with its title.
#[component]
pub(super) fn Panel(title: String, children: Element) -> Element {
    rsx! {
        div { class: "bg-beet-surface-low border border-beet-border-subtle p-6 rounded-[14px]",
            h2 { class: "text-lg font-semibold mb-4 text-beet-text-primary font-display tracking-tight", "{title}" }
            {children}
        }
    }
}

/// A heading inside a panel, above a list.
#[component]
pub(super) fn SectionTitle(title: String) -> Element {
    rsx! {
        h3 { class: "text-sm font-semibold mb-3 text-beet-text-primary font-display border-b border-beet-border-subtle pb-2",
            "{title}"
        }
    }
}

/// The outcome of the panel's last action; an empty text shows nothing.
#[component]
pub(super) fn Messages(error: String, #[props(default)] success: String) -> Element {
    rsx! {
        if !error.is_empty() {
            div { class: "mb-4 px-3.5 py-3 bg-beet-crimson-surface border border-beet-crimson-solid/40 rounded-[10px] text-beet-crimson-text text-sm",
                "{error}"
            }
        }
        if !success.is_empty() {
            div { class: "mb-4 px-3.5 py-3 bg-beet-leaf-surface border border-beet-leaf-solid/30 rounded-[10px] text-beet-leaf-text text-sm",
                "{success}"
            }
        }
    }
}

/// Shown while a panel's data loads.
#[component]
pub(super) fn Loading() -> Element {
    rsx! {
        div { class: "animate-pulse text-sm text-beet-text-tertiary", "Loading..." }
    }
}

/// A failed load with a way to try again.
#[component]
pub(super) fn LoadError(message: String, on_retry: EventHandler<MouseEvent>) -> Element {
    rsx! {
        div { class: "text-beet-crimson-text text-sm mb-3", "{message}" }
        button {
            class: SECONDARY_BUTTON_CLASS,
            onclick: move |e| on_retry.call(e),
            "Retry"
        }
    }
}

/// One entry of a list in a panel.
#[component]
pub(super) fn ListRow(children: Element) -> Element {
    rsx! {
        li { class: "bg-beet-surface border border-beet-border-subtle p-3.5 rounded-[12px] hover:border-beet-border-default transition-colors",
            {children}
        }
    }
}
