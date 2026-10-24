use dioxus::prelude::*;

/// One option of a `ChoiceList`.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub title: String,
    /// Shown right of the title, such as a score.
    pub aside: Option<String>,
    /// A muted second line.
    pub detail: Option<String>,
    /// A small last line.
    pub note: Option<String>,
}

#[derive(Props, Clone, PartialEq)]
pub struct ChoiceListProps {
    /// Groups the radios, so it must be unique on the page.
    #[props(into)]
    pub name: String,
    /// What the group picks, for screen readers.
    #[props(into)]
    pub label: String,
    pub choices: Vec<Choice>,
    pub selected: Option<usize>,
    pub on_select: EventHandler<usize>,
    #[props(default, into)]
    pub class: String,
}

/// A single-select list of full-width options.
#[component]
pub fn ChoiceList(props: ChoiceListProps) -> Element {
    let on_select = props.on_select;
    rsx! {
        div {
            class: "space-y-1 {props.class}",
            role: "radiogroup",
            aria_label: "{props.label}",
            for (i, choice) in props.choices.iter().enumerate() {
                {
                    let checked = props.selected == Some(i);
                    let (row, dot) = if checked {
                        ("border-beet-leaf/50 bg-beet-leaf/10", "border-beet-leaf")
                    } else {
                        ("border-white/10 hover:bg-white/5", "border-gray-500")
                    };
                    rsx! {
                        // relative keeps the sr-only radio inside its option, so
                        // focusing it scrolls this option into view.
                        label { class: "relative flex items-start gap-2 p-2 rounded border cursor-pointer transition-colors has-focus-visible:border-beet-accent {row}",
                            input {
                                class: "sr-only",
                                r#type: "radio",
                                name: "{props.name}",
                                checked,
                                onchange: move |_| on_select.call(i),
                            }
                            span {
                                class: "mt-0.5 w-4 h-4 shrink-0 rounded-full border flex items-center justify-center {dot}",
                                aria_hidden: true,
                                if checked {
                                    span { class: "w-2 h-2 rounded-full bg-beet-leaf" }
                                }
                            }
                            span { class: "min-w-0 flex-1",
                                span { class: "flex items-baseline justify-between gap-2",
                                    span { class: "text-sm font-bold text-white break-words min-w-0", "{choice.title}" }
                                    if let Some(aside) = &choice.aside {
                                        span { class: "text-xs font-mono text-beet-leaf shrink-0", "{aside}" }
                                    }
                                }
                                if let Some(detail) = &choice.detail {
                                    span { class: "block text-xs text-gray-400 break-words", "{detail}" }
                                }
                                if let Some(note) = &choice.note {
                                    span { class: "block text-[10px] font-mono text-gray-500 break-words", "{note}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(title: &str, detail: Option<&str>) -> Choice {
        Choice {
            title: title.to_string(),
            aside: Some("71.4%".to_string()),
            detail: detail.map(str::to_string),
            note: Some("differs: length".to_string()),
        }
    }

    fn render(selected: Option<usize>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |selected: Option<usize>| {
                rsx! {
                    ChoiceList {
                        name: "matches-a",
                        label: "Matches",
                        choices: vec![choice("A - One", Some("Album · 2019")), choice("B - Two", None)],
                        selected,
                        on_select: |_| {},
                    }
                }
            },
            selected,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn one_radio_per_choice_in_one_group() {
        let html = render(None);
        assert!(html.contains(r#"role="radiogroup""#));
        assert!(html.contains(r#"aria-label="Matches""#));
        assert_eq!(html.matches(r#"type="radio""#).count(), 2);
        assert_eq!(html.matches(r#"name="matches-a""#).count(), 2);
        for text in [
            "A - One",
            "B - Two",
            "Album · 2019",
            "71.4%",
            "differs: length",
        ] {
            assert!(html.contains(text), "{text} missing from {html}");
        }
        assert!(!html.contains("checked=true"));
    }

    #[test]
    fn each_hidden_radio_is_positioned_inside_its_option() {
        let html = render(None);
        let labels: Vec<&str> = html.split("<label ").skip(1).collect();
        assert_eq!(labels.len(), 2);
        for label in labels {
            let class = label
                .strip_prefix(r#"class=""#)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default();
            assert!(class.split(' ').any(|c| c == "relative"), "{label}");
            assert!(label.contains(r#"<input class="sr-only""#), "{label}");
        }
    }

    #[test]
    fn only_the_selected_choice_is_checked() {
        let html = render(Some(1));
        assert_eq!(html.matches("checked=true").count(), 1);
        let second = html.find("B - Two").unwrap();
        assert!(html.find("checked=true").unwrap() > html.find("A - One").unwrap());
        assert!(html.find("checked=true").unwrap() < second);
    }
}
