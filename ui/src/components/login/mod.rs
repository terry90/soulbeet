use std::future::Future;
use std::pin::Pin;

use api::login;
use dioxus::prelude::*;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    login: Callback<(String, String), Pin<Box<dyn Future<Output = Result<(), String>>>>>,
}

#[component]
pub fn Login(props: Props) -> Element {
    let mut username = use_signal(|| "".to_string());
    let mut password = use_signal(|| "".to_string());
    let mut error = use_signal(|| "".to_string());

    rsx! {
      div { class: "flex flex-col items-center justify-center min-h-screen bg-gray-900 text-white",
        div { class: "p-8 bg-gray-800 rounded-lg shadow-xl w-full max-w-md",
          h1 { class: "text-2xl font-bold mb-6 text-center text-teal-400",
            "SoulBeet Login"
          }
          div { class: "mb-4",
            label { class: "block text-sm font-medium mb-1", "Username" }
            input {
              class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
              value: "{username}",
              oninput: move |e| username.set(e.value()),
              "type": "text",
            }
          }
          div { class: "mb-6",
            label { class: "block text-sm font-medium mb-1", "Password" }
            input {
              class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
              value: "{password}",
              oninput: move |e| password.set(e.value()),
              "type": "password",
            }
          }

          if !error().is_empty() {
            div { class: "mb-4 text-red-500 text-sm", "{error}" }
          }

          button {
            class: "w-full bg-teal-600 hover:bg-teal-700 text-white font-bold py-2 px-4 rounded transition-colors",
            onclick: move |_| async move {
                let user = username.read().to_string();
                let pass = password.read().to_string();
                error.set("".to_string());
                match props.login.call((user, pass)).await {
                    Ok(_) => {
                        println!("Login success");
                    }
                    Err(e) => error.set(e),
                }
            },
            "Login"
          }
        }
      }
    }
}
