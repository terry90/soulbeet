use std::ops::RangeInclusive;

use api::models::user_settings::{LIFETIME_DAYS, PROMOTE_THRESHOLDS, TRACK_COUNTS};
use api::{create_user_folder, delete_folder, get_user_folders, update_folder, UserSettings};
use dioxus::prelude::*;
use serde_json::Value;
use shared::recommendation::DiscoveryProfile;

use super::panel::{
    ListRow, LoadError, Loading, Messages, Panel, SectionTitle, LABEL_CLASS, REMOVE_BUTTON_CLASS,
};
use super::save_setting::{typed_number, SettingSaver};
use super::toggle::{save_switch, Toggle};
use crate::auth::use_auth;
use crate::{friendly_error, FieldLook, TextInput};

/// The numbers typed for the Safe, Mix and Wild profiles as the JSON object the server stores, or
/// why one of them cannot be saved. `what` names the number in that reason.
fn typed_per_profile(
    what: &str,
    range: RangeInclusive<i64>,
    [safe, mix, wild]: [String; 3],
) -> Result<String, String> {
    let read =
        |label: &str, text: &str| typed_number(text, &format!("The {label} {what}"), range.clone());
    Ok(serde_json::json!({
        "Conservative": read("Safe", &safe)?,
        "Balanced": read("Mix", &mix)?,
        "Adventurous": read("Wild", &wild)?,
    })
    .to_string())
}

/// Puts in `field` the value `read` finds for `profile` in the stored JSON object `json`, of
/// profile to value. A value it cannot find leaves the field as it is.
fn show_profile(
    json: &str,
    profile: &str,
    mut field: Signal<String>,
    read: fn(&Value) -> Option<String>,
) {
    let map = serde_json::from_str::<Value>(json).ok();
    if let Some(text) = map.as_ref().and_then(|m| m.get(profile)).and_then(read) {
        field.set(text);
    }
}

