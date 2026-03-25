use dioxus::prelude::*;
use shared::system::NavidromeStatus;

use crate::friendly_error;
use crate::settings_context::use_settings;
use crate::use_auth;

#[component]
pub fn AppConfigManager() -> Element {
    let mut settings = use_settings();
    let mut config_resource = use_resource(|| async { api::get_app_config().await });

    let config = match &*config_resource.read() {
        None => {
            return rsx! {
                div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
                    div { class: "animate-pulse text-gray-400 font-mono", "Loading..." }
                }
            };
        }
        Some(Err(e)) => {
            let msg = friendly_error(e);
            return rsx! {
                div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
                    h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "Connections" }
                    div { class: "text-red-400 text-sm font-mono mb-3", "{msg}" }
                    button {
                        class: "text-xs font-mono text-gray-400 hover:text-white underline decoration-dotted cursor-pointer",
                        onclick: move |_| config_resource.restart(),
                        "Retry"
                    }
                }
            };
        }
        Some(Ok(data)) => data.clone(),
    };

    let mut slskd_url = use_signal(|| config.slskd_url.unwrap_or_default());
    let mut slskd_api_key = use_signal(|| config.slskd_api_key.unwrap_or_default());
    let mut error = use_signal(String::new);
    let mut success_msg = use_signal(String::new);
    let mut saving = use_signal(|| false);

    let handle_save = move |_| async move {
        error.set(String::new());
        success_msg.set(String::new());
        saving.set(true);

        let config = api::AppConfigValues {
            slskd_url: Some(slskd_url()),
            slskd_api_key: Some(slskd_api_key()),
        };

        match api::update_app_config(config).await {
            Ok(_) => {
                let _ = settings.refresh_providers().await;
                success_msg.set("Configuration saved".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        saving.set(false);
    };

    rsx! {
        div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
            h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "Connections" }

            if !error().is_empty() {
                div { class: "mb-4 p-4 bg-red-900/20 border border-red-500/50 rounded text-red-400 font-mono text-sm",
                    "{error}"
                }
            }
            if !success_msg().is_empty() {
                div { class: "mb-4 p-4 bg-green-900/20 border border-green-500/50 rounded text-green-400 font-mono text-sm",
                    "{success_msg}"
                }
            }

            div { class: "space-y-6 mb-6",
                // Soulseek
                div {
                    h3 { class: "text-sm font-semibold text-white mb-3", "Soulseek (slskd)" }
                    div { class: "space-y-4",
                        div {
                            label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider", "slskd URL" }
                            input {
                                class: "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono",
                                value: "{slskd_url}",
                                oninput: move |e| slskd_url.set(e.value()),
                                placeholder: "http://localhost:5030",
                            }
                        }
                        div {
                            label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider", "slskd API Key" }
                            input {
                                class: "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono",
                                value: "{slskd_api_key}",
                                oninput: move |e| slskd_api_key.set(e.value()),
                                placeholder: "Enter slskd API key",
                                "type": "password",
                            }
                        }
                    }
                }

                // Navidrome note
                div {
                    h3 { class: "text-sm font-semibold text-white mb-3", "Navidrome" }
                    {
                        let auth = use_auth();
                        let (dot_color, status_text) = match auth.navidrome_status() {
                            NavidromeStatus::Connected => ("bg-blue-400", "Connected"),
                            NavidromeStatus::MissingReportRealPath => ("bg-yellow-500", "ReportRealPath not enabled"),
                            NavidromeStatus::InvalidCredentials => ("bg-beet-accent", "User not found in Navidrome"),
                            NavidromeStatus::Offline => ("bg-gray-500", "Server unreachable"),
                            NavidromeStatus::Unknown => ("bg-gray-500", "Not configured"),
                        };
                        rsx! {
                            div { class: "flex items-center gap-2 mb-2",
                                span { class: format!("w-2 h-2 rounded-full {dot_color}") }
                                span { class: "text-xs font-mono text-gray-300", "{status_text}" }
                            }
                        }
                    }
                    p { class: "text-xs text-gray-400 font-mono",
                        "Navidrome credentials are managed per-user through the login flow. "
                        "Set the NAVIDROME_URL environment variable on the server."
                    }
                }


            }

            button {
                class: "retro-btn rounded",
                disabled: saving(),
                onclick: handle_save,
                if saving() { "Saving..." } else { "Save Configuration" }
            }
        }
    }
}
