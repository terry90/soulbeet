//! What the user's backends report about their health.

use crate::auth::UserCtx;
use crate::services::{download_backend, evict_navidrome_client, navidrome_client_for_user};
use dioxus::logger::tracing::debug;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::system::SystemHealth;
use std::future::Future;
use std::time::Duration;

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

/// How long the download backend's health check is waited for. slskd's is
/// two requests in turn, each with its own 30 s timeout, and get_status
/// promises an answer within 30 s.
const DOWNLOADER_WAIT: Duration = Duration::from_secs(30);

/// Whether the download backend can take work, and why not when it cannot:
/// its lookup failed, its own check failed, or it gave no answer in time.
async fn downloader_health() -> (bool, Option<Failure>) {
    let checked = match download_backend(None).await {
        Ok(backend) => within(DOWNLOADER_WAIT, backend.health_check()).await,
        Err(e) => Err(e.failure(Stage::SourceSearch, e.to_string())),
    };
    match checked {
        Ok(()) => (true, None),
        Err(failure) => (false, Some(failure)),
    }
}

/// `check`'s answer, or `backend_unreachable` when it takes longer than
/// `wait`.
async fn within(
    wait: Duration,
    check: impl Future<Output = Result<(), Failure>>,
) -> Result<(), Failure> {
    tokio::time::timeout(wait, check).await.unwrap_or_else(|_| {
        Err(Failure::new(
            FailureCode::BackendUnreachable,
            Stage::SourceSearch,
            RetryAdvice::SameSourceLater,
            format!(
                "The download backend did not answer its health check within {} seconds",
                wait.as_secs()
            ),
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_health_check_without_an_answer_in_time_is_unreachable() {
        let failure = within(DOWNLOADER_WAIT, std::future::pending())
            .await
            .unwrap_err();
        assert_eq!(
            (failure.code, failure.retry, failure.message.as_str()),
            (
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
                "The download backend did not answer its health check within 30 seconds"
            )
        );
        assert_eq!(within(DOWNLOADER_WAIT, async { Ok(()) }).await, Ok(()));
    }
}
