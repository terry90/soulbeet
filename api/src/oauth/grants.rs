//! Grants and the tokens they hold: code exchange (RFC 6749 section 4.1.3,
//! RFC 7636 section 4.6), refresh rotation (section 6), revocation
//! (RFC 7009) and pruning.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::secrets::{hash, new_secret, s256, successor, valid_verifier};
use super::OFFLINE_ACCESS;
use crate::secret_key::SecretKey;

/// How long an access token is accepted, in seconds (D4).
pub(crate) const ACCESS_LIFETIME: i64 = 3600;
/// How long a refresh token can be used, from its issue, in seconds (D4).
pub(crate) const REFRESH_LIFETIME: i64 = 60 * 24 * 3600;
/// How long after its rotation a refresh token still answers its successor
/// pair, in seconds (D4).
pub(crate) const REFRESH_GRACE: i64 = 60;
/// A client with no grant is pruned this long after its last use (its registration when never used), D4.
pub(crate) const UNUSED_CLIENT_LIFETIME: i64 = 90 * 24 * 3600;

/// The answer to a code that cannot be redeemed, and to a code no request
/// of its client names.
pub(crate) const CODE_UNUSABLE: &str = "The code is unknown, expired or already used";
const CODE_MISMATCH: &str = "The code does not match this client, redirect URI or verifier";
const CODE_OTHER_RESOURCE: &str = "resource does not match the authorization request";
/// The answer to a refresh token that is not one of the client's live ones.
pub(crate) const REFRESH_UNKNOWN: &str = "The refresh token is unknown, expired or revoked";
const REFRESH_REUSED: &str = "The refresh token was already used; the connection is ended";
const REFRESH_OTHER_ADDRESS: &str =
    "The refresh token was issued for another address of this server";

/// The tokens a token request answers with.
#[derive(Debug)]
pub(crate) struct Issued {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
    pub scope: String,
}

/// Why a token request is refused: the RFC 6749 section 5.2 error with its
/// description, or the database's failure.
#[derive(Debug, PartialEq)]
pub(crate) enum TokenRefusal {
    InvalidGrant(&'static str),
    InvalidTarget(&'static str),
    Database(String),
}

impl From<sqlx::Error> for TokenRefusal {
    fn from(e: sqlx::Error) -> TokenRefusal {
        TokenRefusal::Database(e.to_string())
    }
}

/// A token request of the `authorization_code` grant.
pub(crate) struct CodeRedemption<'a> {
    pub code: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub verifier: &'a str,
    pub resource: &'a str,
}

type CodeRow = (String, String, String, String, String, String);

/// Redeems a code once, in one BEGIN IMMEDIATE transaction. A code that
/// does not match its client, redirect URI or verifier is used up all the
/// same; a second use of a redeemed code ends the grant it started
/// (design 9.2). The grant replaces any earlier one of the same user, client
/// and resource (D6), and carries a refresh token when its scope has
/// `offline_access` (D5).
pub(crate) async fn redeem_code_in(
    db: &SqlitePool,
    r: &CodeRedemption<'_>,
    now: i64,
) -> Result<Issued, TokenRefusal> {
    let code_hash = hash(r.code);
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<CodeRow> = sqlx::query_as(
        "UPDATE oauth_codes SET used_at = ? \
         WHERE code_hash = ? AND used_at IS NULL AND expires_at > ? \
         RETURNING user_id, client_id, redirect_uri, code_challenge, scope, resource",
    )
    .bind(now)
    .bind(&code_hash)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, client_id, redirect_uri, challenge, scope, resource)) = row else {
        sqlx::query(
            "DELETE FROM oauth_grants WHERE id = \
             (SELECT grant_id FROM oauth_codes WHERE code_hash = ? AND used_at IS NOT NULL)",
        )
        .bind(&code_hash)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Err(TokenRefusal::InvalidGrant(CODE_UNUSABLE));
    };
    if client_id != r.client_id
        || redirect_uri != r.redirect_uri
        || !valid_verifier(r.verifier)
        || s256(r.verifier) != challenge
    {
        tx.commit().await?;
        return Err(TokenRefusal::InvalidGrant(CODE_MISMATCH));
    }
    if resource != r.resource {
        tx.commit().await?;
        return Err(TokenRefusal::InvalidTarget(CODE_OTHER_RESOURCE));
    }
    sqlx::query("DELETE FROM oauth_grants WHERE user_id = ? AND client_id = ? AND resource = ?")
        .bind(&user_id)
        .bind(&client_id)
        .bind(&resource)
        .execute(&mut *tx)
        .await?;
    let grant_id = new_secret();
    sqlx::query(
        "INSERT INTO oauth_grants (id, user_id, client_id, resource, scope, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&grant_id)
    .bind(&user_id)
    .bind(&client_id)
    .bind(&resource)
    .bind(&scope)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let access_token = new_secret();
    insert_token(
        &mut tx,
        &access_token,
        &grant_id,
        "access",
        now + ACCESS_LIFETIME,
    )
    .await?;
    let refresh_token = if scope.split(' ').any(|s| s == OFFLINE_ACCESS) {
        let refresh = new_secret();
        insert_token(
            &mut tx,
            &refresh,
            &grant_id,
            "refresh",
            now + REFRESH_LIFETIME,
        )
        .await?;
        Some(refresh)
    } else {
        None
    };
    sqlx::query("UPDATE oauth_codes SET grant_id = ? WHERE code_hash = ?")
        .bind(&grant_id)
        .bind(&code_hash)
        .execute(&mut *tx)
        .await?;
    mark_client_used(&mut tx, &client_id, now).await?;
    tx.commit().await?;
    Ok(Issued {
        access_token,
        refresh_token,
        expires_in: ACCESS_LIFETIME,
        scope,
    })
}

