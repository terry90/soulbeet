#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{LazyLock, Once};
#[cfg(feature = "server")]
use std::time::Duration;

#[cfg(feature = "server")]
use shared::download::DownloadProgress;
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};
#[cfg(feature = "server")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "server")]
use tracing::{info, warn};

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
        self.last_activity.store(
            Self::current_timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Check if the channel has been idle for longer than the threshold
    pub fn is_stale(&self) -> bool {
        let last = self
            .last_activity
            .load(std::sync::atomic::Ordering::Relaxed);
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
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<String, UserChannel>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Get or create a user channel, returning the sender and cancellation token
#[cfg(feature = "server")]
pub async fn get_or_create_user_channel(
    username: &str,
) -> (broadcast::Sender<Vec<DownloadProgress>>, CancellationToken) {
    let mut map = USER_CHANNELS.write().await;
    let channel = map
        .entry(username.to_string())
        .or_insert_with(UserChannel::new);
    (channel.sender.clone(), channel.cancellation_token.clone())
}

/// Register a new task for a user and return the cancellation token
#[cfg(feature = "server")]
pub async fn register_user_task(username: &str) -> CancellationToken {
    let mut map = USER_CHANNELS.write().await;
    let channel = map
        .entry(username.to_string())
        .or_insert_with(UserChannel::new);
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

        // Start the automation task (sync ratings, discovery)
        tokio::spawn(async {
            // Wait 30s for server to be fully ready
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(6 * 3600)); // 6 hours
            loop {
                interval.tick().await;
                run_automation().await;
            }
        });
        info!("Started automation task (interval: 6h)");
    });
}

