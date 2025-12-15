use dioxus::prelude::*;
use shared::system::SystemHealth;

#[cfg(feature = "server")]
use crate::globals::SLSKD_CLIENT;
use crate::AuthSession;

#[get("/api/system/health", _: AuthSession)]
pub async fn get_system_health() -> Result<SystemHealth, ServerFnError> {
    #[cfg(feature = "server")]
    {
        let slskd_online = SLSKD_CLIENT.check_connection().await;

        let beets_ready = {
            use tokio::process::Command;
            let output = Command::new("beet").arg("version").output().await;
            output.map(|o| o.status.success()).unwrap_or(false)
        };

        Ok(SystemHealth {
            slskd_online,
            beets_ready,
        })
    }
    #[cfg(not(feature = "server"))]
    Ok(SystemHealth::default())
}
