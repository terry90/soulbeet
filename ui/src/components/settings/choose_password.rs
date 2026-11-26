//! The only screen of a session whose account still has the default
//! password: it chooses a new one, or signs out.

use api::auth::PasswordProof;
use dioxus::prelude::*;

use super::panel::{Messages, Panel};
use super::sign_in_methods::PasswordSection;
use crate::auth::use_auth;
use crate::components::friendly_error;

/// Shown in place of the app while the signed-in account still has the
/// default password: the server refuses that session everything else. Once
/// a new password is chosen, the app opens at the address the browser is on.
#[component]
pub fn ChoosePassword() -> Element {
    let mut auth = use_auth();
    let mut sign_out_error = use_signal(String::new);

    let sign_out = move |_| async move {
        sign_out_error.set(String::new());
        if let Err(e) = auth.logout().await {
            sign_out_error.set(format!(
                "Sign-out failed, you are still signed in. {}",
                friendly_error(&e)
            ));
        }
    };

    rsx! {
      div { class: "flex flex-col items-center justify-center min-h-screen px-4 bg-beet-canvas text-beet-text-primary font-display",
        div { class: "w-full max-w-[420px]",
          Panel { title: "Choose a new password",
            p { class: "text-sm text-beet-text-secondary leading-relaxed mb-5",
              "This account still has the default password, which anyone can guess. Choose a new one to continue."
            }
            PasswordSection {
              proof: PasswordProof::Password,
              on_change: move |_| auth.password_chosen(),
            }
            Messages { error: sign_out_error() }
            button {
              class: "mt-6 text-xs font-medium text-beet-text-tertiary hover:text-beet-crimson-text transition-colors underline decoration-dotted underline-offset-4 cursor-pointer",
              onclick: sign_out,
              "Sign out"
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
    fn the_screen_asks_for_the_current_password_and_a_new_one() {
        let mut dom = VirtualDom::new(|| {
            let signed_in = use_signal(|| {
                Some(api::auth::AuthResponse {
                    username: "admin".into(),
                    user_id: "u1".into(),
                    navidrome_status: Default::default(),
                    role: api::auth::Role::Admin,
                    rename_allowed: true,
                    password_change_required: true,
                })
            });
            use_context_provider(|| crate::auth::Auth::new(signed_in));
            rsx! {
                ChoosePassword {}
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(">Choose a new password<"), "{html}");
        assert!(html.contains("still has the default password"), "{html}");
        assert!(html.contains("placeholder=\"Current password\""), "{html}");
        assert!(html.contains("aria-label=\"New password\""), "{html}");
        assert!(html.contains(">Change Password<"), "{html}");
        assert!(html.contains(">Sign out<"), "{html}");
    }
}
