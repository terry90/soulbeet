use crate::auth::use_auth;
use crate::components::{friendly_error, Footer, StatusAction, StatusBanner, WhatsNew};
use dioxus::prelude::*;
use shared::changelog::{changelog, running_version};

/// The signed-in pages' frame. It opens "What's new" once when releases up to
/// the running version came out after the user last closed it.
#[component]
pub fn Layout(children: Element) -> Element {
    let auth = use_auth();
    let mut seen = use_resource(move || async move { auth.call(api::get_changelog_seen()).await });
    // Set once the notes that opened on their own are closed.
    let mut auto_closed = use_signal(|| false);
    let mut showing_all = use_signal(|| false);
    let mut mark_failed = use_signal(|| None::<String>);

    let mark_seen = move || {
        spawn(async move {
            let failed = auth
                .call(api::mark_changelog_seen(running_version()))
                .await
                .err();
            mark_failed.set(failed.map(|e| {
                format!(
                    "Could not save that you saw the release notes. {}",
                    friendly_error(&e)
                )
            }));
        });
    };

    let (unseen, load_failed) = match &*seen.read() {
        Some(Ok(version)) if !auto_closed() => (
            changelog()
                .unseen(*version, running_version())
                .into_iter()
                .cloned()
                .collect(),
            None,
        ),
        Some(Err(e)) => (
            Vec::new(),
            Some(format!(
                "Could not load the release notes. {}",
                friendly_error(e)
            )),
        ),
        _ => (Vec::new(), None),
    };

    rsx! {
      // CRT Scanline Effect Overlay
      div { class: "fixed inset-0 z-50 pointer-events-none opacity-50 crt-overlay h-full w-full" }

      // Main container
      div { class: "relative z-10 flex flex-col h-screen max-w-7xl mx-auto",
        {children}
        if let Some(message) = load_failed {
          StatusBanner { dot_class: "bg-beet-accent", label: "WHAT'S NEW", message,
            StatusAction { label: "Try again", onclick: move |_| seen.restart() }
          }
        }
        if let Some(message) = mark_failed() {
          StatusBanner { dot_class: "bg-beet-accent", label: "WHAT'S NEW", message,
            StatusAction { label: "Try again", onclick: move |_| mark_seen() }
          }
        }
        Footer { on_whats_new: move |_| showing_all.set(true) }

        // Opened from the footer: every section, and closing records nothing.
        if showing_all() {
          WhatsNew {
            sections: changelog().sections.clone(),
            on_close: move |_| showing_all.set(false),
          }
        } else if !unseen.is_empty() {
          WhatsNew {
            sections: unseen,
            on_close: move |_| {
                auto_closed.set(true);
                mark_seen();
            },
          }
        }
      }
    }
}
