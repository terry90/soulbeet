//! How the signed-in user signs in: their password, single sign-on, and the
//! Navidrome login Soulbeet uses for them.

use api::auth::{sso_unlink_advice, PasswordProof, SignInMethods, SsoLinkResult, NO_PASSWORD};
use api::{change_password, connect_navidrome, start_sso_link, unlink_sso};
use dioxus::prelude::*;
use shared::system::NavidromeStatus;

use super::panel::{Messages, LABEL_CLASS};
use crate::auth::use_auth;
use crate::components::friendly_error;
use crate::{submitted_text, Autofill, TextInput};

const NOTE_CLASS: &str = "text-xs text-gray-500 font-mono";

/// The keys the password forms submit their fields under.
const CURRENT_PASSWORD: &str = "current-password";
const NEW_PASSWORD: &str = "new-password";

/// What the link that came back from the provider says, as the panel's
/// first error or success text.
fn link_messages(result: Option<SsoLinkResult>) -> (String, String) {
    match result {
        None => (String::new(), String::new()),
        Some(SsoLinkResult::Linked) => (String::new(), SsoLinkResult::Linked.message().into()),
        Some(failed) => (failed.message().into(), String::new()),
    }
}

/// The hint of the field that takes the account's password before a link or
/// a new password, or `None` when the account has no password to give.
fn password_placeholder(proof: PasswordProof) -> Option<&'static str> {
    match proof {
        PasswordProof::Password => Some("Current password"),
        PasswordProof::NavidromePassword => Some("Your Navidrome password"),
        PasswordProof::NoPassword => None,
    }
}

/// The signed-in account's username, out of sight, so a password manager
/// fills in and saves the passwords beside it for that account.
#[component]
fn AccountName() -> Element {
    let username = use_auth().username().unwrap_or_default();
    rsx! {
      input {
        r#type: "text",
        hidden: true,
        readonly: true,
        autocomplete: "username",
        value: username,
      }
    }
}

/// Changes the signed-in user's password once they enter the one the login
/// form takes (`proof`), as a link asks: a session left open is not enough.
/// An account without one is told to ask an admin. `on_change` runs once the
/// password is changed: the account may now unlink single sign-on.
#[component]
pub(super) fn PasswordSection(proof: PasswordProof, on_change: EventHandler<()>) -> Element {
    let auth = use_auth();
    let mut error = use_signal(String::new);
    let mut success = use_signal(String::new);
    // Counts changed passwords: each one mounts the form again, empty.
    let mut changed = use_signal(|| 0u32);

    // Read from the form, which holds what a password manager filled in
    // without an input event.
    let change = move |e: Event<FormData>| {
        e.prevent_default();
        let current = submitted_text(&e, CURRENT_PASSWORD);
        let password = submitted_text(&e, NEW_PASSWORD);
        error.set(String::new());
        success.set(String::new());
        if password.is_empty() {
            error.set("Password cannot be empty".to_string());
            return;
        }
        spawn(async move {
            match auth.call(change_password(current, password)).await {
                Ok(()) => {
                    success.set("Password updated".to_string());
                    changed += 1;
                    on_change.call(());
                }
                Err(e) => error.set(friendly_error(&e)),
            }
        });
    };

    rsx! {
      div {
        label { class: LABEL_CLASS, "Password" }
        Messages { error: error(), success: success() }
        if let Some(placeholder) = password_placeholder(proof) {
          for count in [changed()] {
            // A post, so a submit the app fails to catch never puts the
            // passwords in the address.
            form { key: "{count}", method: "post", onsubmit: change,
              AccountName {}
              TextInput {
                name: CURRENT_PASSWORD,
                placeholder,
                aria_label: "Current password",
                autofill: Autofill::CurrentPassword,
                password: true,
              }
              TextInput {
                name: NEW_PASSWORD,
                placeholder: "New password",
                aria_label: "New password",
                autofill: Autofill::NewPassword,
                password: true,
                class: "mt-2",
              }
              button { class: "retro-btn mt-2 rounded", "type": "submit", "Change Password" }
            }
          }
        } else {
          p { class: NOTE_CLASS, "{NO_PASSWORD}" }
        }
      }
    }
}

