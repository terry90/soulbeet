//! A user's single personal library folder and automatic provisioning.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::db::DB;
use crate::folder_policy::{canonical_folder_path, check_folder_path, PolicyRows};
use crate::models::folder::{Folder, OwnedFolder};
use crate::models::folder_root::FolderRoot;
use crate::models::user::User;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use sqlx::SqlitePool;
use std::path::Path;
use tracing::{info, warn};

pub fn folder_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Folder not found",
    )
}

pub fn no_library_folder() -> Failure {
    Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "This user has no library folder configured",
    )
    .with_user_action(UserAction::new(
        UserActionCode::AddFolder,
        "Set your library folder",
        "Settings > Library",
    ))
}

/// Clear, actionable refusal when a caller tries to access another user's folder
/// without effective admin library access.
pub(crate) fn admin_access_required(
    ctx: &UserCtx,
    allow_delete: bool,
    stage: Stage,
    for_write: bool,
) -> Failure {
    let message = if !ctx.role.is_admin() {
        if for_write && !allow_delete {
            "Modifying tracks in another user's library requires an Admin Soulbeet account (assign Admin role in Settings > Users), plus 'Admin library access (all users' folders)' and 'Allow managing library files' enabled for this app in Settings > Users > Connected apps"
        } else {
            "Accessing another user's library requires an Admin Soulbeet account (assign Admin role in Settings > Users) and 'Admin library access (all users' folders)' enabled for this app in Settings > Users > Connected apps"
        }
    } else if for_write && !allow_delete {
        "Modifying tracks in another user's library requires both 'Admin library access (all users' folders)' and 'Allow managing library files' enabled for this app in Settings > Users > Connected apps"
    } else {
        "Accessing another user's library requires 'Admin library access (all users' folders)' enabled for this app in Settings > Users > Connected apps"
    };
    Failure::new(
        FailureCode::PermissionNotGranted,
        stage,
        RetryAdvice::NeedsUser,
        message,
    )
}

/// The folder, when `ctx` owns it. Another user's folder is reported as
/// missing, the same as one that does not exist, so an id leaks nothing.
pub(crate) fn ensure_owner(folder: Option<Folder>, ctx: &UserCtx) -> Result<Folder, Failure> {
    folder
        .filter(|f| f.user_id == ctx.user_id)
        .ok_or_else(folder_not_found)
}

/// Returns the user's single personal library folder (or `FolderRequired` if none exists yet).
pub async fn for_user(user_id: &str) -> Result<Folder, Failure> {
    for_user_in(&DB, user_id).await
}

pub(crate) async fn for_user_in(db: &SqlitePool, user_id: &str) -> Result<Folder, Failure> {
    Folder::get_for_user_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(no_library_folder)
}

pub async fn list(ctx: &UserCtx) -> Result<Vec<Folder>, Failure> {
    list_in(&DB, ctx).await
}

pub(crate) async fn list_in(db: &SqlitePool, ctx: &UserCtx) -> Result<Vec<Folder>, Failure> {
    Folder::get_all_by_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Sanitizes a username into a safe single directory component.
pub(crate) fn sanitize_folder_segment(username: &str) -> String {
    let cleaned: String = username
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "user".to_string()
    } else {
        cleaned.replace("..", "_")
    }
}

/// Resolves the directory to seed into `folder_roots`: uses `configured`
/// (falling back to `/music`), stripping a trailing `_default` segment so
/// pointing `NAVIDROME_MUSIC_PATH` at `/data/music/_default` still seeds `/data/music`.
pub(crate) fn effective_seed_root(configured: Option<&Path>) -> &Path {
    let root = configured.unwrap_or_else(|| Path::new("/music"));
    if root.file_name() == Some(std::ffi::OsStr::new("_default")) {
        if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
            return parent;
        }
    }
    root
}

