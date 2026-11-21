use api::auth::AuthResponse;
use dioxus::prelude::*;
use ui::{Auth, ChoosePassword};

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}

#[component]
pub fn AuthProvider(children: Element) -> Element {
    let auth_state =
        use_resource(move || async move { api::get_current_user().await.ok().flatten() });

    let mut auth_signal = use_signal(|| None::<AuthResponse>);
    let mut initialized = use_signal(|| false);

    // Nothing below renders until the server has answered: a page opened by
    // its address must not be taken for signed out and sent to the login.
    use_effect(move || {
        if let Some(user) = auth_state.read().clone() {
            auth_signal.set(user);
            initialized.set(true);
        }
    });

    let auth = use_context_provider(|| Auth::new(auth_signal));

    if !*initialized.read() {
        return rsx! {
            div { class: "flex flex-col items-center justify-center h-screen",
                div { class: "animate-spin rounded-full h-16 w-16 border-t-4 border-b-4 border-beet-accent mb-6" }
                h1 { class: "text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
                    "SoulBeet"
                }
            }
        };
    }

    // A session still on the default password can only choose a new one,
    // so nothing that would call the server for more is mounted.
    if auth.password_change_required() {
        return rsx! {
            ChoosePassword {}
        };
    }

    rsx! {
        {children}
    }
}
