use crate::use_auth;
use dioxus::prelude::*;
use shared::system::SystemHealth;

/// Health context that provides live backend availability throughout the app.
/// Polled in one place so components can react to it without running their own loops.
#[derive(Clone, Copy, Debug)]
pub struct Health {
    state: Signal<SystemHealth>,
}

impl Health {
    /// Get the latest polled health snapshot.
    pub fn get(&self) -> SystemHealth {
        self.state.read().clone()
    }

    pub fn navidrome_online(&self) -> bool {
        self.state.read().navidrome_online
    }
}

/// Hook to access the health context.
pub fn use_system_health() -> Health {
    use_context::<Health>()
}

/// Provider component that polls system health while a user is logged in.
#[component]
pub fn HealthProvider(children: Element) -> Element {
    let auth = use_auth();
    let mut state = use_signal(SystemHealth::default);

    let mut poll = use_future(move || async move {
        loop {
            if auth.is_logged_in() {
                if let Ok(health) = auth.call(api::get_system_health()).await {
                    state.set(health);
                }
            }
            gloo_timers::future::TimeoutFuture::new(10_000).await;
        }
    });

    // The loop only checks login state when it wakes; restart it on login so
    // fresh health arrives immediately instead of after a stale sleep window.
    use_effect(move || {
        if auth.is_logged_in() {
            poll.restart();
        } else {
            state.set(SystemHealth::default());
        }
    });

    use_context_provider(|| Health { state });

    rsx! { {children} }
}
