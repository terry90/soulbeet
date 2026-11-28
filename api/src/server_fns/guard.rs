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

/// A signed-in user, also one whose session may do nothing but set a new
/// password (`security::password_change_required`). Only reading who is
/// signed in and changing the password take it.
pub struct PasswordChangeSession(pub UserCtx);

#[cfg(feature = "server")]
pub const SESSION_GONE: &str = "Session ended: the account no longer exists";
#[cfg(feature = "server")]
pub const SESSION_STALE: &str = "Session ended: sign in again";
#[cfg(feature = "server")]
pub const SESSION_SIGNED_OUT: &str = "Session ended: signed out";
#[cfg(feature = "server")]
pub const ADMIN_ONLY: &str = "Only an admin can do this";
#[cfg(feature = "server")]
pub const PASSWORD_CHANGE_REQUIRED: &str =
    "This account still has the default password. Choose a new one first.";

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

/// Refuses the session of an account that must set a new password first,
/// which `change_required` tells from the account's id.
#[cfg(feature = "server")]
pub(crate) fn require_password_set(
    ctx: UserCtx,
    change_required: impl Fn(&str) -> bool,
) -> Result<UserCtx, (StatusCode, String)> {
    if change_required(&ctx.user_id) {
        Err((StatusCode::FORBIDDEN, PASSWORD_CHANGE_REQUIRED.to_string()))
    } else {
        Ok(ctx)
    }
}

/// Verifies the auth cookie's token and builds the session from the user's
/// row. A session that may only set a new password is refused.
#[cfg(feature = "server")]
pub(crate) async fn load_session(
    token: Option<&str>,
    db: &SqlitePool,
    key: &SecretKey,
) -> Result<UserCtx, (StatusCode, String)> {
    load_session_claims(token, db, key)
        .await
        .map(|(ctx, _)| ctx)
}

/// `load_session`, with the token's claims, so a later write can check that
/// the credentials version and the session it names still stand.
#[cfg(feature = "server")]
pub(crate) async fn load_session_claims(
    token: Option<&str>,
    db: &SqlitePool,
    key: &SecretKey,
) -> Result<(UserCtx, Claims), (StatusCode, String)> {
    let (ctx, claims) = load_any_session_claims(token, db, key).await?;
    let ctx = require_password_set(ctx, crate::security::password_change_required)?;
    Ok((ctx, claims))
}

/// Enables automatic login as the seeded admin when `SOULBEET_DEV_AUTOLOGIN=1`
/// is set. Compiled only in debug builds; release binaries never inspect this.
#[cfg(all(feature = "server", debug_assertions))]
fn dev_autologin_enabled() -> bool {
    std::env::var("SOULBEET_DEV_AUTOLOGIN").as_deref() == Ok("1")
}

#[cfg(all(feature = "server", debug_assertions))]
async fn load_dev_autologin_session(
    db: &SqlitePool,
) -> Result<(UserCtx, Claims), (StatusCode, String)> {
    let user = User::find_by_id_in(db, crate::security::SEEDED_ADMIN_ID)
        .await
        .map_err(|e| {
            tracing::error!("Dev auto-login failed to load seeded admin: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not check the session".to_string(),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                "Seeded admin user not found".to_string(),
            )
        })?;
    let now = chrono::Utc::now();
    let iat = now.timestamp().max(0) as usize;
    let exp = (now + chrono::Duration::days(1)).timestamp().max(0) as usize;
    let claims = Claims {
        sub: user.id.clone(),
        ver: user.credentials_version,
        jti: "dev-autologin".to_string(),
        iat,
        exp,
    };
    let ctx = UserCtx {
        user_id: user.id,
        username: user.username,
        role: user.role,
    };
    Ok((ctx, claims))
}

/// `load_session_claims`, also for a session that may only set a new
/// password.
#[cfg(feature = "server")]
pub(crate) async fn load_any_session_claims(
    token: Option<&str>,
    db: &SqlitePool,
    key: &SecretKey,
) -> Result<(UserCtx, Claims), (StatusCode, String)> {
    let Some(token) = token else {
        #[cfg(debug_assertions)]
        if dev_autologin_enabled() {
            return load_dev_autologin_session(db).await;
        }
        tracing::warn!("Auth failure: No auth token found in request cookies or Authorization header");
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
    let ctx = session_from(&claims, user.as_ref())?;
    Ok((ctx, claims))
}

/// Extracts a bearer JWT from an `Authorization: Bearer <token>` header,
/// matching the `"Bearer"` scheme case-insensitively.
#[cfg(feature = "server")]
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let trimmed = raw.trim();
    let (scheme, rest) = trimmed.split_once(|c: char| c.is_ascii_whitespace())?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Extracts the session JWT from the `auth_token` cookie or an
/// `Authorization: Bearer <jwt>` header.
#[cfg(feature = "server")]
pub(crate) fn extract_session_token(
    cookies: Option<&tower_cookies::Cookies>,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    if let Some(token) = cookies
        .and_then(|c| c.get(crate::AUTH_COOKIE_NAME))
        .map(|c| c.value().trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Some(token);
    }
    bearer_token(headers)
}

/// The token of the request's auth cookie or `Authorization: Bearer <jwt>` header.
#[cfg(feature = "server")]
fn session_token(
    parts: &axum::http::request::Parts,
) -> Result<Option<String>, (StatusCode, String)> {
    let cookies = parts.extensions.get::<tower_cookies::Cookies>();
    if let Some(token) = extract_session_token(cookies, &parts.headers) {
        return Ok(Some(token));
    }
    if cookies.is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            String::from("Missing cookie middleware"),
        ));
    }
    Ok(None)
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
        let token = session_token(parts)?;
        load_session(token.as_deref(), &DB, crate::secret_key::current())
            .await
            .map(AuthSession)
    }
}

