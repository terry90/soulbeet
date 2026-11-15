use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::{
    DownloadEvent, DownloadableItem, ImportChoice, MatchChoices, QueuedDownload,
};
#[cfg(feature = "server")]
use shared::download::{DownloadProgress, EnqueueStatus};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use crate::{
    base_url::{BaseUrl, BaseUrlError},
    server_fns::{forbidden_error, refusal_error, refusal_error_with, server_error},
    AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use tower_cookies::Cookies;

#[cfg(feature = "server")]
use crate::globals::{
    cleanup_stale_channels, get_or_create_user_channel, register_user_task, unregister_user_task,
    USER_CHANNELS,
};
#[cfg(feature = "server")]
use crate::services::download_backend;

// Local modules
pub mod auto_download;
pub use auto_download::{auto_download, AutoDownloadRequest, AutoDownloadResult};

#[cfg(feature = "server")]
pub mod enqueue;
#[cfg(feature = "server")]
pub mod failover;
#[cfg(feature = "server")]
pub mod monitor;
#[cfg(feature = "server")]
pub mod process;
#[cfg(feature = "server")]
pub mod utils;

// Shared with the tests of sibling modules outside `download`, so it cannot
// stay private to this one.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support;

#[cfg(feature = "server")]
use self::enqueue::{
    deliver, enqueue_request, failed_reason, stopped_rows, transfer_key, RequestEnqueue, Watcher,
};
#[cfg(feature = "server")]
use self::monitor::{DownloadMonitor, WatchedTransfer};
#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner, TransferRegistry};
#[cfg(feature = "server")]
use soulbeet::{error::SoulseekError, DownloadBackend};
#[cfg(feature = "server")]
use std::sync::Arc;

/// Name for a manual download's batch: the album its items share, or the
/// first item's title when they span albums.
#[cfg(feature = "server")]
fn manual_batch_label(items: &[DownloadableItem]) -> String {
    match items.split_first() {
        Some((first, rest))
            if !first.album.trim().is_empty() && rest.iter().all(|i| i.album == first.album) =>
        {
            first.album.clone()
        }
        Some((first, _)) => first.title.clone(),
        None => String::new(),
    }
}

/// Name for an auto download's batch: the picked group's title, which a
/// track search leaves empty since no album was searched for, else the same
/// name a manual download of its items gets.
#[cfg(feature = "server")]
fn auto_batch_label(group: &shared::download::DownloadableGroup) -> String {
    if group.title.trim().is_empty() {
        manual_batch_label(&group.items)
    } else {
        group.title.clone()
    }
}

/// How often an open download socket checks that its session still counts.
#[cfg(feature = "server")]
const SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether a socket upgrade may come from the page its `Origin` names: a
/// page on the host and port the request was sent to, so a page opened at
/// any of Soulful's addresses works, whatever `PUBLIC_URL` pins. The scheme
/// is not compared, since a proxy may end TLS. A request with no `Origin`
/// is not from a browser page, so no other site can be riding on the user's
/// cookie.
#[cfg(feature = "server")]
pub(crate) fn origin_allowed(headers: &HeaderMap, uri: &Uri) -> Result<bool, BaseUrlError> {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return Ok(true);
    };
    let addressed = crate::base_url::addressed_to(headers, uri)?;
    Ok(origin
        .to_str()
        .ok()
        .and_then(|o| BaseUrl::parse(o).ok())
        .is_some_and(|o| o.same_host(&addressed)))
}

