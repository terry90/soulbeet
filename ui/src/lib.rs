//! This crate contains all shared UI for the workspace.

mod navbar;
pub use navbar::Navbar;

mod layout;
pub use layout::Layout;

mod auth;
pub use auth::*;

mod health_context;
pub use health_context::*;

mod settings_context;
pub use settings_context::*;

mod player_context;
pub use player_context::*;

mod components;
pub use components::*;

/// Cross-target async sleep usable from both WASM (`gloo_timers`) and native (`tokio`).
#[cfg(target_arch = "wasm32")]
pub async fn sleep_ms(duration_ms: u32) {
    gloo_timers::future::TimeoutFuture::new(duration_ms).await;
}

/// Cross-target async sleep usable from both WASM (`gloo_timers`) and native (`tokio`).
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep_ms(duration_ms: u32) {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(duration_ms))).await;
    }
}
