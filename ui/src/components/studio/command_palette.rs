//! Accessible `<dialog>`-backed `Ctrl+K` / `Cmd+K` Command Palette, fuzzy command filtering,
//! and global keyboard shortcut target guards.

use dioxus::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{IconArrowRight, IconCommand, IconSearch};

static NEXT_PALETTE_ID: AtomicUsize = AtomicUsize::new(0);

/// Visual grouping category inside [`CommandPalette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCategory {
    Navigation,
    Playback,
    Curation,
    Mixes,
    Search,
}

impl PaletteCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Navigation => "Navigate",
            Self::Playback => "Playback",
            Self::Curation => "Curation",
            Self::Mixes => "Mixes & Radio",
            Self::Search => "Catalog",
        }
    }
}

/// Action entry displayed inside [`CommandPalette`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommand {
    pub id: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub category: PaletteCategory,
    pub shortcut: Option<String>,
    pub keywords: Vec<String>,
}

/// Pure guard deciding whether a global window `keydown` event should be ignored.
///
/// Strictly ignores shortcuts while focus sits inside `<input>`, `<textarea>`, `<select>`,
/// `[contenteditable]`, or while a modal/drawer (`dialog_open`) is open — with two safe
/// exceptions:
/// - `Escape` always passes through.
/// - `Ctrl+K` / `Cmd+K` passes through when no other modal/drawer is open, OR when the
///   Command Palette itself is open (`cmdk_open`) so it can toggle closed.
/// - `Space` is also ignored when focus is on `<button>`, `<a>`, or `[role="button"]` so
///   native keyboard activation works cleanly.
pub fn should_ignore_shortcut_target(
    tag_name: &str,
    role: Option<&str>,
    is_content_editable: bool,
    dialog_open: bool,
    cmdk_open: bool,
    key: &str,
    ctrl_or_meta: bool,
) -> bool {
    let is_escape = key.eq_ignore_ascii_case("escape") || key.eq_ignore_ascii_case("esc");
    if is_escape {
        return false;
    }

    let is_palette_chord = ctrl_or_meta && key.eq_ignore_ascii_case("k");
    if is_palette_chord {
        return dialog_open && !cmdk_open;
    }

    if dialog_open || is_content_editable {
        return true;
    }

    let upper_tag = tag_name.trim().to_ascii_uppercase();
    if matches!(upper_tag.as_str(), "INPUT" | "TEXTAREA" | "SELECT") {
        return true;
    }

    if role.is_some_and(|r| {
        r.eq_ignore_ascii_case("combobox")
            || r.eq_ignore_ascii_case("listbox")
            || r.eq_ignore_ascii_case("option")
    }) {
        return true;
    }

    if key == " " {
        let is_button_role = role.is_some_and(|r| r.eq_ignore_ascii_case("button"));
        if matches!(upper_tag.as_str(), "BUTTON" | "A") || is_button_role {
            return true;
        }
    }

    false
}

