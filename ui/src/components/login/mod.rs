use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

use crate::{Autofill, BackgroundGlows, FieldLook, GlowColor, GlowSpread, Logo, TextInput};

type LoginCallback = Callback<(String, String), Pin<Box<dyn Future<Output = Result<(), String>>>>>;

/// The keys the login form submits its fields under.
const USERNAME: &str = "username";
const PASSWORD: &str = "password";

/// What the submitted form holds under `name`: the field's text at that
/// moment, typed or filled in by a password manager without an input event.
fn submitted(form: &FormData, name: &str) -> String {
    match form.get_first(name) {
        Some(FormValue::Text(text)) => text,
        _ => String::new(),
    }
}

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
      div { class: "flex flex-col items-center justify-center min-h-screen text-white font-display",
        // bg decorations
        BackgroundGlows { left: GlowColor::Accent, spread: GlowSpread::Wide }

        div { class: "p-8 bg-beet-panel border border-white/10 rounded-lg shadow-2xl w-full max-w-md relative z-10",
          // Header
          div { class: "flex flex-col items-center mb-8",
            Logo {
              size: 64,
              class: "mb-4 drop-shadow-[0_0_14px_rgba(255,0,255,0.5)] hover:-translate-y-1 transition-transform",
            }
            h1 { class: "text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
              "Soulbeet"
            }
            p { class: "text-sm text-beet-leaf font-mono mt-2 tracking-widest",
              "YOUR LIBRARY MANAGER"
            }
          }

          // Form
          form {
            class: "space-y-6",
            onsubmit: move |e: Event<FormData>| {
                e.prevent_default();
                handle_login(submitted(&e, USERNAME), submitted(&e, PASSWORD));
            },
            div {
              label {
                class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
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
                class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                r#for: "login-password",
                "Password"
              }
              TextInput {
                look: FieldLook::Login,
                id: "login-password",
                name: PASSWORD,
                autofill: Autofill::CurrentPassword,
                placeholder: "Enter password",
                password: true,
              }
            }

            if !shown_error.is_empty() {
              div { class: "p-3 bg-red-500/10 border border-red-500/50 rounded text-red-400 text-sm font-mono flex items-center gap-2",
                svg {
                  class: "w-4 h-4",
                  fill: "none",
                  view_box: "0 0 24 24",
                  stroke: "currentColor",
                  path {
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    stroke_width: "2",
                    d: "M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z",
                  }
                }
                "{shown_error}"
              }
            }

            button {
              class: "w-full retro-btn flex justify-center items-center gap-2 group",
              "type": "submit",
              span { "AUTHENTICATE" }
              svg {
                class: "w-4 h-4 group-hover:translate-x-1 transition-transform",
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
              div { class: "flex-1 h-px bg-white/10" }
              span { class: "text-xs font-mono text-gray-500 uppercase tracking-widest", "or" }
              div { class: "flex-1 h-px bg-white/10" }
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
                class: "w-full retro-btn flex justify-center items-center gap-2",
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
        for (id, name, kind, autocomplete) in [
            ("login-username", USERNAME, "text", "username"),
            ("login-password", PASSWORD, "password", "current-password"),
        ] {
            assert!(html.contains(&format!(" for=\"{id}\">")), "{html}");
            assert!(
                html.contains(&format!(
                    "id=\"{id}\" name=\"{name}\" type=\"{kind}\" autocomplete=\"{autocomplete}\""
                )),
                "{html}"
            );
        }
        // No value for a render to write back over what was filled in.
        assert!(!html.contains("value="), "{html}");
    }

    thread_local! {
        static SENT: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    #[test]
    fn a_sign_in_sends_what_the_form_holds_when_submitted() {
        use dioxus::core::Mutation;
        use dioxus::html::{PlatformEventData, SerializedFormData, SerializedFormObject};
        use futures::FutureExt;
        use std::{any::Any, rc::Rc};

        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        let mut dom = VirtualDom::new(|| {
            let login: LoginCallback = Callback::new(|sent| {
                SENT.with(|s| s.borrow_mut().push(sent));
                Box::pin(async { Ok(()) }) as Pin<Box<dyn Future<Output = Result<(), String>>>>
            });
            rsx! {
              Login { login, sso_provider: None, sso_error: None, return_to: None }
            }
        });
        let submits: Vec<_> = dom
            .rebuild_to_vec()
            .edits
            .into_iter()
            .filter_map(|edit| match edit {
                Mutation::NewEventListener { name, id } if name == "submit" => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(submits.len(), 1);

        // Filled in by a password manager: no input event reached the fields.
        let field = |key: &str, text: &str| SerializedFormObject {
            key: key.into(),
            text: Some(text.into()),
            file: None,
        };
        let form = SerializedFormData::new(
            String::new(),
            vec![field(USERNAME, "ann"), field(PASSWORD, "from the vault")],
        );
        let data = PlatformEventData::new(Box::new(form));
        let event = Event::new(Rc::new(data) as Rc<dyn Any>, true);
        dom.runtime()
            .handle_event("submit", event.clone(), submits[0]);
        loop {
            dom.render_immediate(&mut dioxus::core::NoOpMutations);
            if dom.wait_for_work().now_or_never().is_none() {
                break;
            }
        }

        assert_eq!(
            SENT.with(|s| s.take()),
            [("ann".to_string(), "from the vault".to_string())]
        );
        // The app signs in, not the browser's own form post.
        assert!(!event.default_action_enabled());
    }
}
