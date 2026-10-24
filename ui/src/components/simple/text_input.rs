use dioxus::prelude::*;

/// The look of the app's form fields, text inputs and selects alike.
pub const FIELD_CLASS: &str = "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono";

#[derive(Props, Clone, PartialEq)]
pub struct TextInputProps {
    #[props(into)]
    pub value: String,
    pub oninput: EventHandler<String>,
    #[props(into)]
    pub placeholder: String,
    /// Names the field for screen readers when no label element does.
    #[props(default, into)]
    pub aria_label: Option<String>,
    /// Hides what is typed.
    #[props(default)]
    pub password: bool,
    /// Runs when Enter is pressed in the field.
    #[props(default)]
    pub on_enter: Option<EventHandler>,
    /// Classes added to the field's own.
    #[props(default, into)]
    pub class: String,
}

/// A one-line text field in the app's field style.
#[component]
pub fn TextInput(props: TextInputProps) -> Element {
    let oninput = props.oninput;
    let on_enter = props.on_enter;
    rsx! {
        input {
            class: "{FIELD_CLASS} {props.class}",
            r#type: if props.password { "password" } else { "text" },
            placeholder: props.placeholder,
            aria_label: props.aria_label,
            value: props.value,
            oninput: move |evt: Event<FormData>| oninput.call(evt.value()),
            onkeydown: move |evt: Event<KeyboardData>| {
                if let (Key::Enter, Some(on_enter)) = (evt.key(), on_enter) {
                    on_enter.call(());
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(password: bool, aria_label: Option<&'static str>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(password, aria_label): (bool, Option<&'static str>)| {
                rsx! {
                    TextInput {
                        value: "typed",
                        oninput: |_| {},
                        placeholder: "Username",
                        aria_label: aria_label.map(str::to_string),
                        password,
                        class: "text-sm",
                    }
                }
            },
            (password, aria_label),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_field_has_the_shared_look_plus_its_own_classes() {
        let html = render(false, None);
        assert!(
            html.contains(&format!("class=\"{FIELD_CLASS} text-sm\"")),
            "{html}"
        );
        assert!(html.contains("type=\"text\""), "{html}");
        assert!(html.contains("placeholder=\"Username\""), "{html}");
        assert!(html.contains("value=\"typed\""), "{html}");
        assert!(!html.contains("aria-label"), "{html}");
    }

    #[test]
    fn a_password_field_hides_what_is_typed() {
        let html = render(true, Some("New password"));
        assert!(html.contains("type=\"password\""), "{html}");
        assert!(html.contains("aria-label=\"New password\""), "{html}");
    }
}
