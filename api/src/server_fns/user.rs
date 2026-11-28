#[cfg(feature = "server")]
use super::{
    auth::{
        name_taken_by_account, prove_password_in, start_session, try_navidrome_auth,
        NavidromeAuthResult, SessionCookie,
    },
    failure_error, forbidden_error, refusal_error, server_error, ADMIN_ONLY,
};
use crate::auth::Role;
use crate::models;
#[cfg(feature = "server")]
use crate::{
    auth::UserCtx,
    config::CONFIG,
    login_limiter::{LoginLimiter, LOGIN_LIMITER},
    services::evict_navidrome_client,
    AdminSession, AuthSession, PasswordChangeSession,
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
    let user = User::create(&username, &password)
        .await
        .map_err(user_change_error)?;
    name_taken_by_account(&user.username);
    crate::core::folders::ensure_user_folder(&user).await;
    Ok(user)
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
        .map_err(user_change_error)?;
    name_taken_by_account(new_username.trim());
    Ok(())
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

/// Sets `user`'s own password once `current_password` proves the account is
/// the signed-in person's, as linking single sign-on does: a session left
/// open is not enough to give someone a password to the account. An account
/// with no password gets one from an admin. Keeps the account's single
/// sign-on link and ends its sessions. The seeded admin cannot keep the
/// default password. `navidrome` asks Navidrome about the name it is given,
/// as `prove_password_in` does.
#[cfg(feature = "server")]
pub(crate) async fn change_password_in<F>(
    db: &sqlx::SqlitePool,
    limiter: &LoginLimiter,
    user: &models::user::User,
    current_password: &str,
    password: &str,
    navidrome_configured: bool,
    navidrome: impl FnOnce(String) -> F,
) -> Result<(), ServerFnError>
where
    F: std::future::Future<Output = NavidromeAuthResult>,
{
    if password.is_empty() {
        return Err(refusal_error("Password cannot be empty"));
    }
    if crate::security::is_default_password(&user.id, password) {
        return Err(refusal_error(
            "Choose a password other than the default one.",
        ));
    }
    prove_password_in(
        limiter,
        user,
        current_password,
        navidrome_configured,
        std::time::Instant::now,
        navidrome,
    )
    .await?;
    models::user::User::update_password_in(db, &user.id, password)
        .await
        .map(|_| ())
        .map_err(server_error)
}

/// Changes the signed-in user's password. The change ends every session of
/// the account; this one is renewed. The one thing a session still on the
/// default password can do.
#[post("/api/users/password", auth: PasswordChangeSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn change_password(
    current_password: String,
    password: String,
) -> Result<(), ServerFnError> {
    let session = SessionCookie::for_request(&headers, &uri)?;
    let user = models::user::User::find_by_id(&auth.0.user_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error(models::user::UserChangeError::NotFound))?;
    let current_password = &current_password;
    change_password_in(
        &crate::db::DB,
        &LOGIN_LIMITER,
        &user,
        current_password,
        &password,
        CONFIG.navidrome_url().is_some(),
        |username| async move { try_navidrome_auth(&username, current_password).await },
    )
    .await?;
    crate::security::note_password_set(&user.id, &password);
    start_session(&cookies, &session, &user.id).await?;
    Ok(())
}

/// An admin's new password for another account. It also removes the
/// account's single sign-on link: the admin gives the account to whoever
/// gets the password, and the identity linked to it may be the reason (a
/// name someone else chose at the provider). Ends the account's sessions.
/// The admin's own password is changed with the current one, like anyone's.
#[cfg(feature = "server")]
pub(crate) async fn set_user_password_in(
    db: &sqlx::SqlitePool,
    actor: &UserCtx,
    user_id: &str,
    password: &str,
) -> Result<(), ServerFnError> {
    if !actor.role.is_admin() {
        return Err(forbidden_error(ADMIN_ONLY));
    }
    if actor.user_id == user_id {
        return Err(refusal_error(
            "Change your own password under Your Account, with your current password.",
        ));
    }
    if password.is_empty() {
        return Err(refusal_error("Password cannot be empty"));
    }
    models::user::User::reset_password_in(db, user_id, password)
        .await
        .map_err(server_error)
}

#[post("/api/users/password/set", auth: AdminSession)]
pub async fn set_user_password(user_id: String, password: String) -> Result<(), ServerFnError> {
    set_user_password_in(&crate::db::DB, &auth.0, &user_id, &password).await?;
    crate::security::note_password_set(&user_id, &password);
    Ok(())
}

