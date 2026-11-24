use dioxus::prelude::*;

/// A small down arrow for a control that opens a menu or a list.
#[component]
pub fn Chevron(#[props(into)] class: String) -> Element {
    rsx! {
        svg {
            class: "size-[12px] flex-none {class}",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            view_box: "0 0 24 24",
            "aria-hidden": "true",
            path {
                stroke_linecap: "round",
                stroke_linejoin: "round",
                d: "M6 9l6 6 6-6",
            }
        }
    }
}