/// Whether the socket's token still gives `user` a session: not signed out,
/// no credential change since, the account still there.
#[cfg(feature = "server")]
pub(crate) async fn session_still_valid(
    token: Option<&str>,
    db: &sqlx::SqlitePool,
    key: &crate::secret_key::SecretKey,
    user: &UserId,
) -> bool {
    super::guard::load_session(token, db, key)
        .await
        .is_ok_and(|ctx| ctx.id() == *user)
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
#[get("/api/downloads/updates", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn download_updates_ws(
    options: WebSocketOptions,
) -> Result<Websocket<(), DownloadEvent>, ServerFnError> {
    let user = auth.0.id();

    let allowed = origin_allowed(&headers, &uri)
        .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
    if !allowed {
        warn!("Refused a download updates socket for user {user} from another origin");
        return Err(forbidden_error(
            "Download updates can only be opened from Soulful's own page",
        ));
    }
    let token = cookies
        .get(crate::AUTH_COOKIE_NAME)
        .map(|c| c.value().to_string());

    let rx = {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(&user) {
            channel.sender.subscribe()
        } else {
            drop(map);
            let mut map = USER_CHANNELS.write().await;
            let channel = map
                .entry(user.clone())
                .or_insert_with(crate::globals::UserChannel::new);
            channel.sender.subscribe()
        }
    };

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut rx = rx;
        info!("WebSocket connected for user: {}", user);
        let mut recheck = tokio::time::interval_at(
            tokio::time::Instant::now() + SESSION_RECHECK,
            SESSION_RECHECK,
        );

        loop {
            // handle both broadcast messages and potential socket closure
            tokio::select! {
                _ = recheck.tick() => {
                    let valid = session_still_valid(
                        token.as_deref(),
                        &crate::db::DB,
                        crate::secret_key::current(),
                        &user,
                    )
                    .await;
                    if !valid {
                        info!("WebSocket closed for user {}: the session ended", user);
                        break;
                    }
                }
                result = rx.recv() => {
                    match result {
                        Ok(downloads) => {
                            if socket.send(downloads).await.is_err() {
                                info!("WebSocket closed (client disconnected)");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                "Download updates lagged, skipped {} messages. Consider reducing download batch size.",
                                skipped
                            );
                            // lagged is recoverable - continue receiving
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Broadcast channel closed");
                            break;
                        }
                    }
                }
                result = socket.recv() => {
                    if result.is_err() {
                        info!("WebSocket client disconnected");
                        break;
                    }
                }
            }
        }

        info!("WebSocket disconnected for user: {}", user);

        // Trigger cleanup of stale channels periodically
        cleanup_stale_channels().await;
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelDownloadRequest {
    pub id: String,
    pub source: String,
    pub item: String,
    pub backend: Option<String>,
}

/// The registry key of the transfer the backend lists with exactly this peer
/// and id, built from the backend's own filename. `None` when it lists no
/// such transfer.
#[cfg(feature = "server")]
pub(crate) fn cancel_key(
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

#[post("/api/downloads/cancel", auth: AuthSession)]
pub async fn cancel_download(req: CancelDownloadRequest) -> Result<(), ServerFnError> {
    let user = auth.0.id();

    let backend = download_backend(req.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;

    // The client's `item` is never trusted: the key comes from the transfer
    // the backend lists under that id, so a user cannot pair their own file
    // with another user's transfer id on the same peer.
    let listed = backend
        .get_downloads_for(std::slice::from_ref(&req.source))
        .await
        .map_err(server_error)?;
    let owned = cancel_key(&listed, backend.id(), &req.source, &req.id)
        .is_some_and(|key| TransferRegistry::global().owned_by(&key, &user));
    if !owned {
        return Err(refusal_error("Download not found"));
    }

    backend
        .cancel_download(&req.source, &req.id, false)
        .await
        .map_err(server_error)?;

    info!(
        "User {} cancelled download {} from {}",
        user, req.id, req.source
    );

    // Send cancelled state to UI via broadcast channel
    let (tx, _) = get_or_create_user_channel(&user).await;
    let cancelled = DownloadProgress {
        id: req.id,
        source: req.source,
        item: req.item,
        size: 0,
        transferred: 0,
        state: shared::download::DownloadState::Cancelled,
        percent: 0.0,
        speed: 0.0,
        error: None,
        backend: req.backend,
        batch_id: None,
        batch_label: None,
        local_path: None,
        import_output: None,
        retryable: false,
        can_choose_import: false,
    };
    let _ = tx.send(DownloadEvent::Progress(vec![cancelled]));

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub items: Vec<DownloadableItem>,
    pub target_folder: String,
    #[serde(default)]
    pub backend: Option<String>,
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(req: DownloadRequest) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let user = auth.0.id();

    if let Some(issue) = crate::importer_health::broken_issue().await {
        return Err(refusal_error(issue.title));
    }
    let folder =
        crate::models::folder::Folder::find_for_user_by_path(&auth.0.user_id, &req.target_folder)
            .await
            .map_err(server_error)?
            .ok_or_else(|| refusal_error("Folder not found"))?;

    let target_path_buf = std::path::Path::new(&folder.path).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let backend = download_backend(req.backend.as_deref())
        .await
        .map_err(|e| server_error(format!("download backend not available: {}", e)))?;
    let fresh = BatchRef::new(
        TransferOwner {
            user_id: user.clone(),
            batch_id: uuid::Uuid::new_v4().to_string(),
        },
        manual_batch_label(&req.items),
        target_path_buf,
    );
    let queued = queue_detached(backend, req.items, fresh, user, req.backend)
        .await
        .map_err(server_error)?;
    match queued {
        Queued { error: Some(e), .. } => Err(server_error(e)),
        Queued {
            refusal: Some(reason),
            ..
        } => Err(refusal_error(reason)),
        Queued { results, .. } => Ok(results),
    }
}

/// What a manual request's enqueue came to.
#[cfg(feature = "server")]
#[derive(Debug)]
struct Queued {
    results: Vec<QueuedDownload>,
    error: Option<SoulseekError>,
    /// Nothing was enqueued, for this reason.
    refusal: Option<&'static str>,
}

/// Runs `queue_and_watch` as its own task. Registry keys are claimed from
/// its start, and the client may disconnect at any await and drop the
/// handler: dropping the task's handle does not stop it, so the request
/// always gets as far as a monitor or a release.
#[cfg(feature = "server")]
async fn queue_detached(
    backend: Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
    user: UserId,
    backend_id: Option<String>,
) -> Result<Queued, tokio::task::JoinError> {
    tokio::spawn(queue_and_watch(backend, items, fresh, user, backend_id)).await
}

/// Claims and enqueues a manual request's files, shows their rows, and hands
/// the enqueued ones to a monitor: the running batch's when the request
/// joined one, else a new one.
#[cfg(feature = "server")]
async fn queue_and_watch(
    backend: Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
    fresh: BatchRef,
    user: UserId,
    backend_id: Option<String>,
) -> Queued {
    let enq = enqueue_request(&backend, items, fresh).await;
    let refusal = enq.refusal();
    let RequestEnqueue {
        batch,
        results,
        watcher,
        error,
        ..
    } = enq;

    let (tx, _) = get_or_create_user_channel(&user).await;
    let row = |p: DownloadProgress| {
        let mut p = p.with_batch(batch.owner.batch_id.clone(), batch.label.clone());
        p.backend = backend_id.clone();
        p
    };

    let failed_entries: Vec<DownloadProgress> = results
        .iter()
        .filter_map(|d| {
            let reason = failed_reason(&d.status)?;
            Some(row(DownloadProgress::failed(
                d.id.clone(),
                d.source.clone(),
                d.item.clone(),
                reason.to_string(),
            )))
        })
        .collect();
    if !failed_entries.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(failed_entries));
    }

    let enqueued: Vec<QueuedDownload> = results
        .iter()
        .filter(|d| d.status == EnqueueStatus::Enqueued)
        .cloned()
        .collect();

    // Send initial "Queued" state immediately so UI shows the downloads right away
    let queued_entries: Vec<DownloadProgress> = enqueued
        .iter()
        .map(|d| {
            row(DownloadProgress::queued(
                d.id.clone(),
                d.source.clone(),
                d.item.clone(),
                d.size,
            ))
        })
        .collect();
    if !queued_entries.is_empty() {
        let _ = tx.send(DownloadEvent::Progress(queued_entries));
    }

    match watcher {
        Watcher::Joined(reservation) => {
            info!(
                "Added {} downloads to running batch {}",
                enqueued.len(),
                batch.owner.batch_id
            );
            let lost = deliver(reservation, &enqueued, &backend, &batch).await;
            if !lost.is_empty() {
                let _ = tx.send(DownloadEvent::Progress(stopped_rows(&lost, &batch)));
            }
        }
        Watcher::Own if enqueued.is_empty() && batch.inbox.close_if_idle() => {}
        // Starts empty when nothing was enqueued but another request joined
        // the batch while this one was enqueueing.
        Watcher::Own => {
            info!(
                "Started monitoring downloads: {:?}",
                enqueued.iter().map(|d| &d.item).collect::<Vec<_>>()
            );
            let transfers = enqueued.iter().map(WatchedTransfer::from).collect();
            let task_cancellation = register_user_task(&user).await;
            tokio::spawn(async move {
                let mut monitor = DownloadMonitor::new(
                    transfers,
                    batch.target.clone(),
                    tx,
                    task_cancellation,
                    batch,
                );
                monitor.run().await;
                unregister_user_task(&user).await;
            });
        }
    }

    Queued {
        results,
        error,
        refusal,
    }
}

/// Imports a failed or skipped row again, from the job kept for it, the way
/// `choice` says.
#[post("/api/downloads/retry-import", auth: AuthSession)]
pub async fn retry_import(item: String, choice: ImportChoice) -> Result<(), ServerFnError> {
    crate::import_queue::retry_import_for(&auth.0.id(), &item, choice)
        .await
        .map_err(kept_import_refusal)
}

/// The matches a skipped row can be imported with.
#[post("/api/downloads/import-choices", auth: AuthSession)]
pub async fn import_choices(item: String) -> Result<MatchChoices, ServerFnError> {
    crate::import_queue::import_choices_for(&auth.0.id(), &item).map_err(kept_import_refusal)
}

/// A refused retry or matches request, with what stays kept for the row.
#[cfg(feature = "server")]
fn kept_import_refusal(refusal: crate::import_queue::Refusal) -> ServerFnError {
    refusal_error_with(refusal.message, refusal.kept)
}

/// Drops the stored imports of rows the user cleared.
#[post("/api/downloads/forget-retries", auth: AuthSession)]
pub async fn forget_import_retries(items: Vec<String>) -> Result<(), ServerFnError> {
    crate::import_queue::forget_retries(&auth.0.id(), &items);
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use test_support::item;

    #[test]
    fn auto_label_is_the_group_title_else_its_items_name() {
        let mut group = shared::download::DownloadableGroup {
            source: "p".to_string(),
            group_id: "g".to_string(),
            title: "Glass Atlas".to_string(),
            artist: None,
            item_count: 1,
            total_size: 0,
            items: vec![item("p", "Northern Line", "1.flac")],
            quality: "flac".to_string(),
            score: 1.0,
        };
        assert_eq!(auto_batch_label(&group), "Glass Atlas");

        // A track search: no album was searched for, so neither the group
        // nor its items name one.
        group.title = String::new();
        group.items[0].album = String::new();
        assert_eq!(auto_batch_label(&group), "Northern Line");
    }

    fn listed() -> Vec<DownloadProgress> {
        vec![test_support::transfer(
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

    #[test]
    fn only_pages_on_the_address_the_socket_was_asked_at_may_open_it() {
        use axum::http::{HeaderName, HeaderValue};
        let allowed = |pairs: &[(&str, &str)]| {
            let headers: HeaderMap = pairs
                .iter()
                .map(|(k, v)| {
                    (
                        HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        HeaderValue::from_str(v).unwrap(),
                    )
                })
                .collect();
            origin_allowed(&headers, &Uri::from_static("/api/downloads/updates"))
        };
        let host = ("host", "music.example.com");

        assert_eq!(
            allowed(&[host, ("origin", "https://music.example.com")]),
            Ok(true)
        );
        assert_eq!(
            allowed(&[host, ("origin", "https://evil.example")]),
            Ok(false)
        );
        assert_eq!(
            allowed(&[host, ("origin", "https://music.example.com:8443")]),
            Ok(false)
        );
        assert_eq!(allowed(&[host, ("origin", "null")]), Ok(false));
        // Not a browser page: nothing to compare, and no address needed.
        assert_eq!(allowed(&[]), Ok(true));
        // A page opened at a LAN address talks to that address; PUBLIC_URL
        // takes no part.
        assert_eq!(
            allowed(&[
                ("host", "192.168.1.5:9765"),
                ("origin", "http://192.168.1.5:9765")
            ]),
            Ok(true)
        );
        // http://nas:9765 typed in the browser: the host derives https.
        assert_eq!(
            allowed(&[("host", "nas:9765"), ("origin", "http://nas:9765")]),
            Ok(true)
        );
        // Behind a proxy, the forwarded host is the one the page was on.
        assert_eq!(
            allowed(&[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "music.example.com"),
                ("origin", "https://music.example.com"),
            ]),
            Ok(true)
        );
        assert_eq!(
            allowed(&[
                ("host", "soulful:9765"),
                ("forwarded", "host=music.example.com;proto=https"),
                ("origin", "http://soulful:9765"),
            ]),
            Ok(false)
        );
        assert_eq!(
            allowed(&[("origin", "https://music.example.com")]),
            Err(BaseUrlError::MissingHost)
        );
    }

    #[tokio::test]
    async fn the_socket_session_ends_with_the_session() {
        use crate::models::revoked_session::RevokedSession;
        use crate::secret_key::SecretKey;

        let db = crate::db::test_db::migrated().await;
        let key = SecretKey::new("k".repeat(32));
        let ann = crate::models::user::User::create_in(&db, "ann", "pw")
            .await
            .unwrap();
        let id = UserId::new(&ann.id);
        let token = crate::auth::create_token(&key, &ann.id, 0).unwrap();
        let valid = |t: &str| {
            let (db, key, id, t) = (db.clone(), &key, id.clone(), t.to_string());
            async move { session_still_valid(Some(&t), &db, key, &id).await }
        };
        assert!(valid(&token).await);
        assert!(!session_still_valid(None, &db, &key, &id).await);
        assert!(!session_still_valid(Some(&token), &db, &key, &UserId::new("other")).await);

        // Signed out.
        let jti = crate::auth::verify_token(&key, &token).unwrap().jti;
        RevokedSession::revoke_in(&db, &jti, i64::MAX, 0)
            .await
            .unwrap();
        assert!(!valid(&token).await);

        // Password changed.
        let token = crate::auth::create_token(&key, &ann.id, 0).unwrap();
        assert!(valid(&token).await);
        crate::models::user::User::update_password_in(&db, &ann.id, "new")
            .await
            .unwrap();
        assert!(!valid(&token).await);

        // Account deleted.
        let token = crate::auth::create_token(&key, &ann.id, 1).unwrap();
        assert!(valid(&token).await);
        crate::models::user::User::delete_in(&db, &ann.id)
            .await
            .unwrap();
        assert!(!valid(&token).await);
    }

    #[test]
    fn manual_label_is_the_shared_album_else_the_first_title() {
        let same_album = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];
        assert_eq!(manual_batch_label(&same_album), "Come Over");

        let mut mixed = same_album.clone();
        mixed[1].album = "Other".to_string();
        assert_eq!(manual_batch_label(&mixed), "One");

        let mut no_album = same_album;
        for i in &mut no_album {
            i.album = " ".to_string();
        }
        assert_eq!(manual_batch_label(&no_album), "One");
    }

    #[tokio::test]
    async fn a_request_dropped_mid_enqueue_still_hands_its_transfers_over() {
        use test_support::{batch, key, ScriptedOutcome, StubBackend};

        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let (reached, release) = stub.hold_downloads();
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = batch("Come Over");
        TransferRegistry::global().claim(&[key(&backend, "p", "1.flac")], &running);
        let fresh = BatchRef::new(
            TransferOwner {
                user_id: running.owner.user_id.clone(),
                batch_id: uuid::Uuid::new_v4().to_string(),
            },
            "Come Over".to_string(),
            running.target.clone(),
        );
        let items = vec![item("p", "One", "1.flac"), item("p", "Two", "2.flac")];

        // The handler is dropped while slskd is still answering the enqueue,
        // as when the client disconnects.
        let handler = tokio::spawn(queue_detached(
            backend,
            items,
            fresh,
            UserId::new("detached-request"),
            None,
        ));
        reached.notified().await;
        handler.abort();
        assert!(handler.await.unwrap_err().is_cancelled());
        release.notify_one();

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let slots = running.inbox.take();
                if !slots.is_empty() {
                    break slots;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the enqueued transfer never reached the running batch");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].filename, "2.flac");
        assert_eq!(delivered[0].transfer_id, "t-2.flac");
    }
}
