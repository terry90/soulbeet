//! Authorize requests waiting for the user's answer, and the authorization
//! codes an answer of Allow issues. Both are stored only as hashes.

use sqlx::SqlitePool;

use super::clients::{redirect_uris_from_db, ClientKind, KnownClient};
use super::secrets::hash;

/// How long the consent page can be answered, in seconds (D4).
pub(crate) const REQUEST_LIFETIME: i64 = 600;
/// How long a code can be redeemed, in seconds (D4).
pub(crate) const CODE_LIFETIME: i64 = 60;
/// Most unanswered requests a user has at once; storing one more drops the
/// oldest.
const PENDING_PER_USER: i64 = 20;

/// An authorize request as `GET /oauth/authorize` validated it.
pub(crate) struct PendingRequest {
    pub user_id: String,
    pub client: KnownClient,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub scope: String,
    pub resource: String,
}

/// Stores `req` under `id` for `REQUEST_LIFETIME`, in one BEGIN IMMEDIATE
/// transaction that first drops the user's expired requests and then their
/// oldest, so at most `PENDING_PER_USER` remain.
pub(crate) async fn store_request_in(
    db: &SqlitePool,
    id: &str,
    req: &PendingRequest,
    now: i64,
) -> Result<(), String> {
    let database = |e: sqlx::Error| e.to_string();
    let redirect_uris =
        serde_json::to_string(&req.client.redirect_uris).expect("a list of strings serializes");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(database)?;
    sqlx::query("DELETE FROM oauth_requests WHERE user_id = ? AND expires_at <= ?")
        .bind(&req.user_id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    // rowid follows insertion order: the largest rowid is the newest request.
    sqlx::query(
        "DELETE FROM oauth_requests WHERE rowid IN ( \
             SELECT rowid FROM oauth_requests WHERE user_id = ? \
             ORDER BY rowid DESC LIMIT -1 OFFSET ?)",
    )
    .bind(&req.user_id)
    .bind(PENDING_PER_USER - 1)
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    sqlx::query(
        "INSERT INTO oauth_requests (id_hash, user_id, client_id, client_kind, client_name, \
         client_redirect_uris, redirect_uri, state, code_challenge, scope, resource, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(hash(id))
    .bind(&req.user_id)
    .bind(&req.client.client_id)
    .bind(req.client.kind.as_db())
    .bind(&req.client.client_name)
    .bind(redirect_uris)
    .bind(&req.redirect_uri)
    .bind(&req.state)
    .bind(&req.code_challenge)
    .bind(&req.scope)
    .bind(&req.resource)
    .bind(now + REQUEST_LIFETIME)
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    tx.commit().await.map_err(database)
}

type RequestRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
);

/// The request once: deleted as it is read; None when unknown, another user's or expired.
pub(crate) async fn take_request_in(
    db: &SqlitePool,
    id: &str,
    user_id: &str,
    now: i64,
) -> Result<Option<PendingRequest>, String> {
    let row: Option<RequestRow> = sqlx::query_as(
        "DELETE FROM oauth_requests WHERE id_hash = ? AND user_id = ? AND expires_at > ? \
         RETURNING user_id, client_id, client_kind, client_name, client_redirect_uris, \
         redirect_uri, state, code_challenge, scope, resource",
    )
    .bind(hash(id))
    .bind(user_id)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    let Some((
        user_id,
        client_id,
        kind,
        client_name,
        redirect_uris,
        redirect_uri,
        state,
        code_challenge,
        scope,
        resource,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(PendingRequest {
        user_id,
        client: KnownClient {
            client_id,
            kind: ClientKind::from_db(&kind)?,
            client_name,
            redirect_uris: redirect_uris_from_db(&redirect_uris)?,
        },
        redirect_uri,
        state,
        code_challenge,
        scope,
        resource,
    }))
}

/// Stores `code` for `req`'s user, client, redirect URI, challenge, scope and
/// resource, redeemable for `CODE_LIFETIME`.
pub(crate) async fn store_code_in(
    db: &SqlitePool,
    code: &str,
    req: &PendingRequest,
    now: i64,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO oauth_codes (code_hash, user_id, client_id, redirect_uri, code_challenge, \
         scope, resource, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(hash(code))
    .bind(&req.user_id)
    .bind(&req.client.client_id)
    .bind(&req.redirect_uri)
    .bind(&req.code_challenge)
    .bind(&req.scope)
    .bind(&req.resource)
    .bind(now + CODE_LIFETIME)
    .execute(db)
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::{add_user, count, CHALLENGE, CLAUDE_CALLBACK};
    use crate::db::test_db;

    fn request(user_id: &str) -> PendingRequest {
        PendingRequest {
            user_id: user_id.into(),
            client: KnownClient {
                client_id: "dcr_test".into(),
                kind: ClientKind::Dcr,
                client_name: Some("Claude".into()),
                redirect_uris: vec![CLAUDE_CALLBACK.into()],
            },
            redirect_uri: CLAUDE_CALLBACK.into(),
            state: None,
            code_challenge: CHALLENGE.into(),
            scope: "soulful".into(),
            resource: "https://music.example.com/mcp".into(),
        }
    }

    #[tokio::test]
    async fn a_request_answers_until_it_expires() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        store_request_in(&db, "r1", &request("u1"), 1_000)
            .await
            .unwrap();
        store_request_in(&db, "r2", &request("u1"), 1_000)
            .await
            .unwrap();
        let taken = take_request_in(&db, "r1", "u1", 1_000 + REQUEST_LIFETIME - 1)
            .await
            .unwrap()
            .expect("answerable until its last second");
        assert_eq!(
            (
                taken.client.client_id.as_str(),
                taken.client.kind,
                taken.redirect_uri.as_str()
            ),
            ("dcr_test", ClientKind::Dcr, CLAUDE_CALLBACK)
        );
        assert!(take_request_in(&db, "r2", "u1", 1_000 + REQUEST_LIFETIME)
            .await
            .unwrap()
            .is_none());
        // The expired request goes with the user's next one.
        store_request_in(&db, "r3", &request("u1"), 1_000 + REQUEST_LIFETIME)
            .await
            .unwrap();
        assert_eq!(count(&db, "oauth_requests").await, 1);
    }

    #[tokio::test]
    async fn the_cap_counts_each_users_requests_alone() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        add_user(&db, "u2", "bob").await;
        store_request_in(&db, "bob's", &request("u2"), 0)
            .await
            .unwrap();
        for i in 0..PENDING_PER_USER + 1 {
            store_request_in(&db, &format!("r{i}"), &request("u1"), 0)
                .await
                .unwrap();
        }
        assert_eq!(count(&db, "oauth_requests").await, PENDING_PER_USER + 1);
        assert!(take_request_in(&db, "r0", "u1", 0).await.unwrap().is_none());
        assert!(take_request_in(&db, "bob's", "u2", 0)
            .await
            .unwrap()
            .is_some());
    }
}
