//! Authorize requests waiting for the user's answer, and the authorization
//! codes an answer of Allow issues. Both are stored only as hashes.

use sqlx::{SqliteConnection, SqliteExecutor, SqlitePool};

use super::cimd::ClientDocument;
use super::clients::{redirect_uris_from_db, upsert_cimd_in, ClientKind, KnownClient};
use super::secrets::{code_for, hash};
use crate::secret_key::SecretKey;

/// How long the consent page can be answered, in seconds (D4).
pub(crate) const REQUEST_LIFETIME: i64 = 600;
/// How long a code can be redeemed, in seconds (D4).
pub(crate) const CODE_LIFETIME: i64 = 60;
/// How long an answered request answers the same answer again from the session that gave it,
/// in seconds: a double click sends the form twice, and the browser follows only the second
/// answer's redirect.
pub(crate) const REPEAT_WINDOW: i64 = 30;
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

/// What the session cookie of the user a write is for names: the user's
/// credentials version when it was issued and its own id. The write checks
/// both in its own transaction, so a password change or a sign-out that
/// commits after the session was read still ends it (the credential-change
/// trigger cannot delete a row inserted after it ran).
pub(crate) struct SessionCheck {
    pub credentials_version: i64,
    pub session_id: String,
}

/// Why a write for a signed-in user did not happen.
#[derive(Debug, PartialEq)]
pub(crate) enum Refused {
    /// The user's credentials changed, the session was signed out or the
    /// user is gone.
    SessionEnded,
    Database(String),
}

impl From<sqlx::Error> for Refused {
    fn from(e: sqlx::Error) -> Refused {
        Refused::Database(e.to_string())
    }
}

/// Whether `user_id`'s session `check` still counts.
async fn still_signed_in(
    conn: &mut SqliteConnection,
    user_id: &str,
    check: &SessionCheck,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = ? AND credentials_version = ?) \
         AND NOT EXISTS (SELECT 1 FROM revoked_sessions WHERE jti = ?)",
    )
    .bind(user_id)
    .bind(check.credentials_version)
    .bind(&check.session_id)
    .fetch_one(conn)
    .await
}

/// Stores `req` under `id` for `REQUEST_LIFETIME`, in one BEGIN IMMEDIATE
/// transaction that checks the session, then drops the user's expired
/// requests and then their oldest, so at most `PENDING_PER_USER` remain.
pub(crate) async fn store_request_in(
    db: &SqlitePool,
    check: &SessionCheck,
    id: &str,
    req: &PendingRequest,
    now: i64,
) -> Result<(), Refused> {
    let redirect_uris =
        serde_json::to_string(&req.client.redirect_uris).expect("a list of strings serializes");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if !still_signed_in(&mut tx, &req.user_id, check).await? {
        return Err(Refused::SessionEnded);
    }
    sqlx::query("DELETE FROM oauth_requests WHERE user_id = ? AND expires_at <= ?")
        .bind(&req.user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM oauth_requests WHERE seq IN ( \
             SELECT seq FROM oauth_requests WHERE user_id = ? \
             ORDER BY seq DESC LIMIT -1 OFFSET ?)",
    )
    .bind(&req.user_id)
    .bind(PENDING_PER_USER - 1)
    .execute(&mut *tx)
    .await?;
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
    .await?;
    tx.commit().await?;
    Ok(())
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

/// The request once, answered with `answer` by the session `session_id` as it is read: it then
/// stays only REPEAT_WINDOW longer, for `answered_request_in`. None when unknown, another user's,
/// expired or answered already.
pub(crate) async fn take_request_in<'e>(
    db: impl SqliteExecutor<'e>,
    id: &str,
    user_id: &str,
    session_id: &str,
    answer: &str,
    now: i64,
) -> Result<Option<PendingRequest>, String> {
    let row: Option<RequestRow> = sqlx::query_as(
        "UPDATE oauth_requests SET answer = ?, answered_session = ?, expires_at = ? \
         WHERE id_hash = ? AND user_id = ? AND expires_at > ? AND answer IS NULL \
         RETURNING user_id, client_id, client_kind, client_name, client_redirect_uris, \
         redirect_uri, state, code_challenge, scope, resource",
    )
    .bind(answer)
    .bind(session_id)
    .bind(now + REPEAT_WINDOW)
    .bind(hash(id))
    .bind(user_id)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    row.map(pending_of).transpose()
}

