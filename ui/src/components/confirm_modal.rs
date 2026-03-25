use dioxus::prelude::*;

use crate::Modal;

#[derive(Props, PartialEq, Clone)]
pub struct ConfirmModalProps {
    pub message: String,
    #[props(default = "Confirm".to_string())]
    pub confirm_label: String,
    #[props(default = false)]
    pub danger: bool,
    pub on_confirm: EventHandler,
    pub on_cancel: EventHandler,
}

#[component]
pub fn ConfirmModal(props: ConfirmModalProps) -> Element {
    let confirm_class = if props.danger {
        "px-3 py-1.5 text-sm font-mono rounded bg-red-900/50 text-red-400 hover:bg-red-800/50 cursor-pointer"
    } else {
        "retro-btn rounded text-sm"
    };

    rsx! {
        Modal {
            on_close: move |_| props.on_cancel.call(()),
            header: rsx! {
                h2 { class: "text-lg font-bold text-white font-display", "Confirm" }
            },
            p { class: "text-gray-300 text-sm font-mono mb-4", "{props.message}" }
            div { class: "flex justify-end gap-2",
                button {
                    class: "px-3 py-1.5 text-sm font-mono text-gray-400 hover:text-white transition-colors cursor-pointer",
                    onclick: move |_| props.on_cancel.call(()),
                    "Cancel"
                }
                button {
                    class: confirm_class,
                    onclick: move |_| props.on_confirm.call(()),
                    "{props.confirm_label}"
                }
            }
        }
    }
}