/// Rotates a refresh token of `client_id` issued for `resource`, in one
/// BEGIN IMMEDIATE transaction, so of two requests at once the second waits
/// and sees the rotation. A repeat within `REFRESH_GRACE` of the rotation
/// answers the same successor pair while that pair is the grant's live one;
/// any other reuse ends the grant (design 9.2).
pub(crate) async fn refresh_in(
    db: &SqlitePool,
    key: &SecretKey,
    refresh: &str,
    client_id: &str,
    resource: &str,
    now: i64,
) -> Result<Issued, TokenRefusal> {
    let refresh_hash = hash(refresh);
    let (access_token, next) = successor(key, refresh);
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let rotated: Option<String> = sqlx::query_scalar(
        "UPDATE oauth_tokens SET rotated_at = ? \
         WHERE token_hash = ? AND kind = 'refresh' AND rotated_at IS NULL AND expires_at > ? \
           AND grant_id IN (SELECT id FROM oauth_grants WHERE client_id = ?) \
         RETURNING grant_id",
    )
    .bind(now)
    .bind(&refresh_hash)
    .bind(now)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(grant_id) = rotated {
        let (granted_resource, scope): (String, String) =
            sqlx::query_as("SELECT resource, scope FROM oauth_grants WHERE id = ?")
                .bind(&grant_id)
                .fetch_one(&mut *tx)
                .await?;
        if granted_resource != resource {
            return Err(TokenRefusal::InvalidTarget(REFRESH_OTHER_ADDRESS));
        }
        insert_token(
            &mut tx,
            &access_token,
            &grant_id,
            "access",
            now + ACCESS_LIFETIME,
        )
        .await?;
        insert_token(&mut tx, &next, &grant_id, "refresh", now + REFRESH_LIFETIME).await?;
        mark_client_used(&mut tx, client_id, now).await?;
        tx.commit().await?;
        return Ok(Issued {
            access_token,
            refresh_token: Some(next),
            expires_in: ACCESS_LIFETIME,
            scope,
        });
    }
    let used: Option<(String, i64, String, String)> = sqlx::query_as(
        "SELECT t.grant_id, t.rotated_at, g.resource, g.scope \
         FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id \
         WHERE t.token_hash = ? AND t.kind = 'refresh' AND t.rotated_at IS NOT NULL \
           AND t.expires_at > ? AND g.client_id = ?",
    )
    .bind(&refresh_hash)
    .bind(now)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((grant_id, rotated_at, granted_resource, scope)) = used else {
        return Err(TokenRefusal::InvalidGrant(REFRESH_UNKNOWN));
    };
    if now - rotated_at <= REFRESH_GRACE {
        // The successor pair, while its refresh token is not rotated in turn.
        let access_expires: Option<i64> = sqlx::query_scalar(
            "SELECT a.expires_at FROM oauth_tokens a JOIN oauth_tokens r ON r.grant_id = a.grant_id \
             WHERE a.grant_id = ? AND a.token_hash = ? AND a.kind = 'access' \
               AND r.token_hash = ? AND r.kind = 'refresh' AND r.rotated_at IS NULL",
        )
        .bind(&grant_id)
        .bind(hash(&access_token))
        .bind(hash(&next))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(expires_at) = access_expires {
            if granted_resource != resource {
                return Err(TokenRefusal::InvalidTarget(REFRESH_OTHER_ADDRESS));
            }
            return Ok(Issued {
                access_token,
                refresh_token: Some(next),
                expires_in: expires_at - now,
                scope,
            });
        }
    }
    sqlx::query("DELETE FROM oauth_grants WHERE id = ?")
        .bind(&grant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Err(TokenRefusal::InvalidGrant(REFRESH_REUSED))
}

/// Whether `code` was issued to `client_id`, redeemed or not. A plain read
/// that keeps junk codes from taking the write lock or the client's budget.
pub(crate) async fn code_of_client_in(
    db: &SqlitePool,
    code: &str,
    client_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oauth_codes WHERE code_hash = ? AND client_id = ?)",
    )
    .bind(hash(code))
    .bind(client_id)
    .fetch_one(db)
    .await
    .map_err(|e| e.to_string())
}

