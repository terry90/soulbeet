use crate::auth::UserCtx;

#[cfg(feature = "server")]
use crate::{
    auth::Claims, db::DB, models::revoked_session::RevokedSession, models::user::User,
    secret_key::SecretKey,
};
#[cfg(feature = "server")]
use axum::{extract::FromRequestParts, http::StatusCode};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

/// A signed-in user, as their row says now.
pub struct AuthSession(pub UserCtx);

/// A signed-in user whose row has the admin role.
pub struct AdminSession(pub UserCtx);

#[cfg(feature = "server")]
pub const SESSION_GONE: &str = "Session ended: the account no longer exists";
#[cfg(feature = "server")]
pub const SESSION_STALE: &str = "Session ended: sign in again";
#[cfg(feature = "server")]
pub const SESSION_SIGNED_OUT: &str = "Session ended: signed out";
#[cfg(feature = "server")]
pub const ADMIN_ONLY: &str = "Only an admin can do this";

/// The session a token's claims give, given the user's current row. A token
/// issued before the row's last credential change no longer counts.
#[cfg(feature = "server")]
pub(crate) fn session_from(
    claims: &Claims,
    user: Option<&User>,
) -> Result<UserCtx, (StatusCode, String)> {
    let user = user.ok_or_else(|| (StatusCode::UNAUTHORIZED, SESSION_GONE.to_string()))?;
    if claims.ver != user.credentials_version {
        return Err((StatusCode::UNAUTHORIZED, SESSION_STALE.to_string()));
    }
    Ok(UserCtx {
        user_id: user.id.clone(),
        username: user.username.clone(),
        role: user.role,
    })
}

#[cfg(feature = "server")]
pub(crate) fn require_admin(ctx: UserCtx) -> Result<UserCtx, (StatusCode, String)> {
    if ctx.role.is_admin() {
        Ok(ctx)
    } else {
        Err((StatusCode::FORBIDDEN, ADMIN_ONLY.to_string()))
    }
}

/// Verifies the auth cookie's token and builds the session from the user's row.
#[cfg(feature = "server")]
pub(crate) async fn load_session(
    token: Option<&str>,
    db: &SqlitePool,
    key: &SecretKey,
) -> Result<UserCtx, (StatusCode, String)> {
    let Some(token) = token else {
        tracing::warn!("Auth failure: No auth token found in request cookies");
        return Err((StatusCode::UNAUTHORIZED, "No auth token found".to_string()));
    };
    let claims = crate::auth::verify_token(key, token).map_err(|e| {
        tracing::warn!("Auth failure: invalid token: {}", e);
        (StatusCode::UNAUTHORIZED, format!("Invalid token: {e}"))
    })?;
    let check_failed = |e: String| {
        tracing::error!("Could not check the session: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not check the session".to_string(),
        )
    };
    if RevokedSession::is_revoked_in(db, &claims.jti)
        .await
        .map_err(check_failed)?
    {
        return Err((StatusCode::UNAUTHORIZED, SESSION_SIGNED_OUT.to_string()));
    }
    let user = User::find_by_id_in(db, &claims.sub)
        .await
        .map_err(check_failed)?;
    session_from(&claims, user.as_ref())
}

#[cfg(feature = "server")]
impl<S> FromRequestParts<S> for AuthSession
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let cookies = parts
            .extensions
            .get::<tower_cookies::Cookies>()
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    String::from("Missing cookie middleware"),
                )
            })?;

        let token = cookies
            .get(crate::AUTH_COOKIE_NAME)
            .map(|c| c.value().to_string());

        load_session(token.as_deref(), &DB, crate::secret_key::current())
            .await
            .map(AuthSession)
    }
}

