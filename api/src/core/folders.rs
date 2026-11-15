//! A user's library folders.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::db::DB;
use crate::models::folder::Folder;
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

/// The folder, when `ctx` owns it. Another user's folder is reported as
/// missing, the same as one that does not exist, so an id leaks nothing.
pub(crate) fn ensure_owner(folder: Option<Folder>, ctx: &UserCtx) -> Result<Folder, Failure> {
    folder
        .filter(|f| f.user_id == ctx.user_id)
        .ok_or_else(folder_not_found)
}

pub async fn list(ctx: &UserCtx) -> Result<Vec<Folder>, Failure> {
    Folder::get_all_by_user(&ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
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

/// Stores a new name and path for `folder`, one `owned` gave and whose path
/// the folder rules allowed.
pub async fn update(folder: &Folder, name: &str, path: &str) -> Result<(), Failure> {
    update_in(&DB, folder, name, path).await
}

async fn update_in(
    db: &SqlitePool,
    folder: &Folder,
    name: &str,
    path: &str,
) -> Result<(), Failure> {
    Folder::update_in(db, &folder.id, name, path)
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
    async fn a_folder_write_the_database_refuses_is_a_database_failure() {
        let db = crate::db::test_db::migrated().await;
        let id = crate::core::folder_of(&db, "alice").await;
        let alice = test_ctx("alice");
        let folder = owned_in(&db, &alice, &id).await.unwrap();
        db.close().await;
        let closed = sqlx::Error::PoolClosed.to_string();

        let renamed = update_in(&db, &folder, "Music", "/music/alice")
            .await
            .unwrap_err();
        assert_eq!(renamed, database_failure(Stage::Input, &closed));
        let deleted = delete_in(&db, &alice, &id).await.unwrap_err();
        assert_eq!(deleted, database_failure(Stage::Input, &closed));
    }
}
