use dioxus::prelude::*;

#[derive(Clone, PartialEq, Default)]
pub enum ButtonVariant {
    #[default]
    Primary,
    Secondary,
}

impl ButtonVariant {
    fn get_classes(&self) -> &'static str {
        match self {
            ButtonVariant::Primary => "retro-btn",
            // Keep secondary distinctive or map to same if unused? Let's make it a outlined version or just dimmer
            ButtonVariant::Secondary => "font-mono uppercase text-xs tracking-widest px-6 py-3 border border-white/10 text-gray-400 transition-all duration-200 hover:bg-white/5 hover:text-white cursor-pointer",
        }
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct Props {
    children: Element,
    #[props(into)]
    onclick: EventHandler<MouseEvent>,
    #[props(optional, default)]
    variant: ButtonVariant,
    #[props(optional, default)]
    disabled: bool,
    #[props(optional, into)]
    class: String,
}

#[component]
pub fn Button(props: Props) -> Element {
    let variant_classes = props.variant.get_classes();
    let disabled_classes = if props.disabled {
        "opacity-30 cursor-not-allowed grayscale pointer-events-none"
    } else {
        ""
    };
    let additional_classes = props.class;

    rsx! {
        button {
            class: "{variant_classes} {disabled_classes} {additional_classes} rounded",
            onclick: move |evt| {
                if !props.disabled {
                    props.onclick.call(evt)
                }
            },
            disabled: props.disabled,
            {props.children}
        }
    }
}