/// The request `id` the session `session_id` answered with `answer` within REPEAT_WINDOW.
async fn answered_request_in<'e>(
    db: impl SqliteExecutor<'e>,
    id: &str,
    user_id: &str,
    session_id: &str,
    answer: &str,
    now: i64,
) -> Result<Option<PendingRequest>, String> {
    let row: Option<RequestRow> = sqlx::query_as(
        "SELECT user_id, client_id, client_kind, client_name, client_redirect_uris, \
         redirect_uri, state, code_challenge, scope, resource FROM oauth_requests \
         WHERE id_hash = ? AND user_id = ? AND answer = ? AND answered_session = ? \
           AND expires_at > ?",
    )
    .bind(hash(id))
    .bind(user_id)
    .bind(answer)
    .bind(session_id)
    .bind(now)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    row.map(pending_of).transpose()
}

fn pending_of(row: RequestRow) -> Result<PendingRequest, String> {
    let (
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
    ) = row;
    Ok(PendingRequest {
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
    })
}

/// Stores `code` for `req`'s user, client, redirect URI, challenge, scope and
/// resource, redeemable for `CODE_LIFETIME`.
pub(crate) async fn store_code_in<'e>(
    db: impl SqliteExecutor<'e>,
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

/// What answering a request did.
pub(crate) enum Answered {
    /// No request of the user's has that id: expired, answered or never made.
    Gone,
    Denied(PendingRequest),
    /// Allowed, with the code it issued.
    Allowed(PendingRequest, String),
}

