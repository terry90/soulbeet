use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct RowActionProps {
    /// An action of the row is running; every action waits for it.
    #[props(default)]
    pub busy: bool,
    /// The action cannot run yet.
    #[props(default)]
    pub disabled: bool,
    pub onclick: EventHandler,
    pub children: Element,
}

/// A small text button for an action on a list row.
#[component]
pub fn RowAction(props: RowActionProps) -> Element {
    let cursor = if props.busy {
        "cursor-wait"
    } else {
        "cursor-pointer disabled:cursor-not-allowed"
    };
    rsx! {
        button {
            class: "text-xs font-mono px-2 py-1 rounded border border-white/10 text-gray-300 enabled:hover:border-beet-accent/50 enabled:hover:text-white transition-colors disabled:opacity-50 {cursor}",
            disabled: props.busy || props.disabled,
            aria_busy: props.busy,
            onclick: move |evt: Event<MouseData>| {
                evt.stop_propagation();
                props.onclick.call(());
            },
            {props.children}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(busy: bool, disabled: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(busy, disabled): (bool, bool)| {
                rsx! {
                    RowAction { busy, disabled, onclick: |_| {}, "Retry import" }
                }
            },
            (busy, disabled),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn busy_and_disabled_both_disable_but_only_busy_waits() {
        let idle = render(false, false);
        assert!(idle.contains("Retry import"));
        assert!(!idle.contains("disabled=true"));
        assert!(idle.contains("cursor-pointer"));

        let busy = render(true, false);
        assert!(busy.contains("disabled=true"));
        assert!(busy.contains("aria-busy=true"));
        assert!(busy.contains("cursor-wait"));

        let not_ready = render(false, true);
        assert!(not_ready.contains("disabled=true"));
        assert!(!not_ready.contains("aria-busy=true"));
        assert!(!not_ready.contains("cursor-wait"));
    }
}
