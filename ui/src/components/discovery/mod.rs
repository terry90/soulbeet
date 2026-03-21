use dioxus::prelude::*;
use shared::navidrome::DiscoveryStatus;
use shared::system::NavidromeStatus;

use crate::use_auth;

fn profile_badge_class(profile: &str) -> &'static str {
    match profile {
        "Conservative" => "bg-blue-600/30 text-blue-300 border-blue-500/40",
        "Balanced" => "bg-green-600/30 text-green-300 border-green-500/40",
        "Adventurous" => "bg-purple-600/30 text-purple-300 border-purple-500/40",
        _ => "bg-gray-600/30 text-gray-300 border-gray-500/40",
    }
}

#[component]
pub fn DiscoveryOverview() -> Element {
    let mut config = use_resource(|| async { api::get_discovery_config().await });

    let info = match &*config.read() {
        Some(Ok(c)) => Some(c.clone()),
        _ => None,
    };

    let mut generating = use_signal(|| false);
    let mut error = use_signal(String::new);
    let mut success = use_signal(String::new);

    let handle_generate = move |_| async move {
        generating.set(true);
        error.set(String::new());
        success.set(String::new());
        match api::generate_discovery_playlist().await {
            Ok(count) => {
                success.set(format!("{count} tracks downloaded"));
                config.restart();
            }
            Err(e) => error.set(format!("{e}")),
        }
        generating.set(false);
    };

    let auth = use_auth();
    let nav_status = auth.navidrome_status();

    rsx! {
        div { class: "space-y-4",
            h3 { class: "text-sm font-semibold text-white", "Discovery" }

            if matches!(nav_status, NavidromeStatus::InvalidCredentials | NavidromeStatus::Unknown) {
                p { class: "text-xs font-mono text-beet-accent",
                    "Your username is not linked to Navidrome. Discovery playlists and rating sync require a matching Navidrome account."
                }
            }

            match info {
                None => rsx! {
                    p { class: "text-gray-500 font-mono text-sm", "Loading..." }
                },
                Some(cfg) => {
                    if !cfg.enabled {
                        rsx! {
                            p { class: "text-gray-500 font-mono text-sm",
                                "Discovery is not enabled. Turn it on in Settings > Library."
                            }
                        }
                    } else {
                        rsx! {
                            div { class: "bg-beet-panel border border-white/10 rounded-lg p-4",
                                div { class: "flex justify-between items-start",
                                    div {
                                        if let Some(ref name) = cfg.folder_name {
                                            p { class: "text-white font-medium", "Folder: {name}" }
                                        }
                                        p { class: "text-gray-400 text-xs font-mono",
                                            "Target: {cfg.track_count} tracks / {cfg.lifetime_days}d lifetime"
                                        }
                                        div { class: "mt-1 space-y-0.5",
                                            for (profile, name) in &cfg.playlist_names {
                                                {
                                                    let active = cfg.navidrome_playlist_ids.contains_key(profile);
                                                    rsx! {
                                                        p { class: "text-gray-400 text-xs font-mono",
                                                            span { class: if active { "text-green-400" } else { "text-gray-500" },
                                                                "{name}"
                                                            }
                                                            if active {
                                                                span { class: "text-green-600 ml-1", " (active)" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if let Some(ref ts) = cfg.last_generated_at {
                                            p { class: "text-gray-500 text-xs font-mono mt-1",
                                                "Last generated: {ts}"
                                            }
                                        }
                                    }
                                    div { class: "flex flex-col items-end gap-2",
                                        button {
                                            class: "retro-btn rounded text-xs",
                                            disabled: generating(),
                                            onclick: handle_generate,
                                            if generating() { "Generating..." } else { "Generate" }
                                        }
                                        if !error().is_empty() {
                                            span { class: "text-xs font-mono text-red-400", "{error}" }
                                        }
                                        if !success().is_empty() {
                                            span { class: "text-xs font-mono text-green-400", "{success}" }
                                        }
                                    }
                                }
                            }

                            DiscoveryTrackList {}
                        }
                    }
                }
            }

            EngineReportView {}
        }
    }
}

#[component]
fn EngineReportView() -> Element {
    let mut show = use_signal(|| false);
    let mut entries: Signal<Vec<api::ReportEntry>> = use_signal(Vec::new);
    let mut loading = use_signal(|| false);
    let mut expanded_idx: Signal<Option<usize>> = use_signal(|| None);

    let handle_toggle = move |_| async move {
        if show() {
            show.set(false);
            return;
        }
        loading.set(true);
        match api::get_engine_reports().await {
            Ok(list) => entries.set(list),
            Err(e) => entries.set(vec![api::ReportEntry {
                profile: "Error".to_string(),
                candidate_count: 0,
                created_at: String::new(),
                text: format!("Failed to load reports: {e}"),
            }]),
        }
        loading.set(false);
        show.set(true);
    };

    rsx! {
        div { class: "mt-4",
            button {
                class: "text-xs font-mono text-gray-500 hover:text-gray-300 cursor-pointer underline decoration-dotted",
                onclick: handle_toggle,
                if loading() { "Loading..." }
                else if show() { "Hide engine reports" }
                else { "Show engine reports" }
            }
            if show() {
                div { class: "mt-2 max-h-96 overflow-y-auto border border-white/10 rounded bg-beet-dark",
                    if entries().is_empty() {
                        p { class: "p-3 text-xs font-mono text-gray-500",
                            "No engine reports available. Run discovery generation first."
                        }
                    } else {
                        for (idx, entry) in entries().iter().enumerate() {
                            {
                                let is_expanded = expanded_idx() == Some(idx);
                                let badge_class = profile_badge_class(&entry.profile);
                                let badge_label = entry.profile.clone();
                                let date = entry.created_at.clone();
                                let count = entry.candidate_count;
                                let text = entry.text.clone();
                                rsx! {
                                    div { class: "border-b border-white/5 last:border-b-0",
                                        button {
                                            class: "w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-white/5 cursor-pointer",
                                            onclick: move |_| {
                                                if expanded_idx() == Some(idx) {
                                                    expanded_idx.set(None);
                                                } else {
                                                    expanded_idx.set(Some(idx));
                                                }
                                            },
                                            span {
                                                class: "text-xs font-mono px-1.5 py-0.5 rounded border {badge_class}",
                                                "{badge_label}"
                                            }
                                            span { class: "text-xs font-mono text-gray-500 flex-1", "{date}" }
                                            span { class: "text-xs font-mono text-gray-400", "{count} candidates" }
                                            span { class: "text-xs text-gray-600 ml-1",
                                                if is_expanded { "\u{25BC}" } else { "\u{25B6}" }
                                            }
                                        }
                                        if is_expanded {
                                            pre { class: "px-3 pb-3 text-xs font-mono text-gray-400 whitespace-pre-wrap overflow-x-auto",
                                                "{text}"
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

#[component]
fn DiscoveryTrackList() -> Element {
    let mut tracks = use_resource(|| async { api::get_discovery_tracks().await });

    let items = match &*tracks.read() {
        Some(Ok(items)) => items.clone(),
        _ => vec![],
    };

    let handle_promote = move |track_id: String| async move {
        let req = api::TrackActionRequest { track_id };
        let _ = api::promote_discovery_track(req).await;
        tracks.restart();
    };

    let handle_remove = move |track_id: String| async move {
        let req = api::TrackActionRequest { track_id };
        let _ = api::remove_discovery_track(req).await;
        tracks.restart();
    };

    if items.is_empty() {
        return rsx! {
            p { class: "text-gray-500 font-mono text-sm", "No discovery tracks" }
        };
    }

    // Group tracks by profile
    let mut grouped: std::collections::BTreeMap<String, Vec<shared::navidrome::DiscoveryTrack>> =
        std::collections::BTreeMap::new();
    for track in items {
        grouped
            .entry(track.profile.clone())
            .or_default()
            .push(track);
    }

    rsx! {
        div { class: "space-y-4",
            for (profile, profile_tracks) in grouped {
                {
                    let badge_class = profile_badge_class(&profile);
                    rsx! {
                        div { class: "space-y-2",
                            div { class: "flex items-center gap-2",
                                span {
                                    class: "text-xs font-mono px-1.5 py-0.5 rounded border {badge_class}",
                                    "{profile}"
                                }
                                span { class: "text-xs font-mono text-gray-500",
                                    "{profile_tracks.len()} tracks"
                                }
                            }
                            for track in profile_tracks {
                                {
                                    let id_promote = track.id.clone();
                                    let id_remove = track.id.clone();
                                    let status_class = match track.status {
                                        DiscoveryStatus::Pending => "text-blue-400",
                                        DiscoveryStatus::Promoted => "text-green-400",
                                        DiscoveryStatus::Removed => "text-red-400",
                                    };
                                    rsx! {
                                        div { class: "flex items-center justify-between p-2 bg-beet-panel border border-white/10 rounded text-sm",
                                            div { class: "flex-1 min-w-0",
                                                span { class: "text-white truncate", "{track.title}" }
                                                span { class: "text-gray-400 mx-2", "-" }
                                                span { class: "text-gray-400 truncate", "{track.artist}" }
                                                span { class: "{status_class} text-xs font-mono ml-2",
                                                    "{track.status}"
                                                }
                                                if let Some(r) = track.rating {
                                                    span { class: "text-yellow-500 text-xs font-mono ml-2",
                                                        "({r})"
                                                    }
                                                }
                                            }
                                            if track.status == DiscoveryStatus::Pending {
                                                div { class: "flex gap-1 ml-2",
                                                    button {
                                                        class: "px-2 py-0.5 text-xs rounded bg-green-900/50 text-green-400 hover:bg-green-800/50 cursor-pointer",
                                                        onclick: move |_| {
                                                            let id = id_promote.clone();
                                                            async move { handle_promote(id).await }
                                                        },
                                                        "Promote"
                                                    }
                                                    button {
                                                        class: "px-2 py-0.5 text-xs rounded bg-red-900/50 text-red-400 hover:bg-red-800/50 cursor-pointer",
                                                        onclick: move |_| {
                                                            let id = id_remove.clone();
                                                            async move { handle_remove(id).await }
                                                        },
                                                        "Remove"
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
}
