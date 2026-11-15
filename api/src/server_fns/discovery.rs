use dioxus::prelude::*;
use shared::navidrome::{DiscoveryConfig, DiscoveryTrack};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock};
#[cfg(feature = "server")]
use tokio::sync::Mutex;

#[cfg(feature = "server")]
static GENERATION_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
async fn update_progress(
    user_id: &str,
    profile: &str,
    phase: shared::navidrome::ProfilePhase,
    current: u32,
    total: u32,
) {
    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
    if let Some(progress) = map.get_mut(user_id) {
        if let Some(pp) = progress.profiles.iter_mut().find(|p| p.profile == profile) {
            pp.phase = phase;
            pp.current = current;
            pp.total = total;
        }
    }
}

#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::import_queue::{Duplicates, ImportJob, IMPORT_QUEUE};
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::server_fns::download::enqueue::{
    enqueue_claimed, held_failure, not_enqueued_failure, transfer_key,
};
#[cfg(feature = "server")]
use crate::server_fns::download::failover::{self, SourcePool};
#[cfg(feature = "server")]
use crate::server_fns::download::monitor::{
    read_pending_retry, transfer_by_id, FAILED_STATE_CONFIRM,
};
#[cfg(feature = "server")]
use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner, TransferRegistry};
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::{failure_error, server_error};
#[cfg(feature = "server")]
use crate::core::{database_failure, target_unwritable};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
#[cfg(feature = "server")]
use shared::navidrome::DiscoveryProgress;

#[cfg(feature = "server")]
fn generation_running() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery generation already in progress",
    )
}

#[cfg(feature = "server")]
fn discovery_disabled() -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery is not enabled",
    )
}

#[cfg(feature = "server")]
fn choose_discovery_folder() -> UserAction {
    UserAction::new(
        UserActionCode::AddFolder,
        "Choose a discovery folder",
        "Settings > Library > Discovery",
    )
}

/// The user's settings name no discovery folder.
#[cfg(feature = "server")]
fn discovery_folder_required() -> Failure {
    Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "No download folder configured for discovery",
    )
    .with_user_action(choose_discovery_folder())
}

/// The discovery folder the user's settings name no longer exists.
#[cfg(feature = "server")]
fn discovery_folder_not_found() -> Failure {
    Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Discovery folder not found",
    )
    .with_user_action(choose_discovery_folder())
}

/// A database error of a generation run, shown as the database's own text.
#[cfg(feature = "server")]
fn run_database_failure(stage: Stage, e: String) -> Failure {
    database_failure(stage, &e).with_message(e)
}

/// Records `progress` as `user_id`'s run unless one is still running, in
/// which case nothing changes. The caller holds the `DISCOVERY_PROGRESS`
/// write lock, so two starts cannot both pass.
#[cfg(feature = "server")]
fn claim_run(
    runs: &mut HashMap<String, DiscoveryProgress>,
    user_id: &str,
    progress: DiscoveryProgress,
) -> Result<(), Failure> {
    if runs.get(user_id).is_some_and(|p| !p.is_terminal()) {
        return Err(generation_running());
    }
    runs.insert(user_id.to_string(), progress);
    Ok(())
}

/// Get the current user's discovery configuration
#[get("/api/discovery/config", auth: AuthSession)]
pub async fn get_discovery_config() -> Result<DiscoveryConfig, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let folder_name = if let Some(ref fid) = settings.discovery_folder_id {
        Folder::get_by_id(fid).await.ok().flatten().map(|f| f.name)
    } else {
        None
    };
    let track_counts = settings.parse_track_counts();
    let lifetime_days = settings.parse_lifetime_days();
    let playlist_names =
        serde_json::from_str(&settings.discovery_playlist_name).unwrap_or_default();
    Ok(DiscoveryConfig {
        enabled: settings.discovery_enabled,
        folder_id: settings.discovery_folder_id,
        folder_name,
        track_counts,
        lifetime_days,
        profiles: settings.discovery_profiles,
        playlist_names,
        last_generated_at: settings.discovery_last_generated_at,
    })
}

/// Get discovery tracks for the current user's configured folder
#[get("/api/discovery/tracks", auth: AuthSession)]
pub async fn get_discovery_tracks() -> Result<Vec<DiscoveryTrack>, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    if let Some(ref folder_id) = settings.discovery_folder_id {
        DiscoveryTrackRow::get_by_folder(folder_id)
            .await
            .map_err(server_error)
    } else {
        Ok(vec![])
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TrackActionRequest {
    pub track_id: String,
}

#[post("/api/discovery/promote", auth: AuthSession)]
pub async fn promote_discovery_track(req: TrackActionRequest) -> Result<(), ServerFnError> {
    super::navidrome::promote_discovery_track_internal(
        &req.track_id,
        &auth.0.user_id,
        crate::core::delete_guard::Origin::Ui,
    )
    .await
    .map_err(failure_error)
}

#[post("/api/discovery/remove", auth: AuthSession)]
pub async fn remove_discovery_track(req: TrackActionRequest) -> Result<(), ServerFnError> {
    use crate::core::delete_guard::staging::{remove_staged_in, StagedOutcome};

    let (track, folder) =
        super::navidrome::owned_discovery_track(&req.track_id, &auth.0.user_id, Stage::Delete)
            .await
            .map_err(failure_error)?;
    remove_staged_in(&crate::db::DB, &track, &folder, StagedOutcome::Removed)
        .await
        .map_err(failure_error)
}

#[post("/api/discovery/start-generation", auth: AuthSession)]
pub async fn start_discovery_generation() -> Result<(), ServerFnError> {
    use shared::navidrome::{GenerationStatus, ProfilePhase, ProfileProgress};

    let user_id = auth.0.user_id.clone();

    // Read settings to get profile list for initial progress state
    let settings = UserSettings::get(&user_id)
        .await
        .map_err(|e| failure_error(database_failure(Stage::Input, e)))?;
    if !settings.discovery_enabled {
        return Err(failure_error(discovery_disabled()));
    }
    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();
    let folder_id = settings
        .discovery_folder_id
        .as_ref()
        .ok_or_else(|| failure_error(discovery_folder_required()))?;

    // Determine which profiles need tracks (skip those already at target)
    let mut profile_entries = Vec::new();
    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let target = track_counts.get(&profile_name).copied().unwrap_or(10) as usize;
        let existing =
            DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
        let phase = if target.saturating_sub(existing) == 0 {
            ProfilePhase::Skipped
        } else {
            ProfilePhase::Waiting
        };
        profile_entries.push(ProfileProgress {
            profile: profile_name,
            phase,
            current: 0,
            total: 0,
        });
    }

    // Claim the run and write its initial progress BEFORE spawn (prevents
    // race with first poll)
    {
        let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
        claim_run(
            &mut map,
            &user_id,
            DiscoveryProgress {
                status: GenerationStatus::Running,
                profiles: profile_entries,
                started_at: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            },
        )
        .map_err(failure_error)?;
    }

    // Spawn background task
    let uid = user_id.clone();
    tokio::spawn(async move {
        match generate_discovery_playlist_internal(&uid).await {
            Ok(result) => {
                let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
                if let Some(p) = map.get_mut(&uid) {
                    p.status = GenerationStatus::Complete;
                    p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                    for pp in &mut p.profiles {
                        if pp.phase != ProfilePhase::Skipped {
                            pp.phase = ProfilePhase::Done;
                            pp.current = pp.total;
                        }
                    }
                    p.result = Some(result);
                }
            }
            Err(e) => {
                let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
                if let Some(p) = map.get_mut(&uid) {
                    p.status = GenerationStatus::Error;
                    p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                    p.failure = Some(e);
                }
            }
        }
    });

    Ok(())
}

