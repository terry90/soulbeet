//! Studio Activity view: live Acquire & Play pipelines, active downloads/imports drawer trigger,
//! On-Keep Archival FLAC Upgrade queue (`api::list_archival_upgrades`), and Deletion Guard history.

use dioxus::prelude::*;
use shared::download::{AcquireAndPlayRequest, ArchivalUpgradeStatus};
use ui::{
    dashboard::DeletionHistoryTab, use_player, AcquireStageBadge, IconActivity, IconDownload,
    IconRefreshCw, IconSparkles, JobDiagnosticsPanel, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::{use_studio, ManualChoiceModalState};

#[component]
pub fn ActivityPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let mut refreshing_upgrades = use_signal(|| false);

    let acquire_items: Vec<_> = studio.acquire_states.read().values().cloned().collect();
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
