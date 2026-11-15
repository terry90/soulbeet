//! Cancelling a download a batch of the user holds.

use crate::auth::UserCtx;
use crate::core::backend_unavailable;
use crate::server_fns::download::enqueue::transfer_key;
use crate::services::download_backend;
use crate::transfer_registry::{TransferKey, TransferRegistry};
use dioxus::logger::tracing::info;
use shared::download::DownloadProgress;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::DownloadBackend;
use std::sync::Arc;

/// No transfer of the user's has that id on that peer.
fn download_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Download not found",
    )
}

/// The registry key of the transfer the backend lists with exactly this peer
/// and id, built from the backend's own filename. `None` when it lists no
/// such transfer.
fn cancel_key(
    listed: &[DownloadProgress],
    backend: &'static str,
    source: &str,
    id: &str,
) -> Option<TransferKey> {
    listed
        .iter()
        .find(|row| row.source == source && row.id == id)
        .map(|row| transfer_key(backend, &row.source, &row.item))
}

/// Cancels the transfer the named backend, or the default one, lists under
/// `transfer_id` from `source`, when one of `ctx`'s batches holds it. Its
/// watcher stops it on its next poll: a monitor shows it Cancelled, and a
/// discovery run moves the track on.
pub async fn cancel(
    ctx: &UserCtx,
    backend: Option<&str>,
    source: &str,
    transfer_id: &str,
) -> Result<(), Failure> {
    let backend = download_backend(backend)
        .await
        .map_err(|e| backend_unavailable(Stage::Transfer, &e))?;
    cancel_on(ctx, &backend, source, transfer_id).await
}

/// `cancel` on `backend`.
async fn cancel_on(
    ctx: &UserCtx,
    backend: &Arc<dyn DownloadBackend>,
    source: &str,
    transfer_id: &str,
) -> Result<(), Failure> {
    // The key comes from the transfer the backend lists under that id, never
    // from text the client sends, so a user cannot pair their own file with
    // another user's transfer id on the same peer.
    let listed = backend
        .get_downloads_for(&[source.to_string()])
        .await
        .map_err(|e| e.failure(Stage::Transfer, e.to_string()))?;
    let user = ctx.id();
    let marked = cancel_key(&listed, backend.id(), source, transfer_id)
        .is_some_and(|key| TransferRegistry::global().request_cancel(&key, &user));
    if !marked {
        return Err(download_not_found());
    }
    info!("User {user} cancelled download {transfer_id} from {source}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{ctx_of, own_ctx};
    use super::*;
    use crate::server_fns::download::test_support::{batch, transfer, StubBackend};

    fn listed() -> Vec<DownloadProgress> {
        vec![transfer(
            "peer_a",
            "Music\\A\\01.flac",
            "t-1",
            shared::download::DownloadState::Queued,
        )]
    }

    #[test]
    fn cancel_uses_the_listed_filename_not_the_clients_text() {
        assert_eq!(
            cancel_key(&listed(), "slskd", "peer_a", "t-1"),
            Some(transfer_key("slskd", "peer_a", "Music\\A\\01.flac"))
        );
    }

    #[test]
    fn cancel_needs_a_listed_transfer_with_that_id_and_peer() {
        assert_eq!(cancel_key(&listed(), "slskd", "peer_a", "t-2"), None);
        assert_eq!(cancel_key(&listed(), "slskd", "peer_b", "t-1"), None);
    }

    #[tokio::test]
    async fn another_users_transfer_id_cannot_be_cancelled() {
        let stub = StubBackend::new(vec![]);
        stub.set_listing(listed());
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let theirs = batch("Come Over");
        let held = transfer_key(backend.id(), "peer_a", "Music\\A\\01.flac");
        let registry = TransferRegistry::global();
        registry.claim(std::slice::from_ref(&held), &theirs);

        assert_eq!(
            cancel_on(&own_ctx(), &backend, "peer_a", "t-1").await,
            Err(download_not_found())
        );
        assert!(!registry.is_cancelled(&held, &theirs.owner));

        assert_eq!(
            cancel_on(&ctx_of(&theirs), &backend, "peer_a", "t-1").await,
            Ok(())
        );
        assert!(registry.is_cancelled(&held, &theirs.owner));
        assert!(stub.cancelled().is_empty(), "its watcher stops it");
    }

    #[test]
    fn a_transfer_nobody_of_the_user_holds_is_not_found() {
        let f = download_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Download not found"
            )
        );
    }
}
