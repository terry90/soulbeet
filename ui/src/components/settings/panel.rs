//! Markup every settings panel shares.

use dioxus::prelude::*;

pub(super) const LABEL_CLASS: &str =
    "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider";
/// A small text button that removes something.
pub(super) const REMOVE_BUTTON_CLASS: &str = "text-xs font-mono text-gray-400 hover:text-red-400 transition-colors underline decoration-dotted cursor-pointer";

/// A settings card with its title.
#[component]
pub(super) fn Panel(title: String, children: Element) -> Element {
    rsx! {
        div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
            h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "{title}" }
            {children}
        }
    }
}

/// A heading inside a panel, above a list.
#[component]
pub(super) fn SectionTitle(title: String) -> Element {
    rsx! {
        h3 { class: "text-lg font-bold mb-2 text-white font-display border-b border-white/10 pb-2",
            "{title}"
        }
    }
}

/// The outcome of the panel's last action; an empty text shows nothing.
#[component]
pub(super) fn Messages(error: String, #[props(default)] success: String) -> Element {
    rsx! {
        if !error.is_empty() {
            div { class: "mb-4 p-4 bg-red-900/20 border border-red-500/50 rounded text-red-400 font-mono text-sm",
                "{error}"
            }
        }
        if !success.is_empty() {
            div { class: "mb-4 p-4 bg-green-900/20 border border-green-500/50 rounded text-green-400 font-mono text-sm",
                "{success}"
            }
        }
    }
}

/// Shown while a panel's data loads.
#[component]
pub(super) fn Loading() -> Element {
    rsx! {
        div { class: "animate-pulse text-gray-400 font-mono", "Loading..." }
    }
}

/// A failed load with a way to try again.
#[component]
pub(super) fn LoadError(message: String, on_retry: EventHandler<MouseEvent>) -> Element {
    rsx! {
        div { class: "text-red-400 text-sm font-mono mb-3", "{message}" }
        button {
            class: "text-xs font-mono text-gray-400 hover:text-white underline decoration-dotted cursor-pointer",
            onclick: move |e| on_retry.call(e),
            "Retry"
        }
    }
}

/// One entry of a list in a panel.
#[component]
pub(super) fn ListRow(children: Element) -> Element {
    rsx! {
        li { class: "bg-white/5 border border-white/5 p-3 rounded hover:border-beet-accent/30 transition-colors",
            {children}
        }
    }
}
