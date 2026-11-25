#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{LazyLock, Once};
#[cfg(feature = "server")]
use std::time::Duration;

#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use shared::download::DownloadEvent;
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};
#[cfg(feature = "server")]
use tracing::{info, warn};

/// Interval for cleaning up stale user channels (5 minutes).
#[cfg(feature = "server")]
const CHANNEL_CLEANUP_INTERVAL_SECS: u64 = 300;

/// Minimum idle time before a channel is considered stale (10 minutes).
#[cfg(feature = "server")]
const CHANNEL_STALE_THRESHOLD_SECS: u64 = 600;

/// A user's download event channel and the count of tasks using it, for cleanup
#[cfg(feature = "server")]
pub struct UserChannel {
    pub sender: broadcast::Sender<DownloadEvent>,
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
}

#[cfg(feature = "server")]
impl Default for UserChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "server")]
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<UserId, UserChannel>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "server")]
pub static DISCOVERY_PROGRESS: LazyLock<
    RwLock<HashMap<String, shared::navidrome::DiscoveryProgress>>,
> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Get or create a user channel, returning its sender
#[cfg(feature = "server")]
pub async fn get_or_create_user_channel(user: &UserId) -> broadcast::Sender<DownloadEvent> {
    let mut map = USER_CHANNELS.write().await;
    let channel = map.entry(user.clone()).or_insert_with(UserChannel::new);
    channel.sender.clone()
}

/// Sends to the user's channel as it is now: `cleanup_stale_channels` may
/// have replaced the one a long-running job saw at its start. Without a
/// channel nobody is listening, so none is created.
#[cfg(feature = "server")]
pub async fn send_user_event(user: &UserId, event: DownloadEvent) -> bool {
    let channels = USER_CHANNELS.read().await;
    let Some(channel) = channels.get(user) else {
        return false;
    };
    channel.touch();
    channel.sender.send(event).is_ok()
}

/// Register a new task for a user
#[cfg(feature = "server")]
pub async fn register_user_task(user: &UserId) {
    let mut map = USER_CHANNELS.write().await;
    let channel = map.entry(user.clone()).or_insert_with(UserChannel::new);
    channel.add_task();
}

/// Unregister a task for a user and clean up if no tasks remain
#[cfg(feature = "server")]
pub async fn unregister_user_task(user: &UserId) {
    let should_cleanup = {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(user) {
            channel.remove_task()
        } else {
            false
        }
    };

    // If no more tasks and no receivers, we can clean up
    // Note: We keep the channel around for a bit in case new connections come in
    if should_cleanup {
        let map = USER_CHANNELS.read().await;
        if let Some(channel) = map.get(user) {
            if channel.sender.receiver_count() == 0 && channel.task_count() == 0 {
                info!(
                    "User {} has no active tasks or receivers, eligible for cleanup",
                    user
                );
                // We don't immediately remove to avoid race conditions
                // A background cleanup task could periodically clean stale channels
            }
        }
    }
}