#[get("/api/discovery/progress", auth: AuthSession)]
pub async fn get_discovery_progress(
) -> Result<Option<shared::navidrome::DiscoveryProgress>, ServerFnError> {
    let progress = crate::globals::DISCOVERY_PROGRESS.read().await;
    match progress.get(&auth.0.user_id) {
        Some(p) => {
            // Return None for terminal states older than 5 minutes
            if p.is_terminal() {
                if let Some(ref completed_at) = p.completed_at {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(completed_at) {
                        if chrono::Utc::now().signed_duration_since(ts).num_minutes() > 5 {
                            return Ok(None);
                        }
                    }
                }
            }
            Ok(Some(p.clone()))
        }
        None => Ok(None),
    }
}

/// A track discovery has enqueued and is waiting on, plus the peers it can
/// still fall back to.
#[cfg(feature = "server")]
struct QueuedTrack {
    artist: String,
    track: String,
    album: Option<String>,
    /// slskd's id for the transfer this track is waiting on. slskd keeps
    /// older records of the same peer and file, so only the id tells this
    /// transfer apart.
    transfer_id: String,
    /// The peer and remote filename of that transfer, claimed by `batch`.
    /// Moves to the new peer together with `transfer_id` on every retry.
    key: TransferKey,
    /// This track's own batch: the folder under slskd's download directory
    /// every attempt is enqueued into, and the owner of their registry keys.
    batch: BatchRef,
    /// When this track's transfer was first seen failed, cleared the moment
    /// it is seen in any other state.
    failing_since: Option<tokio::time::Instant>,
    /// When this track's transfer was first seen as a failed attempt slskd
    /// will retry (`read_pending_retry`).
    retry_pending_since: Option<tokio::time::Instant>,
    /// Alternate peers for this track, and how many have been tried.
    pool: SourcePool,
}

/// The track's import is done or abandoned once it is dropped, so whoever
/// asks for its files next may download them.
#[cfg(feature = "server")]
impl Drop for QueuedTrack {
    fn drop(&mut self) {
        TransferRegistry::global().release_batch(&self.batch.owner);
    }
}

/// What a discovery download did, as observed in the phase 2 poll loop.
#[cfg(feature = "server")]
enum Outcome {
    Succeeded,
    /// Transfer error: worth another peer.
    Failed,
    /// Cancelled by a user or by monitor cleanup: never retried.
    Cancelled,
}

/// Whether a discovery transfer in `state` delivered its file.
#[cfg(feature = "server")]
fn succeeded(state: &shared::download::DownloadState) -> bool {
    use shared::download::DownloadState as DS;
    matches!(state, DS::Completed | DS::Imported | DS::ImportSkipped)
}

/// Settle one poll of slskd's transfer list against the tracks still pending.
///
/// `pending` and `failed` hold transfer ids. Finished transfers leave
/// `pending`; a failed one retries from the track's next-best peer, moving
/// the track's key and transfer id to it, and only lands in `failed` once
/// the track is out of peers. A cancellation is a decision rather than a
/// peer problem, so it fails without a retry: one slskd shows, and one its
/// user asked for through the transfer registry, which is cancelled here
/// unless it already finished. A track that settles without a file gives up
/// its key.
///
/// A failure counts only once it has persisted for `FAILED_STATE_CONFIRM`,
/// the same window `DownloadMonitor` applies. slskd 0.26 retries failed
/// transfers itself and reports the failure in between attempts, so acting on
/// the first sighting would put a second peer on a track slskd is still
/// delivering, and the winner would be whichever finished last. At a 5s poll
/// interval this costs one extra poll.
#[cfg(feature = "server")]
async fn settle_poll(
    queued: &mut [QueuedTrack],
    downloads: &[shared::download::DownloadProgress],
    pending: &mut std::collections::HashSet<String>,
    failed: &mut std::collections::HashSet<String>,
    backend: &Arc<dyn soulbeet::DownloadBackend>,
) {
    let registry = TransferRegistry::global();
    let mut newly_done: Vec<(usize, String, Outcome)> = Vec::new();
    for (idx, qt) in queued.iter_mut().enumerate() {
        if !pending.contains(&qt.transfer_id) {
            continue;
        }
        let listed = transfer_by_id(downloads, &qt.key.peer, &qt.transfer_id);
        if registry.is_cancelled(&qt.key, &qt.batch.owner)
            && !listed.is_some_and(|dl| succeeded(&dl.state))
        {
            info!(
                "Discovery: '{}' - {} cancelled on request",
                qt.artist, qt.track
            );
            if let Err(e) = backend
                .cancel_download(&qt.key.peer, &qt.transfer_id, false)
                .await
            {
                warn!(
                    "Failed to cancel transfer {} in slskd: {}",
                    qt.transfer_id, e
                );
            }
            newly_done.push((idx, qt.transfer_id.clone(), Outcome::Cancelled));
            continue;
        }
        let Some(dl) = listed else {
            continue;
        };
        let dl = read_pending_retry(dl, &mut qt.retry_pending_since);
        if !matches!(dl.state, shared::download::DownloadState::Failed(_)) {
            qt.failing_since = None;
        }
        match &dl.state {
            state if succeeded(state) => {
                newly_done.push((idx, qt.transfer_id.clone(), Outcome::Succeeded));
            }
            shared::download::DownloadState::Failed(_) => {
                let failing_since = qt
                    .failing_since
                    .get_or_insert_with(tokio::time::Instant::now);
                if failing_since.elapsed() >= FAILED_STATE_CONFIRM {
                    newly_done.push((idx, qt.transfer_id.clone(), Outcome::Failed));
                }
            }
            shared::download::DownloadState::Cancelled => {
                newly_done.push((idx, qt.transfer_id.clone(), Outcome::Cancelled));
            }
            _ => {}
        }
    }

    for (idx, id, outcome) in newly_done {
        pending.remove(&id);
        let qt = &mut queued[idx];
        match outcome {
            // Its key stays claimed until the import is done.
            Outcome::Succeeded => {}
            Outcome::Cancelled => {
                registry.release(&qt.key, &qt.batch.owner);
                failed.insert(id);
            }
            Outcome::Failed => {
                let next = qt.pool.enqueue_next(0, backend, &qt.batch).await;
                for refused in &next.refused {
                    warn!(
                        "Discovery: '{}' - {} failover source {} refused: {}: {}",
                        qt.artist,
                        qt.track,
                        refused.source,
                        refused.failure.code.as_str(),
                        refused.failure.message
                    );
                }
                match next.queued {
                    Some((_, next)) => {
                        info!(
                            "Discovery: '{}' - {} failed from {}, retrying from {}",
                            qt.artist, qt.track, qt.key.peer, next.source
                        );
                        // The new peer serves this track under its own remote
                        // path and transfer id, so the whole identity moves at
                        // once, a cancel asked for meanwhile with it, and the
                        // fresh transfer starts with no failure recorded.
                        let dead = std::mem::replace(
                            &mut qt.key,
                            transfer_key(backend.id(), &next.source, &next.item),
                        );
                        registry.hand_over(&dead, &qt.key, &qt.batch.owner);
                        qt.transfer_id = next.id.clone();
                        qt.failing_since = None;
                        qt.retry_pending_since = None;
                        pending.insert(next.id);
                    }
                    None => {
                        registry.release(&qt.key, &qt.batch.owner);
                        failed.insert(id);
                    }
                }
            }
        }
    }
}