/// Filters and ranks [`PaletteCommand`]s by case-insensitive substring & token match
/// across label, subtitle, category, and keywords.
pub fn filter_palette_commands<'a>(
    commands: &'a [PaletteCommand],
    raw_query: &str,
) -> Vec<&'a PaletteCommand> {
    let query = raw_query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return commands.iter().collect();
    }

    let tokens: Vec<&str> = query.split_whitespace().collect();
    let mut scored: Vec<(u32, usize, &'a PaletteCommand)> = commands
        .iter()
        .enumerate()
        .filter_map(|(idx, cmd)| {
            let label = cmd.label.to_ascii_lowercase();
            let subtitle = cmd
                .subtitle
                .as_deref()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let category = cmd.category.label().to_ascii_lowercase();
            let haystack = format!(
                "{label} {subtitle} {category} {}",
                cmd.keywords
                    .iter()
                    .map(|k| k.to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(" ")
            );

            if !tokens.iter().all(|tok| haystack.contains(tok)) {
                return None;
            }

            let score = if label == query {
                100
            } else if label.starts_with(&query) {
                80
            } else if label.contains(&query) {
                60
            } else if subtitle.contains(&query) {
                40
            } else {
                20
            };
            Some((score, idx, cmd))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, cmd)| cmd).collect()
}

/// Accessible native `<dialog>` Command Palette (`Ctrl+K` / `Cmd+K`).
#[component]
pub fn CommandPalette(
    is_open: bool,
    commands: Vec<PaletteCommand>,
    on_select: EventHandler<String>,
    #[props(default)] on_search_query: Option<EventHandler<String>>,
    on_close: EventHandler<()>,
) -> Element {
    if !is_open {
        return rsx! {};
    }

    rsx! {
        CommandPaletteDialog {
            commands,
            on_select,
            on_search_query,
            on_close,
        }
    }
}

#[component]
fn CommandPaletteDialog(
    commands: Vec<PaletteCommand>,
    on_select: EventHandler<String>,
    on_search_query: Option<EventHandler<String>>,
    on_close: EventHandler<()>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut selected_idx = use_signal(|| 0usize);
    let id = use_hook(|| format!("cmdk-{}", NEXT_PALETTE_ID.fetch_add(1, Ordering::Relaxed)));
    let shown = use_hook(|| Rc::new(Cell::new(false)));

    let open = {
        let (id, shown) = (id.clone(), shown.clone());
        move |_| {
            shown.set(true);
            document::eval(&format!(
                r#"const dialog = document.getElementById("{id}");
                if (dialog && !dialog.open) {{
                    window["{id}-return"] = document.activeElement;
                    dialog.showModal();
                    const inp = document.getElementById("{id}-input");
                    if (inp) inp.focus({{ preventScroll: true }});
                }}"#
            ));
        }
    };

    {
        let id = id.clone();
        use_drop(move || {
            if shown.get() {
                document::eval(&format!(
                    r#"const dialog = document.getElementById("{id}");
                    if (dialog && dialog.open) dialog.close();
                    const back = window["{id}-return"];
                    delete window["{id}-return"];
                    back?.focus({{ preventScroll: true }});"#
                ));
            }
        });
    }

    let current_query = query();
    let trimmed_query = current_query.trim().to_string();
    let filtered: Vec<PaletteCommand> = filter_palette_commands(&commands, &current_query)
        .into_iter()
        .cloned()
        .collect();

    let has_search_fallback = on_search_query.is_some() && !trimmed_query.is_empty();
    let total_rows = filtered.len() + usize::from(has_search_fallback);
    let clamped_idx = if total_rows == 0 {
        0
    } else {
        selected_idx().min(total_rows - 1)
    };

    let filtered_for_key = filtered.clone();
    let search_q_for_key = trimmed_query.clone();

    rsx! {
        dialog {
            id: "{id}",
            role: "dialog",
            aria_modal: "true",
            aria_label: "Command Palette",
            class: "m-0 h-full w-full max-h-none max-w-none bg-transparent text-inherit backdrop:bg-beet-sunken/75 backdrop:backdrop-blur-sm",
            onmounted: open,
            oncancel: move |evt| {
                evt.prevent_default();
                on_close.call(());
            },

            div {
                class: "flex h-full w-full items-start justify-center pt-[12vh] px-4",
                onclick: move |_| on_close.call(()),

                div {
                    class: "w-full max-w-xl rounded-[14px] bg-beet-surface border border-beet-border-default shadow-2xl overflow-hidden flex flex-col",
                    onclick: move |evt| evt.stop_propagation(),

                    // Search Input Header
                    div { class: "flex items-center gap-3 px-4 h-14 border-b border-beet-border-subtle bg-beet-surface-low",
                        IconSearch { size: 18, class: "text-beet-text-tertiary".to_string() }
                        input {
                            id: "{id}-input",
                            r#type: "text",
                            value: "{current_query}",
                            placeholder: "Jump to view, control playback, or search catalog…",
                            autocomplete: "off",
                            class: "flex-1 bg-transparent text-[15px] text-beet-text-primary placeholder:text-beet-text-tertiary outline-none",
                            oninput: move |evt| {
                                query.set(evt.value());
                                selected_idx.set(0);
                            },
                            onkeydown: move |evt: KeyboardEvent| {
                                match evt.key() {
                                    Key::ArrowDown => {
                                        evt.prevent_default();
                                        if total_rows > 0 {
                                            selected_idx.set((clamped_idx + 1) % total_rows);
                                        }
                                    }
                                    Key::ArrowUp => {
                                        evt.prevent_default();
                                        if total_rows > 0 {
                                            selected_idx.set(if clamped_idx == 0 {
                                                total_rows - 1
                                            } else {
                                                clamped_idx - 1
                                            });
                                        }
                                    }
                                    Key::Enter => {
                                        evt.prevent_default();
                                        if let Some(cmd) = filtered_for_key.get(clamped_idx) {
                                            on_select.call(cmd.id.clone());
                                            on_close.call(());
                                        } else if has_search_fallback {
                                            if let Some(cb) = &on_search_query {
                                                cb.call(search_q_for_key.clone());
                                                on_close.call(());
                                            }
                                        }
                                    }
                                    Key::Escape => {
                                        evt.prevent_default();
                                        on_close.call(());
                                    }
                                    _ => {}
                                }
                            },
                        }
                        span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[10px] font-semibold text-beet-text-tertiary",
                            "ESC"
                        }
                    }

                    // Results List
                    div {
                        role: "listbox",
                        aria_label: "Matching commands",
                        class: "max-h-[52vh] overflow-y-auto studio-scrollbar p-2 space-y-1",
                        if filtered.is_empty() && !has_search_fallback {
                            div { class: "py-10 text-center text-[13px] text-beet-text-tertiary",
                                "No matching commands."
                            }
                        } else {
                            for (idx, cmd) in filtered.iter().enumerate() {
                                {
                                    let is_selected = idx == clamped_idx;
                                    let cmd_id = cmd.id.clone();
                                    let row_cls = if is_selected {
                                        "bg-beet-surface-high text-beet-text-primary border-beet-border-default"
                                    } else {
                                        "bg-transparent text-beet-text-secondary border-transparent hover:bg-beet-surface-low hover:text-beet-text-primary"
                                    };
                                    rsx! {
                                        button {
                                            key: "{cmd.id}",
                                            r#type: "button",
                                            role: "option",
                                            aria_selected: if is_selected { "true" } else { "false" },
                                            class: "w-full flex items-center justify-between gap-3 px-3 py-2.5 rounded-[8px] border text-left transition-colors duration-100 cursor-pointer {row_cls}",
                                            onmouseenter: move |_| selected_idx.set(idx),
                                            onclick: move |_| {
                                                on_select.call(cmd_id.clone());
                                                on_close.call(());
                                            },
                                            div { class: "flex items-center gap-2.5 min-w-0",
                                                span { class: "px-1.5 py-0.5 rounded bg-beet-sunken text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider shrink-0",
                                                    "{cmd.category.label()}"
                                                }
                                                div { class: "min-w-0",
                                                    p { class: "text-[14px] font-medium truncate text-beet-text-primary",
                                                        "{cmd.label}"
                                                    }
                                                    if let Some(sub) = cmd.subtitle.as_deref() {
                                                        p { class: "text-[12px] text-beet-text-tertiary truncate",
                                                            "{sub}"
                                                        }
                                                    }
                                                }
                                            }
                                            if let Some(kbd) = cmd.shortcut.as_deref() {
                                                span { class: "px-1.5 py-0.5 rounded bg-beet-surface-highest border border-beet-border-subtle tabular-nums-mono text-[11px] font-medium text-beet-text-secondary shrink-0",
                                                    "{kbd}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            if has_search_fallback {
                                {
                                    let fallback_idx = filtered.len();
                                    let is_selected = clamped_idx == fallback_idx;
                                    let q_clone = trimmed_query.clone();
                                    let row_cls = if is_selected {
                                        "bg-beet-accent-surface text-beet-accent-text border-beet-accent/35"
                                    } else {
                                        "bg-transparent text-beet-text-secondary border-transparent hover:bg-beet-surface-low"
                                    };
                                    rsx! {
                                        button {
                                            r#type: "button",
                                            role: "option",
                                            aria_selected: if is_selected { "true" } else { "false" },
                                            class: "w-full flex items-center justify-between gap-3 px-3 py-2.5 rounded-[8px] border text-left transition-colors duration-100 cursor-pointer {row_cls}",
                                            onmouseenter: move |_| selected_idx.set(fallback_idx),
                                            onclick: move |_| {
                                                if let Some(cb) = &on_search_query {
                                                    cb.call(q_clone.clone());
                                                    on_close.call(());
                                                }
                                            },
                                            div { class: "flex items-center gap-2.5 min-w-0",
                                                IconSearch { size: 15, class: "text-beet-accent-text".to_string() }
                                                span { class: "text-[14px] font-medium truncate",
                                                    "Search library & catalog for “{trimmed_query}”"
                                                }
                                            }
                                            IconArrowRight { size: 15 }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Footer Hint
                    div { class: "px-4 py-2 border-t border-beet-border-subtle bg-beet-surface-low flex items-center justify-between text-[11px] tabular-nums-mono text-beet-text-tertiary",
                        span { class: "inline-flex items-center gap-1.5",
                            IconCommand { size: 12 }
                            "↑↓ Navigate · Enter Run · Esc Close"
                        }
                        span { "{filtered.len()} commands" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_guard_blocks_form_inputs_buttons_and_other_dialogs() {
        // Normal shortcuts blocked inside inputs/textareas/selects/contenteditable
        assert!(should_ignore_shortcut_target("INPUT", None, false, false, false, " ", false));
        assert!(should_ignore_shortcut_target("textarea", None, false, false, false, "s", false));
        assert!(should_ignore_shortcut_target("SELECT", None, false, false, false, "1", false));
        assert!(should_ignore_shortcut_target("BUTTON", Some("combobox"), false, false, false, "ArrowDown", false));
        assert!(should_ignore_shortcut_target("BUTTON", Some("option"), false, false, false, "s", false));
        assert!(should_ignore_shortcut_target("DIV", None, true, false, false, "x", false));

        // Space does NOT hijack focused buttons, links, or role="button" elements
        assert!(should_ignore_shortcut_target("BUTTON", None, false, false, false, " ", false));
        assert!(should_ignore_shortcut_target("A", None, false, false, false, " ", false));
        assert!(should_ignore_shortcut_target("DIV", Some("button"), false, false, false, " ", false));
        // Non-Space shortcuts still work when a regular button has focus
        assert!(!should_ignore_shortcut_target("BUTTON", None, false, false, false, "s", false));

        // Normal shortcuts and Ctrl+K blocked while ANOTHER modal/drawer is open
        assert!(should_ignore_shortcut_target("BODY", None, false, true, false, " ", false));
        assert!(should_ignore_shortcut_target("BODY", None, false, true, false, "k", true));

        // Ctrl+K passes through when palette itself is open (to close) or no dialog is open
        assert!(!should_ignore_shortcut_target("INPUT", None, false, true, true, "k", true));
        assert!(!should_ignore_shortcut_target("INPUT", None, false, false, false, "K", true));

        // Escape always passes through
        assert!(!should_ignore_shortcut_target("INPUT", None, false, true, false, "Escape", false));

        // Normal shortcuts allowed on non-editable page elements when no dialog is open
        assert!(!should_ignore_shortcut_target("BODY", None, false, false, false, " ", false));
        assert!(!should_ignore_shortcut_target("DIV", None, false, false, false, "l", false));
    }

    #[test]
    fn filters_and_ranks_palette_commands_by_relevance() {
        let cmds = vec![
            PaletteCommand {
                id: "nav.discover".into(),
                label: "Go to Discover".into(),
                subtitle: Some("Seasonal packs & triage".into()),
                category: PaletteCategory::Navigation,
                shortcut: None,
                keywords: vec!["radar".into(), "mixes".into()],
            },
            PaletteCommand {
                id: "play.toggle".into(),
                label: "Play / Pause".into(),
                subtitle: None,
                category: PaletteCategory::Playback,
                shortcut: Some("Space".into()),
                keywords: vec!["resume".into()],
            },
            PaletteCommand {
                id: "curate.keep".into(),
                label: "Keep Discovery Track".into(),
                subtitle: Some("Promote to library".into()),
                category: PaletteCategory::Curation,
                shortcut: Some("P".into()),
                keywords: vec!["promote".into(), "save".into()],
            },
        ];

        assert_eq!(filter_palette_commands(&cmds, "").len(), 3);

        let keep = filter_palette_commands(&cmds, "promote");
        assert_eq!(keep.len(), 1);
        assert_eq!(keep[0].id, "curate.keep");

        let multi = filter_palette_commands(&cmds, "discover triage");
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].id, "nav.discover");

        assert!(filter_palette_commands(&cmds, "nonexistent xyz").is_empty());
    }
}