#[cfg(feature = "server")]
impl<S> FromRequestParts<S> for PasswordChangeSession
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let token = session_token(parts)?;
        load_any_session_claims(token.as_deref(), &DB, crate::secret_key::current())
            .await
            .map(|(ctx, _)| PasswordChangeSession(ctx))
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
            oidc_issuer: None,
            oidc_subject: None,
            oidc_username: None,
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
        let token = create_token(SEEDED_ADMIN_ID, 0).unwrap();
        let (ctx, _) = load_any_session_claims(Some(&token), &db, &key())
            .await
            .unwrap();
        assert_eq!(require_admin(ctx).unwrap().role, Role::Admin);
    }

    #[test]
    fn a_session_that_must_set_a_password_does_nothing_else() {
        let held = require_password_set(ctx(Role::Admin), |id| id == "id-a").unwrap_err();
        assert_eq!(
            held,
            (StatusCode::FORBIDDEN, PASSWORD_CHANGE_REQUIRED.to_string())
        );
        let other = require_password_set(ctx(Role::Admin), |id| id == "id-b").unwrap();
        assert_eq!(other, ctx(Role::Admin));
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
        .key;
        let err = load_session(Some(&old), &db, &generated).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_session_signed_with_a_short_secret_key_is_refused() {
        // A forged session of the seeded admin at credentials version 0 under
        // a placeholder earlier versions shipped: it verifies under that key,
        // which is not the key in use.
        let db = db_with_user().await;
        let dir = tempfile::tempdir().unwrap();
        for placeholder in ["secret", "a_secret_key", "change-me-in-production"] {
            let set = SecretKey::new(placeholder.into());
            let forged = create_token_with(&set, SEEDED_ADMIN_ID, 0).unwrap();
            assert!(load_any_session_claims(Some(&forged), &db, &set)
                .await
                .is_ok());

            let in_use =
                crate::secret_key::resolve(Some(placeholder), dir.path(), "sqlite:soulbeet.db")
                    .unwrap()
                    .key;
            let err = load_session(Some(&forged), &db, &in_use).await.unwrap_err();
            assert_eq!(err.0, StatusCode::UNAUTHORIZED, "{placeholder}");
        }
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

    #[test]
    fn bearer_token_parses_case_insensitively_and_rejects_other_schemes() {
        let header_with = |val: &str| {
            let mut map = axum::http::HeaderMap::new();
            map.insert(axum::http::header::AUTHORIZATION, val.parse().unwrap());
            map
        };

        assert_eq!(
            bearer_token(&header_with("Bearer tok.123")).as_deref(),
            Some("tok.123")
        );
        assert_eq!(
            bearer_token(&header_with("bearer   tok.456  ")).as_deref(),
            Some("tok.456")
        );
        assert_eq!(
            bearer_token(&header_with("BEARER\ttok.789")).as_deref(),
            Some("tok.789")
        );
        assert_eq!(bearer_token(&header_with("Bearer ")), None);
        assert_eq!(bearer_token(&header_with("Bearer")), None);
        assert_eq!(bearer_token(&header_with("Bearertok")), None);
        assert_eq!(bearer_token(&header_with("Basic dXNlcjpwYXNz")), None);
        assert_eq!(bearer_token(&axum::http::HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn session_token_accepts_bearer_header_and_verifies_via_load_session() {
        let db = db_with_user().await;
        let jwt = create_token("u1", 0).unwrap();

        let (parts, ()) = axum::http::Request::builder()
            .header(axum::http::header::AUTHORIZATION, format!("bearer  {jwt}"))
            .body(())
            .unwrap()
            .into_parts();

        // Works even when no cookie middleware extension is present.
        let extracted = session_token(&parts).unwrap();
        assert_eq!(extracted.as_deref(), Some(jwt.as_str()));

        let ctx = load(extracted.as_deref(), &db).await.unwrap();
        assert_eq!(ctx.user_id, "u1");
        assert_eq!(ctx.username, "bob");

        // Without bearer and without cookie middleware, preserves missing middleware rejection.
        let (bare_parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        assert_eq!(
            session_token(&bare_parts).unwrap_err(),
            (StatusCode::UNAUTHORIZED, "Missing cookie middleware".into())
        );
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn dev_autologin_loads_seeded_admin_and_is_disabled_by_default() {
        assert!(!dev_autologin_enabled());

        let db = db_with_user().await;
        let (ctx, claims) = load_dev_autologin_session(&db).await.unwrap();
        assert_eq!(ctx.user_id, SEEDED_ADMIN_ID);
        assert_eq!(ctx.username, "admin");
        assert_eq!(ctx.role, Role::Admin);
        assert_eq!(claims.sub, SEEDED_ADMIN_ID);
        assert_eq!(claims.jti, "dev-autologin");
    }
}
