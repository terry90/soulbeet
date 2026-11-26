//! The Direct Connect settings under its switch in Settings > Config > Connections: where the
//! AirDC++ sidecar is and how Soulful signs in to it, and a check of what the sidecar says of
//! itself. The password is never shown, only whether one is stored.

use std::future::Future;
use std::pin::Pin;

use api::{SidecarReport, SidecarSettings, SidecarSettingsUpdate};
use dioxus::prelude::*;

use super::panel::{
    LoadError, Messages, LABEL_CLASS, PRIMARY_BUTTON_CLASS, SECONDARY_BUTTON_CLASS,
};
use crate::{friendly_error, submitted_text, use_auth, Autofill, FieldKind, TextInput};

const URL: &str = "airdcpp_url";
const USERNAME: &str = "airdcpp_username";
const PASSWORD: &str = "airdcpp_password";

type Saver = Callback<
    SidecarSettingsUpdate,
    Pin<Box<dyn Future<Output = Result<SidecarSettings, ServerFnError>>>>,
>;
type Checker = Callback<(), Pin<Box<dyn Future<Output = Result<SidecarReport, ServerFnError>>>>>;

/// The settings form, once the stored settings are read.
#[component]
pub(super) fn DirectConnectSettings() -> Element {
    let auth = use_auth();
    let mut stored =
        use_resource(move || async move { auth.call(api::get_airdcpp_settings()).await });
    let save: Saver = use_callback(move |update: SidecarSettingsUpdate| {
        Box::pin(auth.call(api::update_airdcpp_settings(update)))
            as Pin<Box<dyn Future<Output = Result<SidecarSettings, ServerFnError>>>>
    });
    let check: Checker = use_callback(move |()| {
        Box::pin(auth.call(api::check_airdcpp()))
            as Pin<Box<dyn Future<Output = Result<SidecarReport, ServerFnError>>>>
    });
    let settings = match &*stored.read() {
        None => return rsx! {},
        Some(Err(e)) => {
            return rsx! {
                LoadError { message: friendly_error(e), on_retry: move |_| stored.restart() }
            };
        }
        Some(Ok(settings)) => settings.clone(),
    };
    rsx! {
        DirectConnectForm { stored: settings, save, check }
    }
}

#[component]
fn DirectConnectForm(stored: SidecarSettings, save: Saver, check: Checker) -> Element {
    let mut shown = use_signal(|| stored.clone());
    let mut error = use_signal(String::new);
    let mut success = use_signal(String::new);
    let mut report = use_signal(|| None::<SidecarReport>);
    let mut checking = use_signal(|| false);
    // Counts saves: each one mounts the form again, its password field empty.
    let mut saves = use_signal(|| 0u32);

    // Read from the form, which holds what a password manager filled in without an input event.
    let submit = move |e: Event<FormData>| {
        e.prevent_default();
        let password = submitted_text(&e, PASSWORD);
        let update = SidecarSettingsUpdate {
            url: submitted_text(&e, URL),
            username: submitted_text(&e, USERNAME),
            password: (!password.is_empty()).then_some(password),
        };
        error.set(String::new());
        success.set(String::new());
        spawn(async move {
            match save.call(update).await {
                Ok(settings) => {
                    shown.set(settings);
                    saves += 1;
                    success.set("Direct Connect settings saved".to_string());
                }
                Err(e) => error.set(friendly_error(&e)),
            }
        });
    };
    let run_check = move |_| {
        error.set(String::new());
        checking.set(true);
        spawn(async move {
            match check.call(()).await {
                Ok(found) => report.set(Some(found)),
                Err(e) => {
                    report.set(None);
                    error.set(friendly_error(&e));
                }
            }
            checking.set(false);
        });
    };
    let settings = shown();
    let password_hint = match settings.password_set {
        true => "Stored. Type a new one to change it",
        false => "Not set",
    };

    rsx! {
        div { class: "mt-3 pl-3.5 border-l border-beet-border-subtle space-y-3",
            h4 { class: "text-[11px] tabular-nums-mono text-beet-text-tertiary uppercase tracking-wider", "AirDC++ sidecar" }
            Messages { error: error(), success: success() }
            for count in [saves()] {
                // A post, so a submit the app fails to catch never puts the password in the
                // address.
                form { key: "{count}", method: "post", class: "space-y-3", onsubmit: submit,
                    div {
                        label { class: LABEL_CLASS, "AirDC++ address" }
                        TextInput {
                            name: URL,
                            value: settings.url.clone(),
                            placeholder: "http://airdcpp:5600",
                            aria_label: "AirDC++ address",
                        }
                    }
                    div {
                        label { class: LABEL_CLASS, "AirDC++ user name" }
                        TextInput {
                            name: USERNAME,
                            value: settings.username.clone(),
                            placeholder: "The web user Soulbeet signs in as",
                            aria_label: "AirDC++ user name",
                        }
                    }
                    div {
                        label { class: LABEL_CLASS, "AirDC++ password" }
                        TextInput {
                            name: PASSWORD,
                            placeholder: password_hint,
                            aria_label: "AirDC++ password",
                            autofill: Autofill::NewPassword,
                            kind: FieldKind::Password,
                        }
                    }
                    div { class: "flex flex-wrap items-center gap-2.5",
                        button { class: PRIMARY_BUTTON_CLASS, "type": "submit", "Save Direct Connect" }
                        button {
                            class: SECONDARY_BUTTON_CLASS,
                            "type": "button",
                            disabled: checking(),
                            onclick: run_check,
                            if checking() { "Checking..." } else { "Check AirDC++" }
                        }
                    }
                }
            }
            if let Some(found) = report() {
                {check_lines(&found)}
            }
        }
    }
}

