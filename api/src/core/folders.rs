//! A user's library folders.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::db::DB;
use crate::models::folder::{Folder, OwnedFolder};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use sqlx::SqlitePool;

pub fn folder_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Folder not found",
    )
}

/// Clear, actionable refusal when a caller tries to access or move tracks involving another
/// user's folder without effective admin library access.
pub(crate) fn admin_access_required(
    ctx: &UserCtx,
    allow_delete: bool,
    stage: Stage,
    for_move: bool,
) -> Failure {
    let message = if !ctx.role.is_admin() {
        if for_move && !allow_delete {
            "Accessing or moving tracks in another user's folder requires an Admin Soulbeet account (assign Admin role in Settings > Users), plus 'Admin library access (all users' folders)' and 'Allow managing library files (delete, move, re-tag)' enabled for this app in Settings > Users > Connected apps"
        } else {
            "Accessing another user's folder requires an Admin Soulbeet account (assign Admin role in Settings > Users) and 'Admin library access (all users' folders)' enabled for this app in Settings > Users > Connected apps"
        }
    } else if for_move && !allow_delete {
        "Moving tracks in another user's folder requires both 'Admin library access (all users' folders)' and 'Allow managing library files (delete, move, re-tag)' enabled for this app in Settings > Users > Connected apps"
    } else {
        "Accessing another user's folder requires 'Admin library access (all users' folders)' enabled for this app in Settings > Users > Connected apps"
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

pub async fn list(ctx: &UserCtx) -> Result<Vec<Folder>, Failure> {
    list_in(&DB, ctx).await
}

pub(crate) async fn list_in(db: &SqlitePool, ctx: &UserCtx) -> Result<Vec<Folder>, Failure> {
    Folder::get_all_by_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
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
            shared: false,
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
}
