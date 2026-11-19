use dioxus::prelude::*;

/// A sliding switch; `on_class` is the track colour while it is on.
#[component]
pub fn Toggle(on: bool, on_class: &'static str, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button {
            class: format!(
                "relative w-11 h-6 rounded-full cursor-pointer transition-colors {}",
                if on { on_class } else { "bg-gray-600" }
            ),
            onclick: move |e| onclick.call(e),
            span {
                class: format!(
                    "absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full transition-transform {}",
                    if on { "translate-x-5" } else { "" }
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(app: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_toggle_shows_its_state() {
        let on = render(|| rsx! { Toggle { on: true, on_class: "bg-red-500", onclick: |_| {} } });
        assert!(on.contains("bg-red-500") && on.contains("translate-x-5"));
        let off = render(|| rsx! { Toggle { on: false, on_class: "bg-red-500", onclick: |_| {} } });
        assert!(off.contains("bg-gray-600") && !off.contains("translate-x-5"));
    }
}
