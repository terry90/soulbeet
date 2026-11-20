//! The account a provider identity signs in to, and linking an identity to
//! the account signed in.

use sqlx::SqlitePool;

use super::loggable;
use super::token::Identity;
use crate::auth::{SsoError, SsoLinkResult};
use crate::config::{OidcConfig, CONFIG};
use crate::db::DB;
use crate::models::user::{NameMatch, OidcLink, User, UserChangeError};
use crate::oauth::codes::{still_signed_in, SessionCheck};
use crate::secret_key::SecretKey;
use crate::server_fns::guard::load_session_claims;

/// The account `identity` signs in to.
pub(super) async fn sign_in_account(
    identity: &Identity,
    config: &OidcConfig,
) -> Result<User, SsoError> {
    let user = find_or_create_account(&DB, identity, config).await?;
    Ok(sync_navidrome_status(&DB, user, CONFIG.navidrome_url().is_some()).await)
}

/// Links `identity` to the account `user_id`, which started the link, when
/// `session_token` is still a valid session of that account. Nobody links
/// an identity to an account they are not signed in to, and an identity
/// stays on one account. The link ends the account's sessions; the account
/// is returned as it is now, for this browser's new session.
pub(super) async fn link_from_session(
    db: &SqlitePool,
    key: &SecretKey,
    session_token: Option<&str>,
    user_id: &str,
    identity: &Identity,
) -> Result<User, SsoLinkResult> {
    let check = match load_session_claims(session_token, db, key).await {
        Ok((session, claims)) if session.user_id == user_id => SessionCheck {
            credentials_version: claims.ver,
            session_id: claims.jti,
        },
        _ => return Err(session_changed()),
    };
    link_for_session_in(db, &check, user_id, identity).await
}

fn session_changed() -> SsoLinkResult {
    tracing::warn!(
        "Linking OIDC refused: this browser is no longer signed in to the account that started it"
    );
    SsoLinkResult::SessionChanged
}

/// The link of `link_from_session` once the session is read, in one BEGIN
/// IMMEDIATE transaction that checks the session `check` again first: a
/// password change or a sign-out that commits after the session was read
/// still stops the link.
async fn link_for_session_in(
    db: &SqlitePool,
    check: &SessionCheck,
    user_id: &str,
    identity: &Identity,
) -> Result<User, SsoLinkResult> {
    let failed = |e: String| {
        tracing::error!("Linking OIDC failed: {e}");
        SsoLinkResult::Failed
    };

    let mut tx = db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !still_signed_in(&mut tx, user_id, check)
        .await
        .map_err(|e| failed(e.to_string()))?
    {
        return Err(session_changed());
    }
    if let Some(owner) =
        User::find_by_oidc_identity_in(&mut *tx, &identity.issuer, &identity.subject)
            .await
            .map_err(failed)?
    {
        return Err(if owner.id == user_id {
            SsoLinkResult::AlreadyLinked
        } else {
            tracing::warn!(
                "Linking OIDC refused: the identity {} is already linked to the account {}",
                identity.for_log(),
                loggable(&owner.username)
            );
            SsoLinkResult::IdentityTaken
        });
    }
    match User::link_oidc_identity_in(
        &mut *tx,
        user_id,
        &identity.issuer,
        &identity.subject,
        &identity.username,
    )
    .await
    .map_err(failed)?
    {
        OidcLink::Linked => {}
        OidcLink::AccountLinked => return Err(SsoLinkResult::AlreadyLinked),
        OidcLink::IdentityTaken => return Err(SsoLinkResult::IdentityTaken),
    }
    tx.commit().await.map_err(|e| failed(e.to_string()))?;

    let user = User::find_by_id_in(db, user_id)
        .await
        .map_err(failed)?
        .ok_or(SsoLinkResult::SessionChanged)?;
    tracing::info!(
        "Linked the account {} to the OIDC identity {} from its session",
        loggable(&user.username),
        identity.for_log()
    );
    Ok(user)
}