/// Best-effort creation of the empty `_default` directory Navidrome uses for
/// Library 1 (`ND_MUSICFOLDER=/music/_default`), so Navidrome does not log a
/// missing-folder warning on a fresh start.
pub(crate) async fn ensure_navidrome_default_dir_at(
    configured: Option<&Path>,
    navidrome_configured: bool,
) {
    let explicit_default = configured
        .filter(|p| p.file_name() == Some(std::ffi::OsStr::new("_default")))
        .map(Path::to_path_buf);
    let target = if let Some(path) = explicit_default {
        let parent_ok = path.parent().is_some_and(Path::is_dir);
        parent_ok.then_some(path)
    } else if navidrome_configured {
        let root = effective_seed_root(configured);
        root.is_dir().then(|| root.join("_default"))
    } else {
        None
    };

    if let Some(dir) = target {
        if !dir.exists() {
            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                warn!(
                    "Could not create Navidrome default placeholder directory {}: {e}",
                    dir.display()
                );
            }
        }
    }
}

/// Seeds a default folder root (`/music` or `NAVIDROME_MUSIC_PATH`) if no roots
/// are approved yet and the directory exists on disk.
pub(crate) async fn seed_default_root_in(db: &SqlitePool) {
    ensure_navidrome_default_dir_at(
        crate::config::CONFIG.navidrome_music_path(),
        crate::config::CONFIG.navidrome_url().is_some(),
    )
    .await;

    let existing = match FolderRoot::get_all_in(db).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Could not inspect folder_roots: {e}");
            return;
        }
    };
    if !existing.is_empty() {
        return;
    }
    let default_root = effective_seed_root(crate::config::CONFIG.navidrome_music_path());
    if default_root.is_dir() {
        if let Ok(canon) = canonical_folder_path(default_root) {
            if let Some(s) = canon.to_str() {
                if let Err(e) = FolderRoot::add_in(db, s).await {
                    warn!("Could not seed default folder root {s}: {e}");
                } else {
                    info!("Seeded default approved music root: {s}");
                }
            }
        }
    }
}

/// Ensures `user` has their single personal library folder at `<root>/<username>`
/// (creating the directory on disk if needed). When a new folder is provisioned,
/// spawns a best-effort Navidrome library sync in the background.
pub async fn ensure_user_folder(user: &User) -> Option<Folder> {
    let (folder, created) = ensure_user_folder_tracked_in(&DB, user).await?;
    if created {
        let uid = user.id.clone();
        tokio::spawn(async move {
            crate::core::navidrome_libraries::sync_single_user_library_best_effort(&uid).await;
        });
    }
    Some(folder)
}

pub(crate) async fn ensure_user_folder_in(db: &SqlitePool, user: &User) -> Option<Folder> {
    ensure_user_folder_tracked_in(db, user)
        .await
        .map(|(folder, _)| folder)
}

async fn try_provision_candidate(
    db: &SqlitePool,
    user: &User,
    actor: &UserCtx,
    candidate: &Path,
) -> Option<Result<Folder, String>> {
    let rows = PolicyRows::load_in(db).await.ok()?;
    let (policy_folders, policy_roots) = rows.inputs();
    if check_folder_path(candidate, actor, &policy_folders, &policy_roots).is_err() {
        return None;
    }
    if let Err(e) = tokio::fs::create_dir_all(candidate).await {
        warn!(
            "Could not create auto-provisioned library directory {}: {e}",
            candidate.display()
        );
        return None;
    }
    let canonical = canonical_folder_path(candidate).ok()?;
    if canonical != candidate
        || check_folder_path(&canonical, actor, &policy_folders, &policy_roots).is_err()
    {
        warn!(
            "Auto-provisioned library path changed or failed policy re-check for '{}': {} -> {}",
            user.username,
            candidate.display(),
            canonical.display()
        );
        return None;
    }
    let path_str = canonical.to_str()?;
    Some(Folder::create_in(db, &user.id, &user.username, path_str).await)
}

