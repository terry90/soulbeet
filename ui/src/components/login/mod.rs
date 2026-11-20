use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

use crate::{FieldLook, Logo, TextInput};

type LoginCallback = Callback<(String, String), Pin<Box<dyn Future<Output = Result<(), String>>>>>;

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
    let mut username = use_signal(|| "".to_string());
    let mut password = use_signal(|| "".to_string());
    let mut error = use_signal(|| "".to_string());
    // A failed password attempt replaces the single sign-on error.
    let shown_error = if error().is_empty() {
        props.sso_error.clone().unwrap_or_default()
    } else {
        error()
    };

    let handle_login = move || {
        let user = username.read().to_string();
        let pass = password.read().to_string();
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
        div { class: "fixed top-1/4 -left-10 w-64 h-64 bg-beet-accent/10 rounded-full blur-[150px] pointer-events-none" }
        div { class: "fixed bottom-1/4 -right-10 w-64 h-64 bg-beet-leaf/10 rounded-full blur-[150px] pointer-events-none" }

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
            onsubmit: move |e| {
                e.prevent_default();
                handle_login();
            },
            div {
              label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                "Username"
              }
              TextInput {
                look: FieldLook::Login,
                value: username(),
                oninput: move |v| username.set(v),
                placeholder: "Enter username",
              }
            }
            div {
              label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                "Password"
              }
              TextInput {
                look: FieldLook::Login,
                value: password(),
                oninput: move |v| password.set(v),
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
}
