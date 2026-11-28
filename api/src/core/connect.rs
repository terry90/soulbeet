//! Per-user multi-device Connect presence, playback state sync, and remote command handoff.
//!
//! Each signed-in user gets an isolated `UserConnectRoom` keyed by `UserId`. Devices register
//! over `WS /api/ws/connect`, broadcast their current `PlaybackSnapshot`, send targeted
//! `ConnectCommand`s to sibling devices, and claim active playback (which pauses the previous
//! active device and transfers its snapshot to the claiming device).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use shared::client::{
    ConnectCommand, ConnectDevice, ConnectServerEvent, DevicePlatform, PlaybackSnapshot,
};
use tokio::sync::{broadcast, RwLock};

use crate::auth::UserId;

const MAX_DEVICE_ID_CHARS: usize = 128;
const MAX_DEVICE_NAME_CHARS: usize = 128;
const MAX_QUEUE_SONG_IDS: usize = 500;

fn cap_chars(s: String, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        trimmed.chars().take(max_chars).collect()
    }
}

fn sanitize_snapshot(mut snapshot: PlaybackSnapshot) -> PlaybackSnapshot {
    if snapshot.queue_song_ids.len() > MAX_QUEUE_SONG_IDS {
        snapshot.queue_song_ids.truncate(MAX_QUEUE_SONG_IDS);
    }
    if snapshot.queue_song_ids.is_empty() {
        snapshot.queue_index = 0;
    } else if snapshot.queue_index >= snapshot.queue_song_ids.len() {
        snapshot.queue_index = snapshot.queue_song_ids.len() - 1;
    }
    snapshot
}

/// Internal envelope routed over a user's broadcast channel.
/// When `target_device_id` is `Some(id)`, only that device's WebSocket loop forwards `event`
/// (and when `target_conn_id` is `Some(cid)`, only that exact connection).
/// `Superseded` is an internal takeover signal causing older sockets for `device_id` to exit
/// immediately.
#[derive(Debug, Clone)]
pub(crate) enum ConnectEnvelope {
    Event {
        target_device_id: Option<String>,
        target_conn_id: Option<u64>,
        event: Box<ConnectServerEvent>,
    },
    Superseded {
        device_id: String,
        before_conn_id: u64,
    },
}

/// Result of checking or restoring a live WebSocket connection's registration in the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnStatus {
    /// This `conn_id` is still the active registration for `device_id`.
    Active,
    /// This `conn_id` had been evicted by `prune_stale` and was automatically re-registered.
    ReRegistered,
    /// A newer `conn_id` superseded `device_id`; this stale socket must not overwrite it.
    Superseded,
}

/// Result of a periodic server keepalive check for a registered WebSocket handler.
#[derive(Debug, Clone)]
pub(crate) enum KeepaliveCheck {
    /// Connection is active in the room; send this `DeviceList` frame to keep proxies alive.
    Active(ConnectServerEvent),
    /// Connection was pruned by `prune_stale` (e.g., throttled background tab) but not superseded;
    /// send this `DeviceList` frame to keep proxies alive until the client wakes and re-registers.
    Pruned(ConnectServerEvent),
    /// A newer connection superseded this `device_id`; close the stale socket cleanly.
    Superseded,
}

struct DeviceEntry {
    device: ConnectDevice,
    conn_id: u64,
    /// True after another device claimed active playback and sent `Pause` to this device.
    /// In-flight `UpdateState { is_playing: true }` frames will not auto-promote this device
    /// back to active until it reports `is_playing: false` or explicitly calls `ClaimActive`.
    awaiting_pause_ack: bool,
}

/// Maximum concurrent registered devices kept per user in `ConnectHub`.
const MAX_DEVICES_PER_USER: usize = 64;

struct UserConnectRoom {
    devices: HashMap<String, DeviceEntry>,
    /// Highest `conn_id` ever registered for each `device_id` in this room, with the unix
    /// timestamp when it was recorded so `prune_stale` can bound memory over time while still
    /// preventing superseded older sockets from resurrecting immediately after disconnect.
    latest_conn_id: HashMap<String, (u64, i64)>,
    active_device_id: Option<String>,
    sender: broadcast::Sender<ConnectEnvelope>,
}

