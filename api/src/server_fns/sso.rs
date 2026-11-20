//! Single sign-on from inside a signed-in account: what it can do, linking
//! it to an identity at the provider, and removing that link.

use crate::auth::SignInMethods;
use dioxus::prelude::*;

#[cfg(feature = "server")]
use super::{
    auth::{link_proof, prove_password, start_session, SessionCookie},
    forbidden_error, refusal_error, server_error, ADMIN_ONLY,
};
#[cfg(feature = "server")]
use crate::{
    auth::{SsoError, SsoLinkResult, UserCtx},
    config::CONFIG,
    db::DB,
    models::user::{User, UserChangeError},
    oidc::{loggable, LinkStartError},
    AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use tower_cookies::Cookies;

/// Whether the account can still sign in without the provider: with a
/// password of its own, or with a stored Navidrome login while Navidrome is
/// set up (that login then sets its password).
#[cfg(feature = "server")]
fn keeps_a_way_in(user: &User, navidrome_configured: bool) -> bool {
    user.has_password() || (navidrome_configured && user.navidrome_token.is_some())
}

#[cfg(feature = "server")]
async fn session_user(user_id: &str) -> Result<User, ServerFnError> {
    User::find_by_id(user_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| server_error(UserChangeError::NotFound))
}

#[get("/api/users/sign_in_methods", auth: AuthSession)]
pub async fn get_sign_in_methods() -> Result<SignInMethods, ServerFnError> {
    let user = session_user(&auth.0.user_id).await?;
    let navidrome = CONFIG.navidrome_url().is_some();
    Ok(SignInMethods {
        sso_provider: CONFIG.oidc().map(|oidc| oidc.provider_name.clone()),
        sso_name: user.oidc_username.clone(),
        sso_unlink_allowed: keeps_a_way_in(&user, navidrome),
        sso_link_proof: link_proof(&user, navidrome),
        navidrome,
    })
}

/// Starts linking the signed-in account to an identity at the provider,
/// once `password` proves the account is the signed-in person's (see
/// `LinkProof`). The browser goes to the address returned; the provider
/// sends it back to the callback, which links the identity while this
/// browser is still signed in to the account, and then shows the outcome in
/// Settings. A POST server function like the other changes, protected from
/// other sites the same way: the session cookie is SameSite=Lax, so a form
/// on another site posts here without it.
#[post("/api/auth/oidc/link", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn start_sso_link(password: String) -> Result<String, ServerFnError> {
    let Some(config) = CONFIG.oidc() else {
        return Err(refusal_error(SsoError::Disabled.message()));
    };
    let user = session_user(&auth.0.user_id).await?;
    if user.oidc_subject.is_some() {
        return Err(refusal_error(SsoLinkResult::AlreadyLinked.message()));
    }
    prove_password(&user, &password).await?;
    match crate::oidc::begin_link(&cookies, config, &user.id, &headers, &uri).await {
        Ok(url) => Ok(url.into()),
        Err(LinkStartError::Elsewhere(callback)) => Err(refusal_error(format!(
            "Single sign-on comes back to {callback}. Open Soulbeet there to link it."
        ))),
        Err(LinkStartError::Failed(e)) => {
            tracing::error!("Linking OIDC failed: {e}");
            Err(refusal_error(
                "Cannot start single sign-on. The server log has the details.",
            ))
        }
    }
}

#[cfg(feature = "server")]
const NOT_LINKED: &str = "This account is not linked to single sign-on.";
#[cfg(feature = "server")]
const NO_OTHER_WAY_IN: &str = "Set a password or connect Navidrome first, so this account can \
                               still sign in without single sign-on.";

/// Removes the link of `user_id` for `actor`: an admin for any account, a
/// user for their own as long as it keeps another way in.
#[cfg(feature = "server")]
pub(crate) async fn unlink_sso_in(
    db: &sqlx::SqlitePool,
    actor: &UserCtx,
    user_id: &str,
    navidrome_configured: bool,
) -> Result<(), ServerFnError> {
    let own = actor.user_id == user_id;
    if !own && !actor.role.is_admin() {
        return Err(forbidden_error(ADMIN_ONLY));
    }
    let user = User::find_by_id_in(db, user_id)
        .await
        .map_err(server_error)?
        .ok_or_else(|| refusal_error(UserChangeError::NotFound))?;
    if user.oidc_subject.is_none() {
        return Err(refusal_error(NOT_LINKED));
    }
    if own && !keeps_a_way_in(&user, navidrome_configured) {
        return Err(refusal_error(NO_OTHER_WAY_IN));
    }
    if !User::unlink_oidc_identity_in(db, user_id)
        .await
        .map_err(server_error)?
    {
        return Err(refusal_error(NOT_LINKED));
    }
    tracing::info!(
        "Removed the single sign-on link of the account {} (by {})",
        loggable(&user.username),
        loggable(&actor.username)
    );
    Ok(())
}

