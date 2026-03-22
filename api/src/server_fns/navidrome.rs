use dioxus::prelude::*;
use shared::navidrome::{DeletionReview, LibraryStats, SyncResult};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use crate::models::deletion_review::DeletionReviewRow;
#[cfg(feature = "server")]
use crate::models::discovery_history::DiscoveryHistoryRow;
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::services::navidrome_client_for_user;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use shared::navidrome::DiscoveryStatus;

#[cfg(feature = "server")]
use super::server_error;

#[post("/api/navidrome/sync-ratings", auth: AuthSession)]
pub async fn sync_ratings() -> Result<SyncResult, ServerFnError> {
    sync_ratings_internal(&auth.0.sub)
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
pub async fn sync_ratings_internal(user_id: &str) -> Result<SyncResult, String> {
    let client = navidrome_client_for_user(user_id).await?;
    let songs = client
        .get_all_songs_with_ratings()
        .await
        .map_err(|e| e.to_string())?;

    let total_songs_scanned = songs.len() as u32;

    let user_settings = UserSettings::get(user_id).await?;
    let promote_threshold = user_settings.discovery_promote_threshold;
    let auto_delete = user_settings.auto_delete_enabled;

    // Navidrome returns absolute paths from ITS filesystem (ReportRealPath).
    // If Navidrome runs on a different host or container, these paths need
    // prefix substitution. NAVIDROME_MUSIC_PATH tells us what prefix to
    // replace with the local mount point.
    // e.g. Navidrome sees /media/music/..., Soulbeet sees /music/...
    //      NAVIDROME_MUSIC_PATH=/media/music -> strip prefix, prepend local folder parent
    let navidrome_prefix = std::env::var("NAVIDROME_MUSIC_PATH").ok().filter(|s| !s.is_empty());
    let folders = crate::models::folder::Folder::get_all_by_user(user_id)
        .await
        .unwrap_or_default();

    let mut real_path_failures = 0u32;
    let mut deleted_tracks = 0u32;
    let mut promoted_tracks = 0u32;
    let mut removed_tracks = 0u32;
    let mut skipped_veto = 0u32;
    let mut skipped_not_found = 0u32;

    let pending_discovery_tracks = if let Some(ref fid) = user_settings.discovery_folder_id {
        DiscoveryTrackRow::get_pending_by_folder(fid).await?
    } else {
        vec![]
    };

    for song in &songs {
        // Auto-delete 1-star tracks (when enabled)
        if auto_delete {
            if let Some(rating) = song.user_rating {
                if rating == 1 {
                    let shared_veto = song
                        .average_rating
                        .map(|avg| avg > 1.0)
                        .unwrap_or(false);
                    if shared_veto {
                        info!(
                            "Auto-delete skipped (shared veto, avg={:.1}): {} - {}",
                            song.average_rating.unwrap_or(0.0),
                            song.artist.as_deref().unwrap_or("?"),
                            song.title
                        );
                        skipped_veto += 1;
                    } else if let Some(ref path_str) = song.path {
                        // Skip auto-delete for pending discovery tracks (handled separately below)
                        let is_discovery = pending_discovery_tracks.iter().any(|dt| {
                            dt.song_id.as_deref() == Some(&song.id)
                                || std::path::Path::new(&dt.path)
                                    .file_name()
                                    .map(|f| f.to_ascii_lowercase())
                                    == std::path::Path::new(path_str)
                                        .file_name()
                                        .map(|f| f.to_ascii_lowercase())
                        });
                        if is_discovery {
                            continue;
                        }
                        // ReportRealPath gives absolute paths from Navidrome's filesystem.
                        // Apply prefix substitution if Navidrome's mount differs from ours.
                        let local_path = resolve_navidrome_path(path_str, &navidrome_prefix, &folders);
                        let path = std::path::Path::new(&local_path);
                        if path.is_absolute() && path.exists() {
                            if let Err(e) = tokio::fs::remove_file(path).await {
                                warn!("Auto-delete failed for {}: {}", path.display(), e);
                            } else {
                                if let Some(parent) = path.parent() {
                                    let _ = cleanup_empty_dirs(parent).await;
                                }
                                DeletionReviewRow::upsert(
                                    &song.id,
                                    &song.title,
                                    song.artist.as_deref().unwrap_or("Unknown"),
                                    song.album.as_deref().unwrap_or("Unknown"),
                                    Some(&local_path),
                                    Some(rating),
                                    user_id,
                                )
                                .await?;
                                deleted_tracks += 1;
                            }
                        } else {
                            real_path_failures += 1;
                            skipped_not_found += 1;
                        }
                    }
                }
            }
        }

        // Check discovery track promotion/removal
        if let Some(user_rating) = song.user_rating {
            // Match by song_id first (exact), then by filename (fuzzy).
            // song_id is authoritative when set by reconciliation.
            let matching_track = pending_discovery_tracks.iter().find(|dt| {
                if let Some(ref dt_song_id) = dt.song_id {
                    return dt_song_id == &song.id;
                }
                // Fallback: match by filename when song_id isn't set yet
                if let Some(ref song_path) = song.path {
                    let song_fn = std::path::Path::new(song_path)
                        .file_name()
                        .map(|f| f.to_ascii_lowercase());
                    let dt_fn = std::path::Path::new(&dt.path)
                        .file_name()
                        .map(|f| f.to_ascii_lowercase());
                    return song_fn.is_some() && song_fn == dt_fn;
                }
                false
            });

            if let Some(track) = matching_track {
                if user_rating >= promote_threshold {
                    if let Err(e) = promote_discovery_track_internal(&track.id).await {
                        warn!("Failed to promote track {}: {}", track.title, e);
                    } else {
                        info!("Promoted discovery track: {} - {} (rating {})", track.artist, track.title, user_rating);
                        if let Err(e) = DiscoveryHistoryRow::update_outcome(
                            user_id,
                            &track.artist,
                            &track.title,
                            "promoted",
                        )
                        .await {
                            warn!("Failed to update history for promoted track '{}': {}", track.title, e);
                        }
                        promoted_tracks += 1;
                    }
                } else if user_rating == 1 {
                    if let Err(e) = remove_discovery_track_internal(&track.id).await {
                        warn!("Failed to remove track {}: {}", track.title, e);
                    } else {
                        info!("Removed discovery track: {} - {} (rating 1)", track.artist, track.title);
                        if let Err(e) = DiscoveryHistoryRow::update_outcome(
                            user_id,
                            &track.artist,
                            &track.title,
                            "removed",
                        )
                        .await {
                            warn!("Failed to update history for removed track '{}': {}", track.title, e);
                        }
                        removed_tracks += 1;
                    }
                }
            }
        }
    }

    if !auto_delete {
        info!("Auto-delete is disabled for this user");
    } else if skipped_veto > 0 || skipped_not_found > 0 {
        info!(
            "Auto-delete: {} skipped (shared veto), {} skipped (file not found)",
            skipped_veto, skipped_not_found
        );
        if real_path_failures > 0 {
            warn!(
                "Auto-delete: {} tracks had non-absolute paths. \
                 Enable ReportRealPath in Navidrome (player settings or ND_SUBSONIC_DEFAULTREPORTREALPATH=true) \
                 and ensure both containers mount music at the same path.",
                real_path_failures
            );
        }
    }
    info!(
        "Ratings sync complete: {} songs scanned, {} deleted, {} promoted, {} removed",
        total_songs_scanned, deleted_tracks, promoted_tracks, removed_tracks
    );

    Ok(SyncResult {
        deleted_tracks,
        promoted_tracks,
        removed_tracks,
        total_songs_scanned,
    })
}

#[get("/api/navidrome/deletion-history", auth: AuthSession)]
pub async fn get_deletion_history() -> Result<Vec<DeletionReview>, ServerFnError> {
    DeletionReviewRow::get_history(&auth.0.sub, 50)
        .await
        .map_err(server_error)
}

/// Map a Navidrome absolute path to the local filesystem.
///
/// When Navidrome runs on a different host/container, its paths have a different
/// prefix. NAVIDROME_MUSIC_PATH defines the prefix to strip, and we find the
/// matching local folder to prepend.
///
/// Example: Navidrome path `/media/music/common/AURORA/track.flac`
///          NAVIDROME_MUSIC_PATH = `/media/music`
///          User folder = `/music/common`
///          Result: `/music/common/AURORA/track.flac`
#[cfg(feature = "server")]
fn resolve_navidrome_path(
    navidrome_path: &str,
    navidrome_prefix: &Option<String>,
    folders: &[crate::models::folder::Folder],
) -> String {
    let Some(prefix) = navidrome_prefix else {
        // No prefix configured, use path as-is (same mount)
        return navidrome_path.to_string();
    };

    // Strip the Navidrome prefix to get the relative path
    let stripped = navidrome_path
        .strip_prefix(prefix.as_str())
        .unwrap_or(navidrome_path)
        .trim_start_matches('/');

    // Find which user folder contains this relative path.
    // stripped looks like "common/AURORA/track.flac"
    // folders have paths like "/music/common", "/music/terry"
    // We check if any folder's basename matches the first component of stripped.
    for folder in folders {
        let folder_name = std::path::Path::new(&folder.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if stripped == folder_name || stripped.starts_with(&format!("{}/", folder_name)) {
            let folder_parent = std::path::Path::new(&folder.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            return format!("{}/{}", folder_parent, stripped);
        }
    }

    // No matching folder found, try joining with the first folder's parent
    if let Some(first) = folders.first() {
        if let Some(parent) = std::path::Path::new(&first.path).parent() {
            return format!("{}/{}", parent.display(), stripped);
        }
    }

    navidrome_path.to_string()
}

/// Remove a directory if empty, then recurse up to its parent.
/// Stops at directories named "Discovery" or that contain a `.beets_library.db`
/// to avoid removing folder roots.
#[cfg(feature = "server")]
async fn cleanup_empty_dirs(dir: &std::path::Path) -> Result<(), std::io::Error> {
    // Don't remove folder roots or the Discovery directory itself
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
            Box::pin(cleanup_empty_dirs(parent)).await?;
        }
    }
    Ok(())
}

#[get("/api/navidrome/library-stats", auth: AuthSession)]
pub async fn get_library_stats() -> Result<LibraryStats, ServerFnError> {
    let client = navidrome_client_for_user(&auth.0.sub)
        .await
        .map_err(server_error)?;
    let songs = client
        .get_all_songs_with_ratings()
        .await
        .map_err(server_error)?;

    let albums = client.get_all_albums().await.map_err(server_error)?;

    let total_tracks = songs.len() as u32;
    let mut rated_tracks = 0u32;
    let mut rating_sum = 0.0f64;
    let mut rating_distribution = [0u32; 5];
    let mut genres: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut artists: std::collections::HashSet<String> = std::collections::HashSet::new();

    for song in &songs {
        if let Some(artist) = &song.artist {
            artists.insert(artist.to_lowercase());
        }
        if let Some(genre) = &song.genre {
            if !genre.is_empty() {
                *genres.entry(genre.clone()).or_default() += 1;
            }
        }
        if let Some(rating) = song.user_rating {
            if (1..=5).contains(&rating) {
                rated_tracks += 1;
                rating_sum += rating as f64;
                rating_distribution[(rating - 1) as usize] += 1;
            }
        }
    }

    let average_rating = if rated_tracks > 0 {
        rating_sum / rated_tracks as f64
    } else {
        0.0
    };

    let mut genre_vec: Vec<(String, u32)> = genres.into_iter().collect();
    genre_vec.sort_by(|a, b| b.1.cmp(&a.1));
    genre_vec.truncate(20);

    Ok(LibraryStats {
        total_tracks,
        rated_tracks,
        unrated_tracks: total_tracks - rated_tracks,
        average_rating,
        rating_distribution,
        total_albums: albums.len() as u32,
        total_artists: artists.len() as u32,
        genres: genre_vec,
    })
}

#[cfg(feature = "server")]
async fn promote_discovery_track_internal(track_id: &str) -> Result<(), String> {
    use crate::models::folder::Folder;

    let track = DiscoveryTrackRow::get_by_id(track_id)
        .await?
        .ok_or("Discovery track not found")?;

    let folder = Folder::get_by_id(&track.folder_id)
        .await?
        .ok_or("Folder not found")?;

    let src = std::path::PathBuf::from(&track.path);
    if !src.exists() {
        return Err(format!("Source file not found: {}", track.path));
    }

    let target = std::path::PathBuf::from(&folder.path);
    crate::server_fns::discovery::import_or_move(&src, &target).await?;

    DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Promoted).await?;
    info!("Promoted discovery track: {} -> {}", track.title, folder.path);
    Ok(())
}

#[cfg(feature = "server")]
async fn remove_discovery_track_internal(track_id: &str) -> Result<(), String> {
    let track = DiscoveryTrackRow::get_by_id(track_id)
        .await?
        .ok_or("Discovery track not found")?;

    let path = std::path::Path::new(&track.path);
    if path.exists() {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| format!("Failed to delete file: {}", e))?;
        if let Some(parent) = path.parent() {
            let _ = cleanup_empty_dirs(parent).await;
        }
    }

    DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Removed).await?;

    info!("Removed discovery track: {}", track.title);
    Ok(())
}
