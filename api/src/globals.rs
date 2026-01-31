#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{LazyLock, Once};
#[cfg(feature = "server")]
use std::time::Duration;

#[cfg(feature = "server")]
use shared::download::DownloadProgress;
#[cfg(feature = "server")]
use soulbeet::{
    beets::BeetsImporter,
    musicbrainz::MusicBrainzProvider,
    slskd::{DownloadConfig, SoulseekClientBuilder},
    Services, ServicesBuilder,
};
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};
#[cfg(feature = "server")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "server")]
use tracing::info;

/// Interval for cleaning up stale user channels (5 minutes).
#[cfg(feature = "server")]
const CHANNEL_CLEANUP_INTERVAL_SECS: u64 = 300;

/// Minimum idle time before a channel is considered stale (10 minutes).
#[cfg(feature = "server")]
const CHANNEL_STALE_THRESHOLD_SECS: u64 = 600;

/// Channel info including the sender and cancellation token for cleanup
#[cfg(feature = "server")]
pub struct UserChannel {
    pub sender: broadcast::Sender<Vec<DownloadProgress>>,
    pub cancellation_token: CancellationToken,
    pub active_tasks: std::sync::atomic::AtomicUsize,
    /// Timestamp of last activity (task registration or message send)
    last_activity: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "server")]
impl UserChannel {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(100);
        Self {
            sender,
            cancellation_token: CancellationToken::new(),
            active_tasks: std::sync::atomic::AtomicUsize::new(0),
            last_activity: std::sync::atomic::AtomicU64::new(Self::current_timestamp()),
        }
    }

    fn current_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Update the last activity timestamp
    pub fn touch(&self) {
        self.last_activity
            .store(Self::current_timestamp(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Check if the channel has been idle for longer than the threshold
    pub fn is_stale(&self) -> bool {
        let last = self.last_activity.load(std::sync::atomic::Ordering::Relaxed);
        let now = Self::current_timestamp();
        now.saturating_sub(last) > CHANNEL_STALE_THRESHOLD_SECS
    }

    /// Increment the active task count
    pub fn add_task(&self) {
        self.touch();
        self.active_tasks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decrement the active task count and return true if no tasks remain
    pub fn remove_task(&self) -> bool {
        self.touch();
        let prev = self
            .active_tasks
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        prev <= 1
    }

    /// Get the current number of active tasks
    pub fn task_count(&self) -> usize {
        self.active_tasks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cancel all active tasks
    pub fn cancel_all(&self) {
        self.cancellation_token.cancel();
    }
}

#[cfg(feature = "server")]
impl Default for UserChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "server")]
use crate::config::CONFIG;

#[cfg(feature = "server")]
pub static SERVICES: LazyLock<Services> = LazyLock::new(|| {
    let slskd_client = SoulseekClientBuilder::new()
        .api_key(CONFIG.slskd_api_key())
        .base_url(CONFIG.slskd_url())
        .download_config(DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 3000,
            max_retries: 3,
            retry_base_delay_ms: 1000,
        })
        .build()
        .expect("Failed to create Soulseek client");

    ServicesBuilder::new()
        .add_metadata(MusicBrainzProvider::new())
        .add_download(slskd_client)
        .add_importer(BeetsImporter::from_env())
        .build()
        .expect("Failed to build services")
});


#[cfg(feature = "server")]
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<String, UserChannel>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Get or create a user channel, returning the sender and cancellation token
#[cfg(feature = "server")]
pub async fn get_or_create_user_channel(
    username: &str,
) -> (broadcast::Sender<Vec<DownloadProgress>>, CancellationToken) {
    let mut map = USER_CHANNELS.write().await;
    let channel = map.entry(username.to_string()).or_insert_with(UserChannel::new);
    (channel.sender.clone(), channel.cancellation_token.clone())
}

/// Register a new task for a user and return the cancellation token
#[cfg(feature = "server")]
pub async fn register_user_task(username: &str) -> CancellationToken {
    let mut map = USER_CHANNELS.write().await;
    let channel = map.entry(username.to_string()).or_insert_with(UserChannel::new);
    channel.add_task();
    channel.cancellation_token.clone()
}

/// Unregister a task for a user and clean up if no tasks remain
#[cfg(feature = "server")]
pub async fn unregister_user_task(username: &str) {
    let should_cleanup = {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(username) {
            channel.remove_task()
        } else {
            false
        }
    };

    // If no more tasks and no receivers, we can clean up
    // Note: We keep the channel around for a bit in case new connections come in
    if should_cleanup {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(username) {
            if channel.sender.receiver_count() == 0 && channel.task_count() == 0 {
                info!(
                    "User {} has no active tasks or receivers, eligible for cleanup",
                    username
                );
                // We don't immediately remove to avoid race conditions
                // A background cleanup task could periodically clean stale channels
            }
        }
    }
}

/// Clean up stale user channels that have no active tasks or receivers
#[cfg(feature = "server")]
pub async fn cleanup_stale_channels() {
    let mut map = USER_CHANNELS.write().await;
    let stale_users: Vec<String> = map
        .iter()
        .filter(|(_, channel)| {
            let no_activity = channel.sender.receiver_count() == 0 && channel.task_count() == 0;
            no_activity && channel.is_stale()
        })
        .map(|(username, _)| username.clone())
        .collect();

    for username in stale_users {
        info!("Cleaning up stale channel for user: {}", username);
        map.remove(&username);
    }
}

/// Ensures the background cleanup task is started exactly once.
#[cfg(feature = "server")]
static CLEANUP_TASK_INIT: Once = Once::new();

/// Start the background task for cleaning up stale user channels.
#[cfg(feature = "server")]
pub fn start_channel_cleanup_task() {
    CLEANUP_TASK_INIT.call_once(|| {
        tokio::spawn(async {
            let mut interval =
                tokio::time::interval(Duration::from_secs(CHANNEL_CLEANUP_INTERVAL_SECS));
            loop {
                interval.tick().await;
                cleanup_stale_channels().await;
            }
        });
        info!(
            "Started user channel cleanup task (interval: {}s, stale threshold: {}s)",
            CHANNEL_CLEANUP_INTERVAL_SECS, CHANNEL_STALE_THRESHOLD_SECS
        );
    });
}
