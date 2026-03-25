use dioxus::prelude::*;
use shared::navidrome::{DiscoveryConfig, DiscoveryTrack};

#[cfg(feature = "server")]
use shared::navidrome::DiscoveryStatus;

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
async fn update_progress(user_id: &str, profile: &str, phase: shared::navidrome::ProfilePhase, current: u32, total: u32) {
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
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::server_error;

/// Get the current user's discovery configuration
#[get("/api/discovery/config", auth: AuthSession)]
pub async fn get_discovery_config() -> Result<DiscoveryConfig, ServerFnError> {
    let settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
    let folder_name = if let Some(ref fid) = settings.discovery_folder_id {
        Folder::get_by_id(fid).await.ok().flatten().map(|f| f.name)
    } else {
        None
    };
    let track_counts = settings.parse_track_counts();
    let lifetime_days = settings.parse_lifetime_days();
    let playlist_names = serde_json::from_str(&settings.discovery_playlist_name).unwrap_or_default();
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
    let settings = UserSettings::get(&auth.0.sub).await.map_err(server_error)?;
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
    let track = DiscoveryTrackRow::get_by_id(&req.track_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("Track not found"))?;

    let folder = Folder::get_by_id(&track.folder_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("Folder not found"))?;

    if folder.user_id != auth.0.sub {
        return Err(server_error("Not authorized to modify this track"));
    }

    super::navidrome::promote_discovery_track_internal(&req.track_id, &auth.0.sub)
        .await
        .map_err(server_error)
}

#[post("/api/discovery/remove", auth: AuthSession)]
pub async fn remove_discovery_track(req: TrackActionRequest) -> Result<(), ServerFnError> {
    #[cfg(feature = "server")]
    use crate::models::discovery_history::DiscoveryHistoryRow;

    let track = DiscoveryTrackRow::get_by_id(&req.track_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("Track not found"))?;

    let folder = Folder::get_by_id(&track.folder_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error("Folder not found"))?;

    if folder.user_id != auth.0.sub {
        return Err(server_error("Not authorized to modify this track"));
    }

    let path = std::path::Path::new(&track.path);
    if path.exists() {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| server_error(format!("Failed to delete: {}", e)))?;
        // Clean up empty Artist/Album/ dirs left by beets
        if let Some(parent) = path.parent() {
            let _ = super::cleanup_empty_ancestors(parent).await;
        }
    }

    DiscoveryTrackRow::update_status(&req.track_id, &DiscoveryStatus::Removed)
        .await
        .map_err(server_error)?;

    if let Err(e) = DiscoveryHistoryRow::update_outcome(&auth.0.sub, &track.artist, &track.title, "removed").await {
        warn!("Failed to update history for removed track '{}': {}", track.title, e);
    }

    info!("Removed discovery track: {}", track.title);
    Ok(())
}

#[post("/api/discovery/start-generation", auth: AuthSession)]
pub async fn start_discovery_generation() -> Result<(), ServerFnError> {
    use shared::navidrome::{DiscoveryProgress, GenerationStatus, ProfileProgress, ProfilePhase};

    let user_id = auth.0.sub.clone();

    // Check if already running
    {
        let progress = crate::globals::DISCOVERY_PROGRESS.read().await;
        if let Some(p) = progress.get(&user_id) {
            if !p.is_terminal() {
                return Err(server_error("Discovery generation already in progress"));
            }
        }
    }

    // Read settings to get profile list for initial progress state
    let settings = UserSettings::get(&user_id).await.map_err(server_error)?;
    if !settings.discovery_enabled {
        return Err(server_error("Discovery is not enabled"));
    }
    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();
    let folder_id = settings.discovery_folder_id.as_ref()
        .ok_or_else(|| server_error("No download folder configured for discovery"))?;

    // Determine which profiles need tracks (skip those already at target)
    let mut profile_entries = Vec::new();
    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let target = track_counts.get(&profile_name).copied().unwrap_or(10) as usize;
        let existing = DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name)
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

    // Write initial progress BEFORE spawn (prevents race with first poll)
    {
        let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
        map.insert(user_id.clone(), DiscoveryProgress {
            status: GenerationStatus::Running,
            profiles: profile_entries,
            started_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        });
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
                    p.error = Some(e.to_string());
                }
            }
        }
    });

    Ok(())
}

