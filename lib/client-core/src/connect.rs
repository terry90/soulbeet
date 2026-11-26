use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, watch, Mutex as TokioMutex, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use url::Url;

use crate::error::{CoreError, CoreResult};
use crate::models::{
    ConnectClientMessageDto, ConnectCommandDto, ConnectDeviceDto, ConnectServerEventDto,
    DevicePlatform, PlaybackSnapshotDto, SoulbeetAuth,
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
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    state: Arc<RwLock<ConnectState>>,
    state_tx: watch::Sender<ConnectState>,
    events_tx: broadcast::Sender<ConnectServerEventDto>,
    outbound_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ConnectClientMessageDto>>>>,
    tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    conn_gen: Arc<AtomicU64>,
    connect_op_lock: TokioMutex<()>,
}

impl ConnectClient {
    pub fn new(
        device_id: String,
        device_name: String,
        platform: DevicePlatform,
        soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(64);
        let (state_tx, _) = watch::channel(ConnectState::default());
        Self {
            device_id,
            device_name,
            platform,
            soulbeet_auth,
            state: Arc::new(RwLock::new(ConnectState::default())),
            state_tx,
            events_tx,
            outbound_tx: Arc::new(RwLock::new(None)),
            tasks: Arc::new(StdMutex::new(Vec::new())),
            conn_gen: Arc::new(AtomicU64::new(0)),
            connect_op_lock: TokioMutex::new(()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ConnectServerEventDto> {
        self.events_tx.subscribe()
    }

    pub fn subscribe_state(&self) -> watch::Receiver<ConnectState> {
        self.state_tx.subscribe()
    }

    pub async fn state(&self) -> ConnectState {
        self.state.read().await.clone()
    }

    /// Connects to `ws(s)://<soulbeet_base_url>/api/ws/connect?device_id=...&name=...&platform=...`,
    /// registers this device, starts periodic heartbeats (clamped to `>= 1s`), and streams server
    /// events encoded as either WebSocket Text or Binary JSON frames.
    pub async fn connect(
        &self,
        soulbeet_base_url: &str,
        heartbeat_interval: Duration,
    ) -> CoreResult<()> {
        let _op_guard = self.connect_op_lock.lock().await;
        self.disconnect_locked().await;

        let heartbeat_interval = heartbeat_interval.max(Duration::from_secs(1));
        let ws_base = http_to_ws_url(soulbeet_base_url, "/api/ws/connect")?;
        let mut ws_url = Url::parse(&ws_base)?;
        ws_url
            .query_pairs_mut()
            .append_pair("device_id", &self.device_id)
            .append_pair("name", &self.device_name)
            .append_pair("platform", self.platform.as_str());

        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| CoreError::WebSocket(e.to_string()))?;

        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            let header_name = HeaderName::from_static(name);
            let header_val = HeaderValue::from_str(&val)
                .map_err(|e| CoreError::WebSocket(format!("invalid auth header: {e}")))?;
            request.headers_mut().insert(header_name, header_val);
        }

        let (ws_stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| match e {
                WsError::Http(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp
                        .into_body()
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    CoreError::ServerHttp { status, body }
                }
                other => CoreError::WebSocket(other.to_string()),
            })?;

        let gen = self.conn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ConnectClientMessageDto>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Register immediately as first frame so servers without query-param bootstrap also see it.
        let _ = out_tx.send(ConnectClientMessageDto::Register {
            device_id: self.device_id.clone(),
            name: self.device_name.clone(),
            platform: self.platform,
        });

        let connected_state = {
            let mut out_guard = self.outbound_tx.write().await;
            let mut st = self.state.write().await;
            *out_guard = Some(out_tx.clone());
            st.devices.clear();
            st.active_device_id = None;
            st.snapshots.clear();
            st.connected = true;
            st.clone()
        };
        let _ = self.state_tx.send(connected_state);

        let writer_state = self.state.clone();
        let writer_state_tx = self.state_tx.clone();
        let writer_outbound = self.outbound_tx.clone();
        let writer_gen = self.conn_gen.clone();
        let writer_shutdown_tx = shutdown_tx.clone();
        let mut writer_shutdown_rx = shutdown_rx.clone();
        let writer_task = tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = writer_shutdown_rx.changed() => break,
                    maybe = out_rx.recv() => match maybe {
                        Some(m) => m,
                        None => break,
                    }
                };
                let Ok(text) = serde_json::to_string(&msg) else {
                    continue;
                };
                let send_res = tokio::select! {
                    _ = writer_shutdown_rx.changed() => break,
                    res = ws_sink.send(Message::Text(text.into())) => res,
                };
                if send_res.is_err() {
                    break;
                }
            }
            let _ = writer_shutdown_tx.send(true);
            let disconnected = {
                let mut out_guard = writer_outbound.write().await;
                let mut st = writer_state.write().await;
                if writer_gen.load(Ordering::SeqCst) == gen {
                    *out_guard = None;
                    clear_state_on_disconnect(&mut st)
                } else {
                    None
                }
            };
            if let Some(st) = disconnected {
                let _ = writer_state_tx.send(st);
            }
        });

        let hb_tx = out_tx;
        let mut hb_shutdown_rx = shutdown_rx.clone();
        let hb_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(heartbeat_interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    _ = hb_shutdown_rx.changed() => break,
                    _ = ticker.tick() => {
                        if hb_tx.send(ConnectClientMessageDto::Heartbeat).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let reader_state = self.state.clone();
        let reader_state_tx = self.state_tx.clone();
        let reader_outbound = self.outbound_tx.clone();
        let reader_gen = self.conn_gen.clone();
        let reader_shutdown_tx = shutdown_tx;
        let mut reader_shutdown_rx = shutdown_rx;
        let events_tx = self.events_tx.clone();
        let reader_task = tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    _ = reader_shutdown_rx.changed() => break,
                    next = ws_source.next() => match next {
                        Some(f) => f,
                        None => break,
                    }
                };
                let Ok(msg) = frame else {
                    break;
                };
                let decoded: Option<ConnectServerEventDto> = match msg {
                    Message::Text(text) => match serde_json::from_str::<ConnectServerEventDto>(&text) {
                        Ok(ev) => Some(ev),
                        Err(err) => {
                            tracing::warn!("failed to decode Connect WS text frame: {err}");
                            None
                        }
                    },
                    Message::Binary(bytes) => {
                        match serde_json::from_slice::<ConnectServerEventDto>(&bytes) {
                            Ok(ev) => Some(ev),
                            Err(err) => {
                                tracing::warn!("failed to decode Connect WS binary frame: {err}");
                                None
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => None,
                };

                if let Some(event) = decoded {
                    let (still_current, updated_state) = {
                        let mut st = reader_state.write().await;
                        if reader_gen.load(Ordering::SeqCst) != gen {
                            (false, None)
                        } else {
                            let state_snapshot = match &event {
                                ConnectServerEventDto::DeviceList {
                                    devices,
                                    active_device_id,
                                } => {
                                    let live_ids: HashSet<&str> =
                                        devices.iter().map(|d| d.device_id.as_str()).collect();
                                    st.snapshots
                                        .retain(|dev_id, _| live_ids.contains(dev_id.as_str()));
                                    st.devices = devices.clone();
                                    st.active_device_id = active_device_id.clone();
                                    for dev in devices {
                                        if let Some(snap) = &dev.snapshot {
                                            st.snapshots
                                                .insert(dev.device_id.clone(), snap.clone());
                                        }
                                    }
                                    Some(st.clone())
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
                                    Some(st.clone())
                                }
                                ConnectServerEventDto::Command { .. } => None,
                            };
                            (true, state_snapshot)
                        }
                    };
                    if !still_current {
                        break;
                    }
                    if let Some(st) = updated_state {
                        let _ = reader_state_tx.send(st);
                    }
                    let _ = events_tx.send(event);
                }
            }
            let _ = reader_shutdown_tx.send(true);
            let disconnected = {
                let mut out_guard = reader_outbound.write().await;
                let mut st = reader_state.write().await;
                if reader_gen.load(Ordering::SeqCst) == gen {
                    *out_guard = None;
                    clear_state_on_disconnect(&mut st)
                } else {
                    None
                }
            };
            if let Some(st) = disconnected {
                let _ = reader_state_tx.send(st);
            }
        });

        if let Ok(mut tasks) = self.tasks.lock() {
            for handle in tasks.drain(..) {
                handle.abort();
            }
            *tasks = vec![writer_task, hb_task, reader_task];
        }
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
        let _op_guard = self.connect_op_lock.lock().await;
        self.disconnect_locked().await;
    }

    async fn disconnect_locked(&self) {
        self.conn_gen.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut tasks) = self.tasks.lock() {
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        let disconnected = {
            let mut out_guard = self.outbound_tx.write().await;
            let mut st = self.state.write().await;
            *out_guard = None;
            clear_state_on_disconnect(&mut st)
        };
        if let Some(st) = disconnected {
            let _ = self.state_tx.send(st);
        }
    }
}

fn clear_state_on_disconnect(st: &mut ConnectState) -> Option<ConnectState> {
    let changed = st.connected
        || !st.devices.is_empty()
        || st.active_device_id.is_some()
        || !st.snapshots.is_empty();
    if !changed {
        return None;
    }
    st.connected = false;
    st.devices.clear();
    st.active_device_id = None;
    st.snapshots.clear();
    Some(st.clone())
}

impl Drop for ConnectClient {
    fn drop(&mut self) {
        self.conn_gen.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut tasks) = self.tasks.lock() {
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
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
