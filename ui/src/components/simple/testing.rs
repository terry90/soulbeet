//! Drives rendered components in tests: events, and what they render.

use std::{any::Any, rc::Rc};

use dioxus::core::{ElementId, Mutation};
use dioxus::html::{
    PlatformEventData, SerializedFormData, SerializedFormObject, SerializedMouseData,
};
use dioxus::prelude::*;
use futures::FutureExt;

/// The elements of a freshly built `dom` that listen for `name`, in
/// document order.
pub(crate) fn listeners(dom: &mut VirtualDom, name: &str) -> Vec<ElementId> {
    dom.rebuild_to_vec()
        .edits
        .into_iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name: n, id } if n == name => Some(id),
            _ => None,
        })
        .collect()
}

/// Runs what is pending until nothing is left to do.
pub(crate) fn settle(dom: &mut VirtualDom) {
    loop {
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        if dom.wait_for_work().now_or_never().is_none() {
            break;
        }
    }
}

/// Fires `name` with `data` at `target`, runs what that starts, and tells
/// whether the browser's own action is still on.
fn fire(dom: &mut VirtualDom, name: &str, data: PlatformEventData, target: ElementId) -> bool {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let event = Event::new(Rc::new(data) as Rc<dyn Any>, true);
    dom.runtime().handle_event(name, event.clone(), target);
    settle(dom);
    event.default_action_enabled()
}

/// Clicks `target` and runs what that starts.
pub(crate) fn click(dom: &mut VirtualDom, target: ElementId) {
    let data = PlatformEventData::new(Box::<SerializedMouseData>::default());
    fire(dom, "click", data, target);
}

/// Submits `form` with `fields` as `(name, text)` pairs, runs what that
/// starts, and tells whether the browser's own submit is still on.
pub(crate) fn submit_form(dom: &mut VirtualDom, form: ElementId, fields: &[(&str, &str)]) -> bool {
    let fields = fields
        .iter()
        .map(|(key, text)| SerializedFormObject {
            key: key.to_string(),
            text: Some(text.to_string()),
            file: None,
        })
        .collect();
    let data = PlatformEventData::new(Box::new(SerializedFormData::new(String::new(), fields)));
    fire(dom, "submit", data, form)
}

/// The opening tag of the first element whose attributes include `marker`.
pub(crate) fn opening_tag<'a>(html: &'a str, marker: &str) -> &'a str {
    let at = html
        .find(marker)
        .unwrap_or_else(|| panic!("{marker} in {html}"));
    let start = html[..at].rfind('<').unwrap();
    &html[start..at + html[at..].find('>').unwrap()]
}