#[cfg(feature = "server")]
pub async fn generate_discovery_playlist_internal(
    user_id: &str,
) -> Result<shared::navidrome::GenerationResult, Failure> {
    use crate::models::deletion_review::DeletionReviewRow;
    use crate::models::discovery_candidate::DiscoveryCandidateRow;
    use crate::models::discovery_history::DiscoveryHistoryRow;
    use crate::services::download_backend;
    use shared::navidrome::{GenerationResult, ProfileGenerationStats};

    // Acquire per-user lock to prevent concurrent generation
    let user_lock = {
        let mut locks = GENERATION_LOCKS.lock().await;
        locks
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let Ok(_guard) = user_lock.try_lock() else {
        return Err(generation_running());
    };

    let settings = UserSettings::get(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    if !settings.discovery_enabled {
        return Err(discovery_disabled());
    }
    let folder_id = settings
        .discovery_folder_id
        .as_ref()
        .ok_or_else(discovery_folder_required)?;
    let folder = Folder::get_by_id(folder_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?
        .ok_or_else(discovery_folder_not_found)?;

    // Shown as the run's error, so the user sees why nothing was imported.
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }

    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();

    let backend = download_backend(None)
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;

    // Create per-profile subdirectories under Discovery/
    for profile in &selected_profiles {
        let profile_dir = folder.discovery_profile_path(&profile.to_string());
        tokio::fs::create_dir_all(&profile_dir).await.map_err(|e| {
            target_unwritable(
                &e,
                format!("Failed to create Discovery/{} dir: {}", profile, e),
            )
        })?;
    }

    // Build the "already seen" set for deduplication
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Past discovery suggestions
    let history_keys = DiscoveryHistoryRow::get_suggested_keys(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!(
        "Loaded {} tracks from discovery history",
        history_keys.len()
    );
    seen.extend(history_keys);

    // Past deletions (user doesn't want these back)
    let disliked = DeletionReviewRow::disliked_keys(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!("Added {} deleted tracks to exclusion set", disliked.len());
    seen.extend(disliked);

    let mut total_downloads = 0u32;
    let mut all_profile_stats: Vec<ProfileGenerationStats> = Vec::new();
    // Set when the importer broke during the run and stopped it.
    let mut importer_issue: Option<Failure> = None;

    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let target = track_counts.get(&profile_name).copied().unwrap_or(10) as usize;

        // Account for tracks already in the playlist so regeneration fills gaps
        let existing =
            DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
        let tracks_per_profile = target.saturating_sub(existing);
        if tracks_per_profile == 0 {
            update_progress(
                user_id,
                &profile_name,
                shared::navidrome::ProfilePhase::Skipped,
                0,
                0,
            )
            .await;
            info!(
                "{}: already at target ({} pending tracks)",
                profile_name, existing
            );
            continue;
        }
        update_progress(
            user_id,
            &profile_name,
            shared::navidrome::ProfilePhase::PullingCandidates,
            0,
            0,
        )
        .await;
        info!(
            "{}: need {} more tracks ({} existing, {} target)",
            profile_name, tracks_per_profile, existing, target
        );

        let mut profile_downloads = 0u32;
        let mut stats = ProfileGenerationStats {
            profile: profile_name.clone(),
            target: tracks_per_profile as u32,
            ..Default::default()
        };

        // Retry loop: keep pulling new candidates until target is met or pool is dry.
        // Each attempt searches Soulseek, downloads, and imports. Tracks that fail
        // at any stage are added to `seen` so the next attempt skips them.
        for _attempt in 0..3u32 {
            stats.attempts += 1;
            let remaining = tracks_per_profile.saturating_sub(profile_downloads as usize);
            if remaining == 0 {
                break;
            }

            // Over-fetch candidates (3x) to survive search/download/import attrition
            let fetch_count = (remaining as f64 * 3.0).ceil() as u32;
            let mut candidates =
                DiscoveryCandidateRow::get_unused(user_id, &profile_name, fetch_count)
                    .await
                    .map_err(|e| run_database_failure(Stage::Input, e))?;

            if candidates.is_empty() {
                if _attempt == 0 {
                    update_progress(
                        user_id,
                        &profile_name,
                        shared::navidrome::ProfilePhase::GeneratingRecommendations,
                        0,
                        0,
                    )
                    .await;
                    info!("No candidates for {}, running engine", profile_name);
                    match generate_recommendations_for_user(user_id, *profile).await {
                        Ok(count) => info!("Generated {} candidates for {}", count, profile_name),
                        Err(e) => {
                            warn!("{} engine failed: {}", profile_name, e);
                            break;
                        }
                    }
                    candidates =
                        DiscoveryCandidateRow::get_unused(user_id, &profile_name, fetch_count)
                            .await
                            .map_err(|e| run_database_failure(Stage::Input, e))?;
                }
                if candidates.is_empty() {
                    info!("{}: no more candidates available", profile_name);
                    break;
                }
            }

            if let Some(failure) = crate::importer_health::blocking_failure().await {
                warn!("Discovery for {user_id} skipped: {}", failure.message);
                importer_issue = Some(failure);
                break;
            }

            // Phase 1: Search and queue downloads
            let mut queued: Vec<QueuedTrack> = Vec::new();

            // Over-queue by 50% to absorb import failures without another retry round
            let queue_target = remaining + remaining.div_ceil(2);

            for candidate in &candidates {
                if queued.len() >= queue_target {
                    break;
                }

                let key = format!(
                    "{}:{}",
                    candidate.artist.to_lowercase(),
                    candidate.track.to_lowercase()
                );
                if seen.contains(&key) {
                    stats.candidates_skipped_seen += 1;
                    continue;
                }
                stats.candidates_tried += 1;
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::SearchingSoulseek,
                    stats.candidates_tried,
                    candidates.len() as u32,
                )
                .await;

                let search_tracks = vec![shared::metadata::Track {
                    id: String::new(),
                    title: candidate.track.clone(),
                    artist: candidate.artist.clone(),
                    album_id: None,
                    album_title: candidate.album.clone(),
                    release_date: None,
                    duration: None,
                    mbid: None,
                    release_mbid: None,
                }];

                let search_id = match backend.start_search(None, &search_tracks).await {
                    Ok(id) => id,
                    Err(e) => {
                        warn!(
                            "Search failed for '{}' - {}: {}",
                            candidate.artist, candidate.track, e
                        );
                        stats.search_errors += 1;
                        seen.insert(key.clone());
                        DiscoveryCandidateRow::mark_used(
                            user_id,
                            &profile_name,
                            &candidate.artist,
                            &candidate.track,
                        )
                        .await
                        .map_err(|e| run_database_failure(Stage::SourceSearch, e))?;
                        continue;
                    }
                };

                // Poll until we get results or the search times out (slskd search has 120s timeout).
                // Each poll_search call long-polls for up to 10s internally.
                // Collect up to 3 ranked items so we can fall back if a download fails.
                let mut ranked_items: Vec<shared::download::DownloadableItem> = Vec::new();
                for _ in 0..12 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    let search_result = match backend.poll_search(&search_id).await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                "Poll failed for '{}' - {}: {}",
                                candidate.artist, candidate.track, e
                            );
                            break;
                        }
                    };

                    if !search_result.groups.is_empty() {
                        for group in &search_result.groups {
                            for item in &group.items {
                                ranked_items.push(item.clone());
                            }
                        }
                        break;
                    }

                    if !search_result.has_more {
                        break;
                    }
                }

                if ranked_items.is_empty() {
                    info!(
                        "No results for '{}' - {}, skipping",
                        candidate.artist, candidate.track
                    );
                    stats.search_misses += 1;
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::SourceSearch, e))?;
                    continue;
                }
                stats.search_hits += 1;

                // Keep the sources this track is allowed to try: the one
                // enqueued here plus the alternates phase 2 fails over to.
                ranked_items.truncate(failover::MAX_SOURCES_PER_TRACK);
                let track_batch = BatchRef::new(
                    TransferOwner {
                        user_id: UserId::new(user_id),
                        batch_id: uuid::Uuid::new_v4().to_string(),
                    },
                    format!("{} - {}", candidate.artist, candidate.track),
                    std::path::PathBuf::from(folder.discovery_profile_path(&profile_name)),
                    soulbeet::ImportMode::Singleton,
                );
                // Discovery watches its own transfers, so nothing may join it.
                track_batch.inbox.close();
                let mut downloaded = false;
                for (attempt_idx, item) in ranked_items.iter().enumerate() {
                    let transfer = transfer_key(backend.id(), &item.source, &item.id);
                    let claim = TransferRegistry::global()
                        .claim(std::slice::from_ref(&transfer), &track_batch);
                    if claim.claimed.is_empty() {
                        info!(
                            "Download source {} skipped for '{}' - {}: {}",
                            attempt_idx + 1,
                            candidate.artist,
                            candidate.track,
                            held_failure(&claim, &track_batch).message
                        );
                        stats.downloads_failed += 1;
                        continue;
                    }
                    // Discovery keeps no ledger, so nothing reads its request back.
                    let request = backend.new_request();
                    let download_results =
                        match enqueue_claimed(&backend, vec![item.clone()], &track_batch, &request)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                if attempt_idx + 1 < ranked_items.len() {
                                    info!(
                                        "Download source {} failed for '{}' - {}, trying next: {}",
                                        attempt_idx + 1,
                                        candidate.artist,
                                        candidate.track,
                                        e
                                    );
                                } else {
                                    warn!(
                                        "Download failed for '{}' - {} (all sources exhausted): {}",
                                        candidate.artist, candidate.track, e
                                    );
                                }
                                stats.downloads_failed += 1;
                                continue;
                            }
                        };

                    if let Some(dl) = download_results
                        .iter()
                        .find(|dl| dl.status == shared::download::EnqueueStatus::Enqueued)
                    {
                        stats.downloads_queued += 1;
                        seen.insert(key.clone());
                        queued.push(QueuedTrack {
                            artist: candidate.artist.clone(),
                            track: candidate.track.clone(),
                            album: candidate.album.clone(),
                            transfer_id: dl.id.clone(),
                            key: transfer,
                            batch: track_batch.clone(),
                            failing_since: None,
                            retry_pending_since: None,
                            // Start the pool at the source that actually
                            // enqueued: earlier entries were already tried and
                            // rejected in this loop, and must not be retried.
                            pool: SourcePool::single(ranked_items[attempt_idx..].to_vec()),
                        });
                        DiscoveryCandidateRow::mark_used(
                            user_id,
                            &profile_name,
                            &candidate.artist,
                            &candidate.track,
                        )
                        .await
                        .map_err(|e| run_database_failure(Stage::Enqueue, e))?;
                        downloaded = true;
                        break;
                    }
                    // This source refused the file in its response
                    stats.downloads_failed += 1;
                    if attempt_idx + 1 < ranked_items.len() {
                        info!(
                            "Download source {} refused '{}' - {} ({}), trying next",
                            attempt_idx + 1,
                            candidate.artist,
                            candidate.track,
                            not_enqueued_failure(&download_results).message
                        );
                    }
                }
                if !downloaded {
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Enqueue, e))?;
                }
            }

            if queued.is_empty() {
                break;
            }

            // Phase 2: Wait for downloads to complete, then move to Discovery/
            info!(
                "{}: {} downloads queued, waiting for completion (attempt {})...",
                profile_name,
                queued.len(),
                _attempt + 1,
            );
            let download_base = crate::config::CONFIG.download_path().clone();

            // Poll slskd until all queued downloads are complete (or timeout after 10 min).
            // Note: This 10-minute timeout starts from enqueue, not from transfer start.
            // A slow peer (3 min queue wait + 8 min transfer) could exceed it. This is
            // acceptable because: (1) discovery tracks are single songs, not albums,
            // (2) most Soulseek transfers start quickly, and (3) timed-out tracks are
            // recorded in stats with no data loss -- they can be rediscovered later.
            let wait_start = tokio::time::Instant::now();
            let max_wait = tokio::time::Duration::from_secs(600);
            let mut pending_ids: std::collections::HashSet<String> =
                queued.iter().map(|q| q.transfer_id.clone()).collect();
            let mut failed_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            while !pending_ids.is_empty() && wait_start.elapsed() < max_wait {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let completed = (queued.len() as u32)
                    .saturating_sub(pending_ids.len() as u32 + failed_ids.len() as u32);
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::Downloading,
                    completed,
                    queued.len() as u32,
                )
                .await;

                // Only the peers this batch is waiting on: the full list
                // carries slskd's entire retained history (#78). Rebuilt each
                // pass so a failover's new peer is included.
                let mut sources: Vec<String> = Vec::new();
                for track in &queued {
                    if !sources.contains(&track.key.peer) {
                        sources.push(track.key.peer.clone());
                    }
                }

                let downloads = match backend.get_downloads_for(&sources).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                settle_poll(
                    &mut queued,
                    &downloads,
                    &mut pending_ids,
                    &mut failed_ids,
                    &backend,
                )
                .await;
            }

            stats.downloads_timed_out += pending_ids.len() as u32;
            stats.downloads_failed += failed_ids.len() as u32;
            stats.downloads_completed +=
                (queued.len() - pending_ids.len() - failed_ids.len()) as u32;
            if !pending_ids.is_empty() {
                warn!(
                    "{}: {} downloads didn't complete within timeout",
                    profile_name,
                    pending_ids.len()
                );
            }
            if !failed_ids.is_empty() {
                warn!(
                    "{}: {} downloads failed in slskd",
                    profile_name,
                    failed_ids.len()
                );
            }

            // Phase 3: Import completed files into Discovery/{profile}/ via beets and record in DB
            let profile_path = folder.discovery_profile_path(&profile_name);
            let discovery_target = std::path::PathBuf::from(&profile_path);

            // Skip both timed-out and slskd-failed downloads
            let skip_ids: std::collections::HashSet<&String> =
                pending_ids.iter().chain(failed_ids.iter()).collect();

            let importable_count = queued
                .iter()
                .filter(|q| !skip_ids.contains(&q.transfer_id))
                .count() as u32;
            let mut import_idx = 0u32;
            for qt in &queued {
                if skip_ids.contains(&qt.transfer_id) {
                    continue;
                }
                import_idx += 1;
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::Importing,
                    import_idx,
                    importable_count,
                )
                .await;

                let resolved = crate::server_fns::download::process::resolve_on_disk(
                    &qt.key.filename,
                    &download_base,
                    Some(&qt.batch.destination),
                    &std::sync::Mutex::new(std::collections::HashSet::new()),
                )
                .await;
                let src = match resolved {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(
                            "Could not find downloaded file for '{}' - {} (slskd: {})",
                            qt.artist, qt.track, qt.key.filename
                        );
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        stats.imports_file_missing += 1;
                        continue;
                    }
                };

                let outcome = IMPORT_QUEUE
                    .submit(ImportJob {
                        sources: vec![src.clone()],
                        target: discovery_target.clone(),
                        mode: soulbeet::ImportMode::Singleton,
                        owner: None,
                        snapshot_target: true,
                        cleanup_root: Some(download_base.clone()),
                        choice: soulbeet::DriverChoice::Auto,
                        choices: None,
                        // Discovery's staging keeps its own rules (section 13).
                        duplicates: Duplicates::AsConfigured,
                    })
                    .await
                    .wait()
                    .await;
                TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                let result = &outcome.result;
                match classify_discovery(&result.outcome) {
                    DiscoveryImport::Imported => {
                        info!(
                            "Imported '{}' - {} into Discovery/{}",
                            qt.artist, qt.track, profile_name
                        );
                    }
                    DiscoveryImport::Permanent => {
                        warn!("Beets skipped '{}' - {}: {result}", qt.artist, qt.track);
                        stats.imports_skipped += 1;
                        discard_download(&src, &download_base).await;
                        // A duplicate is a permanent condition: the library
                        // already holds this track, so suggesting it again
                        // would download it again. Only genuine failures
                        // stay eligible for rediscovery.
                        DiscoveryHistoryRow::record(user_id, &qt.artist, &qt.track, &profile_name)
                            .await
                            .map_err(|e| run_database_failure(Stage::Import, e))?;
                        continue;
                    }
                    DiscoveryImport::Failed => {
                        warn!(
                            "Beets import failed for '{}' - {}: {result}",
                            qt.artist, qt.track
                        );
                        stats.imports_failed += 1;
                        discard_download(&src, &download_base).await;
                        continue;
                    }
                }

                let new_files = &outcome.new_files;
                if new_files.is_empty() {
                    warn!(
                        "Could not find imported file for '{}' - {} in {}",
                        qt.artist, qt.track, profile_path
                    );
                    stats.imports_file_missing += 1;
                    continue;
                }
                for rel in new_files {
                    DiscoveryTrackRow::create(
                        None,
                        &qt.track,
                        &qt.artist,
                        qt.album.as_deref().unwrap_or(""),
                        &discovery_target.join(rel).to_string_lossy(),
                        folder_id,
                        &profile_name,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Import, e))?;
                }
                // Recorded here, not at enqueue: discovery history is the
                // permanent exclusion set, and a track killed by a bad peer
                // must stay available to suggest again.
                DiscoveryHistoryRow::record(user_id, &qt.artist, &qt.track, &profile_name)
                    .await
                    .map_err(|e| run_database_failure(Stage::Import, e))?;
                stats.imports_succeeded += 1;
                profile_downloads += 1;
            }

            if (profile_downloads as usize) >= tracks_per_profile {
                break;
            }
            info!(
                "{}: {}/{} tracks after attempt {}, retrying with more candidates...",
                profile_name,
                profile_downloads,
                tracks_per_profile,
                _attempt + 1
            );
        } // end retry loop

        info!(
            "{}: {} tracks imported to Discovery/",
            profile_name, profile_downloads
        );

        total_downloads += profile_downloads;
        all_profile_stats.push(stats);
    }

    UserSettings::update_discovery_last_generated(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Import, e))?;

    // Trigger a Navidrome scan then wait for it to finish so playlists can be created.
    if total_downloads > 0 {
        if let Ok(navi) = crate::services::navidrome_client_for_user(user_id).await {
            if scan_imports(&navi, total_downloads).await {
                // Create/update smart playlists now that tracks are indexed
                if let Err(e) = reconcile_discovery_playlists(user_id).await {
                    warn!("Playlist reconciliation after generation failed: {}", e);
                }
            }
        }
    }

    if let Some(issue) = importer_issue {
        return Err(issue);
    }
    Ok(GenerationResult {
        total_imported: total_downloads,
        profiles: all_profile_stats,
    })
}

