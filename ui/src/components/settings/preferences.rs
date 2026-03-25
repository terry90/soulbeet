use dioxus::prelude::*;

use crate::friendly_error;
use crate::settings_context::use_settings;

#[component]
pub fn PreferencesManager() -> Element {
    let mut settings = use_settings();
    let mut selected_provider = use_signal(|| settings.default_provider());
    let mut error = use_signal(String::new);
    let mut success_msg = use_signal(String::new);
    let mut saving = use_signal(|| false);
    let mut synced = use_signal(|| false);

    use_effect(move || {
        if settings.is_loaded() && !synced() {
            selected_provider.set(settings.default_provider());
            synced.set(true);
        }
    });

    let providers = settings.providers();

    let selected_unavailable = providers
        .iter()
        .find(|p| p.id == selected_provider())
        .map(|p| !p.available)
        .unwrap_or(false);

    let handle_save = move |_| async move {
        error.set(String::new());
        success_msg.set(String::new());

        let providers = settings.providers();
        let is_unavailable = providers
            .iter()
            .find(|p| p.id == selected_provider())
            .map(|p| !p.available)
            .unwrap_or(false);

        if is_unavailable {
            error.set(
                "Selected provider is not configured. Please set up the API key in the Config tab."
                    .to_string(),
            );
            return;
        }

        saving.set(true);

        let update = api::UpdateUserSettings {
            default_metadata_provider: Some(selected_provider()),
            ..Default::default()
        };

        match settings.update(update).await {
            Ok(_) => {
                success_msg.set("Settings saved successfully".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        saving.set(false);
    };

    rsx! {
        div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
            h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "Search Preferences" }

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

            div { class: "space-y-4 mb-6",
                div {
                    label { class: "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider",
                        "Default Metadata Provider"
                    }
                    select {
                        class: "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono",
                        value: "{selected_provider}",
                        onchange: move |e| selected_provider.set(e.value()),
                        for provider in providers.iter() {
                            option {
                                value: "{provider.id}",
                                selected: provider.id == selected_provider(),
                                if provider.available {
                                    "{provider.name}"
                                } else {
                                    "{provider.name} (not configured)"
                                }
                            }
                        }
                    }
                    if selected_unavailable {
                        p { class: "text-xs text-amber-400 mt-1 font-mono",
                            "This provider requires an API key. Configure it in the Config tab."
                        }
                    } else {
                        p { class: "text-xs text-gray-500 mt-1 font-mono",
                            "Choose which service to use for searching albums and tracks."
                        }
                    }
                }
            }

            button {
                class: "retro-btn rounded",
                disabled: saving(),
                onclick: handle_save,
                if saving() {
                    "Saving..."
                } else {
                    "Save Preferences"
                }
            }
        }
    }
}