/// The grant of a refresh token of `client_id`, rotated or not; None for anything else. For the per-grant limit.
pub(crate) async fn grant_of_refresh_in(
    db: &SqlitePool,
    refresh: &str,
    client_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar(
        "SELECT t.grant_id FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id \
         WHERE t.token_hash = ? AND t.kind = 'refresh' AND g.client_id = ?",
    )
    .bind(hash(refresh))
    .bind(client_id)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())
}

/// Revokes a token of `client_id` (RFC 7009 section 2): a refresh token ends
/// its grant, an access token only itself (D6). Anything else changes
/// nothing and writes nothing.
pub(crate) async fn revoke_in(db: &SqlitePool, token: &str, client_id: &str) -> Result<(), String> {
    let database = |e: sqlx::Error| e.to_string();
    let token_hash = hash(token);
    let is_refresh: Option<bool> = sqlx::query_scalar(
        "SELECT t.kind = 'refresh' FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id \
         WHERE t.token_hash = ? AND g.client_id = ?",
    )
    .bind(&token_hash)
    .bind(client_id)
    .fetch_optional(db)
    .await
    .map_err(database)?;
    // Each statement repeats the read's conditions, so it acts on what is
    // there when it runs.
    let statement = match is_refresh {
        None => return Ok(()),
        Some(true) => {
            "DELETE FROM oauth_grants WHERE id = \
             (SELECT grant_id FROM oauth_tokens WHERE token_hash = ? AND kind = 'refresh') \
             AND client_id = ?"
        }
        Some(false) => {
            "DELETE FROM oauth_tokens WHERE token_hash = ? AND kind = 'access' \
             AND grant_id IN (SELECT id FROM oauth_grants WHERE client_id = ?)"
        }
    };
    sqlx::query(statement)
        .bind(&token_hash)
        .bind(client_id)
        .execute(db)
        .await
        .map(|_| ())
        .map_err(database)
}

