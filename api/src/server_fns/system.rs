use dioxus::prelude::*;
use shared::system::{AvailableBackends, SystemHealth};

#[cfg(feature = "server")]
use shared::system::BackendInfo;

#[cfg(feature = "server")]
use crate::services::{
    available_download_backends, available_importers, available_metadata_providers,
    download_backend, evict_navidrome_client, music_importer, navidrome_client_for_user,
};
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use dioxus::logger::tracing::debug;

#[get("/api/system/health", auth: AuthSession)]
pub async fn get_system_health() -> Result<SystemHealth, ServerFnError> {
    #[cfg(feature = "server")]
    {
        let downloader_online = match download_backend(None).await {
            Ok(backend) => backend.health_check().await,
            Err(_) => false,
        };

        let beets_ready = match music_importer(None).await {
            Ok(importer) => importer.health_check().await,
            Err(_) => false,
        };

        let navidrome_online = match navidrome_client_for_user(&auth.0.sub).await {
            Ok(client) => match client.ping().await {
                Ok(()) => true,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("Circuit breaker open") {
                        debug!(
                            "Evicted stale Navidrome client for user {} after circuit-breaker-open ping",
                            auth.0.sub
                        );
                        evict_navidrome_client(&auth.0.sub).await;
                    }
                    false
                }
            },
            Err(_) => false,
        };

        Ok(SystemHealth {
            downloader_online,
            beets_ready,
            navidrome_online,
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
