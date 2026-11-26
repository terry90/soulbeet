use dioxus::prelude::*;

use crate::IconX;

/// Floating non-blocking Undo toast/snackbar with a smooth linear countdown rail
/// (default 5 seconds, respecting `prefers-reduced-motion`).
#[component]
pub fn UndoSnackbar(
    message: String,
    #[props(default = "Undo".to_string())] action_label: String,
    #[props(default = 5000)] duration_ms: u32,
    on_undo: EventHandler<()>,
    on_dismiss: EventHandler<()>,
    /// Optional unique token to restart the countdown animation when a new action replaces the toast.
    #[props(default)]
    key_id: Option<String>,
    #[props(default)] class: String,
) -> Element {
    let rail_key = key_id.unwrap_or_else(|| message.clone());

    rsx! {
        div {
            role: "status",
            aria_live: "polite",
            aria_atomic: "true",
            class: "relative overflow-hidden rounded-[12px] bg-beet-surface-high border border-beet-border-default shadow-xl max-w-md w-full {class}",

            div { class: "flex items-center justify-between gap-3 px-4 py-3",
                p { class: "text-[13px] font-medium text-beet-text-primary leading-[1.4] min-w-0 flex-1",
                    "{message}"
                }

                div { class: "flex items-center gap-1.5 shrink-0",
                    button {
                        r#type: "button",
                        class: "h-8 px-3 rounded-[8px] tabular-nums-mono text-[12px] font-semibold uppercase tracking-wider text-beet-leaf-text hover:bg-beet-leaf-surface transition-colors duration-150 cursor-pointer",
                        onclick: move |_| on_undo.call(()),
                        "{action_label}"
                    }

                    button {
                        r#type: "button",
                        aria_label: "Dismiss notification",
                        title: "Dismiss",
                        class: "w-8 h-8 rounded-[8px] inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-highest transition-colors duration-150 cursor-pointer",
                        onclick: move |_| on_dismiss.call(()),
                        IconX { size: 15 }
                    }
                }
            }

            // Linear shrinking countdown rail along bottom edge
            div {
                class: "h-[2px] w-full bg-beet-border-subtle overflow-hidden",
                aria_hidden: "true",
                div {
                    key: "{rail_key}",
                    class: "h-full w-full bg-beet-leaf-solid beet-countdown-rail",
                    style: "--beet-countdown-duration: {duration_ms}ms;",
                }
            }
        }
    }
}