/// What one `prune_in` removed.
#[derive(Debug, PartialEq)]
pub(crate) struct Pruned {
    pub requests: u64,
    pub codes: u64,
    pub tokens: u64,
    pub grants: u64,
    pub clients: u64,
}

/// Deletes consent requests, codes and tokens past their expiry, then the
/// grants left without a token, then the clients with no grant, code or
/// consent request whose last use (their registration when never used) is
/// `UNUSED_CLIENT_LIFETIME` old. A rotated refresh token stays until its own
/// expiry, so its reuse is still caught.
pub(crate) async fn prune_in(db: &SqlitePool, now: i64) -> Result<Pruned, String> {
    let delete = |query: Query<'static, Sqlite, SqliteArguments<'static>>| async move {
        query
            .execute(db)
            .await
            .map(|done| done.rows_affected())
            .map_err(|e| e.to_string())
    };
    let expired = |sql: &'static str| sqlx::query(sql).bind(now);
    Ok(Pruned {
        requests: delete(expired("DELETE FROM oauth_requests WHERE expires_at <= ?")).await?,
        codes: delete(expired("DELETE FROM oauth_codes WHERE expires_at <= ?")).await?,
        tokens: delete(expired("DELETE FROM oauth_tokens WHERE expires_at <= ?")).await?,
        grants: delete(sqlx::query(
            "DELETE FROM oauth_grants WHERE NOT EXISTS \
             (SELECT 1 FROM oauth_tokens t WHERE t.grant_id = oauth_grants.id)",
        ))
        .await?,
        clients: delete(
            sqlx::query(
                "DELETE FROM oauth_clients \
                 WHERE COALESCE(last_used_at, created_at) <= ? \
                   AND NOT EXISTS (SELECT 1 FROM oauth_grants g WHERE g.client_id = oauth_clients.client_id) \
                   AND NOT EXISTS (SELECT 1 FROM oauth_codes c WHERE c.client_id = oauth_clients.client_id) \
                   AND NOT EXISTS (SELECT 1 FROM oauth_requests r WHERE r.client_id = oauth_clients.client_id)",
            )
            .bind(now - UNUSED_CLIENT_LIFETIME),
        )
        .await?,
    })
}