#[get("/api/discovery/progress", auth: AuthSession)]
pub async fn get_discovery_progress() -> Result<Option<shared::navidrome::DiscoveryProgress>, ServerFnError> {
    let progress = crate::globals::DISCOVERY_PROGRESS.read().await;
    match progress.get(&auth.0.sub) {
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

#[cfg(feature = "server")]
pub async fn generate_discovery_playlist_internal(user_id: &str) -> Result<shared::navidrome::GenerationResult, String> {
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
        return Err("Discovery generation already in progress".to_string());
    };

    let settings = UserSettings::get(user_id).await?;
    if !settings.discovery_enabled {
        return Err("Discovery is not enabled".to_string());
    }
    let folder_id = settings
        .discovery_folder_id
        .as_ref()
        .ok_or("No download folder configured for discovery")?;
    let folder = Folder::get_by_id(folder_id)
        .await?
        .ok_or("Discovery folder not found")?;

    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();

    let backend = download_backend(None).await?;

    // Create per-profile subdirectories under Discovery/
    for profile in &selected_profiles {
        let profile_dir = folder.discovery_profile_path(&profile.to_string());
        tokio::fs::create_dir_all(&profile_dir)
            .await
            .map_err(|e| format!("Failed to create Discovery/{} dir: {}", profile, e))?;
    }

    // Build the "already seen" set for deduplication
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Past discovery suggestions
    let history_keys = DiscoveryHistoryRow::get_suggested_keys(user_id).await?;
    info!(
        "Loaded {} tracks from discovery history",
        history_keys.len()
    );
    seen.extend(history_keys);

    // Past deletions (user doesn't want these back)
    let deletions = DeletionReviewRow::get_history(user_id, 1000).await?;
    let deletion_count = deletions.len();
    for d in &deletions {
        seen.insert(format!(
            "{}:{}",
            d.artist.to_lowercase(),
            d.title.to_lowercase()
        ));
    }
    info!("Added {} deleted tracks to exclusion set", deletion_count);

    let mut total_downloads = 0u32;
    let mut all_profile_stats: Vec<ProfileGenerationStats> = Vec::new();

    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let target = track_counts.get(&profile_name).copied().unwrap_or(10) as usize;

        // Account for tracks already in the playlist so regeneration fills gaps
        let existing = DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name)
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        let tracks_per_profile = target.saturating_sub(existing);
        if tracks_per_profile == 0 {
            update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::Skipped, 0, 0).await;
            info!("{}: already at target ({} pending tracks)", profile_name, existing);
            continue;
        }
        update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::PullingCandidates, 0, 0).await;
        info!("{}: need {} more tracks ({} existing, {} target)", profile_name, tracks_per_profile, existing, target);

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
            let mut candidates = DiscoveryCandidateRow::get_unused(
                user_id,
                &profile_name,
                fetch_count,
            )
            .await?;

            if candidates.is_empty() {
                if _attempt == 0 {
                    update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::GeneratingRecommendations, 0, 0).await;
                    info!("No candidates for {}, running engine", profile_name);
                    match generate_recommendations_for_user(user_id, *profile).await {
                        Ok(count) => info!("Generated {} candidates for {}", count, profile_name),
                        Err(e) => {
                            warn!("{} engine failed: {}", profile_name, e);
                            break;
                        }
                    }
                    candidates = DiscoveryCandidateRow::get_unused(
                        user_id,
                        &profile_name,
                        fetch_count,
                    )
                    .await?;
                }
                if candidates.is_empty() {
                    info!("{}: no more candidates available", profile_name);
                    break;
                }
            }

            // Phase 1: Search and queue downloads
            struct QueuedTrack {
                artist: String,
                track: String,
                album: Option<String>,
                slskd_filename: String,
            }
            let mut queued: Vec<QueuedTrack> = Vec::new();

            // Over-queue by 50% to absorb import failures without another retry round
            let queue_target = remaining + (remaining + 1) / 2;

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
                update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::SearchingSoulseek, stats.candidates_tried, candidates.len() as u32).await;

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
                        warn!("Search failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
                        stats.search_errors += 1;
                        seen.insert(key.clone());
                        DiscoveryCandidateRow::mark_used(user_id, &profile_name, &candidate.artist, &candidate.track).await?;
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
                            warn!("Poll failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
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
                    info!("No results for '{}' - {}, skipping", candidate.artist, candidate.track);
                    stats.search_misses += 1;
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(user_id, &profile_name, &candidate.artist, &candidate.track).await?;
                    continue;
                }
                stats.search_hits += 1;

                // Try up to 3 sources, falling back on download failure
                ranked_items.truncate(3);
                let mut downloaded = false;
                for (attempt_idx, item) in ranked_items.iter().enumerate() {
                    let download_results = match backend.download(vec![item.clone()]).await {
                        Ok(r) => r,
                        Err(e) => {
                            if attempt_idx < 2 {
                                info!("Download source {} failed for '{}' - {}, trying next: {}", attempt_idx + 1, candidate.artist, candidate.track, e);
                            } else {
                                warn!("Download failed for '{}' - {} (all sources exhausted): {}", candidate.artist, candidate.track, e);
                            }
                            stats.downloads_failed += 1;
                            continue;
                        }
                    };

                    if let Some(dl) = download_results.iter().find(|dl| dl.error.is_none()) {
                        stats.downloads_queued += 1;
                        seen.insert(key.clone());
                        queued.push(QueuedTrack {
                            artist: candidate.artist.clone(),
                            track: candidate.track.clone(),
                            album: candidate.album.clone(),
                            slskd_filename: dl.item.clone(),
                        });
                        DiscoveryCandidateRow::mark_used(
                            user_id,
                            &profile_name,
                            &candidate.artist,
                            &candidate.track,
                        )
                        .await?;
                        DiscoveryHistoryRow::record(
                            user_id,
                            &candidate.artist,
                            &candidate.track,
                            &profile_name,
                        )
                        .await?;
                        downloaded = true;
                        break;
                    }
                    // This source returned an error in the response
                    stats.downloads_failed += 1;
                    if attempt_idx < 2 {
                        info!("Download source {} errored for '{}' - {}, trying next", attempt_idx + 1, candidate.artist, candidate.track);
                    }
                }
                if !downloaded {
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(user_id, &profile_name, &candidate.artist, &candidate.track).await?;
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
            let mut pending_filenames: std::collections::HashSet<String> =
                queued.iter().map(|q| q.slskd_filename.clone()).collect();
            let mut failed_filenames: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            while !pending_filenames.is_empty() && wait_start.elapsed() < max_wait {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let completed = (queued.len() as u32).saturating_sub(pending_filenames.len() as u32 + failed_filenames.len() as u32);
                update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::Downloading, completed, queued.len() as u32).await;

                let downloads = match backend.get_downloads().await {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                let mut newly_done = Vec::new();
                for fname in pending_filenames.iter() {
                    let matched = downloads.iter().find(|d| {
                        crate::server_fns::download::monitor::filenames_match(&d.item, fname)
                    });
                    if let Some(dl) = matched {
                        match &dl.state {
                            shared::download::DownloadState::Completed
                            | shared::download::DownloadState::Imported
                            | shared::download::DownloadState::ImportSkipped => {
                                newly_done.push((fname.clone(), true));
                            }
                            shared::download::DownloadState::Failed(_)
                            | shared::download::DownloadState::Cancelled => {
                                newly_done.push((fname.clone(), false));
                            }
                            _ => {}
                        }
                    }
                }
                for (fname, succeeded) in &newly_done {
                    pending_filenames.remove(fname);
                    if !succeeded {
                        failed_filenames.insert(fname.clone());
                    }
                }
            }

            stats.downloads_timed_out += pending_filenames.len() as u32;
            stats.downloads_failed += failed_filenames.len() as u32;
            stats.downloads_completed += (queued.len() - pending_filenames.len() - failed_filenames.len()) as u32;
            if !pending_filenames.is_empty() {
                warn!(
                    "{}: {} downloads didn't complete within timeout",
                    profile_name,
                    pending_filenames.len()
                );
            }
            if !failed_filenames.is_empty() {
                warn!(
                    "{}: {} downloads failed in slskd",
                    profile_name,
                    failed_filenames.len()
                );
            }

            // Phase 3: Import completed files into Discovery/{profile}/ via beets and record in DB
            let importer = crate::services::music_importer(None).await;
            let profile_path = folder.discovery_profile_path(&profile_name);
            let discovery_target = std::path::PathBuf::from(&profile_path);

            // Skip both timed-out and slskd-failed downloads
            let skip_filenames: std::collections::HashSet<&String> =
                pending_filenames.iter().chain(failed_filenames.iter()).collect();

            let importable_count = queued.iter().filter(|q| !skip_filenames.contains(&q.slskd_filename)).count() as u32;
            let mut import_idx = 0u32;
            for qt in &queued {
                if skip_filenames.contains(&qt.slskd_filename) {
                    continue;
                }
                import_idx += 1;
                update_progress(user_id, &profile_name, shared::navidrome::ProfilePhase::Importing, import_idx, importable_count).await;

                let resolved = crate::server_fns::download::utils::resolve_download_path(
                    &qt.slskd_filename,
                    &download_base,
                );
                let src_path = match resolved {
                    Some(p) => p,
                    None => {
                        warn!(
                            "Could not find downloaded file for '{}' - {} (slskd: {})",
                            qt.artist, qt.track, qt.slskd_filename
                        );
                        stats.imports_file_missing += 1;
                        continue;
                    }
                };

                // Snapshot files before import so we can detect what beets added
                let before = collect_files(&discovery_target).await;

                let src = std::path::Path::new(&src_path);
                if let Ok(ref imp) = importer {
                    match imp.import(&[src], &discovery_target, false).await {
                        Ok(soulbeet::ImportResult::Success) => {
                            info!("Imported '{}' - {} into Discovery/{}", qt.artist, qt.track, profile_name);
                        }
                        Ok(soulbeet::ImportResult::Skipped) => {
                            warn!("Beets skipped '{}' - {} (duplicate?)", qt.artist, qt.track);
                            stats.imports_skipped += 1;
                            let _ = tokio::fs::remove_file(src).await;
                            continue;
                        }
                        Ok(other) => {
                            warn!("Beets import issue for '{}' - {}: {:?}", qt.artist, qt.track, other);
                            stats.imports_failed += 1;
                            let _ = tokio::fs::remove_file(src).await;
                            continue;
                        }
                        Err(e) => {
                            warn!("Beets import failed for '{}' - {}: {}", qt.artist, qt.track, e);
                            stats.imports_failed += 1;
                            let _ = tokio::fs::remove_file(src).await;
                            continue;
                        }
                    }
                } else {
                    // No importer available - fall back to raw move
                    let filename = src
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if filename.is_empty() {
                        continue;
                    }
                    let dest = format!("{}/{}", profile_path, filename);
                    if let Err(e) = tokio::fs::rename(&src_path, &dest).await {
                        if e.raw_os_error() == Some(18) {
                            if let Err(e) = tokio::fs::copy(&src_path, &dest).await {
                                warn!("Failed to copy to Discovery/: {}", e);
                                continue;
                            }
                            let _ = tokio::fs::remove_file(&src_path).await;
                        } else {
                            warn!("Failed to move to Discovery/: {}", e);
                            continue;
                        }
                    }
                }

                // Find imported files by diffing before/after snapshots
                let after = collect_files(&discovery_target).await;
                let new_files = find_new_files(&before, &after);
                if new_files.is_empty() {
                    warn!(
                        "Could not find imported file for '{}' - {} in {}",
                        qt.artist, qt.track, profile_path
                    );
                    stats.imports_file_missing += 1;
                    continue;
                }
                for track_path in &new_files {
                    DiscoveryTrackRow::create(
                        None,
                        &qt.track,
                        &qt.artist,
                        qt.album.as_deref().unwrap_or(""),
                        track_path,
                        folder_id,
                        &profile_name,
                    )
                    .await?;
                }
                stats.imports_succeeded += 1;
                profile_downloads += 1;
            }

            if (profile_downloads as usize) >= tracks_per_profile {
                break;
            }
            info!(
                "{}: {}/{} tracks after attempt {}, retrying with more candidates...",
                profile_name, profile_downloads, tracks_per_profile, _attempt + 1
            );
        } // end retry loop

        info!(
            "{}: {} tracks imported to Discovery/",
            profile_name, profile_downloads
        );

        total_downloads += profile_downloads;
        all_profile_stats.push(stats);
    }

    UserSettings::update_discovery_last_generated(user_id).await?;

    // Trigger a Navidrome scan then wait for it to finish so playlists can be created.
    if total_downloads > 0 {
        if let Ok(navi) = crate::services::navidrome_client_for_user(user_id).await {
            if let Err(e) = navi.start_scan().await {
                warn!("Failed to trigger Navidrome scan: {}", e);
            } else {
                info!("Triggered Navidrome library scan after {} discovery imports", total_downloads);
                // Wait for scan to complete (poll every 3s, max 2 min)
                for _ in 0..40u32 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    match navi.get_scan_status().await {
                        Ok(false) => break,
                        Ok(true) => continue,
                        Err(_) => break,
                    }
                }
                // Create/update smart playlists now that tracks are indexed
                if let Err(e) = reconcile_discovery_playlists(user_id).await {
                    warn!("Playlist reconciliation after generation failed: {}", e);
                }
            }
        }
    }

    Ok(GenerationResult {
        total_imported: total_downloads,
        profiles: all_profile_stats,
    })
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
            info!("Playlist reconciliation skipped (no Navidrome client): {}", e);
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
                if let Err(e) = UserSettings::update_discovery_playlist_id(
                    user_id,
                    &profile_name,
                    &playlist_id,
                )
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
    generate_recommendations_internal(&auth.0.sub)
        .await
        .map_err(server_error)
}

