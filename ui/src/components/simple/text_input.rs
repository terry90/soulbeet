use dioxus::prelude::*;

/// The look of the app's form fields, text inputs and selects alike.
pub const FIELD_CLASS: &str = "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono";

/// How a text field looks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FieldLook {
    /// A form field, as [`FIELD_CLASS`].
    #[default]
    Form,
    /// A smaller field in a list item being edited.
    Compact,
    /// The login form's larger field that glows on focus.
    Login,
    /// A borderless field that fills a row of the search bar. Its text
    /// colour comes from the added classes.
    Bar,
}

impl FieldLook {
    fn class(self) -> &'static str {
        match self {
            FieldLook::Form => FIELD_CLASS,
            FieldLook::Compact => "p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent text-white font-mono text-sm",
            FieldLook::Login => "w-full bg-beet-dark border border-white/10 rounded p-3 text-white focus:outline-none focus:border-beet-accent focus:shadow-[0_0_10px_rgba(217,70,239,0.3)] transition-all font-mono",
            FieldLook::Bar => "flex-1 min-w-0 h-[44px] p-0 bg-transparent border-0 rounded-none outline-none appearance-none font-mono text-base placeholder:text-gray-600 [&::-webkit-search-cancel-button]:appearance-none",
        }
    }
}

/// What kind of text a field takes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FieldKind {
    #[default]
    Text,
    /// Hides what is typed.
    Password,
    /// Words to search for.
    Search,
}

impl FieldKind {
    fn input_type(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Password => "password",
            FieldKind::Search => "search",
        }
    }
}

/// What a field holds, so a password manager knows what to fill in or save.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Autofill {
    /// The username of the account signing in.
    Username,
    /// The password the account signs in with now.
    CurrentPassword,
    /// A password being chosen.
    NewPassword,
    /// Someone else's secret, such as a password an admin sets for another
    /// account: the browser must not fill in or save the signed-in user's own.
    Off,
}

impl Autofill {
    fn token(self) -> &'static str {
        match self {
            Autofill::Username => "username",
            Autofill::CurrentPassword => "current-password",
            Autofill::NewPassword => "new-password",
            Autofill::Off => "off",
        }
    }
}

