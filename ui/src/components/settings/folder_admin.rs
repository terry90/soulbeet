use std::collections::HashMap;

use api::folder_policy::{ConflictKind, FolderConflict, FolderLabel};
use api::{
    add_folder_root, delete_any_folder, dismiss_legacy_folder, get_folder_admin,
    get_legacy_folders, get_navidrome_library_warning, migrate_legacy_folder_fn,
    remove_folder_root, sync_navidrome_libraries, MigrateLegacyResponseDto,
    NavidromeLibrarySyncReportDto, NavidromeLibraryWarningDto,
};
use dioxus::prelude::*;

use super::panel::{
    ListRow, LoadError, Loading, Messages, Panel, SectionTitle, COMPACT_PRIMARY_BUTTON_CLASS,
    PRIMARY_BUTTON_CLASS, REMOVE_BUTTON_CLASS, SECONDARY_BUTTON_CLASS,
};
use crate::auth::use_auth;
use crate::{friendly_error, ConfirmModal, FieldLook, TextInput};

fn folder_label(label: &FolderLabel) -> String {
    format!("{}'s folder {}", label.owner, label.name)
}

/// What the Delete button asks before it removes another user's folder entry.
fn delete_prompt(label: &FolderLabel) -> String {
    format!(
        "Delete {}? This removes only the folder entry, not its files.",
        folder_label(label)
    )
}

/// What the Migrate to Hardlinks button asks before deleting legacy source files.
fn migrate_remove_source_prompt(path: &str) -> String {
    format!(
        "Migrate {path} and remove the original legacy source files after verifying copies? This deletes the source files once hardlinks or copies are verified."
    )
}

/// A folder the admin asked to delete, waiting for confirmation.
#[derive(Clone, PartialEq)]
struct PendingDelete {
    id: String,
    label: FolderLabel,
}

/// One line describing a conflict, naming the folders and their owners.
pub fn conflict_text(conflict: &FolderConflict<FolderLabel>) -> String {
    let folder = folder_label(&conflict.folder);
    match &conflict.kind {
        ConflictKind::SameAsOther(other) => {
            format!("{folder} has the same path as {}", folder_label(other))
        }
        ConflictKind::ContainsOther(other) => format!("{folder} contains {}", folder_label(other)),
        ConflictKind::InsideOther(other) => format!("{folder} is inside {}", folder_label(other)),
        ConflictKind::OutsideApprovedRoots => format!("{folder} is not inside an approved root"),
        ConflictKind::Unresolvable { error } => format!("{folder} cannot be resolved: {error}"),
    }
}