impl UserConnectRoom {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(128);
        Self {
            devices: HashMap::new(),
            latest_conn_id: HashMap::new(),
            active_device_id: None,
            sender,
        }
    }

    fn device_list_event(&self) -> ConnectServerEvent {
        let mut devices: Vec<ConnectDevice> =
            self.devices.values().map(|e| e.device.clone()).collect();
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        ConnectServerEvent::DeviceList {
            devices,
            active_device_id: self.active_device_id.clone(),
        }
    }

    fn refresh_active_flags(&mut self) {
        let active = self.active_device_id.as_deref();
        for (id, entry) in &mut self.devices {
            entry.device.is_active = active == Some(id.as_str());
        }
    }

    fn broadcast_all(&self, event: ConnectServerEvent) {
        let _ = self.sender.send(ConnectEnvelope::Event {
            target_device_id: None,
            target_conn_id: None,
            event: Box::new(event),
        });
    }

    fn send_to(&self, device_id: &str, event: ConnectServerEvent) {
        let target_conn_id = self.devices.get(device_id).map(|e| e.conn_id);
        let _ = self.sender.send(ConnectEnvelope::Event {
            target_device_id: Some(device_id.to_string()),
            target_conn_id,
            event: Box::new(event),
        });
    }

    fn notify_superseded(&self, device_id: &str, before_conn_id: u64) {
        let _ = self.sender.send(ConnectEnvelope::Superseded {
            device_id: device_id.to_string(),
            before_conn_id,
        });
    }

    fn is_superseded(&self, device_id: &str, conn_id: u64) -> bool {
        self.latest_conn_id
            .get(device_id)
            .is_some_and(|&(latest, _)| conn_id < latest)
    }

    /// Ensures the room does not exceed `MAX_DEVICES_PER_USER` when adding a new `device_id` by
    /// evicting the oldest idle non-active device first (or the oldest device if all are active).
    fn enforce_device_cap(&mut self, incoming_device_id: &str) {
        if self.devices.contains_key(incoming_device_id) {
            return;
        }
        while self.devices.len() >= MAX_DEVICES_PER_USER {
            let victim = self
                .devices
                .iter()
                .filter(|(id, _)| self.active_device_id.as_deref() != Some(id.as_str()))
                .min_by_key(|(_, e)| e.device.last_seen_unix)
                .or_else(|| self.devices.iter().min_by_key(|(_, e)| e.device.last_seen_unix))
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    self.devices.remove(&id);
                    if self.active_device_id.as_deref() == Some(id.as_str()) {
                        self.active_device_id = None;
                    }
                }
                None => break,
            }
        }
    }

    /// Ensures `(device_id, conn_id)` is present in `self.devices` (touching `last_seen_unix`).
    /// If the device was evicted by `prune_stale` while its WebSocket stayed open, re-inserts it
    /// with the same `conn_id` and broadcasts the updated `DeviceList`. If a newer `conn_id`
    /// superseded `device_id` (even if that newer socket later disconnected), returns
    /// `ConnStatus::Superseded` without modifying the room.
    fn ensure_conn(
        &mut self,
        device_id: &str,
        conn_id: u64,
        name: &str,
        platform: DevicePlatform,
        now: i64,
    ) -> ConnStatus {
        if self.is_superseded(device_id, conn_id) {
            return ConnStatus::Superseded;
        }
        match self.devices.get_mut(device_id) {
            Some(entry) if entry.conn_id == conn_id => {
                entry.device.last_seen_unix = now;
                ConnStatus::Active
            }
            Some(_) => ConnStatus::Superseded,
            None => {
                let capped_name = {
                    let c = cap_chars(name.to_string(), MAX_DEVICE_NAME_CHARS);
                    if c.is_empty() {
                        "Soulbeet Client".to_string()
                    } else {
                        c
                    }
                };
                if self.active_device_id.is_none() {
                    self.active_device_id = Some(device_id.to_string());
                }
                self.enforce_device_cap(device_id);
                self.devices.insert(
                    device_id.to_string(),
                    DeviceEntry {
                        device: ConnectDevice {
                            device_id: device_id.to_string(),
                            name: capped_name,
                            platform,
                            is_active: self.active_device_id.as_deref() == Some(device_id),
                            last_seen_unix: now,
                            snapshot: None,
                        },
                        conn_id,
                        awaiting_pause_ack: false,
                    },
                );
                self.refresh_active_flags();
                self.broadcast_all(self.device_list_event());
                ConnStatus::ReRegistered
            }
        }
    }
}

/// Global Connect hub shared by all WebSocket handlers.
pub static CONNECT_HUB: LazyLock<ConnectHub> = LazyLock::new(ConnectHub::new);

pub struct ConnectHub {
    rooms: RwLock<HashMap<UserId, UserConnectRoom>>,
    next_conn_id: AtomicU64,
}

