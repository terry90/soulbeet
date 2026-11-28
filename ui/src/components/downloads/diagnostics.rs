//! Expandable on-demand job decision audit trail and diagnostics panel.
//! Lazy-fetches `api::get_job_diagnostics` only when expanded (zero extra requests when closed).

use dioxus::prelude::*;
use shared::download::{DiagnosticAttempt, DiagnosticEvent, JobDiagnostics, SearchStats};
use shared::failure::{Failure, RetryAdvice, Stage};

use crate::{friendly_error, Auth, Disclosure, IconLoader};

fn reason_label(reason: &str) -> &str {
    match reason {
        "version_veto" => "Version veto",
        "duration_delta" => "Duration mismatch",
        "artist_gate" => "Artist mismatch",
        "title_mismatch" => "Title mismatch",
        "below_threshold" => "Low match score",
        "format_filtered" => "Format filtered",
        "bitrate_filtered" => "Low bitrate",
        "queue_length" => "Queue too long",
        "locked_only" => "Locked files only",
        "excluded_source" => "Excluded source",
        other => other,
    }
}

fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Input => "Input",
        Stage::LibraryCheck => "Library",
        Stage::Metadata => "Metadata",
        Stage::SourceSearch => "Search",
        Stage::Enqueue => "Enqueue",
        Stage::Transfer => "Transfer",
        Stage::FileLookup => "File lookup",
        Stage::Verify => "Verify",
        Stage::Import => "Import",
        Stage::Index => "Navidrome",
        Stage::Delete => "Delete",
        Stage::Playlist => "Playlist",
        Stage::Share => "Share",
        Stage::Retag => "Retag",
    }
}

fn retry_advice_label(advice: RetryAdvice) -> &'static str {
    match advice {
        RetryAdvice::SameSourceLater => "Try again later",
        RetryAdvice::OtherSource => "Try another source",
        RetryAdvice::OtherQuery => "Try another search query",
        RetryAdvice::NeedsDecision => "Needs manual choice",
        RetryAdvice::NeedsUser => "Needs configuration",
        RetryAdvice::NotRetryable => "Not retryable",
        RetryAdvice::PerItem => "Check individual items",
    }
}

fn format_duration_span(started: Option<i64>, ended: Option<i64>) -> Option<String> {
    let (s, e) = (started?, ended?);
    let secs = (e - s).max(0);
    if secs >= 60 {
        Some(format!("{}m {:02}s", secs / 60, secs % 60))
    } else {
        Some(format!("{secs}s"))
    }
}

fn outcome_badge_class(outcome: &str) -> &'static str {
    match outcome {
        "imported" | "downloaded" | "completed" => {
            "bg-beet-leaf-surface text-beet-leaf-text border-beet-leaf-solid/30"
        }
        "failed" => "bg-beet-crimson-surface text-beet-crimson-text border-beet-crimson-text/30",
        "skipped" => "bg-beet-gold-surface text-beet-gold border-beet-gold/30",
        "downloading" | "importing" | "in_progress" => {
            "bg-beet-accent-surface text-beet-accent-text border-beet-accent-solid/30"
        }
        _ => "bg-beet-surface-high text-beet-text-tertiary border-beet-border-subtle",
    }
}

