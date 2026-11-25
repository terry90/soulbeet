use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use shared::client::{ConnectClientMessage, ConnectServerEvent};

#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};
#[cfg(feature = "server")]
use shared::client::DevicePlatform;
#[cfg(feature = "server")]
use tokio::sync::broadcast;
#[cfg(feature = "server")]
use tower_cookies::Cookies;

#[cfg(feature = "server")]
use crate::{
    core::connect::CONNECT_HUB,
    server_fns::{
        download::{origin_allowed, session_still_valid},
        forbidden_error, refusal_error,
    },
    AuthSession,
};

#[cfg(feature = "server")]
const SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(15);

/// Per-user multi-device Connect WebSocket (`WS /api/ws/connect`).
///
/// Registers the connecting client device (or waits for an explicit `Register` frame when
/// `device_id` query param is omitted), streams live `DeviceList`, `PlaybackUpdated`, and
/// targeted `Command` events, and removes the device cleanly on disconnect.
#[get("/api/ws/connect?device_id&name&platform", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn connect_ws(
    device_id: Option<String>,
    name: Option<String>,
    platform: Option<String>,
    options: WebSocketOptions,
) -> Result<Websocket<ConnectClientMessage, ConnectServerEvent>, ServerFnError> {
    let user = auth.0.id();

    let public_url = crate::config::CONFIG
        .public_url()
        .and_then(|p| p.as_ref().ok());
    let allowed = origin_allowed(&headers, &uri, public_url)
        .map_err(|e| refusal_error(format!("Cannot tell this request's address: {e}")))?;
    if let Err(why) = allowed {
        warn!("Refused a Connect socket for user {user}: {why}");
        return Err(forbidden_error(
            "Connect WebSocket can only be opened from Soulbeet's own origin or a native client",
        ));
    }

    let token = cookies
        .get(crate::AUTH_COOKIE_NAME)
        .map(|c| c.value().to_string());

    let initial_device_id = device_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let initial_name = name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Soulbeet Client".to_string());
    let initial_platform = platform
        .as_deref()
        .map(DevicePlatform::parse)
        .unwrap_or(DevicePlatform::Web);

    let (initial_list, rx) = CONNECT_HUB
        .register(&user, initial_device_id.clone(), initial_name, initial_platform)
        .await;

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut current_device_id = initial_device_id;
        let mut rx = rx;
        info!(
            "Connect WS connected for user {} (device {})",
            user, current_device_id
        );

        let _ = socket.send(initial_list).await;

        let mut recheck = tokio::time::interval_at(
            tokio::time::Instant::now() + SESSION_RECHECK,
            SESSION_RECHECK,
        );

        loop {
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
                        info!("Connect WS closed for user {}: session ended", user);
                        break;
                    }
                }
                received = rx.recv() => {
                    match received {
                        Ok(env) => {
                            let for_this_device = env.target_device_id.is_none()
                                || env.target_device_id.as_deref() == Some(current_device_id.as_str());
                            if for_this_device && socket.send(env.event).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                "Connect WS for user {} (device {}) lagged by {} events",
                                user, current_device_id, skipped
                            );
                            let devices = CONNECT_HUB.devices_for(&user).await;
                            let active_device_id = devices
                                .iter()
                                .find(|d| d.is_active)
                                .map(|d| d.device_id.clone());
                            if socket
                                .send(ConnectServerEvent::DeviceList {
                                    devices,
                                    active_device_id,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                incoming = socket.recv() => {
                    match incoming {
                        Ok(ConnectClientMessage::Register { device_id, name, platform }) => {
                            if !device_id.trim().is_empty() && device_id != current_device_id {
                                CONNECT_HUB.unregister(&user, &current_device_id).await;
                                current_device_id = device_id;
                            }
                            let (list, new_rx) = CONNECT_HUB
                                .register(&user, current_device_id.clone(), name, platform)
                                .await;
                            rx = new_rx;
                            if socket.send(list).await.is_err() {
                                break;
                            }
                        }
                        Ok(ConnectClientMessage::Heartbeat) => {
                            CONNECT_HUB.heartbeat(&user, &current_device_id).await;
                        }
                        Ok(ConnectClientMessage::UpdateState { snapshot }) => {
                            CONNECT_HUB
                                .update_state(&user, &current_device_id, snapshot)
                                .await;
                        }
                        Ok(ConnectClientMessage::SendCommand { target_device_id, command }) => {
                            CONNECT_HUB
                                .send_command(&user, &current_device_id, &target_device_id, command)
                                .await;
                        }
                        Ok(ConnectClientMessage::ClaimActive) => {
                            CONNECT_HUB.claim_active(&user, &current_device_id).await;
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        CONNECT_HUB.unregister(&user, &current_device_id).await;
        info!(
            "Connect WS disconnected for user {} (device {})",
            user, current_device_id
        );
    }))
}
