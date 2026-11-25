//! What the user's backends report about their health.

use crate::auth::UserCtx;
use crate::core::library::navidrome_failure;
use crate::services::{
    enabled_download_backends, evict_navidrome_client, navidrome_client_for_user, Backends,
    Registry, DEFAULT_DOWNLOAD_BACKEND,
};
use dioxus::logger::tracing::debug;
use shared::backends::BackendMeta;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::system::{DownloaderHealth, SystemHealth};
use std::future::Future;
use std::time::Duration;

/// The probes run at once, so the answer comes after the slowest.
pub async fn system_health(ctx: &UserCtx) -> SystemHealth {
    let (downloaders, importer, navidrome_failure) = tokio::join!(
        downloaders_health(),
        crate::importer_health::snapshot(),
        navidrome_health(ctx),
    );
    let default = downloaders
        .iter()
        .find(|d| d.id == DEFAULT_DOWNLOAD_BACKEND);

    SystemHealth {
        downloader_online: default.is_some_and(|d| d.online),
        downloader_failure: default.and_then(|d| d.failure.clone()),
        downloaders,
        importer,
        navidrome_online: navidrome_failure.is_none(),
        navidrome_failure,
    }
}

/// How long the download backend's health check is waited for. slskd's is
/// two requests in turn, each with its own 30 s timeout, and get_status
/// promises an answer within 30 s.
const DOWNLOADER_WAIT: Duration = Duration::from_secs(30);

/// Every enabled download backend, checked at once.
async fn downloaders_health() -> Vec<DownloaderHealth> {
    downloaders_health_of(&Registry, &enabled_download_backends().await).await
}

/// Whether each of `enabled` can take work, and why not when it cannot: its
/// lookup failed, its own check failed, or it gave no answer in time. One
/// backend's answer never waits for another's.
async fn downloaders_health_of(
    backends: &dyn Backends,
    enabled: &[&BackendMeta],
) -> Vec<DownloaderHealth> {
    futures::future::join_all(enabled.iter().map(|meta| async move {
        let checked = match backends.get(meta.id).await {
            Ok(backend) => within(DOWNLOADER_WAIT, backend.health_check()).await,
            Err(e) => Err(e.failure(Stage::SourceSearch, e.to_string())),
        };
        DownloaderHealth {
            id: meta.id.to_string(),
            name: meta.name.to_string(),
            online: checked.is_ok(),
            failure: checked.err(),
        }
    }))
    .await
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
                Some(navidrome_failure(Stage::LibraryCheck, &e))
            }
        },
        Err(e) => Some(e.failure(Stage::LibraryCheck, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_fns::download::test_support::{StubBackend, StubBackends};
    use crate::services::BackendLookupError;
    use shared::backends::{Switch, SLSKD};

    #[tokio::test(start_paused = true)]
    async fn each_backend_has_its_own_entry_and_one_down_leaves_the_other_online() {
        const ARCHIVE: BackendMeta = BackendMeta {
            id: "archive",
            name: "Internet Archive",
            switch: Switch::AdminToggle,
            ..SLSKD
        };
        const SILENT: BackendMeta = BackendMeta {
            id: "silent",
            name: "Silent",
            switch: Switch::AdminToggle,
            ..SLSKD
        };
        let silent = StubBackend::new(vec![]);
        silent.hang_health_check();
        let backends = StubBackends::default()
            .with("slskd", StubBackend::new(vec![]))
            .with("silent", silent);
        backends.fail(
            "archive",
            BackendLookupError::Disabled("Internet Archive".into()),
        );

        let health = downloaders_health_of(&backends, &[&SLSKD, &ARCHIVE, &SILENT]).await;

        let rows: Vec<(&str, &str, bool, Option<FailureCode>)> = health
            .iter()
            .map(|d| {
                (
                    d.id.as_str(),
                    d.name.as_str(),
                    d.online,
                    d.failure.as_ref().map(|f| f.code),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("slskd", "Soulseek", true, None),
                (
                    "archive",
                    "Internet Archive",
                    false,
                    Some(FailureCode::BackendNotConfigured)
                ),
                (
                    "silent",
                    "Silent",
                    false,
                    Some(FailureCode::BackendUnreachable)
                ),
            ]
        );
    }

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
