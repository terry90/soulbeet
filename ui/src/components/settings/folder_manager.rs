use std::ops::RangeInclusive;

use api::models::folder::Folder;
use api::models::user_settings::{LossyFallback, LIFETIME_DAYS, PROMOTE_THRESHOLDS, TRACK_COUNTS};
use api::{create_user_folder, get_user_folder, update_folder, UserSettings};
use dioxus::prelude::*;
use serde_json::Value;
use shared::recommendation::DiscoveryProfile;

use super::panel::{
    LoadError, Loading, Messages, Panel, LABEL_CLASS, PRIMARY_BUTTON_CLASS, SECONDARY_BUTTON_CLASS,
};
use super::save_setting::{typed_number, SettingSaver};
use super::toggle::{save_switch, Toggle};
use crate::auth::use_auth;
use crate::{friendly_error, TextInput};

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

/// The names of the backends among `enabled` whose files are lossy only, as one text; None when
/// there is none.
fn lossy_names<'a>(enabled: impl Iterator<Item = &'a str>) -> Option<String> {
    let names: Vec<&str> = enabled
        .filter_map(shared::backends::meta)
        .filter(|m| m.delivery == shared::backends::Delivery::LossyOnly)
        .map(|m| m.name)
        .collect();
    (!names.is_empty()).then(|| names.join(" or "))
}

fn lossy_label(choice: LossyFallback) -> &'static str {
    match choice {
        LossyFallback::Never => "Never use it",
        LossyFallback::Ask => "Ask me",
        LossyFallback::Auto => "Download automatically (lossy)",
    }
}

