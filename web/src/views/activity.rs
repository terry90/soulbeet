//! Studio Activity view: Existing Library Imports, live Acquire & Play pipelines,
//! Playlist Imports, active downloads/imports drawer trigger, On-Keep Archival FLAC Upgrade
//! queue (`api::list_archival_upgrades`), and Deletion Guard history.

use std::future::Future;
use std::pin::Pin;

use api::models::track_share::{LinkMode, ShareDirection, ShareHistoryEntry, ShareOrigin};
use dioxus::prelude::*;
use shared::download::{
    AcquireAndPlayRequest, ArchivalUpgradeStatus, ImportChoice, MatchChoices, MatchKind,
};
use shared::library_import::{
    LibraryImportBatch, LibraryImportBatchStatus, LibraryImportItem, LibraryImportItemStatus,
    LibraryImportMode, LibraryImportResolveAction,
};
use shared::playlist_import::ImportJobSummary;
use ui::{
    dashboard::DeletionHistoryTab, friendly_error, use_player, AcquireStageBadge, ChoicesLoader,
    IconActivity, IconDisc3, IconDownload, IconLibrary, IconListMusic, IconLoader, IconMusic,
    IconPause, IconPlay, IconRefreshCw, IconShare2, IconSparkles, IconX, JobDiagnosticsPanel,
    LibraryImportModal, MatchPicker, MatchPickerFailure, RowAction, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::web_player::{use_studio, ManualChoiceModalState};

fn library_import_mode_label(mode: LibraryImportMode) -> &'static str {
    match mode {
        LibraryImportMode::TagAndOrganize => "Tag & Organize",
        LibraryImportMode::TagInPlace => "Tag In-Place",
        LibraryImportMode::RegisterAsIs => "Register As-Is",
    }
}

fn library_import_status_badge(status: LibraryImportBatchStatus) -> (&'static str, &'static str) {
    match status {
        LibraryImportBatchStatus::Queued => (
            "bg-beet-surface-high border-beet-border-default text-beet-text-secondary",
            "Queued",
        ),
        LibraryImportBatchStatus::Running => (
            "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
            "Running",
        ),
        LibraryImportBatchStatus::Paused => (
            "bg-beet-gold-surface border-beet-gold/35 text-beet-gold",
            "Paused",
        ),
        LibraryImportBatchStatus::Completed => (
            "bg-beet-leaf-surface border-beet-leaf-solid/35 text-beet-leaf-text",
            "Completed",
        ),
        LibraryImportBatchStatus::Cancelled => (
            "bg-beet-surface-high border-beet-border-subtle text-beet-text-tertiary",
            "Cancelled",
        ),
        LibraryImportBatchStatus::Failed => (
            "bg-beet-crimson-surface border-beet-crimson-text/30 text-beet-crimson-text",
            "Failed",
        ),
    }
}

fn library_import_progress_bar_class(status: LibraryImportBatchStatus) -> &'static str {
    match status {
        LibraryImportBatchStatus::Running | LibraryImportBatchStatus::Queued => {
            "bg-beet-accent-solid"
        }
        LibraryImportBatchStatus::Paused => "bg-beet-gold",
        LibraryImportBatchStatus::Completed => "bg-beet-leaf-solid",
        LibraryImportBatchStatus::Failed => "bg-beet-crimson-solid",
        LibraryImportBatchStatus::Cancelled => "bg-beet-text-tertiary",
    }
}

fn upsert_library_batch(list: &mut Vec<LibraryImportBatch>, batch: LibraryImportBatch) {
    if let Some(existing) = list.iter_mut().find(|b| b.id == batch.id) {
        *existing = batch;
    } else {
        list.insert(0, batch);
    }
}

fn format_item_reason(item: &LibraryImportItem) -> Option<String> {
    if let Some(ref failure) = item.failure {
        if !failure.message.trim().is_empty() {
            return Some(failure.message.clone());
        }
    }
    item.skip_kind.as_deref().map(|kind| match kind {
        "already_in_library" => "Already present in your library".to_string(),
        "no_match" => "No confident MusicBrainz match found".to_string(),
        "as_is_missing_tags" => {
            "Audio file is missing required artist or title tags for as-is import".to_string()
        }
        "choice_invalid" => {
            "Selected MusicBrainz match was invalid or did not match these files".to_string()
        }
        other => format!("Skipped ({other})"),
    })
}