/// Links the account to the single sign-on provider, or removes the link.
/// Shows nothing while single sign-on is off and the account has no link.
/// `notice` holds how a link that just came back from the provider ended;
/// the section takes it, so it is shown once.
#[component]
pub(super) fn SingleSignOnSection(
    methods: SignInMethods,
    notice: Signal<Option<SsoLinkResult>>,
    on_change: EventHandler<()>,
) -> Element {
    let auth = use_auth();
    let mut notice = notice;
    let result = use_hook(move || notice.write().take());
    let mut error = use_signal(|| link_messages(result).0);
    let mut success = use_signal(|| link_messages(result).1);
    let mut leaving = use_signal(|| false);

    let link = move |e: Event<FormData>| {
        e.prevent_default();
        let password = submitted_text(&e, CURRENT_PASSWORD);
        error.set(String::new());
        success.set(String::new());
        leaving.set(true);
        spawn(async move {
            match auth.call(start_sso_link(password)).await {
                // A full page load: the provider's sign-in page, then back.
                Ok(url) => {
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().set_href(&url);
                    }
                }
                Err(e) => {
                    error.set(friendly_error(&e));
                    leaving.set(false);
                }
            }
        });
    };

    let unlink = move |_| async move {
        error.set(String::new());
        success.set(String::new());
        let Some(user_id) = auth.user_id() else {
            return;
        };
        match auth.call(unlink_sso(user_id)).await {
            Ok(()) => {
                success.set("Single sign-on unlinked".to_string());
                on_change.call(());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    if methods.sso_name.is_none() && methods.sso_provider.is_none() {
        return rsx! {};
    }
    let provider = methods
        .sso_provider
        .clone()
        .unwrap_or_else(|| "the single sign-on provider".to_string());

    rsx! {
      div { class: "mt-6",
        label { class: LABEL_CLASS, "Single sign-on" }
        Messages { error: error(), success: success() }
        if let Some(name) = methods.sso_name.clone() {
          p { class: NOTE_CLASS, "Linked to {name} at {provider}." }
          if methods.password_proof != PasswordProof::NoPassword {
            button { class: "retro-btn mt-2 rounded", onclick: unlink, "Unlink" }
          } else {
            p { class: "{NOTE_CLASS} mt-2", "{sso_unlink_advice(methods.navidrome)}" }
          }
        } else if let Some(placeholder) = password_placeholder(methods.password_proof) {
          p { class: "{NOTE_CLASS} mb-2",
            "Link this account to {provider} to sign in with it. Enter your password first."
          }
          form { method: "post", onsubmit: link,
            AccountName {}
            TextInput {
              name: CURRENT_PASSWORD,
              placeholder,
              aria_label: "Password to link single sign-on",
              autofill: Autofill::CurrentPassword,
              password: true,
            }
            button {
              class: "retro-btn mt-2 rounded",
              "type": "submit",
              disabled: leaving(),
              "Link single sign-on"
            }
          }
        } else {
          p { class: NOTE_CLASS, "{NO_PASSWORD}" }
        }
      }
    }
}

/// Connects the account to Navidrome with its Navidrome password, for
/// discovery and rating sync. Also how an account made through single
/// sign-on gets Navidrome, and how any account stores a changed password.
/// `on_change` runs once a password is stored: the account may now unlink
/// single sign-on.
#[component]
pub(super) fn NavidromeSection(on_change: EventHandler<()>) -> Element {
    let mut auth = use_auth();
    let mut password = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut success = use_signal(String::new);
    let mut connecting = use_signal(|| false);

    let connect = move |_| async move {
        error.set(String::new());
        success.set(String::new());
        connecting.set(true);
        match auth.call(connect_navidrome(password())).await {
            Ok(()) => {
                auth.set_navidrome_status(NavidromeStatus::Connected);
                password.set(String::new());
                success.set("Navidrome connected".to_string());
                on_change.call(());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        connecting.set(false);
    };

    let note = match auth.navidrome_status() {
        NavidromeStatus::Connected => {
            "Connected. Enter your Navidrome password again here after you change it."
        }
        _ => "Enter your Navidrome password to use discovery and rating sync.",
    };

    rsx! {
      div { class: "mt-6",
        label { class: LABEL_CLASS, "Navidrome" }
        Messages { error: error(), success: success() }
        p { class: "{NOTE_CLASS} mb-2", "{note}" }
        TextInput {
          value: password(),
          oninput: move |v| password.set(v),
          placeholder: "Navidrome password",
          aria_label: "Navidrome password",
          password: true,
        }
        button {
          class: "retro-btn mt-2 rounded",
          disabled: connecting(),
          onclick: connect,
          "Connect Navidrome"
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opening_tag;

    /// `copies` sections, one after the other, sharing one notice
    /// `result`, as Settings mounts the section again on each visit.
    fn render_times(
        methods: SignInMethods,
        result: Option<SsoLinkResult>,
        copies: usize,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(methods, result, copies): (SignInMethods, Option<SsoLinkResult>, usize)| {
                let signed_out = use_signal(|| None);
                use_context_provider(|| crate::auth::Auth::new(signed_out));
                let notice = use_signal(|| result);
                rsx! {
                    for _ in 0..copies {
                        SingleSignOnSection { methods: methods.clone(), notice, on_change: |_| {} }
                    }
                }
            },
            (methods, result, copies),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn render(methods: SignInMethods, result: Option<SsoLinkResult>) -> String {
        render_times(methods, result, 1)
    }

    fn offered() -> SignInMethods {
        SignInMethods {
            sso_provider: Some("Pocket ID".into()),
            ..SignInMethods::default()
        }
    }

    #[test]
    fn an_unlinked_account_is_offered_the_link_for_its_password() {
        let html = render(offered(), None);
        assert!(html.contains("Link this account to Pocket ID"), "{html}");
        let field = opening_tag(&html, "aria-label=\"Password to link single sign-on\"");
        for attribute in [
            "name=\"current-password\"",
            "autocomplete=\"current-password\"",
            "placeholder=\"Current password\"",
        ] {
            assert!(field.contains(attribute), "{attribute} in {field}");
        }
        assert!(!field.contains("value"), "{field}");
        // A form, so the browser fills it in apart from the other fields.
        assert!(
            opening_tag(&html, "<form").contains("method=\"post\""),
            "{html}"
        );
        assert!(
            opening_tag(&html, "autocomplete=\"username\"").contains("hidden"),
            "{html}"
        );
        assert!(html.contains("Link single sign-on"), "{html}");
        assert!(!html.contains("Unlink"), "{html}");

        // An account whose only password is its Navidrome one.
        let html = render(
            SignInMethods {
                password_proof: PasswordProof::NavidromePassword,
                ..offered()
            },
            None,
        );
        assert!(
            html.contains("placeholder=\"Your Navidrome password\""),
            "{html}"
        );
        assert!(html.contains("Link single sign-on"), "{html}");
    }

    /// What the advice to get a password from an admin says it costs.
    const LINK_AGAIN: &str =
        "If single sign-on is linked, that removes the link, so link it again from Settings after.";

    #[test]
    fn an_account_without_a_password_is_told_to_ask_an_admin() {
        let html = render(
            SignInMethods {
                password_proof: PasswordProof::NoPassword,
                ..offered()
            },
            None,
        );
        assert!(html.contains("Ask an admin to set one"), "{html}");
        assert!(html.contains(LINK_AGAIN), "{html}");
        assert!(!html.contains("Link single sign-on<"), "{html}");
        assert!(!html.contains("Current password"), "{html}");
    }

    #[test]
    fn nothing_shows_while_single_sign_on_is_off_and_unused() {
        assert_eq!(render(SignInMethods::default(), None), "");
    }

    #[test]
    fn a_linked_account_unlinks_only_when_it_keeps_a_way_in() {
        let linked = SignInMethods {
            sso_name: Some("ann@id".into()),
            password_proof: PasswordProof::NoPassword,
            ..offered()
        };
        let html = render(linked.clone(), None);
        // The identity the provider vouched for, so the user can check it.
        assert!(html.contains("Linked to ann@id at Pocket ID."), "{html}");
        // An account that cannot unlink has no password, so it is told what
        // it can do: connect Navidrome only when that is set up.
        assert!(html.contains(sso_unlink_advice(false)), "{html}");
        assert!(!html.contains(">Unlink<"), "{html}");
        let html = render(
            SignInMethods {
                navidrome: true,
                ..linked.clone()
            },
            None,
        );
        assert!(html.contains(sso_unlink_advice(true)), "{html}");
        assert!(!html.contains(">Unlink<"), "{html}");

        // A password the login form takes is the way in that unlinking keeps.
        for password_proof in [PasswordProof::Password, PasswordProof::NavidromePassword] {
            let html = render(
                SignInMethods {
                    password_proof,
                    navidrome: true,
                    ..linked.clone()
                },
                None,
            );
            assert!(html.contains(">Unlink<"), "{html}");
            assert!(!html.contains("To unlink single sign-on"), "{html}");
        }
    }

    fn signed_in_as_ann(status: NavidromeStatus) -> Signal<Option<api::auth::AuthResponse>> {
        use_signal(|| {
            Some(api::auth::AuthResponse {
                username: "ann".into(),
                user_id: "u1".into(),
                navidrome_status: status,
                role: Default::default(),
                rename_allowed: false,
                password_change_required: false,
            })
        })
    }

    fn password_dom(proof: PasswordProof) -> VirtualDom {
        VirtualDom::new_with_props(
            |proof: PasswordProof| {
                let signed_in = signed_in_as_ann(NavidromeStatus::NotLinked);
                use_context_provider(|| crate::auth::Auth::new(signed_in));
                rsx! {
                    PasswordSection { proof, on_change: |_| {} }
                }
            },
            proof,
        )
    }

    fn render_password(proof: PasswordProof) -> String {
        let mut dom = password_dom(proof);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_new_password_takes_the_current_one() {
        let html = render_password(PasswordProof::Password);
        // A password manager fills in the current one and offers to save the
        // new one, both for the signed-in account.
        assert!(
            opening_tag(&html, "<form").contains("method=\"post\""),
            "{html}"
        );
        let account = opening_tag(&html, "autocomplete=\"username\"");
        assert!(account.contains("value=\"ann\""), "{account}");
        assert!(account.contains("hidden"), "{account}");
        assert!(account.contains("type=\"text\""), "{account}");
        for (label, name, autocomplete) in [
            ("Current password", CURRENT_PASSWORD, "current-password"),
            ("New password", NEW_PASSWORD, "new-password"),
        ] {
            let field = opening_tag(&html, &format!("aria-label=\"{label}\""));
            for attribute in [
                format!("name=\"{name}\""),
                format!("autocomplete=\"{autocomplete}\""),
                format!("placeholder=\"{label}\""),
                "type=\"password\"".to_string(),
            ] {
                assert!(field.contains(&attribute), "{attribute} in {field}");
            }
            assert!(!field.contains("value"), "{field}");
        }
        assert!(html.contains(">Change Password<"), "{html}");

        let html = render_password(PasswordProof::NavidromePassword);
        assert!(
            html.contains("placeholder=\"Your Navidrome password\""),
            "{html}"
        );
        assert!(html.contains(">Change Password<"), "{html}");

        // Without one, an admin sets it, which costs the account its single
        // sign-on link.
        let html = render_password(PasswordProof::NoPassword);
        assert!(
            html.contains("This account has no password. Ask an admin to set one"),
            "{html}"
        );
        assert!(html.contains(LINK_AGAIN), "{html}");
        assert!(!html.contains("Change Password"), "{html}");
        assert!(!html.contains("New password"), "{html}");
    }

    #[test]
    fn a_change_reads_the_form_when_submitted() {
        let mut dom = password_dom(PasswordProof::Password);
        let submits = crate::listeners(&mut dom, "submit");
        assert_eq!(submits.len(), 1);
        // A current password filled in, no new one: refused before any request.
        let browser_submits = crate::submit_form(
            &mut dom,
            submits[0],
            &[(CURRENT_PASSWORD, "from the vault"), (NEW_PASSWORD, "")],
        );
        assert!(!browser_submits);
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Password cannot be empty"), "{html}");
    }

    fn render_navidrome(status: NavidromeStatus) -> String {
        let mut dom = VirtualDom::new_with_props(
            |status: NavidromeStatus| {
                let signed_in = signed_in_as_ann(status);
                use_context_provider(|| crate::auth::Auth::new(signed_in));
                rsx! {
                    NavidromeSection { on_change: |_| {} }
                }
            },
            status,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn an_account_without_navidrome_is_asked_for_its_navidrome_password() {
        let html = render_navidrome(NavidromeStatus::NotLinked);
        assert!(
            html.contains("Enter your Navidrome password to use discovery"),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Navidrome password\""), "{html}");
        assert!(html.contains(">Connect Navidrome<"), "{html}");

        let html = render_navidrome(NavidromeStatus::Connected);
        assert!(html.contains("Connected."), "{html}");
        assert!(html.contains(">Connect Navidrome<"), "{html}");
    }

    #[test]
    fn the_outcome_of_a_link_is_shown() {
        let html = render(offered(), Some(SsoLinkResult::IdentityTaken));
        assert!(
            html.contains(SsoLinkResult::IdentityTaken.message()),
            "{html}"
        );
        let linked = SignInMethods {
            sso_name: Some("ann@id".into()),
            ..offered()
        };
        let html = render(linked, Some(SsoLinkResult::Linked));
        assert!(html.contains(SsoLinkResult::Linked.message()), "{html}");
    }

    #[test]
    fn the_outcome_of_a_link_is_shown_once() {
        let message = SsoLinkResult::IdentityTaken.message();
        let html = render_times(offered(), Some(SsoLinkResult::IdentityTaken), 2);
        assert_eq!(html.matches(message).count(), 1, "{html}");
    }
}
