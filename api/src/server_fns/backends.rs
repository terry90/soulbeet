use dioxus::prelude::*;
use shared::backends::{BackendMeta, BackendSwitch};

#[cfg(feature = "server")]
use super::failure_error;
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};

/// Every download backend currently enabled (always-on or turned on by an admin).
#[get("/api/backends/enabled", _: AuthSession)]
pub async fn get_enabled_download_backends() -> Result<Vec<BackendMeta>, ServerFnError> {
    crate::core::backends::enabled()
        .await
        .map_err(failure_error)
}

/// Every download backend an admin can turn on, and whether it is on.
#[get("/api/backends/switches", _: AdminSession)]
pub async fn get_backend_switches() -> Result<Vec<BackendSwitch>, ServerFnError> {
    crate::core::backends::switches()
        .await
        .map_err(failure_error)
}

/// Turns the download backend `id` on or off.
#[post("/api/backends/switch", auth: AdminSession)]
pub async fn set_backend_enabled(
    id: String,
    enabled: bool,
) -> Result<BackendSwitch, ServerFnError> {
    crate::core::backends::set_enabled(&auth.0, &id, enabled)
        .await
        .map_err(failure_error)?;
    Ok(BackendSwitch { id, enabled })
}
