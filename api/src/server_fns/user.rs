#[cfg(feature = "server")]
use super::{
    auth::{start_session, SessionCookie},
    forbidden_error, refusal_error, server_error, ADMIN_ONLY,
};
use crate::auth::Role;
use crate::models;
#[cfg(feature = "server")]
use crate::{
    auth::UserCtx, config::CONFIG, services::evict_navidrome_client, AdminSession, AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
use dioxus::prelude::*;
#[cfg(feature = "server")]
use tower_cookies::Cookies;

/// Refuses a name no one could sign in with.
#[cfg(feature = "server")]
pub(crate) fn check_username_length(username: &str) -> Result<(), ServerFnError> {
    use models::user::{username_fits, MAX_USERNAME_BYTES};
    if username_fits(username) {
        Ok(())
    } else {
        Err(refusal_error(format!(
            "A username can be at most {MAX_USERNAME_BYTES} bytes"
        )))
    }
}

#[get("/api/users", _: AdminSession)]
pub async fn get_users() -> Result<Vec<models::user::UserSummary>, ServerFnError> {
    let users = models::user::User::get_all().await.map_err(server_error)?;
    Ok(users.iter().map(models::user::UserSummary::from).collect())
}

/// Creates an account. Allowed in every registration mode: the mode limits
/// strangers, not admins.
#[post("/api/users/create", _: AdminSession)]
pub async fn create_user(
    username: String,
    password: String,
) -> Result<models::user::User, ServerFnError> {
    use models::user::User;

    if username.is_empty() || password.is_empty() {
        return Err(refusal_error("Username and password are required"));
    }
    check_username_length(&username)?;
    User::create(&username, &password)
        .await
        .map_err(user_change_error)
}

#[post("/api/users/username", auth: AuthSession)]
pub async fn update_username(new_username: String) -> Result<(), ServerFnError> {
    if CONFIG.navidrome_url().is_some() {
        return Err(refusal_error(
            "Usernames follow Navidrome while NAVIDROME_URL is set, so they cannot be changed here.",
        ));
    }
    if new_username.trim().is_empty() {
        return Err(server_error("Username cannot be empty"));
    }
    check_username_length(new_username.trim())?;
    models::user::User::update_username(&auth.0.user_id, new_username.trim())
        .await
        .map_err(user_change_error)
}

/// Whether `actor` may set the password of `target_user_id`: their own, or any as an admin.
#[cfg(feature = "server")]
pub(crate) fn may_set_password(actor: &UserCtx, target_user_id: &str) -> bool {
    actor.user_id == target_user_id || actor.role.is_admin()
}

/// Refusals a person can act on are shown; database failures are not.
#[cfg(feature = "server")]
pub(crate) fn user_change_error(e: models::user::UserChangeError) -> ServerFnError {
    use models::user::UserChangeError;
    match e {
        UserChangeError::NotFound | UserChangeError::LastAdmin | UserChangeError::UsernameTaken => {
            refusal_error(e)
        }
        UserChangeError::Db(_) => server_error(e),
    }
}

#[post("/api/users/role", _: AdminSession)]
pub async fn set_user_role(user_id: String, role: Role) -> Result<(), ServerFnError> {
    models::user::User::set_role(&user_id, role)
        .await
        .map_err(user_change_error)
}

/// Sets the password of `user_id` for `actor`. Their own keeps its single
/// sign-on link. An admin's new password for another account removes it:
/// the admin gives the account to whoever gets the password, and the
/// identity linked to it may be the reason (a name someone else chose at
/// the provider). Either ends the account's sessions.
#[cfg(feature = "server")]
pub(crate) async fn set_password_in(
    db: &sqlx::SqlitePool,
    actor: &UserCtx,
    user_id: &str,
    password: &str,
) -> Result<(), ServerFnError> {
    use models::user::User;

    if password.is_empty() {
        return Err(refusal_error("Password cannot be empty"));
    }
    if !may_set_password(actor, user_id) {
        return Err(forbidden_error(ADMIN_ONLY));
    }
    if actor.user_id == user_id {
        User::update_password_in(db, user_id, password)
            .await
            .map(|_| ())
    } else {
        User::reset_password_in(db, user_id, password).await
    }
    .map_err(server_error)
}

#[post("/api/users/password", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn update_user_password(user_id: String, password: String) -> Result<(), ServerFnError> {
    // The bump ends every session of this user; the caller's own is renewed.
    let own_session = if user_id == auth.0.user_id {
        Some(SessionCookie::for_request(&headers, &uri)?)
    } else {
        None
    };
    set_password_in(&crate::db::DB, &auth.0, &user_id, &password).await?;
    crate::security::note_password_set(&user_id, &password);
    if let Some(session) = own_session {
        start_session(&cookies, &session, &user_id).await?;
    }
    Ok(())
}

#[delete("/api/users/delete", _: AdminSession)]
pub async fn delete_user(user_id: String) -> Result<(), ServerFnError> {
    models::user::User::delete(&user_id)
        .await
        .map_err(user_change_error)?;
    evict_navidrome_client(&user_id).await;
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    fn ctx(id: &str, role: Role) -> UserCtx {
        UserCtx {
            user_id: id.into(),
            username: id.into(),
            role,
        }
    }

    #[test]
    fn users_set_their_own_password_and_admins_anyone() {
        assert!(may_set_password(&ctx("a", Role::User), "a"));
        assert!(!may_set_password(&ctx("a", Role::User), "b"));
        assert!(may_set_password(&ctx("a", Role::Admin), "b"));
    }
}