/// Deletes the account `user_id` once the staged files of its pending discovery tracks are
/// removed (remove_staged_of_user_in): its folders and tracks go with it, so nothing would find
/// those files after. Nothing is removed when the account cannot be deleted.
#[cfg(feature = "server")]
pub(crate) async fn delete_user_in(
    db: &sqlx::SqlitePool,
    queue: &crate::import_queue::ImportQueue,
    user_id: &str,
) -> Result<(), ServerFnError> {
    use models::user::User;
    User::deletable_in(db, user_id)
        .await
        .map_err(user_change_error)?;
    crate::core::discovery::remove_staged_of_user_in(db, queue, user_id)
        .await
        .map_err(failure_error)?;
    User::delete_in(db, user_id)
        .await
        .map_err(user_change_error)
}

#[delete("/api/users/delete", _: AdminSession)]
pub async fn delete_user(user_id: String) -> Result<(), ServerFnError> {
    delete_user_in(&crate::db::DB, &crate::import_queue::IMPORT_QUEUE, &user_id).await?;
    crate::security::note_user_deleted(&user_id);
    evict_navidrome_client(&user_id).await;
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::NO_PASSWORD;
    use crate::core::delete_guard::test_support::Library;
    use crate::db::test_db;
    use crate::import_queue::test_support::stub_queue;
    use crate::server_fns::{FORBIDDEN_CODE, REFUSAL_CODE};
    use models::user::User;
    use std::time::Instant;

    const ISSUER: &str = "https://id.example.com";

    fn ctx(user: &User) -> UserCtx {
        UserCtx {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
        }
    }

    fn code<T: std::fmt::Debug>(result: Result<T, ServerFnError>) -> u16 {
        match result {
            Err(ServerFnError::ServerError { code, .. }) => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn reread(db: &sqlx::SqlitePool, user: &User) -> User {
        User::find_by_id_in(db, &user.id).await.unwrap().unwrap()
    }

    /// `user` changes their password from `current` to `password` while
    /// Navidrome, set up, answers `navidrome`.
    async fn change(
        db: &sqlx::SqlitePool,
        limiter: &LoginLimiter,
        user: &User,
        current: &str,
        password: &str,
        navidrome: NavidromeAuthResult,
    ) -> Result<(), ServerFnError> {
        change_password_in(db, limiter, user, current, password, true, |_| async {
            navidrome
        })
        .await
    }

    #[tokio::test]
    async fn a_session_alone_cannot_set_a_password_and_then_link() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();

        // Bob's account follows his Navidrome password, and he left a
        // session open. Whoever finds it does not know his password.
        let bob = User::create_in(&db, "bob", "bobs-pw").await.unwrap();
        User::link_navidrome_in(&db, &bob.id, "enc", "connected")
            .await
            .unwrap();
        let bob = reread(&db, &bob).await;
        for guess in ["", "guess"] {
            let refused = change(
                &db,
                &limiter,
                &bob,
                guess,
                "x",
                NavidromeAuthResult::AuthFailed,
            )
            .await;
            assert_eq!(code(refused), REFUSAL_CODE);
        }
        let after = reread(&db, &bob).await;
        assert!(after.password_matches("bobs-pw"));
        assert_eq!(after.credentials_version, bob.credentials_version);
        // So the password they wanted does not start a link either.
        let link = prove_password_in(&limiter, &after, "x", false, Instant::now, |_| async {
            NavidromeAuthResult::NotConfigured
        })
        .await;
        assert_eq!(code(link), REFUSAL_CODE);

        // An account whose only password is its Navidrome one.
        let navi = User::create_oidc_in(&db, "navi", ISSUER, "sub-navi")
            .await
            .unwrap();
        User::update_navidrome_token_in(&db, &navi.id, Some("enc"), "connected")
            .await
            .unwrap();
        User::unlink_oidc_identity_in(&db, &navi.id).await.unwrap();
        let navi = reread(&db, &navi).await;
        let refused = change(
            &db,
            &limiter,
            &navi,
            "guess",
            "x",
            NavidromeAuthResult::AuthFailed,
        )
        .await;
        assert_eq!(code(refused), REFUSAL_CODE);
        assert!(!reread(&db, &navi).await.has_password());

        // An account with no password at all gets one from an admin, never
        // from a session, whatever Navidrome would say.
        let sso = User::create_oidc_in(&db, "sso", ISSUER, "sub-sso")
            .await
            .unwrap();
        let refused = change(&db, &limiter, &sso, "", "x", NavidromeAuthResult::Success)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains(NO_PASSWORD), "{refused}");
        assert!(!reread(&db, &sso).await.has_password());
    }

    #[tokio::test]
    async fn the_password_the_login_form_takes_changes_it() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let ann = User::create_in(&db, "ann", "ann-pw").await.unwrap();
        change(
            &db,
            &limiter,
            &ann,
            "ann-pw",
            "",
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap_err();
        change(
            &db,
            &limiter,
            &ann,
            "ann-pw",
            "new",
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap();
        let after = reread(&db, &ann).await;
        assert!(after.password_matches("new"));
        assert_eq!(after.credentials_version, ann.credentials_version + 1);

        // Its Navidrome password, for an account that has only that.
        let navi = User::create_oidc_in(&db, "navi", ISSUER, "sub-navi")
            .await
            .unwrap();
        User::update_navidrome_token_in(&db, &navi.id, Some("enc"), "connected")
            .await
            .unwrap();
        let navi = reread(&db, &navi).await;
        change(
            &db,
            &limiter,
            &navi,
            "navi-pw",
            "own",
            NavidromeAuthResult::Success,
        )
        .await
        .unwrap();
        assert!(reread(&db, &navi).await.password_matches("own"));
    }

    #[tokio::test]
    async fn the_seeded_admin_cannot_keep_the_default_password() {
        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let admin = User::find_by_id_in(&db, crate::security::SEEDED_ADMIN_ID)
            .await
            .unwrap()
            .unwrap();
        let refused = change(
            &db,
            &limiter,
            &admin,
            "admin",
            "admin",
            NavidromeAuthResult::NotConfigured,
        )
        .await;
        assert_eq!(code(refused), REFUSAL_CODE);
        assert_eq!(
            reread(&db, &admin).await.credentials_version,
            admin.credentials_version
        );
        change(
            &db,
            &limiter,
            &admin,
            "admin",
            "chosen",
            NavidromeAuthResult::NotConfigured,
        )
        .await
        .unwrap();
        assert!(reread(&db, &admin).await.password_matches("chosen"));
    }

    #[tokio::test]
    async fn an_admin_sets_the_password_of_other_accounts() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "ann-pw").await.unwrap();
        let bob = User::create_in(&db, "bob", "bob-pw").await.unwrap();
        assert_eq!(
            code(set_user_password_in(&db, &ctx(&ann), &bob.id, "x").await),
            FORBIDDEN_CODE
        );
        let mut admin = ctx(&ann);
        admin.role = Role::Admin;
        // Their own takes the current one, under Your Account.
        assert_eq!(
            code(set_user_password_in(&db, &admin, &ann.id, "x").await),
            REFUSAL_CODE
        );
        assert!(reread(&db, &ann).await.password_matches("ann-pw"));
        set_user_password_in(&db, &admin, &bob.id, "x")
            .await
            .unwrap();
        assert!(reread(&db, &bob).await.password_matches("x"));
    }

    #[tokio::test]
    async fn deleting_a_user_removes_their_staged_discovery_files_and_keeps_their_music() {
        let lib = Library::new().await;
        lib.write("alice/Discovery/Explore/Marrow Choir/03.flac", b"audio");
        let alices = lib
            .root
            .join("music/alice/Discovery/Explore/Marrow Choir/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &alices, None)
            .await;
        lib.write("bob/Discovery/Explore/04.flac", b"audio");
        let bobs = lib.root.join("music/bob/Discovery/Explore/04.flac");
        lib.pending_track("t2", "bob-music", "Explore", &bobs, None)
            .await;
        let profile =
            std::fs::canonicalize(lib.root.join("music/alice/Discovery/Explore")).unwrap();
        let staged = std::fs::canonicalize(&alices).unwrap();
        let (queue, stub) = stub_queue();
        delete_user_in(&lib.db, &queue, "alice").await.unwrap();
        // Its item leaves the profile folder's library too.
        assert_eq!(stub.forgets_once(1).await, vec![(profile, staged)]);
        assert!(!alices.exists());
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert!(User::find_by_id_in(&lib.db, "alice")
            .await
            .unwrap()
            .is_none());
        assert!(bobs.exists());
        assert_eq!(lib.track_status("t2").await, "Pending");
    }

    #[tokio::test]
    async fn a_refused_user_delete_keeps_their_staged_discovery_files() {
        let lib = Library::new().await;
        let admin: String = sqlx::query_scalar("SELECT id FROM users WHERE role = 'admin'")
            .fetch_one(&lib.db)
            .await
            .unwrap();
        lib.folder(&admin, "admin-music", "music/admin", false)
            .await;
        lib.write("admin/Discovery/Explore/03.flac", b"audio");
        let file = lib.root.join("music/admin/Discovery/Explore/03.flac");
        lib.pending_track("t1", "admin-music", "Explore", &file, None)
            .await;
        let (queue, stub) = stub_queue();
        assert_eq!(
            code(delete_user_in(&lib.db, &queue, &admin).await),
            REFUSAL_CODE
        );
        assert!(stub.forgets().is_empty());
        assert!(file.exists());
        assert_eq!(lib.track_status("t1").await, "Pending");
    }
}
