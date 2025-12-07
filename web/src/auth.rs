use dioxus::prelude::*;
use ui::Auth;

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}

#[component]
pub fn AuthProvider(children: Element) -> Element {
    let auth_state =
        use_resource(move || async move { api::get_current_user().await.ok().flatten() });

    use_context_provider(|| Auth::new(Signal::new(auth_state.read().clone().flatten())));

    rsx! {
        {children}
    }
}