#[cfg(feature = "server")]
async fn run_automation() {
    use crate::models::user::User;

    // Run automation for each connected user
    let connected_users = match User::get_connected_users().await {
        Ok(users) => users,
        Err(e) => {
            info!("Automation: failed to get connected users: {}", e);
            return;
        }
    };

    if connected_users.is_empty() {
        info!("Automation: no connected Navidrome users, skipping");
        return;
    }

    // Reset any tracks stuck in Promoting from a previous crash
    if let Ok(reset_count) = crate::models::discovery_playlist::DiscoveryTrackRow::reset_stale_promoting().await {
        if reset_count > 0 {
            info!("Automation: reset {} stale Promoting tracks", reset_count);
        }
    }

    // 0. Check ReportRealPath configuration for each user
    for user in &connected_users {
        match crate::server_fns::navidrome::check_report_real_path(&user.id).await {
            Ok(Some(true)) => {
                // ReportRealPath is enabled. If status was MissingReportRealPath, restore to Connected.
                if user.navidrome_status == shared::system::NavidromeStatus::MissingReportRealPath.as_str() {
                    let _ = crate::models::user::User::update_navidrome_token(
                        &user.id,
                        user.navidrome_token.as_deref(),
                        shared::system::NavidromeStatus::Connected.as_str(),
                    ).await;
                    let _ = crate::models::user_settings::UserSettings::reset_navidrome_banner(&user.id).await;
                    info!("Automation: ReportRealPath now enabled for user {}", user.username);
                }
            }
            Ok(Some(false)) => {
                warn!(
                    "ReportRealPath is not enabled on the Soulbeet player for user {}. \
                     Enable it in Navidrome Settings > Players > Soulbeet > ReportRealPath.",
                    user.username
                );
                if user.navidrome_status != shared::system::NavidromeStatus::MissingReportRealPath.as_str() {
                    let _ = crate::models::user::User::update_navidrome_token(
                        &user.id,
                        user.navidrome_token.as_deref(),
                        shared::system::NavidromeStatus::MissingReportRealPath.as_str(),
                    ).await;
                    let _ = crate::models::user_settings::UserSettings::reset_navidrome_banner(&user.id).await;
                }
            }
            Ok(None) => {
                // Soulbeet player not registered yet -- skip, it gets created on first API call
            }
            Err(e) => {
                info!("Automation: ReportRealPath check failed for {}: {}", user.username, e);
            }
        }
    }

    // 1. Sync ratings for each connected user
    for user in &connected_users {
        info!("Automation: syncing ratings for user {}...", user.username);
        match crate::server_fns::navidrome::sync_ratings_internal(&user.id).await {
            Ok(result) => info!(
                "Automation: sync complete for {} - {} scanned, {} deletions, {} promoted, {} removed",
                user.username, result.total_songs_scanned, result.deleted_tracks,
                result.promoted_tracks, result.removed_tracks
            ),
            Err(e) => info!("Automation: sync failed for {}: {}", user.username, e),
        }
    }

    // 2. Create/update discovery playlists (after scan has indexed new files)
    for user in &connected_users {
        match crate::server_fns::discovery::reconcile_discovery_playlists(&user.id).await {
            Ok(()) => {}
            Err(e) => info!(
                "Automation: playlist reconciliation failed for {}: {}",
                user.username, e
            ),
        }
    }

    // 3. Regenerate recommendations for each user
    for user in &connected_users {
        match crate::server_fns::discovery::generate_recommendations_internal(&user.id).await {
            Ok(count) => info!(
                "Automation: generated {} candidates for user {}",
                count, user.username
            ),
            Err(e) => info!(
                "Automation: recommendation generation failed for {}: {}",
                user.username, e
            ),
        }
    }

    // 4. Expire old discovery tracks per-profile and refill gaps
    for user in &connected_users {
        let user_id = &user.id;
        let settings = match crate::models::user_settings::UserSettings::get(user_id).await {
            Ok(s) if s.discovery_enabled => s,
            _ => continue,
        };
        let Some(ref folder_id) = settings.discovery_folder_id else { continue };
        let lifetime_map = settings.parse_lifetime_days();
        let now = chrono::Utc::now();

        // Check each profile's pending tracks and expire those past their lifetime
        let pending = match crate::models::discovery_playlist::DiscoveryTrackRow::get_pending_by_folder(folder_id).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let mut expired_count = 0u32;
        for track in &pending {
            let lifetime_days = lifetime_map.get(&track.profile).copied().unwrap_or(7) as i64;
            let created = match chrono::DateTime::parse_from_rfc3339(&track.created_at)
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(&track.created_at, "%Y-%m-%d %H:%M:%S")
                        .map(|naive| naive.and_utc().fixed_offset())
                })
            {
                Ok(dt) => dt,
                Err(_) => continue,
            };
            if now.signed_duration_since(created).num_days() < lifetime_days {
                continue;
            }
            // This track has expired
            let path = std::path::Path::new(&track.path);
            if path.exists() {
                let _ = tokio::fs::remove_file(path).await;
                if let Some(parent) = path.parent() {
                    let _ = crate::server_fns::cleanup_empty_ancestors(parent).await;
                }
            }
            let _ = crate::models::discovery_playlist::DiscoveryTrackRow::update_status(
                &track.id, &shared::navidrome::DiscoveryStatus::Removed,
            ).await;
            let _ = crate::models::discovery_history::DiscoveryHistoryRow::update_outcome(
                user_id, &track.artist, &track.title, "expired",
            ).await;
            expired_count += 1;
        }
        if expired_count > 0 {
            info!("Automation: expired {} tracks for user {}", expired_count, user.username);
        }

        // Refill any gaps (generate_discovery_playlist_internal accounts for existing tracks)
        match crate::server_fns::discovery::generate_discovery_playlist_internal(user_id).await {
            Ok(result) if result.total_imported > 0 => {
                info!("Automation: refilled {} tracks for user {}", result.total_imported, user.username);
            }
            Ok(_) => {}
            Err(e) => info!("Automation: refill failed for {}: {}", user.username, e),
        }
    }
}
