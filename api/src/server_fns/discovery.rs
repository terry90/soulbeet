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
        navidrome_playlist_ids: settings
            .discovery_navidrome_playlist_id
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default(),
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

    let filename = src
        .file_name()
        .ok_or_else(|| server_error("Invalid filename"))?
        .to_string_lossy()
        .to_string();
    let dest = std::path::PathBuf::from(&folder.path).join(&filename);

    if let Err(e) = tokio::fs::rename(&src, &dest).await {
        // Fall back to copy+delete for cross-filesystem moves (EXDEV)
        if e.raw_os_error() == Some(18) {
            tokio::fs::copy(&src, &dest)
                .await
                .map_err(|e| server_error(format!("Failed to copy file: {}", e)))?;
            tokio::fs::remove_file(&src)
                .await
                .map_err(|e| server_error(format!("Failed to remove source after copy: {}", e)))?;
        } else {
            return Err(server_error(format!("Failed to move file: {}", e)));
        }
    }

    DiscoveryTrackRow::update_status(&req.track_id, &DiscoveryStatus::Promoted)
        .await
        .map_err(server_error)?;

    if let Err(e) = DiscoveryHistoryRow::update_outcome(&auth.0.sub, &track.artist, &track.title, "promoted").await {
        warn!("Failed to update history for promoted track '{}': {}", track.title, e);
    }

    info!("Promoted: {} -> {}", track.title, dest.display());
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
    let discovery_path = folder.discovery_path();
    tokio::fs::create_dir_all(&discovery_path)
        .await
        .map_err(|e| format!("Failed to create Discovery dir: {}", e))?;

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

        let mut profile_downloads = 0u32;

        for candidate in &candidates {
            if profile_downloads >= tracks_per_profile as u32 {
                break;
            }

            // Skip tracks already suggested in a past batch, another profile, or deleted
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

            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let search_result = match backend.poll_search(&search_id).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Poll failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
                    continue;
                }
            };

            if search_result.groups.is_empty() || search_result.groups[0].items.is_empty() {
                info!("No results for '{}' - {}, skipping", candidate.artist, candidate.track);
                continue;
            }
            let group = &search_result.groups[0];

            let item = &group.items[0];
            let download_results = match backend.download(vec![item.clone()]).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Download failed for '{}' - {}: {}", candidate.artist, candidate.track, e);
                    continue;
                }
            };

            for dl in &download_results {
                if dl.error.is_none() {
                    DiscoveryTrackRow::create(
                        None,
                        &candidate.track,
                        &candidate.artist,
                        candidate.album.as_deref().unwrap_or(""),
                        &format!(
                        "{}/{}",
                        discovery_path,
                        std::path::Path::new(&dl.item)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                        folder_id,
                        &profile_name,
                    )
                    .await?;
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await?;
                    seen.insert(key.clone());
                    DiscoveryHistoryRow::record(
                        user_id,
                        &candidate.artist,
                        &candidate.track,
                        &profile_name,
                    )
                    .await?;
                    profile_downloads += 1;
                }
            }
        }

        total_downloads += profile_downloads;
    }

    UserSettings::update_discovery_last_generated(user_id).await?;

    // Playlist creation is deferred: files are still downloading at this point.
    // reconcile_discovery_playlists() runs during automation to match tracks
    // with Navidrome once they're indexed.

    Ok(total_downloads)
}

/// Match pending discovery tracks to Navidrome songs and create/update playlists.
///
/// Downloads are async (slskd), so at generation time the files aren't indexed
/// by Navidrome yet. This function runs later (during automation) to:
/// 1. Match discovery tracks to Navidrome songs by path
/// 2. Update song_ids in the database
/// 3. Create or refresh Navidrome playlists per profile
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

    let navi = match crate::services::navidrome_client_for_user(user_id).await {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    // Get all Navidrome songs to match against discovery tracks
    let songs = navi
        .get_all_songs_with_ratings()
        .await
        .map_err(|e| e.to_string())?;

    // Get pending discovery tracks missing a song_id
    let pending = DiscoveryTrackRow::get_pending_by_folder(&folder_id).await?;
    let unlinked: Vec<_> = pending.iter().filter(|t| t.song_id.is_none()).collect();
    if unlinked.is_empty() {
        // All tracks already linked, just ensure playlists exist
        return ensure_playlists(user_id, &settings, &folder_id, &navi).await;
    }

    let mut matched = 0u32;
    for track in &unlinked {
        let track_path = std::path::Path::new(&track.path);
        let track_suffix = path_suffix(track_path);

        let mut found = false;
        for song in &songs {
            if let Some(ref song_path_str) = song.path {
                let song_path = std::path::Path::new(song_path_str);
                // Match by path suffix (parent dir + filename) for accuracy,
                // fall back to filename-only if suffix doesn't match
                let song_suffix = path_suffix(song_path);
                let is_match = track_suffix == song_suffix
                    || track_path.file_name().map(|f| f.to_ascii_lowercase())
                        == song_path.file_name().map(|f| f.to_ascii_lowercase());
                if is_match {
                    DiscoveryTrackRow::update_song_id(&track.id, &song.id).await?;
                    matched += 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            info!(
                "Reconciliation: no Navidrome match for '{}' - {} ({})",
                track.artist, track.title, track.path
            );
        }
    }

    info!(
        "Discovery playlist reconciliation: linked {} of {} unlinked tracks",
        matched,
        unlinked.len()
    );

    ensure_playlists(user_id, &settings, &folder_id, &navi).await
}

#[cfg(feature = "server")]
async fn ensure_playlists(
    user_id: &str,
    settings: &UserSettings,
    folder_id: &str,
    navi: &soulbeet::NavidromeClient,
) -> Result<(), String> {
    let selected_profiles = parse_profiles(&settings.discovery_profiles);

    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let tracks =
            DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name).await?;
        let song_ids: Vec<String> = tracks.iter().filter_map(|t| t.song_id.clone()).collect();

        if song_ids.is_empty() {
            continue;
        }

        let playlist_name = UserSettings::get_playlist_name_for_profile(
            &settings.discovery_playlist_name,
            &profile_name,
        );

        let old_id = UserSettings::get_playlist_id_for_profile(
            &settings.discovery_navidrome_playlist_id,
            &profile_name,
        );

        // Create new playlist first, then delete old one only on success.
        // This avoids losing the old playlist if creation fails.
        match navi.create_playlist(&playlist_name, &song_ids).await {
            Ok(pl) => {
                if let Err(e) = UserSettings::update_discovery_playlist_id(user_id, &profile_name, &pl.id).await {
                    warn!("Failed to save playlist ID for '{}': {}", profile_name, e);
                }
                // Now safe to delete the old one
                if let Some(ref old) = old_id {
                    if let Err(e) = navi.delete_playlist(old).await {
                        warn!("Failed to delete old playlist '{}': {}", old, e);
                    }
                }
                info!(
                    "Created Navidrome playlist '{}' with {} tracks",
                    playlist_name,
                    song_ids.len()
                );
            }
            Err(e) => {
                warn!(
                    "Failed to create Navidrome playlist '{}': {}",
                    playlist_name, e
                );
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

/// Extract the last 2 path components (parent/filename) as a lowercase string
/// for fuzzy path matching between Navidrome and local paths.
#[cfg(feature = "server")]
fn path_suffix(p: &std::path::Path) -> String {
    let components: Vec<_> = p.components().rev().take(2).collect();
    components
        .into_iter()
        .rev()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>()
        .join("/")
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
