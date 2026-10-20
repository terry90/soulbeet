use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

/// Awaited before the body opens.
pub type OpenHandler = Callback<(), Pin<Box<dyn Future<Output = ()>>>>;

/// An `on_open` handler that runs `load` on every open.
pub fn use_open_handler<F>(mut load: impl FnMut() -> F + 'static) -> OpenHandler
where
    F: Future<Output = ()> + 'static,
{
    use_callback(move |()| Box::pin(load()) as Pin<Box<dyn Future<Output = ()>>>)
}

#[derive(Props, Clone, PartialEq)]
pub struct DisclosureProps {
    /// Noun phrase; the toggle reads "Show {label}" / "Hide {label}".
    #[props(into)]
    pub label: String,
    /// Runs on every open; the toggle reads "Loading..." and is disabled
    /// until it finishes.
    #[props(default)]
    pub on_open: Option<OpenHandler>,
    #[props(default, into)]
    pub class: String,
    pub children: Element,
}

/// A text toggle that shows its children only while open.
#[component]
pub fn Disclosure(props: DisclosureProps) -> Element {
    let mut open = use_signal(|| false);
    let mut loading = use_signal(|| false);
    let on_open = props.on_open;

    let toggle = move |_| {
        if open() {
            open.set(false);
            return;
        }
        match on_open {
            Some(load) => {
                loading.set(true);
                spawn(async move {
                    load.call(()).await;
                    loading.set(false);
                    open.set(true);
                });
            }
            None => open.set(true),
        }
    };

    let label = &props.label;
    rsx! {
        div { class: "{props.class}",
            button {
                class: "text-xs font-mono text-gray-500 hover:text-gray-300 cursor-pointer underline decoration-dotted disabled:cursor-wait",
                aria_expanded: open(),
                aria_busy: loading(),
                disabled: loading(),
                onclick: toggle,
                if loading() {
                    "Loading..."
                } else if open() {
                    "Hide {label}"
                } else {
                    "Show {label}"
                }
            }
            if open() {
                {props.children}
            }
        }
    }
}
