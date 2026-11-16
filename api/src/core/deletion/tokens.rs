//! Confirmation tokens (D7): what a preview allowed deleting, bound to the user, the app (its
//! OAuth grant) and the reason, stored as the SHA-256 of a secret only the preview's answer
//! carries, valid TOKEN_SECS and once.

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::DeletionReason;
use sqlx::{SqliteConnection, SqlitePool};

use super::{App, IssuedToken, TOKEN_SECS};
use crate::core::delete_guard::quota::{deleting_app_in, room_in, DAY_SECS};
use crate::core::delete_guard::PreviewedFile;
use crate::core::{database_failure, stored_time, utc_text};
use crate::oauth::secrets::{hash, new_secret};

/// Where a take stopped (D7); the tools choose `next` from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenRefusalKind {
    /// No such token for this user and app.
    NotFound,
    Used,
    Expired,
    /// The token is for another reason than the call needs.
    WrongReason,
    /// The app may not delete now.
    Permission,
    /// The day's room cannot hold the token's files.
    Room,
    Database,
}

/// Why a token could not be taken. `song_ids` and `reason` are the token's, only when it is the
/// caller's (Used, Expired, WrongReason, Permission, Room); empty and None for NotFound and
/// Database.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeleteTokenRefusal {
    pub kind: TokenRefusalKind,
    pub failure: Failure,
    pub song_ids: Vec<String>,
    pub reason: Option<DeletionReason>,
}

impl DeleteTokenRefusal {
    /// A refusal that says nothing of the token: NotFound and Database.
    fn bare(kind: TokenRefusalKind, failure: Failure) -> DeleteTokenRefusal {
        DeleteTokenRefusal {
            kind,
            failure,
            song_ids: vec![],
            reason: None,
        }
    }
}

/// What a taken token allows deleting.
#[derive(Debug)]
pub(crate) struct Taken {
    pub reason: DeletionReason,
    pub files: Vec<PreviewedFile>,
}

