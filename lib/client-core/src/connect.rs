use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{CoreError, CoreResult};
use crate::models::{
    ConnectClientMessageDto, ConnectCommandDto, ConnectDeviceDto, ConnectServerEventDto,
    DevicePlatform, PlaybackSnapshotDto,
};

/// Live multi-device Connect state cached in memory by [`ConnectClient`].
#[derive(Debug, Clone, Default)]
pub struct ConnectState {
    pub connected: bool,
    pub devices: Vec<ConnectDeviceDto>,
    pub active_device_id: Option<String>,
    pub snapshots: HashMap<String, PlaybackSnapshotDto>,
}

/// WebSocket client for Soulbeet's `WS /api/ws/connect` multi-device presence and handoff channel.
pub struct ConnectClient {
    device_id: String,
    device_name: String,
    platform: DevicePlatform,
    state: Arc<RwLock<ConnectState>>,
    events_tx: broadcast::Sender<ConnectServerEventDto>,
    outbound_tx: RwLock<Option<mpsc::UnboundedSender<ConnectClientMessageDto>>>,
    tasks: RwLock<Vec<JoinHandle<()>>>,
}

impl ConnectClient {
    pub fn new(device_id: String, device_name: String, platform: DevicePlatform) -> Self {
        let (events_tx, _) = broadcast::channel(64);
        Self {
            device_id,
            device_name,
            platform,
            state: Arc::new(RwLock::new(ConnectState::default())),
            events_tx,
            outbound_tx: RwLock::new(None),
            tasks: RwLock::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ConnectServerEventDto> {
        self.events_tx.subscribe()
    }

    pub async fn state(&self) -> ConnectState {
        self.state.read().await.clone()
    }

    /// Connects to `ws(s)://<soulbeet_base_url>/api/ws/connect`, registers this device,
    /// starts periodic heartbeats, and begins streaming server events.
    pub async fn connect(
        &self,
        soulbeet_base_url: &str,
        auth_header: Option<&str>,
        heartbeat_interval: Duration,
    ) -> CoreResult<()> {
        self.disconnect().await;

        let ws_url = http_to_ws_url(soulbeet_base_url, "/api/ws/connect")?;
        let mut request = ws_url
            .into_client_request()
            .map_err(|e| CoreError::WebSocket(e.to_string()))?;

        if let Some(auth) = auth_header {
            let val = HeaderValue::from_str(auth)
                .map_err(|e| CoreError::WebSocket(format!("invalid auth header: {e}")))?;
            request.headers_mut().insert("Authorization", val);
        }

        let (ws_stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| CoreError::WebSocket(e.to_string()))?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ConnectClientMessageDto>();

        // Register immediately as first frame.
        let _ = out_tx.send(ConnectClientMessageDto::Register {
            device_id: self.device_id.clone(),
            name: self.device_name.clone(),
            platform: self.platform,
        });

        *self.outbound_tx.write().await = Some(out_tx.clone());
        self.state.write().await.connected = true;

        let writer_state = self.state.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Ok(text) = serde_json::to_string(&msg) else {
                    continue;
                };
                if ws_sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            writer_state.write().await.connected = false;
        });

        let hb_tx = out_tx;
        let hb_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(heartbeat_interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                if hb_tx.send(ConnectClientMessageDto::Heartbeat).is_err() {
                    break;
                }
            }
        });

        let reader_state = self.state.clone();
        let events_tx = self.events_tx.clone();
        let reader_task = tokio::spawn(async move {
            while let Some(frame) = ws_source.next().await {
                let Ok(msg) = frame else {
                    break;
                };
                if let Message::Text(text) = msg {
                    if let Ok(event) = serde_json::from_str::<ConnectServerEventDto>(&text) {
                        {
                            let mut st = reader_state.write().await;
                            match &event {
                                ConnectServerEventDto::DeviceList {
                                    devices,
                                    active_device_id,
                                } => {
                                    st.devices = devices.clone();
                                    st.active_device_id = active_device_id.clone();
                                    for dev in devices {
                                        if let Some(snap) = &dev.snapshot {
                                            st.snapshots.insert(dev.device_id.clone(), snap.clone());
                                        }
                                    }
                                }
                                ConnectServerEventDto::PlaybackUpdated {
                                    device_id,
                                    snapshot,
                                } => {
                                    st.snapshots.insert(device_id.clone(), snapshot.clone());
                                    if let Some(dev) =
                                        st.devices.iter_mut().find(|d| d.device_id == *device_id)
                                    {
                                        dev.snapshot = Some(snapshot.clone());
                                    }
                                }
                                ConnectServerEventDto::Command { .. } => {}
                            }
                        }
                        let _ = events_tx.send(event);
                    }
                } else if matches!(msg, Message::Close(_)) {
                    break;
                }
            }
            reader_state.write().await.connected = false;
        });

        *self.tasks.write().await = vec![writer_task, hb_task, reader_task];
        Ok(())
    }

    pub async fn send(&self, msg: ConnectClientMessageDto) -> CoreResult<()> {
        let guard = self.outbound_tx.read().await;
        let tx = guard
            .as_ref()
            .ok_or_else(|| CoreError::WebSocket("connect socket is not connected".to_string()))?;
        tx.send(msg)
            .map_err(|e| CoreError::WebSocket(format!("send failed: {e}")))
    }

    pub async fn update_state(&self, snapshot: PlaybackSnapshotDto) -> CoreResult<()> {
        self.send(ConnectClientMessageDto::UpdateState { snapshot })
            .await
    }

    pub async fn send_command(
        &self,
        target_device_id: String,
        command: ConnectCommandDto,
    ) -> CoreResult<()> {
        self.send(ConnectClientMessageDto::SendCommand {
            target_device_id,
            command,
        })
        .await
    }

    pub async fn claim_active(&self) -> CoreResult<()> {
        self.send(ConnectClientMessageDto::ClaimActive).await
    }

    pub async fn transfer_playback_to(
        &self,
        target_device_id: String,
        snapshot: PlaybackSnapshotDto,
    ) -> CoreResult<()> {
        self.send_command(
            target_device_id,
            ConnectCommandDto::TransferPlayback {
                from_device_id: self.device_id.clone(),
                snapshot,
            },
        )
        .await
    }

    pub async fn disconnect(&self) {
        *self.outbound_tx.write().await = None;
        let mut tasks = self.tasks.write().await;
        for handle in tasks.drain(..) {
            handle.abort();
        }
        self.state.write().await.connected = false;
    }
}

fn http_to_ws_url(base_url: &str, path: &str) -> CoreResult<String> {
    let trimmed = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        return Err(CoreError::InvalidInput(format!(
            "unsupported base URL scheme: {base_url}"
        )));
    };
    Ok(format!("{ws_base}{path}"))
}