/// Remove stale discovery progress entries (terminal > 30 min, stuck running > 1 hour)
#[cfg(feature = "server")]
pub async fn cleanup_stale_progress() {
    let mut map = DISCOVERY_PROGRESS.write().await;
    let stale: Vec<String> = map
        .iter()
        .filter(|(_, p)| {
            if !p.is_terminal() {
                if let Some(ref started) = p.started_at {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(started) {
                        return chrono::Utc::now().signed_duration_since(ts).num_minutes() > 60;
                    }
                }
                return false;
            }
            if let Some(ref completed) = p.completed_at {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(completed) {
                    return chrono::Utc::now().signed_duration_since(ts).num_minutes() > 30;
                }
            }
            false
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        map.remove(&key);
    }
}

/// Clean up stale user channels that have no active tasks or receivers
#[cfg(feature = "server")]
pub async fn cleanup_stale_channels() {
    let mut map = USER_CHANNELS.write().await;
    let stale_users: Vec<UserId> = map
        .iter()
        .filter(|(_, channel)| {
            let no_activity = channel.sender.receiver_count() == 0 && channel.task_count() == 0;
            no_activity && channel.is_stale()
        })
        .map(|(user, _)| user.clone())
        .collect();

    for user in stale_users {
        info!("Cleaning up stale channel for user: {}", user);
        map.remove(&user);
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
                cleanup_stale_progress().await;
                crate::core::connect::CONNECT_HUB
                    .prune_stale(CHANNEL_STALE_THRESHOLD_SECS as i64)
                    .await;
            }
        });
        info!(
            "Started user channel cleanup task (interval: {}s, stale threshold: {}s)",
            CHANNEL_CLEANUP_INTERVAL_SECS, CHANNEL_STALE_THRESHOLD_SECS
        );

        // Start the automation task (sync ratings, discovery)
        tokio::spawn(async {
            // A promote or removal runs to its end once started, so a track can only be left
            // Promoting or Removing by a previous process.
            match crate::models::discovery_playlist::DiscoveryTrackRow::reset_stale_claims_in(
                &crate::db::DB,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => info!("Reset {n} tracks left Promoting or Removing by a previous run"),
                Err(e) => warn!("Could not reset tracks left Promoting or Removing: {e}"),
            }
            // Wait 30s for server to be fully ready
            tokio::time::sleep(Duration::from_secs(30)).await;
            repeat_spaced(Duration::from_secs(6 * 3600), run_automation).await;
        });
        info!("Started automation task (6h after each run ends)");

        tokio::spawn(async {
            let delay = first_recommendation_delay(&mut rand::rng());
            info!(
                "Recommendations start in {}s, then 6h after each cycle ends",
                delay.as_secs()
            );
            tokio::time::sleep(delay).await;
            repeat_spaced(RECOMMENDATION_INTERVAL, run_recommendations).await;
        });

        tokio::spawn(async {
            // Wait 45s after startup before running low-priority archival FLAC upgrades.
            tokio::time::sleep(Duration::from_secs(45)).await;
            repeat_spaced(
                crate::core::archival::ARCHIVAL_WORKER_INTERVAL,
                crate::core::archival::run_archival_worker_tick,
            )
            .await;
        });
        info!("Started low-priority archival FLAC upgrade worker");
    });
}

/// Runs `cycle` forever and waits `period` after each one ends. A tokio interval, even
/// with MissedTickBehavior::Delay, starts the next cycle at once after one that ran past
/// the period, and these cycles call remote services that may be the reason it ran long.
#[cfg(feature = "server")]
async fn repeat_spaced<F, Fut>(period: Duration, mut cycle: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        cycle().await;
        tokio::time::sleep(period).await;
    }
}

#[cfg(feature = "server")]
const RECOMMENDATION_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// 30 s for the server to be ready, then a random point in the first cycle, so installs
/// started together do not call ListenBrainz and Deezer in step.
#[cfg(feature = "server")]
fn first_recommendation_delay(rng: &mut impl rand::Rng) -> Duration {
    Duration::from_secs(30 + rng.random_range(0..RECOMMENDATION_INTERVAL.as_secs()))
}

/// One recommendation cycle: drops expired music facts, then, for each user with discovery
/// on, builds their profile once and stores their selected profiles' candidates from it.
#[cfg(feature = "server")]
async fn run_recommendations() {
    use crate::core::index::ServerNavidrome;
    use crate::core::recommend::{due_in, generate_in, DiscoveryServices};
    use crate::db::DB;

    match crate::models::knowledge_cache::prune_expired_in(&DB).await {
        Ok(n) => info!("Recommendations: pruned {n} expired cached facts"),
        Err(e) => warn!("Recommendations: could not prune expired cached facts: {e}"),
    }
    let due = match due_in(&DB).await {
        Ok(due) => due,
        Err(e) => {
            warn!("Recommendations: failed to get the users with discovery on: {e}");
            return;
        }
    };
    let services = DiscoveryServices::live();
    for (user, profiles) in &due {
        let id = UserId::new(&user.id);
        match generate_in(&DB, &ServerNavidrome, &services, &id, profiles).await {
            Ok(count) => info!(
                "Recommendations: generated {} candidates for user {}",
                count, user.username
            ),
            Err(f) => warn!(
                "Recommendations: generation failed for {}: {}",
                user.username,
                crate::core::logged(&f)
            ),
        }
    }
}