async fn ensure_user_folder_tracked_in(db: &SqlitePool, user: &User) -> Option<(Folder, bool)> {
    if let Ok(Some(existing)) = Folder::get_for_user_in(db, &user.id).await {
        let _ = tokio::fs::create_dir_all(&existing.path).await;
        return Some((existing, false));
    }

    let roots = FolderRoot::get_all_in(db).await.ok()?;
    let root = roots.first()?;
    let root_path = canonical_folder_path(Path::new(&root.path)).ok()?;
    let segment = sanitize_folder_segment(&user.username);
    let short_id: String = user.id.chars().take(8).collect();
    let fallback_candidate = root_path.join(format!("{segment}-{short_id}"));
    let primary_candidate = root_path.join(&segment);

    let actor = UserCtx {
        user_id: user.id.clone(),
        username: user.username.clone(),
        role: user.role,
    };

    let candidates = [primary_candidate, fallback_candidate];
    for candidate in &candidates {
        match try_provision_candidate(db, user, &actor, candidate).await {
            Some(Ok(folder)) => {
                info!(
                    "Auto-provisioned personal library folder '{}' for '{}'",
                    folder.path, user.username
                );
                return Some((folder, true));
            }
            Some(Err(e)) => {
                // Concurrent creation for the same user may have succeeded; re-read first.
                if let Ok(Some(existing)) = Folder::get_for_user_in(db, &user.id).await {
                    return Some((existing, false));
                }
                warn!(
                    "Could not store auto-provisioned folder '{}' for '{}': {e}",
                    candidate.display(),
                    user.username
                );
                // Otherwise another user raced for this path; try the suffixed fallback candidate.
            }
            None => {}
        }
    }

    warn!(
        "Could not auto-provision non-overlapping library folder for '{}'",
        user.username
    );
    None
}

/// Startup hook: seeds default `/music` root if empty and ensures every user has
/// their personal library folder on disk & in DB (fast local DB/fs only).
/// Navidrome library synchronization runs separately in a background task so
/// HTTP server startup never blocks on Navidrome.
pub async fn ensure_all_user_folders_on_startup() {
    seed_default_root_in(&DB).await;
    match User::get_all_in(&DB).await {
        Ok(users) => {
            for user in &users {
                ensure_user_folder_in(&DB, user).await;
            }
        }
        Err(e) => warn!("Could not load users for startup folder provisioning: {e}"),
    }
}

