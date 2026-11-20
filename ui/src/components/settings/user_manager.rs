use api::auth::{Role, SsoLinkResult};
use api::models::user::UserSummary;
use api::{
    create_user, delete_user, get_sign_in_methods, get_users, set_user_password, set_user_role,
    unlink_sso, update_username,
};
use dioxus::prelude::*;

use super::panel::{
    ListRow, LoadError, Messages, Panel, SectionTitle, LABEL_CLASS, REMOVE_BUTTON_CLASS,
};
use super::sign_in_methods::{NavidromeSection, PasswordSection, SingleSignOnSection};
use crate::auth::use_auth;
use crate::components::friendly_error;
use crate::{FieldLook, TextInput};

const LINK_BUTTON_CLASS: &str = "text-xs font-mono text-gray-400 hover:text-beet-accent transition-colors underline decoration-dotted cursor-pointer";
const TAG_CLASS: &str = "text-xs font-mono text-gray-500 uppercase tracking-wider";

/// How an account signs in, when it is not just with a password: the
/// provider's name for its single sign-on identity, and a missing password.
fn sign_in_tags(user: &UserSummary) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(name) = &user.sso_name {
        tags.push(format!("single sign-on: {name}"));
    }
    if !user.has_password {
        tags.push("no password".to_string());
    }
    tags
}

/// What an admin's new password for the account `user` does besides.
fn reset_note(user: &UserSummary) -> Option<&'static str> {
    user.sso_name
        .is_some()
        .then_some("Setting a password also removes this account's single sign-on link.")
}

/// The signed-in user's own username, password and other ways to sign in,
/// for every role. `sso_notice` holds how a single sign-on link that just
/// came back from the provider ended, until it is shown.
#[component]
pub fn AccountPanel(sso_notice: Signal<Option<SsoLinkResult>>) -> Element {
    let auth = use_auth();
    let mut sign_in_methods =
        use_resource(move || async move { auth.call(get_sign_in_methods()).await });
    let mut new_username = use_signal(|| "".to_string());
    let mut error = use_signal(|| "".to_string());
    let mut success_msg = use_signal(|| "".to_string());

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

    rsx! {
      Panel { title: "Your Account",
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

        match &*sign_in_methods.read() {
          Some(Ok(methods)) => rsx! {
            PasswordSection {
              proof: methods.password_proof,
              // A password of its own may let the account unlink single sign-on.
              on_change: move |_| sign_in_methods.restart(),
            }
            SingleSignOnSection {
              methods: methods.clone(),
              notice: sso_notice,
              on_change: move |_| sign_in_methods.restart(),
            }
            if methods.navidrome {
              NavidromeSection { on_change: move |_| sign_in_methods.restart() }
            }
          },
          Some(Err(e)) => rsx! {
            div { class: "mt-6",
              LoadError {
                message: friendly_error(e),
                on_retry: move |_| sign_in_methods.restart(),
              }
            }
          },
          None => rsx! {},
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

    let handle_unlink_sso = move |id: String| async move {
        error.set("".to_string());
        success_msg.set("".to_string());
        match auth.call(unlink_sso(id)).await {
            Ok(_) => {
                success_msg.set("Single sign-on unlinked".to_string());
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
        match auth.call(set_user_password(id, edit_user_password())).await {
            Ok(_) => {
                success_msg.set("Password updated".to_string());
                editing_user_id.set(None);
                edit_user_password.set("".to_string());
                // It may have removed a single sign-on link.
                fetch_users().await;
            }
            Err(e) => error.set(friendly_error(&e)),
        }
    };

    rsx! {
      Panel { title: "User Management",
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
        SectionTitle { title: "Existing Users" }
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
                        let id_unlink = user.id.clone();
                        let tags = sign_in_tags(&user);
                        let reset_note = reset_note(&user);
                        // Their own takes the current one, under Your Account.
                        let own = auth.user_id().as_deref() == Some(user.id.as_str());
                        let (role_label, other_role, role_action) = if user.role.is_admin() {
                            ("admin", Role::User, "Make user")
                        } else {
                            ("user", Role::Admin, "Make admin")
                        };
                        rsx! {
                          ListRow {
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
                                if let Some(note) = reset_note {
                                  p { class: "text-xs text-gray-500 font-mono", "{note}" }
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
                                div { class: "flex items-center flex-wrap gap-x-3 gap-y-1",
                                  span { class: "font-bold text-white font-display", "{user.username}" }
                                  span { class: TAG_CLASS, "{role_label}" }
                                  for tag in tags {
                                    span { class: TAG_CLASS, "{tag}" }
                                  }
                                }
                                div { class: "flex flex-wrap gap-3",
                                  button {
                                    class: LINK_BUTTON_CLASS,
                                    onclick: move |_| handle_set_role(id_role.clone(), other_role),
                                    "{role_action}"
                                  }
                                  if !own {
                                    button {
                                      class: LINK_BUTTON_CLASS,
                                      onclick: move |_| {
                                          editing_user_id.set(Some(id_edit_pw.clone()));
                                          edit_user_password.set("".to_string());
                                      },
                                      "Password"
                                    }
                                  }
                                  if user.sso_name.is_some() {
                                    button {
                                      class: LINK_BUTTON_CLASS,
                                      onclick: move |_| handle_unlink_sso(id_unlink.clone()),
                                      "Unlink SSO"
                                    }
                                  }
                                  button {
                                    class: REMOVE_BUTTON_CLASS,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(has_password: bool, sso_name: Option<&str>) -> UserSummary {
        UserSummary {
            id: "u".into(),
            username: "ann".into(),
            role: Role::User,
            has_password,
            sso_name: sso_name.map(str::to_string),
        }
    }

    #[test]
    fn a_password_for_another_linked_account_says_it_removes_the_link() {
        assert!(reset_note(&summary(true, Some("ann@id")))
            .is_some_and(|note| note.contains("removes this account's single sign-on link")));
        assert_eq!(reset_note(&summary(true, None)), None);
    }

    #[test]
    fn the_list_tags_the_linked_identity_and_missing_passwords() {
        assert!(sign_in_tags(&summary(true, None)).is_empty());
        assert_eq!(
            sign_in_tags(&summary(true, Some("ann@id"))),
            ["single sign-on: ann@id"]
        );
        assert_eq!(
            sign_in_tags(&summary(false, Some("ann@id"))),
            ["single sign-on: ann@id", "no password"]
        );
    }
}
