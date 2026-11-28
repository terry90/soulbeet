//! Automatic per-user Navidrome library provisioning and overlap detection.
//!
//! Hardlink track sharing requires per-user Navidrome libraries so a file
//! hardlinked into `/music/alice` and `/music/bob` is indexed once per user
//! rather than duplicated inside a single parent `/music` library.

use crate::auth::UserId;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::db::DB;
use crate::models::folder::Folder;
use crate::models::legacy_folder::LegacyFolder;
use crate::models::navidrome_library::NavidromeLibraryRow;
use crate::models::user::User;
use serde::{Deserialize, Serialize};
use soulbeet::navidrome::models::{NavidromeLibrary, NavidromeUserInfo};
use soulbeet::navidrome::NavidromeClient;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Actionable health warning when Navidrome's default library (ID 1) is an
/// ancestor of user library folders (e.g. Library 1 = `/music` while users live
/// at `/music/alice` and `/music/bob`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeLibraryWarning {
    pub default_library_path: String,
    pub overlapping_user_paths: Vec<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLibrarySyncEntry {
    pub user_id: String,
    pub username: String,
    pub folder_path: String,
    pub navidrome_library_id: Option<i64>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavidromeLibrarySyncReport {
    pub has_admin: bool,
    pub entries: Vec<UserLibrarySyncEntry>,
    pub warning: Option<NavidromeLibraryWarning>,
}

/// Finds the first Navidrome-linked user whose account has Navidrome admin
/// privileges (verified by `GET /api/library` succeeding with 200).
pub(crate) async fn find_admin_client_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
) -> Option<(Arc<NavidromeClient>, Vec<NavidromeLibrary>)> {
    let linked = User::linked_to_navidrome_in(db).await.ok()?;
    for u in linked {
        let uid = UserId::new(&u.id);
        let Ok(client) = navidrome.navidrome_of(&uid).await else {
            continue;
        };
        match client.list_libraries().await {
            Ok(Some(libs)) => return Some((client, libs)),
            Ok(None) => continue,
            Err(e) => {
                warn!("Could not query Navidrome libraries as {}: {e}", u.username);
            }
        }
    }
    None
}

