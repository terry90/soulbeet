use dioxus::prelude::*;
use shared::navidrome::{
    DiscoveryProgress, DiscoveryStatus, GenerationStatus, LastRun, ProfilePhase,
};
use shared::recommendation::{EngineState, NO_LISTENING_DATA};
use shared::system::NavidromeStatus;

use crate::friendly_error;
use crate::use_auth;
use crate::{
    use_open_handler, ConfirmModal, Disclosure, IconCheck, IconSparkles, IconStar, IconX,
};

fn format_relative_time(ts: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
            .map(|naive| naive.and_utc().fixed_offset())
    }) else {
        return ts.to_string();
    };
    let delta = chrono::Utc::now().signed_duration_since(parsed);
    let mins = delta.num_minutes();
    if mins < 1 {
        return "just now".to_string();
    }
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = delta.num_days();
    format!("{}d ago", days)
}

fn profile_badge_class(profile: &str) -> &'static str {
    match profile {
        "Conservative" => "bg-beet-leaf-surface text-beet-leaf-text border-beet-leaf/30",
        "Balanced" => "bg-beet-gold-surface text-beet-gold border-beet-gold/30",
        "Adventurous" => "bg-beet-accent-surface text-beet-accent-text border-beet-accent/30",
        _ => "bg-beet-surface-high text-beet-text-secondary border-beet-border-default",
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
            div { class: "flex items-center gap-2",
                IconSparkles { size: 16, class: "text-beet-accent-text".to_string() }
                h3 { class: "text-[16px] font-semibold text-beet-text-primary", "Discovery" }
            }

            if nav_status == NavidromeStatus::NotLinked {
                div { class: "rounded-[10px] bg-beet-accent-surface border border-beet-accent/30 px-3.5 py-2.5 text-[12px] text-beet-accent-text",
                    "Your account is not connected to Navidrome yet. Use Connect Navidrome in Settings → Users for discovery playlists and rating sync."
                }
            } else if matches!(nav_status, NavidromeStatus::InvalidCredentials | NavidromeStatus::Unknown) {
                div { class: "rounded-[10px] bg-beet-accent-surface border border-beet-accent/30 px-3.5 py-2.5 text-[12px] text-beet-accent-text",
                    "Your username is not linked to Navidrome. Discovery playlists and rating sync require a matching Navidrome account."
                }
            }

            match info {
                None => rsx! {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-[13px] text-beet-text-tertiary",
                        "Loading discovery settings…"
                    }
                },
                Some(cfg) => {
                    if !cfg.enabled {
                        rsx! {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-[13px] text-beet-text-secondary",
                                "Discovery is not enabled. Turn it on in Settings → Library."
                            }
                        }
                    } else {
                        rsx! {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5",
                                div { class: "flex flex-wrap justify-between items-start gap-4",
                                    div { class: "space-y-1.5 min-w-0",
                                        if let Some(ref name) = cfg.folder_name {
                                            p { class: "text-[14px] font-semibold text-beet-text-primary",
                                                "Folder: {name}"
                                            }
                                        }
                                        div { class: "flex flex-wrap gap-2 pt-0.5",
                                            for (profile, name) in &cfg.playlist_names {
                                                {
                                                    let tc = cfg.track_counts.get(profile).copied().unwrap_or(10);
                                                    let lt = cfg.lifetime_days.get(profile).copied().unwrap_or(7);
                                                    rsx! {
                                                        span { class: "inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-beet-surface-low border border-beet-border-subtle text-[11px] tabular-nums-mono",
                                                            span { class: "text-beet-leaf-text font-semibold", "{name}" }
                                                            span { class: "text-beet-text-tertiary", "{tc} tracks · {lt}d" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if let Some(run) = cfg.last_run.clone() {
                                            LastRunLine { run }
                                        }
                                        if let Some(ref ts) = cfg.last_generated_at {
                                            {
                                                let ago = format_relative_time(ts);
                                                rsx! {
                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary pt-0.5",
                                                        "Last generated: {ago}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    div { class: "flex flex-col items-end gap-2 shrink-0",
                                        button {
                                            r#type: "button",
                                            class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-50 disabled:cursor-wait transition cursor-pointer",
                                            disabled: is_generating,
                                            onclick: handle_generate,
                                            if is_generating { "Generating…" } else { "Generate" }
                                        }
                                        if !start_error().is_empty() {
                                            span { class: "text-[12px] text-beet-crimson-text", "{start_error}" }
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
fn LastRunLine(run: LastRun) -> Element {
    let used = run.sources.join(", ");
    rsx! {
        if run.state == EngineState::NoListeningData {
            p { class: "text-[12px] text-beet-accent-text pt-0.5", "{NO_LISTENING_DATA}" }
        } else if !run.sources.is_empty() {
            // A run that failed before recommending used no source.
            p { class: "text-[12px] text-beet-text-secondary pt-0.5", "Last run used: {used}" }
        }
        for e in &run.source_errors {
            p { class: "text-[12px] text-beet-crimson-text", "{e.source} failed: {e.message}" }
        }
    }
}

#[component]
fn ProgressPanel(progress: DiscoveryProgress) -> Element {
    match progress.status {
        GenerationStatus::Running => {
            rsx! {
                div { class: "mt-4 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-4 space-y-3",
                    p { class: "tabular-nums-mono text-[11px] font-semibold text-beet-text-primary uppercase tracking-wider",
                        "Generating Discovery"
                    }
                    div { class: "space-y-2.5",
                        for pp in &progress.profiles {
                            { render_profile_row(pp) }
                        }
                    }
                }
            }
        }
        GenerationStatus::Complete => {
            let total = progress
                .result
                .as_ref()
                .map(|r| r.total_imported)
                .unwrap_or(0);
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
                div { class: "mt-4 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-4",
                    p { class: "tabular-nums-mono text-[12px] font-semibold text-beet-leaf-text",
                        "{total} tracks imported"
                    }
                    if !detail.is_empty() {
                        p { class: "tabular-nums-mono text-[11px] text-beet-text-secondary mt-1",
                            "{detail}"
                        }
                    }
                }
            }
        }
        GenerationStatus::Error => {
            let msg = progress
                .failure
                .as_ref()
                .map(|f| f.message.as_str())
                .unwrap_or("Unknown error");
            rsx! {
                div { class: "mt-4 rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-solid/30 p-4",
                    p { class: "text-[12px] text-beet-crimson-text", "{msg}" }
                }
            }
        }
        GenerationStatus::Idle => rsx! {},
    }
}

fn render_profile_row(pp: &shared::navidrome::ProfileProgress) -> Element {
    let badge_class = profile_badge_class(&pp.profile);
    let phase_color = match pp.phase {
        ProfilePhase::Waiting => "text-beet-text-tertiary",
        ProfilePhase::Done => "text-beet-leaf-text",
        ProfilePhase::Skipped => "text-beet-text-disabled",
        _ => "text-beet-text-primary",
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
                class: "tabular-nums-mono text-[11px] font-semibold px-2 py-0.5 rounded-full border {badge_class} w-28 text-center shrink-0",
                "{pp.profile}"
            }
            span { class: "text-[12px] {phase_color} flex-1 truncate", "{phase_text}" }
            if show_bar {
                div { class: "h-1.5 w-24 bg-beet-surface-high rounded-full overflow-hidden relative shrink-0",
                    div {
                        class: "h-full bg-beet-accent-solid rounded-full absolute top-0 left-0 transition-all duration-300",
                        style: "width: {percent}%",
                    }
                }
                span { class: "tabular-nums-mono text-[11px] text-beet-text-secondary w-12 text-right shrink-0",
                    "{pp.current}/{pp.total}"
                }
            }
            if pp.phase == ProfilePhase::Done {
                span { class: "tabular-nums-mono text-[11px] font-semibold text-beet-leaf-text shrink-0",
                    "100%"
                }
            }
        }
    }
}

#[component]
fn EngineReportView() -> Element {
    let mut entries: Signal<Vec<api::ReportEntry>> = use_signal(Vec::new);
    let mut expanded_idx: Signal<Option<usize>> = use_signal(|| None);

    let load = use_open_handler(move || async move {
        match api::get_engine_reports().await {
            Ok(list) => entries.set(list),
            Err(e) => entries.set(vec![api::ReportEntry {
                profile: "Error".to_string(),
                candidate_count: 0,
                created_at: String::new(),
                text: format!("Failed to load reports: {e}"),
            }]),
        }
        true
    });

    rsx! {
        Disclosure { class: "pt-2", label: "engine reports", on_open: load,
            div { class: "mt-2 max-h-96 overflow-y-auto studio-scrollbar border border-beet-border-subtle rounded-[12px] bg-beet-surface-low",
                if entries().is_empty() {
                    p { class: "p-4 text-[12px] text-beet-text-tertiary",
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
                                div { class: "border-b border-beet-border-subtle last:border-b-0",
                                    button {
                                        r#type: "button",
                                        class: "w-full flex items-center gap-2.5 px-3.5 py-2.5 text-left hover:bg-beet-surface-high/60 transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if expanded_idx() == Some(idx) {
                                                expanded_idx.set(None);
                                            } else {
                                                expanded_idx.set(Some(idx));
                                            }
                                        },
                                        span {
                                            class: "tabular-nums-mono text-[11px] font-semibold px-2 py-0.5 rounded-full border {badge_class}",
                                            "{badge_label}"
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary flex-1 truncate",
                                            "{date}"
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-secondary",
                                            "{count} candidates"
                                        }
                                        span { class: "text-[11px] text-beet-text-tertiary ml-1",
                                            if is_expanded { "\u{25BC}" } else { "\u{25B6}" }
                                        }
                                    }
                                    if is_expanded {
                                        pre { class: "px-3.5 pb-3.5 text-[11px] tabular-nums-mono text-beet-text-secondary whitespace-pre-wrap overflow-x-auto",
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

/// Notes in `errors` how a Keep or Drop of `track_id` ended: a failure shows
/// under the track's row until an action on it succeeds.
fn note_outcome(
    errors: &mut std::collections::HashMap<String, String>,
    track_id: String,
    outcome: Result<(), ServerFnError>,
) {
    match outcome {
        Ok(()) => {
            errors.remove(&track_id);
        }
        Err(e) => {
            errors.insert(track_id, friendly_error(&e));
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
    let mut pending_drop: Signal<Option<PendingDrop>> = use_signal(|| None);

    let items = match &*tracks.read() {
        Some(Ok(items)) => items.clone(),
        _ => vec![],
    };

    // Why the last Keep or Drop of a track failed, by track id.
    let mut action_errors: Signal<std::collections::HashMap<String, String>> =
        use_signal(Default::default);

    let handle_promote = move |track_id: String| async move {
        let req = api::TrackActionRequest {
            track_id: track_id.clone(),
        };
        let outcome = api::promote_discovery_track(req).await;
        note_outcome(&mut action_errors.write(), track_id, outcome);
        tracks.restart();
    };

    // A failed Drop leaves the file in place, so the row stays as it is with
    // its reason under it. A reload could move it to history, where the
    // reason is not shown: the server may leave the track Removing.
    let handle_remove = move |track_id: String| async move {
        let req = api::TrackActionRequest {
            track_id: track_id.clone(),
        };
        let outcome = api::remove_discovery_track(req).await;
        let dropped = outcome.is_ok();
        note_outcome(&mut action_errors.write(), track_id, outcome);
        if dropped {
            tracks.restart();
        }
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
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center text-[13px] text-beet-text-secondary",
                "No discovery tracks staged yet."
            }
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
                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 space-y-2.5",
                            div { class: "flex items-center justify-between gap-2",
                                span {
                                    class: "tabular-nums-mono text-[11px] font-semibold px-2.5 py-0.5 rounded-full border {badge_class}",
                                    "{profile}"
                                }
                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    "{count} TRACKS"
                                }
                            }
                            div { class: "space-y-1.5",
                                for track in profile_tracks {
                                    {
                                        let id_promote = track.id.clone();
                                        let id_remove = track.id.clone();
                                        let title_remove = track.title.clone();
                                        let artist_remove = track.artist.clone();
                                        let action_error = action_errors.read().get(&track.id).cloned();
                                        rsx! {
                                            div { class: "px-3 py-2.5 bg-beet-surface-low border border-beet-border-subtle rounded-[8px] text-[13px]",
                                                div { class: "flex items-center justify-between gap-3",
                                                    div { class: "flex-1 min-w-0 flex items-center gap-2",
                                                        span { class: "font-medium text-beet-text-primary truncate",
                                                            "{track.title}"
                                                        }
                                                        span { class: "text-beet-text-disabled shrink-0", "·" }
                                                        span { class: "text-beet-text-secondary truncate",
                                                            "{track.artist}"
                                                        }
                                                        if let Some(r) = track.rating {
                                                            span { class: "inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-beet-gold-surface text-beet-gold tabular-nums-mono text-[10px] font-semibold shrink-0",
                                                                IconStar { size: 10, filled: true }
                                                                span { "{r}" }
                                                            }
                                                        }
                                                    }
                                                    div { class: "flex items-center gap-1.5 shrink-0",
                                                        button {
                                                            r#type: "button",
                                                            class: "inline-flex items-center gap-1 h-7 px-2.5 text-[12px] font-medium rounded-full bg-beet-leaf-surface border border-beet-leaf/30 text-beet-leaf-text hover:bg-beet-leaf-solid hover:text-beet-leaf-ink transition-colors cursor-pointer",
                                                            onclick: move |_| {
                                                                let id = id_promote.clone();
                                                                async move { handle_promote(id).await }
                                                            },
                                                            IconCheck { size: 12 }
                                                            span { "Keep" }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            class: "inline-flex items-center gap-1 h-7 px-2.5 text-[12px] font-medium rounded-full bg-beet-crimson-surface border border-beet-crimson-solid/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white transition-colors cursor-pointer",
                                                            onclick: move |_| {
                                                                pending_drop.set(Some(PendingDrop {
                                                                    track_id: id_remove.clone(),
                                                                    title: title_remove.clone(),
                                                                    artist: artist_remove.clone(),
                                                                }));
                                                            },
                                                            IconX { size: 12 }
                                                            span { "Drop" }
                                                        }
                                                    }
                                                }
                                                if let Some(error) = action_error {
                                                    p { class: "text-[12px] text-beet-crimson-text mt-1.5 break-words",
                                                        "{error}"
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

            // Collapsed history for promoted/removed
            if !history.is_empty() {
                Disclosure { class: "pt-1", label: format!("history ({})", history.len()),
                    div { class: "mt-2 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-2 space-y-1",
                        for track in &history {
                            {
                                let (icon, color) = match track.status {
                                    DiscoveryStatus::Promoted => ("\u{2713}", "text-beet-leaf-text"),
                                    DiscoveryStatus::Removed => ("\u{2717}", "text-beet-crimson-text"),
                                    _ => ("", "text-beet-text-tertiary"),
                                };
                                rsx! {
                                    div { class: "flex items-center gap-2 px-2.5 py-1.5 rounded-[6px] text-[12px] text-beet-text-secondary",
                                        span { class: "tabular-nums-mono font-semibold {color}", "{icon}" }
                                        span { class: "truncate", "{track.artist} · {track.title}" }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary ml-auto shrink-0",
                                            "{track.profile}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::recommendation::SourceError;

    fn render_run(run: LastRun) -> String {
        let mut dom = VirtualDom::new_with_props(|run: LastRun| rsx! { LastRunLine { run } }, run);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_last_run_line_names_sources_failures_and_the_empty_state() {
        let run = LastRun {
            created_at: "2026-11-19 10:00:00".into(),
            state: EngineState::Recommended,
            sources: vec!["Navidrome".into(), "ListenBrainz similar artists".into()],
            source_errors: vec![SourceError {
                source: "Deezer".into(),
                message: "Deezer kept refusing for too many requests".into(),
                failures: 3,
            }],
        };
        let html = render_run(run.clone());
        assert!(html.contains("Last run used: Navidrome, ListenBrainz similar artists"));
        assert!(html.contains("Deezer failed: Deezer kept refusing for too many requests"));
        let empty = LastRun {
            state: EngineState::NoListeningData,
            sources: vec![],
            ..run.clone()
        };
        let html = render_run(empty);
        assert!(html.contains("No listening data yet. Play or star music in Navidrome."));
        assert!(html.contains("Deezer failed: Deezer kept refusing for too many requests"));
        let unread = LastRun {
            sources: vec![],
            source_errors: vec![SourceError {
                source: "Navidrome".into(),
                message: "Discovery needs your Navidrome library".into(),
                failures: 1,
            }],
            ..run
        };
        let html = render_run(unread);
        assert!(!html.contains("Last run used"));
        assert!(html.contains("Navidrome failed: Discovery needs your Navidrome library"));
    }

    #[test]
    fn a_failed_drop_shows_its_reason_until_an_action_on_the_track_succeeds() {
        let mut errors = std::collections::HashMap::new();
        // How remove_discovery_track's failure_error reaches the browser.
        let refused = ServerFnError::ServerError {
            message: "The file changed after it was checked".to_string(),
            code: api::REFUSAL_CODE,
            details: None,
        };
        note_outcome(&mut errors, "t1".to_string(), Err(refused));
        note_outcome(&mut errors, "t2".to_string(), Ok(()));
        assert_eq!(
            errors.get("t1").map(String::as_str),
            Some("The file changed after it was checked")
        );
        assert_eq!(errors.len(), 1);

        note_outcome(&mut errors, "t1".to_string(), Ok(()));
        assert!(errors.is_empty());
    }
}
