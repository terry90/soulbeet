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
/// When `target_device_id` is `Some(id)`, only that device's WebSocket loop forwards `event`.
#[derive(Debug, Clone)]
pub(crate) struct ConnectEnvelope {
    pub target_device_id: Option<String>,
    pub event: ConnectServerEvent,
}

struct DeviceEntry {
    device: ConnectDevice,
    conn_id: u64,
    /// True after another device claimed active playback and sent `Pause` to this device.
    /// In-flight `UpdateState { is_playing: true }` frames will not auto-promote this device
    /// back to active until it reports `is_playing: false` or explicitly calls `ClaimActive`.
    awaiting_pause_ack: bool,
}

struct UserConnectRoom {
    devices: HashMap<String, DeviceEntry>,
    active_device_id: Option<String>,
    sender: broadcast::Sender<ConnectEnvelope>,
}

impl UserConnectRoom {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(128);
        Self {
            devices: HashMap::new(),
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
        let _ = self.sender.send(ConnectEnvelope {
            target_device_id: None,
            event,
        });
    }

    fn send_to(&self, device_id: &str, event: ConnectServerEvent) {
        let _ = self.sender.send(ConnectEnvelope {
            target_device_id: Some(device_id.to_string()),
            event,
        });
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

        room.devices.insert(
            device_id.clone(),
            DeviceEntry {
                device: ConnectDevice {
                    device_id: device_id.clone(),
                    name,
                    platform,
                    is_active: room.active_device_id.as_deref() == Some(device_id.as_str()),
                    last_seen_unix: Self::now_unix(),
                    snapshot: existing_snapshot,
                },
                conn_id,
                awaiting_pause_ack: false,
            },
        );
        room.refresh_active_flags();

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

    /// Updates `last_seen_unix` for `device_id`.
    pub(crate) async fn heartbeat(&self, user: &UserId, device_id: &str) {
        let mut rooms = self.rooms.write().await;
        if let Some(room) = rooms.get_mut(user) {
            if let Some(entry) = room.devices.get_mut(device_id) {
                entry.device.last_seen_unix = Self::now_unix();
            }
        }
    }

    /// Updates a device's playback snapshot and broadcasts `PlaybackUpdated`.
    /// When `snapshot.is_playing` is true (and the device is not still awaiting pause ack from a
    /// `ClaimActive`), promotes the device to active and pauses the previously active device.
    pub(crate) async fn update_state(
        &self,
        user: &UserId,
        device_id: &str,
        snapshot: PlaybackSnapshot,
    ) {
        let snapshot = sanitize_snapshot(snapshot);
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return;
        };
        let now = Self::now_unix();
        let can_promote = match room.devices.get_mut(device_id) {
            Some(entry) => {
                entry.device.last_seen_unix = now;
                if !snapshot.is_playing {
                    entry.awaiting_pause_ack = false;
                }
                entry.device.snapshot = Some(snapshot.clone());
                snapshot.is_playing && !entry.awaiting_pause_ack
            }
            None => return,
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
    }

    /// Transfers active playback to `claiming_device_id`.
    /// If another device was active, marks its snapshot paused (and guards against in-flight
    /// `UpdateState` frames reclaiming active), sends `Pause` to it and, when it had a snapshot,
    /// sends `TransferPlayback` to `claiming_device_id`.
    pub(crate) async fn claim_active(&self, user: &UserId, claiming_device_id: &str) -> bool {
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return false;
        };
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
            entry.device.last_seen_unix = Self::now_unix();
            entry.awaiting_pause_ack = false;
        }
        room.refresh_active_flags();
        room.broadcast_all(room.device_list_event());
        true
    }

    /// Routes a `ConnectCommand` from `from_device_id` to `target_device_id` within `user`'s room.
    /// Touches `from_device_id.last_seen_unix` and returns `true` if `target_device_id` is registered.
    pub(crate) async fn send_command(
        &self,
        user: &UserId,
        from_device_id: &str,
        target_device_id: &str,
        mut command: ConnectCommand,
    ) -> bool {
        match &mut command {
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
            if env.target_device_id.is_none()
                || env.target_device_id.as_deref() == Some(device_id)
            {
                out.push(env.event);
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
}
