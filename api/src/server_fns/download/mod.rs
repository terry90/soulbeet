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
    server_fns::{
        failure_error, forbidden_error, import_refusal_error, refusal_error, server_error,
    },
    AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use tower_cookies::Cookies;

#[cfg(feature = "server")]
use crate::globals::{cleanup_stale_channels, USER_CHANNELS};

// Local modules
pub mod acquire_play;
pub use acquire_play::{acquire_and_play, list_archival_upgrades, trigger_archival_upgrade};
pub mod auto_download;
pub use auto_download::{
    auto_download, download_catalog_album, AutoDownloadRequest, AutoDownloadResult,
    DownloadCatalogAlbumRequest,
};
pub mod diagnostics;
pub use diagnostics::get_job_diagnostics;

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
/// any of Soulbeet's addresses works, or on the address `public_url` (a
/// usable `PUBLIC_URL`) declares, for a proxy that cannot pass the public
/// name. The scheme is not compared, since a proxy may end TLS. A request
/// with no `Origin` is not from a browser page, so no other site can be
/// riding on the user's cookie. A refusal says which addresses were compared.
#[cfg(feature = "server")]
pub(crate) fn origin_allowed(
    headers: &HeaderMap,
    uri: &Uri,
    public_url: Option<&BaseUrl>,
) -> Result<Result<(), String>, BaseUrlError> {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return Ok(Ok(()));
    };
    let page = origin.to_str().ok().and_then(|o| BaseUrl::parse(o).ok());
    if page
        .as_ref()
        .is_some_and(|page| public_url.is_some_and(|pin| page.same_host(pin)))
    {
        return Ok(Ok(()));
    }
    let addressed = crate::base_url::addressed_to(headers, uri)?;
    if page.as_ref().is_some_and(|page| page.same_host(&addressed)) {
        return Ok(Ok(()));
    }
    let shown = String::from_utf8_lossy(origin.as_bytes()).into_owned();
    let pin = match public_url {
        Some(pin) => format!("PUBLIC_URL names {}", pin.host_str()),
        None => "PUBLIC_URL is not set".to_string(),
    };
    Ok(Err(format!(
        "its page is at {shown}, the request was sent to {} and {pin}. A reverse proxy must pass \
         the public name in Host or X-Forwarded-Host, or PUBLIC_URL must name it",
        addressed.host_str()
    )))
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

/// What a new download socket sends before the live events. A frame goes only to a client that
/// asked for it: a bundle older than the variant does not ask, since it cannot decode it and
/// would reconnect on every try. The build frame comes first, then the user's unfinished files:
/// as a snapshot, empty or not, since it replaces whatever an earlier socket left in the panel,
/// or to an older bundle as progress rows when there are any.
#[cfg(feature = "server")]
fn first_frames(
    build_frame: Option<bool>,
    snapshot: Option<bool>,
    rows: Vec<DownloadProgress>,
) -> Vec<DownloadEvent> {
    let build = (build_frame == Some(true))
        .then(|| DownloadEvent::Build(crate::build_id::BUILD_ID.to_string()));
    let rows = if snapshot == Some(true) {
        Some(DownloadEvent::Snapshot(rows))
    } else {
        (!rows.is_empty()).then_some(DownloadEvent::Progress(rows))
    };
    build.into_iter().chain(rows).collect()
}

/// What a socket sends for one receive from `user`'s channel, or why it closes. Events carry
/// changed rows only, and the ones a lag skipped may hold a file's last row, so after a lag a
/// client that asked for snapshots gets the snapshot again; the events kept after the skipped
/// ones still follow it. An older bundle cannot decode one, so its socket closes and the client
/// reconnects onto the unfinished rows.
#[cfg(feature = "server")]
async fn frame_for(
    received: Result<DownloadEvent, broadcast::error::RecvError>,
    snapshot: Option<bool>,
    db: &sqlx::SqlitePool,
    user: &UserId,
) -> Result<DownloadEvent, String> {
    match received {
        Ok(event) => Ok(event),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!("Download updates of user {user} lagged, {skipped} events skipped");
            if snapshot != Some(true) {
                return Err("it lagged and its client takes no snapshot".to_string());
            }
            crate::ledger::store::open_rows(db, user)
                .await
                .map(DownloadEvent::Snapshot)
                .map_err(|e| {
                    warn!("Could not read the unfinished downloads of user {user}: {e}");
                    "its snapshot could not be read".to_string()
                })
        }
        Err(broadcast::error::RecvError::Closed) => Err("the broadcast channel closed".to_string()),
    }
}