fn compact_event_detail(ev: &DiagnosticEvent) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(src) = ev.facts.get("source").and_then(|v| v.as_str()) {
        parts.push(src.to_string());
    }
    if let Some(code) = ev.facts.get("code").and_then(|v| v.as_str()) {
        parts.push(code.to_string());
    }
    if let Some(msg) = ev.facts.get("message").and_then(|v| v.as_str()) {
        parts.push(msg.to_string());
    }
    if ev.event == "search_diagnostics" {
        let responses = ev
            .facts
            .get("response_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let files = ev
            .facts
            .get("file_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        parts.push(format!("{responses} responses, {files} files"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[component]
pub fn JobDiagnosticsPanel(
    job_id: String,
    #[props(default)] failed: bool,
    #[props(default)] class: String,
) -> Element {
    let mut current_job_id = use_signal(|| job_id.clone());
    let mut open = use_signal(|| false);
    let mut loading = use_signal(|| false);
    let mut data = use_signal(|| None::<JobDiagnostics>);
    let mut error = use_signal(|| None::<String>);

    if *current_job_id.peek() != job_id {
        current_job_id.set(job_id.clone());
        open.set(false);
        loading.set(false);
        data.set(None);
        error.set(None);
    }

    let auth_ctx = try_use_context::<Auth>();
    let trigger_fetch = use_callback(move |()| {
        if loading() {
            return;
        }
        loading.set(true);
        error.set(None);
        let jid = current_job_id.peek().clone();
        spawn(async move {
            let result = match auth_ctx {
                Some(auth) => auth.call(api::get_job_diagnostics(jid.clone())).await,
                None => api::get_job_diagnostics(jid.clone()).await,
            };
            if *current_job_id.peek() != jid {
                return;
            }
            match result {
                Ok(diag) => data.set(Some(diag)),
                Err(e) => error.set(Some(friendly_error(&e))),
            }
            loading.set(false);
        });
    });

    let toggle = move |evt: Event<MouseData>| {
        evt.stop_propagation();
        let next = !open();
        open.set(next);
        if next && data.read().is_none() && !loading() {
            trigger_fetch.call(());
        }
    };

    let refresh = move |evt: Event<MouseData>| {
        evt.stop_propagation();
        trigger_fetch.call(());
    };

    let toggle_label = if open() {
        "Hide details"
    } else if failed {
        "Why?"
    } else {
        "Details"
    };

    let toggle_class = if failed {
        "inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] font-medium bg-beet-crimson-surface/70 text-beet-crimson-text border border-beet-crimson-text/30 hover:bg-beet-crimson-surface transition-colors cursor-pointer"
    } else {
        "inline-flex items-center gap-1 text-[11px] font-medium text-beet-text-tertiary hover:text-beet-text-secondary transition-colors cursor-pointer"
    };

    rsx! {
        div { class: "mt-2 {class}",
            div { class: "flex items-center gap-2",
                button {
                    r#type: "button",
                    class: "{toggle_class}",
                    aria_expanded: "{open()}",
                    onclick: toggle,
                    "{toggle_label}"
                }
                if open() && !loading() {
                    button {
                        r#type: "button",
                        class: "text-[10px] tabular-nums-mono text-beet-text-tertiary hover:text-beet-text-secondary cursor-pointer",
                        onclick: refresh,
                        "Refresh"
                    }
                }
            }

            if open() {
                div { class: "mt-2 p-3 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle space-y-3 text-[11px]",
                    if loading() && data.read().is_none() {
                        div { class: "flex items-center gap-2 text-beet-text-secondary tabular-nums-mono",
                            IconLoader { size: 12 }
                            span { "Loading diagnostics..." }
                        }
                    } else if let Some(err_msg) = error() {
                        div { class: "text-beet-crimson-text break-words",
                            "{err_msg}"
                        }
                    } else if let Some(diag) = data() {
                        DiagnosticsBody { diag }
                    }
                }
            }
        }
    }
}

#[component]
fn DiagnosticsBody(diag: JobDiagnostics) -> Element {
    rsx! {
        div { class: "space-y-3",
            // 1. Plain-language failure summary
            if let Some(ref failure) = diag.failure {
                FailureSummary { failure: failure.clone() }
            }

            // 2. Tried sources / attempts
            if !diag.attempts.is_empty() {
                AttemptsSection { attempts: diag.attempts.clone() }
            }

            // 3. Search diagnostics
            if let Some(ref stats) = diag.search_stats {
                SearchDiagnosticsSection { stats: stats.clone() }
            }

            // 4. Compact collapsible stage timeline
            if !diag.timeline.is_empty() {
                TimelineSection { timeline: diag.timeline.clone() }
            }

            if diag.failure.is_none()
                && diag.attempts.is_empty()
                && diag.search_stats.is_none()
                && diag.timeline.is_empty()
            {
                p { class: "text-beet-text-tertiary",
                    "No additional diagnostics recorded for this job."
                }
            }
        }
    }
}

#[component]
fn FailureSummary(failure: Failure) -> Element {
    let stage = stage_label(failure.stage);
    let advice = retry_advice_label(failure.retry);
    rsx! {
        div { class: "p-2.5 rounded-[8px] bg-beet-crimson-surface/40 border border-beet-crimson-text/25 space-y-1.5",
            div { class: "flex flex-wrap items-center gap-1.5",
                span { class: "px-1.5 py-0.5 rounded bg-beet-crimson-surface text-beet-crimson-text text-[10px] font-semibold uppercase tabular-nums-mono",
                    "{stage}"
                }
                span { class: "text-[10px] tabular-nums-mono text-beet-text-tertiary",
                    "{failure.code.as_str()}"
                }
                span { class: "text-[10px] text-beet-text-secondary",
                    "· {advice}"
                }
            }
            p { class: "text-[12px] text-beet-crimson-text leading-snug break-words",
                "{failure.message}"
            }
            if let Some(ref action) = failure.user_action {
                p { class: "text-[11px] text-beet-gold leading-snug",
                    "Action: {action.what} ({action.where_})"
                }
            }
        }
    }
}

#[component]
fn AttemptsSection(attempts: Vec<DiagnosticAttempt>) -> Element {
    rsx! {
        div { class: "space-y-1.5",
            div { class: "text-[10px] uppercase tracking-wider font-semibold text-beet-text-tertiary tabular-nums-mono",
                "Tried sources ({attempts.len()})"
            }
            div { class: "space-y-1.5",
                for att in attempts.iter() {
                    {
                        let badge_cls = outcome_badge_class(&att.outcome);
                        let src = att.source.as_deref().unwrap_or("Unknown source");
                        let span = format_duration_span(att.started_at, att.ended_at);
                        rsx! {
                            div {
                                key: "att-{att.slot}-{att.attempt}",
                                class: "p-2 rounded-[8px] bg-beet-surface border border-beet-border-subtle space-y-1",
                                div { class: "flex flex-wrap items-center justify-between gap-1.5",
                                    div { class: "flex items-center gap-1.5 min-w-0",
                                        span { class: "text-[10px] tabular-nums-mono text-beet-text-tertiary shrink-0",
                                            "#{att.attempt}"
                                        }
                                        span { class: "font-medium text-beet-text-primary truncate",
                                            "{src}"
                                        }
                                        if let Some(ref backend) = att.backend {
                                            span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high text-[10px] text-beet-text-tertiary uppercase tabular-nums-mono",
                                                "{backend}"
                                            }
                                        }
                                    }
                                    div { class: "flex items-center gap-1.5 shrink-0",
                                        if let Some(dur) = span {
                                            span { class: "text-[10px] tabular-nums-mono text-beet-text-tertiary",
                                                "{dur}"
                                            }
                                        }
                                        span { class: "px-1.5 py-0.5 rounded-full border text-[10px] uppercase tabular-nums-mono font-semibold {badge_cls}",
                                            "{att.outcome}"
                                        }
                                    }
                                }
                                if let Some(ref f) = att.failure {
                                    div { class: "text-[11px] text-beet-crimson-text break-words",
                                        "{f.message}"
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
fn SearchDiagnosticsSection(stats: SearchStats) -> Element {
    rsx! {
        div { class: "space-y-2",
            div { class: "text-[10px] uppercase tracking-wider font-semibold text-beet-text-tertiary tabular-nums-mono",
                "Search diagnostics"
            }
            div { class: "flex flex-wrap items-center gap-x-3 gap-y-1 tabular-nums-mono text-[11px] text-beet-text-secondary",
                span { "{stats.response_count} responses" }
                span { "{stats.file_count} files" }
                if stats.locked_file_count > 0 {
                    span { class: "text-beet-gold", "{stats.locked_file_count} locked" }
                }
                if stats.below_floor > 0 {
                    span { "{stats.below_floor} below floor" }
                }
                if stats.not_audio > 0 {
                    span { "{stats.not_audio} non-audio" }
                }
            }
            if !stats.query_text.is_empty() {
                div { class: "text-[11px] text-beet-text-tertiary truncate",
                    title: "{stats.query_text}",
                    "Query: {stats.query_text}"
                }
            }
            if !stats.rejection_counts.is_empty() {
                div { class: "flex flex-wrap gap-1.5",
                    for (reason, count) in stats.rejection_counts.iter() {
                        span {
                            key: "rej-{reason}",
                            class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-subtle text-[10px] tabular-nums-mono text-beet-text-secondary",
                            "{reason_label(reason)}: {count}"
                        }
                    }
                }
            }
            if !stats.top_rejected.is_empty() {
                div { class: "space-y-1",
                    div { class: "text-[10px] text-beet-text-tertiary",
                        "Closest rejected candidates"
                    }
                    for (idx, item) in stats.top_rejected.iter().enumerate() {
                        {
                            let score_text = item
                                .score
                                .map(|s| format!("{:.0}%", (s * 100.0).clamp(0.0, 100.0)));
                            rsx! {
                                div {
                                    key: "top-rej-{idx}",
                                    class: "p-2 rounded-[8px] bg-beet-surface border border-beet-border-subtle space-y-0.5",
                                    div { class: "flex items-center justify-between gap-2",
                                        span {
                                            class: "text-beet-text-primary truncate min-w-0",
                                            title: "{item.label}",
                                            "{item.label}"
                                        }
                                        span { class: "px-1.5 py-0.5 rounded bg-beet-gold-surface text-beet-gold text-[10px] shrink-0",
                                            "{reason_label(&item.reason)}"
                                        }
                                    }
                                    div { class: "flex items-center gap-2 text-[10px] tabular-nums-mono text-beet-text-tertiary",
                                        span { class: "truncate", "{item.source}" }
                                        if let Some(score) = score_text {
                                            span { "· score {score}" }
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
fn TimelineSection(timeline: Vec<DiagnosticEvent>) -> Element {
    let label = format!("stage timeline ({})", timeline.len());
    rsx! {
        Disclosure { label,
            div { class: "mt-1.5 space-y-1 max-h-48 overflow-y-auto studio-scrollbar",
                for ev in timeline.iter() {
                    {
                        let detail = compact_event_detail(ev);
                        let stage = stage_label(ev.stage);
                        rsx! {
                            div {
                                key: "ev-{ev.seq}",
                                class: "flex items-start gap-2 text-[11px] leading-snug",
                                span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high text-beet-text-tertiary text-[10px] uppercase tabular-nums-mono shrink-0",
                                    "{stage}"
                                }
                                div { class: "min-w-0 flex-1",
                                    span { class: "font-medium text-beet-text-secondary",
                                        "{ev.event}"
                                    }
                                    if let Some(detail) = detail {
                                        span { class: "text-beet-text-tertiary break-words",
                                            " · {detail}"
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