/// Maps a local Soulbeet folder path into Navidrome's container path space.
///
/// Handles all mount-root combinations:
/// - Matching roots (`/data/music` <-> `/music`)
/// - Both ending in `_default` (`/data/music/_default` <-> `/music/_default`)
///   by stripping matching trailing components to find the mount root
/// - Library 1 at `/music/_default` while `NAVIDROME_MUSIC_PATH` is `/data/music`
///   by mapping against Library 1's parent (`/music`) rather than nesting under `_default`
pub(crate) fn map_local_to_navidrome_path(
    local_path: &Path,
    local_music_root: Option<&Path>,
    default_lib_path: Option<&Path>,
) -> String {
    if let (Some(local_root), Some(navi_root)) = (local_music_root, default_lib_path) {
        let mut eff_local = local_root.to_path_buf();
        let mut eff_navi = navi_root.to_path_buf();
        let default_seg = OsStr::new("_default");

        // Strip matching trailing components (such as `_default`, or shared suffixes when
        // `local_path` lives in a sibling directory above `eff_local`) to recover the true
        // container mount-root pair.
        while eff_local.file_name().is_some()
            && eff_local.file_name() == eff_navi.file_name()
            && eff_local.parent().is_some_and(|p| !p.as_os_str().is_empty())
            && eff_navi.parent().is_some_and(|p| !p.as_os_str().is_empty())
        {
            if eff_local.file_name() == Some(default_seg)
                || local_path.strip_prefix(&eff_local).is_err()
            {
                eff_local.pop();
                eff_navi.pop();
            } else {
                break;
            }
        }

        // If Library 1 points to `.../_default` while `NAVIDROME_MUSIC_PATH` points to the
        // parent mount root (`/data/music` vs `/music/_default`), map against Library 1's
        // parent (`/music`).
        if eff_navi.file_name() == Some(default_seg) && eff_local.file_name() != Some(default_seg) {
            if let Some(parent) = eff_navi.parent().filter(|p| !p.as_os_str().is_empty()) {
                eff_navi = parent.to_path_buf();
            }
        }

        if let Ok(rel) = local_path.strip_prefix(&eff_local) {
            let mapped = if rel.as_os_str().is_empty() {
                eff_navi
            } else {
                eff_navi.join(rel)
            };
            if let Some(s) = mapped.to_str() {
                return s.trim_end_matches('/').to_string();
            }
        }
    }
    local_path
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

/// Computes the target Navidrome library IDs for a non-admin user without ever
/// revoking legacy libraries while unmigrated `legacy_folders` exist or revoking
/// custom extra libraries (such as Audiobooks).
///
/// - If the user has NO unmigrated `legacy_folders` and currently only has Library 1
///   (`[1]`), switches `[1]` -> `[personal_lib_id]`.
/// - Otherwise preserves all existing assigned library IDs and ensures `personal_lib_id`
///   is included.
pub(crate) fn desired_user_library_ids(
    current_ids: &[i64],
    personal_lib_id: i64,
    has_unmigrated_legacy: bool,
) -> Vec<i64> {
    if !has_unmigrated_legacy && current_ids == [1] {
        return vec![personal_lib_id];
    }
    let mut next = current_ids.to_vec();
    if !next.contains(&personal_lib_id) {
        next.push(personal_lib_id);
    }
    next
}

/// Detects whether Navidrome Library ID 1 is a parent directory of any user's
/// library path, which would cause duplicate tracks in Navidrome when hardlinking
/// files across users.
pub(crate) fn check_default_library_overlap(
    libraries: &[NavidromeLibrary],
    user_navidrome_paths: &[String],
) -> Option<NavidromeLibraryWarning> {
    let default_lib = libraries.iter().find(|l| l.id == 1)?;
    let default_path = Path::new(default_lib.path.trim_end_matches('/'));
    if default_path.as_os_str().is_empty() {
        return None;
    }
    let overlapping: Vec<String> = user_navidrome_paths
        .iter()
        .filter(|p| {
            let candidate = Path::new(p.trim_end_matches('/'));
            candidate != default_path && candidate.starts_with(default_path)
        })
        .cloned()
        .collect();
    if overlapping.is_empty() {
        return None;
    }
    Some(NavidromeLibraryWarning {
        default_library_path: default_lib.path.clone(),
        overlapping_user_paths: overlapping,
        message: format!(
            "Navidrome's default library (ID 1) points to '{}', which contains user library folders. \
             Point Navidrome's ND_MUSICFOLDER to '{}/_default' (an empty sibling folder) so hardlinked \
             shared tracks do not appear as duplicates in Navidrome.",
            default_lib.path,
            default_lib.path.trim_end_matches('/'),
        ),
    })
}

/// Best-effort provisions and assigns Navidrome per-user libraries for every
/// Soulbeet user who has a configured library folder.
pub async fn sync_all_user_libraries() -> NavidromeLibrarySyncReport {
    sync_all_user_libraries_in(
        &DB,
        &ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
    )
    .await
}

pub(crate) async fn sync_all_user_libraries_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    local_music_root: Option<&Path>,
) -> NavidromeLibrarySyncReport {
    let Some((admin, mut libraries)) = find_admin_client_in(db, navidrome).await else {
        return NavidromeLibrarySyncReport {
            has_admin: false,
            entries: Vec::new(),
            warning: None,
        };
    };

    let (navidrome_users, list_users_err): (Option<Vec<NavidromeUserInfo>>, Option<String>) =
        match admin.list_users_full().await {
            Ok(Some(users)) => (Some(users), None),
            Ok(None) => (
                None,
                Some("Navidrome account is not permitted to list users".to_string()),
            ),
            Err(e) => {
                warn!("Could not list Navidrome users during library sync: {e}");
                (None, Some(e.to_string()))
            }
        };

    let unmigrated_legacy_users: HashSet<String> = match LegacyFolder::list_unmigrated_in(db).await
    {
        Ok(rows) => rows.into_iter().map(|r| r.legacy.user_id).collect(),
        Err(e) => {
            warn!("Could not inspect legacy_folders during library sync: {e}");
            HashSet::new()
        }
    };

    let owned_folders = match Folder::get_all_with_owners_in(db).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Could not load folders for Navidrome library sync: {e}");
            return NavidromeLibrarySyncReport {
                has_admin: true,
                entries: Vec::new(),
                warning: None,
            };
        }
    };

    let default_lib_path: Option<PathBuf> = libraries
        .iter()
        .find(|l| l.id == 1)
        .map(|l| PathBuf::from(&l.path));

    let mut entries = Vec::new();
    let mut mapped_paths = Vec::new();

    for owned in &owned_folders {
        let local_path = Path::new(&owned.folder.path);
        let navi_path =
            map_local_to_navidrome_path(local_path, local_music_root, default_lib_path.as_deref());
        mapped_paths.push(navi_path.clone());

        // Ensure directory exists before asking Navidrome to create a library there.
        let _ = tokio::fs::create_dir_all(local_path).await;

        let existing = libraries
            .iter()
            .find(|l| l.path.trim_end_matches('/') == navi_path.trim_end_matches('/'))
            .cloned();

        let library = match existing {
            Some(lib) => Ok(lib),
            None => {
                let mut lib_name = owned.owner.clone();
                if libraries
                    .iter()
                    .any(|l| l.name.eq_ignore_ascii_case(&lib_name))
                {
                    let short_id = &owned.folder.id[..6.min(owned.folder.id.len())];
                    lib_name = format!("{} ({})", owned.owner, short_id);
                }
                match admin.create_library(&lib_name, &navi_path, false).await {
                    Ok(created) => {
                        info!(
                            "Created Navidrome library {} ('{}') at {}",
                            created.id, created.name, created.path
                        );
                        libraries.push(created.clone());
                        Ok(created)
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };

        let lib = match library {
            Ok(lib) => lib,
            Err(err) => {
                entries.push(UserLibrarySyncEntry {
                    user_id: owned.folder.user_id.clone(),
                    username: owned.owner.clone(),
                    folder_path: owned.folder.path.clone(),
                    navidrome_library_id: None,
                    status: format!("Could not create Navidrome library: {err}"),
                });
                continue;
            }
        };

        // Record mapping in Soulbeet's navidrome_libraries table so delete_guard
        // resolves this library immediately without waiting for heuristic learning.
        if let Ok(canon) = tokio::fs::canonicalize(local_path).await {
            if let Some(canon_str) = canon.to_str() {
                let _ = NavidromeLibraryRow::upsert_in(
                    db,
                    &NavidromeLibraryRow {
                        library_id: lib.id,
                        navidrome_path: lib.path.clone(),
                        local_path: canon_str.to_string(),
                    },
                )
                .await;
            }
        }

        // Assign non-admin Navidrome users to their personal library without
        // revoking legacy libraries (while unmigrated legacy folders remain) or
        // custom extra libraries.
        let status = if let Some(ref err) = list_users_err {
            format!("Library synced, but could not list Navidrome users: {err}")
        } else if let Some(nd_user) = navidrome_users
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|u| u.user_name.eq_ignore_ascii_case(&owned.owner))
        {
            if nd_user.is_admin {
                "Synced (Navidrome admin sees all libraries)".to_string()
            } else {
                let current_ids: Vec<i64> = nd_user.libraries.iter().map(|l| l.id).collect();
                let has_unmigrated_legacy =
                    unmigrated_legacy_users.contains(&owned.folder.user_id);
                let desired_ids =
                    desired_user_library_ids(&current_ids, lib.id, has_unmigrated_legacy);
                if desired_ids == current_ids {
                    "Synced".to_string()
                } else {
                    match admin.set_user_libraries(&nd_user.id, &desired_ids).await {
                        Ok(_) => {
                            info!(
                                "Assigned Navidrome user '{}' to libraries {:?}",
                                nd_user.user_name, desired_ids
                            );
                            "Assigned to user".to_string()
                        }
                        Err(e) => format!("Library created, but user assignment failed: {e}"),
                    }
                }
            }
        } else {
            "Library synced (no matching Navidrome user yet)".to_string()
        };

        entries.push(UserLibrarySyncEntry {
            user_id: owned.folder.user_id.clone(),
            username: owned.owner.clone(),
            folder_path: owned.folder.path.clone(),
            navidrome_library_id: Some(lib.id),
            status,
        });
    }

    let warning = check_default_library_overlap(&libraries, &mapped_paths);
    NavidromeLibrarySyncReport {
        has_admin: true,
        entries,
        warning,
    }
}

/// Checks Navidrome library health (read-only: does not create or mutate libraries).
pub async fn inspect_library_warning() -> Option<NavidromeLibraryWarning> {
    inspect_library_warning_in(
        &DB,
        &ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
    )
    .await
}

pub(crate) async fn inspect_library_warning_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    local_music_root: Option<&Path>,
) -> Option<NavidromeLibraryWarning> {
    let (_, libraries) = find_admin_client_in(db, navidrome).await?;
    let owned_folders = Folder::get_all_with_owners_in(db).await.ok()?;
    let default_lib_path: Option<PathBuf> = libraries
        .iter()
        .find(|l| l.id == 1)
        .map(|l| PathBuf::from(&l.path));
    let mapped_paths: Vec<String> = owned_folders
        .iter()
        .map(|o| {
            map_local_to_navidrome_path(
                Path::new(&o.folder.path),
                local_music_root,
                default_lib_path.as_deref(),
            )
        })
        .collect();
    check_default_library_overlap(&libraries, &mapped_paths)
}

