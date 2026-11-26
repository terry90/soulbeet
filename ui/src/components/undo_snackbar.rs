use dioxus::core::Task;
use dioxus::prelude::*;

use crate::IconX;

#[cfg(target_arch = "wasm32")]
async fn sleep_ms(duration_ms: u32) {
    gloo_timers::future::TimeoutFuture::new(duration_ms).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_ms(duration_ms: u32) {
    // Only sleep when running under a Tokio runtime (e.g. dioxus-desktop or tokio tests).
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(duration_ms))).await;
    }
}

/// Floating non-blocking Undo toast/snackbar with an automatic dismiss timer and a smooth
/// linear countdown rail (default 5 seconds, respecting `prefers-reduced-motion`).
///
/// Setting `duration_ms = 0` disables both the auto-dismiss timer and the countdown rail.
#[component]
pub fn UndoSnackbar(
    message: String,
    #[props(default = "Undo".to_string())] action_label: String,
    #[props(default = 5000)] duration_ms: u32,
    on_undo: EventHandler<()>,
    on_dismiss: EventHandler<()>,
    /// Optional unique token to restart the countdown animation and auto-dismiss timer
    /// when a new action replaces the toast.
    #[props(default)]
    key_id: Option<String>,
    #[props(default)] class: String,
) -> Element {
    let rail_key = key_id.unwrap_or_else(|| message.clone());

    let mut active_key = use_signal(|| rail_key.clone());
    let mut active_duration = use_signal(|| duration_ms);
    let mut settled_key = use_signal(|| None::<String>);
    let mut timer_task = use_signal(|| None::<Task>);

    if *active_key.peek() != rail_key {
        active_key.set(rail_key.clone());
        settled_key.set(None);
    }
    if *active_duration.peek() != duration_ms {
        active_duration.set(duration_ms);
    }

    // Reactive auto-dismiss timer: runs only in mounted interactive renderers (never during SSR),
    // automatically cancels when `(rail_key, duration_ms)` changes or when the snackbar unmounts,
    // and never fires when `duration_ms == 0` or after Undo/Dismiss already settled this toast.
    use_effect(move || {
        let current_key = active_key();
        let current_ms = active_duration();

        if let Some(prev) = timer_task.take() {
            prev.cancel();
        }
        if current_ms == 0 {
            return;
        }
        if settled_key.peek().as_deref() == Some(current_key.as_str()) {
            return;
        }

        let task = spawn(async move {
            sleep_ms(current_ms).await;
            if active_key.peek().as_str() == current_key.as_str()
                && settled_key.peek().as_deref() != Some(current_key.as_str())
            {
                settled_key.set(Some(current_key));
                on_dismiss.call(());
            }
        });
        timer_task.set(Some(task));
    });

    let undo_key = rail_key.clone();
    let dismiss_key = rail_key.clone();

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
                        onclick: move |_| {
                            if let Some(prev) = timer_task.take() {
                                prev.cancel();
                            }
                            if settled_key.peek().as_deref() != Some(undo_key.as_str()) {
                                settled_key.set(Some(undo_key.clone()));
                                on_undo.call(());
                            }
                        },
                        "{action_label}"
                    }

                    button {
                        r#type: "button",
                        aria_label: "Dismiss notification",
                        title: "Dismiss",
                        class: "w-8 h-8 rounded-[8px] inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-highest transition-colors duration-150 cursor-pointer",
                        onclick: move |_| {
                            if let Some(prev) = timer_task.take() {
                                prev.cancel();
                            }
                            if settled_key.peek().as_deref() != Some(dismiss_key.as_str()) {
                                settled_key.set(Some(dismiss_key.clone()));
                                on_dismiss.call(());
                            }
                        },
                        IconX { size: 15 }
                    }
                }
            }

            // Linear shrinking countdown rail along bottom edge (single-element iterator so Dioxus 0.7 honors `key`).
            if duration_ms > 0 {
                div {
                    class: "h-[2px] w-full bg-beet-border-subtle overflow-hidden",
                    aria_hidden: "true",
                    for k in [rail_key.as_str()] {
                        div {
                            key: "{k}",
                            class: "h-full w-full bg-beet-leaf-solid beet-countdown-rail",
                            style: "--beet-countdown-duration: {duration_ms}ms;",
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn omits_countdown_rail_when_duration_is_zero_and_renders_when_positive() {
        let mut dom_zero = VirtualDom::new(|| {
            rsx! {
                UndoSnackbar {
                    message: "Pinned forever".to_string(),
                    duration_ms: 0,
                    on_undo: |_| {},
                    on_dismiss: |_| {},
                }
            }
        });
        dom_zero.rebuild_in_place();
        let html_zero = dioxus_ssr::render(&dom_zero);
        assert!(html_zero.contains("Pinned forever"));
        assert!(!html_zero.contains("beet-countdown-rail"));

        let mut dom_timed = VirtualDom::new(|| {
            rsx! {
                UndoSnackbar {
                    message: "Dropped track".to_string(),
                    duration_ms: 5000,
                    key_id: Some("drop-1".to_string()),
                    on_undo: |_| {},
                    on_dismiss: |_| {},
                }
            }
        });
        dom_timed.rebuild_in_place();
        let html_timed = dioxus_ssr::render(&dom_timed);
        assert!(html_timed.contains("beet-countdown-rail"));
        assert!(html_timed.contains("--beet-countdown-duration: 5000ms;"));
    }

    #[tokio::test]
    async fn auto_dismiss_fires_once_after_duration() {
        let dismissed = Arc::new(AtomicUsize::new(0));
        let dismissed_cb = dismissed.clone();

        let mut dom = VirtualDom::new_with_props(
            |counter: Arc<AtomicUsize>| {
                rsx! {
                    UndoSnackbar {
                        message: "Dropped track".to_string(),
                        duration_ms: 25,
                        key_id: Some("k1".to_string()),
                        on_undo: |_| {},
                        on_dismiss: move |_| {
                            counter.fetch_add(1, Ordering::SeqCst);
                        },
                    }
                }
            },
            dismissed_cb,
        );
        dom.rebuild_in_place();
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(200), dom.wait_for_work()).await;
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        dom.render_immediate(&mut dioxus::core::NoOpMutations);

        assert_eq!(dismissed.load(Ordering::SeqCst), 1);
    }
}