#[component]
fn LibraryImportItemRow(
    item: LibraryImportItem,
    mode: LibraryImportMode,
    busy: bool,
    resolve_error: Option<String>,
    on_resolve: EventHandler<(String, LibraryImportResolveAction)>,
) -> Element {
    let mut picker_notice = use_signal(|| None::<String>);
    let mut latest_choices = use_signal(|| item.choices.clone());
    let is_album_dir = item.is_album_dir;
    let is_register_as_is = mode == LibraryImportMode::RegisterAsIs;

    use_effect(use_reactive(&item.choices, move |choices| {
        latest_choices.set(choices);
    }));

    use_effect(use_reactive(&resolve_error, move |err| {
        picker_notice.set(err);
    }));

    let on_load: ChoicesLoader = use_callback(move |_item_id: String| {
        let choices = latest_choices
            .peek()
            .clone()
            .unwrap_or_else(|| MatchChoices {
                kind: if is_album_dir {
                    MatchKind::Album
                } else {
                    MatchKind::Track
                },
                candidates: vec![],
                missing_tags: !is_album_dir,
            });
        Box::pin(async move { Ok(choices) })
            as Pin<Box<dyn Future<Output = Result<MatchChoices, MatchPickerFailure>>>>
    });

    let item_id_for_choose = item.id.clone();
    let on_choose = use_callback(move |choice: ImportChoice| {
        picker_notice.set(None);
        on_resolve.call((
            item_id_for_choose.clone(),
            LibraryImportResolveAction::Choose(choice),
        ));
    });

    let item_id_for_dismiss = item.id.clone();
    let reason = format_item_reason(&item);
    let dismiss_cursor = if busy {
        "cursor-wait"
    } else {
        "cursor-pointer disabled:cursor-not-allowed"
    };
    let unit_badge = if item.is_album_dir {
        if item.file_count == 1 {
            "Album · 1 track".to_string()
        } else {
            format!("Album · {} tracks", item.file_count)
        }
    } else {
        "Single track".to_string()
    };

    rsx! {
        div { class: "rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-3 space-y-2",
            div { class: "min-w-0 space-y-1",
                div { class: "flex flex-wrap items-center gap-2",
                    p {
                        class: "text-[13px] font-semibold text-beet-text-primary break-words",
                        title: "{item.source_path}",
                        "{item.display_name}"
                    }
                    if item.is_album_dir {
                        span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text text-[10px] font-semibold shrink-0",
                            IconDisc3 { size: 11 }
                            "{unit_badge}"
                        }
                    } else {
                        span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[10px] font-semibold shrink-0",
                            IconMusic { size: 11 }
                            "{unit_badge}"
                        }
                    }
                }
                p { class: "text-[11px] tabular-nums-mono text-beet-text-tertiary break-all",
                    "{item.source_path}"
                }
                if let Some(ref msg) = reason {
                    p {
                        class: if item.status == LibraryImportItemStatus::Failed {
                            "text-[12px] text-beet-crimson-text break-words"
                        } else {
                            "text-[12px] text-beet-gold break-words"
                        },
                        "{msg}"
                    }
                }
            }

            div { class: "flex flex-wrap items-center gap-2",
                if is_register_as_is {
                    RowAction {
                        busy,
                        onclick: move |_| on_choose.call(ImportChoice::AsIs),
                        "Retry as-is"
                    }
                } else {
                    RowAction {
                        busy,
                        onclick: move |_| on_choose.call(ImportChoice::Auto),
                        "Retry"
                    }
                    RowAction {
                        busy,
                        onclick: move |_| on_choose.call(ImportChoice::AsIs),
                        "Import as-is"
                    }
                }
                button {
                    r#type: "button",
                    disabled: busy,
                    aria_busy: busy,
                    class: "inline-flex items-center gap-1 text-xs font-medium px-2.5 py-1 rounded-[8px] bg-beet-surface border border-beet-border-subtle text-beet-text-secondary enabled:hover:text-beet-text-primary enabled:hover:bg-beet-surface-high transition-colors disabled:opacity-50 {dismiss_cursor} shrink-0",
                    onclick: move |_| {
                        picker_notice.set(None);
                        on_resolve.call((
                            item_id_for_dismiss.clone(),
                            LibraryImportResolveAction::Dismiss,
                        ));
                    },
                    IconX { size: 12 }
                    span { "Dismiss" }
                }
            }

            if is_register_as_is {
                if let Some(ref notice_msg) = picker_notice() {
                    p { class: "text-[12px] text-beet-crimson-text break-words",
                        "{notice_msg}"
                    }
                }
            } else {
                MatchPicker {
                    item: item.id.clone(),
                    busy,
                    on_load,
                    on_choose,
                    notice: picker_notice,
                }
            }
        }
    }
}

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

