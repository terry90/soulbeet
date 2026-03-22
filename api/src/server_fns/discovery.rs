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
    Ok(DiscoveryConfig {
        enabled: settings.discovery_enabled,
        folder_id: settings.discovery_folder_id,
        folder_name,
        track_count: settings.discovery_track_count as u32,
        lifetime_days: settings.discovery_lifetime_days as u32,
        profiles: settings.discovery_profiles,
        playlist_names: serde_json::from_str(&settings.discovery_playlist_name).unwrap_or_default(),
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

    let src = std::path::PathBuf::from(&track.path);
    if !src.exists() {
        return Err(server_error(format!("File not found: {}", track.path)));
    }

    let target = std::path::PathBuf::from(&folder.path);
    import_or_move(&src, &target).await.map_err(server_error)?;

    DiscoveryTrackRow::update_status(&req.track_id, &DiscoveryStatus::Promoted)
        .await
        .map_err(server_error)?;

    if let Err(e) = DiscoveryHistoryRow::update_outcome(&auth.0.sub, &track.artist, &track.title, "promoted").await {
        warn!("Failed to update history for promoted track '{}': {}", track.title, e);
    }

    info!("Promoted: {} -> {}", track.title, folder.path);
    Ok(())
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
            let _ = cleanup_empty_parent(parent).await;
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

#[post("/api/discovery/generate", auth: AuthSession)]
pub async fn generate_discovery_playlist() -> Result<u32, ServerFnError> {
    generate_discovery_playlist_internal(&auth.0.sub)
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
pub async fn generate_discovery_playlist_internal(user_id: &str) -> Result<u32, String> {
    use crate::models::deletion_review::DeletionReviewRow;
    use crate::models::discovery_candidate::DiscoveryCandidateRow;
    use crate::models::discovery_history::DiscoveryHistoryRow;
    use crate::services::download_backend;

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
    let tracks_per_profile =
        ((settings.discovery_track_count as usize) / selected_profiles.len().max(1)).max(1);

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

    for profile in &selected_profiles {
        let profile_name = profile.to_string();

        // Get unused candidates for this specific profile
        let mut candidates = DiscoveryCandidateRow::get_unused(
            user_id,
            &profile_name,
            (tracks_per_profile as f64 * 1.5) as u32,
        )
        .await?;

        if candidates.is_empty() {
            // Generate for this profile only
            info!("No candidates for {}, running engine", profile_name);
            match generate_recommendations_for_user(user_id, *profile).await {
                Ok(count) => info!("Generated {} candidates for {}", count, profile_name),
                Err(e) => {
                    warn!("{} engine failed: {}", profile_name, e);
                    continue;
                }
            }
            candidates = DiscoveryCandidateRow::get_unused(
                user_id,
                &profile_name,
                (tracks_per_profile as f64 * 1.5) as u32,
            )
            .await?;
            if candidates.is_empty() {
                continue;
            }
        }

        // Phase 1: Search and queue downloads (fast, parallel on slskd)
        struct QueuedTrack {
            artist: String,
            track: String,
            album: Option<String>,
            slskd_filename: String,
        }
        let mut queued: Vec<QueuedTrack> = Vec::new();

        for candidate in &candidates {
            if queued.len() >= tracks_per_profile {
                break;
            }

            let key = format!(
                "{}:{}",
                candidate.artist.to_lowercase(),
                candidate.track.to_lowercase()
            );
            if seen.contains(&key) {
                continue;
            }

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
                    continue;
                }
            };

            // Poll until we get results or the search times out (slskd search has 120s timeout).
            // Each poll_search call long-polls for up to 10s internally.
            let mut found_item = None;
            for _ in 0..12 {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let search_result = match backend.poll_search(&search_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Poll failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
                        break;
                    }
                };

                if !search_result.groups.is_empty() && !search_result.groups[0].items.is_empty() {
                    found_item = Some(search_result.groups[0].items[0].clone());
                    break;
                }

                // Search completed with no results
                if !search_result.has_more {
                    break;
                }
            }

            let item = match found_item {
                Some(item) => item,
                None => {
                    info!("No results for '{}' - {}, skipping", candidate.artist, candidate.track);
                    continue;
                }
            };
            let download_results = match backend.download(vec![item]).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Download failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
                    continue;
                }
            };

            for dl in &download_results {
                if dl.error.is_none() {
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
                    break;
                }
            }
        }

        if queued.is_empty() {
            continue;
        }

        // Phase 2: Wait for downloads to complete, then move to Discovery/
        info!(
            "{}: {} downloads queued, waiting for completion...",
            profile_name,
            queued.len()
        );
        let download_base = crate::config::CONFIG.download_path().clone();
        let mut profile_downloads = 0u32;

        // Poll slskd until all queued downloads are complete (or timeout after 10 min)
        let wait_start = tokio::time::Instant::now();
        let max_wait = tokio::time::Duration::from_secs(600);
        let mut remaining_filenames: std::collections::HashSet<String> =
            queued.iter().map(|q| q.slskd_filename.clone()).collect();

        while !remaining_filenames.is_empty() && wait_start.elapsed() < max_wait {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

            let downloads = match backend.get_downloads().await {
                Ok(d) => d,
                Err(_) => continue,
            };

            let mut newly_done = Vec::new();
            for fname in remaining_filenames.iter() {
                let matched = downloads.iter().find(|d| {
                    crate::server_fns::download::monitor::filenames_match(&d.item, fname)
                });
                if let Some(dl) = matched {
                    let done = matches!(
                        dl.state,
                        shared::download::DownloadState::Completed
                            | shared::download::DownloadState::Failed(_)
                            | shared::download::DownloadState::Cancelled
                    );
                    if done {
                        newly_done.push(fname.clone());
                    }
                }
            }
            for fname in &newly_done {
                remaining_filenames.remove(fname);
            }
        }

        if !remaining_filenames.is_empty() {
            warn!(
                "{}: {} downloads didn't complete within timeout",
                profile_name,
                remaining_filenames.len()
            );
        }

        // Phase 3: Import completed files into Discovery/{profile}/ via beets and record in DB
        let importer = crate::services::music_importer(None).await;
        let profile_path = folder.discovery_profile_path(&profile_name);
        let discovery_target = std::path::PathBuf::from(&profile_path);

        for qt in &queued {
            if remaining_filenames.contains(&qt.slskd_filename) {
                warn!(
                    "Skipping '{}' - {} (download timed out)",
                    qt.artist, qt.track
                );
                continue;
            }

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
                    continue;
                }
            };

            // Snapshot files before import so we can detect what beets added
            let before = collect_files(&discovery_target);

            let src = std::path::Path::new(&src_path);
            if let Ok(ref imp) = importer {
                match imp.import(&[src], &discovery_target, false).await {
                    Ok(soulbeet::ImportResult::Success) => {
                        info!("Imported '{}' - {} into Discovery/{}", qt.artist, qt.track, profile_name);
                    }
                    Ok(soulbeet::ImportResult::Skipped) => {
                        warn!("Beets skipped '{}' - {} (duplicate?)", qt.artist, qt.track);
                        let _ = tokio::fs::remove_file(src).await;
                        continue;
                    }
                    Ok(other) => {
                        warn!("Beets import issue for '{}' - {}: {:?}", qt.artist, qt.track, other);
                        let _ = tokio::fs::remove_file(src).await;
                        continue;
                    }
                    Err(e) => {
                        warn!("Beets import failed for '{}' - {}: {}", qt.artist, qt.track, e);
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

            // Find the imported file by diffing before/after snapshots
            let after = collect_files(&discovery_target);
            let track_path = match find_new_file(&before, &after) {
                Some(p) => p,
                None => {
                    warn!(
                        "Could not find imported file for '{}' - {} in {}",
                        qt.artist, qt.track, profile_path
                    );
                    continue;
                }
            };

            DiscoveryTrackRow::create(
                None,
                &qt.track,
                &qt.artist,
                qt.album.as_deref().unwrap_or(""),
                &track_path,
                folder_id,
                &profile_name,
            )
            .await?;
            profile_downloads += 1;
        }

        info!(
            "{}: {} tracks imported to Discovery/",
            profile_name, profile_downloads
        );

        total_downloads += profile_downloads;
    }

    UserSettings::update_discovery_last_generated(user_id).await?;

    // Trigger a Navidrome library scan so the imported files get indexed.
    // Playlists are created later by reconcile_discovery_playlists() once
    // Navidrome has scanned the new files.
    if total_downloads > 0 {
        if let Ok(navi) = crate::services::navidrome_client_for_user(user_id).await {
            if let Err(e) = navi.start_scan().await {
                warn!("Failed to trigger Navidrome scan: {}", e);
            } else {
                info!("Triggered Navidrome library scan after {} discovery imports", total_downloads);
            }
        }
    }

    Ok(total_downloads)
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
    let folder = Folder::get_by_id(&folder_id)
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

        // Check if we already have a playlist for this profile
        let existing_id = UserSettings::get_playlist_id_for_profile(
            &settings.discovery_navidrome_playlist_id,
            &profile_name,
        );
        if existing_id.is_some() {
            continue; // Smart playlist already exists, it auto-updates
        }

        let playlist_name = UserSettings::get_playlist_name_for_profile(
            &settings.discovery_playlist_name,
            &profile_name,
        );

        // The filepath rule must use the path as Navidrome sees it.
        // If NAVIDROME_MUSIC_PATH is set, we need to reverse the prefix mapping:
        // local /music/terry/Discovery/Balanced -> Navidrome /media/music/terry/Discovery/Balanced
        let local_profile_path = folder.discovery_profile_path(&profile_name);
        let profile_path = to_navidrome_path(&local_profile_path, &folder.path);
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

/// Collect all music file paths under a directory (recursive).
/// Used to snapshot before/after beets import to find what was added.
#[cfg(feature = "server")]
fn collect_files(dir: &std::path::Path) -> std::collections::HashSet<std::path::PathBuf> {
    let mut files = std::collections::HashSet::new();
    fn walk(dir: &std::path::Path, files: &mut std::collections::HashSet<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext != "nsp" && ext != "db" {
                    files.insert(path);
                }
            }
        }
    }
    walk(dir, &mut files);
    files
}

