use dioxus::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Gives each open modal its own element ids.
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    /// Called by the close button, a click outside and Escape.
    pub on_close: EventHandler,
    /// The content to be displayed inside the modal
    pub children: Element,
    /// The header of the modal, which also names the dialog
    pub header: Element,
    /// Actions kept below the scrolling body, always in view.
    #[props(default)]
    pub footer: Option<Element>,
}

/// A native modal `dialog`: the browser keeps focus inside it, makes the page
/// behind inert and turns Escape into `cancel`. The parent closes it by no
/// longer rendering it; focus then returns to where it was before opening.
#[component]
pub fn Modal(props: Props) -> Element {
    let id = use_hook(|| format!("modal-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)));
    // Set once the browser shows the dialog, which never happens on the server.
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
                    document.getElementById("{id}-body").focus({{ preventScroll: true }});
                }}"#
            ));
        }
    };
    {
        let id = id.clone();
        // Runs before the dialog leaves the DOM, while the page behind is
        // still inert: close it first so focus can go back.
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

    rsx! {
      dialog {
        id: "{id}",
        class: "m-0 h-full w-full max-h-none max-w-none bg-transparent text-inherit backdrop:bg-black/60 backdrop:backdrop-blur-sm",
        role: "dialog",
        aria_modal: "true",
        aria_labelledby: "{id}-title",
        onmounted: open,
        // Escape. The parent decides, so the browser does not close it.
        oncancel: move |event| {
            event.prevent_default();
            props.on_close.call(());
        },

        // The area around the panel
        div {
          class: "flex h-full w-full items-center justify-center p-4",
          onclick: move |_| props.on_close.call(()),

          // Content
          div {
            class: "bg-beet-panel border border-white/10 max-h-[85vh] overflow-hidden flex flex-col rounded-lg shadow-2xl max-w-lg w-full",
            onclick: move |event| event.stop_propagation(),
            // Header Container
            div { class: "flex items-center justify-between p-4 border-b border-white/10 bg-black/20",
              div { id: "{id}-title", class: "flex-1 min-w-0", {props.header} }
              button {
                class: "text-gray-400 hover:text-white transition-colors ml-4 cursor-pointer",
                aria_label: "Close",
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
            // Scrollable Body. Focus starts here, so the keys scroll it.
            div {
              id: "{id}-body",
              class: "overflow-y-auto p-4 pb-10 fade-bottom outline-none scrollbar-thin scrollbar-thumb-beet-accent/50 scrollbar-track-transparent",
              tabindex: "-1",
              {props.children}
            }
            if let Some(footer) = props.footer {
              div { class: "p-4 border-t border-white/10 bg-black/20", {footer} }
            }
          }
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::core::{ElementId, Mutation};
    use dioxus::html::{PlatformEventData, SerializedCancelData, SerializedMouseData};
    use std::{any::Any, cell::Cell, rc::Rc};

    thread_local! {
        static CLOSED: Cell<u32> = const { Cell::new(0) };
    }

    /// A rendered modal and the elements listening for `click` and `cancel`,
    /// in document order.
    fn modal() -> (VirtualDom, Vec<ElementId>, Vec<ElementId>) {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        CLOSED.with(|c| c.set(0));
        let mut dom = VirtualDom::new(|| {
            rsx! {
                Modal {
                    on_close: |_| CLOSED.with(|c| c.set(c.get() + 1)),
                    header: rsx! { h2 { "Title" } },
                    footer: rsx! { p { "Actions" } },
                    p { "Body" }
                }
            }
        });
        let mut clicks = Vec::new();
        let mut cancels = Vec::new();
        for edit in dom.rebuild_to_vec().edits {
            if let Mutation::NewEventListener { name, id } = edit {
                match name.as_str() {
                    "click" => clicks.push(id),
                    "cancel" => cancels.push(id),
                    _ => {}
                }
            }
        }
        (dom, clicks, cancels)
    }

    /// Fires an event and tells whether its default action is still on.
    fn fire(dom: &mut VirtualDom, name: &str, data: PlatformEventData, target: ElementId) -> bool {
        let event = Event::new(Rc::new(data) as Rc<dyn Any>, true);
        dom.runtime().handle_event(name, event.clone(), target);
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        event.default_action_enabled()
    }

    fn click(dom: &mut VirtualDom, target: ElementId) {
        let data = PlatformEventData::new(Box::<SerializedMouseData>::default());
        fire(dom, "click", data, target);
    }

    fn closed() -> u32 {
        CLOSED.with(Cell::get)
    }

    #[test]
    fn escape_closes_through_the_parent_not_the_browser() {
        let (mut dom, _, cancels) = modal();
        assert_eq!(cancels.len(), 1);
        let data = PlatformEventData::new(Box::new(SerializedCancelData {}));
        let browser_closes = fire(&mut dom, "cancel", data, cancels[0]);
        assert_eq!(closed(), 1);
        assert!(!browser_closes);
    }

    #[test]
    fn the_area_around_and_the_close_button_close_and_the_panel_does_not() {
        let (mut dom, clicks, _) = modal();
        // The area around the panel, the panel, the close button.
        assert_eq!(clicks.len(), 3);
        click(&mut dom, clicks[1]);
        assert_eq!(closed(), 0);
        click(&mut dom, clicks[0]);
        click(&mut dom, clicks[2]);
        assert_eq!(closed(), 2);
    }

    #[test]
    fn a_native_dialog_named_by_its_header_starts_focus_in_its_body() {
        let (dom, _, _) = modal();
        let html = dioxus_ssr::render(&dom);
        assert!(html.starts_with("<dialog id=\"modal-"), "{html}");
        let id = &html["<dialog id=\"".len()..html.find("\" class").unwrap()];
        assert!(
            html.contains(&format!("aria-labelledby=\"{id}-title\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<div id=\"{id}-title\" class=\"flex-1 min-w-0\"><h2>Title</h2>"
            )),
            "{html}"
        );
        assert!(html.contains("aria-modal=\"true\""), "{html}");
        // The scrolling body is what the open script focuses.
        let body = &html[html.find(&format!("id=\"{id}-body\"")).unwrap()..];
        assert!(body.contains("tabindex=\"-1\""), "{html}");
        assert!(body.find("tabindex").unwrap() < body.find("Body").unwrap());
    }

    #[test]
    fn the_footer_stays_out_of_the_scrolling_body() {
        let (dom, _, _) = modal();
        let html = dioxus_ssr::render(&dom);
        let between = &html[html.find("Body").unwrap()..html.find("Actions").unwrap()];
        assert!(
            between.contains("</div><div class=\"p-4 border-t"),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Close\""), "{html}");
    }
}
