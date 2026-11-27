//! Studio Activity view: live Acquire & Play pipelines, Playlist Imports, active downloads/imports
//! drawer trigger, On-Keep Archival FLAC Upgrade queue (`api::list_archival_upgrades`), and
//! Deletion Guard history.

use dioxus::prelude::*;
use shared::download::{AcquireAndPlayRequest, ArchivalUpgradeStatus};
use shared::playlist_import::ImportJobSummary;
use ui::{
    dashboard::DeletionHistoryTab, friendly_error, use_player, AcquireStageBadge, IconActivity,
    IconDownload, IconListMusic, IconRefreshCw, IconSparkles, JobDiagnosticsPanel, TrackRow,
    TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::{use_studio, ManualChoiceModalState};

fn import_job_badge(state: &str) -> (&'static str, &'static str) {
    match state {
        "queued" => (
            "bg-beet-surface-high border-beet-border-default text-beet-text-secondary",
            "Queued",
        ),
        "searching" => (
            "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
            "Searching",
        ),
        "downloading" | "importing" | "indexing" => (
            "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
            "Downloading",
        ),
        "needs_source_choice" => (
            "bg-beet-gold/15 border-beet-gold/35 text-beet-gold",
            "Needs choice",
        ),
        "needs_decision" => (
            "bg-beet-gold/15 border-beet-gold/35 text-beet-gold",
            "Needs decision",
        ),
        "done" => (
            "bg-beet-leaf-surface border-beet-leaf-solid/35 text-beet-leaf-text",
            "Imported",
        ),
        "failed" => (
            "bg-beet-crimson-surface border-beet-crimson-text/30 text-beet-crimson-text",
            "Failed",
        ),
        "cancelled" => (
            "bg-beet-surface-high border-beet-border-subtle text-beet-text-tertiary",
            "Skipped",
        ),
        _ => (
            "bg-beet-surface-high border-beet-border-subtle text-beet-text-tertiary",
            "Dismissed",
        ),
    }
}

fn is_active_import_state(state: &str) -> bool {
    !matches!(state, "done" | "failed" | "cancelled" | "dismissed")
}

#[component]
pub fn ActivityPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let mut refreshing_upgrades = use_signal(|| false);

    let mut import_jobs = use_signal(Vec::<ImportJobSummary>::new);
    let mut refreshing_imports = use_signal(|| false);
    let mut resolving_job = use_signal(|| None::<String>);

    let load_import_jobs = use_callback(move |()| {
        if *refreshing_imports.peek() {
            return;
        }
        refreshing_imports.set(true);
        spawn(async move {
            if let Ok(list) = auth.call(api::list_import_jobs()).await {
                import_jobs.set(list);
            }
            refreshing_imports.set(false);
        });
    });

    // Initial load + gentle 8s polling while any playlist import job is active.
    use_future(move || async move {
        if let Ok(list) = auth.call(api::list_import_jobs()).await {
            import_jobs.set(list);
        }
        loop {
            gloo_timers::future::TimeoutFuture::new(8000).await;
            let has_active = import_jobs
                .peek()
                .iter()
                .any(|j| is_active_import_state(&j.state));
            if has_active && !*refreshing_imports.peek() {
                if let Ok(list) = auth.call(api::list_import_jobs()).await {
                    import_jobs.set(list);
                }
            }
        }
    });

    let resolve_import_job = use_callback(move |(job_id, candidate_index): (String, Option<usize>)| {
        if resolving_job.peek().is_some() {
            return;
        }
        resolving_job.set(Some(job_id.clone()));
        spawn(async move {
            match auth
                .call(api::resolve_import_job_choice(job_id, candidate_index))
                .await
            {
                Ok(()) => {
                    if let Ok(list) = auth.call(api::list_import_jobs()).await {
                        import_jobs.set(list);
                    }
                }
                Err(e) => {
                    studio.show_toast(format!("Could not update import job: {}", friendly_error(&e)));
                }
            }
            resolving_job.set(None);
        });
    });

    let acquire_items: Vec<_> = studio.acquire_states.read().values().cloned().collect();
    let import_job_list = import_jobs.read().clone();
    let downloads_count = studio.downloads.read().len();
    let in_flight_dl = studio
        .downloads
        .read()
        .values()
        .filter(|d| d.in_flight())
        .count();
    let upgrades = studio.archival_upgrades.read().clone();

    let refresh_upgrades = move |_| {
        refreshing_upgrades.set(true);
        spawn(async move {
            if let Ok(list) = auth.call(api::list_archival_upgrades()).await {
                studio.archival_upgrades.set(list);
            }
            refreshing_upgrades.set(false);
        });
    };

    rsx! {
        div { class: "space-y-8",
            // 1. Live Acquire & Play Pipelines
            section { class: "space-y-3",
                div { class: "flex items-center justify-between gap-2",
                    div { class: "flex items-center gap-2",
                        IconSparkles { size: 16, class: "text-beet-accent-text".to_string() }
                        h2 { class: "text-[18px] font-semibold text-beet-text-primary",
                            "Acquire & Play Pipelines"
                        }
                    }
                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                        "{acquire_items.len()} TRACKED"
                    }
                }

                if acquire_items.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 text-[13px] text-beet-text-secondary",
                        "No active Acquire & Play pipelines. Search for any unowned MusicBrainz track and click Acquire & Play to stream it seamlessly."
                    }
                } else {
                    div { class: "space-y-1", role: "list",
                        for item in acquire_items.iter() {
                            {
                                let retry_req = AcquireAndPlayRequest {
                                    artist: item.artist.clone(),
                                    title: item.title.clone(),
                                    album: None,
                                    recording_mbid: None,
                                    release_mbid: None,
                                    duration_s: None,
                                    folder_id: None,
                                    prefer_fast: true,
                                };
                                let manual = item.manual_choice.clone();
                                let a = item.artist.clone();
                                let t = item.title.clone();
                                let failed = matches!(item.badge, AcquireStageBadge::Failed { .. });
                                rsx! {
                                    div {
                                        key: "act-acq-{item.artist}-{item.title}",
                                        class: "space-y-1",
                                        TrackRow {
                                            title: item.title.clone(),
                                            artist: item.artist.clone(),
                                            state: TrackRowState::UnownedCatalog {
                                                acquiring_stage: Some(item.badge.clone()),
                                            },
                                            on_acquire: move |_| {
                                                studio.trigger_acquire_and_play(auth, player, retry_req.clone());
                                            },
                                            on_pick_source: move |_| {
                                                if let Some((search_id, groups)) = manual.clone() {
                                                    studio.manual_choice_modal.set(Some(ManualChoiceModalState {
                                                        artist: a.clone(),
                                                        title: t.clone(),
                                                        search_id,
                                                        groups,
                                                    }));
                                                }
                                            },
                                        }
                                        if let Some(ref job_id) = item.job_id {
                                            div { class: "px-3 pb-1",
                                                JobDiagnosticsPanel {
                                                    key: "{job_id}",
                                                    job_id: job_id.clone(),
                                                    failed,
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

            // 1b. Playlist Import Jobs (shown only when there are active or recent import jobs)
            if !import_job_list.is_empty() {
                section { class: "space-y-3",
                    div { class: "flex items-center justify-between gap-2",
                        div { class: "flex items-center gap-2",
                            IconListMusic { size: 16, class: "text-beet-leaf-text".to_string() }
                            h2 { class: "text-[18px] font-semibold text-beet-text-primary",
                                "Playlist Imports"
                            }
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                "{import_job_list.len()}"
                            }
                        }
                        button {
                            r#type: "button",
                            disabled: refreshing_imports(),
                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-50",
                            onclick: move |_| load_import_jobs.call(()),
                            IconRefreshCw { size: 13 }
                            span { "Refresh" }
                        }
                    }

                    div { class: "space-y-2",
                        for job in import_job_list.iter() {
                            {
                                let (badge_cls, status_text) = import_job_badge(&job.state);
                                let is_resolving = resolving_job().as_deref() == Some(job.job_id.as_str());
                                let jid_skip = job.job_id.clone();
                                rsx! {
                                    div {
                                        key: "imp-job-{job.job_id}",
                                        class: "rounded-[10px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-2",
                                        div { class: "flex flex-wrap items-center justify-between gap-3",
                                            div { class: "min-w-0 flex-1",
                                                p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                    "{job.title}"
                                                }
                                                if !job.artist.is_empty() {
                                                    p { class: "text-[12px] text-beet-text-secondary truncate",
                                                        "{job.artist}"
                                                    }
                                                }
                                                if let Some(ref err) = job.error {
                                                    p { class: "text-[11px] text-beet-crimson-text mt-1 break-words",
                                                        "{err}"
                                                    }
                                                }
                                            }
                                            span { class: "px-2.5 py-1 rounded-full border tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider shrink-0 {badge_cls}",
                                                "{status_text}"
                                            }
                                        }

                                        if job.state == "needs_source_choice" {
                                            div { class: "pt-2 border-t border-beet-border-subtle space-y-2",
                                                p { class: "text-[11px] text-beet-text-secondary",
                                                    "Select a source to download or skip this track:"
                                                }
                                                div { class: "flex flex-wrap items-center gap-1.5",
                                                    for cand in job.candidates.iter() {
                                                        {
                                                            let jid = job.job_id.clone();
                                                            let idx = cand.candidate_index;
                                                            rsx! {
                                                                button {
                                                                    key: "imp-cand-{job.job_id}-{idx}",
                                                                    r#type: "button",
                                                                    disabled: is_resolving,
                                                                    title: "{cand.label}",
                                                                    class: "inline-flex items-center gap-1.5 px-2.5 py-1 rounded-[8px] bg-beet-surface-high hover:bg-beet-accent-surface border border-beet-border-default hover:border-beet-accent/40 text-[11px] text-beet-text-primary transition-colors cursor-pointer disabled:opacity-50",
                                                                    onclick: move |_| resolve_import_job.call((jid.clone(), Some(idx))),
                                                                    span { class: "font-medium truncate max-w-[160px]", "{cand.source}" }
                                                                    span { class: "tabular-nums-mono text-[10px] text-beet-text-secondary", "{cand.quality}" }
                                                                    if cand.is_lossy {
                                                                        span { class: "px-1 py-0.5 rounded bg-beet-gold/15 text-beet-gold text-[9px] font-semibold uppercase",
                                                                            "LOSSY"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_resolving,
                                                        class: "inline-flex items-center px-2.5 py-1 rounded-[8px] bg-beet-surface hover:bg-beet-crimson-surface border border-beet-border-subtle text-[11px] text-beet-text-secondary hover:text-beet-crimson-text transition-colors cursor-pointer disabled:opacity-50",
                                                        onclick: move |_| resolve_import_job.call((jid_skip.clone(), None)),
                                                        "Skip"
                                                    }
                                                }
                                            }
                                        }

                                        JobDiagnosticsPanel {
                                            key: "{job.job_id}",
                                            job_id: job.job_id.clone(),
                                            failed: job.state == "failed",
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 2. Downloads & Beets Import Queue Summary Card
            section { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 flex flex-wrap items-center justify-between gap-4",
                div { class: "flex items-center gap-3",
                    div { class: "w-10 h-10 rounded-[10px] bg-beet-leaf-surface text-beet-leaf-text flex items-center justify-center",
                        IconDownload { size: 19 }
                    }
                    div {
                        h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                            "Transfers & Beets Imports"
                        }
                        p { class: "tabular-nums-mono text-[12px] text-beet-text-secondary mt-0.5",
                            "{in_flight_dl} IN FLIGHT · {downloads_count} TOTAL IN SESSION"
                        }
                    }
                }
                button {
                    r#type: "button",
                    class: "inline-flex items-center gap-2 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium cursor-pointer",
                    onclick: move |_| studio.downloads_drawer_open.set(true),
                    IconActivity { size: 15 }
                    span { "Open Transfers & Conflict Resolver" }
                }
            }

            // 3. On-Keep Background Archival FLAC Upgrades
            section { class: "space-y-3",
                div { class: "flex items-center justify-between gap-2",
                    div {
                        h2 { class: "text-[18px] font-semibold text-beet-text-primary",
                            "Archival FLAC Upgrades"
                        }
                        p { class: "text-[12px] text-beet-text-tertiary",
                            "Lossy fast-acquired tracks automatically queued for lossless archival replacement when Kept or Starred."
                        }
                    }
                    button {
                        r#type: "button",
                        disabled: refreshing_upgrades(),
                        class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer",
                        onclick: refresh_upgrades,
                        IconRefreshCw { size: 13 }
                        span { "Refresh" }
                    }
                }

                if upgrades.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 text-[13px] text-beet-text-secondary",
                        "No archival FLAC upgrade jobs scheduled yet."
                    }
                } else {
                    div { class: "space-y-2",
                        for job in upgrades.iter() {
                            {
                                let (badge_cls, status_text) = match job.status {
                                    ArchivalUpgradeStatus::Completed => (
                                        "bg-beet-leaf-surface border-beet-leaf-solid/35 text-beet-leaf-text",
                                        "UPGRADED TO FLAC",
                                    ),
                                    ArchivalUpgradeStatus::SkippedAlreadyLossless => (
                                        "bg-beet-leaf-surface border-beet-leaf-solid/25 text-beet-leaf-text",
                                        "ALREADY LOSSLESS",
                                    ),
                                    ArchivalUpgradeStatus::Searching | ArchivalUpgradeStatus::Downloading => (
                                        "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
                                        "UPGRADING…",
                                    ),
                                    ArchivalUpgradeStatus::Queued => (
                                        "bg-beet-surface-high border-beet-border-default text-beet-text-secondary",
                                        "QUEUED",
                                    ),
                                    ArchivalUpgradeStatus::Failed => (
                                        "bg-beet-crimson-surface border-beet-crimson-text/30 text-beet-crimson-text",
                                        "FAILED",
                                    ),
                                };
                                rsx! {
                                    div {
                                        key: "arch-{job.id}",
                                        class: "rounded-[10px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-2",
                                        div { class: "flex flex-wrap items-center justify-between gap-3",
                                            div { class: "min-w-0 flex-1",
                                                p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                    "{job.title}"
                                                }
                                                p { class: "text-[12px] text-beet-text-secondary truncate",
                                                    "{job.artist} · {job.original_codec.to_ascii_uppercase()} → FLAC · trigger: {job.trigger_reason}"
                                                }
                                                if let Some(ref err) = job.last_error {
                                                    p { class: "text-[11px] text-beet-crimson-text mt-1", "{err}" }
                                                }
                                            }
                                            span { class: "px-2.5 py-1 rounded-full border tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider shrink-0 {badge_cls}",
                                                "{status_text}"
                                            }
                                        }
                                        if let Some(ref job_id) = job.job_id {
                                            JobDiagnosticsPanel {
                                                key: "{job_id}",
                                                job_id: job_id.clone(),
                                                failed: job.status == ArchivalUpgradeStatus::Failed,
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 4. Multi-Listener Delete Guard Audit History
            section { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-5",
                DeletionHistoryTab {}
            }
        }
    }
}
