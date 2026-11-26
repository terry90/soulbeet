use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

use crate::{submitted_text, Autofill, FieldKind, FieldLook, Logo, TextInput};

type LoginCallback = Callback<(String, String), Pin<Box<dyn Future<Output = Result<(), String>>>>>;

/// The keys the login form submits its fields under.
const USERNAME: &str = "username";
const PASSWORD: &str = "password";

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    login: LoginCallback,
    /// Name of the single sign-on provider, when one is configured
    sso_provider: Option<String>,
    /// Why the last single sign-on attempt failed
    sso_error: Option<String>,
    /// The authorize request to go back to once signed in
    return_to: Option<String>,
}

#[component]
pub fn Login(props: Props) -> Element {
    let mut error = use_signal(|| "".to_string());
    // A failed password attempt replaces the single sign-on error.
    let shown_error = if error().is_empty() {
        props.sso_error.clone().unwrap_or_default()
    } else {
        error()
    };

    let handle_login = move |user: String, pass: String| {
        spawn(async move {
            error.set("".to_string());
            match props.login.call((user, pass)).await {
                Ok(_) => {
                    // Login success logic usually handled by parent/router redirect
                }
                Err(e) => error.set(e),
            }
        });
    };

    rsx! {
      div { class: "flex flex-col items-center justify-center min-h-screen px-4 py-8 bg-beet-canvas text-beet-text-primary font-display",
        div { class: "w-full max-w-[400px] bg-beet-surface-low border border-beet-border-subtle rounded-[16px] p-7",
          // Brand lockup
          div { class: "flex flex-col items-center text-center mb-7",
            Logo {
              size: 48,
              class: "mb-3.5",
            }
            h1 { class: "text-2xl font-bold tracking-tight text-beet-text-primary",
              "Soulbeet"
            }
            p { class: "text-sm text-beet-text-secondary mt-1",
              "Sign in to your personal studio"
            }
          }

          // A post, so a submit the app fails to catch never puts the
          // password in the address.
          form {
            class: "space-y-6",
            method: "post",
            onsubmit: move |e: Event<FormData>| {
                e.prevent_default();
                handle_login(submitted_text(&e, USERNAME), submitted_text(&e, PASSWORD));
            },
            div { class: "space-y-4",
              div {
                label {
                  class: "block text-xs font-medium text-beet-text-secondary mb-1.5",
                  r#for: "login-username",
                  "Username"
                }
                TextInput {
                  look: FieldLook::Login,
                  id: "login-username",
                  name: USERNAME,
                  autofill: Autofill::Username,
                  placeholder: "Enter username",
                }
              }
              div {
                label {
                  class: "block text-xs font-medium text-beet-text-secondary mb-1.5",
                  r#for: "login-password",
                  "Password"
                }
                TextInput {
                  look: FieldLook::Login,
                  id: "login-password",
                  name: PASSWORD,
                  autofill: Autofill::CurrentPassword,
                  placeholder: "Enter password",
                  kind: FieldKind::Password,
                }
              }
            }

            if !shown_error.is_empty() {
              div { class: "px-3.5 py-2.5 bg-beet-crimson-surface border border-beet-crimson-solid/40 rounded-[10px] text-beet-crimson-text text-sm flex items-center gap-2.5",
                svg {
                  class: "w-4 h-4 shrink-0",
                  fill: "none",
                  view_box: "0 0 24 24",
                  stroke: "currentColor",
                  path {
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    stroke_width: "1.75",
                    d: "M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z",
                  }
                }
                span { "{shown_error}" }
              }
            }

            button {
              class: "w-full h-10 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink font-semibold text-sm tracking-wide flex justify-center items-center gap-2 hover:brightness-105 transition-all cursor-pointer group",
              "type": "submit",
              span { "AUTHENTICATE" }
              svg {
                class: "w-4 h-4 group-hover:translate-x-0.5 transition-transform",
                fill: "none",
                view_box: "0 0 24 24",
                stroke: "currentColor",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  stroke_width: "2",
                  d: "M14 5l7 7m0 0l-7 7m7-7H3",
                }
              }
            }
          }

          if let Some(provider) = props.sso_provider {
            div { class: "flex items-center gap-3 my-6",
              div { class: "flex-1 h-px bg-beet-border-subtle" }
              span { class: "text-xs text-beet-text-tertiary uppercase tracking-wider", "or" }
              div { class: "flex-1 h-px bg-beet-border-subtle" }
            }
            // A native form, not a link: the router would handle a link
            // inside the app, and this must be a full page load so the
            // server can redirect to the provider. A post, so the browser
            // sends its Origin: on plain http at a name, the server then
            // knows to come back on http, as a password login does.
            form { method: "post", action: api::auth::OIDC_LOGIN_PATH,
              if let Some(back) = props.return_to {
                input { "type": "hidden", name: "return_to", value: "{back}" }
              }
              button {
                class: "w-full h-10 px-4 rounded-[10px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-default hover:border-beet-border-strong text-beet-text-primary font-medium text-sm flex justify-center items-center gap-2 transition-colors cursor-pointer",
                "type": "submit",
                "Sign in with {provider}"
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
    use crate::opening_tag as tag;

    fn render(sso_provider: Option<&str>, return_to: Option<&str>) -> String {
        #[derive(Props, PartialEq, Clone)]
        struct Page {
            sso_provider: Option<String>,
            return_to: Option<String>,
        }
        let mut dom = VirtualDom::new_with_props(
            |page: Page| {
                let login: LoginCallback = Callback::new(|_| {
                    Box::pin(async { Ok(()) }) as Pin<Box<dyn Future<Output = Result<(), String>>>>
                });
                rsx! {
                  Login {
                    login,
                    sso_provider: page.sso_provider,
                    sso_error: None,
                    return_to: page.return_to,
                  }
                }
            },
            Page {
                sso_provider: sso_provider.map(Into::into),
                return_to: return_to.map(Into::into),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_single_sign_on_button_posts_with_the_authorize_request() {
        // A post, so the browser sends its Origin: the server reads plain
        // http at a name from it, as for a password login.
        let html = render(
            Some("Pocket ID"),
            Some("/oauth/authorize?client_id=c&state=s"),
        );
        let form = format!(
            "<form method=\"post\" action=\"{}\">\
             <input type=\"hidden\" name=\"return_to\" value=\"/oauth/authorize?client_id=c&#38;state=s\"/>\
             <button ",
            api::auth::OIDC_LOGIN_PATH
        );
        assert!(html.contains(&form), "{html}");
        assert!(html.contains("Sign in with Pocket ID"));

        let html = render(Some("Pocket ID"), None);
        assert!(html.contains(&format!(
            "<form method=\"post\" action=\"{}\"><button ",
            api::auth::OIDC_LOGIN_PATH
        )));
        assert!(!html.contains("return_to"));

        assert!(!render(None, None).contains(api::auth::OIDC_LOGIN_PATH));
    }

    #[test]
    fn the_fields_tell_a_password_manager_what_they_hold() {
        let html = render(None, None);
        // A post, so the browser's own submit never puts them in the address.
        assert!(
            tag(&html, "class=\"space-y-6\"").contains("method=\"post\""),
            "{html}"
        );
        for (id, name, kind, autocomplete) in [
            ("login-username", USERNAME, "text", "username"),
            ("login-password", PASSWORD, "password", "current-password"),
        ] {
            assert!(html.contains(&format!(" for=\"{id}\">")), "{html}");
            let field = tag(&html, &format!("id=\"{id}\""));
            for attribute in [
                format!("name=\"{name}\""),
                format!("type=\"{kind}\""),
                format!("autocomplete=\"{autocomplete}\""),
            ] {
                assert!(field.contains(&attribute), "{attribute} in {field}");
            }
        }
        // No value for a render to write back over what was filled in.
        assert!(!html.contains("value="), "{html}");
    }

    thread_local! {
        static SENT: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    #[test]
    fn a_sign_in_sends_what_the_form_holds_when_submitted() {
        let mut dom = VirtualDom::new(|| {
            let login: LoginCallback = Callback::new(|sent| {
                SENT.with(|s| s.borrow_mut().push(sent));
                Box::pin(async { Ok(()) }) as Pin<Box<dyn Future<Output = Result<(), String>>>>
            });
            rsx! {
              Login { login, sso_provider: None, sso_error: None, return_to: None }
            }
        });
        let submits = crate::listeners(&mut dom, "submit");
        assert_eq!(submits.len(), 1);

        // Filled in by a password manager: no input event reached the fields.
        let browser_submits = crate::submit_form(
            &mut dom,
            submits[0],
            &[(USERNAME, "ann"), (PASSWORD, "from the vault")],
        );

        assert_eq!(
            SENT.with(|s| s.take()),
            [("ann".to_string(), "from the vault".to_string())]
        );
        // The app signs in, not the browser's own form post.
        assert!(!browser_submits);
    }
}
