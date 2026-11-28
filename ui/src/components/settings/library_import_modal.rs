//! Modal for 1-click scanning and importing an existing music library into beets.

use dioxus::prelude::*;
use shared::library_import::{LibraryImportBatch, LibraryImportMode, LibraryScanPreview};

use crate::{
    friendly_error, use_auth, IconCheck, IconDisc3, IconInfo, IconLibrary, IconLoader, IconMusic,
    IconRefreshCw, Modal, TextInput,
};

fn pluralize(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn preview_summary_text(preview: &LibraryScanPreview) -> String {
    let albums = pluralize(preview.album_units, "unindexed album", "unindexed albums");
    let tracks = pluralize(preview.singleton_units, "loose track", "loose tracks");
    let files = pluralize(preview.total_files, "file", "files");
    format!(
        "Found {albums} and {tracks} ({files}), {} already indexed",
        preview.already_indexed_files
    )
}

fn is_source_mismatch(
    use_custom_source: bool,
    custom_source_path: &str,
    scanned_source: Option<&str>,
) -> bool {
    if use_custom_source {
        let trimmed = custom_source_path.trim();
        trimmed.is_empty() || scanned_source != Some(trimmed)
    } else {
        scanned_source.is_some()
    }
}

fn is_mode_disabled_for_external(in_place: Option<bool>, mode: LibraryImportMode) -> bool {
    matches!(in_place, Some(false)) && mode != LibraryImportMode::TagAndOrganize
}

/// Modal dialog for scanning unimported files in the user's library (or an external
/// approved music folder) and starting a persistent background library import batch.
#[component]
pub fn LibraryImportModal(
    mut open: Signal<bool>,
    #[props(default)] on_started: Option<EventHandler<LibraryImportBatch>>,
) -> Element {
    let auth = use_auth();

    let mut prev_open = use_signal(|| false);
    let mut scan_req_id = use_signal(|| 0u64);
    let mut scanning = use_signal(|| false);
    let mut starting = use_signal(|| false);
    let mut preview = use_signal(|| None::<LibraryScanPreview>);
    let mut error = use_signal(|| None::<String>);
    let mut started_batch = use_signal(|| None::<LibraryImportBatch>);

    let mut use_custom_source = use_signal(|| false);
    let mut custom_source_path = use_signal(String::new);
    let mut scanned_source = use_signal(|| None::<String>);
    let mut selected_mode = use_signal(|| LibraryImportMode::TagAndOrganize);
    let mut show_sample = use_signal(|| false);

    let run_scan = use_callback(move |source_path: Option<String>| {
        let req_id = scan_req_id.peek().wrapping_add(1);
        scan_req_id.set(req_id);
        scanning.set(true);
        error.set(None);
        spawn(async move {
            let res = auth
                .call(api::scan_unimported_files(source_path.clone()))
                .await;
            if *scan_req_id.peek() != req_id {
                return;
            }
            match res {
                Ok(scan_preview) => {
                    if !scan_preview.in_place {
                        selected_mode.set(LibraryImportMode::TagAndOrganize);
                    }
                    scanned_source.set(source_path);
                    preview.set(Some(scan_preview));
                }
                Err(e) => {
                    preview.set(None);
                    error.set(Some(friendly_error(&e)));
                }
            }
            scanning.set(false);
        });
    });

    use_effect(move || {
        let is_open = open();
        let was_open = *prev_open.peek();
        if is_open == was_open {
            return;
        }
        prev_open.set(is_open);

        if is_open {
            use_custom_source.set(false);
            custom_source_path.set(String::new());
            scanned_source.set(None);
            selected_mode.set(LibraryImportMode::TagAndOrganize);
            show_sample.set(false);
            error.set(None);
            started_batch.set(None);
            starting.set(false);
            preview.set(None);
            run_scan.call(None);
        } else {
            let next_id = scan_req_id.peek().wrapping_add(1);
            scan_req_id.set(next_id);
            scanning.set(false);
        }
    });

    if !open() {
        return rsx! {};
    }

    let mut close_modal = move || {
        if !*starting.peek() {
            open.set(false);
        }
    };

    let current_preview = preview.read().clone();
    let total_units = current_preview.as_ref().map(|p| p.total_units).unwrap_or(0);
    let custom_mode = use_custom_source();
    let custom_path_val = custom_source_path();
    let custom_path_empty = custom_path_val.trim().is_empty();
    let source_mismatch = is_source_mismatch(
        custom_mode,
        &custom_path_val,
        scanned_source.read().as_deref(),
    );
    let can_start = !scanning()
        && !starting()
        && !source_mismatch
        && current_preview.is_some()
        && total_units > 0
        && started_batch.read().is_none();

    let trigger_rescan = move || {
        if *scanning.peek() || *starting.peek() {
            return;
        }
        let source = if *use_custom_source.peek() {
            let trimmed = custom_source_path.peek().trim().to_string();
            if trimmed.is_empty() {
                return;
            }
            Some(trimmed)
        } else {
            None
        };
        run_scan.call(source);
    };

    let handle_start = move |_| {
        if *scanning.peek() || *starting.peek() {
            return;
        }
        if preview.peek().is_none() {
            return;
        }
        let mismatched = is_source_mismatch(
            *use_custom_source.peek(),
            &custom_source_path.peek(),
            scanned_source.peek().as_deref(),
        );
        if mismatched {
            return;
        }
        let source = scanned_source.peek().clone();
        let mode = if preview.peek().as_ref().is_some_and(|p| !p.in_place) {
            LibraryImportMode::TagAndOrganize
        } else {
            *selected_mode.peek()
        };
        starting.set(true);
        error.set(None);
        spawn(async move {
            match auth.call(api::start_library_import(source, mode)).await {
                Ok(batch) => {
                    starting.set(false);
                    if let Some(handler) = on_started {
                        open.set(false);
                        handler.call(batch);
                    } else {
                        started_batch.set(Some(batch));
                    }
                }
                Err(e) => {
                    starting.set(false);
                    error.set(Some(friendly_error(&e)));
                }
            }
        });
    };

    let footer = if started_batch.read().is_some() {
        rsx! {
            div { class: "flex items-center justify-end gap-2",
                button {
                    r#type: "button",
                    class: "h-9 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold cursor-pointer hover:brightness-105 transition-[filter]",
                    onclick: move |_| close_modal(),
                    "Done"
                }
            }
        }
    } else {
        rsx! {
            div { class: "flex items-center justify-end gap-2",
                button {
                    r#type: "button",
                    disabled: starting(),
                    class: "h-9 px-3.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle text-[13px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-45",
                    onclick: move |_| close_modal(),
                    "Cancel"
                }
                button {
                    r#type: "button",
                    disabled: !can_start,
                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-[10px] bg-beet-accent-solid text-beet-text-primary text-[13px] font-semibold hover:brightness-110 transition-[filter,opacity] cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed",
                    onclick: handle_start,
                    if starting() {
                        IconLoader { size: 14 }
                        span { "Starting..." }
                    } else {
                        IconLibrary { size: 14 }
                        span { "Start Import" }
                    }
                }
            }
        }
    };

    let mode_options = [
        (
            LibraryImportMode::TagAndOrganize,
            "Tag & Organize",
            true,
            "Autotags with MusicBrainz/beets, fetches artwork, and organizes into Artist/Album/Track. Uncertain albums wait in the Activity Match Picker.",
        ),
        (
            LibraryImportMode::TagInPlace,
            "Tag In-Place",
            false,
            "Autotags files where they sit without moving or renaming them. Safe for hardlinks and keeps existing Navidrome ratings and playlists intact.",
        ),
        (
            LibraryImportMode::RegisterAsIs,
            "Register As-Is",
            false,
            "Fast index into beets without modifying tags, artwork, or file paths.",
        ),
    ];

    rsx! {
        Modal {
            on_close: move |_| close_modal(),
            header: rsx! {
                div { class: "space-y-0.5",
                    h2 { class: "text-[16px] font-semibold text-beet-text-primary flex items-center gap-2",
                        IconLibrary { size: 16, class: "text-beet-accent-text".to_string() }
                        span { "Import Existing Library" }
                    }
                    p { class: "text-[12px] text-beet-text-tertiary",
                        "Scan your music folder for unindexed albums and tracks and register them in beets."
                    }
                }
            },
            footer: Some(footer),

            div { class: "space-y-4",
                if let Some(err) = error() {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3.5 py-2.5 text-[12px] text-beet-crimson-text break-words",
                        "{err}"
                    }
                }

                if let Some(batch) = started_batch.read().clone() {
                    div { class: "rounded-[12px] bg-beet-leaf-surface/60 border border-beet-leaf-solid/35 p-4 space-y-2",
                        div { class: "flex items-center gap-2 text-beet-leaf-text font-semibold text-[14px]",
                            IconCheck { size: 16 }
                            span { "Library import started" }
                        }
                        p { class: "text-[12px] text-beet-text-secondary leading-relaxed",
                            "Queued {pluralize(batch.total_units, \"item\", \"items\")} ({pluralize(batch.total_files, \"file\", \"files\")}). Open the Activity page to monitor live progress and resolve any albums that need a Match Picker decision."
                        }
                    }
                } else {
                    // 1. Scan Preview Summary
                    if scanning() {
                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-5 flex items-center justify-center gap-2.5 text-[13px] text-beet-text-secondary",
                            IconLoader { size: 15, class: "text-beet-accent-text".to_string() }
                            span { "Scanning library folder for unindexed music..." }
                        }
                    } else if let Some(ref scan) = current_preview {
                        div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4 space-y-3",
                            div { class: "space-y-1",
                                div { class: "flex items-center justify-between gap-2",
                                    span { class: "text-[11px] font-medium uppercase tracking-wider text-beet-text-tertiary",
                                        "Target Library"
                                    }
                                    if !scan.in_place {
                                        span { class: "px-2 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text text-[10px] font-semibold uppercase tracking-wider",
                                            "External source"
                                        }
                                    }
                                }
                                p { class: "text-[12px] tabular-nums-mono text-beet-text-primary break-all",
                                    "{scan.target_path}"
                                }
                                if !scan.in_place {
                                    p { class: "text-[11px] tabular-nums-mono text-beet-text-tertiary break-all",
                                        "Source: {scan.source_path}"
                                    }
                                }
                            }

                            if scan.total_units == 0 {
                                div { class: "rounded-[10px] bg-beet-leaf-surface/50 border border-beet-leaf-solid/30 px-3.5 py-2.5 text-[12px] text-beet-leaf-text flex items-start gap-2",
                                    IconCheck { size: 15, class: "mt-0.5 shrink-0".to_string() }
                                    div { class: "space-y-0.5",
                                        p { class: "font-semibold", "Everything is already indexed!" }
                                        p { class: "text-[11px] text-beet-text-secondary",
                                            if scan.already_indexed_files > 0 {
                                                "{pluralize(scan.already_indexed_files, \"file\", \"files\")} already registered in beets."
                                            } else {
                                                "No audio files were found in this folder."
                                            }
                                        }
                                    }
                                }
                            } else {
                                p { class: "text-[13px] text-beet-text-primary leading-relaxed",
                                    "{preview_summary_text(scan)}"
                                }

                                if !scan.sample_units.is_empty() {
                                    div { class: "pt-1",
                                        button {
                                            r#type: "button",
                                            class: "text-[12px] font-medium text-beet-accent-text hover:underline cursor-pointer",
                                            onclick: move |_| {
                                                let next = !*show_sample.peek();
                                                show_sample.set(next);
                                            },
                                            if show_sample() {
                                                "Hide discovered sample ({scan.sample_units.len()})"
                                            } else {
                                                "Show discovered sample ({scan.sample_units.len()})"
                                            }
                                        }

                                        if show_sample() {
                                            div { class: "mt-2 space-y-1.5 max-h-48 overflow-y-auto studio-scrollbar pr-1",
                                                for unit in scan.sample_units.iter() {
                                                    div {
                                                        key: "scan-unit-{unit.source_path}",
                                                        class: "flex items-center justify-between gap-2 px-2.5 py-2 rounded-[8px] bg-beet-surface-low border border-beet-border-subtle",
                                                        div { class: "flex items-center gap-2 min-w-0",
                                                            if unit.is_album_dir {
                                                                span { class: "inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text text-[10px] font-semibold uppercase shrink-0",
                                                                    IconDisc3 { size: 11 }
                                                                    "Album"
                                                                }
                                                            } else {
                                                                span { class: "inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[10px] font-semibold uppercase shrink-0",
                                                                    IconMusic { size: 11 }
                                                                    "Track"
                                                                }
                                                            }
                                                            span {
                                                                class: "text-[12px] text-beet-text-primary truncate",
                                                                title: "{unit.source_path}",
                                                                "{unit.display_name}"
                                                            }
                                                        }
                                                        span { class: "text-[11px] tabular-nums-mono text-beet-text-tertiary shrink-0",
                                                            "{pluralize(unit.file_count, \"file\", \"files\")}"
                                                        }
                                                    }
                                                }
                                                if scan.total_units as usize > scan.sample_units.len() {
                                                    p { class: "text-[11px] text-beet-text-tertiary pt-1",
                                                        "Showing first {scan.sample_units.len()} of {scan.total_units} discovered items."
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // 2. Optional external source folder disclosure
                    div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-2.5",
                        button {
                            r#type: "button",
                            class: "w-full flex items-center justify-between gap-2 text-left text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary cursor-pointer",
                            onclick: move |_| {
                                let next = !*use_custom_source.peek();
                                use_custom_source.set(next);
                                if !next {
                                    custom_source_path.set(String::new());
                                    run_scan.call(None);
                                }
                            },
                            span { "Import from a different folder" }
                            span { class: "text-[11px] text-beet-text-tertiary",
                                if use_custom_source() { "Use personal library" } else { "Configure" }
                            }
                        }

                        if custom_mode {
                            div { class: "space-y-2 pt-1 border-t border-beet-border-subtle",
                                div { class: "flex items-center gap-2",
                                    div { class: "flex-1",
                                        TextInput {
                                            value: custom_path_val.clone(),
                                            placeholder: "/music/old-collection",
                                            aria_label: "Source folder path",
                                            class: "tabular-nums-mono text-[13px]",
                                            oninput: move |v: String| custom_source_path.set(v),
                                            on_enter: move |_| trigger_rescan(),
                                        }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: scanning() || starting() || custom_path_empty,
                                        class: "inline-flex items-center gap-1.5 h-10 px-3.5 rounded-[10px] bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary hover:bg-beet-surface-highest cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed shrink-0",
                                        onclick: move |_| trigger_rescan(),
                                        IconRefreshCw { size: 13 }
                                        span { "Rescan" }
                                    }
                                }
                                if !scanning() && source_mismatch {
                                    p { class: "text-[11px] text-beet-gold leading-relaxed",
                                        if custom_path_empty {
                                            "Enter a folder path and click Rescan to preview this folder before importing."
                                        } else {
                                            "Click Rescan to preview this folder before importing."
                                        }
                                    }
                                }
                                p { class: "text-[11px] text-beet-text-tertiary leading-relaxed",
                                    "The source folder must be inside an admin-approved music root."
                                }
                            }
                        }
                    }

                    // 3. Mode Radio Cards
                    div { class: "space-y-2",
                        p { class: "text-[12px] font-medium text-beet-text-secondary",
                            "Import Mode"
                        }
                        div { class: "space-y-2", role: "radiogroup", aria_label: "Library import mode",
                            for (mode, title, recommended, desc) in mode_options {
                                {
                                    let in_place = current_preview.as_ref().map(|p| p.in_place);
                                    let is_external_disabled = is_mode_disabled_for_external(in_place, mode);
                                    let is_selected = selected_mode() == mode;
                                    let card_cls = if is_external_disabled {
                                        "bg-beet-surface-low border-beet-border-subtle opacity-55 cursor-not-allowed"
                                    } else if is_selected {
                                        "bg-beet-accent-surface/50 border-beet-accent/50 cursor-pointer"
                                    } else {
                                        "bg-beet-surface hover:bg-beet-surface-high border-beet-border-subtle cursor-pointer"
                                    };
                                    let input_disabled = starting() || is_external_disabled;
                                    let input_cursor = if input_disabled {
                                        "cursor-not-allowed"
                                    } else {
                                        "cursor-pointer"
                                    };
                                    rsx! {
                                        label {
                                            key: "import-mode-{mode.as_str()}",
                                            aria_disabled: is_external_disabled,
                                            class: "block rounded-[10px] border p-3.5 transition-colors {card_cls}",
                                            div { class: "flex items-start gap-3",
                                                input {
                                                    r#type: "radio",
                                                    name: "library-import-mode",
                                                    value: mode.as_str(),
                                                    checked: is_selected,
                                                    disabled: input_disabled,
                                                    class: "mt-0.5 accent-beet-accent-solid shrink-0 {input_cursor}",
                                                    onchange: move |_| {
                                                        if !is_external_disabled {
                                                            selected_mode.set(mode);
                                                        }
                                                    },
                                                }
                                                div { class: "min-w-0 flex-1 space-y-1",
                                                    div { class: "flex flex-wrap items-center gap-2",
                                                        span { class: "text-[13px] font-semibold text-beet-text-primary",
                                                            "{title}"
                                                        }
                                                        if recommended {
                                                            span { class: "px-2 py-0.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[10px] font-semibold",
                                                                "Recommended"
                                                            }
                                                        }
                                                    }
                                                    p { class: "text-[12px] text-beet-text-secondary leading-relaxed",
                                                        "{desc}"
                                                    }
                                                    if is_external_disabled {
                                                        p { class: "text-[11px] text-beet-text-tertiary italic leading-relaxed",
                                                            "Only available when importing files already inside your personal library folder."
                                                        }
                                                    }

                                                    if mode == LibraryImportMode::TagAndOrganize && is_selected {
                                                        if let Some(true) = in_place {
                                                            div { class: "mt-2 rounded-[8px] bg-beet-gold-surface border border-beet-gold/30 px-3 py-2 text-[11px] text-beet-gold flex items-start gap-2 leading-relaxed",
                                                                IconInfo { size: 13, class: "mt-0.5 shrink-0".to_string() }
                                                                span {
                                                                    "Organizing in-place moves files into Artist/Album/Track folders inside your library; Navidrome may reset play counts or ratings for moved files. Choose Tag In-Place if you want to keep existing paths."
                                                                }
                                                            }
                                                        } else if let Some(false) = in_place {
                                                            div { class: "mt-2 rounded-[8px] bg-beet-surface-low border border-beet-border-subtle px-3 py-2 text-[11px] text-beet-text-secondary flex items-start gap-2 leading-relaxed",
                                                                IconInfo { size: 13, class: "mt-0.5 shrink-0 text-beet-accent-text".to_string() }
                                                                span {
                                                                    "Copies files into your personal library while leaving the source folder untouched."
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_mismatch_blocks_unscanned_custom_paths() {
        // Default personal library scan matches personal library mode.
        assert!(!is_source_mismatch(false, "", None));
        // Switching back to personal library while preview is still from a custom folder mismatches.
        assert!(is_source_mismatch(false, "", Some("/music/old")));
        // Enabling custom folder with empty input mismatches personal library preview.
        assert!(is_source_mismatch(true, "", None));
        assert!(is_source_mismatch(true, "   ", None));
        // Typing a path before rescanning mismatches.
        assert!(is_source_mismatch(true, "/music/old", None));
        // Matching trimmed scanned path succeeds.
        assert!(!is_source_mismatch(true, "/music/old", Some("/music/old")));
        assert!(!is_source_mismatch(
            true,
            "  /music/old  ",
            Some("/music/old")
        ));
        // Editing the path after a scan mismatches again until rescanned.
        assert!(is_source_mismatch(true, "/music/new", Some("/music/old")));
    }

    #[test]
    fn external_folders_disable_in_place_modes_only() {
        assert!(!is_mode_disabled_for_external(
            Some(false),
            LibraryImportMode::TagAndOrganize
        ));
        assert!(is_mode_disabled_for_external(
            Some(false),
            LibraryImportMode::TagInPlace
        ));
        assert!(is_mode_disabled_for_external(
            Some(false),
            LibraryImportMode::RegisterAsIs
        ));
        for mode in [
            LibraryImportMode::TagAndOrganize,
            LibraryImportMode::TagInPlace,
            LibraryImportMode::RegisterAsIs,
        ] {
            assert!(!is_mode_disabled_for_external(Some(true), mode));
            assert!(!is_mode_disabled_for_external(None, mode));
        }
    }
}
