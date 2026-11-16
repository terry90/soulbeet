//! What a connected app may delete: only through a grant of the user's own with deletion on
//! (D5), and at most DAILY_APP_DELETIONS tracks a day per user, all apps together (D4). Both are
//! read on the connection of the transaction that writes, never before it.

use crate::auth::UserId;
use crate::core::database_failure;
use crate::oauth::grants::grant_app_in;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::DAILY_APP_DELETIONS;
use sqlx::SqliteConnection;

/// The window the daily room counts, in seconds.
pub(crate) const DAY_SECS: i64 = 86_400;

/// Design 11.5: how a person lets this app delete music (step 7 D7: the checkbox "Allow deleting
/// music").
pub(crate) fn enable_deletion() -> UserAction {
    UserAction::new(
        UserActionCode::EnableDeletion,
        "Allow deleting music for this app",
        "Settings > Users > Connected apps",
    )
}

/// The app may not delete: its switch is off, or its grant is gone or not the caller's.
pub(crate) fn permission_not_granted() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Delete,
        RetryAdvice::NeedsUser,
        "Deleting music is turned off for this app",
    )
    .with_user_action(enable_deletion())
}

/// D4: `user`'s audit rows of origin mcp created after `now - DAY_SECS`, every app's. Ok(used)
/// when `wanted` more fit DAILY_APP_DELETIONS, else daily_limit with the time the
/// (used + wanted - limit)-th oldest of those rows leaves the window. Callers want at most the
/// limit (one file, or one token's songs).
pub(crate) async fn room_in(
    conn: &mut SqliteConnection,
    user: &UserId,
    wanted: u32,
    now: i64,
) -> Result<u32, Failure> {
    let counted: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(strftime('%s', created_at) AS INTEGER) FROM deletion_reviews \
         WHERE user_id = ? AND origin = 'mcp' AND created_at > datetime(?, 'unixepoch') \
         ORDER BY created_at",
    )
    .bind(user.to_string())
    .bind(now - DAY_SECS)
    .fetch_all(conn)
    .await
    .map_err(|e| database_failure(Stage::Delete, e))?;
    let used = counted.len() as u32;
    if used + wanted <= DAILY_APP_DELETIONS {
        return Ok(used);
    }
    let over = (used + wanted - DAILY_APP_DELETIONS) as usize;
    // Only a request over the limit on its own has no such row: room for it never frees, and the
    // window empties when its newest row leaves.
    let leaves = counted
        .get(over - 1)
        .or(counted.last())
        .map_or(now, |created| *created);
    Err(daily_limit(used, wanted, leaves + DAY_SECS, now))
}

/// The day's room is used up: `used` app deletions in the last 24 hours, and room for `wanted`
/// more at `resets_at`.
pub(crate) fn daily_limit(used: u32, wanted: u32, resets_at: i64, now: i64) -> Failure {
    let at = chrono::DateTime::from_timestamp(resets_at, 0).map_or_else(
        || format!("{resets_at} seconds after 1970-01-01 00:00:00 UTC"),
        |at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    );
    Failure::new(
        FailureCode::DeleteDailyLimit,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        format!("At most {DAILY_APP_DELETIONS} tracks a day can be deleted through connected apps, and {used} were deleted in the last 24 hours. {wanted} more can be deleted from {at}."),
    )
    .with_retry_after((resets_at - now).clamp(1, i64::from(u32::MAX)) as u32)
}

/// D5: the host of grant `grant_id`'s app when the grant is `user`'s and allows deletion, else
/// permission_not_granted (no such grant: revoked, or replaced by a new authorization).
pub(crate) async fn deleting_app_in(
    conn: &mut SqliteConnection,
    user: &UserId,
    grant_id: &str,
) -> Result<String, Failure> {
    let grant = grant_app_in(conn, grant_id)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?;
    match grant {
        Some((owner, true, host)) if owner == user.to_string() => Ok(host),
        _ => Err(permission_not_granted()),
    }
}