/// Best-effort sync for a single user (called after auto-provisioning or updating
/// a user's folder). Never fails the caller if Navidrome is unconfigured/offline.
pub async fn sync_single_user_library_best_effort(user_id: &str) {
    let report = sync_all_user_libraries().await;
    if let Some(entry) = report.entries.iter().find(|e| e.user_id == user_id) {
        info!(
            "Navidrome library sync for {}: {}",
            entry.username, entry.status
        );
    }
}

/// Spawns per-user Navidrome library synchronization in the background so HTTP
/// server startup never blocks on Navidrome network calls.
pub fn start_startup_sync() {
    tokio::spawn(async {
        let report = sync_all_user_libraries().await;
        if let Some(warning) = report.warning {
            warn!("{}", warning.message);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_warning_fires_when_library_1_contains_user_libraries() {
        let libs = vec![
            NavidromeLibrary {
                id: 1,
                name: "Music Library".into(),
                path: "/music".into(),
                default_new_users: true,
            },
            NavidromeLibrary {
                id: 2,
                name: "alice".into(),
                path: "/music/alice".into(),
                default_new_users: false,
            },
        ];
        let warning = check_default_library_overlap(&libs, &["/music/alice".into()]).unwrap();
        assert_eq!(warning.default_library_path, "/music");
        assert_eq!(warning.overlapping_user_paths, vec!["/music/alice"]);
    }

    #[test]
    fn overlap_warning_is_none_when_library_1_is_sibling_default_dir() {
        let libs = vec![
            NavidromeLibrary {
                id: 1,
                name: "Default".into(),
                path: "/music/_default".into(),
                default_new_users: true,
            },
            NavidromeLibrary {
                id: 2,
                name: "alice".into(),
                path: "/music/alice".into(),
                default_new_users: false,
            },
        ];
        assert!(check_default_library_overlap(&libs, &["/music/alice".into()]).is_none());
    }

    #[test]
    fn map_local_to_navidrome_path_handles_all_mount_combinations() {
        // 1. Standard mount-root remapping (/data/music -> /music)
        assert_eq!(
            map_local_to_navidrome_path(
                Path::new("/data/music/alice"),
                Some(Path::new("/data/music")),
                Some(Path::new("/music")),
            ),
            "/music/alice"
        );
        // 2. Both local_music_root and default_lib_path end in `_default`
        assert_eq!(
            map_local_to_navidrome_path(
                Path::new("/data/music/alice"),
                Some(Path::new("/data/music/_default")),
                Some(Path::new("/music/_default")),
            ),
            "/music/alice"
        );
        // 3. Library 1 ends in `_default` while local_music_root is the mount root
        assert_eq!(
            map_local_to_navidrome_path(
                Path::new("/data/music/alice"),
                Some(Path::new("/data/music")),
                Some(Path::new("/music/_default")),
            ),
            "/music/alice"
        );
        // 4. Same path in both containers (no local_music_root configured)
        assert_eq!(
            map_local_to_navidrome_path(
                Path::new("/music/alice"),
                None,
                Some(Path::new("/music/_default")),
            ),
            "/music/alice"
        );
    }

    #[test]
    fn desired_user_library_ids_preserves_legacy_and_custom_libraries() {
        // Fresh user with only default Library 1 and no unmigrated legacy folders switches to personal library.
        assert_eq!(desired_user_library_ids(&[1], 2, false), vec![2]);
        // User with unmigrated legacy folders keeps Library 1 alongside their new personal library.
        assert_eq!(desired_user_library_ids(&[1], 2, true), vec![1, 2]);
        // User with custom extra libraries (e.g. Audiobooks = 5) keeps them all.
        assert_eq!(desired_user_library_ids(&[1, 5], 2, false), vec![1, 5, 2]);
        // Already assigned personal library + custom library stays unchanged.
        assert_eq!(desired_user_library_ids(&[2, 5], 2, false), vec![2, 5]);
    }
}
