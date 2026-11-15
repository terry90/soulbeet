use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::{
    DownloadEvent, DownloadableItem, ImportChoice, MatchChoices, QueuedDownload,
};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::download::DownloadProgress;
#[cfg(feature = "server")]
use tokio::sync::broadcast;

#[cfg(feature = "server")]
use crate::{
    base_url::{BaseUrl, BaseUrlError},
    server_fns::{failure_error, forbidden_error, import_refusal_error, refusal_error},
    AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use tower_cookies::Cookies;

#[cfg(feature = "server")]
use crate::globals::{cleanup_stale_channels, USER_CHANNELS};

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
use crate::auth::UserId;

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

/// What a new download socket sends before the live events: the build frame when the client
/// asked for it (a bundle older than the frame does not ask: it cannot decode the variant and
/// would reconnect on every try), then the rows of the user's unfinished files when there are any.
#[cfg(feature = "server")]
fn first_frames(build_frame: Option<bool>, rows: Vec<DownloadProgress>) -> Vec<DownloadEvent> {
    let build = (build_frame == Some(true))
        .then(|| DownloadEvent::Build(crate::build_id::BUILD_ID.to_string()));
    let rows = (!rows.is_empty()).then_some(DownloadEvent::Progress(rows));
    build.into_iter().chain(rows).collect()
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
/// A client that passes `build_frame=true` gets `DownloadEvent::Build` first.
#[get("/api/downloads/updates?build_frame", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn download_updates_ws(
    build_frame: Option<bool>,
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

    // Read after subscribing, so a row sent from here on arrives live; one sent both ways is the
    // same row twice, which the panel's map keeps once. The ledger is written behind the sends,
    // so a change sent just before the subscribe can be missing from this read too: the panel
    // then shows the slot's stored row until its next one.
    let rows = match crate::ledger::store::open_rows(&crate::db::DB, &user).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Could not read the unfinished downloads of user {user}: {e}");
            Vec::new()
        }
    };

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut rx = rx;
        info!("WebSocket connected for user: {}", user);
        for frame in first_frames(build_frame, rows) {
            // A client already gone ends the loop below at its first receive.
            let _ = socket.send(frame).await;
        }
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
    pub backend: Option<String>,
}

#[post("/api/downloads/cancel", auth: AuthSession)]
pub async fn cancel_download(req: CancelDownloadRequest) -> Result<(), ServerFnError> {
    crate::core::acquire::cancel(&auth.0, req.backend.as_deref(), &req.source, &req.id)
        .await
        .map_err(failure_error)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub items: Vec<DownloadableItem>,
    pub target_folder: String,
    #[serde(default)]
    pub backend: Option<String>,
    /// The search the items were picked from.
    pub search_id: String,
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(req: DownloadRequest) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let folder = crate::core::folders::owned_at(&auth.0, &req.target_folder)
        .await
        .map_err(failure_error)?;
    crate::core::acquire::download(
        &auth.0,
        &folder,
        req.items,
        req.backend.as_deref(),
        &req.search_id,
    )
    .await
    .map_err(failure_error)
}

/// Imports a failed or skipped row again, from the job kept for it, the way
/// `choice` says.
#[post("/api/downloads/retry-import", auth: AuthSession)]
pub async fn retry_import(item: String, choice: ImportChoice) -> Result<(), ServerFnError> {
    crate::import_queue::retry_import_for(&auth.0.id(), &item, choice)
        .await
        .map_err(import_refusal_error)
}

/// The matches a skipped row can be imported with.
#[post("/api/downloads/import-choices", auth: AuthSession)]
pub async fn import_choices(item: String) -> Result<MatchChoices, ServerFnError> {
    crate::import_queue::import_choices_for(&auth.0.id(), &item).map_err(import_refusal_error)
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

    #[test]
    fn a_socket_opens_with_the_build_then_the_unfinished_rows() {
        let row = DownloadProgress::queued("t-1".into(), "peer_a".into(), "01.flac".into(), 100);
        let kinds = |frames: Vec<DownloadEvent>| {
            frames
                .into_iter()
                .map(|f| match f {
                    DownloadEvent::Build(id) => {
                        format!("build {}", id == crate::build_id::BUILD_ID)
                    }
                    DownloadEvent::Progress(rows) => format!("rows {}", rows.len()),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(first_frames(Some(true), vec![row.clone()])),
            vec!["build true", "rows 1"]
        );
        assert_eq!(kinds(first_frames(None, vec![row])), vec!["rows 1"]);
        assert!(first_frames(Some(false), vec![]).is_empty());
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
}
