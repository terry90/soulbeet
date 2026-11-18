use api::folder_policy::{ConflictKind, FolderConflict, FolderLabel};
use api::{
    add_folder_root, delete_any_folder, get_folder_admin, remove_folder_root, set_folder_shared,
};
use dioxus::prelude::*;

use super::panel::{
    ListRow, LoadError, Loading, Messages, Panel, SectionTitle, REMOVE_BUTTON_CLASS,
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

/// Admin-only panel for approved folder roots, shared folders and folder conflicts.
#[component]
pub fn FolderAdminPanel() -> Element {
    let auth = use_auth();
    let mut view = use_resource(move || async move { auth.call(get_folder_admin()).await });
    let mut new_root = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut pending_delete = use_signal(|| None::<PendingDelete>);

    // Every change reloads the view, so the lists and conflicts stay current.
    let mut finish = move |result: Result<(), ServerFnError>| {
        match result {
            Ok(()) => error.set(String::new()),
            Err(e) => error.set(friendly_error(&e)),
        }
        view.restart();
    };

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
                    p { class: "text-xs font-mono text-gray-400 mb-2",
                        "Users who are not admins can add folders inside these roots."
                    }
                    if data.roots.is_empty() {
                        p { class: "text-gray-500 font-mono italic mb-4", "No approved roots yet." }
                    } else {
                        ul { class: "space-y-2 mb-4",
                            for root in data.roots.clone() {
                                ListRow {
                                    div { class: "flex justify-between items-center",
                                        span { class: "text-gray-300 text-xs font-mono", "{root}" }
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
                                let result = auth.call(add_folder_root(new_root())).await;
                                if result.is_ok() {
                                    new_root.set(String::new());
                                }
                                finish(result);
                            },
                            "Add Root"
                        }
                    }

                    SectionTitle { title: "All Folders" }
                    if data.folders.is_empty() {
                        p { class: "text-gray-500 font-mono italic mb-6", "No folders yet." }
                    } else {
                        ul { class: "space-y-2 mb-6",
                            for entry in data.folders.clone() {
                                ListRow {
                                    div { class: "flex justify-between items-center gap-3",
                                        div {
                                            span { class: "font-bold text-white block font-display", "{entry.folder.name}" }
                                            span { class: "text-gray-500 text-xs font-mono block", "{entry.folder.path}" }
                                            span { class: "text-gray-400 text-xs font-mono", "Owner: {entry.owner}" }
                                        }
                                        div { class: "flex items-center gap-3",
                                            label { class: "flex items-center gap-2 text-xs font-mono text-gray-300 cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    checked: entry.folder.shared,
                                                    onchange: {
                                                        let id = entry.folder.id.clone();
                                                        move |e: Event<FormData>| {
                                                            let id = id.clone();
                                                            async move { finish(auth.call(set_folder_shared(id, e.checked())).await) }
                                                        }
                                                    },
                                                }
                                                "Shared"
                                            }
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
                        p { class: "text-sm font-mono text-gray-300", "No folder conflicts." }
                    } else {
                        ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400 mb-3",
                            for conflict in data.conflicts.iter() {
                                li { "{conflict_text(conflict)}" }
                            }
                        }
                        p { class: "text-xs font-mono text-gray-400",
                            "Mark a folder shared or delete it in the list above. Deleting removes only the folder entry, not its files. Its owner can also move it to another path."
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