/// Starts a Navidrome scan of `imported` new tracks and waits up to two
/// minutes for it to end. Returns whether the scan was started.
///
/// The scan runs in a goroutine and startScan answers at once, so "not
/// scanning" alone can be read before it begins: the wait is for a scan that
/// ended after the one recorded before it. When that cannot be read the scan
/// is still started, only not waited for.
#[cfg(feature = "server")]
async fn scan_imports(navi: &soulbeet::navidrome::NavidromeClient, imported: u32) -> bool {
    let before = match navi.get_scan_status().await {
        Ok(status) => Some(status.last_scan),
        Err(e) => {
            warn!("Failed to read Navidrome scan status, not waiting for the scan: {e}");
            None
        }
    };
    if let Err(e) = navi.start_scan().await {
        warn!("Failed to trigger Navidrome scan: {}", e);
        return false;
    }
    info!(
        "Triggered Navidrome library scan after {} discovery imports",
        imported
    );
    let Some(before) = before else {
        return true;
    };
    // Poll every 3s, at most 2 min
    for _ in 0..40u32 {
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        match navi.get_scan_status().await {
            Ok(status) if status.ended_after(before) => {
                if !status.error.is_empty() {
                    warn!("Navidrome scan reported an error: {}", status.error);
                }
                info!("Navidrome scan ended after {} items", status.count);
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    true
}

/// Create or update smart playlists in Navidrome for each discovery profile.
///
/// Uses Navidrome's native API to create smart playlists with a filepath rule
/// that matches the Discovery/{profile}/ directory. The playlist auto-updates
/// as Navidrome scans new files. Owned by the authenticated user.
#[cfg(feature = "server")]
pub async fn reconcile_discovery_playlists(user_id: &str) -> Result<(), String> {
    let settings = UserSettings::get(user_id).await?;
    if !settings.discovery_enabled {
        return Ok(());
    }
    let folder_id = match settings.discovery_folder_id.as_ref() {
        Some(id) => id.clone(),
        None => return Ok(()),
    };
    Folder::get_by_id(&folder_id)
        .await?
        .ok_or("Discovery folder not found")?;

    let navi = match crate::services::navidrome_client_for_user(user_id).await {
        Ok(c) => c,
        Err(e) => {
            info!(
                "Playlist reconciliation skipped (no Navidrome client): {}",
                e
            );
            return Ok(());
        }
    };

    let selected_profiles = parse_profiles(&settings.discovery_profiles);

    for profile in &selected_profiles {
        let profile_name = profile.to_string();

        // Delete stale playlist so we can recreate with the correct rule
        if let Some(old_id) = UserSettings::get_playlist_id_for_profile(
            &settings.discovery_navidrome_playlist_id,
            &profile_name,
        ) {
            let _ = navi.delete_smart_playlist(&old_id).await;
        }

        let playlist_name = UserSettings::get_playlist_name_for_profile(
            &settings.discovery_playlist_name,
            &profile_name,
        );

        // Derive the Navidrome-relative path prefix by sampling a song from this profile's folder.
        // The native API returns the raw media_file.path (relative to library root), which is what
        // the filepath smart playlist operator matches against.
        let profile_path = match navi.get_songs_by_path_prefix(&profile_name, 50).await {
            Ok(songs) if !songs.is_empty() => {
                // Extract the directory prefix from the first song's path.
                // Song path looks like "Discovery/Balanced/Artist/Album/track.flac"
                // or "staging/Discovery/Balanced/Artist/Album/track.flac"
                // We want everything up to and including the profile name.
                let song_path = &songs[0].path;
                if let Some(idx) = song_path.find(&format!("{}/", profile_name)) {
                    let end = idx + profile_name.len();
                    song_path[..end].to_string()
                } else {
                    // Song matched but path doesn't contain profile name as a directory
                    warn!(
                        "Could not extract profile prefix from song path '{}', falling back to Discovery/{}",
                        song_path, profile_name
                    );
                    format!("Discovery/{}", profile_name)
                }
            }
            _ => {
                // No songs imported yet (first run) or API error: use hardcoded default.
                // This is correct because on first run the folder structure matches the default.
                format!("Discovery/{}", profile_name)
            }
        };
        let comment = format!("Soulbeet discovery ({})", profile_name);

        match navi
            .create_smart_playlist(&playlist_name, &comment, &profile_path)
            .await
        {
            Ok(playlist_id) => {
                if let Err(e) =
                    UserSettings::update_discovery_playlist_id(user_id, &profile_name, &playlist_id)
                        .await
                {
                    warn!("Failed to save playlist ID for '{}': {}", profile_name, e);
                }
                info!(
                    "Created smart playlist '{}' (path filter: {}) for user {}",
                    playlist_name, profile_path, user_id
                );
            }
            Err(e) => {
                warn!("Failed to create smart playlist '{}': {}", playlist_name, e);
            }
        }
    }

    Ok(())
}

#[post("/api/discovery/generate-recommendations", auth: AuthSession)]
pub async fn generate_recommendations() -> Result<u32, ServerFnError> {
    generate_recommendations_internal(&auth.0.user_id)
        .await
        .map_err(server_error)
}

/// How a discovery import counts. Config and runtime failures must not land
/// in the history, or the track could never be suggested again.
#[cfg(feature = "server")]
#[derive(Debug, PartialEq)]
enum DiscoveryImport {
    Imported,
    /// The library cannot take this track: recorded so it is not suggested again.
    Permanent,
    Failed,
}

#[cfg(feature = "server")]
fn classify_discovery(outcome: &soulbeet::ImportOutcome) -> DiscoveryImport {
    use soulbeet::{ImportOutcome, SkipReason};
    match outcome {
        ImportOutcome::Imported { .. } => DiscoveryImport::Imported,
        ImportOutcome::Skipped(
            SkipReason::NoMatch { .. } | SkipReason::AlreadyInLibrary { .. },
        ) => DiscoveryImport::Permanent,
        ImportOutcome::Skipped(SkipReason::NothingToImport)
        | ImportOutcome::LookupFailed { .. }
        | ImportOutcome::Failed { .. }
        | ImportOutcome::TimedOut { .. }
        | ImportOutcome::Refused { .. } => DiscoveryImport::Failed,
    }
}

/// Deletes a download discovery could not import (it has no retry) and
/// prunes the folder it leaves empty.
#[cfg(feature = "server")]
async fn discard_download(src: &std::path::Path, download_base: &std::path::Path) {
    let _ = tokio::fs::remove_file(src).await;
    if let Some(dir) = src.parent() {
        let _ = super::cleanup_empty_ancestors(dir, download_base).await;
    }
}

#[cfg(feature = "server")]
fn parse_profiles(s: &str) -> Vec<shared::recommendation::DiscoveryProfile> {
    let mut profiles = Vec::new();
    for part in s.split(',') {
        if let Ok(p) = part.trim().parse() {
            profiles.push(p);
        }
    }
    if profiles.is_empty() {
        profiles.push(shared::recommendation::DiscoveryProfile::default());
    }
    profiles
}

#[cfg(feature = "server")]
pub async fn generate_recommendations_internal(user_id: &str) -> Result<u32, String> {
    use shared::recommendation::DiscoveryProfile;

    let profiles = [
        DiscoveryProfile::Conservative,
        DiscoveryProfile::Balanced,
        DiscoveryProfile::Adventurous,
    ];
    let mut total = 0u32;

    for profile in &profiles {
        match generate_recommendations_for_user(user_id, *profile).await {
            Ok(count) => total += count,
            Err(e) => {
                tracing::warn!("Profile {} failed: {}", profile, e);
            }
        }
    }

    Ok(total)
}

#[cfg(feature = "server")]
pub async fn generate_recommendations_for_user(
    user_id: &str,
    discovery_profile: shared::recommendation::DiscoveryProfile,
) -> Result<u32, String> {
    use crate::models::discovery_candidate::DiscoveryCandidateRow;
    use crate::models::engine_report::EngineReportRow;
    use crate::models::user_profile::UserProfileRow;
    use std::sync::Arc;

    let settings = UserSettings::get(user_id).await?;

    // Build providers based on user's configured credentials
    let mut providers: Vec<Arc<dyn soulbeet::ScrobbleProvider>> = Vec::new();
    let mut generators: Vec<Arc<dyn soulbeet::CandidateGenerator>> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    let lb_configured = matches!(
        (&settings.listenbrainz_token, &settings.listenbrainz_username),
        (Some(t), Some(u)) if !t.is_empty() && !u.is_empty()
    );
    let lfm_configured = matches!(
        (&settings.lastfm_api_key, &settings.lastfm_username),
        (Some(k), Some(u)) if !k.is_empty() && !u.is_empty()
    );

    if lb_configured {
        let lb = Arc::new(soulbeet::ListenBrainzProvider::new(
            settings.listenbrainz_username.as_ref().unwrap().clone(),
            settings.listenbrainz_token.clone(),
        ));
        providers.push(lb.clone());
        generators.push(Arc::new(
            soulbeet::engine::listenbrainz_pipeline::ListenBrainzPipeline::new(lb),
        ));
    } else {
        skipped.push(format!(
            "ListenBrainz: {}",
            if settings
                .listenbrainz_token
                .as_ref()
                .is_none_or(|t| t.is_empty())
            {
                "no token"
            } else {
                "no username"
            }
        ));
    }

    if lfm_configured {
        let lfm = Arc::new(soulbeet::LastFmProvider::with_user(
            settings.lastfm_api_key.as_ref().unwrap().clone(),
            settings.lastfm_username.as_ref().unwrap().clone(),
        ));
        providers.push(lfm.clone());
        generators.push(Arc::new(
            soulbeet::engine::lastfm_pipeline::LastFmPipeline::new(lfm),
        ));
    } else {
        skipped.push(format!(
            "Last.fm: {}",
            if settings
                .lastfm_api_key
                .as_ref()
                .is_none_or(|t| t.is_empty())
            {
                "no API key"
            } else {
                "no username"
            }
        ));
    }

    info!(
        "Provider registration: {} active, {} skipped{}",
        providers.len(),
        skipped.len(),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(" ({})", skipped.join(", "))
        }
    );

    if providers.is_empty() {
        return Err(
            format!("No scrobble providers configured ({}). Set up Last.fm or ListenBrainz in Settings > Library.",
                skipped.join("; "))
        );
    }

    // Use first available provider for profile building
    let profile_provider = &*providers[0];

    let config = shared::recommendation::ProfileConfig::from_profile(discovery_profile);

    let (profile, candidates, report) = soulbeet::engine::build_and_recommend(
        &providers.iter().map(Clone::clone).collect::<Vec<_>>(),
        &generators.iter().map(Clone::clone).collect::<Vec<_>>(),
        profile_provider,
        &config,
        50,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Cache profile
    UserProfileRow::upsert(user_id, &profile).await?;

    // Store engine report in history
    let report_json = serde_json::to_string(&report).map_err(|e| e.to_string())?;
    EngineReportRow::insert(
        user_id,
        &discovery_profile.to_string(),
        &report_json,
        candidates.len() as u32,
    )
    .await?;
    EngineReportRow::prune(user_id, 30).await?;

    // Clear and store candidates for this specific profile
    let profile_name = discovery_profile.to_string();
    DiscoveryCandidateRow::clear_for_user_profile(user_id, &profile_name).await?;
    DiscoveryCandidateRow::upsert_batch(user_id, &profile_name, &candidates).await?;

    Ok(candidates.len() as u32)
}

#[get("/api/discovery/report", auth: AuthSession)]
pub async fn get_engine_report() -> Result<String, ServerFnError> {
    #[cfg(feature = "server")]
    {
        use crate::models::engine_report::EngineReportRow;
        // Try new history table first, fall back to legacy user_profiles.last_report
        let rows = EngineReportRow::get_history(&auth.0.user_id, 1)
            .await
            .map_err(server_error)?;
        if let Some(row) = rows.into_iter().next() {
            let report: shared::recommendation::EngineReport =
                serde_json::from_str(&row.report_json).map_err(|e| server_error(e.to_string()))?;
            return Ok(report.to_text());
        }
        // Legacy fallback
        use crate::models::user_profile::UserProfileRow;
        let report_json = UserProfileRow::get_report(&auth.0.user_id)
            .await
            .map_err(server_error)?;
        match report_json {
            Some(json) => {
                let report: shared::recommendation::EngineReport =
                    serde_json::from_str(&json).map_err(|e| server_error(e.to_string()))?;
                Ok(report.to_text())
            }
            None => Ok("No engine report available. Run discovery generation first.".to_string()),
        }
    }
    #[cfg(not(feature = "server"))]
    Ok(String::new())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ReportEntry {
    pub profile: String,
    pub candidate_count: u32,
    pub created_at: String,
    pub text: String,
}

#[get("/api/discovery/reports", auth: AuthSession)]
pub async fn get_engine_reports() -> Result<Vec<ReportEntry>, ServerFnError> {
    #[cfg(feature = "server")]
    {
        use crate::models::engine_report::EngineReportRow;
        let rows = EngineReportRow::get_history(&auth.0.user_id, 30)
            .await
            .map_err(server_error)?;
        let mut entries = Vec::new();
        for row in rows {
            let text = match serde_json::from_str::<shared::recommendation::EngineReport>(
                &row.report_json,
            ) {
                Ok(report) => report.to_text(),
                Err(_) => "Failed to parse report".to_string(),
            };
            entries.push(ReportEntry {
                profile: row.profile,
                candidate_count: row.candidate_count as u32,
                created_at: row.created_at,
                text,
            });
        }
        Ok(entries)
    }
    #[cfg(not(feature = "server"))]
    Ok(Vec::new())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::{
        claim_run, discovery_disabled, discovery_folder_not_found, discovery_folder_required,
        generation_running, run_database_failure, scan_imports, settle_poll, QueuedTrack,
        FAILED_STATE_CONFIRM,
    };
    use crate::server_fns::download::enqueue::transfer_key;
    use crate::server_fns::download::failover::SourcePool;
    use crate::server_fns::download::monitor::RETRY_PENDING_FOR;
    use crate::server_fns::download::test_support::{
        batch, item, transfer, Cancel, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferRegistry;
    use shared::download::{DownloadProgress, DownloadState, DownloadableItem};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserActionCode};
    use shared::navidrome::{DiscoveryProgress, GenerationStatus};
    use soulbeet::DownloadBackend;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    #[test]
    fn discovery_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, retry, message) in [
            (
                generation_running(),
                AlreadyRunning,
                NotRetryable,
                "Discovery generation already in progress",
            ),
            (
                discovery_disabled(),
                InvalidInput,
                NotRetryable,
                "Discovery is not enabled",
            ),
            (
                discovery_folder_required(),
                FolderRequired,
                NeedsUser,
                "No download folder configured for discovery",
            ),
            (
                discovery_folder_not_found(),
                FolderRequired,
                NeedsUser,
                "Discovery folder not found",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, Stage::Input, retry, message)
            );
        }
        for f in [discovery_folder_required(), discovery_folder_not_found()] {
            let action = f.user_action.unwrap();
            assert_eq!(
                (action.code, action.what.as_str(), action.where_.as_str()),
                (
                    UserActionCode::AddFolder,
                    "Choose a discovery folder",
                    "Settings > Library > Discovery"
                )
            );
        }
    }

    #[test]
    fn a_run_database_failure_keeps_the_database_text_at_its_stage() {
        use shared::failure::{Cause, Service};
        let raw = "error returned from database: (code: 5) database is locked";
        let f = run_database_failure(Stage::SourceSearch, raw.to_string());
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                raw
            )
        );
        assert_eq!(f.cause, Some(Cause::new(Service::Database, None, raw)));
    }

    #[test]
    fn a_second_start_is_refused_while_the_first_runs() {
        let running = || DiscoveryProgress {
            status: GenerationStatus::Running,
            ..Default::default()
        };
        let mut runs = HashMap::new();
        assert_eq!(claim_run(&mut runs, "user-a", running()), Ok(()));
        assert_eq!(
            claim_run(&mut runs, "user-a", running()),
            Err(generation_running())
        );
        assert_eq!(claim_run(&mut runs, "user-b", running()), Ok(()));
        runs.get_mut("user-a").unwrap().status = GenerationStatus::Complete;
        assert_eq!(claim_run(&mut runs, "user-a", running()), Ok(()));
    }

    #[test]
    fn classify_discovery_keeps_config_failures_out_of_history() {
        use super::{classify_discovery, DiscoveryImport};
        use soulbeet::{ImportOutcome, SkipReason, TimeoutKind};
        use std::time::Duration;

        let cases = [
            (
                ImportOutcome::Imported {
                    untagged: false,
                    warnings: vec![],
                    items: vec![],
                    failure: None,
                },
                DiscoveryImport::Imported,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NoMatch { candidates: None }),
                DiscoveryImport::Permanent,
            ),
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
                DiscoveryImport::Permanent,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NothingToImport),
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::LookupFailed {
                    source: "MusicBrainz".into(),
                    message: "timed out".into(),
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::Failed {
                    kind: soulbeet::FailKind::Beets,
                    message: "beets exited with code 1".into(),
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::TimedOut {
                    limit: TimeoutKind::Idle(Duration::from_secs(180)),
                    last_line: None,
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::Refused {
                    issue: "No metadata source plugin loaded".into(),
                },
                DiscoveryImport::Failed,
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(classify_discovery(&outcome), expected, "{outcome:?}");
        }
    }

    /// A track waiting on `items[0]`, with the rest as its alternates. Its
    /// transfer id is the one `StubBackend` would hand out, `t-<file>`.
    fn queued_track(
        items: Vec<DownloadableItem>,
        backend: &Arc<dyn DownloadBackend>,
    ) -> QueuedTrack {
        let batch = batch("Kowloon - Wake Up");
        let key = transfer_key(backend.id(), &items[0].source, &items[0].id);
        TransferRegistry::global().claim(std::slice::from_ref(&key), &batch);
        QueuedTrack {
            artist: "Kowloon".to_string(),
            track: "Wake Up".to_string(),
            album: Some("Come Over".to_string()),
            transfer_id: format!("t-{}", items[0].id),
            key,
            batch,
            failing_since: None,
            retry_pending_since: None,
            pool: SourcePool::single(items),
        }
    }

    /// Backdate a track's failure so it has already outlived the confirmation
    /// window, letting a single poll act on it without sleeping through it.
    fn already_confirmed_failing(qt: &mut QueuedTrack) {
        qt.failing_since = Some(tokio::time::Instant::from_std(
            std::time::Instant::now()
                .checked_sub(FAILED_STATE_CONFIRM + std::time::Duration::from_secs(1))
                .expect("monotonic clock younger than the confirmation window"),
        ));
    }

    fn failed(source: &str, path: &str) -> DownloadProgress {
        transfer(
            source,
            path,
            &format!("t-{path}"),
            DownloadState::Failed("Transfer rejected".to_string()),
        )
    }

    fn live(source: &str, path: &str) -> DownloadProgress {
        transfer(source, path, &format!("t-{path}"), DownloadState::Queued)
    }

    fn completed(source: &str, path: &str) -> DownloadProgress {
        transfer(source, path, &format!("t-{path}"), DownloadState::Completed)
    }

    fn pending_of(queued: &[QueuedTrack]) -> HashSet<String> {
        queued.iter().map(|q| q.transfer_id.clone()).collect()
    }

    fn stub() -> Arc<dyn DownloadBackend> {
        StubBackend::new(vec![ScriptedOutcome::Enqueued])
    }

    #[tokio::test]
    async fn a_retry_moves_the_whole_identity_to_the_new_peer() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let dead = queued[0].key.clone();
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);

        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        // Peer, path and transfer id identify the transfer together: leaving
        // any of them on the dead peer would lose sight of the retry.
        assert_eq!(
            queued[0].key,
            transfer_key(backend.id(), "peer_b", "b/07 - Wake Up.flac")
        );
        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
        let other = batch("Y");
        assert!(
            TransferRegistry::global().may_adopt(&dead, &other.owner),
            "the dead peer's key is free again"
        );
        assert!(
            !TransferRegistry::global().may_adopt(&queued[0].key, &other.owner),
            "the retry's key is the track's"
        );
    }

    #[tokio::test]
    async fn a_dead_transfer_listed_first_does_not_shadow_the_live_one() {
        // One peer holding the track in two directories is two ranked items,
        // so an alternate can be the peer that just failed, and both its
        // transfers are listed for that peer.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "b/07 - Wake Up.flac"),
                item("peer_a", "Wake Up", "c/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            live("peer_a", "b/07 - Wake Up.flac"),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
        assert_eq!(
            queued[0].pool.attempts_used(0),
            1,
            "the live transfer must not cost the track a source"
        );
    }

    #[tokio::test]
    async fn a_completed_retry_is_read_past_the_dead_transfer_it_replaced() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "b/07 - Wake Up.flac"),
                item("peer_a", "Wake Up", "c/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            completed("peer_a", "b/07 - Wake Up.flac"),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty(), "the track settled as done");
        assert!(failed_ids.is_empty());
        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
    }

    #[tokio::test]
    async fn settle_poll_ignores_a_same_name_transfer_with_another_id() {
        // slskd keeps records of earlier downloads of the same file from the
        // same peer, say an earlier discovery run's. Neither settles this
        // track: a finished one would send the import looking for a file this
        // transfer never wrote, and a failed one would cost it a peer.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-earlier",
                DownloadState::Completed,
            ),
            transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-older",
                DownloadState::Failed("Transfer rejected".to_string()),
            ),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(pending, pending_of(&queued), "still waiting on its own id");
        assert!(failed_ids.is_empty());
        assert_eq!(queued[0].transfer_id, "t-a/07 - Wake Up.flac");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
    }

    #[tokio::test]
    async fn a_track_that_runs_out_of_peers_is_failed_exactly_once() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);

        // The first peer dies and the track moves to its only alternate.
        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;
        assert!(failed_ids.is_empty());

        // The alternate dies too, and there is nothing left to try. The dead
        // first peer is still in slskd's list and must not be counted as a
        // second loss: stats.downloads_failed adds the failed set's size.
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            failed("peer_b", "b/07 - Wake Up.flac"),
        ];
        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty());
        assert_eq!(failed_ids.len(), 1);
        assert!(failed_ids.contains("t-b/07 - Wake Up.flac"));
    }

    #[tokio::test]
    async fn a_failure_that_clears_on_the_next_poll_never_reaches_another_peer() {
        // slskd 0.26 retries a failed transfer itself, reporting the failure
        // only between its own attempts. Enqueueing a second peer on that
        // sighting would leave two transfers running, and whichever landed
        // second would decide which file the track ends up pointing at.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].key.peer, "peer_a", "the first sighting waits");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
        assert!(queued[0].failing_since.is_some());
        assert_eq!(pending, pending_of(&queued));

        // slskd's own retry re-queued the transfer, so the failure is gone and
        // the clock must go with it.
        settle_poll(
            &mut queued,
            &[live("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].key.peer, "peer_a");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
        assert!(queued[0].failing_since.is_none());
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
    }

    #[tokio::test]
    async fn a_pending_retry_held_past_its_bound_moves_to_the_next_peer() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        queued[0].retry_pending_since = Some(tokio::time::Instant::from_std(
            std::time::Instant::now()
                .checked_sub(RETRY_PENDING_FOR)
                .expect("monotonic clock younger than the retry window"),
        ));
        let row = DownloadProgress {
            pending_retry: Some(Failure::new(
                FailureCode::TransferConnectionFailed,
                Stage::Transfer,
                RetryAdvice::OtherSource,
                "Connection reset by peer",
            )),
            ..live("peer_a", "a/07 - Wake Up.flac")
        };

        settle_poll(&mut queued, &[row], &mut pending, &mut failed_ids, &backend).await;

        assert_eq!(queued[0].pool.attempts_used(0), 2);
    }

    #[tokio::test]
    async fn a_track_its_user_cancels_is_cancelled_and_not_retried() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let registry = TransferRegistry::global();
        let own = queued[0].key.clone();
        assert!(registry.request_cancel(&own, &queued[0].batch.owner.user_id));
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[live("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-a/07 - Wake Up.flac")]
        );
        assert!(pending.is_empty());
        assert!(failed_ids.contains("t-a/07 - Wake Up.flac"));
        assert_eq!(queued[0].pool.attempts_used(0), 1, "no other peer");
        assert!(registry.may_adopt(&own, &batch("Y").owner));
    }

    #[tokio::test]
    async fn a_track_cancelled_after_its_transfer_finished_is_still_imported() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
            &backend,
        )];
        assert!(TransferRegistry::global()
            .request_cancel(&queued[0].key, &queued[0].batch.owner.user_id));
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[completed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty());
        assert!(failed_ids.is_empty(), "it settled as done");
        assert!(stub.cancelled().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_asked_for_during_a_tracks_failover_follows_it() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let registry = TransferRegistry::global();
        let first = queued[0].key.clone();
        let user = queued[0].batch.owner.user_id.clone();
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let (reached, release) = stub.hold_downloads();
        let cancel = async {
            reached.notified().await; // the failover's enqueue is in flight
            assert!(registry.request_cancel(&first, &user));
            release.notify_one();
        };

        let dead = [failed("peer_a", "a/07 - Wake Up.flac")];
        tokio::join!(
            settle_poll(&mut queued, &dead, &mut pending, &mut failed_ids, &backend,),
            cancel
        );
        assert_eq!(queued[0].key.peer, "peer_b");
        settle_poll(
            &mut queued,
            &[live("peer_b", "b/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(stub
            .cancelled()
            .contains(&Cancel::only("peer_b", "t-b/07 - Wake Up.flac")));
        assert!(pending.is_empty());
        assert!(failed_ids.contains("t-b/07 - Wake Up.flac"));
    }

    #[tokio::test]
    async fn a_scan_status_that_cannot_be_read_still_starts_the_scan() {
        // getLastScanTime's `Internal Error` (server/subsonic/library_scanning.go:16-21); the scan
        // is accepted.
        let navi = crate::test_navidrome::NavidromeStub::start();
        navi.answer(
            "/rest/getScanStatus",
            &[r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Internal Error"}}}"#],
        );
        assert!(scan_imports(&navi.client(), 3).await);
        assert_eq!(navi.asked(), vec!["/rest/getScanStatus", "/rest/startScan"]);
    }
}
