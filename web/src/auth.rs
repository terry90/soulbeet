use api::auth::AuthResponse;
use dioxus::prelude::*;
use ui::Auth;

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}

#[component]
pub fn AuthProvider(children: Element) -> Element {
    let auth_state =
        use_resource(move || async move { api::get_current_user().await.ok().flatten() });

    let mut auth_signal = use_signal(|| None::<AuthResponse>);
    let mut initialized = use_signal(|| false);

    use_effect(move || {
        let user = auth_state.read().clone().flatten();
        auth_signal.set(user);
        initialized.set(true);
    });

    use_context_provider(|| Auth::new(auth_signal));

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

    rsx! {
        {children}
    }
}
