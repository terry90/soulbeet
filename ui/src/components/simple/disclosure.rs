use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

/// Awaited before the body opens. Resolves to whether to open it.
pub type OpenHandler = Callback<(), Pin<Box<dyn Future<Output = bool>>>>;

/// An `on_open` handler that runs `load` on every open.
pub fn use_open_handler<F>(mut load: impl FnMut() -> F + 'static) -> OpenHandler
where
    F: Future<Output = bool> + 'static,
{
    use_callback(move |()| Box::pin(load()) as Pin<Box<dyn Future<Output = bool>>>)
}

#[derive(Props, Clone, PartialEq)]
pub struct DisclosureProps {
    /// Noun phrase; the toggle reads "Show {label}" / "Hide {label}".
    #[props(into)]
    pub label: String,
    /// Runs on every open; the toggle reads "Loading..." and is disabled
    /// until it finishes, and the body opens only if it says so.
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
                    let opens = load.call(()).await;
                    loading.set(false);
                    open.set(opens);
                });
            }
            None => open.set(true),
        }
    };

    let label = &props.label;
    rsx! {
        div { class: "{props.class}",
            button {
                class: "text-xs font-medium text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer underline decoration-dotted underline-offset-4 transition-colors disabled:cursor-wait",
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

/// Clicks a rendered Disclosure's toggle in tests.
#[cfg(test)]
pub(crate) struct ToggleDriver {
    dom: VirtualDom,
    toggle: dioxus::core::ElementId,
}

#[cfg(test)]
impl ToggleDriver {
    /// Renders `dom`, whose only click listener must be the toggle.
    pub(crate) fn new(mut dom: VirtualDom) -> Self {
        let clicks = super::testing::listeners(&mut dom, "click");
        assert_eq!(
            clicks.len(),
            1,
            "expected only the toggle to listen for clicks"
        );
        // What the first render started (a resource, an effect) ends before any click.
        super::testing::settle(&mut dom);
        Self {
            dom,
            toggle: clicks[0],
        }
    }

    /// Clicks the toggle and runs what that starts until nothing is left to do.
    pub(crate) fn click(&mut self) {
        super::testing::click(&mut self.dom, self.toggle);
    }

    pub(crate) fn html(&self) -> String {
        dioxus_ssr::render(&self.dom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(opens: bool) -> ToggleDriver {
        ToggleDriver::new(VirtualDom::new_with_props(
            |opens: bool| {
                let on_open = use_open_handler(move || async move { opens });
                rsx! {
                    Disclosure { label: "matches", on_open,
                        p { "the body" }
                    }
                }
            },
            opens,
        ))
    }

    #[test]
    fn the_body_opens_only_when_the_handler_says_so() {
        let mut kept_shut = driver(false);
        kept_shut.click();
        let html = kept_shut.html();
        assert!(!html.contains("the body"), "{html}");
        assert!(html.contains("Show matches"), "{html}");
        assert!(html.contains("aria-expanded=false"), "{html}");

        let mut opened = driver(true);
        opened.click();
        let html = opened.html();
        assert!(html.contains("the body"), "{html}");
        assert!(html.contains("Hide matches"), "{html}");
    }
}
