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
#[allow(
    clippy::result_large_err,
    reason = "core functions fail with a Failure by value, as the async ones do"
)]
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
}
