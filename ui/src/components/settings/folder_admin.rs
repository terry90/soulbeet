use api::folder_policy::{ConflictKind, FolderAdminView, FolderConflict};
use api::{add_folder_root, get_folder_admin, remove_folder_root, set_folder_shared};
use dioxus::prelude::*;

use crate::auth::use_auth;
use crate::{friendly_error, FieldLook, TextInput};

fn folder_label(view: &FolderAdminView, folder_id: &str) -> String {
    match view.folders.iter().find(|e| e.folder.id == folder_id) {
        Some(e) => format!("{}'s folder {}", e.owner, e.folder.name),
        None => format!("folder {folder_id}"),
    }
}

/// One line describing a conflict, naming the folders and their owners.
pub fn conflict_text(conflict: &FolderConflict, view: &FolderAdminView) -> String {
    let folder = folder_label(view, &conflict.folder_id);
    let other = conflict
        .with_folder_id
        .as_deref()
        .map(|id| folder_label(view, id))
        .unwrap_or_default();
    match &conflict.kind {
        ConflictKind::SameAsOther => format!("{folder} has the same path as {other}"),
        ConflictKind::ContainsOther => format!("{folder} contains {other}"),
        ConflictKind::InsideOther => format!("{folder} is inside {other}"),
        ConflictKind::OutsideApprovedRoots => format!("{folder} is not under an approved root"),
        ConflictKind::Unresolvable { error } => format!("{folder} cannot be resolved: {error}"),
    }
}

