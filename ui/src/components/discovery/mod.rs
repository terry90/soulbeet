use dioxus::prelude::*;
use shared::navidrome::{DiscoveryProgress, DiscoveryStatus, GenerationStatus, ProfilePhase};
use shared::system::NavidromeStatus;

use crate::friendly_error;
use crate::use_auth;
use crate::ConfirmModal;

fn format_relative_time(ts: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
                .map(|naive| naive.and_utc().fixed_offset())
        })
    else {
        return ts.to_string();
    };
    let delta = chrono::Utc::now().signed_duration_since(parsed);
    let mins = delta.num_minutes();
    if mins < 1 { return "just now".to_string(); }
    if mins < 60 { return format!("{}m ago", mins); }
    let hours = delta.num_hours();
    if hours < 24 { return format!("{}h ago", hours); }
    let days = delta.num_days();
    if days < 30 { return format!("{}d ago", days); }
    format!("{}d ago", days)
}

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

    // Poll tick drives progress refetch
    let mut poll_tick = use_signal(|| 0u32);

    let progress = use_resource(move || {
        let _ = poll_tick();
        async move { api::get_discovery_progress().await.ok().flatten() }
    });

    // Derive generating state from server progress (survives page reload per DISC-03)
    let is_generating = progress
        .read()
        .as_ref()
        .and_then(|p| p.as_ref())
        .map(|p| p.status == GenerationStatus::Running)
        .unwrap_or(false);

    // Poll every 2.5s while generation is active
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(2500).await;
            let is_active = progress
                .read()
                .as_ref()
                .and_then(|p| p.as_ref())
                .map(|p| p.status == GenerationStatus::Running)
                .unwrap_or(false);
            if is_active {
                poll_tick += 1;
            }
        }
    });

    // Refresh track list when generation completes
    use_effect(move || {
        let status = progress
            .read()
            .as_ref()
            .and_then(|p| p.as_ref())
            .map(|p| p.status.clone());
        if matches!(status, Some(GenerationStatus::Complete)) {
            config.restart();
        }
    });

    let mut start_error = use_signal(String::new);

    let handle_generate = move |_| async move {
        start_error.set(String::new());
        match api::start_discovery_generation().await {
            Ok(()) => {
                poll_tick += 1;
            }
            Err(e) => {
                start_error.set(friendly_error(&e));
            }
        }
    };

    let current_progress = progress.read().as_ref().and_then(|p| p.clone()).clone();

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
                                        div { class: "mt-1 space-y-0.5",
                                            for (profile, name) in &cfg.playlist_names {
                                                {
                                                    let tc = cfg.track_counts.get(profile).copied().unwrap_or(10);
                                                    let lt = cfg.lifetime_days.get(profile).copied().unwrap_or(7);
                                                    rsx! {
                                                        p { class: "text-gray-400 text-xs font-mono",
                                                            span { class: "text-green-400", "{name}" }
                                                            span { class: "text-gray-600 ml-1", "({tc} tracks / {lt}d)" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if let Some(ref ts) = cfg.last_generated_at {
                                            {
                                                let ago = format_relative_time(ts);
                                                rsx! {
                                                    p { class: "text-gray-500 text-xs font-mono mt-1",
                                                        "Last generated: {ago}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    div { class: "flex flex-col items-end gap-2",
                                        button {
                                            class: "retro-btn rounded text-xs",
                                            disabled: is_generating,
                                            onclick: handle_generate,
                                            if is_generating { "Generating..." } else { "Generate" }
                                        }
                                        if !start_error().is_empty() {
                                            span { class: "text-xs font-mono text-red-400", "{start_error}" }
                                        }
                                    }
                                }
                                if let Some(ref p) = current_progress {
                                    if p.status != GenerationStatus::Idle {
                                        ProgressPanel { progress: p.clone() }
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
fn ProgressPanel(progress: DiscoveryProgress) -> Element {
    match progress.status {
        GenerationStatus::Running => {
            rsx! {
                div { class: "mt-4 bg-beet-dark/50 border border-white/10 rounded-lg p-4",
                    p { class: "text-xs font-mono font-semibold text-white mb-3 uppercase tracking-wider",
                        "Generating Discovery"
                    }
                    div { class: "space-y-2",
                        for pp in &progress.profiles {
                            { render_profile_row(pp) }
                        }
                    }
                }
            }
        }
        GenerationStatus::Complete => {
            let total = progress.result.as_ref().map(|r| r.total_imported).unwrap_or(0);
            let detail = progress
                .result
                .as_ref()
                .map(|r| {
                    r.profiles
                        .iter()
                        .map(|ps| format!("{}: {}", ps.profile, ps.imports_succeeded))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            rsx! {
                div { class: "mt-4 bg-beet-dark/50 border border-white/10 rounded-lg p-4",
                    p { class: "text-xs font-mono text-beet-leaf", "{total} tracks imported" }
                    if !detail.is_empty() {
                        p { class: "text-xs font-mono text-gray-400 mt-1", "{detail}" }
                    }
                }
            }
        }
        GenerationStatus::Error => {
            let msg = progress.error.as_deref().unwrap_or("Unknown error");
            rsx! {
                div { class: "mt-4 bg-beet-dark/50 border border-white/10 rounded-lg p-4",
                    p { class: "text-xs font-mono text-red-400", "{msg}" }
                }
            }
        }
        GenerationStatus::Idle => rsx! {},
    }
}

fn render_profile_row(pp: &shared::navidrome::ProfileProgress) -> Element {
    let badge_class = profile_badge_class(&pp.profile);
    let phase_color = match pp.phase {
        ProfilePhase::Waiting => "text-gray-500",
        ProfilePhase::Done => "text-beet-leaf",
        ProfilePhase::Skipped => "text-gray-600",
        _ => "text-white",
    };
    let phase_text = pp.phase.to_string();
    let show_bar = !matches!(
        pp.phase,
        ProfilePhase::Waiting | ProfilePhase::Done | ProfilePhase::Skipped
    ) && pp.total > 0;
    let percent = if pp.total > 0 {
        (pp.current as f64 / pp.total as f64 * 100.0) as u32
    } else {
        0
    };

    rsx! {
        div { class: "flex items-center gap-3",
            span {
                class: "text-xs font-mono px-1.5 py-0.5 rounded border {badge_class} w-24 text-center shrink-0",
                "{pp.profile}"
            }
            span { class: "text-xs font-mono {phase_color} flex-1", "{phase_text}" }
            if show_bar {
                div { class: "h-1.5 w-24 bg-gray-800 rounded-full overflow-hidden relative shrink-0",
                    div {
                        class: "h-full bg-beet-accent absolute top-0 left-0 transition-all duration-300",
                        style: "width: {percent}%",
                    }
                }
                span { class: "text-xs font-mono text-gray-400 w-12 text-right shrink-0",
                    "{pp.current}/{pp.total}"
                }
            }
            if pp.phase == ProfilePhase::Done {
                span { class: "text-xs font-mono text-beet-leaf shrink-0", "100%" }
            }
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

#[derive(Clone)]
struct PendingDrop {
    track_id: String,
    title: String,
    artist: String,
}

#[component]
fn DiscoveryTrackList() -> Element {
    let mut tracks = use_resource(|| async { api::get_discovery_tracks().await });
    let mut show_history = use_signal(|| false);
    let mut pending_drop: Signal<Option<PendingDrop>> = use_signal(|| None);

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

    // Split into active (pending) and history (promoted/removed)
    let mut active: std::collections::BTreeMap<String, Vec<shared::navidrome::DiscoveryTrack>> =
        std::collections::BTreeMap::new();
    let mut history: Vec<shared::navidrome::DiscoveryTrack> = Vec::new();

    for track in items {
        match track.status {
            DiscoveryStatus::Pending => {
                active.entry(track.profile.clone()).or_default().push(track);
            }
            _ => history.push(track),
        }
    }

    if active.is_empty() && history.is_empty() {
        return rsx! {
            p { class: "text-gray-500 font-mono text-sm", "No discovery tracks" }
        };
    }

    rsx! {
        div { class: "space-y-4",
            // Active tracks per profile
            for (profile, profile_tracks) in &active {
                {
                    let badge_class = profile_badge_class(profile);
                    let count = profile_tracks.len();
                    rsx! {
                        div { class: "space-y-1.5",
                            div { class: "flex items-center gap-2",
                                span {
                                    class: "text-xs font-mono px-1.5 py-0.5 rounded border {badge_class}",
                                    "{profile}"
                                }
                                span { class: "text-xs font-mono text-gray-500",
                                    "{count} tracks"
                                }
                            }
                            for track in profile_tracks {
                                {
                                    let id_promote = track.id.clone();
                                    let id_remove = track.id.clone();
                                    let title_remove = track.title.clone();
                                    let artist_remove = track.artist.clone();
                                    rsx! {
                                        div { class: "flex items-center justify-between p-2 bg-beet-panel border border-white/10 rounded text-sm",
                                            div { class: "flex-1 min-w-0",
                                                span { class: "text-white truncate", "{track.title}" }
                                                span { class: "text-gray-400 mx-2", "-" }
                                                span { class: "text-gray-400 truncate", "{track.artist}" }
                                                if let Some(r) = track.rating {
                                                    span { class: "text-yellow-500 text-xs font-mono ml-2",
                                                        "({r})"
                                                    }
                                                }
                                            }
                                            div { class: "flex gap-1 ml-2",
                                                button {
                                                    class: "px-2 py-0.5 text-xs rounded bg-green-900/50 text-green-400 hover:bg-green-800/50 cursor-pointer",
                                                    onclick: move |_| {
                                                        let id = id_promote.clone();
                                                        async move { handle_promote(id).await }
                                                    },
                                                    "Keep"
                                                }
                                                button {
                                                    class: "px-2 py-0.5 text-xs rounded bg-red-900/50 text-red-400 hover:bg-red-800/50 cursor-pointer",
                                                    onclick: move |_| {
                                                        pending_drop.set(Some(PendingDrop {
                                                            track_id: id_remove.clone(),
                                                            title: title_remove.clone(),
                                                            artist: artist_remove.clone(),
                                                        }));
                                                    },
                                                    "Drop"
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

            // Collapsed history for promoted/removed
            if !history.is_empty() {
                div { class: "mt-2",
                    button {
                        class: "text-xs font-mono text-gray-500 hover:text-gray-300 cursor-pointer underline decoration-dotted",
                        onclick: move |_| show_history.set(!show_history()),
                        if show_history() {
                            "Hide history ({history.len()})"
                        } else {
                            "Show history ({history.len()})"
                        }
                    }
                    if show_history() {
                        div { class: "mt-2 space-y-1",
                            for track in &history {
                                {
                                    let (icon, color) = match track.status {
                                        DiscoveryStatus::Promoted => ("\u{2713}", "text-green-600"),
                                        DiscoveryStatus::Removed => ("\u{2717}", "text-red-600"),
                                        _ => ("", "text-gray-600"),
                                    };
                                    rsx! {
                                        div { class: "flex items-center gap-2 px-2 py-1 text-xs font-mono text-gray-600",
                                            span { class: "{color}", "{icon}" }
                                            span { "{track.artist} - {track.title}" }
                                            span { class: "text-gray-700 ml-auto",
                                                "{track.profile}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Confirmation modal for drop action
            if let Some(pending) = pending_drop() {
                ConfirmModal {
                    message: format!("Drop '{}' by {}? This removes the file from disk.", pending.title, pending.artist),
                    confirm_label: "Drop",
                    danger: true,
                    on_confirm: move |_| {
                        let id = pending.track_id.clone();
                        pending_drop.set(None);
                        spawn(async move { handle_remove(id).await });
                    },
                    on_cancel: move |_| pending_drop.set(None),
                }
            }
        }
    }
}