/// WebSocket endpoint for real-time download updates.
/// Uses WebSocket instead of HTTP streaming for more reliable delivery.
/// A client that passes `build_frame=true` gets `DownloadEvent::Build` first, and one that
/// passes `snapshot=true` gets `DownloadEvent::Snapshot` on opening and after a lag.
#[get("/api/downloads/updates?build_frame&snapshot", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn download_updates_ws(
    build_frame: Option<bool>,
    snapshot: Option<bool>,
    options: WebSocketOptions,
) -> Result<Websocket<(), DownloadEvent>, ServerFnError> {
    let user = auth.0.id();

    let public_url = crate::config::CONFIG
        .public_url()
        .and_then(|p| p.as_ref().ok());
    let allowed = origin_allowed(&headers, &uri, public_url)
        .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
    if let Err(why) = allowed {
        warn!("Refused a download updates socket for user {user}: {why}");
        return Err(forbidden_error(
            "Download updates can only be opened from Soulbeet's own page",
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
    // then shows the slot's stored row until its next one. Without the read the socket would
    // leave the panel's rows as an earlier socket left them, so the client is refused and tries
    // again.
    let rows = crate::ledger::store::open_rows(&crate::db::DB, &user)
        .await
        .map_err(|e| {
            warn!("Could not read the unfinished downloads of user {user}: {e}");
            server_error("Could not read your downloads")
        })?;

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut rx = rx;
        info!("WebSocket connected for user: {}", user);
        for frame in first_frames(build_frame, snapshot, rows) {
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
                // Only the receive races the other arms: a snapshot read once a lag was taken
                // must not be dropped for a tick.
                result = rx.recv() => {
                    let frame = match frame_for(result, snapshot, &crate::db::DB, &user).await {
                        Ok(frame) => frame,
                        Err(why) => {
                            info!("WebSocket closed for user {}: {}", user, why);
                            break;
                        }
                    };
                    if socket.send(frame).await.is_err() {
                        info!("WebSocket closed (client disconnected)");
                        break;
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
    crate::core::acquire::cancel(
        &auth.0,
        &crate::services::Registry,
        req.backend.as_deref(),
        &req.source,
        &req.id,
    )
    .await
    .map_err(failure_error)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub items: Vec<DownloadableItem>,
    /// The search the items were picked from.
    pub search_id: String,
}

#[post("/api/downloads/queue", auth: AuthSession)]
pub async fn download(req: DownloadRequest) -> Result<Vec<QueuedDownload>, ServerFnError> {
    let folder = crate::core::folders::for_user(&auth.0.user_id)
        .await
        .map_err(failure_error)?;
    crate::core::acquire::download(&auth.0, &folder, req.items, &req.search_id)
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
    fn a_socket_opens_with_the_build_then_the_unfinished_rows_as_its_client_asked() {
        let row = DownloadProgress::queued("t-1".into(), "peer_a".into(), "01.flac".into(), 100);
        let kinds = |frames: Vec<DownloadEvent>| {
            frames
                .into_iter()
                .map(|f| match f {
                    DownloadEvent::Build(id) => {
                        format!("build {}", id == crate::build_id::BUILD_ID)
                    }
                    DownloadEvent::Snapshot(rows) => format!("snapshot {}", rows.len()),
                    DownloadEvent::Progress(rows) => format!("rows {}", rows.len()),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(first_frames(Some(true), Some(true), vec![row.clone()])),
            vec!["build true", "snapshot 1"]
        );
        assert_eq!(
            kinds(first_frames(None, Some(true), vec![row.clone()])),
            vec!["snapshot 1"]
        );
        // Empty, it still clears what an earlier socket left in the panel.
        assert_eq!(
            kinds(first_frames(Some(false), Some(true), vec![])),
            vec!["snapshot 0"]
        );
        // A bundle older than the snapshot cannot decode one.
        assert_eq!(
            kinds(first_frames(Some(true), None, vec![row.clone()])),
            vec!["build true", "rows 1"]
        );
        assert_eq!(
            kinds(first_frames(None, Some(false), vec![row])),
            vec!["rows 1"]
        );
        assert!(first_frames(None, None, vec![]).is_empty());
    }

    #[tokio::test]
    async fn a_socket_that_lagged_sends_the_snapshot_then_the_events_kept() {
        use crate::ledger::{JobEvent, JobKind, JobState, Ledger, NewJob};
        use crate::server_fns::download::test_support::item;
        use shared::download::{DownloadQuery, DownloadState, JobSlot};

        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: soulbeet::ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                    picked: vec![(0, item("peer_a", "Wake Up", "dirA/aaa.flac"))],
                    kind: JobKind::Track,
                    options: None,
                },
                None,
            )
            .await
            .unwrap();
        let running =
            DownloadProgress::queued("t-1".into(), "peer_a".into(), "dirA/aaa.flac".into(), 100)
                .with_state(DownloadState::InProgress)
                .with_job(JobSlot {
                    job: "j1".into(),
                    slot: 0,
                });
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![running.clone()],
        });
        ledger.settled().await;

        let alice = UserId::new("alice");
        let (tx, mut rx) = broadcast::channel(1);
        let mut older_bundle = tx.subscribe();
        // The last row of a file, lost to the lag.
        let ended = DownloadProgress::queued("t-2".into(), "peer_a".into(), "02.flac".into(), 100)
            .with_state(DownloadState::Imported);
        tx.send(DownloadEvent::Progress(vec![ended])).unwrap();
        tx.send(DownloadEvent::Progress(vec![running.clone()]))
            .unwrap();

        match frame_for(rx.recv().await, Some(true), &db, &alice).await {
            Ok(DownloadEvent::Snapshot(rows)) => assert_eq!(
                rows.iter()
                    .map(|r| (r.item.as_str(), r.state.clone()))
                    .collect::<Vec<_>>(),
                vec![("dirA/aaa.flac", DownloadState::InProgress)]
            ),
            other => panic!("not a snapshot: {other:?}"),
        }
        match frame_for(rx.recv().await, Some(true), &db, &alice).await {
            Ok(DownloadEvent::Progress(rows)) => assert_eq!(rows, vec![running]),
            other => panic!("not the kept event: {other:?}"),
        }
        // A bundle older than the snapshot cannot decode one: its socket closes instead.
        assert!(frame_for(older_bundle.recv().await, None, &db, &alice)
            .await
            .is_err());
        drop(tx);
        assert!(frame_for(rx.recv().await, Some(true), &db, &alice)
            .await
            .is_err());
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
            origin_allowed(&headers, &Uri::from_static("/api/downloads/updates"), None)
                .map(|allowed| allowed.is_ok())
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

    #[test]
    fn a_page_at_the_public_url_may_open_the_socket_through_a_proxy_that_drops_the_name() {
        use axum::http::{HeaderName, HeaderValue};
        let headers = |pairs: &[(&str, &str)]| -> HeaderMap {
            pairs
                .iter()
                .map(|(k, v)| {
                    (
                        HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        HeaderValue::from_str(v).unwrap(),
                    )
                })
                .collect()
        };
        let uri = Uri::from_static("/api/downloads/updates");
        // nginx without `proxy_set_header Host $host` sends its upstream name.
        let proxied = headers(&[
            ("host", "soulbeet:9765"),
            ("origin", "https://music.example.com"),
        ]);
        let pin = BaseUrl::parse("https://music.example.com").unwrap();
        assert_eq!(origin_allowed(&proxied, &uri, Some(&pin)), Ok(Ok(())));
        assert_eq!(
            origin_allowed(&proxied, &uri, None),
            Ok(Err(
                "its page is at https://music.example.com, the request was sent to \
                 soulbeet:9765 and PUBLIC_URL is not set. A reverse proxy must pass the public \
                 name in Host or X-Forwarded-Host, or PUBLIC_URL must name it"
                    .to_string()
            ))
        );
        let other = BaseUrl::parse("https://other.example.com").unwrap();
        let refused = origin_allowed(&proxied, &uri, Some(&other))
            .unwrap()
            .unwrap_err();
        assert!(
            refused.contains("PUBLIC_URL names other.example.com"),
            "{refused}"
        );
        // The page's own address still works with PUBLIC_URL set elsewhere.
        let lan = headers(&[
            ("host", "192.168.1.5:9765"),
            ("origin", "http://192.168.1.5:9765"),
        ]);
        assert_eq!(origin_allowed(&lan, &uri, Some(&pin)), Ok(Ok(())));
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