#[cfg(feature = "server")]
async fn run_automation() {
    use crate::core::ratings::AutoDeletePermit;
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

    // 1. Sync ratings for each connected user. Expiry deletes only what this cycle's sync of the
    // user found not rated to keep, so a user whose sync failed has no entry and nothing expires.
    let mut expirable_by_user = HashMap::new();
    for user in &connected_users {
        info!("Automation: syncing ratings for user {}...", user.username);
        let ctx = crate::auth::UserCtx {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
        };
        match crate::core::ratings::sync(&ctx, AutoDeletePermit::Allowed).await {
            Ok(report) => {
                let result = report.counts();
                info!(
                    "Automation: sync complete for {} - {} scanned, {} deletions, {} promoted, {} removed, {} kept",
                    user.username, result.total_songs_scanned, result.deleted_tracks,
                    result.promoted_tracks, result.removed_tracks, result.kept_tracks
                );
                expirable_by_user.insert(user.id.clone(), report.expirable);
            }
            Err(f) => info!(
                "Automation: sync failed for {}: {}",
                user.username,
                crate::core::logged(&f)
            ),
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

    // 3. Expire old discovery tracks per-profile and refill gaps
    for user in &connected_users {
        let user_id = &user.id;
        let settings = match crate::models::user_settings::UserSettings::get(user_id).await {
            Ok(s) if s.discovery_enabled => s,
            _ => continue,
        };
        let Some(ref folder_id) = settings.discovery_folder_id else {
            continue;
        };
        let lifetime_map = settings.parse_lifetime_days();
        let now = chrono::Utc::now();
        let folder = match crate::models::folder::Folder::get_by_id(folder_id).await {
            Ok(Some(folder)) => Some(folder),
            Ok(None) => {
                warn!(
                    "Automation: discovery folder {folder_id} of user {} is gone, so no track expires",
                    user.username
                );
                None
            }
            Err(e) => {
                warn!(
                    "Automation: discovery folder {folder_id} of user {} cannot be read, so no track expires: {e}",
                    user.username
                );
                None
            }
        };

        match (folder, expirable_by_user.get(&user.id)) {
            (None, _) => {}
            (Some(_), None) => info!(
                "Automation: the rating sync of user {} failed, so no track expires",
                user.username
            ),
            (Some(folder), Some(expirable)) => {
                let expired = crate::core::discovery::expire_in(
                    &crate::db::DB,
                    &crate::import_queue::IMPORT_QUEUE,
                    &folder,
                    &lifetime_map,
                    now,
                    expirable,
                )
                .await;
                match expired {
                    Ok(0) => {}
                    Ok(n) => info!("Automation: expired {n} tracks for user {}", user.username),
                    Err(e) => {
                        warn!(
                            "Automation: pending tracks of user {} cannot be read, so no track expires: {e}",
                            user.username
                        );
                        continue;
                    }
                }
            }
        }

        // Refill any gaps (generate_discovery_playlist_internal accounts for existing tracks)
        match crate::server_fns::discovery::generate_discovery_playlist_internal(user_id).await {
            Ok(result) if result.total_imported > 0 => {
                info!(
                    "Automation: refilled {} tracks for user {}",
                    result.total_imported, user.username
                );
            }
            Ok(_) => {}
            Err(e) => info!(
                "Automation: refill failed for {}: {}",
                user.username, e.message
            ),
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn the_recommendation_cycle_starts_within_six_hours_of_boot() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let d = first_recommendation_delay(&mut rng);
            assert!(d >= Duration::from_secs(30) && d < Duration::from_secs(30 + 21_600));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cycle_that_runs_past_the_period_is_followed_by_a_full_period() {
        use std::sync::{Arc, Mutex};
        use tokio::time::Instant;

        let hour = Duration::from_secs(3600);
        let boot = Instant::now();
        let starts = Arc::new(Mutex::new(Vec::new()));
        let seen = starts.clone();
        let task = tokio::spawn(repeat_spaced(6 * hour, move || {
            let seen = seen.clone();
            async move {
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(boot.elapsed());
                    seen.len() == 1
                };
                tokio::time::sleep(if first { 7 * hour } else { hour }).await;
            }
        }));

        tokio::time::sleep(21 * hour).await;
        task.abort();
        assert_eq!(
            *starts.lock().unwrap(),
            vec![Duration::ZERO, 13 * hour, 20 * hour]
        );
    }
}
