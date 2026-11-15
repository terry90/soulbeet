use dioxus::prelude::*;
use shared::system::{AvailableBackends, SystemHealth};

#[cfg(feature = "server")]
use shared::system::BackendInfo;

#[cfg(feature = "server")]
use crate::config::CONFIG;
#[cfg(feature = "server")]
use crate::services::{
    available_download_backends, available_importers, available_metadata_providers,
};
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};

#[get("/api/system/health", auth: AuthSession)]
pub async fn get_system_health() -> Result<SystemHealth, ServerFnError> {
    Ok(crate::core::health::system_health(&auth.0).await)
}

#[get("/api/system/backends", _: AuthSession)]
pub async fn get_backends() -> Result<AvailableBackends, ServerFnError> {
    #[cfg(feature = "server")]
    {
        Ok(AvailableBackends {
            metadata: available_metadata_providers()
                .into_iter()
                .map(|(id, name)| BackendInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect(),
            download: available_download_backends()
                .into_iter()
                .map(|(id, name)| BackendInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect(),
            importer: available_importers()
                .into_iter()
                .map(|(id, name)| BackendInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect(),
        })
    }
    #[cfg(not(feature = "server"))]
    Ok(AvailableBackends::default())
}

#[get("/api/system/security", _: AdminSession)]
pub async fn get_security_status() -> Result<crate::security::SecurityStatus, ServerFnError> {
    Ok(crate::security::SecurityStatus {
        mcp_blocked_by: crate::security::gate_reasons(),
        sign_in_problem: CONFIG.public_url_problem(),
    })
}
