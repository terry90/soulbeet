use dioxus::prelude::*;
use shared::system::NavidromeStatus;

use super::connection_status_row::{beets_row, ConnectionStatusRow, ImporterDetails};
use super::panel::{LoadError, Loading, Messages, Panel, LABEL_CLASS};
use crate::friendly_error;
use crate::settings_context::use_settings;
use crate::{use_auth, use_system_health, TextInput, NOT_LINKED_HINT};

#[component]
pub fn AppConfigManager() -> Element {
    let mut settings = use_settings();
    let auth = use_auth();
    let mut config_resource =
        use_resource(move || async move { auth.call(api::get_app_config()).await });

    let config = match &*config_resource.read() {
        None => {
            return rsx! {
                Panel { title: "Connections", Loading {} }
            };
        }
        Some(Err(e)) => {
            return rsx! {
                Panel { title: "Connections",
                    LoadError { message: friendly_error(e), on_retry: move |_| config_resource.restart() }
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

        match auth.call(api::update_app_config(config)).await {
            Ok(_) => {
                let _ = settings.refresh_providers().await;
                success_msg.set("Configuration saved".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        saving.set(false);
    };

    rsx! {
        Panel { title: "Connections",
            Messages { error: error(), success: success_msg() }

            div { class: "space-y-6 mb-6",
                // Soulseek
                div {
                    h3 { class: "text-sm font-semibold text-white mb-3", "Soulseek (slskd)" }
                    div { class: "space-y-4",
                        div {
                            label { class: LABEL_CLASS, "slskd URL" }
                            TextInput {
                                value: slskd_url(),
                                oninput: move |v| slskd_url.set(v),
                                placeholder: "http://localhost:5030",
                            }
                        }
                        div {
                            label { class: LABEL_CLASS, "slskd API Key" }
                            TextInput {
                                value: slskd_api_key(),
                                oninput: move |v| slskd_api_key.set(v),
                                placeholder: "Enter slskd API key",
                                password: true,
                            }
                        }
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

/// Navidrome and Beets status, shown to every user.
#[component]
pub fn ConnectionStatusPanel() -> Element {
    let auth = use_auth();
    let health = use_system_health();

    rsx! {
        Panel { title: "Status",
            div { class: "space-y-6",
                {
                    let (dot_class, summary) = match auth.navidrome_status() {
                        NavidromeStatus::Connected => ("bg-blue-400", "Connected"),
                        NavidromeStatus::InvalidCredentials => ("bg-beet-accent", "User not found in Navidrome"),
                        NavidromeStatus::Offline => ("bg-gray-500", "Server unreachable"),
                        NavidromeStatus::NotLinked => ("bg-beet-accent", NOT_LINKED_HINT),
                        NavidromeStatus::Unknown => ("bg-gray-500", "Not configured"),
                    };
                    rsx! {
                        ConnectionStatusRow {
                            label: "Navidrome",
                            dot_class,
                            summary,
                            note: "Each user's Navidrome password is stored when they sign in with it or use Connect Navidrome in Settings > Users. Set the NAVIDROME_URL environment variable on the server.".to_string(),
                        }
                    }
                }
                {
                    let importer = health.importer();
                    let row = beets_row(&importer);
                    let has_details = !importer.issues.is_empty() || !importer.facts.is_empty();
                    rsx! {
                        ConnectionStatusRow {
                            label: "Beets",
                            dot_class: row.dot_class,
                            summary: row.summary,
                            note: row.note,
                            details: has_details.then(|| rsx! { ImporterDetails { health: importer.clone() } }),
                        }
                    }
                }
            }
        }
    }
}