/// Collect all file paths under a directory (recursive, async).
/// Used to snapshot before/after beets import to find what was added.
#[cfg(feature = "server")]
async fn collect_files(dir: &std::path::Path) -> std::collections::HashSet<std::path::PathBuf> {
    let mut files = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext != "nsp" && ext != "db" {
                    files.insert(path);
                }
            }
        }
    }
    files
}

/// Find all files added after a beets import by comparing before/after snapshots.
/// Returns only audio files when possible, falls back to any new file.
#[cfg(feature = "server")]
fn find_new_files(
    before: &std::collections::HashSet<std::path::PathBuf>,
    after: &std::collections::HashSet<std::path::PathBuf>,
) -> Vec<String> {
    let audio_ext = ["flac", "mp3", "m4a", "ogg", "aac", "wav", "wma", "opus"];
    let new_files: Vec<_> = after.difference(before).collect();
    let audio_files: Vec<String> = new_files
        .iter()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| audio_ext.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false)
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    if audio_files.is_empty() {
        new_files.iter().map(|p| p.to_string_lossy().to_string()).collect()
    } else {
        audio_files
    }
}

/// Import a file into a target directory via beets, with raw-move fallback.
#[cfg(feature = "server")]
pub async fn import_or_move(src: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    match crate::services::music_importer(None).await {
        Ok(imp) => match imp.import(&[src], target, false).await {
            Ok(soulbeet::ImportResult::Success) => Ok(()),
            Ok(soulbeet::ImportResult::Skipped) => {
                Err("Beets skipped track (duplicate?)".to_string())
            }
            Ok(other) => Err(format!("Import issue: {:?}", other)),
            Err(e) => Err(format!("Import failed: {}", e)),
        },
        Err(_) => {
            let filename = src
                .file_name()
                .ok_or("Invalid filename")?
                .to_string_lossy()
                .to_string();
            let dest = target.join(&filename);
            if let Err(e) = tokio::fs::rename(src, &dest).await {
                if e.raw_os_error() == Some(18) {
                    tokio::fs::copy(src, &dest)
                        .await
                        .map_err(|e| format!("Failed to copy: {}", e))?;
                    let _ = tokio::fs::remove_file(src).await;
                } else {
                    return Err(format!("Failed to move: {}", e));
                }
            }
            Ok(())
        }
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
        let rows = EngineReportRow::get_history(&auth.0.sub, 1)
            .await
            .map_err(server_error)?;
        if let Some(row) = rows.into_iter().next() {
            let report: shared::recommendation::EngineReport =
                serde_json::from_str(&row.report_json).map_err(|e| server_error(e.to_string()))?;
            return Ok(report.to_text());
        }
        // Legacy fallback
        use crate::models::user_profile::UserProfileRow;
        let report_json = UserProfileRow::get_report(&auth.0.sub)
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
        let rows = EngineReportRow::get_history(&auth.0.sub, 30)
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
