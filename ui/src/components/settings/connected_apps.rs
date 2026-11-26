use api::connections::ConnectedApp;
use api::{get_connected_apps, revoke_connected_app, set_app_deletion, set_grant_allow_admin};
use dioxus::prelude::*;
use shared::navidrome::DAILY_APP_DELETIONS;

use super::panel::{utc_time, ListRow, LoadError, Loading, Messages, Panel, REMOVE_BUTTON_CLASS};
use crate::auth::use_auth;
use crate::{friendly_error, ConfirmModal};

/// The page's origin, `https://music.example.com`. Empty until the first
/// effect runs, which happens only in the browser, so the server render and
/// the hydrated page agree.
pub(super) fn use_browser_origin() -> Signal<String> {
    let origin = use_signal(String::new);
    use_effect(move || {
        #[cfg(target_arch = "wasm32")]
        if let Some(found) = web_sys::window().and_then(|w| w.location().origin().ok()) {
            let mut origin = origin;
            origin.set(found);
        }
    });
    origin
}

/// Shown to every user: the apps they connected. An admin sees every user's.
#[component]
pub fn ConnectedAppsPanel() -> Element {
    let auth = use_auth();
    let origin = use_browser_origin();
    let mut apps = use_resource(move || async move { auth.call(get_connected_apps()).await });
    let mut error = use_signal(String::new);
    // The app whose switch is being saved, from the click until the list is read again: its box
    // takes no click meanwhile, so a second one sends nothing.
    let mut saving = use_signal(|| None::<String>);
    use_effect(move || {
        // The list is read again after every change (finish), and only then does the box show
        // the stored value: Resource::restart keeps the old list until the new one is set.
        if apps.read().is_some() {
            saving.set(None);
        }
    });

    // Every change reloads the list, so it shows what the server holds.
    let mut finish = move |result: Result<(), ServerFnError>| {
        match result {
            Ok(()) => error.set(String::new()),
            Err(e) => error.set(friendly_error(&e)),
        }
        apps.restart();
    };

    rsx! {
        Panel { title: "Connected apps",
            Messages { error: error() }
            match &*apps.read() {
                None => rsx! { Loading {} },
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| apps.restart() }
                },
                Some(Ok(list)) => rsx! {
                    AppList {
                        apps: list.clone(),
                        origin: origin(),
                        is_admin: auth.is_admin(),
                        saving: saving(),
                        on_allow_delete: move |(grant_id, allowed): (String, bool)| {
                            saving.set(Some(grant_id.clone()));
                            spawn(async move { finish(auth.call(set_app_deletion(grant_id, allowed)).await) });
                        },
                        on_allow_admin: move |(grant_id, allowed): (String, bool)| {
                            saving.set(Some(grant_id.clone()));
                            spawn(async move { finish(auth.call(set_grant_allow_admin(grant_id, allowed)).await) });
                        },
                        on_disconnect: move |grant_id: String| {
                            spawn(async move { finish(auth.call(revoke_connected_app(grant_id)).await) });
                        },
                    }
                },
            }
        }
    }
}

/// An app waiting for the user's yes: to disconnect it, or to let it delete music.
#[derive(Clone, PartialEq)]
struct PendingApp {
    grant_id: String,
    app_host: String,
}

/// Whether changing the switch from `was_allowed` asks first. Turning it on
/// does, since it is the one gate of every deletion an app can make; turning
/// it off does not.
fn needs_confirmation(was_allowed: bool) -> bool {
    !was_allowed
}

/// What turning file management on for `app_host` lets it do, asked before it is turned on:
/// everything the switch opens, including moving and re-tagging tracks, the two deletions that
/// need no preview, and the daily deletion limit shared by every app of the user.
fn enable_deletion_message(app_host: &str) -> String {
    format!(
        "Let {app_host} manage library files (delete, move between folders, and re-tag)? Before deleting, moving or re-tagging tracks it has to ask you first. While this is on it can also run your rating sync, which deletes the songs you rated 1 star when auto-delete is on, and keep a discovery track, which replaces an earlier copy of the song. Deletions are limited to at most {DAILY_APP_DELETIONS} tracks in any 24 hours, for all your connected apps together. Deleted files cannot be restored."
    )
}

fn enable_admin_message(app_host: &str) -> String {
    format!(
        "Grant {app_host} admin library access across all users' folders? While this is on, it can list and inspect all registered library folders across users (including private and shared folders) and move tracks between any users' folders when file management is also enabled."
    )
}

