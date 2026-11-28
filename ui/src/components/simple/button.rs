use dioxus::prelude::*;

#[derive(Clone, PartialEq, Default)]
pub enum ButtonVariant {
    #[default]
    Primary,
    Secondary,
}

impl ButtonVariant {
    fn get_classes(&self, disabled: bool) -> &'static str {
        match (self, disabled) {
            (ButtonVariant::Primary, false) => "inline-flex items-center justify-center gap-2 px-4 h-10 text-sm font-semibold rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 transition-[color,background-color,border-color,filter] cursor-pointer",
            (ButtonVariant::Primary, true) => "inline-flex items-center justify-center gap-2 px-4 h-10 text-sm font-semibold rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink transition-[color,background-color,border-color,filter]",
            (ButtonVariant::Secondary, false) => "inline-flex items-center justify-center gap-2 px-4 h-10 text-sm font-medium rounded-[10px] bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:bg-beet-surface-high hover:text-beet-text-primary transition-colors cursor-pointer",
            (ButtonVariant::Secondary, true) => "inline-flex items-center justify-center gap-2 px-4 h-10 text-sm font-medium rounded-[10px] bg-beet-surface border border-beet-border-default text-beet-text-secondary transition-colors",
        }
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct Props {
    children: Element,
    /// Leave out for a submit button, whose form handles the press.
    #[props(optional)]
    onclick: Option<EventHandler<MouseEvent>>,
    #[props(optional, default)]
    variant: ButtonVariant,
    #[props(optional, default)]
    disabled: bool,
    /// Submits the form it is in.
    #[props(optional, default)]
    submit: bool,
    /// The action it started is under way: disabled, but not dimmed.
    #[props(optional, default)]
    busy: bool,
    #[props(optional, into)]
    class: String,
}

#[component]
pub fn Button(props: Props) -> Element {
    let disabled = props.disabled || props.busy;
    let variant_classes = props.variant.get_classes(disabled);
    let disabled_classes = if props.busy {
        "opacity-80 cursor-not-allowed pointer-events-none"
    } else if props.disabled {
        "opacity-50 cursor-not-allowed grayscale pointer-events-none"
    } else {
        ""
    };
    let additional_classes = props.class;
    let onclick = props.onclick;

    rsx! {
        button {
            class: "{variant_classes} {disabled_classes} {additional_classes}",
            // Preserves the `search_bar.rs` unit-test selector without applying legacy `.retro-btn` CSS.
            "data-button": (props.variant == ButtonVariant::Primary).then_some("retro-btn"),
            r#type: if props.submit { "submit" } else { "button" },
            onclick: move |evt| {
                if let (false, Some(onclick)) = (disabled, onclick) {
                    onclick.call(evt)
                }
            },
            disabled,
            {props.children}
        }
    }
}
