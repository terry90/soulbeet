//! Admin migration tool for retired legacy/shared folders recorded in
//! `legacy_folders` during the single-folder-per-user upgrade.
//!
//! Splits each legacy folder into per-user hardlinks inside each participating
//! user's personal library folder, registers files in each user's beets DB,
//! best-effort carries over Navidrome ratings/stars/static playlist entries,
//! and optionally removes legacy copies ONLY after verifying every target link
//! exists on disk with matching byte size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::navidrome::models::NativeSong;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::link::{link_or_copy_file, predict_link_mode, share_sidecars};
use super::{
    find_sidecars, trigger_and_wait_scan, wait_for_idle_scan, ScanWaitOutcome,
    IDLE_SCAN_WAIT_MAX, SCAN_WAIT_MAX,
};
use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::delete_guard::{paths, Guard};
use crate::core::index::NavidromeOf;
use crate::folder_policy::canonical_folder_path;
use crate::import_queue::ImportQueue;
use crate::models::legacy_folder::{LegacyFolder, LegacyFolderWithOwner};
use crate::models::track_share::{LinkMode, NewTrackShare, ShareOrigin, TrackShareRow};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyFolderGroup {
    pub path: String,
    pub was_shared: bool,
    pub recipients: Vec<LegacyGroupRecipient>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGroupRecipient {
    pub legacy_id: String,
    pub user_id: String,
    pub username: String,
    pub target_folder_id: String,
    pub target_path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrateLegacyRequest {
    /// Legacy folder path from `legacy_folders` to migrate.
    pub legacy_path: String,
    /// When true, unlink source files in `legacy_path` ONLY after verifying every
    /// target hardlink/copy exists with matching size.
    pub remove_source_after: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrateLegacyReport {
    pub legacy_path: String,
    pub dry_run: bool,
    pub total_audio_files: usize,
    pub target_users: Vec<String>,
    pub linked_count: usize,
    pub already_present_count: usize,
    pub failed_count: usize,
    pub removed_source_count: usize,
    pub hardlink_count: usize,
    pub reflink_count: usize,
    pub copy_count: usize,
    pub annotations_restored: usize,
    pub warnings: Vec<String>,
}

/// Groups unmigrated `legacy_folders` rows by their legacy source `path`.
pub async fn list_legacy_groups() -> Result<Vec<LegacyFolderGroup>, Failure> {
    list_legacy_groups_in(&crate::db::DB).await
}

pub(crate) async fn list_legacy_groups_in(db: &SqlitePool) -> Result<Vec<LegacyFolderGroup>, Failure> {
    let rows = LegacyFolder::list_unmigrated_in(db)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    Ok(group_legacy_rows(rows))
}

fn group_legacy_rows(rows: Vec<LegacyFolderWithOwner>) -> Vec<LegacyFolderGroup> {
    let mut by_path: HashMap<String, LegacyFolderGroup> = HashMap::new();
    for r in rows {
        let entry = by_path
            .entry(r.legacy.path.clone())
            .or_insert_with(|| LegacyFolderGroup {
                path: r.legacy.path.clone(),
                was_shared: r.legacy.was_shared,
                recipients: Vec::new(),
            });
        entry.was_shared |= r.legacy.was_shared;
        entry.recipients.push(LegacyGroupRecipient {
            legacy_id: r.legacy.id,
            user_id: r.legacy.user_id,
            username: r.owner,
            target_folder_id: r.legacy.target_folder_id,
            target_path: r.target_path,
        });
    }
    let mut groups: Vec<LegacyFolderGroup> = by_path.into_values().collect();
    groups.sort_by(|a, b| a.path.cmp(&b.path));
    groups
}

/// Walks `root` recursively (max depth 16), skipping hidden dot-directories and
/// `Discovery/` staging folders, returning sorted audio file paths.
fn walk_audio_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 16 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if depth == 0 && name.eq_ignore_ascii_case("Discovery") {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if meta.is_file() && soulbeet::audio::is_audio_path(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Computes the SHA-256 digest of a regular file on disk.
fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Verifies `src` and `dst` are both regular non-symlink files with identical
/// content before allowing `src` to be unlinked:
/// - For `LinkMode::Hardlink`, requires matching `(st_dev, st_ino)`.
/// - For `already_present` (`None`), `Reflink`, or `Copy`, accepts matching
///   `(st_dev, st_ino)` or SHA-256 digest equality.
fn verify_target_matches_source(src: &Path, dst: &Path, mode: Option<LinkMode>) -> bool {
    let (Ok(src_meta), Ok(dst_meta)) =
        (std::fs::symlink_metadata(src), std::fs::symlink_metadata(dst))
    else {
        return false;
    };
    if !src_meta.is_file()
        || src_meta.file_type().is_symlink()
        || !dst_meta.is_file()
        || dst_meta.file_type().is_symlink()
    {
        return false;
    }
    if src_meta.len() != dst_meta.len() {
        return false;
    }

    #[cfg(unix)]
    let same_inode = {
        use std::os::unix::fs::MetadataExt;
        (src_meta.dev(), src_meta.ino()) == (dst_meta.dev(), dst_meta.ino())
    };
    #[cfg(not(unix))]
    let same_inode = false;

    if mode == Some(LinkMode::Hardlink) {
        return same_inode;
    }
    if same_inode {
        return true;
    }
    match (sha256_file(src), sha256_file(dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Returns the first active folder in `folders` whose canonical path equals,
/// contains, or is contained by `source_root`.
async fn find_overlapping_active_folder(
    db: &SqlitePool,
    source_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let active_paths: Vec<String> = sqlx::query_scalar("SELECT path FROM folders")
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
    for raw in active_paths {
        let active = canonical_folder_path(Path::new(&raw)).unwrap_or_else(|_| PathBuf::from(&raw));
        if source_root == active
            || source_root.starts_with(&active)
            || active.starts_with(source_root)
        {
            return Ok(Some(active));
        }
    }
    Ok(None)
}

/// Snapshot of one user's Navidrome annotations on a legacy track before migration.
struct SavedAnnotation {
    user_id: String,
    source_song: NativeSong,
    rating: u8,
    starred: bool,
    static_playlist_ids: Vec<String>,
}

/// Migrates a legacy/shared folder recorded in `legacy_folders` into per-user
/// hardlinks inside each participating user's personal folder.
pub(crate) async fn migrate_legacy_folder(
    guard: &Guard<'_>,
    navidrome: &dyn NavidromeOf,
    queue: &ImportQueue,
    actor: &UserCtx,
    req: MigrateLegacyRequest,
) -> Result<MigrateLegacyReport, Failure> {
    let rows = LegacyFolder::list_unmigrated_for_path_in(guard.db, &req.legacy_path)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    if rows.is_empty() {
        return Err(Failure::new(
            FailureCode::UnknownHandle,
            Stage::Share,
            RetryAdvice::NotRetryable,
            format!("No unmigrated legacy folder entry found for '{}'", req.legacy_path),
        ));
    }

    let source_root = canonical_folder_path(Path::new(&req.legacy_path)).map_err(|e| {
        Failure::new(
            FailureCode::ShareFailed,
            Stage::Share,
            RetryAdvice::NotRetryable,
            format!("Could not resolve legacy folder '{}': {e}", req.legacy_path),
        )
    })?;

    if !source_root.is_dir() {
        return Err(Failure::new(
            FailureCode::ShareFailed,
            Stage::Share,
            RetryAdvice::NotRetryable,
            format!("Legacy folder '{}' does not exist on disk", source_root.display()),
        ));
    }

    let mut targets: Vec<(LegacyFolderWithOwner, PathBuf)> = Vec::new();
    for row in rows {
        let target_root = canonical_folder_path(Path::new(&row.target_path)).map_err(|e| {
            Failure::new(
                FailureCode::ShareFailed,
                Stage::Share,
                RetryAdvice::NotRetryable,
                format!(
                    "Could not resolve target folder '{}' for {}: {e}",
                    row.target_path, row.owner
                ),
            )
        })?;
        if target_root == source_root {
            continue;
        }
        if !req.dry_run {
            let _ = tokio::fs::create_dir_all(&target_root).await;
        }
        targets.push((row, target_root));
    }

    if targets.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Share,
            RetryAdvice::NotRetryable,
            "All target folders already equal the legacy folder path",
        ));
    }

    let walk_root = source_root.clone();
    let audio_files = tokio::task::spawn_blocking(move || walk_audio_files(&walk_root))
        .await
        .map_err(|e| {
            Failure::new(
                FailureCode::ShareFailed,
                Stage::Share,
                RetryAdvice::SameSourceLater,
                format!("Failed scanning legacy folder: {e}"),
            )
        })?;

    let target_users: Vec<String> = targets.iter().map(|(r, _)| r.owner.clone()).collect();
    let mut linked_count = 0usize;
    let mut already_present_count = 0usize;
    let mut failed_count = 0usize;
    let mut removed_source_count = 0usize;
    let mut hardlink_count = 0usize;
    let mut reflink_count = 0usize;
    let mut copy_count = 0usize;
    let mut warnings: Vec<String> = Vec::new();

    if req.dry_run {
        for src in &audio_files {
            let Ok(rel) = src.strip_prefix(&source_root) else {
                continue;
            };
            for (_, target_root) in &targets {
                let dst = target_root.join(rel);
                if dst.exists() {
                    already_present_count += 1;
                } else {
                    linked_count += 1;
                    match predict_link_mode(src, target_root) {
                        LinkMode::Hardlink => hardlink_count += 1,
                        LinkMode::Reflink => reflink_count += 1,
                        LinkMode::Copy => copy_count += 1,
                    }
                }
            }
        }
        return Ok(MigrateLegacyReport {
            legacy_path: req.legacy_path,
            dry_run: true,
            total_audio_files: audio_files.len(),
            target_users,
            linked_count,
            already_present_count,
            failed_count,
            removed_source_count,
            hardlink_count,
            reflink_count,
            copy_count,
            annotations_restored: 0,
            warnings,
        });
    }

    let actor_uid = UserId::new(&actor.user_id);
    if let Ok(own_navi) = navidrome.navidrome_of(&actor_uid).await {
        let _ = wait_for_idle_scan(&own_navi, IDLE_SCAN_WAIT_MAX).await;
    }

    // Best-effort snapshot of Navidrome annotations (ratings, stars, static playlists)
    // for each participating user on legacy files before linking/removing.
    let mut saved_annotations: Vec<(PathBuf, SavedAnnotation)> = Vec::new();
    for (row, _) in &targets {
        let uid = UserId::new(&row.legacy.user_id);
        let Ok(navi) = navidrome.navidrome_of(&uid).await else {
            continue;
        };
        let mut songs = Vec::new();
        let mut start = 0u32;
        const PAGE: u32 = 500;
        while let Ok(page) = navi.list_songs(start, start + PAGE).await {
            let count = page.songs.len() as u32;
            songs.extend(page.songs);
            if count < PAGE || songs.len() as u64 >= page.total {
                break;
            }
            start = start.saturating_add(PAGE);
        }
        for song in songs {
                let Ok((local, _)) =
                    paths::local_file_learning(guard.db, &song, guard.music_path, &uid).await
                else {
                    continue;
                };
                if !local.starts_with(&source_root) {
                    continue;
                }
                let starred = song.starred;
                let rating = song.rating;
                let static_playlists: Vec<String> = navi
                    .get_song_playlists(&song.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| !p.is_smart() && p.owner_name.eq_ignore_ascii_case(&row.owner))
                    .map(|p| p.id)
                    .collect();
                if rating > 0 || starred || !static_playlists.is_empty() {
                    if let Ok(rel) = local.strip_prefix(&source_root) {
                        saved_annotations.push((
                            rel.to_path_buf(),
                            SavedAnnotation {
                                user_id: row.legacy.user_id.clone(),
                                source_song: song,
                                rating,
                                starred,
                                static_playlist_ids: static_playlists,
                            },
                        ));
                    }
                }
        }
    }

    let batch_id = uuid::Uuid::new_v4().to_string();
    let mut verified_removable: Vec<PathBuf> = Vec::new();

    for src in &audio_files {
        let Ok(rel) = src.strip_prefix(&source_root) else {
            continue;
        };
        let mut all_targets_verified = true;

        for (row, target_root) in &targets {
            let dst = target_root.join(rel);
            if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
                already_present_count += 1;
                let src_v = src.clone();
                let dst_v = dst.clone();
                let matches = tokio::task::spawn_blocking(move || {
                    verify_target_matches_source(&src_v, &dst_v, None)
                })
                .await
                .unwrap_or(false);
                if !matches {
                    all_targets_verified = false;
                    warnings.push(format!(
                        "Target '{}' already exists for {} with different content than source; source file will not be removed",
                        rel.display(),
                        row.owner
                    ));
                }
                continue;
            }

            let src_c = src.clone();
            let dst_c = dst.clone();
            let link_res = tokio::task::spawn_blocking(move || -> std::io::Result<(LinkMode, Vec<String>, bool)> {
                let mode = link_or_copy_file(&src_c, &dst_c)?;
                let verified = verify_target_matches_source(&src_c, &dst_c, Some(mode));
                let sidecars = find_sidecars(&src_c);
                let w = match dst_c.parent() {
                    Some(dst_dir) => share_sidecars(&sidecars, dst_dir, dst_c.file_stem()),
                    None => Vec::new(),
                };
                Ok((mode, w, verified))
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|r| r);

            match link_res {
                Ok((mode, w, verified)) => {
                    warnings.extend(w);
                    linked_count += 1;
                    match mode {
                        LinkMode::Hardlink => hardlink_count += 1,
                        LinkMode::Reflink => reflink_count += 1,
                        LinkMode::Copy => copy_count += 1,
                    }
                    if !verified {
                        all_targets_verified = false;
                        warnings.push(format!(
                            "Could not verify linked target '{}' for {}; source file will not be removed",
                            rel.display(),
                            row.owner
                        ));
                    }
                    let _ = queue.register_in_place(target_root, &dst).await;
                    let _ = TrackShareRow::insert_in(
                        guard.db,
                        &NewTrackShare {
                            batch_id: &batch_id,
                            sender_user_id: &actor.user_id,
                            recipient_user_id: &row.legacy.user_id,
                            source_song_id: "",
                            target_song_id: None,
                            source_path: &src.to_string_lossy(),
                            target_path: &dst.to_string_lossy(),
                            artist: "",
                            album: "",
                            title: src
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(""),
                            link_mode: mode,
                            origin: ShareOrigin::Migration,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    failed_count += 1;
                    all_targets_verified = false;
                    warnings.push(format!(
                        "Failed migrating {} to {}: {e}",
                        rel.display(),
                        row.owner
                    ));
                }
            }
        }

        if all_targets_verified {
            verified_removable.push(src.clone());
        }
    }

    // Trigger Navidrome scan so newly hardlinked target files get indexed.
    let mut annotations_restored = 0usize;
    let mut scan_and_restore_completed = saved_annotations.is_empty();
    if linked_count > 0 || !saved_annotations.is_empty() {
        if let Ok(own_navi) = navidrome.navidrome_of(&actor_uid).await {
            let outcome = trigger_and_wait_scan(
                guard.db,
                navidrome,
                &actor_uid,
                &own_navi,
                Stage::Share,
                SCAN_WAIT_MAX,
            )
            .await;

            // Restore ratings, stars, and static playlist memberships on each user's new song_id.
            if outcome == ScanWaitOutcome::Completed && !saved_annotations.is_empty() {
                for (rel, saved) in &saved_annotations {
                    let Some((_, target_root)) = targets
                        .iter()
                        .find(|(r, _)| r.legacy.user_id == saved.user_id)
                    else {
                        continue;
                    };
                    let target_file = target_root.join(rel);
                    let uid = UserId::new(&saved.user_id);
                    let Ok(navi) = navidrome.navidrome_of(&uid).await else {
                        continue;
                    };
                    let item = ImportedItem {
                        path: target_file.clone(),
                        recording_mbid: saved.source_song.mbz_recording_id.clone(),
                        artist: saved.source_song.artist.clone(),
                        title: saved.source_song.title.clone(),
                        album: saved.source_song.album.clone(),
                        beets_id: None,
                        release_mbid: None,
                        match_per_mille: None,
                    };
                    let Ok(Some(cands)) = guard.listeners.songs_for_item(&uid, &item).await else {
                        continue;
                    };
                    let _ = paths::learn_in(guard.db, &cands, &target_file).await;
                    let Ok(canon_target) = std::fs::canonicalize(&target_file) else {
                        continue;
                    };
                    let mut new_song_id = None;
                    for c in cands {
                        if let Ok((local, _)) =
                            paths::local_file_learning(guard.db, &c, guard.music_path, &uid).await
                        {
                            if local == canon_target {
                                new_song_id = Some(c.id);
                                break;
                            }
                        }
                    }
                    let Some(new_id) = new_song_id else {
                        continue;
                    };
                    if saved.rating > 0 {
                        let _ = navi.set_rating(&new_id, saved.rating).await;
                    }
                    if saved.starred {
                        let _ = navi
                            .set_star(soulbeet::navidrome::StarTarget::Song(&new_id), true)
                            .await;
                    }
                    for pl_id in &saved.static_playlist_ids {
                        let existing_tracks = navi.playlist_tracks(pl_id).await.ok().flatten();
                        let already_has_new = existing_tracks
                            .as_ref()
                            .is_some_and(|tracks| tracks.iter().any(|t| t.song.id == new_id));
                        let appended = if already_has_new {
                            true
                        } else {
                            navi.append_playlist_tracks(pl_id, std::slice::from_ref(&new_id))
                                .await
                                .unwrap_or(false)
                        };
                        if appended && saved.source_song.id != new_id {
                            if let Some(tracks) = existing_tracks {
                                let old_indexes: Vec<usize> = tracks
                                    .iter()
                                    .filter(|t| t.song.id == saved.source_song.id)
                                    .map(|t| (t.position.get() - 1) as usize)
                                    .collect();
                                if !old_indexes.is_empty() {
                                    let _ = navi.remove_playlist_tracks(pl_id, &old_indexes).await;
                                }
                            }
                        }
                    }
                    annotations_restored += 1;
                }
                scan_and_restore_completed = annotations_restored == saved_annotations.len();
            }
        }
    }

    // Optional safe cleanup of legacy files ONLY after verifying:
    //   1. No active user folder equals, contains, or is contained by source_root
    //   2. Zero migration failures occurred
    //   3. All saved Navidrome annotations were restored after a completed scan
    //   4. Every target file passed inode or SHA-256 digest verification
    let mut removal_refused = false;
    if req.remove_source_after {
        if failed_count > 0 {
            removal_refused = true;
            warnings.push(format!(
                "Refused to remove legacy source files because {failed_count} file(s) failed to migrate"
            ));
        } else if !saved_annotations.is_empty() && !scan_and_restore_completed {
            removal_refused = true;
            warnings.push(
                "Refused to remove legacy source files because Navidrome scan or annotation restoration did not complete"
                    .to_string(),
            );
        } else {
            match find_overlapping_active_folder(guard.db, &source_root).await {
                Ok(Some(active)) => {
                    removal_refused = true;
                    warnings.push(format!(
                        "Refused to remove legacy source files because '{}' overlaps active library folder '{}'",
                        source_root.display(),
                        active.display()
                    ));
                }
                Err(e) => {
                    removal_refused = true;
                    warnings.push(format!(
                        "Refused to remove legacy source files: could not check active folders: {e}"
                    ));
                }
                Ok(None) => {
                    for src in &verified_removable {
                        let _ = queue.forget(&source_root, src).await;
                        match tokio::fs::remove_file(src).await {
                            Ok(()) => {
                                removed_source_count += 1;
                            }
                            Err(e) => {
                                warnings.push(format!(
                                    "Could not remove legacy file {}: {e}",
                                    src.display()
                                ));
                            }
                        }
                    }
                    if removed_source_count > 0 {
                        if let Ok(own_navi) = navidrome.navidrome_of(&actor_uid).await {
                            let _ = trigger_and_wait_scan(
                                guard.db,
                                navidrome,
                                &actor_uid,
                                &own_navi,
                                Stage::Share,
                                SCAN_WAIT_MAX,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    if failed_count == 0 && !removal_refused {
        if let Err(e) = LegacyFolder::mark_path_migrated_in(guard.db, &req.legacy_path).await {
            warn!("Could not mark legacy path '{}' migrated: {e}", req.legacy_path);
        }
        info!(
            "Migrated legacy folder '{}' ({} linked, {} already present, {} source removed)",
            req.legacy_path, linked_count, already_present_count, removed_source_count
        );
    }

    Ok(MigrateLegacyReport {
        legacy_path: req.legacy_path,
        dry_run: false,
        total_audio_files: audio_files.len(),
        target_users,
        linked_count,
        already_present_count,
        failed_count,
        removed_source_count,
        hardlink_count,
        reflink_count,
        copy_count,
        annotations_restored,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::jobs::runner::Acquire;
    use crate::test_navidrome::NavidromeFor;

    fn admin_ctx() -> UserCtx {
        UserCtx {
            user_id: "alice".into(),
            username: "alice".into(),
            role: Role::Admin,
        }
    }

    async fn insert_legacy(
        db: &SqlitePool,
        id: &str,
        user_id: &str,
        legacy_path: &Path,
        target_folder_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO legacy_folders (id, user_id, name, path, was_shared, target_folder_id) \
             VALUES (?, ?, 'Legacy', ?, 1, ?)",
        )
        .bind(id)
        .bind(user_id)
        .bind(legacy_path.to_str().unwrap())
        .bind(target_folder_id)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn refuses_removing_source_when_legacy_path_overlaps_active_user_folder() {
        let lib = Library::new().await;
        let (acq, _effects) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| crate::services::NavidromeLookupError::NotLinked));
        let guard = Guard::new(&lib.db, lib.fake.as_ref(), None);

        // Alice's active folder is lib.root/music/alice (created by Library::new).
        // Record a bogus legacy row pointing at Alice's live folder with Bob as recipient!
        let alice_dir = lib.root.join("music/alice");
        let bob_folder_id: String =
            sqlx::query_scalar("SELECT id FROM folders WHERE user_id = 'bob'")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        insert_legacy(&lib.db, "leg-overlap", "bob", &alice_dir, &bob_folder_id).await;

        let report = migrate_legacy_folder(
            &guard,
            &navidrome,
            &acq.queue,
            &admin_ctx(),
            MigrateLegacyRequest {
                legacy_path: alice_dir.to_string_lossy().into_owned(),
                remove_source_after: true,
                dry_run: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(report.linked_count, 1);
        assert_eq!(report.removed_source_count, 0);
        assert!(
            report.warnings.iter().any(|w| w.contains("overlaps active library folder")),
            "expected active-folder overlap warning, got: {:?}",
            report.warnings
        );
        // Alice's live file MUST still exist on disk!
        assert!(alice_dir.join("A/x.flac").exists());
    }

    #[tokio::test]
    async fn refuses_removing_source_when_already_present_target_has_same_size_but_different_bytes() {
        let lib = Library::new().await;
        let (acq, _effects) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| crate::services::NavidromeLookupError::NotLinked));
        let guard = Guard::new(&lib.db, lib.fake.as_ref(), None);

        let shared_dir = lib.root.join("music/shared_legacy");
        let bob_dir = lib.root.join("music/bob");
        std::fs::create_dir_all(shared_dir.join("Artist")).unwrap();
        std::fs::create_dir_all(bob_dir.join("Artist")).unwrap();
        // Same byte length (12 bytes) but completely different audio content!
        std::fs::write(shared_dir.join("Artist/01.flac"), b"ORIGINAL_12B").unwrap();
        std::fs::write(bob_dir.join("Artist/01.flac"), b"DIFFERENT_12").unwrap();

        let bob_folder_id: String =
            sqlx::query_scalar("SELECT id FROM folders WHERE user_id = 'bob'")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        insert_legacy(&lib.db, "leg-diff", "bob", &shared_dir, &bob_folder_id).await;

        let report = migrate_legacy_folder(
            &guard,
            &navidrome,
            &acq.queue,
            &admin_ctx(),
            MigrateLegacyRequest {
                legacy_path: shared_dir.to_string_lossy().into_owned(),
                remove_source_after: true,
                dry_run: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(report.already_present_count, 1);
        assert_eq!(report.removed_source_count, 0);
        assert!(
            shared_dir.join("Artist/01.flac").exists(),
            "source file must NOT be removed when target has different bytes"
        );
    }

    #[tokio::test]
    async fn removes_source_safely_when_inode_or_sha256_matches() {
        let lib = Library::new().await;
        let (acq, _effects) = Acquire::for_test(lib.db.clone());
        let navidrome = NavidromeFor(Err(|| crate::services::NavidromeLookupError::NotLinked));
        let guard = Guard::new(&lib.db, lib.fake.as_ref(), None);

        let shared_dir = lib.root.join("music/shared_ok");
        let bob_dir = lib.root.join("music/bob");
        std::fs::create_dir_all(shared_dir.join("Artist")).unwrap();
        std::fs::create_dir_all(bob_dir.join("Artist")).unwrap();
        // Track 1: will be freshly hardlinked (verified via inode)
        std::fs::write(shared_dir.join("Artist/01.flac"), b"TRACK_ONE_DATA").unwrap();
        // Track 2: already present at target as a separate copy with identical SHA-256
        std::fs::write(shared_dir.join("Artist/02.flac"), b"TRACK_TWO_DATA").unwrap();
        std::fs::write(bob_dir.join("Artist/02.flac"), b"TRACK_TWO_DATA").unwrap();

        let bob_folder_id: String =
            sqlx::query_scalar("SELECT id FROM folders WHERE user_id = 'bob'")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        insert_legacy(&lib.db, "leg-ok", "bob", &shared_dir, &bob_folder_id).await;

        let report = migrate_legacy_folder(
            &guard,
            &navidrome,
            &acq.queue,
            &admin_ctx(),
            MigrateLegacyRequest {
                legacy_path: shared_dir.to_string_lossy().into_owned(),
                remove_source_after: true,
                dry_run: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(report.linked_count, 1);
        assert_eq!(report.already_present_count, 1);
        assert_eq!(report.removed_source_count, 2);
        assert!(!shared_dir.join("Artist/01.flac").exists());
        assert!(!shared_dir.join("Artist/02.flac").exists());
        assert_eq!(
            std::fs::read(bob_dir.join("Artist/01.flac")).unwrap(),
            b"TRACK_ONE_DATA"
        );
        assert_eq!(
            std::fs::read(bob_dir.join("Artist/02.flac")).unwrap(),
            b"TRACK_TWO_DATA"
        );
    }
}
