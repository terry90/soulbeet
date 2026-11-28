use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch, Mutex as TokioMutex, RwLock};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::error::{CoreError, CoreResult};
use crate::models::{
    ConnectClientMessageDto, ConnectCommandDto, ConnectDeviceDto, ConnectServerEventDto,
    DevicePlatform, PlaybackSnapshotDto, SoulbeetAuth,
};

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);
const HEALTHY_CONNECTION_RESET: Duration = Duration::from_secs(10);
const CONNECT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3600);
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const READ_LIVENESS_GRACE: Duration = Duration::from_secs(10);
const OUTBOUND_CHANNEL_CAPACITY: usize = 128;
const BROADCAST_EVENT_CAPACITY: usize = 256;

type ConnectWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum OutboundMessage {
    Client(ConnectClientMessageDto),
    Pong(tokio_tungstenite::tungstenite::Bytes),
}

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
    outbound_tx: Arc<RwLock<Option<mpsc::Sender<OutboundMessage>>>>,
    tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    cancel_tx: Arc<StdMutex<Option<watch::Sender<bool>>>>,
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
        let (events_tx, _) = broadcast::channel(BROADCAST_EVENT_CAPACITY);
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
            cancel_tx: Arc::new(StdMutex::new(None)),
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
    /// starts periodic heartbeats with WebSocket Ping frames (clamped to `>= 1s`), streams server
    /// events encoded as Text or Binary JSON frames, and supervises the connection with
    /// exponential-backoff auto-reconnect until `disconnect()` or `Drop`.
    ///
    /// If the initial dial fails with a transient network error (non-HTTP-401/403), the reconnect
    /// supervisor is still started in the background while returning the initial error to the caller.
    pub async fn connect(
        &self,
        soulbeet_base_url: &str,
        heartbeat_interval: Duration,
    ) -> CoreResult<()> {
        let heartbeat_interval =
            heartbeat_interval.clamp(MIN_HEARTBEAT_INTERVAL, MAX_HEARTBEAT_INTERVAL);
        let ws_base = http_to_ws_url(soulbeet_base_url, "/api/ws/connect")?;
        let mut ws_url = Url::parse(&ws_base)?;
        ws_url
            .query_pairs_mut()
            .append_pair("device_id", &self.device_id)
            .append_pair("name", &self.device_name)
            .append_pair("platform", self.platform.as_str());

        // Disconnect any previous session and claim a new generation WITHOUT holding the lock across I/O.
        let gen = {
            let _op_guard = self.connect_op_lock.lock().await;
            self.disconnect_locked().await;
            self.conn_gen.fetch_add(1, Ordering::SeqCst) + 1
        };

        let dial_result = dial_connect_ws(&ws_url, &self.soulbeet_auth).await;

        let _op_guard = self.connect_op_lock.lock().await;
        if self.conn_gen.load(Ordering::SeqCst) != gen {
            return Err(CoreError::WebSocket("connect cancelled".to_string()));
        }

        let (initial_conn, initial_err) = match dial_result {
            Ok(initial_stream) => {
                let (out_tx, out_rx) = mpsc::channel::<OutboundMessage>(OUTBOUND_CHANNEL_CAPACITY);
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
                (Some((initial_stream, out_tx, out_rx)), None)
            }
            Err(err @ CoreError::ServerHttp { status: 401 | 403, .. }) => {
                return Err(err);
            }
            Err(err) => (None, Some(err)),
        };

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let soulbeet_auth = self.soulbeet_auth.clone();
        let state = self.state.clone();
        let state_tx = self.state_tx.clone();
        let events_tx = self.events_tx.clone();
        let outbound_tx = self.outbound_tx.clone();
        let conn_gen = self.conn_gen.clone();

        let supervisor_task = tokio::spawn(async move {
            let mut current_conn = initial_conn;
            let mut backoff = INITIAL_RECONNECT_BACKOFF;

            loop {
                if *cancel_rx.borrow() || conn_gen.load(Ordering::SeqCst) != gen {
                    break;
                }

                let (ws_stream, active_out_tx, active_out_rx) = match current_conn.take() {
                    Some(conn) => conn,
                    None => {
                        let wait = backoff;
                        backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                        tracing::info!(
                            "Connect WS disconnected; reconnecting in {}s",
                            wait.as_secs()
                        );
                        tokio::select! {
                            _ = cancel_rx.changed() => break,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        if *cancel_rx.borrow() || conn_gen.load(Ordering::SeqCst) != gen {
                            break;
                        }
                        let dial_res = tokio::select! {
                            _ = cancel_rx.changed() => break,
                            res = dial_connect_ws(&ws_url, &soulbeet_auth) => res,
                        };
                        match dial_res {
                            Ok(new_stream) => {
                                if *cancel_rx.borrow() || conn_gen.load(Ordering::SeqCst) != gen {
                                    break;
                                }
                                let (new_out_tx, new_out_rx) =
                                    mpsc::channel::<OutboundMessage>(OUTBOUND_CHANNEL_CAPACITY);
                                let reconnected_state = {
                                    let mut out_guard = outbound_tx.write().await;
                                    let mut st = state.write().await;
                                    if conn_gen.load(Ordering::SeqCst) != gen {
                                        None
                                    } else {
                                        *out_guard = Some(new_out_tx.clone());
                                        st.devices.clear();
                                        st.active_device_id = None;
                                        st.snapshots.clear();
                                        st.connected = true;
                                        Some(st.clone())
                                    }
                                };
                                match reconnected_state {
                                    Some(st) => {
                                        let _ = state_tx.send(st);
                                        (new_stream, new_out_tx, new_out_rx)
                                    }
                                    None => break,
                                }
                            }
                            Err(CoreError::ServerHttp {
                                status: status @ (401 | 403),
                                ..
                            }) => {
                                tracing::warn!(
                                    status,
                                    "Connect WS reconnect rejected by auth; stopping reconnect loop"
                                );
                                break;
                            }
                            Err(err) => {
                                tracing::warn!(
                                    "Connect WS reconnect failed: {err}; next retry in {}s",
                                    backoff.as_secs()
                                );
                                continue;
                            }
                        }
                    }
                };

                let connected_at = Instant::now();
                run_single_connection(
                    ws_stream,
                    active_out_tx,
                    active_out_rx,
                    heartbeat_interval,
                    gen,
                    &conn_gen,
                    &state,
                    &state_tx,
                    &events_tx,
                    &mut cancel_rx,
                )
                .await;

                let disconnected = {
                    let mut out_guard = outbound_tx.write().await;
                    let mut st = state.write().await;
                    if conn_gen.load(Ordering::SeqCst) == gen {
                        *out_guard = None;
                        clear_state_on_disconnect(&mut st)
                    } else {
                        None
                    }
                };
                if let Some(st) = disconnected {
                    let _ = state_tx.send(st);
                }

                if *cancel_rx.borrow() || conn_gen.load(Ordering::SeqCst) != gen {
                    break;
                }

                if connected_at.elapsed() >= HEALTHY_CONNECTION_RESET {
                    backoff = INITIAL_RECONNECT_BACKOFF;
                }
            }
        });

        {
            let mut guard = self
                .cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(cancel_tx);
        }
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for handle in tasks.drain(..) {
                handle.abort();
            }
            *tasks = vec![supervisor_task];
        }
        if let Some(err) = initial_err {
            return Err(err);
        }
        Ok(())
    }

    pub async fn send(&self, msg: ConnectClientMessageDto) -> CoreResult<()> {
        let tx = {
            let guard = self.outbound_tx.read().await;
            guard
                .clone()
                .ok_or_else(|| CoreError::WebSocket("connect socket is not connected".to_string()))?
        };
        tx.try_send(OutboundMessage::Client(msg)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                CoreError::WebSocket("connect outbound queue full".to_string())
            }
            mpsc::error::TrySendError::Closed(_) => {
                CoreError::WebSocket("connect socket is closed".to_string())
            }
        })
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
        {
            let mut guard = self
                .cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(tx) = guard.take() {
                let _ = tx.send(true);
            }
        }
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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