/// Admin-only panel for approved folder roots, user libraries, folder conflicts,
/// Navidrome library isolation sync, and legacy folder migration.
#[component]
pub fn FolderAdminPanel() -> Element {
    let auth = use_auth();
    let mut view = use_resource(move || async move { auth.call(get_folder_admin()).await });
    let mut warning_resource =
        use_resource(move || async move { auth.call(get_navidrome_library_warning()).await });
    let mut legacy_resource =
        use_resource(move || async move { auth.call(get_legacy_folders()).await });

    let mut new_root = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut pending_delete = use_signal(|| None::<PendingDelete>);

    // Navidrome library isolation sync state
    let mut syncing_libraries = use_signal(|| false);
    let mut sync_error = use_signal(String::new);
    let mut sync_report = use_signal(|| None::<NavidromeLibrarySyncReportDto>);

    // Legacy folder migration state
    let mut remove_source_flags = use_signal(HashMap::<String, bool>::new);
    let mut migrating_path = use_signal(|| None::<String>);
    let mut legacy_error = use_signal(String::new);
    let mut legacy_reports = use_signal(HashMap::<String, MigrateLegacyResponseDto>::new);
    let mut pending_migrate_delete = use_signal(|| None::<String>);

    let run_migration = move |p: String, rm: bool| {
        if migrating_path.peek().is_some() {
            return;
        }
        let mut migrating_path = migrating_path;
        let mut legacy_error = legacy_error;
        let mut legacy_reports = legacy_reports;
        let mut legacy_resource = legacy_resource;
        spawn(async move {
            migrating_path.set(Some(p.clone()));
            legacy_error.set(String::new());
            match auth
                .call(migrate_legacy_folder_fn(p.clone(), rm, false))
                .await
            {
                Ok(resp) => {
                    legacy_reports.write().insert(p, resp);
                    legacy_resource.restart();
                }
                Err(e) => legacy_error.set(friendly_error(&e)),
            }
            migrating_path.set(None);
        });
    };

    // Every change reloads the view, so the lists and conflicts stay current.
    let mut finish = move |result: Result<(), ServerFnError>| {
        match result {
            Ok(()) => error.set(String::new()),
            Err(e) => error.set(friendly_error(&e)),
        }
        view.restart();
    };

    let active_warning: Option<NavidromeLibraryWarningDto> = sync_report
        .read()
        .as_ref()
        .and_then(|r| r.warning.clone())
        .or_else(|| {
            warning_resource
                .read()
                .as_ref()
                .and_then(|res| res.as_ref().ok())
                .and_then(|opt| opt.clone())
        });

    let legacy_groups = legacy_resource
        .read()
        .as_ref()
        .and_then(|res| res.as_ref().ok())
        .cloned()
        .unwrap_or_default();

    rsx! {
        Panel { title: "Folder Administration",
            Messages { error: error() }
            match &*view.read() {
                None => rsx! { Loading {} },
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| view.restart() }
                },
                Some(Ok(data)) => rsx! {
                    SectionTitle { title: "Approved Folder Roots" }
                    p { class: "text-xs text-beet-text-secondary mb-3 leading-relaxed",
                        "Users who are not admins can set their music library folder inside these roots."
                    }
                    if data.roots.is_empty() {
                        p { class: "text-xs text-beet-text-tertiary italic mb-4", "No approved roots yet." }
                    } else {
                        ul { class: "space-y-2 mb-4",
                            for root in data.roots.clone() {
                                ListRow {
                                    div { class: "flex justify-between items-center gap-3",
                                        span { class: "text-beet-text-primary text-xs tabular-nums-mono break-all", "{root}" }
                                        button {
                                            class: REMOVE_BUTTON_CLASS,
                                            onclick: move |_| {
                                                let root = root.clone();
                                                async move { finish(auth.call(remove_folder_root(root)).await) }
                                            },
                                            "Remove"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div { class: "flex gap-2.5 items-center mb-6",
                        div { class: "flex-1",
                            TextInput {
                                look: FieldLook::Compact,
                                value: new_root(),
                                oninput: move |v| new_root.set(v),
                                placeholder: "/music",
                                class: "tabular-nums-mono",
                            }
                        }
                        button {
                            r#type: "button",
                            class: COMPACT_PRIMARY_BUTTON_CLASS,
                            onclick: move |_| async move {
                                let result = auth.call(add_folder_root(new_root())).await;
                                if result.is_ok() {
                                    new_root.set(String::new());
                                }
                                finish(result);
                            },
                            "Add Root"
                        }
                    }

                    SectionTitle { title: "User Library Folders" }
                    if data.folders.is_empty() {
                        p { class: "text-xs text-beet-text-tertiary italic mb-6", "No user folders yet." }
                    } else {
                        ul { class: "space-y-2 mb-6",
                            for entry in data.folders.clone() {
                                ListRow {
                                    div { class: "flex justify-between items-center gap-3",
                                        div { class: "min-w-0",
                                            span { class: "font-semibold text-beet-text-primary block font-display text-sm", "{entry.folder.name}" }
                                            span { class: "text-beet-text-tertiary text-xs tabular-nums-mono block break-all", "{entry.folder.path}" }
                                            span { class: "text-beet-text-secondary text-xs", "Owner: {entry.owner}" }
                                        }
                                        div { class: "flex items-center gap-3 shrink-0",
                                            button {
                                                class: REMOVE_BUTTON_CLASS,
                                                onclick: {
                                                    let pending = PendingDelete {
                                                        id: entry.folder.id.clone(),
                                                        label: FolderLabel {
                                                            owner: entry.owner.clone(),
                                                            name: entry.folder.name.clone(),
                                                        },
                                                    };
                                                    move |_| pending_delete.set(Some(pending.clone()))
                                                },
                                                "Delete"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    SectionTitle { title: "Folder Conflicts" }
                    if data.conflicts.is_empty() {
                        p { class: "text-sm text-beet-text-secondary mb-6", "No folder conflicts." }
                    } else {
                        ul { class: "list-disc pl-5 space-y-1.5 text-sm text-beet-gold mb-3",
                            for conflict in data.conflicts.iter() {
                                li { "{conflict_text(conflict)}" }
                            }
                        }
                        p { class: "text-xs text-beet-text-tertiary leading-relaxed mb-6",
                            "Delete the conflicting folder entry above or have its owner move it to another path. Deleting removes only the folder entry, not its files."
                        }
                    }

                    // Navidrome Library Isolation
                    SectionTitle { title: "Navidrome Library Isolation" }
                    div { class: "space-y-3 mb-6",
                        p { class: "text-xs text-beet-text-secondary leading-relaxed",
                            "Soulbeet maps each user's personal folder to a dedicated Navidrome library so household libraries stay isolated and tracks can be shared safely with hardlinks."
                        }

                        if let Some(warn) = active_warning {
                            div { class: "p-3.5 rounded-[10px] bg-beet-gold-surface border border-beet-gold/40 text-beet-gold space-y-1.5 text-xs",
                                p { class: "font-semibold", "Navidrome default library overlaps user folders" }
                                p { class: "leading-relaxed", "{warn.message}" }
                                p { class: "tabular-nums-mono text-[11px] opacity-90 break-all",
                                    "Default Library 1 path: {warn.default_library_path}"
                                }
                                if !warn.overlapping_user_paths.is_empty() {
                                    p { class: "tabular-nums-mono text-[11px] opacity-90 break-all",
                                        "Overlapping user folders: {warn.overlapping_user_paths.join(\", \")}"
                                    }
                                }
                                p { class: "text-[11px] leading-relaxed pt-0.5",
                                    "To isolate users completely, point Navidrome's default root at an empty directory by setting "
                                    code { class: "px-1.5 py-0.5 rounded bg-beet-surface-low text-beet-text-primary tabular-nums-mono",
                                        "ND_MUSICFOLDER=/music/_default"
                                    }
                                    " in your Navidrome environment and re-running Sync Navidrome Libraries."
                                }
                            }
                        }

                        if !sync_error().is_empty() {
                            div { class: "px-3.5 py-2.5 bg-beet-crimson-surface border border-beet-crimson-solid/40 rounded-[10px] text-beet-crimson-text text-xs",
                                "{sync_error}"
                            }
                        }

                        div { class: "flex flex-wrap items-center gap-2.5",
                            button {
                                r#type: "button",
                                disabled: syncing_libraries(),
                                class: PRIMARY_BUTTON_CLASS,
                                onclick: move |_| async move {
                                    syncing_libraries.set(true);
                                    sync_error.set(String::new());
                                    match auth.call(sync_navidrome_libraries()).await {
                                        Ok(report) => {
                                            sync_report.set(Some(report));
                                            warning_resource.restart();
                                        }
                                        Err(e) => sync_error.set(friendly_error(&e)),
                                    }
                                    syncing_libraries.set(false);
                                },
                                if syncing_libraries() {
                                    "Syncing Libraries..."
                                } else {
                                    "Sync Navidrome Libraries"
                                }
                            }
                        }

                        if let Some(report) = sync_report.read().clone() {
                            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle overflow-hidden",
                                if !report.has_admin {
                                    p { class: "p-3.5 text-xs text-beet-gold",
                                        "No Navidrome admin credentials are configured. Connect a Navidrome admin account so Soulbeet can create and assign per-user libraries."
                                    }
                                } else if report.entries.is_empty() {
                                    p { class: "p-3.5 text-xs text-beet-text-tertiary",
                                        "No user library folders to sync yet."
                                    }
                                } else {
                                    div { class: "overflow-x-auto",
                                        table { class: "w-full text-left text-xs",
                                            thead { class: "bg-beet-surface-low border-b border-beet-border-subtle text-beet-text-tertiary uppercase tabular-nums-mono text-[10px]",
                                                tr {
                                                    th { class: "px-3 py-2 font-semibold", "User" }
                                                    th { class: "px-3 py-2 font-semibold", "Folder Path" }
                                                    th { class: "px-3 py-2 font-semibold", "Library ID" }
                                                    th { class: "px-3 py-2 font-semibold", "Status" }
                                                }
                                            }
                                            tbody { class: "divide-y divide-beet-border-subtle",
                                                for entry in report.entries.iter() {
                                                    tr { key: "sync-{entry.user_id}",
                                                        td { class: "px-3 py-2 font-medium text-beet-text-primary whitespace-nowrap",
                                                            "{entry.username}"
                                                        }
                                                        td { class: "px-3 py-2 tabular-nums-mono text-beet-text-secondary break-all",
                                                            "{entry.folder_path}"
                                                        }
                                                        td { class: "px-3 py-2 tabular-nums-mono text-beet-text-tertiary whitespace-nowrap",
                                                            match entry.navidrome_library_id {
                                                                Some(id) => format!("#{id}"),
                                                                None => "None".to_string(),
                                                            }
                                                        }
                                                        td { class: "px-3 py-2 text-beet-text-secondary",
                                                            "{entry.status}"
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

                    // Legacy Folder Migration (shown when unmigrated legacy folders exist)
                    if !legacy_groups.is_empty() || !legacy_reports.read().is_empty() {
                        SectionTitle { title: "Legacy Folder Migration" }
                        div { class: "space-y-3",
                            p { class: "text-xs text-beet-text-secondary leading-relaxed",
                                "These folders were previously shared or configured as extra folders before single-folder isolation. Migrate them to hardlink audio files into each user's personal library without duplicating disk space."
                            }
                            if !legacy_error().is_empty() {
                                div { class: "px-3.5 py-2.5 bg-beet-crimson-surface border border-beet-crimson-solid/40 rounded-[10px] text-beet-crimson-text text-xs",
                                    "{legacy_error}"
                                }
                            }

                            for group in legacy_groups.iter() {
                                {
                                    let group_path = group.path.clone();
                                    let path_for_check = group.path.clone();
                                    let path_for_preview = group.path.clone();
                                    let path_for_migrate = group.path.clone();
                                    let dismiss_ids: Vec<String> = group
                                        .recipients
                                        .iter()
                                        .map(|r| r.legacy_id.clone())
                                        .collect();
                                    let remove_source = remove_source_flags
                                        .read()
                                        .get(&group_path)
                                        .copied()
                                        .unwrap_or(false);
                                    let is_busy = migrating_path().as_deref() == Some(group_path.as_str());
                                    let any_busy = migrating_path().is_some();
                                    let report_opt = legacy_reports.read().get(&group_path).cloned();
                                    let (badge_cls, badge_text) = if group.was_shared {
                                        (
                                            "bg-beet-accent-surface border-beet-accent/35 text-beet-accent-text",
                                            "Shared",
                                        )
                                    } else {
                                        (
                                            "bg-beet-surface-high border-beet-border-default text-beet-text-secondary",
                                            "Extra",
                                        )
                                    };

                                    rsx! {
                                        div {
                                            key: "legacy-{group.path}",
                                            class: "p-4 rounded-[12px] bg-beet-surface border border-beet-border-subtle space-y-3",
                                            div { class: "flex flex-wrap items-start justify-between gap-2",
                                                div { class: "min-w-0 space-y-1",
                                                    div { class: "flex flex-wrap items-center gap-2",
                                                        span { class: "px-2 py-0.5 rounded-full border text-[10px] font-semibold uppercase tabular-nums-mono {badge_cls}",
                                                            "{badge_text}"
                                                        }
                                                        span { class: "text-xs font-semibold text-beet-text-primary tabular-nums-mono break-all",
                                                            "{group.path}"
                                                        }
                                                    }
                                                    if !group.recipients.is_empty() {
                                                        div { class: "text-xs text-beet-text-secondary space-y-0.5 pt-1",
                                                            for recip in group.recipients.iter() {
                                                                p { key: "recip-{recip.legacy_id}",
                                                                    span { class: "font-medium text-beet-text-primary", "{recip.username}" }
                                                                    span { class: "text-beet-text-tertiary", " -> " }
                                                                    span { class: "tabular-nums-mono text-[11px] break-all", "{recip.target_path}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            label { class: "flex items-center gap-2 text-xs text-beet-text-secondary cursor-pointer select-none",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "accent-beet-leaf-solid cursor-pointer",
                                                    checked: remove_source,
                                                    disabled: any_busy,
                                                    onchange: move |e: Event<FormData>| {
                                                        remove_source_flags
                                                            .write()
                                                            .insert(path_for_check.clone(), e.checked());
                                                    },
                                                }
                                                "Remove legacy source files after verifying copies"
                                            }

                                            div { class: "flex flex-wrap items-center gap-2",
                                                button {
                                                    r#type: "button",
                                                    disabled: any_busy,
                                                    class: SECONDARY_BUTTON_CLASS,
                                                    onclick: move |_| {
                                                        let p = path_for_preview.clone();
                                                        let rm = remove_source_flags.peek().get(&p).copied().unwrap_or(false);
                                                        async move {
                                                            migrating_path.set(Some(p.clone()));
                                                            legacy_error.set(String::new());
                                                            match auth.call(migrate_legacy_folder_fn(p.clone(), rm, true)).await {
                                                                Ok(resp) => {
                                                                    legacy_reports.write().insert(p, resp);
                                                                }
                                                                Err(e) => legacy_error.set(friendly_error(&e)),
                                                            }
                                                            migrating_path.set(None);
                                                        }
                                                    },
                                                    if is_busy { "Working..." } else { "Preview Migration" }
                                                }
                                                button {
                                                    r#type: "button",
                                                    disabled: any_busy,
                                                    class: "{PRIMARY_BUTTON_CLASS} h-9 text-xs",
                                                    onclick: move |_| {
                                                        let p = path_for_migrate.clone();
                                                        let rm = remove_source_flags.peek().get(&p).copied().unwrap_or(false);
                                                        if rm {
                                                            pending_migrate_delete.set(Some(p));
                                                        } else {
                                                            run_migration(p, false);
                                                        }
                                                    },
                                                    if is_busy { "Migrating..." } else { "Migrate to Hardlinks" }
                                                }
                                                button {
                                                    r#type: "button",
                                                    disabled: any_busy,
                                                    class: REMOVE_BUTTON_CLASS,
                                                    onclick: move |_| {
                                                        let ids = dismiss_ids.clone();
                                                        let p = group_path.clone();
                                                        async move {
                                                            migrating_path.set(Some(p.clone()));
                                                            legacy_error.set(String::new());
                                                            let mut dismissed_ok = true;
                                                            for id in ids {
                                                                if let Err(e) = auth.call(dismiss_legacy_folder(id)).await {
                                                                    legacy_error.set(friendly_error(&e));
                                                                    dismissed_ok = false;
                                                                    break;
                                                                }
                                                            }
                                                            if dismissed_ok {
                                                                legacy_reports.write().remove(&p);
                                                                remove_source_flags.write().remove(&p);
                                                            }
                                                            legacy_resource.restart();
                                                            migrating_path.set(None);
                                                        }
                                                    },
                                                    "Dismiss"
                                                }
                                            }

                                            if let Some(report) = report_opt {
                                                LegacyMigrationReportCard { report }
                                            }
                                        }
                                    }
                                }
                            }

                            // Show reports for paths that were just migrated and removed from the pending list
                            for (path, report) in legacy_reports.read().iter() {
                                if !legacy_groups.iter().any(|g| g.path == *path) {
                                    div {
                                        key: "legacy-done-{path}",
                                        class: "p-3.5 rounded-[12px] bg-beet-surface border border-beet-leaf-solid/30 space-y-2",
                                        p { class: "text-xs font-semibold text-beet-leaf-text tabular-nums-mono break-all",
                                            "Migrated: {path}"
                                        }
                                        LegacyMigrationReportCard { report: report.clone() }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
        if let Some(pending) = pending_delete() {
            ConfirmModal {
                message: delete_prompt(&pending.label),
                confirm_label: "Delete",
                danger: true,
                on_confirm: move |_| {
                    let id = pending.id.clone();
                    pending_delete.set(None);
                    spawn(async move { finish(auth.call(delete_any_folder(id)).await) });
                },
                on_cancel: move |_| pending_delete.set(None),
            }
        }
        if let Some(path) = pending_migrate_delete() {
            ConfirmModal {
                message: migrate_remove_source_prompt(&path),
                confirm_label: "Migrate & Remove Sources",
                danger: true,
                on_confirm: move |_| {
                    let p = path.clone();
                    pending_migrate_delete.set(None);
                    run_migration(p, true);
                },
                on_cancel: move |_| pending_migrate_delete.set(None),
            }
        }
    }
}

#[component]
fn LegacyMigrationReportCard(report: MigrateLegacyResponseDto) -> Element {
    let heading = if report.dry_run {
        "Migration Preview (no files changed)"
    } else {
        "Migration Complete"
    };
    let border_cls = if report.failed_count > 0 {
        "border-beet-gold/35 bg-beet-gold-surface/50"
    } else if report.dry_run {
        "border-beet-accent/30 bg-beet-accent-surface/40"
    } else {
        "border-beet-leaf-solid/30 bg-beet-leaf-surface/40"
    };

    rsx! {
        div { class: "p-3 rounded-[10px] border text-xs space-y-1.5 {border_cls}",
            p { class: "font-semibold text-beet-text-primary", "{heading}" }
            div { class: "flex flex-wrap gap-x-3 gap-y-1 tabular-nums-mono text-[11px] text-beet-text-secondary",
                span { "{report.total_audio_files} audio files" }
                span { "{report.linked_count} linked" }
                span { "{report.already_present_count} already present" }
                if report.failed_count > 0 {
                    span { class: "text-beet-crimson-text font-semibold", "{report.failed_count} failed" }
                }
                if report.hardlink_count > 0 {
                    span { "{report.hardlink_count} hardlink" }
                }
                if report.reflink_count > 0 {
                    span { "{report.reflink_count} reflink" }
                }
                if report.copy_count > 0 {
                    span { "{report.copy_count} copy" }
                }
                if report.annotations_restored > 0 {
                    span { "{report.annotations_restored} ratings/stars restored" }
                }
                if report.removed_source_count > 0 {
                    span { "{report.removed_source_count} source files removed" }
                }
            }
            if !report.warnings.is_empty() {
                ul { class: "list-disc pl-4 space-y-0.5 text-[11px] text-beet-gold",
                    for w in report.warnings.iter() {
                        li { "{w}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(owner: &str, name: &str) -> FolderLabel {
        FolderLabel {
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn conflict(kind: ConflictKind<FolderLabel>) -> FolderConflict<FolderLabel> {
        FolderConflict {
            folder: label("bob", "Bob"),
            kind,
        }
    }

    #[test]
    fn conflict_text_names_both_folders() {
        let c = conflict(ConflictKind::InsideOther(label("alice", "Main")));
        assert_eq!(
            conflict_text(&c),
            "bob's folder Bob is inside alice's folder Main"
        );
    }

    #[test]
    fn deleting_a_folder_asks_first_and_says_the_files_stay() {
        assert_eq!(
            delete_prompt(&label("bob", "Bob")),
            "Delete bob's folder Bob? This removes only the folder entry, not its files."
        );
    }

    #[test]
    fn migrating_with_source_removal_asks_first_and_names_the_path() {
        assert_eq!(
            migrate_remove_source_prompt("/music/shared"),
            "Migrate /music/shared and remove the original legacy source files after verifying copies? This deletes the source files once hardlinks or copies are verified."
        );
    }

    #[test]
    fn conflict_text_for_a_rootless_folder() {
        let c = conflict(ConflictKind::OutsideApprovedRoots);
        assert_eq!(
            conflict_text(&c),
            "bob's folder Bob is not inside an approved root"
        );
    }

    #[test]
    fn conflict_text_for_each_other_kind() {
        let same = conflict(ConflictKind::SameAsOther(label("alice", "Main")));
        assert_eq!(
            conflict_text(&same),
            "bob's folder Bob has the same path as alice's folder Main"
        );
        let contains = conflict(ConflictKind::ContainsOther(label("alice", "Main")));
        assert_eq!(
            conflict_text(&contains),
            "bob's folder Bob contains alice's folder Main"
        );
        let bad = conflict(ConflictKind::Unresolvable {
            error: "no such directory".into(),
        });
        assert_eq!(
            conflict_text(&bad),
            "bob's folder Bob cannot be resolved: no such directory"
        );
    }
}