/// Removes an account's single sign-on link, which ends its sessions. The
/// caller's own session is renewed.
#[post("/api/users/sso/unlink", auth: AuthSession, cookies: Cookies, headers: HeaderMap, uri: Uri)]
pub async fn unlink_sso(user_id: String) -> Result<(), ServerFnError> {
    let own_session = if user_id == auth.0.user_id {
        Some(SessionCookie::for_request(&headers, &uri)?)
    } else {
        None
    };
    unlink_sso_in(&DB, &auth.0, &user_id, CONFIG.navidrome_url().is_some()).await?;
    if let Some(session) = own_session {
        start_session(&cookies, &session, &user_id).await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::db::test_db;
    use crate::server_fns::{FORBIDDEN_CODE, REFUSAL_CODE};

    fn ctx(user: &User) -> UserCtx {
        UserCtx {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
        }
    }

    fn code(result: Result<(), ServerFnError>) -> u16 {
        match result {
            Err(ServerFnError::ServerError { code, .. }) => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn linked_sso_account(db: &sqlx::SqlitePool, name: &str) -> User {
        User::create_oidc_in(db, name, "https://id.example.com", &format!("sub-{name}"))
            .await
            .unwrap()
    }

    async fn reread(db: &sqlx::SqlitePool, user: &User) -> User {
        User::find_by_id_in(db, &user.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn an_owner_unlinks_only_an_account_that_keeps_a_way_in() {
        let db = test_db::migrated().await;
        let sso_only = linked_sso_account(&db, "sso").await;
        // Unlinking would lock its owner out.
        for navidrome in [false, true] {
            assert_eq!(
                code(unlink_sso_in(&db, &ctx(&sso_only), &sso_only.id, navidrome).await),
                REFUSAL_CODE
            );
        }
        assert!(reread(&db, &sso_only).await.oidc_subject.is_some());

        // A stored Navidrome login is a way in only while Navidrome is set up.
        User::update_navidrome_token_in(&db, &sso_only.id, Some("enc"), "connected")
            .await
            .unwrap();
        assert_eq!(
            code(unlink_sso_in(&db, &ctx(&sso_only), &sso_only.id, false).await),
            REFUSAL_CODE
        );
        unlink_sso_in(&db, &ctx(&sso_only), &sso_only.id, true)
            .await
            .unwrap();
        let after = reread(&db, &sso_only).await;
        assert_eq!((after.oidc_issuer, after.oidc_subject), (None, None));
        assert_eq!(after.credentials_version, sso_only.credentials_version + 1);

        // An account with its own password.
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        User::link_oidc_identity_in(&db, &ann.id, "https://id.example.com", "sub-ann", "ann")
            .await
            .unwrap();
        unlink_sso_in(&db, &ctx(&ann), &ann.id, false)
            .await
            .unwrap();
        assert!(reread(&db, &ann).await.oidc_subject.is_none());
        // Nothing left to unlink.
        assert_eq!(
            code(unlink_sso_in(&db, &ctx(&ann), &ann.id, false).await),
            REFUSAL_CODE
        );
    }

    #[tokio::test]
    async fn only_an_admin_unlinks_another_account() {
        let db = test_db::migrated().await;
        let victim = linked_sso_account(&db, "victim").await;
        let eve = User::create_in(&db, "eve", "pw").await.unwrap();
        assert_eq!(eve.role, Role::User);
        assert_eq!(
            code(unlink_sso_in(&db, &ctx(&eve), &victim.id, true).await),
            FORBIDDEN_CODE
        );
        assert_eq!(reread(&db, &victim).await.credentials_version, 0);

        // An admin fixes a wrong link, even one that leaves no way in: they
        // can set a password next.
        let mut admin = ctx(&eve);
        admin.role = Role::Admin;
        unlink_sso_in(&db, &admin, &victim.id, false).await.unwrap();
        let after = reread(&db, &victim).await;
        assert!(after.oidc_subject.is_none());
        assert_eq!(after.credentials_version, 1);
        assert_eq!(
            code(unlink_sso_in(&db, &admin, "nobody", false).await),
            REFUSAL_CODE
        );
    }
}
