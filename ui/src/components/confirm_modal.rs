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
        "px-4 py-2 text-sm font-semibold rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-solid/40 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white transition-colors cursor-pointer"
    } else {
        "px-4 py-2 text-sm font-semibold rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 transition-[color,background-color,border-color,filter] cursor-pointer"
    };

    rsx! {
        Modal {
            on_close: move |_| props.on_cancel.call(()),
            header: rsx! {
                h2 { class: "text-base font-semibold text-beet-text-primary font-display", "Confirm" }
            },
            p { class: "text-beet-text-secondary text-sm leading-relaxed mb-5", "{props.message}" }
            div { class: "flex justify-end gap-2.5",
                button {
                    class: "px-4 py-2 text-sm font-medium rounded-[10px] bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary transition-colors cursor-pointer",
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
