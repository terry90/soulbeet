use api::{create_user_folder, delete_folder, get_user_folders, update_folder};
use dioxus::prelude::*;

use super::panel::{
    ListRow, LoadError, Loading, Messages, Panel, SectionTitle, LABEL_CLASS, REMOVE_BUTTON_CLASS,
};
use super::toggle::{save_switch, Toggle};
use crate::auth::use_auth;
use crate::{friendly_error, FieldLook, TextInput};

#[component]
pub fn FolderManager() -> Element {
    let mut folder_name = use_signal(|| "".to_string());
    let mut folder_path = use_signal(|| "".to_string());
    let mut folders = use_signal(Vec::new);

    let mut editing_folder_id = use_signal(|| None::<String>);
    let mut edit_folder_name = use_signal(|| "".to_string());
    let mut edit_folder_path = use_signal(|| "".to_string());

    let mut error = use_signal(|| "".to_string());
    let mut success_msg = use_signal(|| "".to_string());
    let auth = use_auth();

    // Library settings state
    let mut auto_delete_enabled = use_signal(|| false);
    let mut discovery_promote_threshold = use_signal(|| "3".to_string());
    let mut lb_username = use_signal(String::new);
    let mut lb_token = use_signal(String::new);
    let mut deezer_recommendations = use_signal(|| false);
    let library_error = use_signal(String::new);

    // Discovery settings state
    let mut discovery_enabled = use_signal(|| false);
    let mut discovery_folder_id = use_signal(String::new);
    let mut discovery_profiles = use_signal(|| "Conservative,Balanced,Adventurous".to_string());
    let mut pl_name_safe = use_signal(|| "Comfort Zone".to_string());
    let mut pl_name_mix = use_signal(|| "Fresh Picks".to_string());
    let mut pl_name_wild = use_signal(|| "Deep Cuts".to_string());
    // Per-profile track counts and lifetimes
    let mut tc_safe = use_signal(|| "10".to_string());
    let mut tc_mix = use_signal(|| "10".to_string());
    let mut tc_wild = use_signal(|| "10".to_string());
    let mut lt_safe = use_signal(|| "7".to_string());
    let mut lt_mix = use_signal(|| "7".to_string());
    let mut lt_wild = use_signal(|| "7".to_string());

    let mut settings_resource = use_resource(|| async { api::get_user_settings().await });

    use_effect(move || {
        if let Some(Ok(user_settings)) = &*settings_resource.read() {
            auto_delete_enabled.set(user_settings.auto_delete_enabled);
            discovery_promote_threshold.set(user_settings.discovery_promote_threshold.to_string());
            lb_username.set(
                user_settings
                    .listenbrainz_username
                    .clone()
                    .unwrap_or_default(),
            );
            lb_token.set(user_settings.listenbrainz_token.clone().unwrap_or_default());
            deezer_recommendations.set(user_settings.deezer_recommendations);
            discovery_enabled.set(user_settings.discovery_enabled);
            discovery_folder_id.set(
                user_settings
                    .discovery_folder_id
                    .clone()
                    .unwrap_or_default(),
            );
            discovery_profiles.set(user_settings.discovery_profiles.clone());
            // Parse per-profile playlist names from JSON
            if let Ok(names) = serde_json::from_str::<std::collections::HashMap<String, String>>(
                &user_settings.discovery_playlist_name,
            ) {
                if let Some(n) = names.get("Conservative") {
                    pl_name_safe.set(n.clone());
                }
                if let Some(n) = names.get("Balanced") {
                    pl_name_mix.set(n.clone());
                }
                if let Some(n) = names.get("Adventurous") {
                    pl_name_wild.set(n.clone());
                }
            }
            // Parse per-profile track counts
            if let Ok(counts) = serde_json::from_str::<std::collections::HashMap<String, u32>>(
                &user_settings.discovery_track_count,
            ) {
                if let Some(n) = counts.get("Conservative") {
                    tc_safe.set(n.to_string());
                }
                if let Some(n) = counts.get("Balanced") {
                    tc_mix.set(n.to_string());
                }
                if let Some(n) = counts.get("Adventurous") {
                    tc_wild.set(n.to_string());
                }
            }
            // Parse per-profile lifetime days
            if let Ok(days) = serde_json::from_str::<std::collections::HashMap<String, u32>>(
                &user_settings.discovery_lifetime_days,
            ) {
                if let Some(n) = days.get("Conservative") {
                    lt_safe.set(n.to_string());
                }
                if let Some(n) = days.get("Balanced") {
                    lt_mix.set(n.to_string());
                }
                if let Some(n) = days.get("Adventurous") {
                    lt_wild.set(n.to_string());
                }
            }
        }
    });

    let fetch_folders = move || async move {
        match auth.call(get_user_folders()).await {
            Ok(fetched_folders) => folders.set(fetched_folders),
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    use_future(move || async move {
        fetch_folders().await;
    });

    let handle_add_folder = move |_| async move {
        error.set("".to_string());
        success_msg.set("".to_string());

        if folder_name().is_empty() || folder_path().is_empty() {
            error.set("Name and Path are required".to_string());
            return;
        }

        match auth
            .call(create_user_folder(folder_name(), folder_path()))
            .await
        {
            Ok(_) => {
                success_msg.set("Folder added successfully".to_string());
                folder_name.set("".to_string());
                folder_path.set("".to_string());
                fetch_folders().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_delete_folder = move |id: String| async move {
        match auth.call(delete_folder(id)).await {
            Ok(_) => {
                success_msg.set("Folder deleted successfully".to_string());
                fetch_folders().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_update_folder = move |id: String| async move {
        match auth
            .call(update_folder(id, edit_folder_name(), edit_folder_path()))
            .await
        {
            Ok(_) => {
                success_msg.set("Folder updated successfully".to_string());
                editing_folder_id.set(None);
                fetch_folders().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    rsx! {
        div { class: "space-y-6",
        Panel { title: "Manage Music Folders",
            Messages { error: error(), success: success_msg() }

            div { class: "grid grid-cols-1 md:grid-cols-2 gap-4 mb-4",
                div {
                    label { class: LABEL_CLASS, "Folder Name (e.g., 'Music/Common')" }
                    TextInput {
                        value: folder_name(),
                        oninput: move |v| folder_name.set(v),
                        placeholder: "My Music",
                    }
                }
                div {
                    label { class: LABEL_CLASS, "Folder Path" }
                    TextInput {
                        value: folder_path(),
                        oninput: move |v| folder_path.set(v),
                        placeholder: "/home/user/Music",
                    }
                }
            }

            button { class: "retro-btn mb-6 rounded", onclick: handle_add_folder, "Add Folder" }

            // Existing Folders List
            SectionTitle { title: "Existing Folders" }
            if folders.read().is_empty() {
                p { class: "text-gray-500 font-mono italic", "No folders added yet." }
            } else {
                ul { class: "space-y-2",
                    {
                        folders
                            .read()
                            .clone()
                            .into_iter()
                            .map(|folder| {
                                let id_edit = folder.id.clone();
                                let id_delete = folder.id.clone();
                                let id_update = folder.id.clone();
                                rsx! {
                                    ListRow {
                                        if editing_folder_id() == Some(folder.id.clone()) {
                                            div { class: "flex flex-col gap-2",
                                                TextInput {
                                                    look: FieldLook::Compact,
                                                    value: edit_folder_name(),
                                                    oninput: move |v| edit_folder_name.set(v),
                                                    placeholder: "Name",
                                                }
                                                TextInput {
                                                    look: FieldLook::Compact,
                                                    value: edit_folder_path(),
                                                    oninput: move |v| edit_folder_path.set(v),
                                                    placeholder: "Path",
                                                }
                                                div { class: "flex gap-2 mt-2",
                                                    button {
                                                        class: "text-xs uppercase tracking-wider font-bold text-beet-leaf hover:text-white transition-colors",
                                                        onclick: move |_| handle_update_folder(id_update.clone()),
                                                        "[ Save ]"
                                                    }
                                                    button {
                                                        class: "text-xs uppercase tracking-wider font-bold text-gray-500 hover:text-white transition-colors",
                                                        onclick: move |_| editing_folder_id.set(None),
                                                        "[ Cancel ]"
                                                    }
                                                }
                                            }
                                        } else {
                                            div { class: "flex justify-between items-center",
                                                div {
                                                    span { class: "font-bold text-white block font-display", "{folder.name}" }
                                                    span { class: "text-gray-500 text-xs font-mono", "{folder.path}" }
                                                }
                                                div { class: "flex gap-3",
                                                    button {
                                                        class: "text-xs font-mono text-gray-400 hover:text-beet-accent transition-colors underline decoration-dotted",
                                                        onclick: move |_| {
                                                            edit_folder_name.set(folder.name.clone());
                                                            edit_folder_path.set(folder.path.clone());
                                                            editing_folder_id.set(Some(id_edit.clone()));
                                                        },
                                                        "Edit"
                                                    }
                                                    button {
                                                        class: REMOVE_BUTTON_CLASS,
                                                        onclick: move |_| handle_delete_folder(id_delete.clone()),
                                                        "Delete"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            })
                    }
                }
            }
        }

        // Library Settings
        Panel { title: "Library Settings",
            Messages { error: library_error() }
            match &*settings_resource.read() {
                None => rsx! { Loading {} },
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| settings_resource.restart() }
                },
                Some(Ok(_)) => rsx! {
                div { class: "space-y-4",
                    // Discovery (user-level)
                    div {
                        h3 { class: "text-sm font-semibold text-white mb-3", "Discovery" }
                        p { class: "text-xs text-gray-500 font-mono mb-4",
                            "Automatically download new tracks for listening based on your scrobble history."
                        }
                        div { class: "space-y-3",
                            // Enable/disable toggle
                            div { class: "flex items-center justify-between p-3 bg-beet-dark rounded border border-white/10",
                                div {
                                    p { class: "text-sm text-white font-medium", "Enable Discovery" }
                                    p { class: "text-xs text-gray-500 font-mono mt-0.5",
                                        "Periodically generate a playlist of new tracks to try"
                                    }
                                }
                                Toggle {
                                    on: discovery_enabled(),
                                    on_class: "bg-beet-leaf",
                                    onclick: move |_| save_switch(discovery_enabled, library_error, |on| {
                                        api::update_user_settings(api::UpdateUserSettings {
                                            discovery_enabled: Some(on),
                                            ..Default::default()
                                        })
                                    }),
                                }
                            }

                            if discovery_enabled() {
                                // Folder selector
                                div { class: "p-3 bg-beet-dark rounded border border-white/10",
                                    label { class: LABEL_CLASS, "Download Folder" }
                                    if folders.read().is_empty() {
                                        p { class: "text-gray-500 font-mono text-sm italic", "No folders configured. Add a folder above first." }
                                    } else {
                                        select {
                                            class: "w-full p-2 rounded bg-beet-panel border border-white/10 text-white font-mono text-sm",
                                            value: "{discovery_folder_id}",
                                            onchange: move |e| async move {
                                                let val = e.value();
                                                discovery_folder_id.set(val.clone());
                                                let update = api::UpdateUserSettings {
                                                    discovery_folder_id: Some(val),
                                                    ..Default::default()
                                                };
                                                let _ = api::update_user_settings(update).await;
                                            },
                                            option { value: "", "Select a folder..." }
                                            for f in folders.read().iter() {
                                                option {
                                                    value: "{f.id}",
                                                    selected: discovery_folder_id() == f.id,
                                                    "{f.name} ({f.path})"
                                                }
                                            }
                                        }
                                    }
                                }

                                // Playlists: toggle + name + track count + lifetime per profile
                                div { class: "p-3 bg-beet-dark rounded border border-white/10",
                                    label { class: "block text-xs font-mono text-gray-400 mb-2 uppercase tracking-wider", "Playlists" }
                                    div { class: "space-y-2",
                                        for (label, value, color, name_signal, tc_signal, lt_signal) in [
                                            ("Safe", "Conservative", "border-blue-500/40 bg-blue-600/10", &pl_name_safe, &tc_safe, &lt_safe),
                                            ("Mix", "Balanced", "border-green-500/40 bg-green-600/10", &pl_name_mix, &tc_mix, &lt_mix),
                                            ("Wild", "Adventurous", "border-purple-500/40 bg-purple-600/10", &pl_name_wild, &tc_wild, &lt_wild),
                                        ] {
                                            {
                                                let active = discovery_profiles().split(',').any(|p| p.trim() == value);
                                                rsx! {
                                                    div {
                                                        class: format!(
                                                            "p-2 rounded border transition-colors {}",
                                                            if active { color } else { "border-white/5 bg-white/[0.02] opacity-50" }
                                                        ),
                                                        div { class: "flex items-center gap-2",
                                                            // Toggle button
                                                            button {
                                                                class: format!(
                                                                    "shrink-0 w-16 py-0.5 text-xs font-mono rounded cursor-pointer transition-colors text-center {}",
                                                                    if active { "bg-white/15 text-white" } else { "bg-white/5 text-gray-500" }
                                                                ),
                                                                onclick: move |_| async move {
                                                                    let current: Vec<String> = discovery_profiles().split(',')
                                                                        .map(|s| s.trim().to_string())
                                                                        .filter(|s| !s.is_empty())
                                                                        .collect();
                                                                    let new_profiles = if current.iter().any(|p| p == value) {
                                                                        let filtered: Vec<_> = current.into_iter().filter(|p| p != value).collect();
                                                                        if filtered.is_empty() { value.to_string() } else { filtered.join(",") }
                                                                    } else {
                                                                        let mut updated = current;
                                                                        updated.push(value.to_string());
                                                                        updated.join(",")
                                                                    };
                                                                    discovery_profiles.set(new_profiles.clone());
                                                                    let update = api::UpdateUserSettings {
                                                                        discovery_profiles: Some(new_profiles),
                                                                        ..Default::default()
                                                                    };
                                                                    let _ = api::update_user_settings(update).await;
                                                                },
                                                                "{label}"
                                                            }
                                                            // Name input
                                                            input {
                                                                class: "flex-1 p-1 rounded bg-beet-panel/50 border border-white/5 focus:border-beet-accent focus:outline-none text-white font-mono text-xs",
                                                                value: "{name_signal}",
                                                                disabled: !active,
                                                                oninput: {
                                                                    let signal = *name_signal;
                                                                    move |e: Event<FormData>| signal.clone().set(e.value())
                                                                },
                                                                onchange: move |_| async move {
                                                                    let names = serde_json::json!({
                                                                        "Conservative": pl_name_safe(),
                                                                        "Balanced": pl_name_mix(),
                                                                        "Adventurous": pl_name_wild(),
                                                                    });
                                                                    let update = api::UpdateUserSettings {
                                                                        discovery_playlist_name: Some(names.to_string()),
                                                                        ..Default::default()
                                                                    };
                                                                    let _ = api::update_user_settings(update).await;
                                                                },
                                                                placeholder: value,
                                                            }
                                                        }
                                                        // Per-profile track count and lifetime
                                                        if active {
                                                            div { class: "flex gap-2 mt-1.5 ml-18",
                                                                div { class: "flex items-center gap-1",
                                                                    span { class: "text-[10px] font-mono text-gray-500", "Tracks" }
                                                                    input {
                                                                        class: "w-12 p-0.5 rounded bg-beet-panel/50 border border-white/5 focus:border-beet-accent focus:outline-none text-white font-mono text-xs text-center",
                                                                        "type": "number",
                                                                        min: "1",
                                                                        max: "50",
                                                                        value: "{tc_signal}",
                                                                        oninput: {
                                                                            let sig = *tc_signal;
                                                                            move |e: Event<FormData>| sig.clone().set(e.value())
                                                                        },
                                                                        onchange: move |_| async move {
                                                                            let counts = serde_json::json!({
                                                                                "Conservative": tc_safe().parse::<u32>().unwrap_or(10),
                                                                                "Balanced": tc_mix().parse::<u32>().unwrap_or(10),
                                                                                "Adventurous": tc_wild().parse::<u32>().unwrap_or(10),
                                                                            });
                                                                            let update = api::UpdateUserSettings {
                                                                                discovery_track_count: Some(counts.to_string()),
                                                                                ..Default::default()
                                                                            };
                                                                            let _ = api::update_user_settings(update).await;
                                                                        },
                                                                    }
                                                                }
                                                                div { class: "flex items-center gap-1",
                                                                    span { class: "text-[10px] font-mono text-gray-500", "Days" }
                                                                    input {
                                                                        class: "w-12 p-0.5 rounded bg-beet-panel/50 border border-white/5 focus:border-beet-accent focus:outline-none text-white font-mono text-xs text-center",
                                                                        "type": "number",
                                                                        min: "1",
                                                                        max: "90",
                                                                        value: "{lt_signal}",
                                                                        oninput: {
                                                                            let sig = *lt_signal;
                                                                            move |e: Event<FormData>| sig.clone().set(e.value())
                                                                        },
                                                                        onchange: move |_| async move {
                                                                            let days = serde_json::json!({
                                                                                "Conservative": lt_safe().parse::<u32>().unwrap_or(7),
                                                                                "Balanced": lt_mix().parse::<u32>().unwrap_or(7),
                                                                                "Adventurous": lt_wild().parse::<u32>().unwrap_or(7),
                                                                            });
                                                                            let update = api::UpdateUserSettings {
                                                                                discovery_lifetime_days: Some(days.to_string()),
                                                                                ..Default::default()
                                                                            };
                                                                            let _ = api::update_user_settings(update).await;
                                                                        },
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // ListenBrainz credentials
                    div { class: "p-3 bg-beet-dark rounded border border-white/10",
                        div {
                            p { class: "text-sm text-white font-medium mb-1", "ListenBrainz Username" }
                            input {
                                class: "w-full p-2 rounded bg-beet-panel border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono text-sm",
                                value: "{lb_username}",
                                oninput: move |e| lb_username.set(e.value()),
                                onchange: move |_| async move {
                                    let update = api::UpdateUserSettings {
                                        listenbrainz_username: Some(lb_username()),
                                        ..Default::default()
                                    };
                                    let _ = api::update_user_settings(update).await;
                                },
                                placeholder: "Enter ListenBrainz username",
                                "type": "text",
                            }
                            p { class: "text-xs text-gray-500 font-mono mt-2",
                                "Your ListenBrainz history joins your Navidrome plays for recommendations."
                            }
                        }
                        div { class: "mt-3",
                            p { class: "text-sm text-white font-medium mb-1", "ListenBrainz Token (optional)" }
                            p { class: "text-xs text-gray-500 font-mono mb-2",
                                "Not needed. With it, artist top tracks come from ListenBrainz's own ranking and ListenBrainz allows more requests."
                            }
                            input {
                                class: "w-full p-2 rounded bg-beet-panel border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono text-sm",
                                value: "{lb_token}",
                                oninput: move |e| lb_token.set(e.value()),
                                onchange: move |_| async move {
                                    let update = api::UpdateUserSettings {
                                        listenbrainz_token: Some(lb_token()),
                                        ..Default::default()
                                    };
                                    let _ = api::update_user_settings(update).await;
                                },
                                placeholder: "Enter ListenBrainz token",
                                "type": "password",
                            }
                        }
                    }

                    // Deezer recommendations
                    div { class: "flex items-center justify-between p-3 bg-beet-dark rounded border border-white/10",
                        div {
                            p { class: "text-sm text-white font-medium", "Also use Deezer for recommendations" }
                            p { class: "text-xs text-gray-500 font-mono mt-0.5",
                                "Sends the names of artists you play to Deezer. Deezer has not approved this use."
                            }
                        }
                        Toggle {
                            on: deezer_recommendations(),
                            on_class: "bg-beet-leaf",
                            onclick: move |_| save_switch(deezer_recommendations, library_error, |on| {
                                api::update_user_settings(api::UpdateUserSettings {
                                    deezer_recommendations: Some(on),
                                    ..Default::default()
                                })
                            }),
                        }
                    }

                    // Auto-delete toggle
                    div { class: "flex items-center justify-between p-3 bg-beet-dark rounded border border-white/10",
                        div {
                            p { class: "text-sm text-white font-medium", "Auto-delete 1-star tracks" }
                            p { class: "text-xs text-gray-500 font-mono mt-0.5",
                                "Automatically delete files rated 1 star in Navidrome during sync"
                            }
                        }
                        Toggle {
                            on: auto_delete_enabled(),
                            on_class: "bg-red-500",
                            onclick: move |_| save_switch(auto_delete_enabled, library_error, |on| {
                                api::update_user_settings(api::UpdateUserSettings {
                                    auto_delete_enabled: Some(on),
                                    ..Default::default()
                                })
                            }),
                        }
                    }

                    // Discovery promote threshold
                    div { class: "flex items-center justify-between p-3 bg-beet-dark rounded border border-white/10",
                        div {
                            p { class: "text-sm text-white font-medium", "Discovery promote threshold" }
                            p { class: "text-xs text-gray-500 font-mono mt-0.5",
                                "Promote discovery tracks to your library when rated at or above this"
                            }
                        }
                        input {
                            class: "w-16 p-2 rounded bg-beet-panel border border-white/10 text-white font-mono text-sm text-center",
                            value: "{discovery_promote_threshold}",
                            oninput: move |e| discovery_promote_threshold.set(e.value()),
                            onchange: move |_| async move {
                                let val: u8 = discovery_promote_threshold().parse().unwrap_or(3);
                                let update = api::UpdateUserSettings {
                                    discovery_promote_threshold: Some(val),
                                    ..Default::default()
                                };
                                let _ = api::update_user_settings(update).await;
                            },
                            "type": "number",
                            min: "1",
                            max: "5",
                        }
                    }
                }
                },
            }
        }
        }
    }
}
