use dioxus::prelude::*;
use shared::system::{AvailableBackends, SystemHealth};

#[cfg(feature = "server")]
use shared::system::BackendInfo;

#[cfg(feature = "server")]
use crate::{globals::SERVICES, AuthSession};

#[get("/api/system/health", _: AuthSession)]
pub async fn get_system_health() -> Result<SystemHealth, ServerFnError> {
    #[cfg(feature = "server")]
    {
        let slskd_online = match SERVICES.download(None) {
            Some(backend) => backend.health_check().await,
            None => false,
        };

        let beets_ready = match SERVICES.importer(None) {
            Some(importer) => importer.health_check().await,
            None => false,
        };

        Ok(SystemHealth {
            slskd_online,
            beets_ready,
        })
    }
    #[cfg(not(feature = "server"))]
    Ok(SystemHealth::default())
}

#[get("/api/system/backends", _: AuthSession)]
pub async fn get_backends() -> Result<AvailableBackends, ServerFnError> {
    #[cfg(feature = "server")]
    {
        Ok(AvailableBackends {
            metadata: SERVICES
                .list_metadata()
                .into_iter()
                .map(|(id, name)| BackendInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect(),
            download: SERVICES
                .list_downloads()
                .into_iter()
                .map(|(id, name)| BackendInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect(),
            importer: SERVICES
                .list_importers()
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
