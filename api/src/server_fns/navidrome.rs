use dioxus::prelude::*;
use shared::navidrome::{DeletionReview, LibraryStats, SyncResult};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use crate::folder_policy::{
    canonical_folder_path, file_owner, FileOwner, PolicyFolder, PolicyRows,
};
#[cfg(feature = "server")]
use crate::import_queue::{ImportQueue, IMPORT_QUEUE};
#[cfg(feature = "server")]
use crate::models::deletion_review::DeletionReviewRow;
#[cfg(feature = "server")]
use crate::models::discovery_history::DiscoveryHistoryRow;
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::services::navidrome_client_for_user;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use shared::navidrome::DiscoveryStatus;
#[cfg(feature = "server")]
use soulbeet::ForgetOutcome;
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};

#[cfg(feature = "server")]
use super::server_error;

#[post("/api/navidrome/sync-ratings", auth: AuthSession)]
pub async fn sync_ratings() -> Result<SyncResult, ServerFnError> {
    sync_ratings_internal(&auth.0.user_id)
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

    let folders = local_folders(&Folder::get_all_by_user(user_id).await.unwrap_or_default());
    // Every user's folders, so a file in someone else's folder that overlaps
    // one of the user's is left to its owner.
    let everyones_folders = if auto_delete {
        PolicyRows::load().await?.inputs().0
    } else {
        Vec::new()
    };
    let navidrome_music_path = std::env::var("NAVIDROME_MUSIC_PATH").ok();

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
        if auto_delete && song.user_rating == Some(1) {
            let artist = song.artist.as_deref().unwrap_or("?");
            match shared_veto(song.average_rating) {
                Veto::OtherRaters(avg) => {
                    info!(
                        "Auto-delete skipped (shared veto, avg={:.1}): {} - {}",
                        avg, artist, song.title
                    );
                    skipped_veto += 1;
                }
                Veto::AverageUnknown => {
                    info!(
                        "Auto-delete skipped (average rating unknown): {} - {}",
                        artist, song.title
                    );
                    skipped_veto += 1;
                }
                Veto::Allowed => {
                    if let Some(ref path_str) = song.path {
                        // A pending discovery track is deleted by the
                        // discovery branch below instead, which also marks
                        // its row Removed and records the outcome. Skip only
                        // the library delete: `continue`ing the whole song
                        // here jumped over that branch, so a 1-star pending
                        // track was never removed at all (#77).
                        let is_discovery = pending_discovery_tracks.iter().any(|dt| {
                            dt.song_id.as_deref() == Some(&song.id)
                                || std::path::Path::new(&dt.path)
                                    .file_name()
                                    .map(|f| f.to_ascii_lowercase())
                                    == std::path::Path::new(path_str)
                                        .file_name()
                                        .map(|f| f.to_ascii_lowercase())
                        });
                        if !is_discovery {
                            let local = resolve_navidrome_path(
                                path_str,
                                &folders,
                                navidrome_music_path.as_deref(),
                            );
                            match auto_delete_file(
                                &IMPORT_QUEUE,
                                local,
                                &everyones_folders,
                                user_id,
                            )
                            .await
                            {
                                AutoDelete::Deleted(path) => {
                                    DeletionReviewRow::upsert(
                                        &song.id,
                                        &song.title,
                                        song.artist.as_deref().unwrap_or("Unknown"),
                                        song.album.as_deref().unwrap_or("Unknown"),
                                        Some(&path.to_string_lossy()),
                                        song.user_rating,
                                        user_id,
                                    )
                                    .await?;
                                    deleted_tracks += 1;
                                }
                                AutoDelete::Kept => {}
                                AutoDelete::NotFound => {
                                    real_path_failures += 1;
                                    skipped_not_found += 1;
                                }
                            }
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
                    if let Err(e) = promote_discovery_track_internal(&track.id, user_id).await {
                        warn!("Failed to promote track {}: {}", track.title, e);
                    } else {
                        promoted_tracks += 1;
                    }
                } else if user_rating == 1 {
                    if let Err(e) = remove_discovery_track_internal(&track.id).await {
                        warn!("Failed to remove track {}: {}", track.title, e);
                    } else {
                        info!(
                            "Removed discovery track: {} - {} (rating 1)",
                            track.artist, track.title
                        );
                        if let Err(e) = DiscoveryHistoryRow::update_outcome(
                            user_id,
                            &track.artist,
                            &track.title,
                            "removed",
                        )
                        .await
                        {
                            warn!(
                                "Failed to update history for removed track '{}': {}",
                                track.title, e
                            );
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
                "Auto-delete: {} tracks could not be resolved to a local file. \
                 Ensure your folder paths in Soulbeet match the Navidrome library root.",
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
    DeletionReviewRow::get_history(&auth.0.user_id, 50)
        .await
        .map_err(server_error)
}

/// Check if the Soulbeet player in Navidrome has ReportRealPath enabled.
/// Returns Some(true) if enabled, Some(false) if disabled, None if player not found yet.
#[cfg(feature = "server")]
pub async fn check_report_real_path(user_id: &str) -> Result<Option<bool>, String> {
    let client = navidrome_client_for_user(user_id).await?;
    let players = client.get_players().await.map_err(|e| e.to_string())?;
    let soulbeet = players.iter().find(|p| p.client == "Soulbeet");
    Ok(soulbeet.map(|p| p.report_real_path))
}

/// Where a track Navidrome reports lives on this machine.
#[cfg(feature = "server")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LocalPath {
    /// A regular file strictly inside `folder`, the deepest of the user's
    /// folders that holds it.
    Found {
        path: PathBuf,
        folder: PathBuf,
    },
    NotFound,
    /// Several distinct files match, so none of them is chosen.
    Ambiguous(Vec<PathBuf>),
}

/// What auto-delete did with the file behind a 1-star song.
#[cfg(feature = "server")]
#[derive(Debug, PartialEq)]
enum AutoDelete {
    Deleted(PathBuf),
    /// Left in place: someone else's, several files match, or removing failed.
    Kept,
    NotFound,
}

/// Removes the local file `local` when the deepest folder holding it, among
/// `everyones_folders`, is `user_id`'s, then drops it from that folder's
/// library through `queue` and removes the directories that leaves empty up
/// to the user's folder.
#[cfg(feature = "server")]
async fn auto_delete_file(
    queue: &ImportQueue,
    local: LocalPath,
    everyones_folders: &[PolicyFolder],
    user_id: &str,
) -> AutoDelete {
    let (path, folder) = match local {
        LocalPath::Found { path, folder } => (path, folder),
        LocalPath::Ambiguous(paths) => {
            let paths: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
            info!(
                "Auto-delete skipped (several local files match): {}",
                paths.join(", ")
            );
            return AutoDelete::Kept;
        }
        LocalPath::NotFound => return AutoDelete::NotFound,
    };
    match file_owner(&path, everyones_folders, user_id) {
        FileOwner::Actor => {}
        FileOwner::Other => {
            info!(
                "Auto-delete skipped (in another user's folder): {}",
                path.display()
            );
            return AutoDelete::Kept;
        }
        FileOwner::Unresolved { folder, error } => {
            info!(
                "Auto-delete skipped: {} may be inside another user's folder {}, which cannot be resolved ({error})",
                path.display(),
                folder.display()
            );
            return AutoDelete::Kept;
        }
    }
    if let Err(e) = tokio::fs::remove_file(&path).await {
        warn!("Auto-delete failed for {}: {}", path.display(), e);
        return AutoDelete::Kept;
    }
    info!("Auto-deleted {}", path.display());
    // The file is gone whatever the library says, so the delete counts.
    match queue.forget(&folder, &path).await {
        ForgetOutcome::Removed => info!(
            "Removed {} from the library of {}",
            path.display(),
            folder.display()
        ),
        ForgetOutcome::NotInLibrary => warn!(
            "{} was not in the library of {}",
            path.display(),
            folder.display()
        ),
        ForgetOutcome::Several(n) => warn!(
            "Left {} in the library of {}: {n} items have that path",
            path.display(),
            folder.display()
        ),
        ForgetOutcome::Failed(e) => warn!(
            "Could not remove {} from the library of {}: {e}",
            path.display(),
            folder.display()
        ),
    }
    if let Some(parent) = path.parent() {
        let _ = super::cleanup_empty_ancestors(parent, &folder).await;
    }
    AutoDelete::Deleted(path)
}

/// The user's folders through `canonical_folder_path`, leaving out the ones
/// that cannot be resolved.
#[cfg(feature = "server")]
fn local_folders(folders: &[Folder]) -> Vec<PathBuf> {
    folders
        .iter()
        .filter_map(|f| canonical_folder_path(Path::new(&f.path)).ok())
        .collect()
}

/// Finds the local file behind a path Navidrome reports, only ever inside
/// one of `folders` (canonical, from `local_folders`).
///
/// An absolute path is tried as given, a relative one (Navidrome's library
/// root) under each folder. When Navidrome mounts the music elsewhere,
/// `navidrome_music_path` is its root: the part of the path below it is
/// tried under each folder and each of the folder's ancestors.
#[cfg(feature = "server")]
pub(crate) fn resolve_navidrome_path(
    navidrome_path: &str,
    folders: &[PathBuf],
    navidrome_music_path: Option<&str>,
) -> LocalPath {
    let reported = Path::new(navidrome_path);
    let mut candidates = Vec::new();
    if reported.is_absolute() {
        candidates.push(reported.to_path_buf());
    } else {
        candidates.extend(folders.iter().map(|f| f.join(reported)));
    }
    if let Some(rest) = navidrome_music_path.and_then(|root| reported.strip_prefix(root).ok()) {
        for folder in folders {
            candidates.extend(folder.ancestors().map(|dir| dir.join(rest)));
        }
    }

    let mut found = std::collections::BTreeMap::new();
    for candidate in candidates {
        let Ok(path) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let deepest = folders
            .iter()
            .filter(|f| path.starts_with(f) && path != **f)
            .max_by_key(|f| f.components().count());
        if let Some(folder) = deepest {
            found.insert(path, folder.clone());
        }
    }

    if found.len() > 1 {
        return LocalPath::Ambiguous(found.into_keys().collect());
    }
    match found.pop_first() {
        Some((path, folder)) => LocalPath::Found { path, folder },
        None => LocalPath::NotFound,
    }
}

/// Whether other listeners' ratings block an auto-delete.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Veto {
    Allowed,
    OtherRaters(f64),
    /// Navidrome leaves the average out when average ratings are off.
    AverageUnknown,
}

#[cfg(feature = "server")]
pub(crate) fn shared_veto(average_rating: Option<f64>) -> Veto {
    match average_rating {
        None => Veto::AverageUnknown,
        Some(avg) if avg > 1.0 => Veto::OtherRaters(avg),
        Some(_) => Veto::Allowed,
    }
}

#[get("/api/navidrome/library-stats", auth: AuthSession)]
pub async fn get_library_stats() -> Result<LibraryStats, ServerFnError> {
    let client = navidrome_client_for_user(&auth.0.user_id)
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

/// Why a promote did not happen.
#[cfg(feature = "server")]
#[derive(Debug)]
pub(crate) enum PromoteError {
    /// Written for the user: the track's state or the import outcome.
    Refused(String),
    /// Internal, such as a database error.
    Failed(String),
}

#[cfg(feature = "server")]
impl std::fmt::Display for PromoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromoteError::Refused(m) | PromoteError::Failed(m) => f.write_str(m),
        }
    }
}

#[cfg(feature = "server")]
impl From<String> for PromoteError {
    fn from(e: String) -> Self {
        PromoteError::Failed(e)
    }
}

#[cfg(feature = "server")]
impl From<&str> for PromoteError {
    fn from(e: &str) -> Self {
        PromoteError::Failed(e.to_string())
    }
}

/// Promotes in its own task, so the status and history updates always follow
/// the import even when the request that started it goes away while the
/// import waits in the library's queue.
#[cfg(feature = "server")]
pub(crate) async fn promote_discovery_track_internal(
    track_id: &str,
    user_id: &str,
) -> Result<(), PromoteError> {
    let (track_id, user_id) = (track_id.to_string(), user_id.to_string());
    tokio::spawn(async move { promote_detached(&track_id, &user_id).await })
        .await
        .unwrap_or_else(|e| Err(PromoteError::Failed(format!("Promote stopped: {e}"))))
}

#[cfg(feature = "server")]
async fn promote_detached(track_id: &str, user_id: &str) -> Result<(), PromoteError> {
    use crate::models::discovery_history::DiscoveryHistoryRow;

    // CAS guard: only one caller can promote a given track
    if !DiscoveryTrackRow::try_set_promoting(track_id).await? {
        return Err(PromoteError::Refused(
            "Track is no longer pending (already promoted, removed, or being promoted)".to_string(),
        ));
    }

    let track = DiscoveryTrackRow::get_by_id(track_id)
        .await?
        .ok_or("Discovery track not found")?;

    let folder = Folder::get_by_id(&track.folder_id)
        .await?
        .ok_or("Folder not found")?;

    let src = std::path::PathBuf::from(&track.path);
    if !src.exists() {
        // File missing -- revert to Pending so expiry can handle it
        let _ = DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Pending).await;
        warn!("Promote of '{}': {} is gone", track.title, track.path);
        return Err(PromoteError::Refused(
            "The downloaded file is gone".to_string(),
        ));
    }

    let target = std::path::PathBuf::from(&folder.path);
    if let Err(outcome) = crate::import_queue::import_to_library(&src, &target).await {
        // Import failed -- revert to Pending
        let _ = DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Pending).await;
        return Err(PromoteError::Refused(outcome));
    }

    if let Some(parent) = src.parent() {
        let profile_root = folder.discovery_profile_path(&track.profile);
        let _ = super::cleanup_empty_ancestors(parent, std::path::Path::new(&profile_root)).await;
    }

    DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Promoted).await?;

    if let Err(e) =
        DiscoveryHistoryRow::update_outcome(user_id, &track.artist, &track.title, "promoted").await
    {
        warn!(
            "Failed to update history for promoted track '{}': {}",
            track.title, e
        );
    }

    info!(
        "Promoted discovery track: {} -> {}",
        track.title, folder.path
    );
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
        let folder = Folder::get_by_id(&track.folder_id).await;
        if let (Some(parent), Ok(Some(folder))) = (path.parent(), folder) {
            let profile_root = folder.discovery_profile_path(&track.profile);
            let _ =
                super::cleanup_empty_ancestors(parent, std::path::Path::new(&profile_root)).await;
        }
    }

    DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Removed).await?;

    info!("Removed discovery track: {}", track.title);
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    /// `tmp/music/alice/A/x.flac`, `tmp/music/bob/B/y.flac`, `tmp/outside/z.flac`.
    fn library() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        for file in [
            "music/alice/A/x.flac",
            "music/bob/B/y.flac",
            "outside/z.flac",
        ] {
            let path = tmp.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"audio").unwrap();
        }
        (dir, tmp)
    }

    fn folder(path: &Path) -> Folder {
        Folder {
            id: "f1".to_string(),
            user_id: "alice".to_string(),
            name: "Music".to_string(),
            path: path.to_str().unwrap().to_string(),
            shared: false,
        }
    }

    #[test]
    fn absolute_path_inside_a_folder_is_found() {
        let (_dir, tmp) = library();
        let alice = tmp.join("music/alice");
        let track = alice.join("A/x.flac");
        assert_eq!(
            resolve_navidrome_path(track.to_str().unwrap(), std::slice::from_ref(&alice), None),
            LocalPath::Found {
                path: track,
                folder: alice
            }
        );
    }

    #[test]
    fn absolute_path_outside_every_folder_is_not_found() {
        let (_dir, tmp) = library();
        let outside = tmp.join("outside/z.flac");
        assert_eq!(
            resolve_navidrome_path(outside.to_str().unwrap(), &[tmp.join("music/alice")], None),
            LocalPath::NotFound
        );
    }

    #[test]
    fn parent_components_cannot_escape_a_folder() {
        let (_dir, tmp) = library();
        assert_eq!(
            resolve_navidrome_path("../bob/B/y.flac", &[tmp.join("music/alice")], None),
            LocalPath::NotFound
        );
    }

    #[test]
    fn a_symlink_inside_a_folder_pointing_outside_is_not_found() {
        let (_dir, tmp) = library();
        let link = tmp.join("music/alice/link.flac");
        symlink(tmp.join("outside/z.flac"), &link).unwrap();
        assert_eq!(
            resolve_navidrome_path(link.to_str().unwrap(), &[tmp.join("music/alice")], None),
            LocalPath::NotFound
        );
        assert_eq!(
            resolve_navidrome_path("link.flac", &[tmp.join("music/alice")], None),
            LocalPath::NotFound
        );
    }

    #[test]
    fn folder_reached_through_a_symlink_still_matches() {
        let (_dir, tmp) = library();
        symlink(tmp.join("music"), tmp.join("lib")).unwrap();
        let folders = local_folders(&[folder(&tmp.join("lib/alice"))]);
        let track = tmp.join("music/alice/A/x.flac");
        assert_eq!(
            resolve_navidrome_path(track.to_str().unwrap(), &folders, None),
            LocalPath::Found {
                path: track,
                folder: tmp.join("music/alice")
            }
        );
    }

    #[test]
    fn navidrome_music_path_maps_onto_the_folder_that_holds_the_file() {
        let (_dir, tmp) = library();
        let alice = tmp.join("music/alice");
        assert_eq!(
            resolve_navidrome_path(
                "/navi/alice/A/x.flac",
                std::slice::from_ref(&alice),
                Some("/navi")
            ),
            LocalPath::Found {
                path: alice.join("A/x.flac"),
                folder: alice
            }
        );
    }

    #[test]
    fn navidrome_music_path_never_reaches_a_sibling_folder() {
        let (_dir, tmp) = library();
        assert_eq!(
            resolve_navidrome_path(
                "/navi/bob/B/y.flac",
                &[tmp.join("music/alice")],
                Some("/navi")
            ),
            LocalPath::NotFound
        );
    }

    #[test]
    fn a_directory_is_never_a_track() {
        let (_dir, tmp) = library();
        let album = tmp.join("music/alice/A");
        assert_eq!(
            resolve_navidrome_path(album.to_str().unwrap(), &[tmp.join("music/alice")], None),
            LocalPath::NotFound
        );
    }

    #[test]
    fn several_matching_files_are_ambiguous() {
        let (_dir, tmp) = library();
        let (alice, bob) = (tmp.join("music/alice"), tmp.join("music/bob"));
        std::fs::create_dir_all(bob.join("A")).unwrap();
        std::fs::write(bob.join("A/x.flac"), b"audio").unwrap();
        assert_eq!(
            resolve_navidrome_path("A/x.flac", &[alice.clone(), bob.clone()], None),
            LocalPath::Ambiguous(vec![alice.join("A/x.flac"), bob.join("A/x.flac")])
        );
    }

    #[test]
    fn the_deepest_folder_holding_the_file_is_the_cleanup_root() {
        let (_dir, tmp) = library();
        let (music, alice) = (tmp.join("music"), tmp.join("music/alice"));
        let track = alice.join("A/x.flac");
        assert_eq!(
            resolve_navidrome_path(track.to_str().unwrap(), &[music, alice.clone()], None),
            LocalPath::Found {
                path: track,
                folder: alice
            }
        );
    }

    fn owned(user: &str, path: &Path, shared: bool) -> PolicyFolder {
        PolicyFolder {
            user_id: user.to_string(),
            owner_is_admin: false,
            path: Ok(path.to_path_buf()),
            stored_path: path.to_path_buf(),
            shared,
        }
    }

    fn write_file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"audio").unwrap();
    }

    /// A queue whose importer records what it is asked to forget.
    fn import_queue() -> (ImportQueue, Arc<StubImporter>) {
        let stub = Arc::new(StubImporter::new());
        (ImportQueue::new(RecordingDeps::new(stub.clone())), stub)
    }

    /// What auto-delete does with the file Navidrome reports at `file`, for
    /// `user`, whose own folders are `own`.
    async fn rate_one_star(
        queue: &ImportQueue,
        file: &Path,
        own: &[PathBuf],
        everyones: &[PolicyFolder],
        user: &str,
    ) -> AutoDelete {
        let local = resolve_navidrome_path(file.to_str().unwrap(), own, None);
        auto_delete_file(queue, local, everyones, user).await
    }

    #[tokio::test]
    async fn auto_delete_never_removes_another_users_folder_on_the_way_up() {
        // Alice has /music (shared) and /music/x/a; Bob has /music/x.
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let (music, x, a) = (
            tmp.join("music"),
            tmp.join("music/x"),
            tmp.join("music/x/a"),
        );
        let everyones = [
            owned("alice", &music, true),
            owned("alice", &a, false),
            owned("bob", &x, false),
        ];
        let own = [music.clone(), a.clone()];
        let (queue, stub) = import_queue();

        // The only file in Alice's /music/x/a, two levels down: the walk
        // stops at her deepest folder, so Bob's /music/x stays.
        let alices = a.join("Artist/Album/s.flac");
        write_file(&alices);
        assert_eq!(
            rate_one_star(&queue, &alices, &own, &everyones, "alice").await,
            AutoDelete::Deleted(alices.clone())
        );
        assert!(!a.join("Artist").exists());
        assert!(a.is_dir());
        assert!(x.is_dir());

        // A file in Bob's folder is his, though Alice's shared /music holds it.
        let bobs = x.join("B/t.flac");
        write_file(&bobs);
        assert_eq!(
            rate_one_star(&queue, &bobs, &own, &everyones, "alice").await,
            AutoDelete::Kept
        );
        assert!(bobs.is_file());
        assert_eq!(stub.forgets(), vec![(a, alices)]);
    }

    #[tokio::test]
    async fn auto_delete_keeps_a_file_an_unresolvable_foreign_folder_may_hold() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let (music, bob) = (tmp.join("music"), tmp.join("music/bob"));
        let everyones = [
            owned("alice", &music, false),
            PolicyFolder {
                path: Err("Permission denied".to_string()),
                ..owned("bob", &bob, false)
            },
        ];
        let bobs = bob.join("B/y.flac");
        write_file(&bobs);
        let (queue, stub) = import_queue();

        assert_eq!(
            rate_one_star(
                &queue,
                &bobs,
                std::slice::from_ref(&music),
                &everyones,
                "alice"
            )
            .await,
            AutoDelete::Kept
        );
        assert!(bobs.is_file());

        // A file outside Bob's stored path is still Alice's to delete.
        let alices = music.join("alice/A/x.flac");
        write_file(&alices);
        assert_eq!(
            rate_one_star(
                &queue,
                &alices,
                std::slice::from_ref(&music),
                &everyones,
                "alice"
            )
            .await,
            AutoDelete::Deleted(alices.clone())
        );
        assert!(bob.is_dir());
        assert_eq!(stub.forgets(), vec![(music, alices)]);
    }

    #[test]
    fn the_veto_fails_closed() {
        assert_eq!(shared_veto(None), Veto::AverageUnknown);
        assert_eq!(shared_veto(Some(2.5)), Veto::OtherRaters(2.5));
        assert_eq!(shared_veto(Some(1.0)), Veto::Allowed);
    }
}