/// Stores a new token for `files`, valid TOKEN_SECS from `now`, in one BEGIN IMMEDIATE
/// transaction that first checks the grant (deleting_app_in) and the day's room for every file
/// (room_in): either refused is its failure, and nothing is stored.
pub(crate) async fn issue_in(
    db: &SqlitePool,
    app: &App,
    reason: DeletionReason,
    files: &[PreviewedFile],
    now: i64,
) -> Result<IssuedToken, Failure> {
    let unavailable = |e: sqlx::Error| database_failure(Stage::Delete, e);
    let stored = serde_json::to_string(files).map_err(|e| database_failure(Stage::Delete, e))?;
    let mut tx = db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(unavailable)?;
    deleting_app_in(&mut tx, &app.user, &app.grant_id).await?;
    room_in(&mut tx, &app.user, files.len() as u32, now).await?;
    let secret = new_secret();
    let expires_at = now + TOKEN_SECS;
    let shown = chrono::DateTime::from_timestamp(expires_at, 0)
        .expect("now_unix reads the clock, ten minutes from which chrono can place");
    sqlx::query(
        "INSERT INTO delete_tokens (token_hash, user_id, grant_id, reason, files, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(hash(&secret))
    .bind(app.user.to_string())
    .bind(&app.grant_id)
    .bind(reason.as_str())
    .bind(stored)
    .bind(now)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(unavailable)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(IssuedToken {
        secret,
        expires_at: shown,
        song_ids: files.iter().map(|f| f.song_id.clone()).collect(),
    })
}

/// A database error as a refusal that says nothing of the token.
fn unavailable(e: sqlx::Error) -> DeleteTokenRefusal {
    DeleteTokenRefusal::bare(
        TokenRefusalKind::Database,
        database_failure(Stage::Delete, e),
    )
}

/// Takes the token `secret` for `app`: D7's checks (classify, with `expect` the reason the call
/// needs, None for any), then `used_at = now`, in one BEGIN IMMEDIATE transaction, so of two
/// takes of one token only one passes. A refused take leaves the token as it was.
pub(crate) async fn take_in(
    db: &SqlitePool,
    app: &App,
    secret: &str,
    expect: Option<DeletionReason>,
    now: i64,
) -> Result<Taken, DeleteTokenRefusal> {
    let mut tx = db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(unavailable)?;
    let taken = classify(&mut tx, app, secret, expect, now).await?;
    sqlx::query("UPDATE delete_tokens SET used_at = ? WHERE token_hash = ?")
        .bind(now)
        .bind(hash(secret))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(taken)
}

/// D7's checks of the token `secret` for `app` (classify, with `expect`) without marking it
/// used: what a token allows, before the call that takes it looks anything up.
pub(crate) async fn peek_in(
    db: &SqlitePool,
    app: &App,
    secret: &str,
    expect: Option<DeletionReason>,
    now: i64,
) -> Result<Taken, DeleteTokenRefusal> {
    let mut conn = db.acquire().await.map_err(unavailable)?;
    classify(&mut conn, app, secret, expect, now).await
}

/// D7's checks of the token `secret` for `app` at `now`, in this order: a token that is not
/// stored, or is another user's or another app's, is NotFound (one answer, so a foreign token
/// tells nothing); then Used, Expired (`expires_at <= now`), WrongReason (a reason other than
/// `expect`, when given), Permission (the grant may not delete, deleting_app_in) and Room (the
/// day's room cannot hold its files, room_in).
async fn classify(
    conn: &mut SqliteConnection,
    app: &App,
    secret: &str,
    expect: Option<DeletionReason>,
    now: i64,
) -> Result<Taken, DeleteTokenRefusal> {
    let database = |e: String| {
        DeleteTokenRefusal::bare(
            TokenRefusalKind::Database,
            database_failure(Stage::Delete, e),
        )
    };
    let row: Option<(String, String, String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, grant_id, reason, files, expires_at, used_at FROM delete_tokens \
         WHERE token_hash = ?",
    )
    .bind(hash(secret))
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| database(e.to_string()))?;
    let callers = |(user_id, grant_id, ..): &(String, String, String, String, i64, Option<i64>)| {
        *user_id == app.user.to_string() && *grant_id == app.grant_id
    };
    let Some((_, _, reason, files, expires_at, used_at)) = row.filter(callers) else {
        return Err(DeleteTokenRefusal::bare(
            TokenRefusalKind::NotFound,
            token_not_found(),
        ));
    };
    let reason = DeletionReason::parse(&reason)
        .ok_or_else(|| database(format!("Unknown deletion reason {reason}")))?;
    let files: Vec<PreviewedFile> =
        serde_json::from_str(&files).map_err(|e| database(e.to_string()))?;
    let refused = |kind, failure| DeleteTokenRefusal {
        kind,
        failure,
        song_ids: files.iter().map(|f| f.song_id.clone()).collect(),
        reason: Some(reason),
    };
    if used_at.is_some() {
        return Err(refused(TokenRefusalKind::Used, token_used()));
    }
    if expires_at <= now {
        let at = stored_time(Stage::Delete, expires_at)
            .map_err(|f| DeleteTokenRefusal::bare(TokenRefusalKind::Database, f))?;
        return Err(refused(TokenRefusalKind::Expired, token_expired(at)));
    }
    if let Some(expected) = expect.filter(|expected| *expected != reason) {
        return Err(refused(
            TokenRefusalKind::WrongReason,
            token_for_another_reason(reason, expected),
        ));
    }
    if let Err(f) = deleting_app_in(conn, &app.user, &app.grant_id).await {
        return Err(match f.code {
            FailureCode::PermissionNotGranted => refused(TokenRefusalKind::Permission, f),
            _ => DeleteTokenRefusal::bare(TokenRefusalKind::Database, f),
        });
    }
    if let Err(f) = room_in(conn, &app.user, files.len() as u32, now).await {
        return Err(match f.code {
            FailureCode::DeleteDailyLimit => refused(TokenRefusalKind::Room, f),
            _ => DeleteTokenRefusal::bare(TokenRefusalKind::Database, f),
        });
    }
    Ok(Taken { reason, files })
}

/// Deletes the tokens that expired more than DAY_SECS before `now`; how many.
pub(crate) async fn prune_in(db: &SqlitePool, now: i64) -> Result<u64, String> {
    sqlx::query("DELETE FROM delete_tokens WHERE expires_at < ?")
        .bind(now - DAY_SECS)
        .execute(db)
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| e.to_string())
}

/// No token by this secret for this user and app.
pub(crate) fn token_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Confirmation token not found",
    )
}

/// The token was taken before.
pub(crate) fn token_used() -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "This confirmation token was already used",
    )
}

/// The token's time ran out at `at`.
pub(crate) fn token_expired(at: chrono::DateTime<chrono::Utc>) -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        format!(
            "This confirmation token expired at {}; preview the songs again",
            utc_text(at)
        ),
    )
}