/// Find the file that was added after a beets import by comparing before/after snapshots.
#[cfg(feature = "server")]
fn find_new_file(
    before: &std::collections::HashSet<std::path::PathBuf>,
    after: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<String> {
    let new_files: Vec<_> = after.difference(before).collect();
    new_files
        .first()
        .map(|p| p.to_string_lossy().to_string())
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

/// Remove a directory if it's empty, then try its parent too.
/// Stops at profile directories (Conservative/Balanced/Adventurous) and folder roots.
#[cfg(feature = "server")]
async fn cleanup_empty_parent(dir: &std::path::Path) -> Result<(), std::io::Error> {
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(dir_name, "Discovery" | "Conservative" | "Balanced" | "Adventurous")
        || dir.join(".beets_library.db").exists()
    {
        return Ok(());
    }

    let mut read_dir = tokio::fs::read_dir(dir).await?;
    if read_dir.next_entry().await?.is_none() {
        tokio::fs::remove_dir(dir).await?;
        if let Some(parent) = dir.parent() {
            let _ = Box::pin(cleanup_empty_parent(parent)).await;
        }
    }
    Ok(())
}

/// Convert a local filesystem path to the path as Navidrome sees it.
/// Uses the folder's parent as the local mount point and substitutes
/// with NAVIDROME_MUSIC_PATH.
///
/// e.g. folder_path = /music/terry, local_path = /music/terry/Discovery/Balanced
///      NAVIDROME_MUSIC_PATH = /media/music
///      local mount = /music (folder parent)
///      result = /media/music/terry/Discovery/Balanced
#[cfg(feature = "server")]
fn to_navidrome_path(local_path: &str, folder_path: &str) -> String {
    let navidrome_prefix = std::env::var("NAVIDROME_MUSIC_PATH").ok().filter(|s| !s.is_empty());
    let Some(nd_prefix) = navidrome_prefix else {
        return local_path.to_string();
    };

    // The folder parent is the local mount point (e.g. /music from /music/terry)
    let local_mount = std::path::Path::new(folder_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    if local_mount.is_empty() {
        return local_path.to_string();
    }

    let nd_trimmed = nd_prefix.trim_end_matches('/');

    match local_path.strip_prefix(&local_mount) {
        Some(relative) => {
            let relative = relative.trim_start_matches('/');
            format!("{}/{}", nd_trimmed, relative)
        }
        None => local_path.to_string(),
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
