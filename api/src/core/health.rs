//! What the user's backends report about their health.

use crate::auth::UserCtx;
use crate::services::{download_backend, evict_navidrome_client, navidrome_client_for_user};
use dioxus::logger::tracing::debug;
use shared::failure::Stage;
use shared::system::SystemHealth;

pub async fn system_health(ctx: &UserCtx) -> SystemHealth {
    let downloader_online = match download_backend(None).await {
        Ok(backend) => backend.health_check().await,
        Err(_) => false,
    };

    let importer = crate::importer_health::snapshot().await;

    let navidrome_failure = match navidrome_client_for_user(&ctx.user_id).await {
        Ok(client) => match client.ping().await {
            Ok(()) => None,
            Err(e) => {
                if matches!(e, soulbeet::NavidromeError::CircuitOpen) {
                    debug!(
                        "Evicted stale Navidrome client for user {} after circuit-breaker-open ping",
                        ctx.user_id
                    );
                    evict_navidrome_client(&ctx.user_id).await;
                }
                Some(e.failure(Stage::LibraryCheck, e.to_string()))
            }
        },
        Err(e) => Some(e.failure(Stage::LibraryCheck, e.to_string())),
    };

    SystemHealth {
        downloader_online,
        importer,
        navidrome_online: navidrome_failure.is_none(),
        navidrome_failure,
    }
}
