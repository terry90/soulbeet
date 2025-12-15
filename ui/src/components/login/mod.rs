use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;

type LoginCallback = Callback<(String, String), Pin<Box<dyn Future<Output = Result<(), String>>>>>;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    login: LoginCallback,
}

#[component]
pub fn Login(props: Props) -> Element {
    let mut username = use_signal(|| "".to_string());
    let mut password = use_signal(|| "".to_string());
    let mut error = use_signal(|| "".to_string());

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
            div { class: "w-12 h-12 bg-beet-accent rounded-sm flex items-center justify-center shadow-[0_0_15px_rgba(217,70,239,0.5)] mb-4 rotate-3 hover:rotate-6 transition-transform",
              svg {
                class: "w-8 h-8 text-white",
                fill: "none",
                stroke: "currentColor",
                view_box: "0 0 24 24",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  stroke_width: "2",
                  d: "M9 19V6l12-3v13M9 19c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2zm12-3c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2zM9 10l12-3",
                }
              }
            }
            h1 { class: "text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
              "Soulbeet"
            }
            p { class: "text-sm text-beet-leaf font-mono mt-2 tracking-widest",
              "YOUR LIBRARY MANAGER"
            }
          }

          // Form
          div { class: "space-y-6",
            div {
              label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                "Username"
              }
              input {
                class: "w-full bg-beet-dark border border-white/10 rounded p-3 text-white focus:outline-none focus:border-beet-accent focus:shadow-[0_0_10px_rgba(217,70,239,0.3)] transition-all font-mono",
                value: "{username}",
                oninput: move |e| username.set(e.value()),
                "type": "text",
                placeholder: "Enter username",
                onkeydown: move |e| {
                    if e.key() == Key::Enter {
                        handle_login();
                    }
                },
              }
            }
            div {
              label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                "Password"
              }
              input {
                class: "w-full bg-beet-dark border border-white/10 rounded p-3 text-white focus:outline-none focus:border-beet-accent focus:shadow-[0_0_10px_rgba(217,70,239,0.3)] transition-all font-mono",
                value: "{password}",
                oninput: move |e| password.set(e.value()),
                "type": "password",
                placeholder: "Enter password",
                onkeydown: move |e| {
                    if e.key() == Key::Enter {
                        handle_login();
                    }
                },
              }
            }

            if !error().is_empty() {
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
                "{error}"
              }
            }

            button {
              class: "w-full retro-btn flex justify-center items-center gap-2 group",
              onclick: move |_| handle_login(),
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
        }
      }
    }
}
