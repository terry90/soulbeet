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
        use dioxus::core::Mutation;
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        let clicks: Vec<dioxus::core::ElementId> = dom
            .rebuild_to_vec()
            .edits
            .into_iter()
            .filter_map(|edit| match edit {
                Mutation::NewEventListener { name, id } if name == "click" => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(
            clicks.len(),
            1,
            "expected only the toggle to listen for clicks"
        );
        Self {
            dom,
            toggle: clicks[0],
        }
    }

    /// Clicks the toggle and runs what that starts until nothing is left to do.
    pub(crate) fn click(&mut self) {
        use dioxus::html::{PlatformEventData, SerializedMouseData};
        use futures::FutureExt;
        use std::{any::Any, rc::Rc};
        let data = PlatformEventData::new(Box::<SerializedMouseData>::default());
        let event = Event::new(Rc::new(data) as Rc<dyn Any>, true);
        self.dom.runtime().handle_event("click", event, self.toggle);
        loop {
            self.dom.render_immediate(&mut dioxus::core::NoOpMutations);
            if self.dom.wait_for_work().now_or_never().is_none() {
                break;
            }
        }
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
