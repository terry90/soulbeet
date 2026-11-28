//! Reusable Refined Midnight Studio custom select component.
//!
//! Replaces ugly native browser `<select>` popups and cramped OS chevrons with a
//! consistent dark Studio trigger and accessible popover listbox shared across
//! `web` (`dioxus-web`) and `desktop` (`dioxus-desktop`).

use std::sync::atomic::{AtomicUsize, Ordering};

use dioxus::prelude::*;

use crate::{IconCheck, IconChevronDown};

static NEXT_SELECT_ID: AtomicUsize = AtomicUsize::new(0);

/// One selectable item in a [`StudioSelect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    /// Machine value emitted by `on_change` and matched against `value`.
    pub value: String,
    /// Primary display label shown in the trigger and option row.
    pub label: String,
    /// Optional muted helper line displayed underneath the label in the dropdown.
    pub description: Option<String>,
}

impl SelectOption {
    /// Creates an option with a value and display label.
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Attaches a secondary muted description line shown inside the dropdown menu.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Returns the index of the option matching `value`, if any.
pub fn find_selected_index(options: &[SelectOption], value: &str) -> Option<usize> {
    options.iter().position(|opt| opt.value == value)
}

/// Computes the next highlighted option index when stepping by `delta` (`-1` or `+1`),
/// wrapping cleanly at both ends. Returns `0` for an empty option slice.
pub fn step_highlight_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let clamped = current.min(len - 1);
    if delta >= 0 {
        (clamped + (delta as usize)) % len
    } else {
        let back = delta.unsigned_abs() % len;
        (clamped + len - back) % len
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct StudioSelectProps {
    /// Currently selected option value.
    #[props(into)]
    pub value: String,
    /// Ordered list of selectable options.
    pub options: Vec<SelectOption>,
    /// Fires with the newly selected option's `value` when the selection changes.
    pub on_change: EventHandler<String>,
    /// Fallback text displayed when `value` matches no option.
    #[props(default = "Select an option".to_string(), into)]
    pub placeholder: String,
    /// Optional HTML `id` placed on the trigger button so `<label for="...">` binds to it.
    #[props(default, into)]
    pub id: Option<String>,
    /// Optional HTML `name` placed on a companion hidden input and trigger attribute.
    #[props(default, into)]
    pub name: Option<String>,
    /// Accessible label for the combobox trigger and listbox popup.
    #[props(default, into)]
    pub aria_label: Option<String>,
    /// Prevents opening or changing the selection.
    #[props(default)]
    pub disabled: bool,
    /// Optional extra classes appended to the outer relative container.
    #[props(default, into)]
    pub class: String,
}

/// Accessible dark Studio select control with a custom popover listbox,
/// rotating chevron, checkmark indicator, and full keyboard navigation.
#[component]
pub fn StudioSelect(props: StudioSelectProps) -> Element {
    let base_id = use_hook(|| {
        format!(
            "studio-select-{}",
            NEXT_SELECT_ID.fetch_add(1, Ordering::Relaxed)
        )
    });
    let mut open = use_signal(|| false);
    let mut highlighted_idx = use_signal(|| 0usize);

    let is_open = open() && !props.disabled;
    let selected_idx = find_selected_index(&props.options, &props.value);
    let selected_label = selected_idx
        .and_then(|idx| props.options.get(idx))
        .map(|opt| opt.label.clone());

    let total_options = props.options.len();
    let clamped_highlight = if total_options == 0 {
        0
    } else {
        highlighted_idx().min(total_options - 1)
    };

    let trigger_id = props
        .id
        .clone()
        .unwrap_or_else(|| format!("{base_id}-trigger"));
    let listbox_id = format!("{base_id}-listbox");
    let active_descendant = if is_open && total_options > 0 {
        Some(format!("{base_id}-opt-{clamped_highlight}"))
    } else {
        None
    };

    let trigger_border = if is_open {
        "border-beet-leaf-solid/60 bg-beet-surface-low"
    } else {
        "border-beet-border-default hover:border-beet-border-strong hover:bg-beet-surface-high/40 bg-beet-surface-low"
    };
    let label_tone = if selected_label.is_some() {
        "text-beet-text-primary"
    } else {
        "text-beet-text-tertiary"
    };
    let chevron_cls = if is_open {
        "text-beet-text-primary rotate-180 transition-transform duration-150"
    } else {
        "text-beet-text-tertiary transition-transform duration-150"
    };

    let options_for_key = props.options.clone();
    let current_value_for_key = props.value.clone();
    let on_change = props.on_change;
    let disabled = props.disabled;

    rsx! {
        div {
            class: if props.class.is_empty() {
                "relative w-full".to_string()
            } else {
                format!("relative w-full {}", props.class)
            },
            onkeydown: move |evt: KeyboardEvent| {
                if disabled || options_for_key.is_empty() {
                    return;
                }
                let currently_open = open();
                let len = options_for_key.len();
                let cur = highlighted_idx().min(len - 1);

                match evt.key() {
                    Key::ArrowDown => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        if currently_open {
                            highlighted_idx.set(step_highlight_index(cur, len, 1));
                        } else {
                            let start = find_selected_index(&options_for_key, &current_value_for_key)
                                .unwrap_or(0);
                            highlighted_idx.set(start);
                            open.set(true);
                        }
                    }
                    Key::ArrowUp => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        if currently_open {
                            highlighted_idx.set(step_highlight_index(cur, len, -1));
                        } else {
                            let start = find_selected_index(&options_for_key, &current_value_for_key)
                                .unwrap_or(len - 1);
                            highlighted_idx.set(start);
                            open.set(true);
                        }
                    }
                    Key::Home if currently_open => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        highlighted_idx.set(0);
                    }
                    Key::End if currently_open => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        highlighted_idx.set(len - 1);
                    }
                    Key::Enter => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        if currently_open {
                            if let Some(opt) = options_for_key.get(cur) {
                                if opt.value != current_value_for_key {
                                    on_change.call(opt.value.clone());
                                }
                            }
                            open.set(false);
                        } else {
                            let start = find_selected_index(&options_for_key, &current_value_for_key)
                                .unwrap_or(0);
                            highlighted_idx.set(start);
                            open.set(true);
                        }
                    }
                    Key::Character(ref s) if s == " " => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        if currently_open {
                            if let Some(opt) = options_for_key.get(cur) {
                                if opt.value != current_value_for_key {
                                    on_change.call(opt.value.clone());
                                }
                            }
                            open.set(false);
                        } else {
                            let start = find_selected_index(&options_for_key, &current_value_for_key)
                                .unwrap_or(0);
                            highlighted_idx.set(start);
                            open.set(true);
                        }
                    }
                    Key::Escape if currently_open => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        open.set(false);
                    }
                    Key::Tab if currently_open => {
                        open.set(false);
                    }
                    _ => {}
                }
            },

            if let Some(ref field_name) = props.name {
                input {
                    r#type: "hidden",
                    name: "{field_name}",
                    value: "{props.value}",
                }
            }

            button {
                id: "{trigger_id}",
                r#type: "button",
                role: "combobox",
                aria_haspopup: "listbox",
                aria_expanded: if is_open { "true" } else { "false" },
                aria_controls: "{listbox_id}",
                aria_activedescendant: active_descendant,
                aria_label: props.aria_label.clone(),
                disabled,
                class: "w-full h-10 pl-3.5 pr-3 rounded-[10px] border text-sm text-left flex items-center justify-between gap-2.5 transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none focus-visible:border-beet-leaf-solid/60 {trigger_border}",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    if disabled || total_options == 0 {
                        return;
                    }
                    if open() {
                        open.set(false);
                    } else {
                        highlighted_idx.set(selected_idx.unwrap_or(0));
                        open.set(true);
                    }
                },
                span { class: "min-w-0 flex-1 truncate {label_tone}",
                    {selected_label.unwrap_or_else(|| props.placeholder.clone())}
                }
                IconChevronDown {
                    size: 16,
                    class: chevron_cls.to_string(),
                }
            }

            if is_open {
                div {
                    class: "fixed inset-0 z-40",
                    aria_hidden: "true",
                    onclick: move |evt: MouseEvent| {
                        evt.prevent_default();
                        evt.stop_propagation();
                        open.set(false);
                    },
                }

                div {
                    id: "{listbox_id}",
                    role: "listbox",
                    aria_label: props.aria_label.clone(),
                    class: "absolute left-0 right-0 top-full mt-1.5 z-50 max-h-64 overflow-y-auto studio-scrollbar rounded-[12px] bg-beet-surface-high border border-beet-border-default shadow-2xl p-1.5 space-y-0.5",
                    onclick: move |evt: MouseEvent| evt.stop_propagation(),

                    for (idx, opt) in props.options.iter().enumerate() {
                        {
                            let is_selected = opt.value == props.value;
                            let is_highlighted = idx == clamped_highlight;
                            let opt_id = format!("{base_id}-opt-{idx}");
                            let opt_value = opt.value.clone();
                            let current_val = props.value.clone();
                            let row_cls = match (is_selected, is_highlighted) {
                                (true, true) => "bg-beet-leaf-surface text-beet-text-primary border-beet-leaf-solid/45",
                                (true, false) => "bg-beet-leaf-surface text-beet-text-primary border-beet-leaf-solid/25",
                                (false, true) => "bg-beet-surface-highest text-beet-text-primary border-transparent",
                                (false, false) => "bg-transparent text-beet-text-secondary border-transparent hover:bg-beet-surface-highest hover:text-beet-text-primary",
                            };

                            rsx! {
                                button {
                                    key: "{opt.value}",
                                    id: "{opt_id}",
                                    r#type: "button",
                                    role: "option",
                                    tabindex: "-1",
                                    aria_selected: if is_selected { "true" } else { "false" },
                                    class: "w-full flex items-center justify-between gap-3 px-3 py-2 rounded-[8px] border text-left transition-colors duration-100 cursor-pointer {row_cls}",
                                    onmouseenter: move |_| highlighted_idx.set(idx),
                                    onclick: move |evt: MouseEvent| {
                                        evt.stop_propagation();
                                        if opt_value != current_val {
                                            on_change.call(opt_value.clone());
                                        }
                                        open.set(false);
                                    },
                                    div { class: "min-w-0 flex-1",
                                        span { class: "block text-[13px] font-medium leading-snug truncate",
                                            "{opt.label}"
                                        }
                                        if let Some(desc) = opt.description.as_deref() {
                                            span { class: "block text-[11px] text-beet-text-tertiary leading-snug mt-0.5",
                                                "{desc}"
                                            }
                                        }
                                    }
                                    span { class: "w-4 h-4 flex items-center justify-center shrink-0",
                                        if is_selected {
                                            IconCheck {
                                                size: 15,
                                                class: "text-beet-leaf-text".to_string(),
                                            }
                                        }
                                    }
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
    use crate::components::simple::{click, click_and_find, listeners};
    use dioxus::html::{PlatformEventData, SerializedKeyboardData};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn sample_options() -> Vec<SelectOption> {
        vec![
            SelectOption::new("never", "Never use it"),
            SelectOption::new("ask", "Ask me").with_description("Prompt before lossy downloads"),
            SelectOption::new("auto", "Download automatically (lossy)"),
        ]
    }

    fn fire_key(dom: &mut VirtualDom, target: dioxus::core::ElementId, key: Key) {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        let data = SerializedKeyboardData::new(
            key,
            Code::Unidentified,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        );
        let event = Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>,
            true,
        );
        dom.runtime().handle_event("keydown", event, target);
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
    }

    #[test]
    fn highlight_stepping_wraps_cleanly() {
        assert_eq!(step_highlight_index(0, 3, 1), 1);
        assert_eq!(step_highlight_index(2, 3, 1), 0);
        assert_eq!(step_highlight_index(0, 3, -1), 2);
        assert_eq!(step_highlight_index(2, 3, -1), 1);
        assert_eq!(step_highlight_index(9, 3, 1), 0);
        assert_eq!(step_highlight_index(0, 0, 1), 0);
    }

    #[test]
    fn closed_select_renders_accessible_combobox_without_listbox() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                StudioSelect {
                    id: "lossy-select",
                    name: "lossy_fallback",
                    aria_label: "When only YouTube Music has it",
                    value: "ask",
                    options: sample_options(),
                    on_change: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);

        assert!(html.contains(r#"id="lossy-select""#), "{html}");
        assert!(html.contains(r#"role="combobox""#), "{html}");
        assert!(html.contains(r#"aria-haspopup="listbox""#), "{html}");
        assert!(html.contains(r#"aria-expanded="false""#), "{html}");
        assert!(
            html.contains(r#"aria-label="When only YouTube Music has it""#),
            "{html}"
        );
        assert!(html.contains(r#"name="lossy_fallback""#), "{html}");
        assert!(html.contains("Ask me"), "{html}");
        assert!(!html.contains(r#"role="listbox""#), "{html}");
        assert!(!html.contains("<select"), "{html}");
    }

    #[test]
    fn clicking_trigger_opens_menu_and_picking_option_emits_value_and_closes() {
        let picked = Rc::new(RefCell::new(Vec::<String>::new()));
        let picked_for_app = picked.clone();

        let mut dom = VirtualDom::new_with_props(
            |picked: Rc<RefCell<Vec<String>>>| {
                let mut current = use_signal(|| "ask".to_string());
                rsx! {
                    StudioSelect {
                        value: current(),
                        options: sample_options(),
                        on_change: move |val: String| {
                            picked.borrow_mut().push(val.clone());
                            current.set(val);
                        },
                    }
                }
            },
            picked_for_app,
        );

        let initial_clicks = listeners(&mut dom, "click");
        assert_eq!(initial_clicks.len(), 1);

        // Open dropdown by clicking the trigger button.
        let opened_clicks = click_and_find(&mut dom, initial_clicks[0], "click");
        let open_html = dioxus_ssr::render(&dom);
        assert!(open_html.contains(r#"aria-expanded="true""#), "{open_html}");
        assert!(open_html.contains(r#"role="listbox""#), "{open_html}");
        assert!(open_html.contains("Prompt before lossy downloads"), "{open_html}");
        assert!(open_html.contains("rotate-180"), "{open_html}");

        // Opened clicks in DOM order: backdrop, listbox container, option 0, option 1, option 2.
        assert_eq!(opened_clicks.len(), 5);

        // Click option 0 ("never").
        click(&mut dom, opened_clicks[2]);
        let closed_html = dioxus_ssr::render(&dom);
        assert_eq!(*picked.borrow(), vec!["never".to_string()]);
        assert!(closed_html.contains(r#"aria-expanded="false""#), "{closed_html}");
        assert!(!closed_html.contains(r#"role="listbox""#), "{closed_html}");
        assert!(closed_html.contains("Never use it"), "{closed_html}");
    }

    #[test]
    fn clicking_backdrop_closes_menu_without_changing_value() {
        let picked = Rc::new(RefCell::new(Vec::<String>::new()));
        let picked_for_app = picked.clone();

        let mut dom = VirtualDom::new_with_props(
            |picked: Rc<RefCell<Vec<String>>>| {
                rsx! {
                    StudioSelect {
                        value: "ask",
                        options: sample_options(),
                        on_change: move |val: String| picked.borrow_mut().push(val),
                    }
                }
            },
            picked_for_app,
        );

        let trigger = listeners(&mut dom, "click")[0];
        let opened_clicks = click_and_find(&mut dom, trigger, "click");
        assert!(dioxus_ssr::render(&dom).contains(r#"role="listbox""#));

        // Backdrop is the first newly rendered click listener.
        click(&mut dom, opened_clicks[0]);
        let html = dioxus_ssr::render(&dom);
        assert!(!html.contains(r#"role="listbox""#), "{html}");
        assert!(picked.borrow().is_empty());
    }

    #[test]
    fn keyboard_navigation_selects_and_escape_closes() {
        let picked = Rc::new(RefCell::new(Vec::<String>::new()));
        let picked_for_app = picked.clone();

        let mut dom = VirtualDom::new_with_props(
            |picked: Rc<RefCell<Vec<String>>>| {
                let mut current = use_signal(|| "ask".to_string());
                rsx! {
                    StudioSelect {
                        value: current(),
                        options: sample_options(),
                        on_change: move |val: String| {
                            picked.borrow_mut().push(val.clone());
                            current.set(val);
                        },
                    }
                }
            },
            picked_for_app,
        );

        let key_target = listeners(&mut dom, "keydown")[0];

        // ArrowDown opens at current selection ("ask", index 1).
        fire_key(&mut dom, key_target, Key::ArrowDown);
        assert!(dioxus_ssr::render(&dom).contains(r#"role="listbox""#));

        // Escape closes without changing.
        fire_key(&mut dom, key_target, Key::Escape);
        assert!(!dioxus_ssr::render(&dom).contains(r#"role="listbox""#));
        assert!(picked.borrow().is_empty());

        // ArrowDown opens at index 1, second ArrowDown moves to index 2 ("auto"), Enter commits.
        fire_key(&mut dom, key_target, Key::ArrowDown);
        fire_key(&mut dom, key_target, Key::ArrowDown);
        fire_key(&mut dom, key_target, Key::Enter);

        let html = dioxus_ssr::render(&dom);
        assert_eq!(*picked.borrow(), vec!["auto".to_string()]);
        assert!(!html.contains(r#"role="listbox""#), "{html}");
        assert!(html.contains("Download automatically (lossy)"), "{html}");
    }

    #[test]
    fn disabled_select_cannot_be_opened() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                StudioSelect {
                    value: "ask",
                    options: sample_options(),
                    disabled: true,
                    on_change: |_| {},
                }
            }
        });

        let trigger = listeners(&mut dom, "click")[0];
        click(&mut dom, trigger);
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("disabled"), "{html}");
        assert!(!html.contains(r#"role="listbox""#), "{html}");
    }
}