/// The token is for deleting with `reason`, and the call needs one for `expected`.
pub(crate) fn token_for_another_reason(
    reason: DeletionReason,
    expected: DeletionReason,
) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        format!(
            "This confirmation token is for deleting with reason {}; this needs one with reason {}",
            reason.as_str(),
            expected.as_str()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::core::delete_guard::quota::permission_not_granted;
    use crate::core::delete_guard::test_support::Library;
    use crate::core::delete_guard::Verdict;

    fn alice_app() -> App {
        App {
            user: UserId::new("alice"),
            grant_id: "g1".into(),
        }
    }

    /// s1 as a preview stores it.
    async fn previewed(lib: &Library) -> Vec<PreviewedFile> {
        match lib.guard().check_song(&lib.alice(), "s1").await {
            Ok(Verdict::Library(checked)) => vec![checked.previewed()],
            other => panic!("{other:?}"),
        }
    }

    async fn tokens(lib: &Library) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM delete_tokens")
            .fetch_one(&lib.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_app_that_may_not_delete_gets_no_token() {
        let lib = Library::new().await;
        lib.app("g1", "alice", false).await;
        let files = previewed(&lib).await;
        let now = chrono::Utc::now().timestamp();
        let f = issue_in(&lib.db, &alice_app(), DeletionReason::Cleanup, &files, now)
            .await
            .unwrap_err();
        assert_eq!(f, permission_not_granted());
        assert_eq!(tokens(&lib).await, 0);
    }

    #[tokio::test]
    async fn a_stored_token_that_cannot_be_read_is_refused_without_its_songs() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let files = previewed(&lib).await;
        let now = chrono::Utc::now().timestamp();
        for (column, value) in [
            ("reason", serde_json::json!("because")),
            ("files", serde_json::json!("[")),
            // Past and out of chrono's range: its expiry cannot be told.
            ("expires_at", serde_json::json!(-9_000_000_000_000_000_i64)),
        ] {
            let issued = issue_in(&lib.db, &alice_app(), DeletionReason::Cleanup, &files, now)
                .await
                .unwrap();
            sqlx::query(&format!(
                "UPDATE delete_tokens SET {column} = ? WHERE token_hash = ?"
            ))
            .bind(
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string),
            )
            .bind(hash(&issued.secret))
            .execute(&lib.db)
            .await
            .unwrap();
            let r = take_in(&lib.db, &alice_app(), &issued.secret, None, now)
                .await
                .unwrap_err();
            assert_eq!(
                (r.kind, r.failure.code, r.song_ids.len(), r.reason),
                (
                    TokenRefusalKind::Database,
                    FailureCode::SettingsUnreadable,
                    0,
                    None
                ),
                "{column}"
            );
            let used: Option<i64> =
                sqlx::query_scalar("SELECT used_at FROM delete_tokens WHERE token_hash = ?")
                    .bind(hash(&issued.secret))
                    .fetch_one(&lib.db)
                    .await
                    .unwrap();
            assert_eq!(used, None, "{column}");
        }
    }

    #[tokio::test]
    async fn a_peek_leaves_the_token_unused_and_a_wrong_reason_is_refused() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let files = previewed(&lib).await;
        let now = chrono::Utc::now().timestamp();
        let issued = issue_in(&lib.db, &alice_app(), DeletionReason::Cleanup, &files, now)
            .await
            .unwrap();
        let peeked = peek_in(
            &lib.db,
            &alice_app(),
            &issued.secret,
            Some(DeletionReason::Cleanup),
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            (peeked.reason, peeked.files),
            (DeletionReason::Cleanup, files.clone())
        );
        let wrong = peek_in(
            &lib.db,
            &alice_app(),
            &issued.secret,
            Some(DeletionReason::Replaced),
            now,
        )
        .await
        .unwrap_err();
        assert_eq!(
            wrong,
            DeleteTokenRefusal {
                kind: TokenRefusalKind::WrongReason,
                failure: token_for_another_reason(
                    DeletionReason::Cleanup,
                    DeletionReason::Replaced
                ),
                song_ids: vec!["s1".into()],
                reason: Some(DeletionReason::Cleanup),
            }
        );
        assert_eq!(
            wrong.failure.message,
            "This confirmation token is for deleting with reason cleanup; this needs one with reason replaced"
        );
        let taken_wrong = take_in(
            &lib.db,
            &alice_app(),
            &issued.secret,
            Some(DeletionReason::Replaced),
            now,
        )
        .await
        .unwrap_err();
        assert_eq!(taken_wrong.kind, TokenRefusalKind::WrongReason);
        let taken = take_in(&lib.db, &alice_app(), &issued.secret, None, now)
            .await
            .unwrap();
        assert_eq!(
            taken.files, files,
            "neither the peek nor the refused take used it"
        );
    }

    #[tokio::test]
    async fn old_tokens_are_pruned() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let files = previewed(&lib).await;
        let now = chrono::Utc::now().timestamp();
        for expires_at in [now - DAY_SECS - 1, now - 10] {
            let issued_at = expires_at - TOKEN_SECS;
            issue_in(
                &lib.db,
                &alice_app(),
                DeletionReason::Cleanup,
                &files,
                issued_at,
            )
            .await
            .unwrap();
        }
        assert_eq!(prune_in(&lib.db, now).await, Ok(1));
        let left: Vec<i64> = sqlx::query_scalar("SELECT expires_at FROM delete_tokens")
            .fetch_all(&lib.db)
            .await
            .unwrap();
        assert_eq!(left, vec![now - 10]);
    }

    #[tokio::test]
    async fn a_revoked_app_takes_its_tokens_with_it() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let files = previewed(&lib).await;
        let now = chrono::Utc::now().timestamp();
        issue_in(&lib.db, &alice_app(), DeletionReason::Cleanup, &files, now)
            .await
            .unwrap();
        assert_eq!(tokens(&lib).await, 1);
        sqlx::query("DELETE FROM oauth_grants WHERE id = 'g1'")
            .execute(&lib.db)
            .await
            .unwrap();
        assert_eq!(tokens(&lib).await, 0);
    }
}