/// What a check found, one line each.
fn check_lines(report: &SidecarReport) -> Element {
    let yes_no = |flag: Option<bool>, yes: &str, no: &str| match flag {
        Some(true) => yes.to_string(),
        Some(false) => no.to_string(),
        None => "unknown".to_string(),
    };
    let mut lines: Vec<(&str, String)> = Vec::new();
    if let Some(version) = &report.client_version {
        lines.push(("Client", version.clone()));
    }
    lines.push((
        "Hubs",
        format!("{} of {} connected", report.hubs_connected, report.hubs),
    ));
    lines.push((
        "Connections",
        yes_no(
            report.passive,
            "passive: peers that take no incoming connection either cannot send",
            "active",
        ),
    ));
    lines.push((
        "Shared files",
        report
            .shared_files
            .map_or("unknown".into(), |n| n.to_string()),
    ));
    lines.push((
        "Partial sharing",
        yes_no(
            report.partial_sharing,
            "on: other peers can fetch parts of a file while it downloads",
            "off",
        ),
    ));
    if let Some(folder) = &report.download_directory {
        let same = yes_no(
            report.same_folder,
            "the folder Soulbeet downloads into",
            "not the folder Soulbeet downloads into",
        );
        lines.push(("Download folder", format!("{folder} ({same})")));
    }
    lines.push((
        "Web user",
        yes_no(report.admin_user, "an AirDC++ admin", "not an admin"),
    ));
    rsx! {
        div { class: "space-y-1 pt-1",
            match &report.problem {
                Some(problem) => rsx! {
                    p { class: "text-xs text-beet-crimson-text", "{problem}" }
                },
                None => rsx! {
                    p { class: "text-xs text-beet-leaf-text", "Soulbeet can use this AirDC++" }
                },
            }
            for (label, value) in lines {
                p { class: "text-xs tabular-nums-mono text-beet-text-tertiary",
                    span { class: "text-beet-text-secondary", "{label}: " }
                    "{value}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Asked = Rc<RefCell<Vec<SidecarSettingsUpdate>>>;

    fn form(stored: SidecarSettings, asked: Asked, report: SidecarReport) -> VirtualDom {
        VirtualDom::new_with_props(
            move |(stored, asked, report): (SidecarSettings, Asked, SidecarReport)| {
                let save: Saver = use_callback(move |update: SidecarSettingsUpdate| {
                    asked.borrow_mut().push(update.clone());
                    Box::pin(async move {
                        Ok(SidecarSettings {
                            url: update.url,
                            username: update.username,
                            password_set: true,
                        })
                    })
                        as Pin<Box<dyn Future<Output = Result<SidecarSettings, ServerFnError>>>>
                });
                let check: Checker = use_callback(move |()| {
                    let report = report.clone();
                    Box::pin(async move { Ok(report) })
                        as Pin<Box<dyn Future<Output = Result<SidecarReport, ServerFnError>>>>
                });
                rsx! { DirectConnectForm { stored, save, check } }
            },
            (stored, asked, report),
        )
    }

    fn stored() -> SidecarSettings {
        SidecarSettings {
            url: "http://airdcpp:5600".into(),
            username: "soulbeet".into(),
            password_set: true,
        }
    }

    #[test]
    fn the_form_shows_the_address_and_the_user_and_never_a_password() {
        let mut dom = form(stored(), Rc::default(), SidecarReport::default());
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("value=\"http://airdcpp:5600\""), "{html}");
        assert!(html.contains("value=\"soulbeet\""), "{html}");
        let password = crate::opening_tag(&html, "aria-label=\"AirDC++ password\"");
        assert!(password.contains("type=\"password\""), "{password}");
        assert!(!password.contains("value"), "{password}");
        assert!(
            password.contains("Stored. Type a new one to change it"),
            "{password}"
        );

        let mut dom = form(
            SidecarSettings::default(),
            Rc::default(),
            SidecarReport::default(),
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("placeholder=\"Not set\""), "{html}");
    }

    #[test]
    fn a_save_sends_the_typed_settings_and_no_password_when_none_is_typed() {
        let asked: Asked = Rc::default();
        let mut dom = form(stored(), asked.clone(), SidecarReport::default());
        let submits = crate::listeners(&mut dom, "submit");
        assert_eq!(submits.len(), 1);
        crate::submit_form(
            &mut dom,
            submits[0],
            &[
                (URL, "http://dc.lan:5600"),
                (USERNAME, "sb"),
                (PASSWORD, ""),
            ],
        );
        assert_eq!(
            *asked.borrow(),
            vec![SidecarSettingsUpdate {
                url: "http://dc.lan:5600".into(),
                username: "sb".into(),
                password: None,
            }]
        );
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Direct Connect settings saved"), "{html}");
        assert!(html.contains("value=\"http://dc.lan:5600\""), "{html}");

        let submits = crate::listeners(&mut dom, "submit");
        crate::submit_form(
            &mut dom,
            submits[0],
            &[
                (URL, "http://dc.lan:5600"),
                (USERNAME, "sb"),
                (PASSWORD, "n3w"),
            ],
        );
        assert_eq!(asked.borrow()[1].password.as_deref(), Some("n3w"));
    }

    #[test]
    fn a_check_shows_what_the_sidecar_said() {
        let report = SidecarReport {
            problem: Some("AirDC++ is connected to no hub".into()),
            client_version: Some("AirDC++w 2.14.0 x86_64 (API 1)".into()),
            hubs_connected: 0,
            hubs: 3,
            passive: Some(true),
            shared_files: Some(0),
            partial_sharing: Some(false),
            download_directory: Some("/downloads/".into()),
            same_folder: Some(true),
            admin_user: Some(false),
        };
        let mut dom = form(stored(), Rc::default(), report);
        let clicks = crate::listeners(&mut dom, "click");
        assert_eq!(
            clicks.len(),
            1,
            "the check is the one button that listens to clicks"
        );
        crate::click(&mut dom, clicks[0]);
        let html = dioxus_ssr::render(&dom);
        for text in [
            "AirDC++ is connected to no hub",
            "AirDC++w 2.14.0 x86_64 (API 1)",
            "0 of 3 connected",
            "passive: peers that take no incoming connection either cannot send",
            "/downloads/ (the folder Soulbeet downloads into)",
            "not an admin",
        ] {
            assert!(html.contains(text), "{text} in {html}");
        }
        assert!(!html.contains("Online"), "{html}");
    }
}
