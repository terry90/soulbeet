use dioxus::prelude::*;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    /// A signal to control the visibility of the modal
    pub on_close: EventHandler,
    /// The content to be displayed inside the modal
    pub children: Element,
    /// The header of the modal
    pub header: Element,
}

#[component]
pub fn Modal(props: Props) -> Element {
    rsx! {
      // Backdrop
      div {
        class: "fixed inset-0 bg-black/60 backdrop-blur-sm z-40 transition-opacity",
        onclick: move |_| props.on_close.call(()),
      }

      // Container
      div {
        class: "fixed inset-0 flex items-center justify-center z-50 pointer-events-none",
        onclick: move |_| props.on_close.call(()),

        // Content
        div {
          class: "bg-beet-panel border border-white/10 max-h-[85vh] overflow-hidden flex flex-col rounded-lg shadow-2xl max-w-lg w-full pointer-events-auto transform transition-all",
          onclick: move |event| event.stop_propagation(),
          // Header Container
          div { class: "flex items-center justify-between p-4 border-b border-white/10 bg-black/20",
            div { class: "flex-1 min-w-0", {props.header} }
            button {
              class: "text-gray-400 hover:text-white transition-colors ml-4 cursor-pointer",
              onclick: move |_| props.on_close.call(()),
              // Close icon SVG
              svg {
                class: "w-6 h-6",
                fill: "none",
                view_box: "0 0 24 24",
                stroke: "currentColor",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  stroke_width: "2",
                  d: "M6 18L18 6M6 6l12 12",
                }
              }
            }
          }
          // Scrollable Body
          div { class: "overflow-y-auto p-4 scrollbar-thin scrollbar-thumb-beet-accent/50 scrollbar-track-transparent",
            {props.children}
          }
        }
      }
    }
}