fn link_mode_badge(mode: LinkMode) -> (&'static str, &'static str) {
    match mode {
        LinkMode::Hardlink => (
            "bg-beet-leaf-surface border-beet-leaf-solid/35 text-beet-leaf-text",
            "hardlink",
        ),
        LinkMode::Reflink => (
            "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
            "reflink",
        ),
        LinkMode::Copy => (
            "bg-beet-surface-high border-beet-border-default text-beet-text-secondary",
            "copy",
        ),
    }
}

fn format_share_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_string())
}

#[component]
pub fn ActivityPage() -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let mut refreshing_upgrades = use_signal(|| false);

    // Existing Library Import batches state
    let mut library_batches = use_signal(Vec::<LibraryImportBatch>::new);
    let mut library_batches_loading = use_signal(|| true);
    let mut library_batches_error = use_signal(|| None::<String>);
    let mut import_modal_open = use_signal(|| false);
    let mut mutating_batch = use_signal(|| None::<String>);
    let mut expanded_batch = use_signal(|| None::<String>);
    let mut expanded_filter = use_signal(|| LibraryImportItemStatus::NeedsDecision);
    let mut expanded_items = use_signal(Vec::<LibraryImportItem>::new);
    let mut expanded_loading = use_signal(|| false);
    let mut expanded_error = use_signal(|| None::<String>);
    let mut expanded_req_id = use_signal(|| 0u64);
    let mut resolving_item = use_signal(|| None::<String>);
    let mut resolve_error_for_item = use_signal(|| None::<(String, String)>);

    let mut import_jobs = use_signal(Vec::<ImportJobSummary>::new);
    let mut refreshing_imports = use_signal(|| false);
    let mut resolving_job = use_signal(|| None::<String>);

    // Shared Music history state
    let mut share_direction = use_signal(|| ShareDirection::All);
    let mut share_entries = use_signal(Vec::<ShareHistoryEntry>::new);
    let mut share_loading = use_signal(|| true);
    let mut share_error = use_signal(|| None::<String>);
    let mut share_req_id = use_signal(|| 0u64);

    let load_library_batches = use_callback(move |()| {
        library_batches_loading.set(true);
        library_batches_error.set(None);
        spawn(async move {
            match auth.call(api::list_library_import_batches()).await {
                Ok(list) => library_batches.set(list),
                Err(e) => library_batches_error.set(Some(friendly_error(&e))),
            }
            library_batches_loading.set(false);
        });
    });

    let load_expanded_items = use_callback(
        move |(batch_id, filter): (String, LibraryImportItemStatus)| {
            let req_id = expanded_req_id.peek().wrapping_add(1);
            expanded_req_id.set(req_id);
            expanded_batch.set(Some(batch_id.clone()));
            expanded_filter.set(filter);
            expanded_loading.set(true);
            expanded_error.set(None);
            spawn(async move {
                let res = auth
                    .call(api::get_library_import_batch(
                        batch_id.clone(),
                        Some(filter),
                        Some(50),
                        Some(0),
                    ))
                    .await;
                if *expanded_req_id.peek() != req_id {
                    return;
                }
                match res {
                    Ok(detail) => {
                        upsert_library_batch(&mut library_batches.write(), detail.batch);
                        expanded_items.set(detail.items);
                    }
                    Err(e) => expanded_error.set(Some(friendly_error(&e))),
                }
                expanded_loading.set(false);
            });
        },
    );

    use_future(move || async move {
        load_library_batches.call(());
    });

    // Live WebSocket updates for library import batches & expanded item lists
    use_effect(move || {
        let Some(event) = studio.library_import_event.read().clone() else {
            return;
        };
        let event_batch_id = event.batch.id.clone();
        upsert_library_batch(&mut library_batches.write(), event.batch);

        if expanded_batch.peek().as_deref() == Some(event_batch_id.as_str()) {
            if let Some(latest) = event.latest_item {
                let active_filter = *expanded_filter.peek();
                let mut items = expanded_items.write();
                items.retain(|i| i.id != latest.id);
                if latest.status == active_filter {
                    items.push(latest);
                    items.sort_by_key(|i| i.seq);
                }
            }
        }
    });

    let handle_batch_control = use_callback(move |(batch_id, action): (String, &'static str)| {
        if mutating_batch.peek().is_some() {
            return;
        }
        mutating_batch.set(Some(batch_id.clone()));
        spawn(async move {
            let res = match action {
                "pause" => auth.call(api::pause_library_import(batch_id)).await,
                "resume" => auth.call(api::resume_library_import(batch_id)).await,
                _ => auth.call(api::cancel_library_import(batch_id)).await,
            };
            match res {
                Ok(updated) => upsert_library_batch(&mut library_batches.write(), updated),
                Err(e) => studio.show_toast(format!(
                    "Could not {action} library import: {}",
                    friendly_error(&e)
                )),
            }
            mutating_batch.set(None);
        });
    });

    let handle_resolve_library_item = use_callback(
        move |(item_id, action): (String, LibraryImportResolveAction)| {
            if resolving_item.peek().is_some() {
                return;
            }
            resolving_item.set(Some(item_id.clone()));
            resolve_error_for_item.set(None);
            spawn(async move {
                match auth
                    .call(api::resolve_library_import_item(item_id.clone(), action))
                    .await
                {
                    Ok(detail) => {
                        let batch_id = detail.batch.id.clone();
                        let remaining_for_filter = match *expanded_filter.peek() {
                            LibraryImportItemStatus::NeedsDecision => {
                                detail.batch.needs_decision_units
                            }
                            LibraryImportItemStatus::Failed => detail.batch.failed_units,
                            _ => 0,
                        };
                        upsert_library_batch(&mut library_batches.write(), detail.batch);
                        if expanded_batch.peek().as_deref() == Some(batch_id.as_str()) {
                            let active_filter = *expanded_filter.peek();
                            let filtered: Vec<LibraryImportItem> = detail
                                .items
                                .into_iter()
                                .filter(|i| i.status == active_filter)
                                .collect();
                            if (filtered.len() as u64) < remaining_for_filter.min(50) {
                                load_expanded_items.call((batch_id, active_filter));
                            } else {
                                expanded_items.set(filtered);
                            }
                        }
                    }
                    Err(e) => {
                        let msg = friendly_error(&e);
                        resolve_error_for_item.set(Some((item_id, msg.clone())));
                        studio.show_toast(format!("Could not resolve item: {msg}"));
                    }
                }
                resolving_item.set(None);
            });
        },
    );

    let load_share_history = use_callback(move |dir: ShareDirection| {
        let req_id = share_req_id.peek().wrapping_add(1);
        share_req_id.set(req_id);
        share_loading.set(true);
        share_error.set(None);
        spawn(async move {
            let res = auth
                .call(api::get_share_history(Some(dir), Some(50), Some(0)))
                .await;
            if *share_req_id.peek() != req_id {
                return;
            }
            match res {
                Ok(page) => share_entries.set(page.entries),
                Err(e) => share_error.set(Some(friendly_error(&e))),
            }
            share_loading.set(false);
        });
    });

    use_effect(move || {
        let dir = share_direction();
        load_share_history.call(dir);
    });

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

    let library_batch_list: Vec<LibraryImportBatch> =
        library_batches.read().iter().take(5).cloned().collect();
    let expanded_batch_id = expanded_batch();
    let active_expanded_filter = expanded_filter();
    let current_expanded_items = expanded_items.read().clone();
    let active_resolving_item = resolving_item();
    let active_resolve_error = resolve_error_for_item.read().clone();
    let active_mutating_batch = mutating_batch();

    let acquire_items: Vec<(String, _)> = studio
        .acquire_states
        .read()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
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
            // 0. Existing Library Imports
            section { class: "space-y-3",
                div { class: "flex flex-wrap items-center justify-between gap-2",
                    div { class: "flex items-center gap-2",
                        IconLibrary { size: 16, class: "text-beet-accent-text".to_string() }
                        h2 { class: "text-[18px] font-semibold text-beet-text-primary",
                            "Library Imports"
                        }
                        if !library_batch_list.is_empty() {
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                "{library_batch_list.len()}"
                            }
                        }
                    }
                    div { class: "flex flex-wrap items-center gap-2",
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-accent-solid text-beet-text-primary text-[12px] font-semibold hover:brightness-110 transition-all cursor-pointer",
                            onclick: move |_| import_modal_open.set(true),
                            IconLibrary { size: 13 }
                            span { "Import Existing Library" }
                        }
                        button {
                            r#type: "button",
                            disabled: library_batches_loading(),
                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-50",
                            onclick: move |_| load_library_batches.call(()),
                            IconRefreshCw { size: 13 }
                            span { "Refresh" }
                        }
                    }
                }

                if let Some(err) = library_batches_error() {
                    div { class: "rounded-[12px] bg-beet-crimson-surface border border-beet-crimson-text/30 p-4 text-[13px] text-beet-crimson-text",
                        "Could not load library imports: {err}"
                    }
                } else if library_batches_loading() && library_batch_list.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 flex items-center gap-2 text-[13px] text-beet-text-tertiary",
                        IconLoader { size: 14 }
                        span { "Loading library imports..." }
                    }
                } else if library_batch_list.is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 text-[13px] text-beet-text-secondary",
                        "No library imports yet. Click Import Existing Library to scan your music folder and index albums into beets."
                    }
                } else {
                    div { class: "space-y-3",
                        for batch in library_batch_list.iter() {
                            {
                                let (status_cls, status_text) = library_import_status_badge(batch.status);
                                let mode_text = library_import_mode_label(batch.mode);
                                let progress_cls = library_import_progress_bar_class(batch.status);
                                let pct = (batch.completed_units.min(batch.total_units) * 100)
                                    .checked_div(batch.total_units)
                                    .unwrap_or(100);
                                let is_external = batch.source_path != batch.target_path;
                                let is_mutating = active_mutating_batch.as_deref() == Some(batch.id.as_str());
                                let is_expanded = expanded_batch_id.as_deref() == Some(batch.id.as_str());
                                let batch_id_pause = batch.id.clone();
                                let batch_id_resume = batch.id.clone();
                                let batch_id_cancel = batch.id.clone();
                                let batch_id_needs = batch.id.clone();
                                let batch_id_failed = batch.id.clone();
                                let created_label = format_share_timestamp(&batch.created_at);

                                rsx! {
                                    div {
                                        key: "lib-batch-{batch.id}",
                                        class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 space-y-3",
                                        // Header row
                                        div { class: "flex flex-wrap items-start justify-between gap-3",
                                            div { class: "min-w-0 flex-1 space-y-1",
                                                div { class: "flex flex-wrap items-center gap-1.5",
                                                    span { class: "px-2.5 py-0.5 rounded-full border tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider {status_cls}",
                                                        "{status_text}"
                                                    }
                                                    span { class: "px-2.5 py-0.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary text-[11px] font-medium",
                                                        "{mode_text}"
                                                    }
                                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{created_label}"
                                                    }
                                                }
                                                p { class: "text-[12px] tabular-nums-mono text-beet-text-primary break-all",
                                                    "{batch.source_path}"
                                                    if is_external {
                                                        span { class: "text-beet-text-tertiary", " -> {batch.target_path}" }
                                                    }
                                                }
                                            }
                                            div { class: "flex flex-wrap items-center gap-1.5 shrink-0",
                                                if matches!(batch.status, LibraryImportBatchStatus::Running | LibraryImportBatchStatus::Queued) {
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_mutating,
                                                        class: "inline-flex items-center gap-1 h-7 px-2.5 rounded-[8px] bg-beet-surface-high border border-beet-border-default text-[11px] font-medium text-beet-text-primary hover:bg-beet-surface-highest cursor-pointer disabled:opacity-45",
                                                        onclick: move |_| handle_batch_control.call((batch_id_pause.clone(), "pause")),
                                                        IconPause { size: 12 }
                                                        span { "Pause" }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_mutating,
                                                        class: "inline-flex items-center gap-1 h-7 px-2.5 rounded-[8px] bg-beet-surface border border-beet-border-subtle text-[11px] font-medium text-beet-text-secondary hover:bg-beet-crimson-surface hover:text-beet-crimson-text hover:border-beet-crimson-text/30 cursor-pointer disabled:opacity-45",
                                                        onclick: move |_| handle_batch_control.call((batch_id_cancel.clone(), "cancel")),
                                                        IconX { size: 12 }
                                                        span { "Cancel" }
                                                    }
                                                } else if batch.status == LibraryImportBatchStatus::Paused {
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_mutating,
                                                        class: "inline-flex items-center gap-1 h-7 px-2.5 rounded-[8px] bg-beet-leaf-solid text-beet-leaf-ink text-[11px] font-semibold hover:brightness-105 cursor-pointer disabled:opacity-45",
                                                        onclick: move |_| handle_batch_control.call((batch_id_resume.clone(), "resume")),
                                                        IconPlay { size: 12 }
                                                        span { "Resume" }
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        disabled: is_mutating,
                                                        class: "inline-flex items-center gap-1 h-7 px-2.5 rounded-[8px] bg-beet-surface border border-beet-border-subtle text-[11px] font-medium text-beet-text-secondary hover:bg-beet-crimson-surface hover:text-beet-crimson-text hover:border-beet-crimson-text/30 cursor-pointer disabled:opacity-45",
                                                        onclick: move |_| handle_batch_control.call((batch_id_cancel.clone(), "cancel")),
                                                        IconX { size: 12 }
                                                        span { "Cancel" }
                                                    }
                                                }
                                            }
                                        }

                                        // Progress bar
                                        div { class: "space-y-1.5",
                                            div { class: "flex items-center justify-between gap-2 tabular-nums-mono text-[11px] text-beet-text-secondary",
                                                span {
                                                    "{batch.completed_units} / {batch.total_units} items ({batch.total_files} files)"
                                                }
                                                span { class: "font-semibold text-beet-text-primary",
                                                    "{pct}%"
                                                }
                                            }
                                            div { class: "h-1.5 w-full bg-beet-surface-high rounded-full overflow-hidden",
                                                div {
                                                    class: "h-full {progress_cls} rounded-full transition-all duration-300",
                                                    style: "width: {pct}%",
                                                }
                                            }
                                        }

                                        // Counters
                                        div { class: "flex flex-wrap items-center gap-x-4 gap-y-1 text-[12px] tabular-nums-mono",
                                            span { class: "text-beet-leaf-text",
                                                "Imported: {batch.imported_units}"
                                            }
                                            span {
                                                class: if batch.needs_decision_units > 0 {
                                                    "text-beet-gold font-medium"
                                                } else {
                                                    "text-beet-text-tertiary"
                                                },
                                                "Needs Attention: {batch.needs_decision_units}"
                                            }
                                            span { class: "text-beet-text-secondary",
                                                "Skipped: {batch.skipped_units}"
                                            }
                                            span {
                                                class: if batch.failed_units > 0 {
                                                    "text-beet-crimson-text font-medium"
                                                } else {
                                                    "text-beet-text-tertiary"
                                                },
                                                "Failed: {batch.failed_units}"
                                            }
                                            if batch.already_indexed_files > 0 {
                                                span { class: "text-beet-text-tertiary",
                                                    "({batch.already_indexed_files} already indexed)"
                                                }
                                            }
                                        }

                                        if let Some(ref err) = batch.error {
                                            div { class: "rounded-[8px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3 py-2 text-[12px] text-beet-crimson-text break-words",
                                                "{err}"
                                            }
                                        }

                                        // Expandable Needs Attention / Failed controls
                                        if batch.needs_decision_units > 0 || batch.failed_units > 0 || is_expanded {
                                            div { class: "pt-2 border-t border-beet-border-subtle flex flex-wrap items-center gap-2",
                                                if batch.needs_decision_units > 0 || (is_expanded && active_expanded_filter == LibraryImportItemStatus::NeedsDecision) {
                                                    {
                                                        let active = is_expanded && active_expanded_filter == LibraryImportItemStatus::NeedsDecision;
                                                        let btn_cls = if active {
                                                            "bg-beet-gold-surface border-beet-gold/45 text-beet-gold"
                                                        } else {
                                                            "bg-beet-surface-high border-beet-border-default text-beet-gold hover:border-beet-gold/40"
                                                        };
                                                        rsx! {
                                                            button {
                                                                r#type: "button",
                                                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-[8px] border text-[12px] font-medium cursor-pointer transition-colors {btn_cls}",
                                                                onclick: move |_| {
                                                                    if active {
                                                                        expanded_batch.set(None);
                                                                    } else {
                                                                        load_expanded_items.call((batch_id_needs.clone(), LibraryImportItemStatus::NeedsDecision));
                                                                    }
                                                                },
                                                                span { "Needs Attention ({batch.needs_decision_units})" }
                                                            }
                                                        }
                                                    }
                                                }
                                                if batch.failed_units > 0 || (is_expanded && active_expanded_filter == LibraryImportItemStatus::Failed) {
                                                    {
                                                        let active = is_expanded && active_expanded_filter == LibraryImportItemStatus::Failed;
                                                        let btn_cls = if active {
                                                            "bg-beet-crimson-surface border-beet-crimson-text/45 text-beet-crimson-text"
                                                        } else {
                                                            "bg-beet-surface-high border-beet-border-default text-beet-crimson-text hover:border-beet-crimson-text/40"
                                                        };
                                                        rsx! {
                                                            button {
                                                                r#type: "button",
                                                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-[8px] border text-[12px] font-medium cursor-pointer transition-colors {btn_cls}",
                                                                onclick: move |_| {
                                                                    if active {
                                                                        expanded_batch.set(None);
                                                                    } else {
                                                                        load_expanded_items.call((batch_id_failed.clone(), LibraryImportItemStatus::Failed));
                                                                    }
                                                                },
                                                                span { "Failed ({batch.failed_units})" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        if is_expanded {
                                            div { class: "pt-2 space-y-2",
                                                if let Some(err) = expanded_error() {
                                                    div { class: "rounded-[8px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3 py-2 text-[12px] text-beet-crimson-text",
                                                        "{err}"
                                                    }
                                                } else if expanded_loading() && current_expanded_items.is_empty() {
                                                    div { class: "flex items-center gap-2 py-3 text-[12px] text-beet-text-tertiary",
                                                        IconLoader { size: 14 }
                                                        span { "Loading items..." }
                                                    }
                                                } else if current_expanded_items.is_empty() {
                                                    p { class: "text-[12px] text-beet-text-tertiary py-1",
                                                        "No items remaining in this category."
                                                    }
                                                } else {
                                                    div { class: "space-y-2",
                                                        for item in current_expanded_items.iter() {
                                                            {
                                                                let item_busy = active_resolving_item.as_deref() == Some(item.id.as_str());
                                                                let item_err = active_resolve_error
                                                                    .as_ref()
                                                                    .filter(|(id, _)| id == &item.id)
                                                                    .map(|(_, msg)| msg.clone());
                                                                rsx! {
                                                                    LibraryImportItemRow {
                                                                        key: "lib-item-{item.id}",
                                                                        item: item.clone(),
                                                                        mode: batch.mode,
                                                                        busy: item_busy,
                                                                        resolve_error: item_err,
                                                                        on_resolve: move |pair| handle_resolve_library_item.call(pair),
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

                LibraryImportModal {
                    open: import_modal_open,
                    on_started: move |batch: LibraryImportBatch| {
                        upsert_library_batch(&mut library_batches.write(), batch);
                    },
                }
            }

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
                        "No active Acquire & Play pipelines. Search the catalog for any unowned track and click Acquire & Play to stream it seamlessly."
                    }
                } else {
                    div { class: "space-y-1", role: "list",
                        for (map_key, item) in acquire_items.iter() {
                            {
                                let retry_req = AcquireAndPlayRequest {
                                    artist: item.artist.clone(),
                                    title: item.title.clone(),
                                    album: None,
                                    recording_mbid: item.recording_mbid.clone(),
                                    release_mbid: None,
                                    duration_s: None,
                                    prefer_fast: true,
                                };
                                let manual = item.manual_choice.clone();
                                let mbid = item.recording_mbid.clone();
                                let cid = item.catalog_id.clone();
                                let cid_retry = item.catalog_id.clone();
                                let a = item.artist.clone();
                                let t = item.title.clone();
                                let failed = matches!(item.badge, AcquireStageBadge::Failed { .. });
                                rsx! {
                                    div {
                                        key: "act-acq-{map_key}",
                                        class: "space-y-1",
                                        TrackRow {
                                            title: item.title.clone(),
                                            artist: item.artist.clone(),
                                            state: TrackRowState::UnownedCatalog {
                                                acquiring_stage: Some(item.badge.clone()),
                                            },
                                            on_acquire: move |_| {
                                                studio.trigger_acquire_and_play(
                                                    auth,
                                                    player,
                                                    retry_req.clone(),
                                                    cid_retry.clone(),
                                                );
                                            },
                                            on_pick_source: move |_| {
                                                if let Some((search_id, groups)) = manual.clone() {
                                                    studio.manual_choice_modal.set(Some(ManualChoiceModalState {
                                                        recording_mbid: mbid.clone(),
                                                        catalog_id: cid.clone(),
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

            // 4. Shared Music History
            section { class: "space-y-3",
                div { class: "flex flex-wrap items-center justify-between gap-2",
                    div { class: "flex items-center gap-2",
                        IconShare2 { size: 16, class: "text-beet-leaf-text".to_string() }
                        h2 { class: "text-[18px] font-semibold text-beet-text-primary",
                            "Shared Music"
                        }
                        if !share_entries.read().is_empty() {
                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                "{share_entries.read().len()}"
                            }
                        }
                    }
                    div { class: "flex flex-wrap items-center gap-2",
                        div { class: "inline-flex items-center gap-1 p-0.5 rounded-full bg-beet-surface-low border border-beet-border-subtle",
                            for (dir, label) in [
                                (ShareDirection::All, "All"),
                                (ShareDirection::Incoming, "Received"),
                                (ShareDirection::Outgoing, "Sent"),
                            ] {
                                {
                                    let active = share_direction() == dir;
                                    let cls = if active {
                                        "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                                    } else {
                                        "border-transparent text-beet-text-secondary hover:text-beet-text-primary"
                                    };
                                    rsx! {
                                        button {
                                            key: "share-dir-{label}",
                                            r#type: "button",
                                            class: "h-7 px-3 rounded-full border text-[12px] font-medium transition-colors cursor-pointer {cls}",
                                            onclick: move |_| share_direction.set(dir),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                        button {
                            r#type: "button",
                            disabled: share_loading(),
                            class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-50",
                            onclick: move |_| load_share_history.call(share_direction()),
                            IconRefreshCw { size: 13 }
                            span { "Refresh" }
                        }
                    }
                }

                if let Some(err) = share_error() {
                    div { class: "rounded-[12px] bg-beet-crimson-surface border border-beet-crimson-text/30 p-4 text-[13px] text-beet-crimson-text",
                        "Could not load shared music history: {err}"
                    }
                } else if share_loading() && share_entries.read().is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 flex items-center gap-2 text-[13px] text-beet-text-tertiary",
                        IconLoader { size: 14 }
                        span { "Loading share history..." }
                    }
                } else if share_entries.read().is_empty() {
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 text-[13px] text-beet-text-secondary",
                        match share_direction() {
                            ShareDirection::All => "No shared tracks yet. Click the Share button on any library track or album to hardlink it into another household user's library.",
                            ShareDirection::Incoming => "No tracks have been shared with you yet.",
                            ShareDirection::Outgoing => "You have not shared any tracks yet.",
                        }
                    }
                } else {
                    {
                        let current_username = auth.username();
                        let entries = share_entries.read().clone();
                        rsx! {
                            div { class: "space-y-2",
                                for entry in entries.iter() {
                                    {
                                        let is_migration = entry.origin == ShareOrigin::Migration;
                                        let is_outgoing = current_username
                                            .as_deref()
                                            .is_some_and(|u| u.eq_ignore_ascii_case(&entry.sender_username));
                                        let (direction_label, dir_cls, detail_text) = if is_migration {
                                            (
                                                "Migrated".to_string(),
                                                "bg-beet-surface-high border-beet-border-default text-beet-text-primary",
                                                Some("Migrated from legacy folder"),
                                            )
                                        } else if is_outgoing {
                                            (
                                                format!("Sent to {}", entry.recipient_username),
                                                "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
                                                None,
                                            )
                                        } else {
                                            (
                                                format!("Received from {}", entry.sender_username),
                                                "bg-beet-leaf-surface border-beet-leaf-solid/35 text-beet-leaf-text",
                                                None,
                                            )
                                        };
                                        let (mode_cls, mode_label) = link_mode_badge(entry.link_mode);
                                        let timestamp = format_share_timestamp(&entry.created_at);
                                        let subtitle = match (entry.artist.trim(), entry.album.trim()) {
                                            ("", "") => None,
                                            (artist, "") => Some(artist.to_string()),
                                            ("", album) => Some(album.to_string()),
                                            (artist, album) => Some(format!("{artist} · {album}")),
                                        };
                                        rsx! {
                                            div {
                                                key: "share-{entry.id}",
                                                class: "rounded-[10px] bg-beet-surface border border-beet-border-subtle p-3.5 flex flex-wrap items-center justify-between gap-3",
                                                div { class: "min-w-0 flex-1 space-y-0.5",
                                                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                                        "{entry.title}"
                                                    }
                                                    if let Some(sub) = subtitle {
                                                        p { class: "text-[12px] text-beet-text-secondary truncate",
                                                            "{sub}"
                                                        }
                                                    }
                                                    if let Some(detail) = detail_text {
                                                        p { class: "text-[11px] text-beet-text-secondary",
                                                            "{detail}"
                                                        }
                                                    }
                                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                        "{timestamp}"
                                                    }
                                                }
                                                div { class: "flex flex-wrap items-center gap-1.5 shrink-0",
                                                    span { class: "px-2.5 py-0.5 rounded-full border text-[11px] font-medium {dir_cls}",
                                                        "{direction_label}"
                                                    }
                                                    span { class: "px-2 py-0.5 rounded-full border tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider {mode_cls}",
                                                        "{mode_label}"
                                                    }
                                                    if !is_migration && entry.origin != ShareOrigin::Ui {
                                                        span { class: "px-2 py-0.5 rounded-full border bg-beet-surface-high border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] uppercase",
                                                            "{entry.origin.as_str()}"
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

            // 5. Multi-Listener Delete Guard Audit History
            section { class: "rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-5",
                DeletionHistoryTab {}
            }
        }
    }
}