async fn insert_token(
    tx: &mut Transaction<'_, Sqlite>,
    token: &str,
    grant_id: &str,
    kind: &'static str,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oauth_tokens (token_hash, grant_id, kind, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(hash(token))
    .bind(grant_id)
    .bind(kind)
    .bind(expires_at)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

/// A client's last use is its last code redemption or refresh (D4).
async fn mark_client_used(
    tx: &mut Transaction<'_, Sqlite>,
    client_id: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE oauth_clients SET last_used_at = ? WHERE client_id = ?")
        .bind(now)
        .bind(client_id)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::*;
    use crate::connection::test_util::*;
    use crate::db::test_db;
    use crate::oauth::clients::find_in;
    use crate::oauth::codes::{store_request_in, PendingRequest};
    use crate::oauth::secrets::hash;

    const T: i64 = 1_800_000_000;
    const DAY: i64 = 24 * 3600;
    const USED: TokenRefusal =
        TokenRefusal::InvalidGrant("The code is unknown, expired or already used");
    const MISMATCH: TokenRefusal =
        TokenRefusal::InvalidGrant("The code does not match this client, redirect URI or verifier");
    const UNKNOWN: TokenRefusal =
        TokenRefusal::InvalidGrant("The refresh token is unknown, expired or revoked");
    const ENDED: TokenRefusal =
        TokenRefusal::InvalidGrant("The refresh token was already used; the connection is ended");

    /// A fresh database with user u1 and one code issued at T.
    async fn issue(scope: &str) -> (SqlitePool, String) {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let code = issue_code(&db, "u1", scope, RESOURCE, T).await;
        (db, code)
    }

    /// A fresh database with u1 connected at T, and its refresh token.
    async fn refreshable() -> (SqlitePool, String) {
        let (db, code) = issue("soulful offline_access").await;
        let issued = redeem_code_in(&db, &good(&code), T).await.unwrap();
        (db, issued.refresh_token.unwrap())
    }

    async fn client_last_used(db: &SqlitePool, client_id: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT last_used_at FROM oauth_clients WHERE client_id = ?")
            .bind(client_id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_code_is_redeemed_once_and_its_second_use_ends_the_grant() {
        let (db, code) = issue("soulful offline_access").await;
        let issued = redeem_code_in(&db, &good(&code), T + 5).await.unwrap();
        assert_eq!(
            (
                issued.expires_in,
                issued.scope.as_str(),
                issued.refresh_token.is_some()
            ),
            (3600, "soulful offline_access", true)
        );
        assert_eq!(
            (
                count(&db, "oauth_grants").await,
                count(&db, "oauth_tokens").await
            ),
            (1, 2)
        );
        assert_eq!(client_last_used(&db, "dcr_c").await, Some(T + 5));
        assert_eq!(
            redeem_code_in(&db, &good(&code), T + 6).await.err(),
            Some(USED)
        );
        assert_eq!(
            (
                count(&db, "oauth_grants").await,
                count(&db, "oauth_tokens").await
            ),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn a_code_lives_60_seconds() {
        let (db, late) = issue("soulful").await;
        assert_eq!(
            redeem_code_in(&db, &good(&late), T + 60).await.err(),
            Some(USED)
        );
        let on_time = issue_code(&db, "u1", "soulful", RESOURCE, T).await;
        assert!(redeem_code_in(&db, &good(&on_time), T + 59).await.is_ok());
    }

    #[tokio::test]
    async fn a_wrong_verifier_burns_the_code() {
        let (db, code) = issue("soulful offline_access").await;
        let wrong = "a".repeat(43);
        assert_eq!(
            redeem_code_in(
                &db,
                &CodeRedemption {
                    verifier: &wrong,
                    ..good(&code)
                },
                T + 1
            )
            .await
            .err(),
            Some(MISMATCH)
        );
        assert_eq!(
            redeem_code_in(&db, &good(&code), T + 2).await.err(),
            Some(USED)
        );
        assert_eq!(count(&db, "oauth_grants").await, 0);
    }

    #[tokio::test]
    async fn the_code_belongs_to_its_client_redirect_and_address() {
        let (db, c1) = issue("soulful").await;
        assert_eq!(
            redeem_code_in(
                &db,
                &CodeRedemption {
                    client_id: "dcr_other",
                    ..good(&c1)
                },
                T + 1
            )
            .await
            .err(),
            Some(MISMATCH)
        );
        let c2 = issue_code(&db, "u1", "soulful", RESOURCE, T).await;
        assert_eq!(
            redeem_code_in(
                &db,
                &CodeRedemption {
                    redirect_uri: "https://claude.ai/other",
                    ..good(&c2)
                },
                T + 1
            )
            .await
            .err(),
            Some(MISMATCH)
        );
        let c3 = issue_code(&db, "u1", "soulful", RESOURCE, T).await;
        assert_eq!(
            redeem_code_in(
                &db,
                &CodeRedemption {
                    resource: "https://tunnel.example.net/mcp",
                    ..good(&c3)
                },
                T + 1
            )
            .await
            .err(),
            Some(TokenRefusal::InvalidTarget(
                "resource does not match the authorization request"
            ))
        );
        assert_eq!(count(&db, "oauth_grants").await, 0);
    }

    #[tokio::test]
    async fn without_offline_access_there_is_no_refresh_token() {
        let (db, code) = issue("soulful").await;
        let issued = redeem_code_in(&db, &good(&code), T).await.unwrap();
        assert_eq!(
            (issued.refresh_token, issued.scope.as_str()),
            (None, "soulful")
        );
        assert_eq!(count(&db, "oauth_tokens").await, 1);
    }

    #[tokio::test]
    async fn a_new_authorization_replaces_the_apps_earlier_grant() {
        let (db, c1) = issue("soulful offline_access").await;
        let first = redeem_code_in(&db, &good(&c1), T + 1).await.unwrap();
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&db)
            .await
            .unwrap();
        let c2 = issue_code(&db, "u1", "soulful offline_access", RESOURCE, T + 2).await;
        let second = redeem_code_in(&db, &good(&c2), T + 3).await.unwrap();
        let grants: Vec<(String, bool)> =
            sqlx::query_as("SELECT id, allow_delete FROM oauth_grants")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!((grants.len(), grants[0].1), (1, false));
        let held = |t: String| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM oauth_tokens WHERE token_hash = ?",
                )
                .bind(hash(&t))
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            (
                held(first.access_token).await,
                held(first.refresh_token.unwrap()).await
            ),
            (0, 0)
        );
        assert_eq!(held(second.access_token).await, 1);
    }

    #[tokio::test]
    async fn a_repeat_within_the_grace_window_gets_the_same_pair() {
        let (db, r) = refreshable().await;
        let a = refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 100)
            .await
            .unwrap();
        let b = refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 130)
            .await
            .unwrap();
        assert_eq!(
            (a.access_token.as_str(), a.refresh_token.as_deref()),
            (b.access_token.as_str(), b.refresh_token.as_deref())
        );
        assert_eq!((a.expires_in, b.expires_in), (3600, 3570));
        assert_eq!(client_last_used(&db, "dcr_c").await, Some(T + 100));
        assert_eq!(
            refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 161)
                .await
                .err(),
            Some(ENDED)
        );
        assert_eq!(count(&db, "oauth_grants").await, 0);
    }

    #[tokio::test]
    async fn reuse_is_detected_after_a_prune() {
        let (db, r) = refreshable().await;
        refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 10)
            .await
            .unwrap();
        prune_in(&db, T + 2 * 3600).await.unwrap();
        assert_eq!(
            refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 2 * 3600)
                .await
                .err(),
            Some(ENDED)
        );
        assert_eq!(count(&db, "oauth_grants").await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_refreshes_rotate_once_and_agree() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db::migrated_file(dir.path()).await;
        add_user(&db, "u1", "alice").await;
        for round in 0..20 {
            let at = T + round * 10;
            let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, at).await;
            let refresh = redeem_code_in(&db, &good(&code), at)
                .await
                .unwrap()
                .refresh_token
                .unwrap();
            let spawn = |db: SqlitePool, r: String| {
                tokio::spawn(async move {
                    refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, at + 1).await
                })
            };
            let (a, b) = (
                spawn(db.clone(), refresh.clone()),
                spawn(db.clone(), refresh.clone()),
            );
            let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
            assert_eq!(
                (a.access_token, a.refresh_token),
                (b.access_token, b.refresh_token),
                "round {round}"
            );
            let unrotated: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM oauth_tokens WHERE kind = 'refresh' AND rotated_at IS NULL",
            )
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(unrotated, 1, "round {round}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_redemptions_of_one_code_at_once_issue_once_and_end_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db::migrated_file(dir.path()).await;
        add_user(&db, "u1", "alice").await;
        for round in 0..20 {
            let at = T + round * 10;
            let code = issue_code(&db, "u1", "soulful offline_access", RESOURCE, at).await;
            let spawn = |db: SqlitePool, c: String| {
                tokio::spawn(async move { redeem_code_in(&db, &good(&c), at + 1).await })
            };
            let (a, b) = (
                spawn(db.clone(), code.clone()),
                spawn(db.clone(), code.clone()),
            );
            let results = [a.await.unwrap(), b.await.unwrap()];
            assert_eq!(
                results.iter().filter(|r| r.is_ok()).count(),
                1,
                "round {round}"
            );
            assert!(
                results.iter().any(|r| r.as_ref().err() == Some(&USED)),
                "round {round}"
            );
            assert_eq!(count(&db, "oauth_grants").await, 0, "round {round}");
        }
    }

    #[tokio::test]
    async fn a_refresh_is_bound_to_its_client_and_address() {
        let (db, r) = refreshable().await;
        assert_eq!(
            refresh_in(&db, test_key(), &r, "dcr_other", RESOURCE, T + 10)
                .await
                .err(),
            Some(UNKNOWN)
        );
        assert_eq!(
            refresh_in(
                &db,
                test_key(),
                &r,
                "dcr_c",
                "https://tunnel.example.net/mcp",
                T + 11
            )
            .await
            .err(),
            Some(TokenRefusal::InvalidTarget(
                "The refresh token was issued for another address of this server"
            ))
        );
        // Neither refusal rotated the token or ended the grant.
        assert!(refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 12)
            .await
            .is_ok());
        assert_eq!(count(&db, "oauth_grants").await, 1);
    }

    #[tokio::test]
    async fn a_refresh_token_names_its_grant_for_its_own_client_only() {
        let (db, code) = issue("soulful offline_access").await;
        let issued = redeem_code_in(&db, &good(&code), T).await.unwrap();
        let r = issued.refresh_token.unwrap();
        let grant: String = sqlx::query_scalar("SELECT id FROM oauth_grants")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            grant_of_refresh_in(&db, &r, "dcr_c").await,
            Ok(Some(grant.clone()))
        );
        assert_eq!(grant_of_refresh_in(&db, &r, "dcr_other").await, Ok(None));
        assert_eq!(
            grant_of_refresh_in(&db, &issued.access_token, "dcr_c").await,
            Ok(None)
        );
        refresh_in(&db, test_key(), &r, "dcr_c", RESOURCE, T + 10)
            .await
            .unwrap();
        assert_eq!(grant_of_refresh_in(&db, &r, "dcr_c").await, Ok(Some(grant)));
    }

    #[tokio::test]
    async fn revoking_a_refresh_token_ends_the_grant_and_an_access_token_only_itself() {
        let (db, code) = issue("soulful offline_access").await;
        let issued = redeem_code_in(&db, &good(&code), T).await.unwrap();
        let r = issued.refresh_token.unwrap();
        revoke_in(&db, &issued.access_token, "dcr_c").await.unwrap();
        assert_eq!(
            (
                count(&db, "oauth_grants").await,
                count(&db, "oauth_tokens").await
            ),
            (1, 1)
        );
        revoke_in(&db, "made-up", "dcr_c").await.unwrap();
        revoke_in(&db, &r, "dcr_other").await.unwrap();
        assert_eq!(
            (
                count(&db, "oauth_grants").await,
                count(&db, "oauth_tokens").await
            ),
            (1, 1)
        );
        revoke_in(&db, &r, "dcr_c").await.unwrap();
        assert_eq!(
            (
                count(&db, "oauth_grants").await,
                count(&db, "oauth_tokens").await
            ),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn pruning_drops_what_has_expired_and_keeps_live_grants() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        issue_code(&db, "u1", "soulful", RESOURCE, T).await; // never redeemed
        let a = issue_code(&db, "u1", "soulful", "https://a.example.com/mcp", T).await;
        redeem_code_in(
            &db,
            &CodeRedemption {
                resource: "https://a.example.com/mcp",
                ..good(&a)
            },
            T,
        )
        .await
        .unwrap();
        let b = issue_code(
            &db,
            "u1",
            "soulful offline_access",
            "https://b.example.com/mcp",
            T,
        )
        .await;
        let b = redeem_code_in(
            &db,
            &CodeRedemption {
                resource: "https://b.example.com/mcp",
                ..good(&b)
            },
            T,
        )
        .await
        .unwrap();
        refresh_in(
            &db,
            test_key(),
            &b.refresh_token.unwrap(),
            "dcr_c",
            "https://b.example.com/mcp",
            T + 3590,
        )
        .await
        .unwrap();
        store_request_in(
            &db,
            "pending",
            &PendingRequest {
                user_id: "u1".into(),
                client: find_in(&db, "dcr_c").await.unwrap().unwrap(),
                redirect_uri: CLAUDE_CALLBACK.into(),
                state: None,
                code_challenge: CHALLENGE.into(),
                scope: "soulful".into(),
                resource: RESOURCE.into(),
            },
            T,
        )
        .await
        .unwrap();
        for (id, created, used) in [
            ("dcr_old", T - 91 * DAY, None),
            ("dcr_used", T - 100 * DAY, Some(T - 10 * DAY)),
            ("dcr_new", T - 89 * DAY, None),
        ] {
            sqlx::query(
                "INSERT INTO oauth_clients (client_id, kind, redirect_uris, created_at, last_used_at) \
                 VALUES (?, 'dcr', ?, ?, ?)",
            )
            .bind(id)
            .bind(format!("[\"{CLAUDE_CALLBACK}\"]"))
            .bind(created)
            .bind(used)
            .execute(&db)
            .await
            .unwrap();
        }
        assert_eq!(
            prune_in(&db, T + 3601).await,
            Ok(Pruned {
                requests: 1,
                codes: 3,
                tokens: 2,
                grants: 1,
                clients: 1
            })
        );
        let resources: Vec<String> = sqlx::query_scalar("SELECT resource FROM oauth_grants")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(resources, ["https://b.example.com/mcp"]);
        // b's rotated refresh token stays until its own expiry
        assert_eq!(count(&db, "oauth_tokens").await, 3);
        let clients: Vec<String> =
            sqlx::query_scalar("SELECT client_id FROM oauth_clients ORDER BY client_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(clients, ["dcr_c", "dcr_new", "dcr_used"]);
    }

    #[tokio::test]
    async fn a_client_signing_in_is_kept_until_its_code_and_request_expire() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        // dcr_c registered 91 days ago and never used; a code issued now.
        issue_code(&db, "u1", "soulful", RESOURCE, T - 91 * DAY).await;
        issue_code(&db, "u1", "soulful", RESOURCE, T).await;
        sqlx::query(
            "INSERT INTO oauth_clients (client_id, kind, redirect_uris, created_at) \
             VALUES ('dcr_r', 'dcr', ?, ?)",
        )
        .bind(format!("[\"{CLAUDE_CALLBACK}\"]"))
        .bind(T - 91 * DAY)
        .execute(&db)
        .await
        .unwrap();
        store_request_in(
            &db,
            "pending",
            &PendingRequest {
                user_id: "u1".into(),
                client: find_in(&db, "dcr_r").await.unwrap().unwrap(),
                redirect_uri: CLAUDE_CALLBACK.into(),
                state: None,
                code_challenge: CHALLENGE.into(),
                scope: "soulful".into(),
                resource: RESOURCE.into(),
            },
            T,
        )
        .await
        .unwrap();
        let pruned = |requests, codes, clients| Pruned {
            requests,
            codes,
            tokens: 0,
            grants: 0,
            clients,
        };
        assert_eq!(prune_in(&db, T + 1).await, Ok(pruned(0, 1, 0)));
        assert_eq!(prune_in(&db, T + 600).await, Ok(pruned(1, 1, 2)));
        assert_eq!(count(&db, "oauth_clients").await, 0);
    }
}
