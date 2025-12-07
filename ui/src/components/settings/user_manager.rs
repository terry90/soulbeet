use api::{delete_user, get_users, register, update_user_password};
use dioxus::prelude::*;

use crate::auth::use_auth;

#[component]
pub fn UserManager() -> Element {
    let mut new_username = use_signal(|| "".to_string());
    let mut new_password = use_signal(|| "".to_string());
    let mut users = use_signal(Vec::new);

    let mut editing_user_id = use_signal(|| None::<String>);
    let mut edit_user_password = use_signal(|| "".to_string());

    let mut error = use_signal(|| "".to_string());
    let mut success_msg = use_signal(|| "".to_string());
    let auth = use_auth();

    let fetch_users = move || async move {
        match auth.call(get_users()).await {
            Ok(fetched_users) => users.set(fetched_users),
            Err(e) => error.set(format!("Failed to fetch users: {e}")),
        }
    };

    use_future(move || async move {
        fetch_users().await;
    });

    let handle_create_user = move |_| async move {
        error.set("".to_string());
        success_msg.set("".to_string());

        if new_username().is_empty() || new_password().is_empty() {
            error.set("Username and Password are required".to_string());
            return;
        }

        match auth.call(register(new_username(), new_password())).await {
            Ok(_) => {
                success_msg.set(format!("User '{}' created successfully", new_username()));
                new_username.set("".to_string());
                new_password.set("".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(format!("Failed to create user: {e}")),
        }
    };

    let handle_delete_user = move |id: String| async move {
        match auth.call(delete_user(id)).await {
            Ok(_) => {
                success_msg.set("User deleted successfully".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(format!("Failed to delete user: {e}")),
        }
    };

    let handle_update_user = move |id: String| async move {
        if edit_user_password().is_empty() {
            error.set("Password cannot be empty".to_string());
            return;
        }
        match auth
            .call(update_user_password(id, edit_user_password()))
            .await
        {
            Ok(_) => {
                success_msg.set("User updated successfully".to_string());
                editing_user_id.set(None);
                edit_user_password.set("".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(format!("Failed to update user: {e}")),
        }
    };

    rsx! {
      div { class: "bg-gray-800 p-6 rounded-lg shadow-lg",
        h2 { class: "text-xl font-semibold mb-4 text-indigo-300", "User Management" }

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

        // Create User
        div { class: "grid grid-cols-1 md:grid-cols-2 gap-4 mb-4",
          div {
            label { class: "block text-sm font-medium mb-1", "New Username" }
            input {
              class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
              value: "{new_username}",
              oninput: move |e| new_username.set(e.value()),
              placeholder: "Username",
              "type": "text",
            }
          }
          div {
            label { class: "block text-sm font-medium mb-1", "New Password" }
            input {
              class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
              value: "{new_password}",
              oninput: move |e| new_password.set(e.value()),
              placeholder: "Password",
              "type": "password",
            }
          }
        }
        button {
          class: "bg-indigo-600 hover:bg-indigo-700 text-white font-bold py-2 px-4 rounded transition-colors mb-6",
          onclick: handle_create_user,
          "Create User"
        }

        // User List
        h3 { class: "text-lg font-semibold mb-2 text-indigo-200", "Existing Users" }
        if users.read().is_empty() {
          p { class: "text-gray-400", "No users found." }
        } else {
          ul { class: "space-y-2",
            {
                users
                    .read()
                    .clone()
                    .into_iter()
                    .map(|user| {
                        let id_update = user.id.clone();
                        let id_edit = user.id.clone();
                        let id_delete = user.id.clone();
                        rsx! {
                          li { class: "bg-gray-700 p-3 rounded",
                            if editing_user_id() == Some(user.id.clone()) {
                              div { class: "flex flex-col gap-2",
                                div { class: "font-medium text-teal-200", "{user.username}" }
                                input {
                                  class: "p-2 rounded bg-gray-600 border border-gray-500 focus:border-teal-500",
                                  value: "{edit_user_password}",
                                  oninput: move |e| edit_user_password.set(e.value()),
                                  placeholder: "New Password",
                                  "type": "password",
                                }
                                div { class: "flex gap-2 mt-2",
                                  button {
                                    class: "bg-green-600 hover:bg-green-700 text-white px-3 py-1 rounded text-sm",
                                    onclick: move |_| handle_update_user(id_update.clone()),
                                    "Save"
                                  }
                                  button {
                                    class: "bg-gray-500 hover:bg-gray-600 text-white px-3 py-1 rounded text-sm",
                                    onclick: move |_| editing_user_id.set(None),
                                    "Cancel"
                                  }
                                }
                              }
                            } else {
                              div { class: "flex justify-between items-center",
                                span { class: "font-medium text-teal-200", "{user.username}" }
                                div { class: "flex gap-2",
                                  button {
                                    class: "text-blue-400 hover:text-blue-300",
                                    onclick: move |_| {
                                        editing_user_id.set(Some(id_edit.clone()));
                                        edit_user_password.set("".to_string());
                                    },
                                    "Change Password"
                                  }
                                  button {
                                    class: "text-red-400 hover:text-red-300",
                                    onclick: move |_| handle_delete_user(id_delete.clone()),
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