/// What the submitted form holds under `name`: the field's text at that
/// moment, typed or filled in by a password manager without an input event.
pub fn submitted_text(form: &FormData, name: &str) -> String {
    match form.get_first(name) {
        Some(FormValue::Text(text)) => text,
        _ => String::new(),
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct TextInputProps {
    /// The field's text, which `oninput` keeps up to date. Leave both out
    /// for a field its form reads when submitted: the field then keeps
    /// whatever is typed or filled in, and no render writes over it.
    #[props(default, into)]
    pub value: Option<String>,
    #[props(default)]
    pub oninput: Option<EventHandler<String>>,
    #[props(into)]
    pub placeholder: String,
    /// The id a `label` names in its `for`.
    #[props(default, into)]
    pub id: Option<String>,
    /// The key its form submits the text under.
    #[props(default, into)]
    pub name: Option<String>,
    #[props(default)]
    pub autofill: Option<Autofill>,
    /// Names the field for screen readers when no label element does.
    #[props(default, into)]
    pub aria_label: Option<String>,
    #[props(default)]
    pub kind: FieldKind,
    /// What the Enter key on a phone keyboard says, such as "search".
    #[props(default)]
    pub enterkeyhint: Option<&'static str>,
    /// Runs when Enter is pressed in the field.
    #[props(default)]
    pub on_enter: Option<EventHandler>,
    #[props(default)]
    pub onmounted: Option<EventHandler<MountedEvent>>,
    #[props(default)]
    pub look: FieldLook,
    /// Classes added to the look's own.
    #[props(default, into)]
    pub class: String,
}

/// A one-line text field in the app's field style.
#[component]
pub fn TextInput(props: TextInputProps) -> Element {
    let oninput = props.oninput;
    let on_enter = props.on_enter;
    let onmounted = props.onmounted;
    let class = match props.class.is_empty() {
        true => props.look.class().to_string(),
        false => format!("{} {}", props.look.class(), props.class),
    };
    rsx! {
        input {
            class,
            id: props.id,
            name: props.name,
            r#type: props.kind.input_type(),
            enterkeyhint: props.enterkeyhint,
            autocomplete: props.autofill.map(Autofill::token),
            placeholder: props.placeholder,
            aria_label: props.aria_label,
            value: props.value,
            oninput: move |evt: Event<FormData>| {
                if let Some(oninput) = oninput {
                    oninput.call(evt.value());
                }
            },
            onkeydown: move |evt: Event<KeyboardData>| {
                if let (Key::Enter, Some(on_enter)) = (evt.key(), on_enter) {
                    on_enter.call(());
                }
            },
            onmounted: move |evt: MountedEvent| {
                if let Some(onmounted) = onmounted {
                    onmounted.call(evt);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(kind: FieldKind, aria_label: Option<&'static str>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(kind, aria_label): (FieldKind, Option<&'static str>)| {
                rsx! {
                    TextInput {
                        value: "typed",
                        oninput: |_| {},
                        placeholder: "Username",
                        aria_label: aria_label.map(str::to_string),
                        kind,
                        class: "text-sm",
                    }
                }
            },
            (kind, aria_label),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_field_has_the_shared_look_plus_its_own_classes() {
        let html = render(FieldKind::Text, None);
        assert!(
            html.contains(&format!("class=\"{FIELD_CLASS} text-sm\"")),
            "{html}"
        );
        assert!(html.contains("type=\"text\""), "{html}");
        assert!(html.contains("placeholder=\"Username\""), "{html}");
        assert!(html.contains("value=\"typed\""), "{html}");
        assert!(!html.contains("aria-label"), "{html}");
        assert!(!html.contains("id="), "{html}");
        assert!(!html.contains("name="), "{html}");
        assert!(!html.contains("autocomplete"), "{html}");
        assert!(!html.contains("enterkeyhint"), "{html}");
    }

    #[test]
    fn a_field_its_form_reads_has_no_value_to_write_back() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                TextInput {
                    id: "field",
                    name: "secret",
                    autofill: Autofill::CurrentPassword,
                    placeholder: "Password",
                    kind: FieldKind::Password,
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for attribute in [
            "id=\"field\"",
            "name=\"secret\"",
            "type=\"password\"",
            "autocomplete=\"current-password\"",
        ] {
            assert!(html.contains(attribute), "{attribute} in {html}");
        }
        assert!(!html.contains("value"), "{html}");
    }

    #[test]
    fn a_password_field_hides_what_is_typed() {
        let html = render(FieldKind::Password, Some("New password"));
        assert!(html.contains("type=\"password\""), "{html}");
        assert!(html.contains("aria-label=\"New password\""), "{html}");
    }

    #[test]
    fn a_search_field_asks_phones_for_a_search_key() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                TextInput {
                    kind: FieldKind::Search,
                    enterkeyhint: "search",
                    placeholder: "Search albums",
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("type=\"search\""), "{html}");
        assert!(html.contains("enterkeyhint=\"search\""), "{html}");
    }

    fn render_look(look: FieldLook) -> String {
        let mut dom = VirtualDom::new_with_props(
            |look: FieldLook| {
                rsx! {
                    TextInput { look, value: "", oninput: |_| {}, placeholder: "Name" }
                }
            },
            look,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn each_look_is_its_class_alone_without_added_classes() {
        for (look, class) in [
            (FieldLook::Form, FIELD_CLASS),
            (
                FieldLook::Compact,
                "p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent text-white font-mono text-sm",
            ),
            (
                FieldLook::Login,
                "w-full bg-beet-dark border border-white/10 rounded p-3 text-white focus:outline-none focus:border-beet-accent focus:shadow-[0_0_10px_rgba(217,70,239,0.3)] transition-all font-mono",
            ),
        ] {
            let html = render_look(look);
            assert!(html.contains(&format!("class=\"{class}\"")), "{html}");
        }
    }
}