/// Admin-only panel for approved folder roots, shared folders and folder conflicts.
#[component]
pub fn FolderAdminPanel() -> Element {
    let auth = use_auth();
    let mut view = use_resource(move || async move { auth.call(get_folder_admin()).await });
    let mut new_root = use_signal(String::new);
    let mut error = use_signal(String::new);

    rsx! {
        div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
            h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "Folder Administration" }
            if !error().is_empty() {
                div { class: "mb-4 p-4 bg-red-900/20 border border-red-500/50 rounded text-red-400 font-mono text-sm",
                    "{error}"
                }
            }
            match &*view.read() {
                None => rsx! {
                    div { class: "animate-pulse text-gray-400 font-mono", "Loading..." }
                },
                Some(Err(e)) => rsx! {
                    div { class: "text-red-400 text-sm font-mono mb-3", "{friendly_error(e)}" }
                    button {
                        class: "text-xs font-mono text-gray-400 hover:text-white underline decoration-dotted cursor-pointer",
                        onclick: move |_| view.restart(),
                        "Retry"
                    }
                },
                Some(Ok(data)) => rsx! {
                    h3 { class: "text-lg font-bold mb-2 text-white font-display border-b border-white/10 pb-2",
                        "Approved Folder Roots"
                    }
                    if data.roots.is_empty() {
                        p { class: "text-gray-500 font-mono italic mb-4", "No approved roots yet." }
                    } else {
                        ul { class: "space-y-2 mb-4",
                            for root in data.roots.clone() {
                                li { class: "flex justify-between items-center bg-white/5 border border-white/5 p-3 rounded",
                                    span { class: "text-gray-300 text-xs font-mono", "{root}" }
                                    button {
                                        class: "text-xs font-mono text-gray-400 hover:text-red-400 transition-colors underline decoration-dotted",
                                        onclick: move |_| {
                                            let root = root.clone();
                                            async move {
                                                match auth.call(remove_folder_root(root)).await {
                                                    Ok(()) => {
                                                        error.set(String::new());
                                                        view.restart();
                                                    }
                                                    Err(e) => error.set(friendly_error(&e)),
                                                }
                                            }
                                        },
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                    div { class: "flex gap-2 items-center mb-6",
                        div { class: "flex-1",
                            TextInput {
                                look: FieldLook::Compact,
                                value: new_root(),
                                oninput: move |v| new_root.set(v),
                                placeholder: "/music",
                            }
                        }
                        button {
                            class: "retro-btn rounded",
                            onclick: move |_| async move {
                                match auth.call(add_folder_root(new_root())).await {
                                    Ok(()) => {
                                        error.set(String::new());
                                        new_root.set(String::new());
                                        view.restart();
                                    }
                                    Err(e) => error.set(friendly_error(&e)),
                                }
                            },
                            "Add Root"
                        }
                    }

                    h3 { class: "text-lg font-bold mb-2 text-white font-display border-b border-white/10 pb-2",
                        "All Folders"
                    }
                    if data.folders.is_empty() {
                        p { class: "text-gray-500 font-mono italic mb-6", "No folders yet." }
                    } else {
                        ul { class: "space-y-2 mb-6",
                            for entry in data.folders.clone() {
                                li { class: "flex justify-between items-center bg-white/5 border border-white/5 p-3 rounded",
                                    div {
                                        span { class: "font-bold text-white block font-display", "{entry.folder.name}" }
                                        span { class: "text-gray-500 text-xs font-mono block", "{entry.folder.path}" }
                                        span { class: "text-gray-400 text-xs font-mono", "Owner: {entry.owner}" }
                                    }
                                    label { class: "flex items-center gap-2 text-xs font-mono text-gray-300 cursor-pointer",
                                        input {
                                            r#type: "checkbox",
                                            checked: entry.folder.shared,
                                            onchange: move |e| {
                                                let id = entry.folder.id.clone();
                                                async move {
                                                    match auth.call(set_folder_shared(id, e.checked())).await {
                                                        Ok(()) => error.set(String::new()),
                                                        Err(err) => error.set(friendly_error(&err)),
                                                    }
                                                    view.restart();
                                                }
                                            },
                                        }
                                        "Shared"
                                    }
                                }
                            }
                        }
                    }

                    h3 { class: "text-lg font-bold mb-2 text-white font-display border-b border-white/10 pb-2",
                        "Folder Conflicts"
                    }
                    if data.conflicts.is_empty() {
                        p { class: "text-sm font-mono text-gray-300", "No folder conflicts." }
                    } else {
                        ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400 mb-3",
                            for conflict in data.conflicts.iter() {
                                li { "{conflict_text(conflict, data)}" }
                            }
                        }
                        p { class: "text-xs font-mono text-gray-400",
                            "Mark one of the folders shared, or change a path."
                        }
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::folder_policy::FolderAdminEntry;
    use api::models::folder::Folder;

    fn entry(id: &str, owner: &str, name: &str) -> FolderAdminEntry {
        FolderAdminEntry {
            folder: Folder {
                id: id.into(),
                user_id: format!("{owner}-id"),
                name: name.into(),
                path: format!("/music/{name}"),
                shared: false,
            },
            owner: owner.into(),
            owner_is_admin: false,
        }
    }

    fn view() -> FolderAdminView {
        FolderAdminView {
            roots: vec![],
            folders: vec![entry("a", "alice", "Main"), entry("b", "bob", "Bob")],
            conflicts: vec![],
        }
    }

    fn conflict(kind: ConflictKind, with: Option<&str>) -> FolderConflict {
        FolderConflict {
            folder_id: "b".into(),
            kind,
            with_folder_id: with.map(Into::into),
        }
    }

    #[test]
    fn conflict_text_names_both_folders() {
        let c = conflict(ConflictKind::InsideOther, Some("a"));
        assert_eq!(
            conflict_text(&c, &view()),
            "bob's folder Bob is inside alice's folder Main"
        );
    }

    #[test]
    fn conflict_text_for_a_rootless_folder() {
        let c = conflict(ConflictKind::OutsideApprovedRoots, None);
        assert_eq!(
            conflict_text(&c, &view()),
            "bob's folder Bob is not under an approved root"
        );
    }

    #[test]
    fn conflict_text_for_each_other_kind() {
        let v = view();
        let same = conflict(ConflictKind::SameAsOther, Some("a"));
        assert_eq!(
            conflict_text(&same, &v),
            "bob's folder Bob has the same path as alice's folder Main"
        );
        let contains = conflict(ConflictKind::ContainsOther, Some("a"));
        assert_eq!(
            conflict_text(&contains, &v),
            "bob's folder Bob contains alice's folder Main"
        );
        let bad = conflict(
            ConflictKind::Unresolvable {
                error: "no such directory".into(),
            },
            None,
        );
        assert_eq!(
            conflict_text(&bad, &v),
            "bob's folder Bob cannot be resolved: no such directory"
        );
    }
}