#[component]
pub fn FolderManager() -> Element {
    let auth = use_auth();

    // Single personal library folder state
    let mut folder = use_signal(|| None::<Folder>);
    let mut folder_loading = use_signal(|| true);
    let mut editing = use_signal(|| false);
    let mut saving_folder = use_signal(|| false);
    let mut folder_name = use_signal(String::new);
    let mut folder_path = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut success_msg = use_signal(String::new);

    // Library settings state
    let mut accept_shares = use_signal(|| true);
    let mut auto_delete_enabled = use_signal(|| false);
    let mut external_rating_sync_enabled = use_signal(|| true);
    let mut discovery_promote_threshold = use_signal(|| "3".to_string());
    let mut lb_username = use_signal(String::new);
    let mut lb_token = use_signal(String::new);
    let mut deezer_recommendations = use_signal(|| false);
    let mut lossy_fallback = use_signal(LossyFallback::default);
    let library_error = use_signal(String::new);
    // The backends an admin turned on whose files are lossy only; the choice shows while one is.
    let lossy_backends = use_resource(move || async move {
        let backends = auth
            .call(api::get_backends())
            .await
            .map(|b| b.download)
            .unwrap_or_default();
        lossy_names(backends.iter().map(|b| b.id.as_str()))
    });

    // Discovery settings state
    let mut discovery_enabled = use_signal(|| false);
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

    let mut settings_resource =
        use_resource(move || async move { auth.call(api::get_user_settings()).await });
    let saver = SettingSaver {
        stored: settings_resource,
        error: library_error,
    };

    // Each puts one input's stored value in place: after a save only the saved input is
    // refilled, so the reply never overwrites another field being typed in.
    let mut show_accept_shares = move |s: &UserSettings| accept_shares.set(s.accept_shares);
    let mut show_threshold = move |s: &UserSettings| {
        discovery_promote_threshold.set(s.discovery_promote_threshold.to_string())
    };
    let mut show_lb_username = move |s: &UserSettings| {
        lb_username.set(s.listenbrainz_username.clone().unwrap_or_default())
    };
    let mut show_lb_token =
        move |s: &UserSettings| lb_token.set(s.listenbrainz_token.clone().unwrap_or_default());
    let mut show_profiles =
        move |s: &UserSettings| discovery_profiles.set(s.discovery_profiles.clone());
    let mut show_lossy = move |s: &UserSettings| lossy_fallback.set(s.lossy_fallback);

    use_effect(move || {
        if let Some(Ok(s)) = &*settings_resource.read() {
            show_accept_shares(s);
            auto_delete_enabled.set(s.auto_delete_enabled);
            external_rating_sync_enabled.set(s.external_rating_sync_enabled);
            deezer_recommendations.set(s.deezer_recommendations);
            show_lossy(s);
            discovery_enabled.set(s.discovery_enabled);
            show_threshold(s);
            show_lb_username(s);
            show_lb_token(s);
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

    let fetch_folder = move || async move {
        match auth.call(get_user_folder()).await {
            Ok(fetched) => {
                if let Some(ref f) = fetched {
                    folder_name.set(f.name.clone());
                    folder_path.set(f.path.clone());
                }
                folder.set(fetched);
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        folder_loading.set(false);
    };

    use_future(move || async move {
        fetch_folder().await;
    });

    let handle_save_folder = move |_| async move {
        error.set(String::new());
        success_msg.set(String::new());

        let name = folder_name().trim().to_string();
        let path = folder_path().trim().to_string();
        if name.is_empty() || path.is_empty() {
            error.set("Folder name and path are required.".to_string());
            return;
        }

        saving_folder.set(true);
        let existing_id = folder.peek().as_ref().map(|f| f.id.clone());
        let result = if let Some(id) = existing_id {
            auth.call(update_folder(id, name, path)).await
        } else {
            auth.call(create_user_folder(name, path))
                .await
                .map(|_| ())
        };

        match result {
            Ok(()) => {
                success_msg.set("Music library folder saved.".to_string());
                editing.set(false);
                fetch_folder().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
        saving_folder.set(false);
    };

    let current_folder = folder.read().clone();
    let show_form = editing() || (!folder_loading() && current_folder.is_none());

    rsx! {
        div { class: "space-y-6",
        // My Music Library (Single Folder per User)
        Panel { title: "My Music Library",
            Messages { error: error(), success: success_msg() }

            if folder_loading() {
                Loading {}
            } else if show_form {
                div { class: "space-y-4",
                    p { class: "text-xs text-beet-text-secondary leading-relaxed",
                        "Each user has one personal music library folder. Downloads, Discovery tracks, and music shared with you are organized here."
                    }
                    div { class: "grid grid-cols-1 md:grid-cols-2 gap-4",
                        div {
                            label { class: LABEL_CLASS, "Library Name" }
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
                                placeholder: "/music/alice",
                                class: "tabular-nums-mono",
                            }
                        }
                    }
                    div { class: "flex items-center gap-2.5",
                        button {
                            r#type: "button",
                            disabled: saving_folder(),
                            class: PRIMARY_BUTTON_CLASS,
                            onclick: handle_save_folder,
                            if saving_folder() {
                                "Saving..."
                            } else if current_folder.is_some() {
                                "Save Changes"
                            } else {
                                "Set Library Folder"
                            }
                        }
                        if let Some(existing) = current_folder.clone() {
                            button {
                                r#type: "button",
                                disabled: saving_folder(),
                                class: SECONDARY_BUTTON_CLASS,
                                onclick: move |_| {
                                    folder_name.set(existing.name.clone());
                                    folder_path.set(existing.path.clone());
                                    error.set(String::new());
                                    editing.set(false);
                                },
                                "Cancel"
                            }
                        }
                    }
                }
            } else if let Some(existing) = current_folder.clone() {
                div { class: "p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle flex flex-wrap justify-between items-center gap-4",
                    div { class: "min-w-0 space-y-1",
                        span { class: "font-semibold text-beet-text-primary block font-display text-sm",
                            "{existing.name}"
                        }
                        span { class: "text-beet-text-tertiary text-xs tabular-nums-mono break-all block",
                            "{existing.path}"
                        }
                        p { class: "text-[11px] text-beet-text-tertiary pt-0.5",
                            "Used automatically for downloads, Discovery playlists, and shared tracks."
                        }
                    }
                    button {
                        r#type: "button",
                        class: SECONDARY_BUTTON_CLASS,
                        onclick: move |_| {
                            folder_name.set(existing.name.clone());
                            folder_path.set(existing.path.clone());
                            error.set(String::new());
                            success_msg.set(String::new());
                            editing.set(true);
                        },
                        "Edit Folder"
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
                    // Track Sharing
                    div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                        div {
                            p { class: "text-sm text-beet-text-primary font-medium",
                                "Allow other users to share music to my library"
                            }
                            p { class: "text-xs text-beet-text-tertiary mt-0.5 leading-relaxed",
                                "Let other household members share tracks and albums directly into your personal library via zero-copy hardlinks."
                            }
                        }
                        Toggle {
                            on: accept_shares(),
                            on_class: "bg-beet-leaf-solid",
                            onclick: move |_| async move {
                                let next = !accept_shares();
                                accept_shares.set(next);
                                let change = api::UpdateUserSettings {
                                    accept_shares: Some(next),
                                    ..Default::default()
                                };
                                saver.save(Ok(change), show_accept_shares).await;
                            },
                        }
                    }

                    // Discovery (user-level)
                    div {
                        h3 { class: "text-sm font-semibold text-beet-text-primary mb-1.5", "Discovery" }
                        p { class: "text-xs text-beet-text-secondary leading-relaxed mb-4",
                            "Automatically download new tracks based on what you play and star in Navidrome, plus your ListenBrainz listens when a username is saved."
                        }
                        div { class: "space-y-3",
                            // Enable/disable toggle
                            div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                                div {
                                    p { class: "text-sm text-beet-text-primary font-medium", "Enable Discovery" }
                                    p { class: "text-xs text-beet-text-tertiary mt-0.5",
                                        "Periodically generate playlists of new tracks inside your music library folder"
                                    }
                                }
                                Toggle {
                                    on: discovery_enabled(),
                                    on_class: "bg-beet-leaf-solid",
                                    onclick: move |_| save_switch(discovery_enabled, library_error, move |on| {
                                        auth.call(api::update_user_settings(api::UpdateUserSettings {
                                            discovery_enabled: Some(on),
                                            ..Default::default()
                                        }))
                                    }),
                                }
                            }

                            if discovery_enabled() {
                                if current_folder.is_none() && !folder_loading() {
                                    div { class: "p-3.5 rounded-[10px] bg-beet-gold-surface border border-beet-gold/30 text-xs text-beet-gold",
                                        "Set your music library folder above before running Discovery."
                                    }
                                }

                                // Playlists: toggle + name + track count + lifetime per profile
                                div { class: "p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                                    label { class: "block text-xs font-medium text-beet-text-secondary mb-2.5", "Playlists" }
                                    div { class: "space-y-2.5",
                                        for (label, value, color, active_pill, name_signal, tc_signal, lt_signal) in [
                                            ("Safe", "Conservative", "border-beet-gold/30 bg-beet-gold-surface", "bg-beet-gold/20 text-beet-gold border border-beet-gold/30", &pl_name_safe, &tc_safe, &lt_safe),
                                            ("Mix", "Balanced", "border-beet-leaf-solid/30 bg-beet-leaf-surface", "bg-beet-leaf-solid/20 text-beet-leaf-text border border-beet-leaf-solid/30", &pl_name_mix, &tc_mix, &lt_mix),
                                            ("Wild", "Adventurous", "border-beet-accent-solid/40 bg-beet-accent-surface", "bg-beet-accent-solid/25 text-beet-accent-text border border-beet-accent-solid/40", &pl_name_wild, &tc_wild, &lt_wild),
                                        ] {
                                            {
                                                let active = discovery_profiles().split(',').any(|p| p.trim() == value);
                                                rsx! {
                                                    div {
                                                        class: format!(
                                                            "p-2.5 rounded-[10px] border transition-colors {}",
                                                            if active { color } else { "border-beet-border-subtle bg-beet-surface-low/60 opacity-55" }
                                                        ),
                                                        div { class: "flex items-center gap-2",
                                                            // Toggle button
                                                            button {
                                                                class: format!(
                                                                    "shrink-0 w-16 py-1 text-xs tabular-nums-mono font-semibold rounded-[8px] cursor-pointer transition-colors text-center {}",
                                                                    if active { active_pill } else { "bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary" }
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
                                                                class: "flex-1 h-8 px-2.5 rounded-[8px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary text-xs transition-colors",
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
                                                            div { class: "flex gap-3 mt-2 ml-18",
                                                                div { class: "flex items-center gap-1.5",
                                                                    span { class: "text-[11px] text-beet-text-tertiary", "Tracks" }
                                                                    input {
                                                                        class: "w-14 h-7 px-1.5 rounded-[8px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary tabular-nums-mono text-xs text-center",
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
                                                                div { class: "flex items-center gap-1.5",
                                                                    span { class: "text-[11px] text-beet-text-tertiary", "Days" }
                                                                    input {
                                                                        class: "w-14 h-7 px-1.5 rounded-[8px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary tabular-nums-mono text-xs text-center",
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
                    div { class: "p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle space-y-4",
                        div {
                            p { class: "text-sm text-beet-text-primary font-medium mb-1.5", "ListenBrainz Username" }
                            input {
                                class: "w-full h-10 px-3.5 rounded-[10px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary text-sm placeholder:text-beet-text-tertiary transition-colors",
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
                            p { class: "text-xs text-beet-text-tertiary mt-1.5 leading-relaxed",
                                "Your ListenBrainz history joins your Navidrome plays for recommendations."
                            }
                        }
                        div {
                            p { class: "text-sm text-beet-text-primary font-medium mb-1", "ListenBrainz Token (optional)" }
                            p { class: "text-xs text-beet-text-tertiary mb-2 leading-relaxed",
                                "Not needed. With it, artist top tracks come from ListenBrainz's own ranking and ListenBrainz allows more requests."
                            }
                            input {
                                class: "w-full h-10 px-3.5 rounded-[10px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary tabular-nums-mono text-sm placeholder:text-beet-text-tertiary transition-colors",
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
                    div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                        div {
                            p { class: "text-sm text-beet-text-primary font-medium", "Also use Deezer for recommendations" }
                            p { class: "text-xs text-beet-text-tertiary mt-0.5 leading-relaxed",
                                "Sends the names of artists you play to Deezer. Deezer has not approved this use."
                            }
                        }
                        Toggle {
                            on: deezer_recommendations(),
                            on_class: "bg-beet-leaf-solid",
                            onclick: move |_| save_switch(deezer_recommendations, library_error, move |on| {
                                auth.call(api::update_user_settings(api::UpdateUserSettings {
                                    deezer_recommendations: Some(on),
                                    ..Default::default()
                                }))
                            }),
                        }
                    }

                    if let Some(Some(names)) = lossy_backends.read().as_ref() {
                        div { class: "p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                            p { class: "text-sm text-beet-text-primary font-medium", "When only {names} has it" }
                            p { class: "text-xs text-beet-text-tertiary mt-0.5 mb-2.5 leading-relaxed",
                                "Its files are lossy. Auto-download looks there only when no other source has a match it would take, and never replaces a copy already in your library."
                            }
                            select {
                                class: "w-full h-10 px-3 rounded-[10px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary text-sm transition-colors",
                                name: "lossy_fallback",
                                onchange: move |e| {
                                    let change = LossyFallback::parse(&e.value())
                                        .map(|choice| api::UpdateUserSettings {
                                            lossy_fallback: Some(choice),
                                            ..Default::default()
                                        })
                                        .ok_or_else(|| "Choose one of the three".to_string());
                                    saver.save(change, show_lossy)
                                },
                                for choice in LossyFallback::ALL {
                                    option {
                                        value: choice.as_str(),
                                        selected: lossy_fallback() == choice,
                                        {lossy_label(choice)}
                                    }
                                }
                            }
                        }
                    }

                    // External Subsonic Client Compatibility
                    div { class: "pt-2 space-y-3",
                        div {
                            h3 { class: "text-sm font-semibold text-beet-text-primary mb-1",
                                "External Subsonic Client Compatibility"
                            }
                            p { class: "text-xs text-beet-text-secondary leading-relaxed",
                                "Soulbeet uses Keep and Drop directly and keeps stars as pure 1 to 5 ratings. Enable compatibility sync if you curate Discovery tracks using star ratings in third-party Subsonic clients such as Symfonium or Navidrome Web."
                            }
                        }

                        // External star rating sync toggle
                        div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                            div {
                                p { class: "text-sm text-beet-text-primary font-medium",
                                    "Sync star ratings from external clients for Discovery"
                                }
                                p { class: "text-xs text-beet-text-tertiary mt-0.5 leading-relaxed",
                                    "Treat 1-star ratings as Drop and threshold-or-higher ratings as Keep when syncing ratings made outside Soulbeet"
                                }
                            }
                            Toggle {
                                on: external_rating_sync_enabled(),
                                on_class: "bg-beet-leaf-solid",
                                onclick: move |_| save_switch(external_rating_sync_enabled, library_error, move |on| {
                                    auth.call(api::update_user_settings(api::UpdateUserSettings {
                                        external_rating_sync_enabled: Some(on),
                                        ..Default::default()
                                    }))
                                }),
                            }
                        }

                        // Discovery promote threshold (gated under external compat toggle)
                        if external_rating_sync_enabled() {
                            div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                                div {
                                    p { class: "text-sm text-beet-text-primary font-medium",
                                        "External client promote threshold"
                                    }
                                    p { class: "text-xs text-beet-text-tertiary mt-0.5 leading-relaxed",
                                        "Promote pending discovery tracks during sync when rated at or above this in an external Subsonic client"
                                    }
                                }
                                input {
                                    class: "w-16 h-9 px-2 rounded-[10px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-beet-text-primary tabular-nums-mono text-sm text-center",
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

                        // Auto-delete 1-star toggle
                        div { class: "flex items-center justify-between gap-4 p-4 bg-beet-surface rounded-[12px] border border-beet-border-subtle",
                            div {
                                p { class: "text-sm text-beet-text-primary font-medium", "Auto-delete 1-star tracks" }
                                p { class: "text-xs text-beet-text-tertiary mt-0.5 leading-relaxed",
                                    "Automatically delete files rated 1 star in Navidrome or external Subsonic clients during sync"
                                }
                            }
                            Toggle {
                                on: auto_delete_enabled(),
                                on_class: "bg-beet-crimson-solid",
                                onclick: move |_| save_switch(auto_delete_enabled, library_error, move |on| {
                                    auth.call(api::update_user_settings(api::UpdateUserSettings {
                                        auto_delete_enabled: Some(on),
                                        ..Default::default()
                                    }))
                                }),
                            }
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
    fn the_lossy_choice_names_only_backends_whose_files_are_lossy() {
        assert_eq!(lossy_names(["slskd", "archive"].into_iter()), None);
        assert_eq!(
            lossy_names(["slskd", "ytdlp"].into_iter()).as_deref(),
            Some("YouTube Music")
        );
        assert_eq!(
            LossyFallback::ALL.map(lossy_label),
            ["Never use it", "Ask me", "Download automatically (lossy)"]
        );
    }

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
