//! The switches of the download backends an admin turns on, under Settings > Config > Connections.

use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;
use shared::backends::{BackendMeta, BackendSwitch, Switch};
use shared::system::DownloaderHealth;

use super::connection_status_row::StatusLine;
use super::direct_connect::DirectConnectSettings;
use super::panel::LoadError;
use super::toggle::{save_switch, Toggle};
use crate::{friendly_error, use_auth, use_system_health};

/// Saves the switch of the backend with an id. Resolves to what the server stored.
type SwitchSaver =
    Callback<(String, bool), Pin<Box<dyn Future<Output = Result<BackendSwitch, ServerFnError>>>>>;

/// One row per backend an admin can turn on; nothing when this build has none.
#[component]
pub(super) fn BackendSwitches() -> Element {
    let switchable: Vec<&'static BackendMeta> = (shared::backends::ALL.iter())
        .filter(|m| m.switch == Switch::AdminToggle)
        .collect();
    // Fixed for the build, so the hooks below run on every render or on none.
    if switchable.is_empty() {
        return rsx! {};
    }
    let auth = use_auth();
    let health = use_system_health();
    let mut stored =
        use_resource(move || async move { auth.call(api::get_backend_switches()).await });
    let save: SwitchSaver = use_callback(move |(id, enabled): (String, bool)| {
        Box::pin(auth.call(api::set_backend_enabled(id, enabled)))
            as Pin<Box<dyn Future<Output = Result<BackendSwitch, ServerFnError>>>>
    });

    let switches = match &*stored.read() {
        None => return rsx! {},
        Some(Err(e)) => {
            return rsx! {
                div { class: "mt-6",
                    LoadError { message: friendly_error(e), on_retry: move |_| stored.restart() }
                }
            };
        }
        Some(Ok(switches)) => switches.clone(),
    };
    rsx! {
        div { class: "mt-6 pt-6 border-t border-white/10 space-y-6",
            for meta in switchable {
                BackendSwitchRow {
                    key: "{meta.id}",
                    meta,
                    enabled: switches.iter().any(|s| s.id == meta.id && s.enabled),
                    health: health.downloader(meta.id),
                    save,
                }
            }
        }
    }
}

/// A backend's name, its switch, what it is, and while on, whether it can take work. Turning it
/// on asks first.
#[component]
fn BackendSwitchRow(
    meta: &'static BackendMeta,
    enabled: bool,
    health: Option<DownloaderHealth>,
    save: SwitchSaver,
) -> Element {
    let on = use_signal(|| enabled);
    let error = use_signal(String::new);
    let mut confirming = use_signal(|| false);
    let id = meta.id;
    let store = move |value: bool| save.call((id.to_string(), value));

    rsx! {
        div { class: "space-y-2",
            div { class: "flex items-center justify-between gap-3",
                h3 { class: "text-sm font-semibold text-white", "{meta.name}" }
                Toggle {
                    on: on(),
                    on_class: "bg-beet-leaf",
                    onclick: move |_| {
                        if on() {
                            confirming.set(false);
                            spawn(save_switch(on, error, store));
                        } else {
                            confirming.set(true);
                        }
                    },
                }
            }
            for line in meta.about {
                p { class: "text-xs text-gray-400 font-mono", "{line}" }
            }
            if let Some((text, url)) = meta.link {
                a {
                    class: "text-xs font-mono text-gray-300 hover:text-white underline decoration-dotted",
                    href: "{url}",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "{text}"
                }
            }
            if confirming() {
                div { class: "flex flex-wrap items-center gap-3",
                    button {
                        class: "retro-btn rounded",
                        onclick: move |_| {
                            confirming.set(false);
                            spawn(save_switch(on, error, store));
                        },
                        "Turn on {meta.name}"
                    }
                    button {
                        class: "text-xs font-mono text-gray-400 hover:text-white underline decoration-dotted cursor-pointer",
                        onclick: move |_| confirming.set(false),
                        "Cancel"
                    }
                }
            }
            if !error().is_empty() {
                p { class: "text-xs text-red-400 font-mono", "{error}" }
            }
            if on() {
                {status_line(health.as_ref())}
            }
            if let Some(form) = extra_settings(id) {
                {form}
            }
        }
    }
}

/// A backend's own settings form, under its switch. Each backend that has one adds its arm.
fn extra_settings(id: &str) -> Option<Element> {
    match id {
        shared::airdcpp::BACKEND_ID => Some(rsx! { DirectConnectSettings {} }),
        _ => None,
    }
}