/// An account without a stored Navidrome login cannot reach Navidrome. Say
/// so when `navidrome_configured`, so the UI points to Connect Navidrome. An
/// account that has one keeps the status its last use of it gave.
async fn sync_navidrome_status(
    db: &SqlitePool,
    mut user: User,
    navidrome_configured: bool,
) -> User {
    use shared::system::NavidromeStatus;

    if user.navidrome_token.is_some() {
        return user;
    }
    let status = if navidrome_configured {
        NavidromeStatus::NotLinked
    } else {
        NavidromeStatus::Unknown
    };
    if user.navidrome_status != status.as_str() {
        match User::update_navidrome_token_in(db, &user.id, None, status.as_str()).await {
            Ok(()) => user.navidrome_status = status.as_str().to_string(),
            Err(e) => tracing::warn!("OIDC: cannot update the Navidrome status: {e}"),
        }
    }
    user
}

/// Which account an identity signs in to.
#[derive(Debug)]
enum Account {
    /// Already linked to this identity
    Linked(User),
    /// Has the username (ignoring case) and no identity yet, and the admin
    /// lets a first sign-in link such an account: link it
    Unlinked(User),
    /// Has the username and no identity yet: its owner links it from a
    /// session
    Exists,
    /// Nobody has the username: create the account
    New,
    /// The username belongs to an account linked to another identity, or
    /// to several accounts that differ only in case
    Conflict,
    /// Nobody has the username and accounts are not created
    NotRegistered,
}

/// `same_name`: the accounts whose username matches the identity's.
fn choose_account(by_identity: Option<User>, same_name: NameMatch, config: &OidcConfig) -> Account {
    if let Some(user) = by_identity {
        return Account::Linked(user);
    }
    match same_name {
        NameMatch::One(user) if user.oidc_subject.is_none() && config.link_by_username => {
            Account::Unlinked(*user)
        }
        NameMatch::One(user) if user.oidc_subject.is_none() => Account::Exists,
        NameMatch::One(_) | NameMatch::Several => Account::Conflict,
        NameMatch::None if config.auto_register => Account::New,
        NameMatch::None => Account::NotRegistered,
    }
}

fn db_error(e: String) -> SsoError {
    tracing::error!("OIDC sign-in failed: {e}");
    SsoError::Failed
}

async fn find_or_create_account(
    db: &SqlitePool,
    identity: &Identity,
    config: &OidcConfig,
) -> Result<User, SsoError> {
    let by_identity = User::find_by_oidc_identity_in(db, &identity.issuer, &identity.subject)
        .await
        .map_err(db_error)?;
    let same_name = match by_identity {
        Some(_) => NameMatch::None,
        None => User::find_by_name_in(db, &identity.username)
            .await
            .map_err(db_error)?,
    };
    enter_account(db, identity, choose_account(by_identity, same_name, config)).await
}

/// Signs `identity` in to `account`, as chosen from what the database held
/// a moment ago.
async fn enter_account(
    db: &SqlitePool,
    identity: &Identity,
    account: Account,
) -> Result<User, SsoError> {
    match account {
        Account::Linked(user) => with_current_name(db, identity, user).await,
        Account::Unlinked(user) => {
            match User::link_oidc_identity_in(
                db,
                &user.id,
                &identity.issuer,
                &identity.subject,
                &identity.username,
            )
            .await
            .map_err(db_error)?
            {
                OidcLink::Linked => {}
                // Linked by a concurrent sign-in meanwhile.
                OidcLink::AccountLinked | OidcLink::IdentityTaken => {
                    return linked_meanwhile(db, identity, || conflict(identity)).await
                }
            }
            tracing::info!(
                "Linked the account {} to the OIDC identity {} by its name \
                 (OIDC_LINK_BY_USERNAME)",
                loggable(&user.username),
                identity.for_log()
            );
            // Read back: the link changed the credentials version.
            User::find_by_id_in(db, &user.id)
                .await
                .map_err(db_error)?
                .ok_or(SsoError::Failed)
        }
        Account::New => {
            match User::create_oidc_in(db, &identity.username, &identity.issuer, &identity.subject)
                .await
            {
                Ok(user) => {
                    tracing::info!(
                        "Created the account {} on OIDC sign-in",
                        loggable(&user.username)
                    );
                    Ok(user)
                }
                // Created by a concurrent sign-in or registration meanwhile:
                // the name, or the identity, is taken now.
                Err(UserChangeError::UsernameTaken) => {
                    linked_meanwhile(db, identity, || conflict(identity)).await
                }
                Err(e) => linked_meanwhile(db, identity, || db_error(e.to_string())).await,
            }
        }
        Account::Exists => {
            tracing::warn!(
                "OIDC sign-in refused: the account {} exists and is not linked to this OIDC \
                 identity. Its owner links it in Settings after a password sign-in",
                loggable(&identity.username)
            );
            Err(SsoError::AccountExists)
        }
        Account::Conflict => Err(conflict(identity)),
        Account::NotRegistered => {
            tracing::info!(
                "OIDC sign-in refused: no account {}, and OIDC_AUTO_REGISTER is off \
                 (it is by default unless REGISTRATION=open)",
                loggable(&identity.username)
            );
            Err(SsoError::NotRegistered)
        }
    }
}