async fn dial_connect_ws(
    ws_url: &Url,
    soulbeet_auth: &Arc<RwLock<Option<SoulbeetAuth>>>,
) -> CoreResult<ConnectWsStream> {
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| CoreError::WebSocket(e.to_string()))?;

    if let Some(auth) = soulbeet_auth.read().await.as_ref() {
        let (name, val) = auth.header_name_and_value();
        let header_name = HeaderName::from_static(name);
        let header_val = HeaderValue::from_str(&val)
            .map_err(|e| CoreError::WebSocket(format!("invalid auth header: {e}")))?;
        request.headers_mut().insert(header_name, header_val);
    }

    let (ws_stream, _) = tokio::time::timeout(
        CONNECT_DIAL_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| CoreError::WebSocket("connect handshake timed out".to_string()))?
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
    Ok(ws_stream)
}

#[allow(clippy::too_many_arguments)]
async fn run_single_connection(
    ws_stream: ConnectWsStream,
    out_tx: mpsc::Sender<OutboundMessage>,
    mut out_rx: mpsc::Receiver<OutboundMessage>,
    heartbeat_interval: Duration,
    gen: u64,
    conn_gen: &Arc<AtomicU64>,
    state: &Arc<RwLock<ConnectState>>,
    state_tx: &watch::Sender<ConnectState>,
    events_tx: &broadcast::Sender<ConnectServerEventDto>,
    cancel_rx: &mut watch::Receiver<bool>,
) {
    let (mut ws_sink, mut ws_source) = ws_stream.split();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut workers = JoinSet::new();

    let writer_shutdown_tx = shutdown_tx.clone();
    let mut writer_shutdown_rx = shutdown_rx.clone();
    workers.spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = writer_shutdown_rx.changed() => break,
                maybe = out_rx.recv() => match maybe {
                    Some(m) => m,
                    None => break,
                }
            };
            match msg {
                OutboundMessage::Pong(payload) => {
                    let pong_res = tokio::select! {
                        _ = writer_shutdown_rx.changed() => break,
                        res = tokio::time::timeout(WS_WRITE_TIMEOUT, ws_sink.send(Message::Pong(payload))) => res,
                    };
                    if !matches!(pong_res, Ok(Ok(()))) {
                        break;
                    }
                }
                OutboundMessage::Client(client_msg) => {
                    let is_heartbeat = matches!(client_msg, ConnectClientMessageDto::Heartbeat);
                    let Ok(text) = serde_json::to_string(&client_msg) else {
                        continue;
                    };
                    let send_res = tokio::select! {
                        _ = writer_shutdown_rx.changed() => break,
                        res = tokio::time::timeout(WS_WRITE_TIMEOUT, ws_sink.send(Message::Text(text.into()))) => res,
                    };
                    if !matches!(send_res, Ok(Ok(()))) {
                        break;
                    }
                    // Send a WebSocket Ping frame alongside every Heartbeat so reverse proxies
                    // (e.g. Nginx Proxy Manager 60s proxy_read_timeout) see bidirectional traffic
                    // and the peer's Pong refreshes our read liveness deadline.
                    if is_heartbeat {
                        let ping_res = tokio::select! {
                            _ = writer_shutdown_rx.changed() => break,
                            res = tokio::time::timeout(WS_WRITE_TIMEOUT, ws_sink.send(Message::Ping(Vec::new().into()))) => res,
                        };
                        if !matches!(ping_res, Ok(Ok(()))) {
                            break;
                        }
                    }
                }
            }
        }
        let _ = writer_shutdown_tx.send(true);
    });

    let hb_out_tx = out_tx.clone();
    let hb_shutdown_tx = shutdown_tx.clone();
    let mut hb_shutdown_rx = shutdown_rx.clone();
    workers.spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat_interval);
        ticker.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = hb_shutdown_rx.changed() => break,
                _ = ticker.tick() => {
                    match hb_out_tx.try_send(OutboundMessage::Client(ConnectClientMessageDto::Heartbeat)) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!("Connect WS outbound queue full on heartbeat; recycling connection");
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
        }
        let _ = hb_shutdown_tx.send(true);
    });

    let read_liveness_timeout = heartbeat_interval
        .saturating_mul(2)
        .saturating_add(READ_LIVENESS_GRACE);
    let reader_state = state.clone();
    let reader_state_tx = state_tx.clone();
    let reader_gen = conn_gen.clone();
    let reader_shutdown_tx = shutdown_tx;
    let mut reader_shutdown_rx = shutdown_rx;
    let reader_events_tx = events_tx.clone();
    let reader_out_tx = out_tx;
    workers.spawn(async move {
        loop {
            let frame = tokio::select! {
                _ = reader_shutdown_rx.changed() => break,
                next = tokio::time::timeout(read_liveness_timeout, ws_source.next()) => match next {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(
                            timeout_secs = read_liveness_timeout.as_secs(),
                            "Connect WS read liveness timeout elapsed; closing half-open socket"
                        );
                        break;
                    }
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
                Message::Ping(payload) => {
                    let _ = reader_out_tx.try_send(OutboundMessage::Pong(payload));
                    None
                }
                Message::Pong(_) => None,
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
                let _ = reader_events_tx.send(event);
            }
        }
        let _ = reader_shutdown_tx.send(true);
    });

    tokio::select! {
        _ = cancel_rx.changed() => {}
        _ = workers.join_next() => {}
    }
    workers.abort_all();
    while workers.join_next().await.is_some() {}
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
        {
            let mut guard = self
                .cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(tx) = guard.take() {
                let _ = tx.send(true);
            }
        }
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