/// Whether the backend can take work: what the last health check said, or that it is checked.
fn status_line(health: Option<&DownloaderHealth>) -> Element {
    let (dot, summary) = match health {
        None => ("bg-gray-500 animate-pulse", "Checking...".to_string()),
        Some(h) if h.online => ("bg-beet-leaf", "Online".to_string()),
        Some(h) => (
            "bg-red-500",
            h.failure
                .as_ref()
                .map_or_else(|| "Offline".to_string(), |f| f.message.clone()),
        ),
    };
    rsx! {
        StatusLine { dot_class: dot, summary }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    static ARCHIVE: BackendMeta = BackendMeta {
        id: "archive",
        name: "Internet Archive",
        service: "archive.org",
        switch: Switch::AdminToggle,
        about: &["Free and public domain recordings, many of them live."],
        link: Some((
            "What the archive holds",
            "https://archive.org/details/audio",
        )),
        ..shared::backends::SLSKD
    };

    /// A row starting `enabled`, whose saves answer `fails` or what was asked, recorded in
    /// `asked`.
    fn row(enabled: bool, fails: bool, asked: Rc<RefCell<Vec<bool>>>) -> VirtualDom {
        VirtualDom::new_with_props(
            move |(enabled, fails, asked): (bool, bool, Rc<RefCell<Vec<bool>>>)| {
                let save: SwitchSaver = use_callback(move |(id, value): (String, bool)| {
                    assert_eq!(id, "archive");
                    asked.borrow_mut().push(value);
                    Box::pin(async move {
                        if fails {
                            Err(ServerFnError::ServerError {
                                message: "Only an admin can do this".into(),
                                code: api::FORBIDDEN_CODE,
                                details: None,
                            })
                        } else {
                            Ok(BackendSwitch {
                                id: "archive".into(),
                                enabled: value,
                            })
                        }
                    })
                        as Pin<Box<dyn Future<Output = Result<BackendSwitch, ServerFnError>>>>
                });
                let health = Some(DownloaderHealth {
                    id: "archive".into(),
                    name: "Internet Archive".into(),
                    online: true,
                    failure: None,
                });
                rsx! { BackendSwitchRow { meta: &ARCHIVE, enabled, health, save } }
            },
            (enabled, fails, asked),
        )
    }

    /// Clicks the switch, then returns the buttons the click showed.
    fn click_switch(dom: &mut VirtualDom) -> Vec<dioxus::core::ElementId> {
        let switch = crate::listeners(dom, "click");
        assert_eq!(switch.len(), 1, "only the switch listens before a click");
        crate::click_and_find(dom, switch[0], "click")
    }

    #[test]
    fn a_row_shows_the_backend_and_what_it_is() {
        let mut dom = row(false, false, Rc::default());
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for text in [
            ">Internet Archive</h3>",
            ">Free and public domain recordings, many of them live.</p>",
            "href=\"https://archive.org/details/audio\"",
            ">What the archive holds</a>",
        ] {
            assert!(html.contains(text), "{text} in {html}");
        }
        assert!(!html.contains("Online"), "no status while off: {html}");
        assert!(extra_settings("archive").is_none());
        assert!(!html.contains("<form"), "no form of its own: {html}");
    }

    #[test]
    fn turning_on_asks_first_and_saves_only_on_yes() {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let mut dom = row(false, false, asked.clone());
        let buttons = click_switch(&mut dom);
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains(">Turn on Internet Archive</button>"),
            "{html}"
        );
        assert!(asked.borrow().is_empty(), "nothing saved before the yes");
        // The yes, then the cancel button, after the switch.
        assert_eq!(buttons.len(), 2, "{html}");
        crate::click(&mut dom, buttons[0]);
        let html = dioxus_ssr::render(&dom);
        assert_eq!(*asked.borrow(), vec![true]);
        assert!(html.contains("bg-beet-leaf"), "{html}");
        assert!(!html.contains("Turn on Internet Archive"), "{html}");
        assert!(html.contains(">Online</span>"), "{html}");
    }

    #[test]
    fn a_cancelled_turn_on_saves_nothing() {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let mut dom = row(false, false, asked.clone());
        let buttons = click_switch(&mut dom);
        crate::click(&mut dom, buttons[1]);
        let html = dioxus_ssr::render(&dom);
        assert!(asked.borrow().is_empty());
        assert!(!html.contains("Turn on Internet Archive"), "{html}");
        assert!(html.contains("bg-gray-600"), "{html}");
    }

    #[test]
    fn turning_off_asks_nothing_and_a_refusal_puts_the_switch_back() {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let mut dom = row(true, true, asked.clone());
        let shown = click_switch(&mut dom);
        assert!(shown.is_empty(), "no question before turning off");
        let html = dioxus_ssr::render(&dom);
        assert_eq!(*asked.borrow(), vec![false]);
        assert!(html.contains("bg-beet-leaf"), "{html}");
        assert!(html.contains("Only an admin can do this"), "{html}");
    }
}
