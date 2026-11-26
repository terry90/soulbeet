use dioxus::fullstack::{WebSocketOptions, Websocket};
use dioxus::prelude::*;
use shared::client::{ConnectClientMessage, ConnectServerEvent};

#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use dioxus::fullstack::WebsocketError;
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
    core::connect::{ConnectEnvelope, CONNECT_HUB},
    server_fns::{
        download::{origin_allowed, session_still_valid},
        forbidden_error,
        guard::extract_session_token,
        refusal_error,
    },
    AuthSession,
};

#[cfg(feature = "server")]
const SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(15);

#[cfg(feature = "server")]
struct RegisteredDevice {
    device_id: String,
    conn_id: u64,
    rx: broadcast::Receiver<ConnectEnvelope>,
}

/// Per-user multi-device Connect WebSocket (`WS /api/ws/connect`).
///
/// Registers the connecting client device (or defers hub registration until an explicit
/// `Register` frame arrives when `device_id` query param is omitted), streams live
/// `DeviceList`, `PlaybackUpdated`, and targeted `Command` events, and removes the device
/// cleanly on disconnect.
#[allow(clippy::too_many_arguments)]
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

    let token = extract_session_token(Some(&cookies), &headers);

    let query_device_id = device_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let initial_name = name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Soulbeet Client".to_string());
    let initial_platform = platform
        .as_deref()
        .map(DevicePlatform::parse)
        .unwrap_or(DevicePlatform::Web);

    Ok(options.on_upgrade(move |mut socket| async move {
        let mut registered: Option<RegisteredDevice> = if let Some(dev_id) = query_device_id {
            let (conn_id, canonical_id, initial_list, rx) = CONNECT_HUB
                .register(&user, dev_id, initial_name, initial_platform)
                .await;
            info!(
                "Connect WS connected for user {} (device {})",
                user, canonical_id
            );
            if socket.send(initial_list).await.is_err() {
                drop(rx);
                CONNECT_HUB.unregister(&user, &canonical_id, conn_id).await;
                return;
            }
            Some(RegisteredDevice {
                device_id: canonical_id,
                conn_id,
                rx,
            })
        } else {
            info!(
                "Connect WS connected for user {} (awaiting Register frame)",
                user
            );
            None
        };

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
                received = async {
                    match registered.as_mut() {
                        Some(reg) => reg.rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(reg) = registered.as_ref() else {
                        continue;
                    };
                    match received {
                        Ok(env) => {
                            let for_this_device = env.target_device_id.is_none()
                                || env.target_device_id.as_deref() == Some(reg.device_id.as_str());
                            if for_this_device && socket.send(env.event).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                "Connect WS for user {} (device {}) lagged by {} events",
                                user, reg.device_id, skipped
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
                            let trimmed = device_id.trim();
                            let target_id = if !trimmed.is_empty() {
                                trimmed.to_string()
                            } else if let Some(ref reg) = registered {
                                reg.device_id.clone()
                            } else {
                                uuid::Uuid::new_v4().to_string()
                            };
                            let previous = registered
                                .as_ref()
                                .map(|r| (r.device_id.as_str(), r.conn_id));
                            let (conn_id, canonical_id, list, new_rx) = CONNECT_HUB
                                .reregister(&user, previous, target_id, name, platform)
                                .await;
                            registered = Some(RegisteredDevice {
                                device_id: canonical_id,
                                conn_id,
                                rx: new_rx,
                            });
                            if socket.send(list).await.is_err() {
                                break;
                            }
                        }
                        Ok(ConnectClientMessage::Heartbeat) => {
                            if let Some(ref reg) = registered {
                                CONNECT_HUB.heartbeat(&user, &reg.device_id).await;
                            }
                        }
                        Ok(ConnectClientMessage::UpdateState { snapshot }) => {
                            if let Some(ref reg) = registered {
                                CONNECT_HUB
                                    .update_state(&user, &reg.device_id, snapshot)
                                    .await;
                            }
                        }
                        Ok(ConnectClientMessage::SendCommand { target_device_id, command }) => {
                            if let Some(ref reg) = registered {
                                CONNECT_HUB
                                    .send_command(&user, &reg.device_id, &target_device_id, command)
                                    .await;
                            }
                        }
                        Ok(ConnectClientMessage::ClaimActive) => {
                            if let Some(ref reg) = registered {
                                CONNECT_HUB.claim_active(&user, &reg.device_id).await;
                            }
                        }
                        Err(
                            err @ (WebsocketError::Json(_)
                            | WebsocketError::Deserialization(_)
                            | WebsocketError::Cbor(_)),
                        ) => {
                            warn!(
                                "Ignoring malformed Connect WS frame for user {} (device {:?}): {}",
                                user,
                                registered.as_ref().map(|r| r.device_id.as_str()),
                                err
                            );
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        if let Some(reg) = registered.take() {
            let device_id = reg.device_id;
            let conn_id = reg.conn_id;
            drop(reg.rx);
            CONNECT_HUB.unregister(&user, &device_id, conn_id).await;
            info!(
                "Connect WS disconnected for user {} (device {})",
                user, device_id
            );
        } else {
            info!("Connect WS disconnected for user {} (unregistered)", user);
        }
    }))
}
