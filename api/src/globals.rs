#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::LazyLock;

#[cfg(feature = "server")]
use shared::slskd::FileEntry;
#[cfg(feature = "server")]
use soulbeet::slskd::{DownloadConfig, SoulseekClient, SoulseekClientBuilder};
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};
#[cfg(feature = "server")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "server")]
use tracing::info;

/// Channel info including the sender and cancellation token for cleanup
#[cfg(feature = "server")]
pub struct UserChannel {
    pub sender: broadcast::Sender<Vec<FileEntry>>,
    pub cancellation_token: CancellationToken,
    pub active_tasks: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "server")]
impl UserChannel {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(100);
        Self {
            sender,
            cancellation_token: CancellationToken::new(),
            active_tasks: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Increment the active task count
    pub fn add_task(&self) {
        self.active_tasks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decrement the active task count and return true if no tasks remain
    pub fn remove_task(&self) -> bool {
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
pub static SLSKD_CLIENT: LazyLock<SoulseekClient> = LazyLock::new(|| {
    let api_key = std::env::var("SLSKD_API_KEY").expect("Missing SLSKD_API_KEY env var");
    let base_url = std::env::var("SLSKD_URL").expect("Missing SLSKD_URL env var");

    SoulseekClientBuilder::new()
        .api_key(&api_key)
        .base_url(&base_url)
        .download_config(DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 3000,
            max_retries: 3,
            retry_base_delay_ms: 1000,
        })
        .build()
        .expect("Failed to create Soulseek client")
});

#[cfg(feature = "server")]
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<String, UserChannel>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Get or create a user channel, returning the sender and cancellation token
#[cfg(feature = "server")]
pub async fn get_or_create_user_channel(
    username: &str,
) -> (broadcast::Sender<Vec<FileEntry>>, CancellationToken) {
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
            channel.sender.receiver_count() == 0 && channel.task_count() == 0
        })
        .map(|(username, _)| username.clone())
        .collect();

    for username in stale_users {
        info!("Cleaning up stale channel for user: {}", username);
        map.remove(&username);
    }
}