/// `user`, linked to `identity`, showing the name the provider gives it now.
async fn with_current_name(
    db: &SqlitePool,
    identity: &Identity,
    mut user: User,
) -> Result<User, SsoError> {
    if user.oidc_username.as_deref() != Some(identity.username.as_str()) {
        User::note_oidc_username_in(db, &user.id, &identity.username)
            .await
            .map_err(db_error)?;
        user.oidc_username = Some(identity.username.clone());
    }
    Ok(user)
}

/// After a link or an account creation failed, maybe because it lost a
/// race: the account another sign-in of `identity` linked or created
/// meanwhile, else the error `otherwise` gives.
async fn linked_meanwhile(
    db: &SqlitePool,
    identity: &Identity,
    otherwise: impl FnOnce() -> SsoError,
) -> Result<User, SsoError> {
    match User::find_by_oidc_identity_in(db, &identity.issuer, &identity.subject)
        .await
        .map_err(db_error)?
    {
        Some(user) => with_current_name(db, identity, user).await,
        None => Err(otherwise()),
    }
}

fn conflict(identity: &Identity) -> SsoError {
    tracing::warn!(
        "OIDC sign-in refused: the account {} is linked to another OIDC identity, \
         or several accounts have that name in other letter case",
        loggable(&identity.username)
    );
    SsoError::Conflict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::oidc::testing::{identity, identity_at, test_config, ISSUER};
    use crate::server_fns::guard::load_session;
    use crate::test_log::LogCapture;

    /// A first or later sign-in with the default settings, and auto-register.
    async fn sign_in(db: &SqlitePool, subject: &str, username: &str) -> Result<User, SsoError> {
        find_or_create_account(db, &identity(subject, username), &test_config()).await
    }

    /// The same, for an admin who chose OIDC_LINK_BY_USERNAME=true.
    async fn sign_in_linking_by_name(
        db: &SqlitePool,
        subject: &str,
        username: &str,
    ) -> Result<User, SsoError> {
        let config = OidcConfig {
            link_by_username: true,
            ..test_config()
        };
        find_or_create_account(db, &identity(subject, username), &config).await
    }

    #[tokio::test]
    async fn creates_an_account_then_finds_it_by_subject() {
        let db = test_db::migrated().await;
        let created = sign_in(&db, "sub-a", "alice").await.unwrap();
        assert!(!created.has_password());
        assert_eq!(created.oidc_username.as_deref(), Some("alice"));
        // Renamed at the provider: still the same account, which shows the
        // identity's new name.
        let again = sign_in(&db, "sub-a", "alice-renamed").await.unwrap();
        assert_eq!(again.id, created.id);
        assert_eq!(again.username, "alice");
        assert_eq!(again.oidc_username.as_deref(), Some("alice-renamed"));
        assert_eq!(
            reread(&db, &again).await.oidc_username.as_deref(),
            Some("alice-renamed")
        );
    }

    #[tokio::test]
    async fn the_same_subject_at_another_issuer_is_another_identity() {
        // Providers such as Gitea number their users, so after a switch of
        // provider the new one's user "1" must not get the old one's account.
        let db = test_db::migrated().await;
        let alice = sign_in(&db, "1", "alice").await.unwrap();
        assert_eq!(alice.oidc_issuer.as_deref(), Some(ISSUER));

        let other = "https://other.example.com";
        let stranger =
            find_or_create_account(&db, &identity_at(other, "1", "mallory"), &test_config())
                .await
                .unwrap();
        assert_ne!(stranger.id, alice.id);
        // Under alice's name, it is not let in either.
        assert_eq!(
            find_or_create_account(&db, &identity_at(other, "2", "alice"), &test_config())
                .await
                .unwrap_err(),
            SsoError::Conflict
        );
        assert_eq!(sign_in(&db, "1", "alice").await.unwrap().id, alice.id);
    }

    #[tokio::test]
    async fn a_new_account_is_a_plain_user_even_the_first_one() {
        let db = test_db::migrated().await;
        let user = sign_in(&db, "sub-b", "bob").await.unwrap();
        assert_eq!(user.role, crate::auth::Role::User);
        // With no account at all, whoever the provider sends first does not
        // become the admin.
        sqlx::query("DELETE FROM users").execute(&db).await.unwrap();
        let first = sign_in(&db, "sub-c", "carol").await.unwrap();
        assert_eq!(first.role, crate::auth::Role::User);
    }

    /// The account as it is in the database now.
    async fn reread(db: &SqlitePool, user: &User) -> User {
        User::find_by_id_in(db, &user.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_provider_user_named_admin_does_not_get_the_admin_account() {
        // A provider where users pick their own name (Pocket ID by default):
        // someone renames themselves to the seeded admin's name.
        let db = test_db::migrated().await;
        let NameMatch::One(admin) = User::find_by_name_in(&db, "admin").await.unwrap() else {
            panic!("the seeded admin");
        };
        assert_eq!(
            sign_in(&db, "mallory", "Admin").await.unwrap_err(),
            SsoError::AccountExists
        );
        let after = reread(&db, &admin).await;
        assert_eq!(after.oidc_subject, None);
        assert_eq!(after.credentials_version, admin.credentials_version);
    }

    #[tokio::test]
    async fn names_from_the_provider_cannot_forge_log_lines() {
        let db = test_db::migrated().await;
        let log = LogCapture::start();
        let forged = "carol\nINFO User 'admin' signed in";

        // Created under that name, refused under it for another identity,
        // and unlinked by an admin.
        let carol = sign_in(&db, "sub-carol", forged).await.unwrap();
        assert_eq!(
            sign_in(&db, "sub-other", forged).await.unwrap_err(),
            SsoError::Conflict
        );
        let NameMatch::One(admin) = User::find_by_name_in(&db, "admin").await.unwrap() else {
            panic!("the seeded admin");
        };
        let actor = crate::auth::UserCtx {
            user_id: admin.id.clone(),
            username: admin.username.clone(),
            role: admin.role,
        };
        crate::server_fns::sso::unlink_sso_in(&db, &actor, &carol.id, false)
            .await
            .unwrap();

        let lines = log.lines();
        assert_eq!(
            lines.iter().filter(|l| l.contains("carol")).count(),
            3,
            "{lines:?}"
        );
        for line in lines {
            assert!(!line.contains('\n'), "{line}");
        }
    }

    #[tokio::test]
    async fn a_name_planted_before_the_first_sign_in_is_not_joined() {
        // Eve registers (or renames herself to) "bob" before Bob's first
        // single sign-on: Bob is not put into Eve's account.
        let db = test_db::migrated().await;
        let eve = User::create_in(&db, "bob", "eve-pw").await.unwrap();
        assert_eq!(
            sign_in(&db, "sub-bob", "bob").await.unwrap_err(),
            SsoError::AccountExists
        );
        let after = reread(&db, &eve).await;
        assert_eq!(after.oidc_subject, None);
        assert_eq!(after.credentials_version, eve.credentials_version);
        // Refused with auto-register off too: the name is taken either way.
        let closed = OidcConfig {
            auto_register: false,
            ..test_config()
        };
        assert_eq!(
            find_or_create_account(&db, &identity("sub-bob", "bob"), &closed)
                .await
                .unwrap_err(),
            SsoError::AccountExists
        );
    }

    #[tokio::test]
    async fn links_an_existing_account_once_ignoring_case_when_allowed() {
        let db = test_db::migrated().await;
        let local = User::create_in(&db, "alice", "pw").await.unwrap();
        let old_session = session_of(&local);
        let linked = sign_in_linking_by_name(&db, "sub-a", "Alice")
            .await
            .unwrap();
        assert_eq!(linked.id, local.id);
        assert_eq!(linked.oidc_subject.as_deref(), Some("sub-a"));
        assert_eq!(linked.oidc_username.as_deref(), Some("Alice"));
        // Linking ends the sessions the account had, so whoever held the
        // name before is signed out.
        assert_eq!(linked.credentials_version, local.credentials_version + 1);
        assert!(load_session(Some(&old_session), &db, &session_key())
            .await
            .is_err());
        assert!(linked.password_matches("pw"));

        // Another identity with that name does not get in.
        assert_eq!(
            sign_in_linking_by_name(&db, "sub-other", "alice")
                .await
                .unwrap_err(),
            SsoError::Conflict
        );
        // And the linked identity keeps signing in to it.
        assert_eq!(sign_in(&db, "sub-a", "alice").await.unwrap().id, local.id);
    }

    /// Two first sign-ins of `ann`'s identity at once: each chose what to do
    /// before the other wrote. `planned` is what the slower one chose.
    async fn first_sign_ins_at_once(
        db: &SqlitePool,
        config: &OidcConfig,
        planned: Account,
        ann: &Identity,
    ) -> Result<User, SsoError> {
        // The faster one finishes first.
        let first = find_or_create_account(db, ann, config).await.unwrap();
        let second = enter_account(db, ann, planned).await;
        if let Ok(user) = &second {
            assert_eq!(user.id, first.id);
        }
        second
    }

    #[tokio::test]
    async fn two_first_sign_ins_of_one_identity_end_in_its_one_account() {
        let db = test_db::migrated().await;
        let ann = identity("sub-ann", "ann");

        // Both create the account: the second one signs in to it.
        let user = first_sign_ins_at_once(&db, &test_config(), Account::New, &ann)
            .await
            .unwrap();
        assert_eq!(user.oidc_subject.as_deref(), Some("sub-ann"));
        assert_eq!(user_count(&db).await, 2);

        // Both link the account of that name (OIDC_LINK_BY_USERNAME).
        let db = test_db::migrated().await;
        let bob = User::create_in(&db, "bob", "pw").await.unwrap();
        let bobs = identity("sub-bob", "bob");
        let by_name = OidcConfig {
            link_by_username: true,
            ..test_config()
        };
        let user = first_sign_ins_at_once(&db, &by_name, Account::Unlinked(bob.clone()), &bobs)
            .await
            .unwrap();
        assert_eq!(user.id, bob.id);
        assert_eq!(user.credentials_version, bob.credentials_version + 1);
    }

    #[tokio::test]
    async fn a_first_sign_in_that_loses_to_another_identity_is_a_conflict() {
        let db = test_db::migrated().await;
        let ann = identity("sub-ann", "ann");
        // Another identity of that name was created meanwhile.
        sign_in(&db, "sub-other", "ann").await.unwrap();
        assert_eq!(
            enter_account(&db, &ann, Account::New).await.unwrap_err(),
            SsoError::Conflict
        );

        // Another identity linked the account of that name meanwhile.
        let db = test_db::migrated().await;
        let bob = User::create_in(&db, "bob", "pw").await.unwrap();
        User::link_oidc_identity_in(&db, &bob.id, ISSUER, "sub-other", "bob")
            .await
            .unwrap();
        assert_eq!(
            enter_account(&db, &identity("sub-bob", "bob"), Account::Unlinked(bob))
                .await
                .unwrap_err(),
            SsoError::Conflict
        );
    }

    async fn user_count(db: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn refuses_unknown_names_without_auto_register() {
        let db = test_db::migrated().await;
        let closed = OidcConfig {
            auto_register: false,
            ..test_config()
        };
        let refused = find_or_create_account(&db, &identity("sub-c", "carol"), &closed).await;
        assert_eq!(refused.unwrap_err(), SsoError::NotRegistered);
        assert!(matches!(
            User::find_by_name_in(&db, "carol").await.unwrap(),
            NameMatch::None
        ));
    }

    fn session_key() -> SecretKey {
        SecretKey::new("k".repeat(32))
    }

    /// A session of `user` as it is now, as this browser's cookie holds it.
    fn session_of(user: &User) -> String {
        crate::auth::create_token(&session_key(), &user.id, user.credentials_version).unwrap()
    }

    async fn link(
        db: &SqlitePool,
        token: Option<&str>,
        user: &User,
        identity: &Identity,
    ) -> Result<User, SsoLinkResult> {
        link_from_session(db, &session_key(), token, &user.id, identity).await
    }

    /// What a link checks again when it writes, read from `user`'s session
    /// `token`.
    fn check_of(token: &str) -> SessionCheck {
        let claims = crate::auth::verify_token(&session_key(), token).unwrap();
        SessionCheck {
            credentials_version: claims.ver,
            session_id: claims.jti,
        }
    }

    #[tokio::test]
    async fn nothing_is_linked_for_a_session_that_ended_after_it_was_read() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let id = identity("sub-ann", "ann");

        // Signed out since the callback read the cookie.
        let token = session_of(&ann);
        let check = check_of(&token);
        sqlx::query("INSERT INTO revoked_sessions (jti, expires_at) VALUES (?, 9999999999)")
            .bind(&check.session_id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            link_for_session_in(&db, &check, &ann.id, &id)
                .await
                .unwrap_err(),
            SsoLinkResult::SessionChanged
        );

        // The password changed since the callback read the cookie.
        let check = check_of(&session_of(&ann));
        User::update_password_in(&db, &ann.id, "new").await.unwrap();
        assert_eq!(
            link_for_session_in(&db, &check, &ann.id, &id)
                .await
                .unwrap_err(),
            SsoLinkResult::SessionChanged
        );
        let after = reread(&db, &ann).await;
        assert_eq!(after.oidc_subject, None);
        assert_eq!(after.credentials_version, ann.credentials_version + 1);

        // A session that still stands links.
        let check = check_of(&session_of(&after));
        let linked = link_for_session_in(&db, &check, &ann.id, &id)
            .await
            .unwrap();
        assert_eq!(linked.oidc_subject.as_deref(), Some("sub-ann"));
    }

    #[tokio::test]
    async fn an_account_links_its_identity_from_its_session() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let old_session = session_of(&ann);
        // Whatever name the provider gives, the session decides the account.
        let id = identity("sub-ann", "ann-at-the-provider");

        let linked = link(&db, Some(&old_session), &ann, &id).await.unwrap();
        assert_eq!(linked.id, ann.id);
        assert_eq!(linked.oidc_issuer.as_deref(), Some(ISSUER));
        assert_eq!(linked.oidc_subject.as_deref(), Some("sub-ann"));
        // Shown in Settings, so the user sees which identity they linked.
        assert_eq!(linked.oidc_username.as_deref(), Some("ann-at-the-provider"));
        assert!(linked.password_matches("pw"));
        // Every session of the account ended; this browser gets a new one.
        assert_eq!(linked.credentials_version, ann.credentials_version + 1);
        let key = session_key();
        assert!(load_session(Some(&old_session), &db, &key).await.is_err());
        let renewed = session_of(&linked);
        assert_eq!(
            load_session(Some(&renewed), &db, &key)
                .await
                .unwrap()
                .user_id,
            ann.id
        );

        // From now on the identity signs in to the account.
        assert_eq!(
            sign_in(&db, "sub-ann", "whatever").await.unwrap().id,
            ann.id
        );
    }

    #[tokio::test]
    async fn nobody_links_an_identity_to_an_account_they_are_not_signed_in_to() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let eve = User::create_in(&db, "eve", "pw").await.unwrap();
        let eves_identity = identity("sub-eve", "eve");

        // No session, another account's session, a session that ended (the
        // password changed since the link started), and a forged token.
        let mut stale = ann.clone();
        let stale_session = session_of(&stale);
        User::update_password_in(&db, &ann.id, "new").await.unwrap();
        stale = reread(&db, &stale).await;
        let other_key = crate::auth::create_token(
            &SecretKey::new("x".repeat(32)),
            &ann.id,
            stale.credentials_version,
        )
        .unwrap();
        for token in [
            None,
            Some(session_of(&eve)),
            Some(stale_session),
            Some(other_key),
        ] {
            assert_eq!(
                link(&db, token.as_deref(), &ann, &eves_identity)
                    .await
                    .unwrap_err(),
                SsoLinkResult::SessionChanged
            );
        }
        for user in [&ann, &eve] {
            assert_eq!(reread(&db, user).await.oidc_subject, None);
        }
        assert_eq!(
            reread(&db, &ann).await.credentials_version,
            stale.credentials_version
        );
    }

    #[tokio::test]
    async fn an_admin_takes_a_wrongly_linked_account_back_from_the_identity() {
        // Mallory's identity ended up on the admin account: an admin turned
        // OIDC_LINK_BY_USERNAME on and Mallory chose the name at the
        // provider. Removing the link ends her session, but keeps her out
        // only once the setting is off or her name there is changed.
        let db = test_db::migrated().await;
        let NameMatch::One(admin) = User::find_by_name_in(&db, "admin").await.unwrap() else {
            panic!("the seeded admin");
        };
        let taken = sign_in_linking_by_name(&db, "sub-mallory", "admin")
            .await
            .unwrap();
        assert_eq!(taken.id, admin.id);
        let her_session = session_of(&taken);

        admin_unlinks(&db, &admin).await;
        let key = session_key();
        assert!(load_session(Some(&her_session), &db, &key).await.is_err());

        // The setting still on, her next sign-in under that name takes the
        // account again.
        let again = sign_in_linking_by_name(&db, "sub-mallory", "admin")
            .await
            .unwrap();
        assert_eq!(again.id, admin.id);
        admin_unlinks(&db, &admin).await;

        // Renamed at the provider, she gets an account of her own.
        let hers = sign_in_linking_by_name(&db, "sub-mallory", "mallory")
            .await
            .unwrap();
        assert_ne!(hers.id, admin.id);
        assert_eq!(reread(&db, &admin).await.oidc_subject, None);

        // With the setting off, the name proves nothing.
        let db = test_db::migrated().await;
        let taken = sign_in_linking_by_name(&db, "sub-mallory", "admin")
            .await
            .unwrap();
        admin_unlinks(&db, &taken).await;
        assert_eq!(
            sign_in(&db, "sub-mallory", "admin").await.unwrap_err(),
            SsoError::AccountExists
        );
        assert_eq!(reread(&db, &taken).await.oidc_subject, None);
    }

    /// The admin `admin` removes their own account's link.
    async fn admin_unlinks(db: &SqlitePool, admin: &User) {
        crate::server_fns::sso::unlink_sso_in(db, &ctx_of(admin), &admin.id, false)
            .await
            .unwrap();
    }

    fn ctx_of(user: &User) -> crate::auth::UserCtx {
        crate::auth::UserCtx {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
        }
    }

    #[tokio::test]
    async fn an_admin_password_reset_takes_an_account_back_from_its_identity() {
        use crate::login_limiter::LoginLimiter;
        use crate::server_fns::auth::{connect_navidrome_in, NavidromeAuthResult};
        use crate::server_fns::user::set_user_password_in;

        // Mallory chose the name "bob" at the provider and her first sign-in
        // made the account before Navidrome's Bob used Soulbeet.
        let db = test_db::migrated().await;
        let taken = sign_in(&db, "sub-mallory", "bob").await.unwrap();
        let her_session = session_of(&taken);
        let NameMatch::One(admin) = User::find_by_name_in(&db, "admin").await.unwrap() else {
            panic!("the seeded admin");
        };

        // The admin gives the account a password for Bob.
        set_user_password_in(&db, &ctx_of(&admin), &taken.id, "for-bob")
            .await
            .unwrap();
        let bobs = reread(&db, &taken).await;
        assert_eq!(
            (&bobs.oidc_issuer, &bobs.oidc_subject, &bobs.oidc_username),
            (&None, &None, &None)
        );
        assert!(bobs.password_matches("for-bob"));
        // Her session ended, and her identity no longer opens the account.
        let key = session_key();
        assert!(load_session(Some(&her_session), &db, &key).await.is_err());
        assert_eq!(
            sign_in(&db, "sub-mallory", "bob").await.unwrap_err(),
            SsoError::AccountExists
        );

        // Bob signs in with that password and connects his Navidrome account:
        // none of it reaches Mallory.
        connect_navidrome_in(
            &db,
            &LoginLimiter::new(),
            &ctx_of(&bobs),
            "enc",
            std::time::Instant::now,
            |_| async { NavidromeAuthResult::Success },
        )
        .await
        .unwrap();
        let connected = reread(&db, &bobs).await;
        assert_eq!(connected.navidrome_token.as_deref(), Some("enc"));
        assert_eq!(connected.oidc_subject, None);
        assert_eq!(
            sign_in(&db, "sub-mallory", "bob").await.unwrap_err(),
            SsoError::AccountExists
        );
    }

    #[tokio::test]
    async fn changing_ones_own_password_keeps_the_link() {
        use crate::login_limiter::LoginLimiter;
        use crate::server_fns::{auth::NavidromeAuthResult, user::change_password_in};

        let db = test_db::migrated().await;
        let limiter = LoginLimiter::new();
        let ann = User::create_in(&db, "ann", "ann-pw").await.unwrap();
        User::link_oidc_identity_in(&db, &ann.id, ISSUER, "sub-ann", "ann")
            .await
            .unwrap();
        // An admin's own password too.
        let NameMatch::One(admin) = User::find_by_name_in(&db, "admin").await.unwrap() else {
            panic!("the seeded admin");
        };
        User::link_oidc_identity_in(&db, &admin.id, ISSUER, "sub-admin", "admin")
            .await
            .unwrap();
        for (user, current) in [(&ann, "ann-pw"), (&admin, "admin")] {
            change_password_in(&db, &limiter, user, current, "new-pw", false, |_| async {
                NavidromeAuthResult::NotConfigured
            })
            .await
            .unwrap();
            let after = reread(&db, user).await;
            assert!(after.oidc_subject.is_some(), "{}", after.username);
            assert!(after.password_matches("new-pw"));
        }
        assert_eq!(sign_in(&db, "sub-ann", "ann").await.unwrap().id, ann.id);
    }

    #[tokio::test]
    async fn an_identity_stays_on_one_account_and_an_account_keeps_its_identity() {
        let db = test_db::migrated().await;
        let ann = User::create_in(&db, "ann", "pw").await.unwrap();
        let bob = User::create_in(&db, "bob", "pw").await.unwrap();
        let anns = identity("sub-ann", "ann");
        let ann = link(&db, Some(&session_of(&ann)), &ann, &anns)
            .await
            .unwrap();

        // Bob cannot take Ann's identity, even signed in as himself.
        assert_eq!(
            link(&db, Some(&session_of(&bob)), &bob, &anns)
                .await
                .unwrap_err(),
            SsoLinkResult::IdentityTaken
        );
        assert_eq!(reread(&db, &bob).await.oidc_subject, None);

        // Ann keeps hers: another one, or the same one again, changes nothing.
        for other in [identity("sub-other", "ann"), identity("sub-ann", "ann")] {
            assert_eq!(
                link(&db, Some(&session_of(&ann)), &ann, &other)
                    .await
                    .unwrap_err(),
                SsoLinkResult::AlreadyLinked
            );
        }
        let after = reread(&db, &ann).await;
        assert_eq!(after.oidc_subject.as_deref(), Some("sub-ann"));
        assert_eq!(after.credentials_version, ann.credentials_version);
    }

    #[tokio::test]
    async fn not_linked_means_no_stored_navidrome_login() {
        let db = test_db::migrated().await;
        let sso = User::create_oidc_in(&db, "sso", ISSUER, "sub-sso")
            .await
            .unwrap();
        let synced = sync_navidrome_status(&db, sso.clone(), true).await;
        assert_eq!(synced.navidrome_status, "not_linked");
        assert_eq!(reread(&db, &sso).await.navidrome_status, "not_linked");
        // Without Navidrome there is nothing to connect.
        let synced = sync_navidrome_status(&db, synced, false).await;
        assert_eq!(synced.navidrome_status, "unknown");

        // An account that has a Navidrome login keeps its status, whatever it is.
        User::update_navidrome_token_in(&db, &sso.id, Some("enc"), "offline")
            .await
            .unwrap();
        let connected = reread(&db, &sso).await;
        for navidrome in [true, false] {
            let synced = sync_navidrome_status(&db, connected.clone(), navidrome).await;
            assert_eq!(synced.navidrome_status, "offline");
        }
        assert_eq!(reread(&db, &sso).await.navidrome_status, "offline");
    }

    #[test]
    fn several_accounts_of_the_name_are_a_conflict() {
        let config = OidcConfig {
            link_by_username: true,
            ..test_config()
        };
        assert!(matches!(
            choose_account(None, NameMatch::Several, &config),
            Account::Conflict
        ));
    }
}
