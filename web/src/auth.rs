use api::refresh_token;
use dioxus::logger::tracing;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use ui::Auth;

use crate::storage::Storage;

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}

#[component]
pub fn AuthProvider(children: Element) -> Element {
    let mut auth_state = use_signal(Storage::get);

    use_context_provider(|| Auth::new(auth_state));

    // Reactive refresh loop: runs whenever auth_state changes
    use_effect(move || {
        let current_auth = auth_state.read().clone();

        if let Some(auth) = current_auth {
            spawn(async move {
                let delay_ms = calculate_refresh_delay(auth.expires_at);

                TimeoutFuture::new(delay_ms.max(30 * 1000)).await;

                // Verify we are still logged in and the token hasn't changed mid-wait
                if auth_state.read().as_ref() != Some(&auth) {
                    return;
                }

                match refresh_token(auth.refresh_token).await {
                    Ok(new_auth) => {
                        Storage::set(&new_auth);
                        auth_state.set(Some(new_auth));
                    }
                    Err(e) => {
                        tracing::error!("Token refresh failed: {e}");
                        Storage::remove();
                        auth_state.set(None);
                    }
                }
            });
        }
    });

    rsx! {
        {children}
    }
}

/// Calculus to determine how long to wait (in ms) before refreshing.
/// buffers 5 minutes (300s) before actual expiration.
fn calculate_refresh_delay(expires_at: i64) -> u32 {
    let now = chrono::Utc::now().timestamp();
    let buffer = 300;
    let seconds_until_refresh = expires_at - now - buffer;

    if seconds_until_refresh > 0 {
        // Clamp to u32::MAX to prevent overflow crashes on very long tokens
        (seconds_until_refresh * 1000)
            .try_into()
            .unwrap_or(u32::MAX)
    } else {
        0
    }
}