/// One row per app, or how to connect one. `origin` is the page's, empty
/// until the browser has it. `saving` is the app whose switch is being saved.
#[component]
fn AppList(
    apps: Vec<ConnectedApp>,
    origin: String,
    is_admin: bool,
    saving: Option<String>,
    on_allow_delete: EventHandler<(String, bool)>,
    on_allow_admin: EventHandler<(String, bool)>,
    on_disconnect: EventHandler<String>,
) -> Element {
    let mut pending = use_signal(|| None::<PendingApp>);
    let mut pending_enable = use_signal(|| None::<PendingApp>);
    let mut pending_enable_admin = use_signal(|| None::<PendingApp>);

    rsx! {
        if apps.is_empty() {
            p { class: "text-sm text-beet-text-secondary leading-relaxed",
                "No app is connected. Add {origin}/mcp as a remote MCP server in your AI assistant to connect one."
            }
        } else {
            ul { class: "space-y-2.5",
                for app in apps {
                    ListRow { key: "{app.grant_id}",
                        div { class: "flex justify-between items-start flex-wrap gap-3",
                            div { class: "text-sm text-beet-text-secondary space-y-1",
                                div {
                                    span { class: "font-semibold text-beet-text-primary font-display", "{app.app_host}" }
                                    if let Some(owner) = &app.owner {
                                        span { class: "text-xs text-beet-text-tertiary ml-2", "({owner})" }
                                    }
                                    if !app.mine && app.allow_admin {
                                        span { class: "text-[11px] font-medium text-beet-accent-text bg-beet-accent-surface border border-beet-accent-solid/40 rounded px-1.5 py-0.5 ml-2",
                                            "Admin access"
                                        }
                                    }
                                }
                                if let Some(name) = &app.client_name {
                                    div { class: "text-xs text-beet-text-secondary", "It calls itself \"{name}\"." }
                                }
                                div { class: "text-xs tabular-nums-mono text-beet-text-tertiary",
                                    match app.last_used_at {
                                        Some(at) => format!("Last used {}", utc_time(at, "%Y-%m-%d")),
                                        None => "Never used".to_string(),
                                    }
                                }
                                if app.mine {
                                    div { class: "space-y-1.5 pt-1",
                                        label { class: "flex items-center gap-2 text-xs text-beet-text-secondary cursor-pointer",
                                            input {
                                                class: "accent-beet-leaf-solid",
                                                r#type: "checkbox",
                                                checked: app.allow_delete,
                                                // A box being saved fires no click, so a second click
                                                // before the list is read again sends nothing.
                                                disabled: saving.as_deref() == Some(app.grant_id.as_str()),
                                                // The click is cancelled so the browser never toggles the
                                                // box itself: it only changes when `checked` changes, that
                                                // is when the stored value does. A cancelled confirmation
                                                // therefore leaves it as stored. (Resource::restart keeps
                                                // the old value, so a reload would not reset a toggled box.)
                                                onclick: {
                                                    let asked = PendingApp {
                                                        grant_id: app.grant_id.clone(),
                                                        app_host: app.app_host.clone(),
                                                    };
                                                    let was_allowed = app.allow_delete;
                                                    move |evt: MouseEvent| {
                                                        evt.prevent_default();
                                                        if needs_confirmation(was_allowed) {
                                                            pending_enable.set(Some(asked.clone()));
                                                        } else {
                                                            on_allow_delete.call((asked.grant_id.clone(), false));
                                                        }
                                                    }
                                                },
                                            }
                                            "Allow managing library files (delete, move, re-tag)"
                                        }
                                        if is_admin {
                                            label { class: "flex items-center gap-2 text-xs text-beet-text-secondary cursor-pointer",
                                                input {
                                                    class: "accent-beet-accent-solid",
                                                    r#type: "checkbox",
                                                    checked: app.allow_admin,
                                                    disabled: saving.as_deref() == Some(app.grant_id.as_str()),
                                                    onclick: {
                                                        let asked = PendingApp {
                                                            grant_id: app.grant_id.clone(),
                                                            app_host: app.app_host.clone(),
                                                        };
                                                        let was_allowed = app.allow_admin;
                                                        move |evt: MouseEvent| {
                                                            evt.prevent_default();
                                                            if needs_confirmation(was_allowed) {
                                                                pending_enable_admin.set(Some(asked.clone()));
                                                            } else {
                                                                on_allow_admin.call((asked.grant_id.clone(), false));
                                                            }
                                                        }
                                                    },
                                                }
                                                "Admin library access (all users' folders)"
                                            }
                                        }
                                    }
                                }
                            }
                            button {
                                class: REMOVE_BUTTON_CLASS,
                                onclick: {
                                    let asked = PendingApp {
                                        grant_id: app.grant_id.clone(),
                                        app_host: app.app_host.clone(),
                                    };
                                    move |_| pending.set(Some(asked.clone()))
                                },
                                "Disconnect"
                            }
                        }
                    }
                }
            }
        }
        if let Some(asked) = pending_enable() {
            ConfirmModal {
                message: enable_deletion_message(&asked.app_host),
                confirm_label: "Allow managing files",
                danger: true,
                on_confirm: move |_| {
                    pending_enable.set(None);
                    on_allow_delete.call((asked.grant_id.clone(), true));
                },
                on_cancel: move |_| {
                    pending_enable.set(None);
                },
            }
        }
        if let Some(asked) = pending_enable_admin() {
            ConfirmModal {
                message: enable_admin_message(&asked.app_host),
                confirm_label: "Allow admin access",
                danger: true,
                on_confirm: move |_| {
                    pending_enable_admin.set(None);
                    on_allow_admin.call((asked.grant_id.clone(), true));
                },
                on_cancel: move |_| {
                    pending_enable_admin.set(None);
                },
            }
        }
        if let Some(asked) = pending() {
            ConfirmModal {
                message: format!(
                    "Disconnect {}? It will have to be connected again from the app.",
                    asked.app_host,
                ),
                confirm_label: "Disconnect",
                danger: true,
                on_confirm: move |_| {
                    pending.set(None);
                    on_disconnect.call(asked.grant_id.clone());
                },
                on_cancel: move |_| pending.set(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(apps: Vec<ConnectedApp>, origin: &str) -> String {
        render_with(apps, origin, false, None)
    }

    /// render, with the switch of grant `saving` being saved.
    fn render_saving(apps: Vec<ConnectedApp>, origin: &str, saving: Option<&str>) -> String {
        render_with(apps, origin, false, saving)
    }

    fn render_with(
        apps: Vec<ConnectedApp>,
        origin: &str,
        is_admin: bool,
        saving: Option<&str>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(apps, origin, is_admin, saving): (Vec<ConnectedApp>, String, bool, Option<String>)| {
                rsx! {
                    AppList {
                        apps,
                        origin,
                        is_admin,
                        saving,
                        on_allow_delete: |_| {},
                        on_allow_admin: |_| {},
                        on_disconnect: |_| {},
                    }
                }
            },
            (apps, origin.to_string(), is_admin, saving.map(str::to_string)),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn app(
        mine: bool,
        owner: Option<&str>,
        allow_delete: bool,
        last_used_at: Option<i64>,
    ) -> ConnectedApp {
        ConnectedApp {
            grant_id: "g1".into(),
            app_host: "claude.ai".into(),
            client_name: Some("Claude".into()),
            owner: owner.map(Into::into),
            last_used_at,
            allow_delete,
            allow_admin: false,
            mine,
        }
    }

    #[test]
    fn turning_deletion_on_asks_and_turning_it_off_does_not() {
        assert!(needs_confirmation(false));
        assert!(!needs_confirmation(true));
    }

    #[test]
    fn turning_deletion_on_asks_with_the_apps_name() {
        assert_eq!(
            enable_deletion_message("claude.ai"),
            "Let claude.ai manage library files (delete, move between folders, and re-tag)? Before deleting, moving or re-tagging tracks it has to ask you first. While this is on it can also run your rating sync, which deletes the songs you rated 1 star when auto-delete is on, and keep a discovery track, which replaces an earlier copy of the song. Deletions are limited to at most 100 tracks in any 24 hours, for all your connected apps together. Deleted files cannot be restored."
        );
    }

    #[test]
    fn an_app_row_shows_its_host_name_and_switch() {
        // 1_800_000_000 is 2027-01-15T08:00:00Z
        let html = render(
            vec![app(true, None, true, Some(1_800_000_000))],
            "https://music.example.com",
        );
        // dioxus-ssr escapes text with askama_escape (dioxus-ssr 0.7.4 src/renderer.rs:197),
        // which writes a quote as &#34; (askama_escape 0.13.0 src/html.rs:64,113)
        for text in [
            "claude.ai",
            "It calls itself &#34;Claude&#34;.",
            "Last used 2027-01-15",
            "Allow managing library files (delete, move, re-tag)",
            "checked",
            "Disconnect",
        ] {
            assert!(html.contains(text), "{text}");
        }
    }

    #[test]
    fn the_switch_being_saved_takes_no_click() {
        let apps = || vec![app(true, None, false, None)];
        let html = render_saving(apps(), "https://music.example.com", Some("g1"));
        assert!(html.contains("disabled"), "{html}");
        let html = render(apps(), "https://music.example.com");
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn another_users_app_has_no_switch_for_the_admin() {
        let html = render(
            vec![app(false, Some("bob"), false, None)],
            "https://music.example.com",
        );
        assert!(
            html.contains("(bob)") && html.contains("Never used") && html.contains("Disconnect")
        );
        assert!(!html.contains("Allow managing library files (delete, move, re-tag)"));
    }

    #[test]
    fn no_apps_says_how_to_connect_one() {
        let html = render(vec![], "https://music.example.com");
        assert!(html.contains(
            "No app is connected. Add https://music.example.com/mcp as a remote MCP server in your AI assistant to connect one."
        ));
    }

    #[test]
    fn admin_sees_admin_library_access_toggle_on_own_apps_only() {
        let mut own = app(true, None, false, None);
        own.allow_admin = true;
        let html_user = render_with(vec![own.clone()], "https://music.example.com", false, None);
        assert!(!html_user.contains("Admin library access (all users"));
        let html_admin = render_with(vec![own], "https://music.example.com", true, None);
        assert!(html_admin.contains("Admin library access (all users&#39; folders)"));

        let other = app(false, Some("bob"), false, None);
        let html_other = render_with(vec![other], "https://music.example.com", true, None);
        assert!(!html_other.contains("Admin library access (all users"));
    }
}
