//! What the user's backends report about their health.

use crate::auth::UserCtx;
use crate::services::{download_backend, evict_navidrome_client, navidrome_client_for_user};
use dioxus::logger::tracing::debug;
use shared::failure::{Failure, Stage};
use shared::system::SystemHealth;

/// The three probes run at once, so the answer comes after the slowest.
pub async fn system_health(ctx: &UserCtx) -> SystemHealth {
    let ((downloader_online, downloader_failure), importer, navidrome_failure) = tokio::join!(
        downloader_health(),
        crate::importer_health::snapshot(),
        navidrome_health(ctx),
    );

    SystemHealth {
        downloader_online,
        downloader_failure,
        importer,
        navidrome_online: navidrome_failure.is_none(),
        navidrome_failure,
    }
}

/// Whether the download backend answers, and why it could not be looked up
/// when it could not.
async fn downloader_health() -> (bool, Option<Failure>) {
    match download_backend(None).await {
        Ok(backend) => (backend.health_check().await, None),
        Err(e) => (false, Some(e.failure(Stage::SourceSearch, e.to_string()))),
    }
}

async fn navidrome_health(ctx: &UserCtx) -> Option<Failure> {
    match navidrome_client_for_user(&ctx.user_id).await {
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
    }
}
