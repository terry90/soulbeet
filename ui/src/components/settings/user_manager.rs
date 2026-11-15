use api::auth::Role;
use api::{
    create_user, delete_user, get_users, set_user_role, update_user_password, update_username,
};
use dioxus::prelude::*;

use crate::auth::use_auth;
use crate::components::friendly_error;
use crate::{FieldLook, TextInput};

const PANEL_CLASS: &str =
    "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10";
const LABEL_CLASS: &str = "block text-xs font-mono text-gray-400 mb-1 uppercase tracking-wider";
const LINK_BUTTON_CLASS: &str = "text-xs font-mono text-gray-400 hover:text-beet-accent transition-colors underline decoration-dotted cursor-pointer";

#[component]
fn Messages(error: String, success: String) -> Element {
    rsx! {
      if !error.is_empty() {
        div { class: "mb-4 p-4 bg-red-900/20 border border-red-500/50 rounded text-red-400 font-mono text-sm",
          "{error}"
        }
      }
      if !success.is_empty() {
        div { class: "mb-4 p-4 bg-green-900/20 border border-green-500/50 rounded text-green-400 font-mono text-sm",
          "{success}"
        }
      }
    }
}

/// The signed-in user's own username and password, for every role.
#[component]
pub fn AccountPanel() -> Element {
    let mut new_username = use_signal(|| "".to_string());
    let mut new_password = use_signal(|| "".to_string());
    let mut error = use_signal(|| "".to_string());
    let mut success_msg = use_signal(|| "".to_string());
    let auth = use_auth();

    let handle_rename = move |_| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        if new_username().trim().is_empty() {
            error.set("Username cannot be empty".to_string());
            return;
        }
        match auth.call(update_username(new_username())).await {
            Ok(_) => {
                success_msg.set("Username updated".to_string());
                new_username.set("".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_password = move |_| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        let Some(user_id) = auth.user_id() else {
            return;
        };
        if new_password().is_empty() {
            error.set("Password cannot be empty".to_string());
            return;
        }
        match auth
            .call(update_user_password(user_id, new_password()))
            .await
        {
            Ok(_) => {
                success_msg.set("Password updated".to_string());
                new_password.set("".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    rsx! {
      div { class: PANEL_CLASS,
        h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display",
          "Your Account"
        }
        Messages { error: error(), success: success_msg() }

        if auth.rename_allowed() {
          div { class: "mb-4",
            label { class: LABEL_CLASS, "Username" }
            TextInput {
              value: new_username(),
              oninput: move |v| new_username.set(v),
              placeholder: "New username",
            }
            button { class: "retro-btn mt-2 rounded", onclick: handle_rename, "Rename" }
          }
        }

        div {
          label { class: LABEL_CLASS, "Password" }
          TextInput {
            value: new_password(),
            oninput: move |v| new_password.set(v),
            placeholder: "New password",
            password: true,
          }
          button { class: "retro-btn mt-2 rounded", onclick: handle_password, "Change Password" }
        }
      }
    }
}

/// Account administration. The server refuses every call from a non-admin;
/// the Users tab only mounts this for admins.
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
            Err(e) => error.set(friendly_error(&e)),
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

        match auth.call(create_user(new_username(), new_password())).await {
            Ok(_) => {
                success_msg.set(format!("User '{}' created successfully", new_username()));
                new_username.set("".to_string());
                new_password.set("".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_delete_user = move |id: String| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        match auth.call(delete_user(id)).await {
            Ok(_) => {
                success_msg.set("User deleted successfully".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_set_role = move |id: String, role: Role| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        match auth.call(set_user_role(id, role)).await {
            Ok(_) => {
                success_msg.set("Role updated".to_string());
                fetch_users().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    let handle_update_password = move |id: String| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        if edit_user_password().is_empty() {
            error.set("Password cannot be empty".to_string());
            return;
        }
        match auth
            .call(update_user_password(id, edit_user_password()))
            .await
        {
            Ok(_) => {
                success_msg.set("Password updated".to_string());
                editing_user_id.set(None);
                edit_user_password.set("".to_string());
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    rsx! {
      div { class: PANEL_CLASS,
        h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display",
          "User Management"
        }
        Messages { error: error(), success: success_msg() }

        // Create User
        div { class: "grid grid-cols-1 md:grid-cols-2 gap-4 mb-4",
          div {
            label { class: LABEL_CLASS, "New Username" }
            TextInput {
              value: new_username(),
              oninput: move |v| new_username.set(v),
              placeholder: "Username",
            }
          }
          div {
            label { class: LABEL_CLASS, "New Password" }
            TextInput {
              value: new_password(),
              oninput: move |v| new_password.set(v),
              placeholder: "Password",
              password: true,
            }
          }
        }
        button {
          class: "retro-btn mb-6 rounded",
          onclick: handle_create_user,
          "Create User"
        }

        // User List
        h3 { class: "text-lg font-bold mb-2 text-white font-display border-b border-white/10 pb-2",
          "Existing Users"
        }
        if users.read().is_empty() {
          p { class: "text-gray-500 font-mono italic", "No users found." }
        } else {
          ul { class: "space-y-2",
            {
                users
                    .read()
                    .clone()
                    .into_iter()
                    .map(|user| {
                        let id_pw = user.id.clone();
                        let id_edit_pw = user.id.clone();
                        let id_role = user.id.clone();
                        let id_delete = user.id.clone();
                        let (role_label, other_role, role_action) = if user.role.is_admin() {
                            ("admin", Role::User, "Make user")
                        } else {
                            ("user", Role::Admin, "Make admin")
                        };
                        rsx! {
                          li { class: "bg-white/5 border border-white/5 p-3 rounded hover:border-beet-accent/30 transition-colors",
                            if editing_user_id() == Some(user.id.clone()) {
                              div { class: "flex flex-col gap-2",
                                div { class: "font-bold text-white font-display", "{user.username}" }
                                TextInput {
                                  look: FieldLook::Compact,
                                  value: edit_user_password(),
                                  oninput: move |v| edit_user_password.set(v),
                                  placeholder: "New password",
                                  password: true,
                                }
                                div { class: "flex gap-2 mt-1",
                                  button {
                                    class: "text-xs uppercase tracking-wider font-bold text-beet-leaf hover:text-white transition-colors cursor-pointer",
                                    onclick: move |_| handle_update_password(id_pw.clone()),
                                    "[ Save ]"
                                  }
                                  button {
                                    class: "text-xs uppercase tracking-wider font-bold text-gray-500 hover:text-white transition-colors cursor-pointer",
                                    onclick: move |_| editing_user_id.set(None),
                                    "[ Cancel ]"
                                  }
                                }
                              }
                            } else {
                              div { class: "flex justify-between items-center flex-wrap gap-2",
                                div { class: "flex items-center gap-3",
                                  span { class: "font-bold text-white font-display", "{user.username}" }
                                  span { class: "text-xs font-mono text-gray-500 uppercase tracking-wider",
                                    "{role_label}"
                                  }
                                }
                                div { class: "flex gap-3",
                                  button {
                                    class: LINK_BUTTON_CLASS,
                                    onclick: move |_| handle_set_role(id_role.clone(), other_role),
                                    "{role_action}"
                                  }
                                  button {
                                    class: LINK_BUTTON_CLASS,
                                    onclick: move |_| {
                                        editing_user_id.set(Some(id_edit_pw.clone()));
                                        edit_user_password.set("".to_string());
                                    },
                                    "Password"
                                  }
                                  button {
                                    class: "text-xs font-mono text-gray-400 hover:text-red-400 transition-colors underline decoration-dotted cursor-pointer",
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
