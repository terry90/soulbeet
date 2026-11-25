//! Per-user multi-device Connect presence, playback state sync, and remote command handoff.
//!
//! Each signed-in user gets an isolated `UserConnectRoom` keyed by `UserId`. Devices register
//! over `WS /api/ws/connect`, broadcast their current `PlaybackSnapshot`, send targeted
//! `ConnectCommand`s to sibling devices, and claim active playback (which pauses the previous
//! active device and transfers its snapshot to the claiming device).

use std::collections::HashMap;
use std::sync::LazyLock;

use shared::client::{
    ConnectCommand, ConnectDevice, ConnectServerEvent, DevicePlatform, PlaybackSnapshot,
};
use tokio::sync::{broadcast, RwLock};

use crate::auth::UserId;

/// Internal envelope routed over a user's broadcast channel.
/// When `target_device_id` is `Some(id)`, only that device's WebSocket loop forwards `event`.
#[derive(Debug, Clone)]
pub(crate) struct ConnectEnvelope {
    pub target_device_id: Option<String>,
    pub event: ConnectServerEvent,
}

struct UserConnectRoom {
    devices: HashMap<String, ConnectDevice>,
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
        let mut devices: Vec<ConnectDevice> = self.devices.values().cloned().collect();
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        ConnectServerEvent::DeviceList {
            devices,
            active_device_id: self.active_device_id.clone(),
        }
    }

    fn refresh_active_flags(&mut self) {
        let active = self.active_device_id.as_deref();
        for (id, dev) in &mut self.devices {
            dev.is_active = active == Some(id.as_str());
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
        }
    }

    fn now_unix() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Registers `device_id` in `user`'s room, subscribes to its live channel, and returns the
    /// initial `DeviceList` snapshot alongside the receiver.
    pub(crate) async fn register(
        &self,
        user: &UserId,
        device_id: String,
        name: String,
        platform: DevicePlatform,
    ) -> (ConnectServerEvent, broadcast::Receiver<ConnectEnvelope>) {
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(user.clone()).or_insert_with(UserConnectRoom::new);
        let rx = room.sender.subscribe();

        let existing_snapshot = room
            .devices
            .get(&device_id)
            .and_then(|d| d.snapshot.clone());

        if room.active_device_id.is_none() {
            room.active_device_id = Some(device_id.clone());
        }

        room.devices.insert(
            device_id.clone(),
            ConnectDevice {
                device_id: device_id.clone(),
                name,
                platform,
                is_active: room.active_device_id.as_deref() == Some(device_id.as_str()),
                last_seen_unix: Self::now_unix(),
                snapshot: existing_snapshot,
            },
        );
        room.refresh_active_flags();

        let list = room.device_list_event();
        room.broadcast_all(list.clone());
        (list, rx)
    }

    /// Removes `device_id` from `user`'s room, reassigning active status if needed.
    pub(crate) async fn unregister(&self, user: &UserId, device_id: &str) {
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return;
        };
        room.devices.remove(device_id);
        if room.active_device_id.as_deref() == Some(device_id) {
            room.active_device_id = room
                .devices
                .values()
                .max_by_key(|d| d.last_seen_unix)
                .map(|d| d.device_id.clone());
            room.refresh_active_flags();
        }
        if room.devices.is_empty() && room.sender.receiver_count() == 0 {
            rooms.remove(user);
        } else {
            room.broadcast_all(room.device_list_event());
        }
    }

    /// Updates `last_seen_unix` for `device_id`.
    pub(crate) async fn heartbeat(&self, user: &UserId, device_id: &str) {
        let mut rooms = self.rooms.write().await;
        if let Some(room) = rooms.get_mut(user) {
            if let Some(dev) = room.devices.get_mut(device_id) {
                dev.last_seen_unix = Self::now_unix();
            }
        }
    }

    /// Updates a device's playback snapshot and broadcasts `PlaybackUpdated`.
    /// When `snapshot.is_playing` is true, promotes the device to active.
    pub(crate) async fn update_state(
        &self,
        user: &UserId,
        device_id: &str,
        snapshot: PlaybackSnapshot,
    ) {
        let mut rooms = self.rooms.write().await;
        let Some(room) = rooms.get_mut(user) else {
            return;
        };
        let now = Self::now_unix();
        let active_changed = if snapshot.is_playing
            && room.active_device_id.as_deref() != Some(device_id)
            && room.devices.contains_key(device_id)
        {
            room.active_device_id = Some(device_id.to_string());
            true
        } else {
            false
        };
        if let Some(dev) = room.devices.get_mut(device_id) {
            dev.last_seen_unix = now;
            dev.snapshot = Some(snapshot.clone());
        } else {
            return;
        }
        if active_changed {
            room.refresh_active_flags();
            room.broadcast_all(room.device_list_event());
        }
        room.broadcast_all(ConnectServerEvent::PlaybackUpdated {
            device_id: device_id.to_string(),
            snapshot,
        });
    }

    /// Transfers active playback to `claiming_device_id`.
    /// If another device was active, sends `Pause` to it and, when it had a snapshot,
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
            let prev_snapshot = room
                .devices
                .get(prev_id)
                .and_then(|d| d.snapshot.clone());
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
        if let Some(dev) = room.devices.get_mut(claiming_device_id) {
            dev.last_seen_unix = Self::now_unix();
        }
        room.refresh_active_flags();
        room.broadcast_all(room.device_list_event());
        true
    }

    /// Routes a `ConnectCommand` from `from_device_id` to `target_device_id` within `user`'s room.
    /// Returns `true` if `target_device_id` is currently registered.
    pub(crate) async fn send_command(
        &self,
        user: &UserId,
        from_device_id: &str,
        target_device_id: &str,
        command: ConnectCommand,
    ) -> bool {
        let rooms = self.rooms.read().await;
        let Some(room) = rooms.get(user) else {
            return false;
        };
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
        let mut devices: Vec<ConnectDevice> = room.devices.values().cloned().collect();
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
            room.devices.retain(|_, d| d.last_seen_unix >= cutoff);
            if room.devices.len() != before {
                if room
                    .active_device_id
                    .as_deref()
                    .is_some_and(|id| !room.devices.contains_key(id))
                {
                    room.active_device_id = room
                        .devices
                        .values()
                        .max_by_key(|d| d.last_seen_unix)
                        .map(|d| d.device_id.clone());
                    room.refresh_active_flags();
                }
                if !room.devices.is_empty() {
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

        let (initial, mut desktop_rx) = hub
            .register(
                &alice,
                "desk-1".into(),
                "Linux Studio".into(),
                DevicePlatform::Desktop,
            )
            .await;
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

        let (_, mut phone_rx) = hub
            .register(
                &alice,
                "phone-1".into(),
                "Pixel 9".into(),
                DevicePlatform::Android,
            )
            .await;

        // Drain registration broadcasts
        let _ = recv_for(&mut desktop_rx, "desk-1");
        let _ = recv_for(&mut phone_rx, "phone-1");

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
        hub.unregister(&alice, "phone-1").await;
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
}