/// Takes `user_id`'s request `id` and answers it, in one BEGIN IMMEDIATE
/// transaction that first checks the session: Allow stores a CIMD client
/// as its document read (the code's client must exist) and issues the code
/// `code_for(key, id)`. The same answer from the same session within
/// REPEAT_WINDOW answers as the first did, while its code is not redeemed:
/// a double click gets the redirect the first answer's was abandoned for.
pub(crate) async fn answer_in(
    db: &SqlitePool,
    key: &SecretKey,
    user_id: &str,
    check: &SessionCheck,
    id: &str,
    allow: bool,
    now: i64,
) -> Result<Answered, Refused> {
    let answer = if allow { "allow" } else { "deny" };
    let session = &check.session_id;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if !still_signed_in(&mut tx, user_id, check).await? {
        return Err(Refused::SessionEnded);
    }
    let taken = take_request_in(&mut *tx, id, user_id, session, answer, now)
        .await
        .map_err(Refused::Database)?;
    let Some(pending) = taken else {
        let repeat = answered_request_in(&mut *tx, id, user_id, session, answer, now)
            .await
            .map_err(Refused::Database)?;
        return Ok(match repeat {
            None => Answered::Gone,
            Some(pending) if !allow => Answered::Denied(pending),
            Some(pending) => {
                let code = code_for(key, id);
                let unused: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM oauth_codes \
                     WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?)",
                )
                .bind(hash(&code))
                .bind(now)
                .fetch_one(&mut *tx)
                .await?;
                match unused {
                    true => Answered::Allowed(pending, code),
                    false => Answered::Gone,
                }
            }
        });
    };
    if !allow {
        tx.commit().await?;
        return Ok(Answered::Denied(pending));
    }
    if pending.client.kind == ClientKind::Cimd {
        let doc = ClientDocument {
            client_id: pending.client.client_id.clone(),
            client_name: pending.client.client_name.clone(),
            redirect_uris: pending.client.redirect_uris.clone(),
        };
        upsert_cimd_in(&mut *tx, &doc, now)
            .await
            .map_err(Refused::Database)?;
    }
    let code = code_for(key, id);
    store_code_in(&mut *tx, &code, &pending, now)
        .await
        .map_err(Refused::Database)?;
    tx.commit().await?;
    Ok(Answered::Allowed(pending, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::{
        add_user, behind_a_writer, count, fresh_session, test_key, CHALLENGE, CLAUDE_CALLBACK,
    };
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
        store_request_in(&db, &fresh_session(), "r1", &request("u1"), 1_000)
            .await
            .unwrap();
        store_request_in(&db, &fresh_session(), "r2", &request("u1"), 1_000)
            .await
            .unwrap();
        let taken = take_request_in(&db, "r1", "u1", "fresh-session", "allow", 1_000 + REQUEST_LIFETIME - 1)
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
        assert!(take_request_in(&db, "r2", "u1", "fresh-session", "allow", 1_000 + REQUEST_LIFETIME)
            .await
            .unwrap()
            .is_none());
        // The expired request goes with the user's next one; the answered one stays
        // REPEAT_WINDOW after its answer.
        store_request_in(
            &db,
            &fresh_session(),
            "r3",
            &request("u1"),
            1_000 + REQUEST_LIFETIME,
        )
        .await
        .unwrap();
        assert_eq!(count(&db, "oauth_requests").await, 2);
        store_request_in(
            &db,
            &fresh_session(),
            "r4",
            &request("u1"),
            1_000 + REQUEST_LIFETIME - 1 + REPEAT_WINDOW,
        )
        .await
        .unwrap();
        assert_eq!(count(&db, "oauth_requests").await, 2);
    }

    #[tokio::test]
    async fn the_cap_counts_each_users_requests_alone() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        add_user(&db, "u2", "bob").await;
        store_request_in(&db, &fresh_session(), "bob's", &request("u2"), 0)
            .await
            .unwrap();
        for i in 0..PENDING_PER_USER + 1 {
            store_request_in(&db, &fresh_session(), &format!("r{i}"), &request("u1"), 0)
                .await
                .unwrap();
        }
        assert_eq!(count(&db, "oauth_requests").await, PENDING_PER_USER + 1);
        assert!(take_request_in(&db, "r0", "u1", "fresh-session", "allow", 0).await.unwrap().is_none());
        assert!(take_request_in(&db, "bob's", "u2", "fresh-session", "allow", 0)
            .await
            .unwrap()
            .is_some());
    }

    async fn bump_credentials(db: &SqlitePool, user_id: &str) {
        sqlx::query("UPDATE users SET credentials_version = credentials_version + 1 WHERE id = ?")
            .bind(user_id)
            .execute(db)
            .await
            .unwrap();
    }

    /// A DCR client dcr_test, so a code for `request` has its client.
    async fn with_client(db: &SqlitePool) {
        sqlx::query(
            "INSERT INTO oauth_clients (client_id, kind, redirect_uris, created_at) \
             VALUES ('dcr_test', 'dcr', ?, 0)",
        )
        .bind(format!("[\"{CLAUDE_CALLBACK}\"]"))
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn nothing_is_stored_for_a_session_that_ended_after_it_was_read() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        with_client(&db).await;
        store_request_in(&db, &fresh_session(), "r1", &request("u1"), 0)
            .await
            .unwrap();
        // Signed out since the cookie was read: the request stays unanswered.
        sqlx::query(
            "INSERT INTO revoked_sessions (jti, expires_at) VALUES ('fresh-session', 9999999999)",
        )
        .execute(&db)
        .await
        .unwrap();
        assert!(matches!(
            answer_in(&db, test_key(), "u1", &fresh_session(), "r1", true, 0).await,
            Err(Refused::SessionEnded)
        ));
        assert_eq!(
            (
                count(&db, "oauth_requests").await,
                count(&db, "oauth_codes").await
            ),
            (1, 0)
        );
        let other = SessionCheck {
            session_id: "other-session".into(),
            ..fresh_session()
        };
        // The password changed since the cookie was read.
        bump_credentials(&db, "u1").await;
        assert_eq!(
            store_request_in(&db, &other, "r2", &request("u1"), 0).await,
            Err(Refused::SessionEnded)
        );
        assert!(matches!(
            answer_in(&db, test_key(), "u1", &other, "r1", true, 0).await,
            Err(Refused::SessionEnded)
        ));
        assert_eq!(
            (
                count(&db, "oauth_requests").await,
                count(&db, "oauth_codes").await
            ),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn allowing_issues_a_code_and_denying_only_takes_the_request() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        with_client(&db).await;
        for id in ["deny", "allow"] {
            store_request_in(&db, &fresh_session(), id, &request("u1"), 0)
                .await
                .unwrap();
        }
        assert!(matches!(
            answer_in(&db, test_key(), "u1", &fresh_session(), "deny", false, 0).await,
            Ok(Answered::Denied(_))
        ));
        let Ok(Answered::Allowed(pending, code)) =
            answer_in(&db, test_key(), "u1", &fresh_session(), "allow", true, 0).await
        else {
            panic!("allow issues a code");
        };
        assert_eq!(pending.redirect_uri, CLAUDE_CALLBACK);
        let stored: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes WHERE code_hash = ?")
                .bind(hash(&code))
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored, 1);
        // A second submit of the same answer by the same session (a double click) answers the
        // same, within REPEAT_WINDOW and while the code is unused.
        let Ok(Answered::Allowed(_, again)) =
            answer_in(&db, test_key(), "u1", &fresh_session(), "allow", true, 10).await
        else {
            panic!("the repeat gets the same redirect");
        };
        assert_eq!(again, code);
        assert!(matches!(
            answer_in(&db, test_key(), "u1", &fresh_session(), "deny", false, 10).await,
            Ok(Answered::Denied(_))
        ));
        assert_eq!(count(&db, "oauth_codes").await, 1, "no second code");
        // Another answer, another session, or too late: gone.
        let other = SessionCheck {
            session_id: "other-session".into(),
            ..fresh_session()
        };
        for (check, allow, at) in [
            (fresh_session(), false, 10),
            (other, true, 10),
            (fresh_session(), true, REPEAT_WINDOW),
        ] {
            assert!(matches!(
                answer_in(&db, test_key(), "u1", &check, "allow", allow, at).await,
                Ok(Answered::Gone)
            ));
        }
        // Once the code is redeemed, a repeat is gone too.
        sqlx::query("UPDATE oauth_codes SET used_at = 5")
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            answer_in(&db, test_key(), "u1", &fresh_session(), "allow", true, 10).await,
            Ok(Answered::Gone)
        ));
    }

    // Each transaction reads the session before it writes: begun with
    // BEGIN IMMEDIATE it waits for another writer, a plain BEGIN fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storing_and_answering_wait_for_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db::migrated_file(dir.path()).await;
        add_user(&db, "u1", "alice").await;
        with_client(&db).await;
        let stored = behind_a_writer(&db, {
            let db = db.clone();
            async move { store_request_in(&db, &fresh_session(), "r1", &request("u1"), 0).await }
        })
        .await;
        assert_eq!(stored, Ok(()));
        let answered = behind_a_writer(&db, {
            let db = db.clone();
            async move {
                answer_in(&db, test_key(), "u1", &fresh_session(), "r1", true, 0)
                    .await
                    .map(|answered| matches!(answered, Answered::Allowed(..)))
            }
        })
        .await;
        assert_eq!(answered, Ok(true));
    }
}