fn name_text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn number_text(value: &Value) -> Option<String> {
    value.as_i64().map(|n| n.to_string())
}

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
    let pl_name_safe = use_signal(|| "Comfort Zone".to_string());
    let pl_name_mix = use_signal(|| "Fresh Picks".to_string());
    let pl_name_wild = use_signal(|| "Deep Cuts".to_string());
    // Per-profile track counts and lifetimes
    let tc_safe = use_signal(|| "10".to_string());
    let tc_mix = use_signal(|| "10".to_string());
    let tc_wild = use_signal(|| "10".to_string());
    let lt_safe = use_signal(|| "7".to_string());
    let lt_mix = use_signal(|| "7".to_string());
    let lt_wild = use_signal(|| "7".to_string());

    let mut settings_resource = use_resource(|| async { api::get_user_settings().await });
    let saver = SettingSaver {
        stored: settings_resource,
        error: library_error,
    };

    // Each puts one input's stored value in place: after a save only the saved input is
    // refilled, so the reply never overwrites another field being typed in.
    let mut show_threshold = move |s: &UserSettings| {
        discovery_promote_threshold.set(s.discovery_promote_threshold.to_string())
    };
    let mut show_lb_username = move |s: &UserSettings| {
        lb_username.set(s.listenbrainz_username.clone().unwrap_or_default())
    };
    let mut show_lb_token =
        move |s: &UserSettings| lb_token.set(s.listenbrainz_token.clone().unwrap_or_default());
    let mut show_discovery_folder = move |s: &UserSettings| {
        discovery_folder_id.set(s.discovery_folder_id.clone().unwrap_or_default())
    };
    let mut show_profiles =
        move |s: &UserSettings| discovery_profiles.set(s.discovery_profiles.clone());

    use_effect(move || {
        if let Some(Ok(s)) = &*settings_resource.read() {
            auto_delete_enabled.set(s.auto_delete_enabled);
            deezer_recommendations.set(s.deezer_recommendations);
            discovery_enabled.set(s.discovery_enabled);
            show_threshold(s);
            show_lb_username(s);
            show_lb_token(s);
            show_discovery_folder(s);
            show_profiles(s);
            let fields = [
                (pl_name_safe, tc_safe, lt_safe),
                (pl_name_mix, tc_mix, lt_mix),
                (pl_name_wild, tc_wild, lt_wild),
            ];
            for (profile, (name, count, lifetime)) in
                DiscoveryProfile::NAMES.into_iter().zip(fields)
            {
                show_profile(&s.discovery_playlist_name, profile, name, name_text);
                show_profile(&s.discovery_track_count, profile, count, number_text);
                show_profile(&s.discovery_lifetime_days, profile, lifetime, number_text);
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
                            "Automatically download new tracks based on what you play and star in Navidrome, plus your ListenBrainz listens when a username is saved."
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
                                            onchange: move |e| {
                                                let val = e.value();
                                                discovery_folder_id.set(val.clone());
                                                let change = api::UpdateUserSettings {
                                                    discovery_folder_id: Some(val),
                                                    ..Default::default()
                                                };
                                                saver.save(Ok(change), show_discovery_folder)
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
                                                                onclick: move |_| {
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
                                                                    let change = api::UpdateUserSettings {
                                                                        discovery_profiles: Some(new_profiles),
                                                                        ..Default::default()
                                                                    };
                                                                    saver.save(Ok(change), show_profiles)
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
                                                                onchange: {
                                                                    let field = *name_signal;
                                                                    move |_: Event<FormData>| {
                                                                        let names = serde_json::json!({
                                                                            "Conservative": pl_name_safe(),
                                                                            "Balanced": pl_name_mix(),
                                                                            "Adventurous": pl_name_wild(),
                                                                        });
                                                                        let change = api::UpdateUserSettings {
                                                                            discovery_playlist_name: Some(names.to_string()),
                                                                            ..Default::default()
                                                                        };
                                                                        saver.save(Ok(change), move |s: &UserSettings| {
                                                                            show_profile(&s.discovery_playlist_name, value, field, name_text)
                                                                        })
                                                                    }
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
                                                                        min: TRACK_COUNTS.start().to_string(),
                                                                        max: TRACK_COUNTS.end().to_string(),
                                                                        value: "{tc_signal}",
                                                                        oninput: {
                                                                            let sig = *tc_signal;
                                                                            move |e: Event<FormData>| sig.clone().set(e.value())
                                                                        },
                                                                        onchange: {
                                                                            let field = *tc_signal;
                                                                            move |_: Event<FormData>| {
                                                                                let counts = [tc_safe(), tc_mix(), tc_wild()];
                                                                                let change = typed_per_profile("track count", TRACK_COUNTS, counts)
                                                                                    .map(|counts| api::UpdateUserSettings {
                                                                                        discovery_track_count: Some(counts),
                                                                                        ..Default::default()
                                                                                    });
                                                                                saver.save(change, move |s: &UserSettings| {
                                                                                    show_profile(&s.discovery_track_count, value, field, number_text)
                                                                                })
                                                                            }
                                                                        },
                                                                    }
                                                                }
                                                                div { class: "flex items-center gap-1",
                                                                    span { class: "text-[10px] font-mono text-gray-500", "Days" }
                                                                    input {
                                                                        class: "w-12 p-0.5 rounded bg-beet-panel/50 border border-white/5 focus:border-beet-accent focus:outline-none text-white font-mono text-xs text-center",
                                                                        "type": "number",
                                                                        min: LIFETIME_DAYS.start().to_string(),
                                                                        max: LIFETIME_DAYS.end().to_string(),
                                                                        value: "{lt_signal}",
                                                                        oninput: {
                                                                            let sig = *lt_signal;
                                                                            move |e: Event<FormData>| sig.clone().set(e.value())
                                                                        },
                                                                        onchange: {
                                                                            let field = *lt_signal;
                                                                            move |_: Event<FormData>| {
                                                                                let days = [lt_safe(), lt_mix(), lt_wild()];
                                                                                let change = typed_per_profile("lifetime in days", LIFETIME_DAYS, days)
                                                                                    .map(|days| api::UpdateUserSettings {
                                                                                        discovery_lifetime_days: Some(days),
                                                                                        ..Default::default()
                                                                                    });
                                                                                saver.save(change, move |s: &UserSettings| {
                                                                                    show_profile(&s.discovery_lifetime_days, value, field, number_text)
                                                                                })
                                                                            }
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
                                onchange: move |_| {
                                    let change = api::UpdateUserSettings {
                                        listenbrainz_username: Some(lb_username()),
                                        ..Default::default()
                                    };
                                    saver.save(Ok(change), show_lb_username)
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
                                onchange: move |_| {
                                    let change = api::UpdateUserSettings {
                                        listenbrainz_token: Some(lb_token()),
                                        ..Default::default()
                                    };
                                    saver.save(Ok(change), show_lb_token)
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
                            onchange: move |_| {
                                let change = typed_number(
                                    &discovery_promote_threshold(),
                                    "The discovery promote threshold",
                                    PROMOTE_THRESHOLDS,
                                )
                                .map(|n| api::UpdateUserSettings {
                                    discovery_promote_threshold: Some(n),
                                    ..Default::default()
                                });
                                saver.save(change, show_threshold)
                            },
                            "type": "number",
                            min: PROMOTE_THRESHOLDS.start().to_string(),
                            max: PROMOTE_THRESHOLDS.end().to_string(),
                        }
                    }
                }
                },
            }
        }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_profile_numbers_are_sent_only_when_each_one_reads() {
        let counts = typed_per_profile(
            "track count",
            TRACK_COUNTS,
            ["10".into(), "50".into(), " 1 ".into()],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&counts).unwrap(),
            serde_json::json!({"Conservative": 10, "Balanced": 50, "Adventurous": 1})
        );
        assert_eq!(
            typed_per_profile(
                "track count",
                TRACK_COUNTS,
                ["10".into(), "".into(), "10".into()]
            ),
            Err("The Mix track count must be a whole number from 1 to 50".to_string())
        );
        assert_eq!(
            typed_per_profile(
                "lifetime in days",
                LIFETIME_DAYS,
                ["7".into(), "7".into(), "91".into()]
            ),
            Err("The Wild lifetime in days must be a whole number from 1 to 90".to_string())
        );
    }
}