#[cfg(feature = "server")]
impl<S> FromRequestParts<S> for AdminSession
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let AuthSession(ctx) = AuthSession::from_request_parts(parts, state).await?;
        require_admin(ctx).map(AdminSession)
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::{create_token as create_token_with, Role};
    use crate::db::test_db;
    use crate::models::user::User;
    use crate::security::SEEDED_ADMIN_ID;
    use sqlx::SqlitePool;

    fn user_row(id: &str, username: &str, role: Role, credentials_version: i64) -> User {
        User {
            id: id.into(),
            username: username.into(),
            password_hash: String::new(),
            navidrome_token: None,
            navidrome_status: "unknown".into(),
            role,
            credentials_version,
        }
    }

    fn claims(sub: &str, ver: i64) -> Claims {
        Claims {
            sub: sub.into(),
            ver,
            jti: "t".into(),
            iat: 0,
            exp: 0,
        }
    }

    fn key() -> SecretKey {
        SecretKey::new("k".repeat(32))
    }

    fn create_token(user_id: &str, ver: i64) -> Result<String, String> {
        create_token_with(&key(), user_id, ver)
    }

    async fn load(token: Option<&str>, db: &SqlitePool) -> Result<UserCtx, (StatusCode, String)> {
        load_session(token, db, &key()).await
    }

    fn ctx(role: Role) -> UserCtx {
        UserCtx {
            user_id: "id-a".into(),
            username: "a".into(),
            role,
        }
    }

    async fn insert_user(db: &SqlitePool, id: &str, username: &str) {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id)
            .bind(username)
            .execute(db)
            .await
            .unwrap();
    }

    /// The migrated database: the seeded admin plus user "u1" at version 0.
    async fn db_with_user() -> SqlitePool {
        let db = test_db::migrated().await;
        insert_user(&db, "u1", "bob").await;
        db
    }

    #[test]
    fn session_comes_from_the_row_not_the_token() {
        let user = user_row("id-a", "new-name", Role::User, 3);
        let ctx = session_from(&claims("id-a", 3), Some(&user)).unwrap();
        assert_eq!(
            ctx,
            UserCtx {
                user_id: "id-a".into(),
                username: "new-name".into(),
                role: Role::User
            }
        );
    }

    #[tokio::test]
    async fn a_renamed_users_cookie_carries_the_database_name() {
        let db = test_db::migrated().await;
        insert_user(&db, "id-a", "old-name").await;
        let token = create_token("id-a", 0).unwrap();
        sqlx::query("UPDATE users SET username = 'renamed' WHERE id = 'id-a'")
            .execute(&db)
            .await
            .unwrap();
        insert_user(&db, "id-b", "old-name").await;
        let ctx = load(Some(&token), &db).await.unwrap();
        assert_eq!(
            (ctx.user_id.as_str(), ctx.username.as_str()),
            ("id-a", "renamed")
        );
    }

    #[test]
    fn changed_credentials_end_the_session() {
        let err = session_from(
            &claims("id-a", 2),
            Some(&user_row("id-a", "a", Role::User, 3)),
        )
        .unwrap_err();
        assert_eq!(err, (StatusCode::UNAUTHORIZED, SESSION_STALE.to_string()));
    }

    #[test]
    fn a_deleted_user_has_no_session() {
        assert_eq!(
            session_from(&claims("id-a", 0), None).unwrap_err(),
            (StatusCode::UNAUTHORIZED, SESSION_GONE.to_string())
        );
    }

    #[test]
    fn only_admins_pass_the_admin_check() {
        assert_eq!(
            require_admin(ctx(Role::User)).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        assert!(require_admin(ctx(Role::Admin)).is_ok());
    }

    #[tokio::test]
    async fn no_cookie_is_unauthorized() {
        let db = db_with_user().await;
        assert_eq!(
            load(None, &db).await.unwrap_err(),
            (StatusCode::UNAUTHORIZED, "No auth token found".to_string())
        );
    }

    #[tokio::test]
    async fn a_forged_cookie_is_unauthorized() {
        let db = db_with_user().await;
        let err = load(Some("not-a-token"), &db).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        assert!(err.1.starts_with("Invalid token: "), "{}", err.1);
    }

    #[tokio::test]
    async fn a_cookie_older_than_the_last_credential_change_is_unauthorized() {
        let db = db_with_user().await;
        let token = create_token("u1", 0).unwrap();
        sqlx::query("UPDATE users SET credentials_version = 1 WHERE id = 'u1'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            load(Some(&token), &db).await.unwrap_err(),
            (StatusCode::UNAUTHORIZED, SESSION_STALE.to_string())
        );
    }

    #[tokio::test]
    async fn a_user_cookie_is_forbidden_for_admin_functions() {
        let db = db_with_user().await;
        let ctx = load(Some(&create_token("u1", 0).unwrap()), &db)
            .await
            .unwrap();
        assert_eq!(
            require_admin(ctx).unwrap_err(),
            (StatusCode::FORBIDDEN, ADMIN_ONLY.to_string())
        );
    }

    #[tokio::test]
    async fn an_admin_cookie_passes() {
        let db = db_with_user().await;
        let ctx = load(Some(&create_token(SEEDED_ADMIN_ID, 0).unwrap()), &db)
            .await
            .unwrap();
        assert_eq!(require_admin(ctx).unwrap().role, Role::Admin);
    }

    #[tokio::test]
    async fn a_session_signed_with_the_old_built_in_key_ends() {
        // An install that ran without SECRET_KEY used "secret"; it now gets
        // a generated key, so its cookies no longer verify.
        let db = db_with_user().await;
        let old = create_token_with(&SecretKey::new("secret".into()), "u1", 0).unwrap();
        let generated = crate::secret_key::resolve(
            None,
            tempfile::tempdir().unwrap().path(),
            "sqlite:soulbeet.db",
        )
        .unwrap()
        .0;
        let err = load_session(Some(&old), &db, &generated).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_revoked_token_is_unauthorized() {
        let db = db_with_user().await;
        let token = create_token("u1", 0).unwrap();
        let jti = crate::auth::verify_token(&key(), &token).unwrap().jti;
        RevokedSession::revoke_in(&db, &jti, i64::MAX, 0)
            .await
            .unwrap();
        assert_eq!(
            load(Some(&token), &db).await.unwrap_err(),
            (StatusCode::UNAUTHORIZED, SESSION_SIGNED_OUT.to_string())
        );
    }
}