impl Default for ConnectHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectHub {
    pub fn new() -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
        }
    }

    fn now_unix() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Registers `device_id` in `user`'s room, broadcasts the updated `DeviceList` to existing
    /// subscribers, and then subscribes the new connection (under the same write lock so the
    /// connecting client does not receive a duplicate initial `DeviceList`).
    /// Returns `(conn_id, sanitized_device_id, initial_device_list, receiver)`.
    pub(crate) async fn register(
        &self,
        user: &UserId,
        device_id: String,
        name: String,
        platform: DevicePlatform,
    ) -> (
        u64,
        String,
        ConnectServerEvent,
        broadcast::Receiver<ConnectEnvelope>,
    ) {
        self.reregister(user, None, device_id, name, platform).await
    }

    /// Atomically registers or renames a connection under a single write lock.
    /// If `previous` is `Some((old_device_id, old_conn_id))` and the device id changed, removes
    /// `old_device_id` only if its `conn_id` still matches `old_conn_id` (carrying over snapshot
    /// and active status cleanly).
    pub(crate) async fn reregister(
        &self,
        user: &UserId,
        previous: Option<(&str, u64)>,
        device_id: String,
        name: String,
        platform: DevicePlatform,
    ) -> (
        u64,
        String,
        ConnectServerEvent,
        broadcast::Receiver<ConnectEnvelope>,
    ) {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let device_id = cap_chars(device_id, MAX_DEVICE_ID_CHARS);
        let name = {
            let capped = cap_chars(name, MAX_DEVICE_NAME_CHARS);
            if capped.is_empty() {
                "Soulbeet Client".to_string()
            } else {
                capped
            }
        };

        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);

        let mut carried_snapshot = None;
        if let Some((old_id, old_conn_id)) = previous {
            if old_id != device_id {
                let owned = room
                    .devices
                    .get(old_id)
                    .is_some_and(|e| e.conn_id == old_conn_id);
                if owned {
                    if let Some(old_entry) = room.devices.remove(old_id) {
                        carried_snapshot = old_entry.device.snapshot;
                    }
                    if room.active_device_id.as_deref() == Some(old_id) {
                        room.active_device_id = Some(device_id.clone());
                    }
                }
            }
        }

        let existing_snapshot = room
            .devices
            .get(&device_id)
            .and_then(|e| e.device.snapshot.clone())
            .or(carried_snapshot);

        if room.active_device_id.is_none() {
            room.active_device_id = Some(device_id.clone());
        }

        let now = Self::now_unix();
        room.enforce_device_cap(&device_id);
        room.latest_conn_id.insert(device_id.clone(), (conn_id, now));
        room.devices.insert(
            device_id.clone(),
            DeviceEntry {
                device: ConnectDevice {
                    device_id: device_id.clone(),
                    name,
                    platform,
                    is_active: room.active_device_id.as_deref() == Some(device_id.as_str()),
                    last_seen_unix: now,
                    snapshot: existing_snapshot,
                },
                conn_id,
                awaiting_pause_ack: false,
            },
        );
        room.refresh_active_flags();

        // Tell any older socket for this `device_id` to exit immediately before subscribing the new one.
        room.notify_superseded(&device_id, conn_id);
        let list = room.device_list_event();
        room.broadcast_all(list.clone());
        // Subscribe AFTER broadcasting so this connection gets `list` once via direct send.
        let rx = room.sender.subscribe();
        (conn_id, device_id, list, rx)
    }

    /// Removes `device_id` from `user`'s room only if its active connection id matches `conn_id`,
    /// preventing a closing stale socket from evicting a newer live connection with the same id.
    pub(crate) async fn unregister(&self, user: &UserId, device_id: &str, conn_id: u64) {
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return;
        };
        let owned = room
            .devices
            .get(device_id)
            .is_some_and(|entry| entry.conn_id == conn_id);
        if owned {
            room.devices.remove(device_id);
            if room.active_device_id.as_deref() == Some(device_id) {
                room.active_device_id = room
                    .devices
                    .values()
                    .max_by_key(|e| e.device.last_seen_unix)
                    .map(|e| e.device.device_id.clone());
                room.refresh_active_flags();
            }
            if !room.devices.is_empty() || room.sender.receiver_count() > 0 {
                room.broadcast_all(room.device_list_event());
            }
        }
        if room.devices.is_empty() && room.sender.receiver_count() == 0 {
            rooms.remove(user);
        }
    }

    /// Connection-aware heartbeat: refreshes `last_seen_unix` if `conn_id` is still active,
    /// automatically re-registers the device if it was evicted by `prune_stale`, and refuses
    /// to touch the entry if a newer `conn_id` superseded it.
    pub(crate) async fn heartbeat_conn(
        &self,
        user: &UserId,
        device_id: &str,
        conn_id: u64,
        name: &str,
        platform: DevicePlatform,
    ) -> ConnStatus {
        let now = Self::now_unix();
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);
        room.ensure_conn(device_id, conn_id, name, platform, now)
    }

    /// Returns a `DeviceList` keepalive snapshot for `(device_id, conn_id)`, or `Superseded`
    /// when a newer connection has taken over `device_id`.
    pub(crate) async fn keepalive_snapshot(
        &self,
        user: &UserId,
        device_id: &str,
        conn_id: u64,
    ) -> KeepaliveCheck {
        let rooms = self.rooms.read().await;
        let Some(room) = rooms.get(user) else {
            return KeepaliveCheck::Pruned(ConnectServerEvent::DeviceList {
                devices: vec![],
                active_device_id: None,
            });
        };
        let list = room.device_list_event();
        if room.is_superseded(device_id, conn_id) {
            return KeepaliveCheck::Superseded;
        }
        match room.devices.get(device_id) {
            Some(entry) if entry.conn_id == conn_id => KeepaliveCheck::Active(list),
            Some(_) => KeepaliveCheck::Superseded,
            None => KeepaliveCheck::Pruned(list),
        }
    }

    /// Updates a device's playback snapshot and broadcasts `PlaybackUpdated`.
    /// When `snapshot.is_playing` is true (and the device is not still awaiting pause ack from a
    /// `ClaimActive`), promotes the device to active and pauses the previously active device.
    /// Returns `true` if the device was registered.
    #[cfg(test)]
    pub(crate) async fn update_state(
        &self,
        user: &UserId,
        device_id: &str,
        snapshot: PlaybackSnapshot,
    ) -> bool {
        let snapshot = sanitize_snapshot(snapshot);
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return false;
        };
        Self::apply_state_update(room, device_id, snapshot, Self::now_unix())
    }

    /// Connection-aware state update: recovers from stale pruning before applying `snapshot`,
    /// and ignores frames from a superseded `conn_id`.
    pub(crate) async fn update_state_conn(
        &self,
        user: &UserId,
        device_id: &str,
        conn_id: u64,
        name: &str,
        platform: DevicePlatform,
        snapshot: PlaybackSnapshot,
    ) -> ConnStatus {
        let snapshot = sanitize_snapshot(snapshot);
        let now = Self::now_unix();
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);
        let status = room.ensure_conn(device_id, conn_id, name, platform, now);
        if status != ConnStatus::Superseded {
            Self::apply_state_update(room, device_id, snapshot, now);
        }
        status
    }

    fn apply_state_update(
        room: &mut UserConnectRoom,
        device_id: &str,
        snapshot: PlaybackSnapshot,
        now: i64,
    ) -> bool {
        let can_promote = match room.devices.get_mut(device_id) {
            Some(entry) => {
                entry.device.last_seen_unix = now;
                if !snapshot.is_playing {
                    entry.awaiting_pause_ack = false;
                }
                entry.device.snapshot = Some(snapshot.clone());
                snapshot.is_playing && !entry.awaiting_pause_ack
            }
            None => return false,
        };

        let previous_active = if can_promote && room.active_device_id.as_deref() != Some(device_id)
        {
            let prev = room.active_device_id.clone();
            room.active_device_id = Some(device_id.to_string());
            prev
        } else {
            None
        };

        if let Some(prev_id) = previous_active {
            if let Some(prev_entry) = room.devices.get_mut(&prev_id) {
                if let Some(ref mut prev_snap) = prev_entry.device.snapshot {
                    prev_snap.is_playing = false;
                }
                prev_entry.awaiting_pause_ack = true;
            }
            room.send_to(
                &prev_id,
                ConnectServerEvent::Command {
                    from_device_id: device_id.to_string(),
                    command: ConnectCommand::Pause,
                },
            );
            room.refresh_active_flags();
            room.broadcast_all(room.device_list_event());
        } else if can_promote && room.active_device_id.as_deref() == Some(device_id) {
            // Device was already active (no previous active to pause).
        }

        room.broadcast_all(ConnectServerEvent::PlaybackUpdated {
            device_id: device_id.to_string(),
            snapshot,
        });
        true
    }

    /// Transfers active playback to `claiming_device_id`.
    /// If another device was active, marks its snapshot paused (and guards against in-flight
    /// `UpdateState` frames reclaiming active), sends `Pause` to it and, when it had a snapshot,
    /// sends `TransferPlayback` to `claiming_device_id`.
    #[cfg(test)]
    pub(crate) async fn claim_active(&self, user: &UserId, claiming_device_id: &str) -> bool {
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return false;
        };
        Self::apply_claim_active(room, claiming_device_id, Self::now_unix())
    }

    /// Connection-aware active claim: recovers from stale pruning before claiming active,
    /// and ignores claims from a superseded `conn_id`.
    pub(crate) async fn claim_active_conn(
        &self,
        user: &UserId,
        claiming_device_id: &str,
        conn_id: u64,
        name: &str,
        platform: DevicePlatform,
    ) -> ConnStatus {
        let now = Self::now_unix();
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);
        let status = room.ensure_conn(claiming_device_id, conn_id, name, platform, now);
        if status != ConnStatus::Superseded {
            Self::apply_claim_active(room, claiming_device_id, now);
        }
        status
    }

    fn apply_claim_active(
        room: &mut UserConnectRoom,
        claiming_device_id: &str,
        now: i64,
    ) -> bool {
        if !room.devices.contains_key(claiming_device_id) {
            return false;
        }

        let previous_active = room
            .active_device_id
            .clone()
            .filter(|prev| prev != claiming_device_id);

        if let Some(ref prev_id) = previous_active {
            let prev_snapshot = room.devices.get_mut(prev_id).and_then(|entry| {
                let transferred = entry.device.snapshot.clone();
                if let Some(ref mut snap) = entry.device.snapshot {
                    snap.is_playing = false;
                }
                entry.awaiting_pause_ack = true;
                transferred
            });
            room.send_to(
                prev_id,
                ConnectServerEvent::Command {
                    from_device_id: claiming_device_id.to_string(),
                    command: ConnectCommand::Pause,
                },
            );
            if let Some(snapshot) = prev_snapshot {
                room.send_to(
                    claiming_device_id,
                    ConnectServerEvent::Command {
                        from_device_id: prev_id.clone(),
                        command: ConnectCommand::TransferPlayback {
                            from_device_id: prev_id.clone(),
                            snapshot,
                        },
                    },
                );
            }
        }

        room.active_device_id = Some(claiming_device_id.to_string());
        if let Some(entry) = room.devices.get_mut(claiming_device_id) {
            entry.device.last_seen_unix = now;
            entry.awaiting_pause_ack = false;
        }
        room.refresh_active_flags();
        room.broadcast_all(room.device_list_event());
        true
    }

    /// Routes a `ConnectCommand` from `from_device_id` to `target_device_id` within `user`'s room.
    /// Touches `from_device_id.last_seen_unix` and returns `true` if `target_device_id` is registered.
    #[cfg(test)]
    pub(crate) async fn send_command(
        &self,
        user: &UserId,
        from_device_id: &str,
        target_device_id: &str,
        mut command: ConnectCommand,
    ) -> bool {
        Self::sanitize_command(&mut command);

        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return false;
        };
        if let Some(sender_entry) = room.devices.get_mut(from_device_id) {
            sender_entry.device.last_seen_unix = Self::now_unix();
        }
        if !room.devices.contains_key(target_device_id) {
            return false;
        }
        room.send_to(
            target_device_id,
            ConnectServerEvent::Command {
                from_device_id: from_device_id.to_string(),
                command,
            },
        );
        true
    }

    /// Connection-aware command routing: recovers `from_device_id` from stale pruning before
    /// routing `command`, and refuses superseded sockets.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_command_conn(
        &self,
        user: &UserId,
        from_device_id: &str,
        conn_id: u64,
        name: &str,
        platform: DevicePlatform,
        target_device_id: &str,
        mut command: ConnectCommand,
    ) -> (ConnStatus, bool) {
        Self::sanitize_command(&mut command);
        let now = Self::now_unix();
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);
        let status = room.ensure_conn(from_device_id, conn_id, name, platform, now);
        if status == ConnStatus::Superseded {
            return (status, false);
        }
        if !room.devices.contains_key(target_device_id) {
            return (status, false);
        }
        room.send_to(
            target_device_id,
            ConnectServerEvent::Command {
                from_device_id: from_device_id.to_string(),
                command,
            },
        );
        (status, true)
    }

    fn sanitize_command(command: &mut ConnectCommand) {
        match command {
            ConnectCommand::LoadQueue {
                song_ids,
                start_index,
                ..
            } => {
                if song_ids.len() > MAX_QUEUE_SONG_IDS {
                    song_ids.truncate(MAX_QUEUE_SONG_IDS);
                }
                if song_ids.is_empty() {
                    *start_index = 0;
                } else if *start_index >= song_ids.len() {
                    *start_index = song_ids.len() - 1;
                }
            }
            ConnectCommand::TransferPlayback { snapshot, .. } => {
                *snapshot = sanitize_snapshot(snapshot.clone());
            }
            _ => {}
        }
    }

    /// Lists currently registered devices for `user`.
    pub async fn devices_for(&self, user: &UserId) -> Vec<ConnectDevice> {
        let rooms = self.rooms.read().await;
        let Some(room) = rooms.get(user) else {
            return Vec::new();
        };
        let mut devices: Vec<ConnectDevice> =
            room.devices.values().map(|e| e.device.clone()).collect();
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        devices
    }

    /// Prunes devices whose `last_seen_unix` is older than `max_idle_secs` and removes empty rooms.
    pub async fn prune_stale(&self, max_idle_secs: i64) {
        let cutoff = Self::now_unix().saturating_sub(max_idle_secs);
        let mut rooms = self.rooms.write().await;
        let mut empty_users = Vec::new();
        for (user, room) in rooms.iter_mut() {
            let before = room.devices.len();
            room.devices.retain(|_, e| e.device.last_seen_unix >= cutoff);
            room.latest_conn_id
                .retain(|id, &mut (_, updated_at)| {
                    room.devices.contains_key(id) || updated_at >= cutoff
                });
            if room.devices.len() != before {
                if room
                    .active_device_id
                    .as_deref()
                    .is_some_and(|id| !room.devices.contains_key(id))
                {
                    room.active_device_id = room
                        .devices
                        .values()
                        .max_by_key(|e| e.device.last_seen_unix)
                        .map(|e| e.device.device_id.clone());
                    room.refresh_active_flags();
                }
                if !room.devices.is_empty() || room.sender.receiver_count() > 0 {
                    room.broadcast_all(room.device_list_event());
                }
            }
            if room.devices.is_empty() && room.sender.receiver_count() == 0 {
                empty_users.push(user.clone());
            }
        }
        for user in empty_users {
            rooms.remove(&user);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot(song_id: &str, playing: bool) -> PlaybackSnapshot {
        PlaybackSnapshot {
            song_id: Some(song_id.to_string()),
            title: Some("La Femme d'Argent".to_string()),
            artist: Some("Air".to_string()),
            album: Some("Moon Safari".to_string()),
            cover_art_id: Some("al-1".to_string()),
            is_playing: playing,
            position_ms: 42_000,
            duration_ms: 430_000,
            queue_song_ids: vec![song_id.to_string(), "song-2".to_string()],
            queue_index: 0,
            volume: 0.85,
            is_pending_discovery: false,
            updated_at_unix: 1_732_560_000,
        }
    }

    fn recv_for(
        rx: &mut broadcast::Receiver<ConnectEnvelope>,
        device_id: &str,
    ) -> Vec<ConnectServerEvent> {
        let mut out = Vec::new();
        while let Ok(env) = rx.try_recv() {
            if let ConnectEnvelope::Event {
                target_device_id,
                event,
                ..
            } = env
            {
                if target_device_id.is_none() || target_device_id.as_deref() == Some(device_id) {
                    out.push(*event);
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn connect_hub_hands_off_playback_between_devices_and_routes_commands() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (_desk_conn, _, initial, mut desktop_rx) = hub
            .register(
                &alice,
                "desk-1".into(),
                "Linux Studio".into(),
                DevicePlatform::Desktop,
            )
            .await;
        // Connecting device must NOT receive a duplicate initial DeviceList via broadcast.
        assert!(recv_for(&mut desktop_rx, "desk-1").is_empty());

        match initial {
            ConnectServerEvent::DeviceList {
                devices,
                active_device_id,
            } => {
                assert_eq!(devices.len(), 1);
                assert_eq!(active_device_id.as_deref(), Some("desk-1"));
                assert!(devices[0].is_active);
            }
            other => panic!("unexpected initial event: {other:?}"),
        }

        let (phone_conn, _, _, mut phone_rx) = hub
            .register(
                &alice,
                "phone-1".into(),
                "Pixel 9".into(),
                DevicePlatform::Android,
            )
            .await;
        assert!(recv_for(&mut phone_rx, "phone-1").is_empty());

        // Drain desktop's notification of phone joining.
        let _ = recv_for(&mut desktop_rx, "desk-1");

        // Desktop updates playback state
        let snap = sample_snapshot("song-air-1", true);
        hub.update_state(&alice, "desk-1", snap.clone()).await;

        let phone_events = recv_for(&mut phone_rx, "phone-1");
        assert!(phone_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::PlaybackUpdated { device_id, snapshot }
                if device_id == "desk-1" && snapshot.song_id.as_deref() == Some("song-air-1")
        )));
        let _ = recv_for(&mut desktop_rx, "desk-1");

        // Phone claims active playback -> Desktop receives Pause, Phone receives TransferPlayback
        assert!(hub.claim_active(&alice, "phone-1").await);

        let desktop_events = recv_for(&mut desktop_rx, "desk-1");
        assert!(desktop_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::Command {
                from_device_id,
                command: ConnectCommand::Pause
            } if from_device_id == "phone-1"
        )));
        assert!(!desktop_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::Command {
                command: ConnectCommand::TransferPlayback { .. },
                ..
            }
        )));

        let phone_events = recv_for(&mut phone_rx, "phone-1");
        assert!(phone_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::Command {
                from_device_id,
                command: ConnectCommand::TransferPlayback { snapshot, .. }
            } if from_device_id == "desk-1" && snapshot.position_ms == 42_000
        )));

        // Targeted remote command from phone -> desktop only
        assert!(
            hub.send_command(
                &alice,
                "phone-1",
                "desk-1",
                ConnectCommand::Seek { position_ms: 90_000 }
            )
            .await
        );
        let desktop_events = recv_for(&mut desktop_rx, "desk-1");
        assert!(desktop_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::Command {
                command: ConnectCommand::Seek { position_ms: 90_000 },
                ..
            }
        )));
        assert!(recv_for(&mut phone_rx, "phone-1").is_empty());

        // Unregistering active device falls back to remaining device
        hub.unregister(&alice, "phone-1", phone_conn).await;
        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "desk-1");
        assert!(devices[0].is_active);
    }

    #[tokio::test]
    async fn users_cannot_send_commands_across_accounts() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");

        let _ = hub
            .register(&alice, "alice-desk".into(), "Desk".into(), DevicePlatform::Desktop)
            .await;
        let _ = hub
            .register(&bob, "bob-phone".into(), "Phone".into(), DevicePlatform::Android)
            .await;

        assert!(
            !hub.send_command(&bob, "bob-phone", "alice-desk", ConnectCommand::Pause)
                .await
        );
    }

    #[tokio::test]
    async fn stale_socket_unregister_cannot_evict_reconnected_device_with_same_id() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (old_conn, _, _, _old_rx) = hub
            .register(
                &alice,
                "desk-1".into(),
                "Linux Studio".into(),
                DevicePlatform::Desktop,
            )
            .await;
        let (new_conn, _, _, _new_rx) = hub
            .register(
                &alice,
                "desk-1".into(),
                "Linux Studio (reconnected)".into(),
                DevicePlatform::Desktop,
            )
            .await;
        assert_ne!(old_conn, new_conn);

        // Stale socket closes after the new socket already registered.
        hub.unregister(&alice, "desk-1", old_conn).await;

        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "desk-1");
        assert_eq!(devices[0].name, "Linux Studio (reconnected)");
        assert!(devices[0].is_active);

        // Closing the new socket cleanly removes the device.
        hub.unregister(&alice, "desk-1", new_conn).await;
        assert!(hub.devices_for(&alice).await.is_empty());
    }

    #[tokio::test]
    async fn claim_active_guards_against_in_flight_update_state_reclaiming_active() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (_, _, _, mut desktop_rx) = hub
            .register(&alice, "desk-1".into(), "Desk".into(), DevicePlatform::Desktop)
            .await;
        let (_, _, _, mut phone_rx) = hub
            .register(&alice, "phone-1".into(), "Phone".into(), DevicePlatform::Android)
            .await;
        let _ = recv_for(&mut desktop_rx, "desk-1");

        hub.update_state(&alice, "desk-1", sample_snapshot("s1", true))
            .await;
        let _ = recv_for(&mut desktop_rx, "desk-1");
        let _ = recv_for(&mut phone_rx, "phone-1");

        // Phone claims active playback.
        assert!(hub.claim_active(&alice, "phone-1").await);

        // Desktop's stored snapshot is immediately marked not playing.
        let devices = hub.devices_for(&alice).await;
        let desk = devices.iter().find(|d| d.device_id == "desk-1").unwrap();
        assert!(!desk.is_active);
        assert!(!desk.snapshot.as_ref().unwrap().is_playing);
        assert!(devices
            .iter()
            .find(|d| d.device_id == "phone-1")
            .unwrap()
            .is_active);

        // An in-flight UpdateState { is_playing: true } from desk-1 must NOT steal active back.
        hub.update_state(&alice, "desk-1", sample_snapshot("s1", true))
            .await;
        let devices = hub.devices_for(&alice).await;
        assert!(devices
            .iter()
            .find(|d| d.device_id == "phone-1")
            .unwrap()
            .is_active);
        assert!(!devices
            .iter()
            .find(|d| d.device_id == "desk-1")
            .unwrap()
            .is_active);

        // Once desk-1 acknowledges the pause (is_playing: false), a subsequent genuine play
        // promotes desk-1 and pauses phone-1.
        hub.update_state(&alice, "desk-1", sample_snapshot("s1", false))
            .await;
        let _ = recv_for(&mut phone_rx, "phone-1");

        hub.update_state(&alice, "desk-1", sample_snapshot("s1", true))
            .await;
        let devices = hub.devices_for(&alice).await;
        assert!(devices
            .iter()
            .find(|d| d.device_id == "desk-1")
            .unwrap()
            .is_active);
        let phone_events = recv_for(&mut phone_rx, "phone-1");
        assert!(phone_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::Command {
                from_device_id,
                command: ConnectCommand::Pause
            } if from_device_id == "desk-1"
        )));
    }

    #[tokio::test]
    async fn caps_device_fields_and_queue_and_prune_broadcasts_empty_list_to_receivers() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let long_id = "d".repeat(300);
        let long_name = "n".repeat(300);
        let (_, capped_id, _, mut rx) = hub
            .register(&alice, long_id, long_name, DevicePlatform::Desktop)
            .await;
        assert_eq!(capped_id.chars().count(), MAX_DEVICE_ID_CHARS);
        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices[0].name.chars().count(), MAX_DEVICE_NAME_CHARS);

        let mut big_snap = sample_snapshot("s0", true);
        big_snap.queue_song_ids = (0..800).map(|i| format!("s{i}")).collect();
        big_snap.queue_index = 799;
        hub.update_state(&alice, &capped_id, big_snap).await;
        let _ = recv_for(&mut rx, &capped_id);

        let stored = hub.devices_for(&alice).await[0].snapshot.clone().unwrap();
        assert_eq!(stored.queue_song_ids.len(), MAX_QUEUE_SONG_IDS);
        assert_eq!(stored.queue_index, MAX_QUEUE_SONG_IDS - 1);

        // Age the device past cutoff while receiver is still alive; prune_stale must broadcast empty DeviceList.
        {
            let mut rooms = hub.rooms.write().await;
            rooms
                .get_mut(&alice)
                .unwrap()
                .devices
                .get_mut(&capped_id)
                .unwrap()
                .device
                .last_seen_unix = 0;
        }
        hub.prune_stale(60).await;
        let events = recv_for(&mut rx, &capped_id);
        assert!(events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::DeviceList {
                devices,
                active_device_id: None
            } if devices.is_empty()
        )));
    }

    #[tokio::test]
    async fn stale_pruned_live_socket_recovers_on_heartbeat_and_restores_multi_device_visibility() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (desk_conn, _, _, mut desk_rx) = hub
            .register(
                &alice,
                "desk-1".into(),
                "Linux Studio".into(),
                DevicePlatform::Desktop,
            )
            .await;
        let (_phone_conn, _, _, mut phone_rx) = hub
            .register(
                &alice,
                "phone-1".into(),
                "Pixel 9".into(),
                DevicePlatform::Android,
            )
            .await;
        let _ = recv_for(&mut desk_rx, "desk-1");
        let _ = recv_for(&mut phone_rx, "phone-1");

        // Simulate desk-1 tab sleeping past stale threshold while phone-1 stays fresh.
        {
            let mut rooms = hub.rooms.write().await;
            rooms
                .get_mut(&alice)
                .unwrap()
                .devices
                .get_mut("desk-1")
                .unwrap()
                .device
                .last_seen_unix = 0;
        }
        hub.prune_stale(60).await;
        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "phone-1");
        let _ = recv_for(&mut phone_rx, "phone-1");
        let _ = recv_for(&mut desk_rx, "desk-1");

        // Keepalive check sees desk-1 was pruned (not superseded).
        assert!(matches!(
            hub.keepalive_snapshot(&alice, "desk-1", desk_conn).await,
            KeepaliveCheck::Pruned(_)
        ));

        // desk-1 wakes up and sends a heartbeat: must auto-re-register and notify phone-1.
        let status = hub
            .heartbeat_conn(
                &alice,
                "desk-1",
                desk_conn,
                "Linux Studio",
                DevicePlatform::Desktop,
            )
            .await;
        assert_eq!(status, ConnStatus::ReRegistered);

        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices.len(), 2);
        let phone_events = recv_for(&mut phone_rx, "phone-1");
        assert!(phone_events.iter().any(|e| matches!(
            e,
            ConnectServerEvent::DeviceList { devices, .. }
                if devices.len() == 2 && devices.iter().any(|d| d.device_id == "desk-1")
        )));

        // Subsequent heartbeat from desk-1 is now Active.
        assert_eq!(
            hub.heartbeat_conn(
                &alice,
                "desk-1",
                desk_conn,
                "Linux Studio",
                DevicePlatform::Desktop,
            )
            .await,
            ConnStatus::Active
        );
    }

    #[tokio::test]
    async fn superseded_socket_cannot_hijack_newer_connection() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (old_conn, _, _, _old_rx) = hub
            .register(&alice, "desk-1".into(), "Old".into(), DevicePlatform::Desktop)
            .await;
        let (new_conn, _, _, _new_rx) = hub
            .register(&alice, "desk-1".into(), "New".into(), DevicePlatform::Desktop)
            .await;

        assert!(matches!(
            hub.keepalive_snapshot(&alice, "desk-1", old_conn).await,
            KeepaliveCheck::Superseded
        ));
        assert_eq!(
            hub.heartbeat_conn(&alice, "desk-1", old_conn, "Old", DevicePlatform::Desktop)
                .await,
            ConnStatus::Superseded
        );
        assert_eq!(
            hub.update_state_conn(
                &alice,
                "desk-1",
                old_conn,
                "Old",
                DevicePlatform::Desktop,
                sample_snapshot("s-old", true),
            )
            .await,
            ConnStatus::Superseded
        );
        assert_eq!(
            hub.claim_active_conn(&alice, "desk-1", old_conn, "Old", DevicePlatform::Desktop)
                .await,
            ConnStatus::Superseded
        );

        let devices = hub.devices_for(&alice).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "New");
        assert!(devices[0].snapshot.is_none());
        assert!(matches!(
            hub.keepalive_snapshot(&alice, "desk-1", new_conn).await,
            KeepaliveCheck::Active(_)
        ));
    }

    #[tokio::test]
    async fn superseded_socket_cannot_resurrect_after_newer_socket_disconnects_and_is_isolated() {
        let hub = ConnectHub::new();
        let alice = UserId::new("alice");

        let (old_conn, _, _, mut old_rx) = hub
            .register(&alice, "desk-1".into(), "Old".into(), DevicePlatform::Desktop)
            .await;
        let (new_conn, _, _, mut new_rx) = hub
            .register(&alice, "desk-1".into(), "New".into(), DevicePlatform::Desktop)
            .await;
        let (_phone_conn, _, _, _phone_rx) = hub
            .register(
                &alice,
                "phone-1".into(),
                "Phone".into(),
                DevicePlatform::Android,
            )
            .await;

        // 1. Old socket must immediately receive the internal Superseded takeover signal.
        let mut saw_superseded = false;
        while let Ok(env) = old_rx.try_recv() {
            if matches!(
                env,
                ConnectEnvelope::Superseded { ref device_id, before_conn_id }
                    if device_id == "desk-1" && before_conn_id == new_conn
            ) {
                saw_superseded = true;
            }
        }
        assert!(saw_superseded, "old socket must receive Superseded on takeover");
        let _ = recv_for(&mut new_rx, "desk-1");

        // 2. Targeted command sent to desk-1 must carry target_conn_id == Some(new_conn).
        assert!(
            hub.send_command(
                &alice,
                "phone-1",
                "desk-1",
                ConnectCommand::Seek { position_ms: 12_345 },
            )
            .await
        );
        let mut saw_targeted_for_new = false;
        while let Ok(env) = old_rx.try_recv() {
            if let ConnectEnvelope::Event {
                target_device_id: Some(ref dev),
                target_conn_id,
                ref event,
            } = env
            {
                if matches!(
                    event.as_ref(),
                    ConnectServerEvent::Command {
                        command: ConnectCommand::Seek { .. },
                        ..
                    }
                ) {
                    assert_eq!(dev, "desk-1");
                    assert_eq!(
                        target_conn_id,
                        Some(new_conn),
                        "targeted command must be pinned to new_conn so old_conn drops it"
                    );
                    saw_targeted_for_new = true;
                }
            }
        }
        assert!(saw_targeted_for_new);

        // 3. Even after new_conn disconnects, old_conn must NEVER resurrect desk-1.
        hub.unregister(&alice, "desk-1", new_conn).await;
        assert!(
            hub.devices_for(&alice)
                .await
                .iter()
                .all(|d| d.device_id != "desk-1")
        );
        assert!(matches!(
            hub.keepalive_snapshot(&alice, "desk-1", old_conn).await,
            KeepaliveCheck::Superseded
        ));
        assert_eq!(
            hub.heartbeat_conn(&alice, "desk-1", old_conn, "Old", DevicePlatform::Desktop)
                .await,
            ConnStatus::Superseded
        );
        assert_eq!(
            hub.update_state_conn(
                &alice,
                "desk-1",
                old_conn,
                "Old",
                DevicePlatform::Desktop,
                sample_snapshot("s-ghost", true),
            )
            .await,
            ConnStatus::Superseded
        );
        assert_eq!(
            hub.claim_active_conn(&alice, "desk-1", old_conn, "Old", DevicePlatform::Desktop)
                .await,
            ConnStatus::Superseded
        );
        assert!(
            hub.devices_for(&alice)
                .await
                .iter()
                .all(|d| d.device_id != "desk-1"),
            "superseded socket must not resurrect after newer socket disconnects"
        );
    }
}
