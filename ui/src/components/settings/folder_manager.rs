use api::{create_user_folder, delete_folder, get_user_folders, update_folder};
use dioxus::prelude::*;

use crate::auth::use_auth;

#[component]
pub fn FolderManager() -> Element {
    let mut folder_name = use_signal(|| "".to_string());
    let mut folder_path = use_signal(|| "".to_string());
    let mut folders = use_signal(Vec::new);

    let mut editing_folder_id = use_signal(|| None::<String>);
    let mut edit_folder_name = use_signal(|| "".to_string());
    let mut edit_folder_path = use_signal(|| "".to_string());

    let mut error = use_signal(|| "".to_string());
    let mut success_msg = use_signal(|| "".to_string());
    let auth = use_auth();

    let fetch_folders = move || async move {
        match auth.call(get_user_folders()).await {
            Ok(fetched_folders) => folders.set(fetched_folders),
            Err(e) => error.set(format!("Failed to fetch folders: {e}")),
        }
    };

    use_future(move || async move {
        fetch_folders().await;
    });

    let handle_add_folder = move |_| async move {
        error.set("".to_string());
        success_msg.set("".to_string());

        if folder_name().is_empty() || folder_path().is_empty() {
            error.set("Name and Path are required".to_string());
            return;
        }

        match auth
            .call(create_user_folder(folder_name(), folder_path()))
            .await
        {
            Ok(_) => {
                success_msg.set("Folder added successfully".to_string());
                folder_name.set("".to_string());
                folder_path.set("".to_string());
                fetch_folders().await;
            }
            Err(e) => error.set(format!("Failed to add folder: {e}")),
        }
    };

    let handle_delete_folder = move |id: String| async move {
        match auth.call(delete_folder(id)).await {
            Ok(_) => {
                success_msg.set("Folder deleted successfully".to_string());
                fetch_folders().await;
            }
            Err(e) => error.set(format!("Failed to delete folder: {e}")),
        }
    };

    let handle_update_folder = move |id: String| async move {
        match auth
            .call(update_folder(id, edit_folder_name(), edit_folder_path()))
            .await
        {
            Ok(_) => {
                success_msg.set("Folder updated successfully".to_string());
                editing_folder_id.set(None);
                fetch_folders().await;
            }
            Err(e) => error.set(format!("Failed to update folder: {e}")),
        }
    };

    rsx! {
        div { class: "bg-gray-800 p-6 rounded-lg shadow-lg mb-8",
            h2 { class: "text-xl font-semibold mb-4 text-indigo-300", "Manage Music Folders" }

            // Local Messages
            if !error().is_empty() {
                div { class: "mb-4 p-4 bg-red-900/50 border border-red-500 rounded text-red-200",
                    "{error}"
                }
            }
            if !success_msg().is_empty() {
                div { class: "mb-4 p-4 bg-green-900/50 border border-green-500 rounded text-green-200",
                    "{success_msg}"
                }
            }

            div { class: "grid grid-cols-1 md:grid-cols-2 gap-4 mb-4",
                div {
                    label { class: "block text-sm font-medium mb-1", "Folder Name (e.g., 'Music/Common')" }
                    input {
                        class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
                        value: "{folder_name}",
                        oninput: move |e| folder_name.set(e.value()),
                        placeholder: "My Music",
                        "type": "text",
                    }
                }
                div {
                    label { class: "block text-sm font-medium mb-1", "Folder Path" }
                    input {
                        class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
                        value: "{folder_path}",
                        oninput: move |e| folder_path.set(e.value()),
                        placeholder: "/home/user/Music",
                        "type": "text",
                    }
                }
            }

            button {
                class: "bg-teal-600 hover:bg-teal-700 text-white font-bold py-2 px-4 rounded transition-colors mb-6",
                onclick: handle_add_folder,
                "Add Folder"
            }

            // Existing Folders List
            h3 { class: "text-lg font-semibold mb-2 text-indigo-200", "Existing Folders" }
            if folders.read().is_empty() {
                p { class: "text-gray-400", "No folders added yet." }
            } else {
                ul { class: "space-y-2",
                    {
                        folders
                            .read()
                            .clone()
                            .into_iter()
                            .map(|folder| {
                                let id_edit = folder.id.clone();
                                let id_delete = folder.id.clone();
                                let id_update = folder.id.clone();
                                rsx! {
                                    li { class: "bg-gray-700 p-3 rounded",
                                        if editing_folder_id() == Some(folder.id.clone()) {
                                            div { class: "flex flex-col gap-2",
                                                input {
                                                    class: "p-2 rounded bg-gray-600 border border-gray-500 focus:border-teal-500",
                                                    value: "{edit_folder_name}",
                                                    oninput: move |e| edit_folder_name.set(e.value()),
                                                    placeholder: "Name",
                                                }
                                                input {
                                                    class: "p-2 rounded bg-gray-600 border border-gray-500 focus:border-teal-500",
                                                    value: "{edit_folder_path}",
                                                    oninput: move |e| edit_folder_path.set(e.value()),
                                                    placeholder: "Path",
                                                }
                                                div { class: "flex gap-2 mt-2",
                                                    button {
                                                        class: "bg-green-600 hover:bg-green-700 text-white px-3 py-1 rounded text-sm",
                                                        onclick: move |_| handle_update_folder(id_update.clone()),
                                                        "Save"
                                                    }
                                                    button {
                                                        class: "bg-gray-500 hover:bg-gray-600 text-white px-3 py-1 rounded text-sm",
                                                        onclick: move |_| editing_folder_id.set(None),
                                                        "Cancel"
                                                    }
                                                }
                                            }
                                        } else {
                                            div { class: "flex justify-between items-center",
                                                div {
                                                    span { class: "font-medium text-teal-200 block", "{folder.name}" }
                                                    span { class: "text-gray-400 text-sm", "{folder.path}" }
                                                }
                                                div { class: "flex gap-2",
                                                    button {
                                                        class: "text-blue-400 hover:text-blue-300",
                                                        onclick: move |_| {
                                                            edit_folder_name.set(folder.name.clone());
                                                            edit_folder_path.set(folder.path.clone());
                                                            editing_folder_id.set(Some(id_edit.clone()));
                                                        },
                                                        "Edit"
                                                    }
                                                    button {
                                                        class: "text-red-400 hover:text-red-300",
                                                        onclick: move |_| handle_delete_folder(id_delete.clone()),
                                                        "Delete"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            })
                    }
                }
            }
        }
    }
}