/// Every registered library folder across all users, with owner usernames.
pub(crate) async fn list_all_with_owners_in(db: &SqlitePool) -> Result<Vec<OwnedFolder>, Failure> {
    Folder::get_all_with_owners_in(db)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Any registered folder by id, regardless of owner (for admin-authorized flows).
pub(crate) async fn get_in(db: &SqlitePool, folder_id: &str) -> Result<Folder, Failure> {
    Folder::get_by_id_in(db, folder_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(folder_not_found)
}

/// The folder `folder_id` names, when `ctx` owns it.
pub async fn owned(ctx: &UserCtx, folder_id: &str) -> Result<Folder, Failure> {
    owned_in(&DB, ctx, folder_id).await
}

pub(crate) async fn owned_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    folder_id: &str,
) -> Result<Folder, Failure> {
    let folder = Folder::get_by_id_in(db, folder_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    ensure_owner(folder, ctx)
}

/// Stores a new name and path for `ctx`'s folder `folder_id`, a path the
/// folder rules allowed.
pub async fn update(ctx: &UserCtx, folder_id: &str, name: &str, path: &str) -> Result<(), Failure> {
    update_in(&DB, ctx, folder_id, name, path).await
}

async fn update_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    folder_id: &str,
    name: &str,
    path: &str,
) -> Result<(), Failure> {
    owned_in(db, ctx, folder_id).await?;
    Folder::update_in(db, folder_id, name, path)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Deletes `ctx`'s folder `folder_id`. Only the entry goes; its files stay
/// on disk.
pub async fn delete(ctx: &UserCtx, folder_id: &str) -> Result<(), Failure> {
    delete_in(&DB, ctx, folder_id).await
}

async fn delete_in(db: &SqlitePool, ctx: &UserCtx, folder_id: &str) -> Result<(), Failure> {
    owned_in(db, ctx, folder_id).await?;
    Folder::delete_in(db, folder_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// `ctx`'s own folder at `path`.
pub async fn owned_at(ctx: &UserCtx, path: &str) -> Result<Folder, Failure> {
    Folder::find_for_user_by_path(&ctx.user_id, path)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(folder_not_found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;

    #[test]
    fn another_users_folder_reads_like_a_missing_one() {
        let folder = |id: &str, user_id: &str| Folder {
            id: id.into(),
            user_id: user_id.into(),
            name: "Music".into(),
            path: "/music".into(),
        };
        let alice = test_ctx("user-a");
        assert_eq!(
            ensure_owner(Some(folder("f1", "user-b")), &alice),
            Err(folder_not_found())
        );
        assert_eq!(ensure_owner(None, &alice), Err(folder_not_found()));
        assert_eq!(
            ensure_owner(Some(folder("f1", "user-a")), &alice),
            Ok(folder("f1", "user-a"))
        );
        let f = folder_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Folder not found"
            )
        );
    }

    #[tokio::test]
    async fn a_folder_id_reads_the_same_missing_or_another_users() {
        let db = crate::db::test_db::migrated().await;
        let alice_folder = crate::core::folder_of(&db, "alice").await;
        let bob_folder = crate::core::folder_of(&db, "bob").await;
        let bob = test_ctx("bob");
        assert_eq!(
            owned_in(&db, &bob, &alice_folder).await,
            Err(folder_not_found())
        );
        assert_eq!(
            owned_in(&db, &bob, "no-such-folder").await,
            Err(folder_not_found())
        );
        assert_eq!(
            owned_in(&db, &bob, &bob_folder).await.map(|f| f.id),
            Ok(bob_folder.clone())
        );
    }

    #[tokio::test]
    async fn only_the_owner_deletes_a_folder() {
        let db = crate::db::test_db::migrated().await;
        let alice_folder = crate::core::folder_of(&db, "alice").await;
        let alice = test_ctx("alice");
        assert_eq!(
            delete_in(&db, &test_ctx("bob"), &alice_folder).await,
            Err(folder_not_found())
        );
        assert!(owned_in(&db, &alice, &alice_folder).await.is_ok());
        assert_eq!(delete_in(&db, &alice, &alice_folder).await, Ok(()));
        assert_eq!(
            owned_in(&db, &alice, &alice_folder).await,
            Err(folder_not_found())
        );
    }

    #[tokio::test]
    async fn only_the_owner_updates_a_folder() {
        let db = crate::db::test_db::migrated().await;
        let alice_folder = crate::core::folder_of(&db, "alice").await;
        let alice = test_ctx("alice");
        assert_eq!(
            update_in(&db, &test_ctx("bob"), &alice_folder, "Bob's", "/music/bob").await,
            Err(folder_not_found())
        );
        let stored = owned_in(&db, &alice, &alice_folder).await.unwrap();
        assert_eq!(stored.name, "f");
        assert_eq!(
            update_in(&db, &alice, &alice_folder, "Music", "/music/alice").await,
            Ok(())
        );
        let stored = owned_in(&db, &alice, &alice_folder).await.unwrap();
        assert_eq!(stored.name, "Music");
    }

    #[tokio::test]
    async fn a_folder_write_the_database_refuses_is_a_database_failure() {
        let db = crate::db::test_db::migrated().await;
        let id = crate::core::folder_of(&db, "alice").await;
        let alice = test_ctx("alice");
        db.close().await;
        let closed = sqlx::Error::PoolClosed.to_string();

        let renamed = update_in(&db, &alice, &id, "Music", "/music/alice")
            .await
            .unwrap_err();
        assert_eq!(renamed, database_failure(Stage::Input, &closed));
        let deleted = delete_in(&db, &alice, &id).await.unwrap_err();
        assert_eq!(deleted, database_failure(Stage::Input, &closed));
    }

    #[test]
    fn effective_seed_root_strips_trailing_default_directory() {
        assert_eq!(effective_seed_root(None), Path::new("/music"));
        assert_eq!(
            effective_seed_root(Some(Path::new("/data/music"))),
            Path::new("/data/music")
        );
        assert_eq!(
            effective_seed_root(Some(Path::new("/data/music/_default"))),
            Path::new("/data/music")
        );
    }

    #[tokio::test]
    async fn ensure_navidrome_default_dir_creates_placeholder_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("_default");
        ensure_navidrome_default_dir_at(Some(&explicit), false).await;
        assert!(explicit.is_dir());

        let dir2 = tempfile::tempdir().unwrap();
        ensure_navidrome_default_dir_at(Some(dir2.path()), true).await;
        assert!(dir2.path().join("_default").is_dir());

        // When Navidrome is not configured and path does not end with _default, do not create it.
        let dir3 = tempfile::tempdir().unwrap();
        ensure_navidrome_default_dir_at(Some(dir3.path()), false).await;
        assert!(!dir3.path().join("_default").exists());
    }
}
