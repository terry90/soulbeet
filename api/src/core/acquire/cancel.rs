//! Cancelling a download a batch of the user holds.

use crate::auth::UserCtx;
use crate::core::backend_unavailable;
use crate::server_fns::download::enqueue::transfer_key;
use crate::services::Backends;
use crate::transfer_registry::{TransferKey, TransferRegistry};
use dioxus::logger::tracing::info;
use shared::download::DownloadProgress;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

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

/// Cancels the transfer `backend` lists under `transfer_id` from `source`,
/// when one of `ctx`'s batches holds it. Its watcher stops it on its next
/// poll: a monitor shows it Cancelled, and a discovery run moves the track on.
/// A row that names no backend has no transfer to cancel.
pub async fn cancel(
    ctx: &UserCtx,
    backends: &dyn Backends,
    backend: Option<&str>,
    source: &str,
    transfer_id: &str,
) -> Result<(), Failure> {
    let id = backend.ok_or_else(download_not_found)?;
    let backend = (backends.get(id).await).map_err(|e| backend_unavailable(Stage::Transfer, &e))?;
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
    use crate::server_fns::download::test_support::{
        batch, batch_on, transfer, StubBackend, StubBackends,
    };
    use crate::services::DEFAULT_DOWNLOAD_BACKEND;
    use soulbeet::DownloadBackend;

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
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub.clone());
        let theirs = batch("Come Over");
        let held = transfer_key(stub.id(), "peer_a", "Music\\A\\01.flac");
        let registry = TransferRegistry::global();
        registry.claim(std::slice::from_ref(&held), &theirs);
        let on_default = Some(DEFAULT_DOWNLOAD_BACKEND);

        assert_eq!(
            cancel(&own_ctx(), &backends, on_default, "peer_a", "t-1").await,
            Err(download_not_found())
        );
        assert!(!registry.is_cancelled(&held, &theirs.owner));

        assert_eq!(
            cancel(&ctx_of(&theirs), &backends, on_default, "peer_a", "t-1").await,
            Ok(())
        );
        assert!(registry.is_cancelled(&held, &theirs.owner));
        assert!(stub.cancelled().is_empty(), "its watcher stops it");
    }

    #[tokio::test]
    async fn a_cancel_goes_to_the_backend_its_row_names() {
        let (a, b) = (StubBackend::new(vec![]), StubBackend::new(vec![]));
        b.set_listing(listed());
        let backends = StubBackends::default()
            .with("a", a.clone())
            .with("b", b.clone());
        let theirs = batch_on("b", "Come Over");
        let held = transfer_key(b.id(), "peer_a", "Music\\A\\01.flac");
        let registry = TransferRegistry::global();
        registry.claim(std::slice::from_ref(&held), &theirs);

        assert_eq!(
            cancel(&ctx_of(&theirs), &backends, Some("b"), "peer_a", "t-1").await,
            Ok(())
        );
        assert!(registry.is_cancelled(&held, &theirs.owner));
        assert_eq!(
            (a.listings(), b.listings()),
            (vec![], vec![vec!["peer_a".to_string()]])
        );
        assert_eq!(backends.asked(), ["b"]);
    }

    #[tokio::test]
    async fn a_cancel_that_names_no_backend_is_not_found_and_asks_none() {
        let stub = StubBackend::new(vec![]);
        stub.set_listing(listed());
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub.clone());
        let theirs = batch("Come Over");
        let held = transfer_key(stub.id(), "peer_a", "Music\\A\\01.flac");
        TransferRegistry::global().claim(std::slice::from_ref(&held), &theirs);

        assert_eq!(
            cancel(&ctx_of(&theirs), &backends, None, "peer_a", "t-1").await,
            Err(download_not_found())
        );
        assert!(backends.asked().is_empty());
        assert!(stub.listings().is_empty());
        assert!(!TransferRegistry::global().is_cancelled(&held, &theirs.owner));
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
