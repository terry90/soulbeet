use dioxus::prelude::*;

use crate::Button;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    is_selection_empty: bool,
    on_select: EventHandler,
}

#[component]
pub fn AlbumFooter(props: Props) -> Element {
    rsx! {
      div { class: "p-4 border-t border-white/10 mt-auto",
        Button {
          class: "w-full",
          disabled: props.is_selection_empty,
          onclick: move |_| props.on_select.call(()),
          div { class: "flex items-center justify-center gap-2",
            svg {
              class: "w-4 h-4",
              fill: "none",
              stroke: "currentColor",
              view_box: "0 0 24 24",
              path {
                stroke_linecap: "round",
                stroke_linejoin: "round",
                stroke_width: "2",
                d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
              }
            }
            "SEARCH SELECTED"
          }
        }
      }
    }
}
