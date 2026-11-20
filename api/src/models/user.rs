#[cfg(feature = "server")]
use super::folder::Folder;
use crate::auth::Role;
#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use crate::security;
#[cfg(feature = "server")]
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use shared::changelog::Version;
#[cfg(feature = "server")]
use sqlx::{SqliteExecutor, SqlitePool};
#[cfg(feature = "server")]
use std::fmt;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
    #[serde(skip)]
    pub navidrome_token: Option<String>,
    pub navidrome_status: String,
    pub role: Role,
    #[serde(skip)]
    pub credentials_version: i64,
    /// The OpenID Connect issuer of the identity this account signs in as,
    /// once linked; set together with `oidc_subject`
    #[serde(skip)]
    pub oidc_issuer: Option<String>,
    /// The subject (`sub`) that issuer gave the identity
    #[serde(skip)]
    pub oidc_subject: Option<String>,
    /// The name the provider gave the identity at its last sign-in, set
    /// together with the identity. Only shown, never used to find an account
    #[serde(skip)]
    pub oidc_username: Option<String>,
}

/// An account as user management lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserSummary {
    pub id: String,
    pub username: String,
    pub role: Role,
    /// The account signs in with a password of its own
    pub has_password: bool,
    /// The provider's name for the single sign-on identity the account is
    /// linked to, when it is linked
    pub sso_name: Option<String>,
}

#[cfg(feature = "server")]
impl From<&User> for UserSummary {
    fn from(user: &User) -> Self {
        UserSummary {
            id: user.id.clone(),
            username: user.username.clone(),
            role: user.role,
            has_password: user.has_password(),
            sso_name: user.oidc_username.clone(),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, PartialEq)]
pub enum UserChangeError {
    NotFound,
    LastAdmin,
    /// Another account has this name, in any letter case.
    UsernameTaken,
    Db(String),
}

#[cfg(feature = "server")]
impl fmt::Display for UserChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("User not found"),
            Self::LastAdmin => f.write_str("Soulbeet needs at least one admin"),
            Self::UsernameTaken => f.write_str("That username is taken"),
            Self::Db(e) => f.write_str(e),
        }
    }
}

/// Longest username an account can have. Sign-ins with a longer name are
/// refused before anything is remembered about them.
#[cfg(feature = "server")]
pub const MAX_USERNAME_BYTES: usize = 256;

#[cfg(feature = "server")]
pub fn username_fits(username: &str) -> bool {
    username.len() <= MAX_USERNAME_BYTES
}

/// The rows a name belongs to. Names are unique and matched without regard
/// to ASCII letter case (as Navidrome does), so `Bob` signs in as `bob`.
#[cfg(feature = "server")]
#[derive(Debug)] // User has no PartialEq; tests match on the variant
pub enum NameMatch {
    None,
    One(User),
    /// Rows from before the rule whose names differ only in letter case.
    /// None of them can sign in until an admin deletes the extra ones.
    Several,
}

/// What linking an account to an OpenID Connect identity did.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OidcLink {
    Linked,
    /// The account already has an identity, or is gone: nothing changed
    AccountLinked,
    /// Another account has this identity: nothing changed
    IdentityTaken,
}

/// The `password_hash` of an account without a password. It is no hash, so
/// no password matches it.
#[cfg(feature = "server")]
const NO_PASSWORD: &str = "";

/// What the triggers of the username migration raise for a taken name.
#[cfg(feature = "server")]
const USERNAME_TAKEN_RAISED: &str = "username taken";

#[cfg(feature = "server")]
fn change_error(e: sqlx::Error) -> UserChangeError {
    match &e {
        sqlx::Error::Database(d) if d.message() == USERNAME_TAKEN_RAISED => {
            UserChangeError::UsernameTaken
        }
        _ => UserChangeError::Db(e.to_string()),
    }
}

#[cfg(feature = "server")]
fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string())
}

#[cfg(feature = "server")]
impl User {
    pub async fn create(username: &str, password: &str) -> Result<User, UserChangeError> {
        Self::create_in(&DB, username, password).await
    }

    pub(crate) async fn create_in(
        db: &SqlitePool,
        username: &str,
        password: &str,
    ) -> Result<User, UserChangeError> {
        let password_hash = hash_password(password).map_err(UserChangeError::Db)?;
        let id = Uuid::new_v4().to_string();

        // A new account has nothing to catch up on in "What's new".
        sqlx::query_as::<_, User>(
            "INSERT INTO users (id, username, password_hash, navidrome_status, changelog_seen, \
             role) VALUES (?, ?, ?, ?, ?, CASE WHEN EXISTS (SELECT 1 FROM users WHERE \
             role = 'admin') THEN 'user' ELSE 'admin' END) RETURNING *",
        )
        .bind(&id)
        .bind(username)
        .bind(password_hash)
        .bind(shared::system::NavidromeStatus::Unknown.as_str())
        .bind(shared::changelog::running_version().to_string())
        .fetch_one(db)
        .await
        .map_err(change_error)
    }

    pub fn password_matches(&self, password: &str) -> bool {
        PasswordHash::new(&self.password_hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    }

    pub async fn get_folders(&self) -> Result<Vec<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ?")
            .bind(&self.id)
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn get_all() -> Result<Vec<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users")
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn find_by_id(id: &str) -> Result<Option<User>, String> {
        Self::find_by_id_in(&DB, id).await
    }

    pub(crate) async fn find_by_id_in(db: &SqlitePool, id: &str) -> Result<Option<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn update_password(id: &str, password: &str) -> Result<(), String> {
        Self::update_password_in(&DB, id, password).await?;
        security::note_password_set(id, password);
        Ok(())
    }

    pub(crate) async fn update_password_in(
        db: &SqlitePool,
        id: &str,
        password: &str,
    ) -> Result<i64, String> {
        let password_hash = hash_password(password)?;
        sqlx::query_scalar(
            "UPDATE users SET password_hash = ?, credentials_version = credentials_version + 1 \
             WHERE id = ? RETURNING credentials_version",
        )
        .bind(password_hash)
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| UserChangeError::NotFound.to_string())
    }

    pub async fn update_username(id: &str, new_username: &str) -> Result<(), UserChangeError> {
        Self::update_username_in(&DB, id, new_username).await
    }

    pub(crate) async fn update_username_in(
        db: &SqlitePool,
        id: &str,
        new_username: &str,
    ) -> Result<(), UserChangeError> {
        sqlx::query("UPDATE users SET username = ? WHERE id = ?")
            .bind(new_username)
            .bind(id)
            .execute(db)
            .await
            .map_err(change_error)?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<(), UserChangeError> {
        Self::delete_in(&DB, id).await?;
        security::note_user_deleted(id);
        Ok(())
    }

    pub(crate) async fn delete_in(db: &SqlitePool, id: &str) -> Result<(), UserChangeError> {
        let done = sqlx::query(
            "DELETE FROM users WHERE id = ? AND (role <> 'admin' \
             OR (SELECT COUNT(*) FROM users WHERE role = 'admin') > 1)",
        )
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| UserChangeError::Db(e.to_string()))?;
        Self::refusal_for_untouched(db, id, done.rows_affected()).await
    }

    pub async fn set_role(id: &str, role: Role) -> Result<(), UserChangeError> {
        Self::set_role_in(&DB, id, role).await
    }

    pub(crate) async fn set_role_in(
        db: &SqlitePool,
        id: &str,
        role: Role,
    ) -> Result<(), UserChangeError> {
        let done = sqlx::query(
            "UPDATE users SET role = ?1 WHERE id = ?2 AND (?1 = 'admin' OR role <> 'admin' \
             OR (SELECT COUNT(*) FROM users WHERE role = 'admin') > 1)",
        )
        .bind(role)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| UserChangeError::Db(e.to_string()))?;
        Self::refusal_for_untouched(db, id, done.rows_affected()).await
    }

    async fn refusal_for_untouched(
        db: &SqlitePool,
        id: &str,
        rows_affected: u64,
    ) -> Result<(), UserChangeError> {
        if rows_affected > 0 {
            return Ok(());
        }
        match Self::find_by_id_in(db, id).await {
            Ok(Some(_)) => Err(UserChangeError::LastAdmin),
            Ok(None) => Err(UserChangeError::NotFound),
            Err(e) => Err(UserChangeError::Db(e)),
        }
    }

    /// The account a name belongs to, matched without ASCII letter case.
    pub(crate) async fn find_by_name_in(
        db: &SqlitePool,
        username: &str,
    ) -> Result<NameMatch, String> {
        let mut rows =
            sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ? COLLATE NOCASE")
                .bind(username)
                .fetch_all(db)
                .await
                .map_err(|e| e.to_string())?;
        Ok(match rows.len() {
            0 => NameMatch::None,
            1 => NameMatch::One(rows.remove(0)),
            _ => NameMatch::Several,
        })
    }

    /// Names that another account also has in other letter case, from before
    /// names were unique without case. Ordered so case variants sit together.
    pub(crate) async fn case_collisions_in(db: &SqlitePool) -> Result<Vec<String>, String> {
        sqlx::query_scalar(
            "SELECT u.username FROM users u WHERE EXISTS (SELECT 1 FROM users o \
             WHERE o.id <> u.id AND o.username = u.username COLLATE NOCASE) \
             ORDER BY u.username COLLATE NOCASE, u.username",
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub(crate) async fn link_navidrome_in(
        db: &SqlitePool,
        id: &str,
        token: &str,
        status: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE users SET navidrome_token = ?, navidrome_status = ?, \
             credentials_version = credentials_version + (navidrome_token IS NULL) WHERE id = ?",
        )
        .bind(token)
        .bind(status)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn update_navidrome_token(
        id: &str,
        token: Option<&str>,
        status: &str,
    ) -> Result<(), String> {
        Self::update_navidrome_token_in(&DB, id, token, status).await
    }

    pub async fn update_navidrome_token_in(
        db: &sqlx::SqlitePool,
        id: &str,
        token: Option<&str>,
        status: &str,
    ) -> Result<(), String> {
        sqlx::query("UPDATE users SET navidrome_token = ?, navidrome_status = ? WHERE id = ?")
            .bind(token)
            .bind(status)
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The account linked to an OpenID Connect identity: a subject is unique
    /// only within its issuer, so both are matched.
    pub(crate) async fn find_by_oidc_identity_in<'e>(
        db: impl SqliteExecutor<'e>,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE oidc_issuer = ? AND oidc_subject = ?")
            .bind(issuer)
            .bind(subject)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())
    }

    /// Links an account that has no OpenID Connect identity yet to the
    /// subject `subject` of `issuer`, which the provider calls `username`.
    /// Like a first link to Navidrome, it ends the account's other sessions:
    /// someone else can now sign in to it.
    pub(crate) async fn link_oidc_identity_in<'e>(
        db: impl SqliteExecutor<'e>,
        id: &str,
        issuer: &str,
        subject: &str,
        username: &str,
    ) -> Result<OidcLink, String> {
        let done = sqlx::query(
            "UPDATE users SET oidc_issuer = ?, oidc_subject = ?, oidc_username = ?, \
             credentials_version = credentials_version + 1 \
             WHERE id = ? AND oidc_subject IS NULL",
        )
        .bind(issuer)
        .bind(subject)
        .bind(username)
        .bind(id)
        .execute(db)
        .await;
        match done {
            Ok(done) if done.rows_affected() == 1 => Ok(OidcLink::Linked),
            Ok(_) => Ok(OidcLink::AccountLinked),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(OidcLink::IdentityTaken),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Records `username`, the provider's name for the identity the account
    /// `id` is linked to now, for Settings to show.
    pub(crate) async fn note_oidc_username_in(
        db: &SqlitePool,
        id: &str,
        username: &str,
    ) -> Result<(), String> {
        sqlx::query("UPDATE users SET oidc_username = ? WHERE id = ? AND oidc_subject IS NOT NULL")
            .bind(username)
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Removes the account's OpenID Connect identity, so it no longer signs
    /// in through the provider. Ends the account's sessions, as a password
    /// change does. False when the account had none.
    pub(crate) async fn unlink_oidc_identity_in(db: &SqlitePool, id: &str) -> Result<bool, String> {
        let done = sqlx::query(
            "UPDATE users SET oidc_issuer = NULL, oidc_subject = NULL, oidc_username = NULL, \
             credentials_version = credentials_version + 1 \
             WHERE id = ? AND oidc_subject IS NOT NULL",
        )
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(done.rows_affected() == 1)
    }

    /// Creates an account that signs in through OpenID Connect, always a
    /// plain user: the provider's first user is not this instance's admin,
    /// even on an empty install. It has no password, so the password form
    /// stays closed to it until one is set in Settings > Users. A Navidrome
    /// login of that name does not join it: the provider and Navidrome may
    /// give the name to different people.
    pub(crate) async fn create_oidc_in(
        db: &SqlitePool,
        username: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<User, UserChangeError> {
        sqlx::query_as::<_, User>(
            "INSERT INTO users (id, username, password_hash, navidrome_status, changelog_seen, \
             role, oidc_issuer, oidc_subject, oidc_username) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             RETURNING *",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(username)
        .bind(NO_PASSWORD)
        .bind(shared::system::NavidromeStatus::Unknown.as_str())
        .bind(shared::changelog::running_version().to_string())
        .bind(Role::User)
        .bind(issuer)
        .bind(subject)
        .bind(username)
        .fetch_one(db)
        .await
        .map_err(change_error)
    }

    /// The account has a password of its own; one created through single
    /// sign-on has none until it is given one.
    pub fn has_password(&self) -> bool {
        self.password_hash != NO_PASSWORD
    }

    pub async fn get_connected_users() -> Result<Vec<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE navidrome_status = ?")
            .bind(shared::system::NavidromeStatus::Connected.as_str())
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    /// The version whose release notes this user last closed.
    pub(crate) async fn changelog_seen_in(db: &SqlitePool, id: &str) -> Result<Version, String> {
        let seen: String = sqlx::query_scalar("SELECT changelog_seen FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| UserChangeError::NotFound.to_string())?;
        Version::parse(&seen)
    }

    /// Records that this user closed the release notes up to `version`. The
    /// stored version never goes down: a tab left open on an older version
    /// must not bring back notes closed on a newer one.
    pub(crate) async fn mark_changelog_seen_in(
        db: &SqlitePool,
        id: &str,
        version: Version,
    ) -> Result<(), String> {
        loop {
            let stored: String =
                sqlx::query_scalar("SELECT changelog_seen FROM users WHERE id = ?")
                    .bind(id)
                    .fetch_optional(db)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| UserChangeError::NotFound.to_string())?;
            if Version::parse(&stored)? >= version {
                return Ok(());
            }
            // Written only over the value compared, so a concurrent close is
            // compared again instead of overwritten.
            let done = sqlx::query(
                "UPDATE users SET changelog_seen = ? WHERE id = ? AND changelog_seen = ?",
            )
            .bind(version.to_string())
            .bind(id)
            .bind(&stored)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
            if done.rows_affected() == 1 {
                return Ok(());
            }
        }
    }

    /// Every user with a Navidrome password stored, whatever its last status.
    pub(crate) async fn linked_to_navidrome_in(db: &SqlitePool) -> Result<Vec<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE navidrome_token IS NOT NULL")
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Startup check: accounts whose names differ only in letter case cannot
/// sign in until an admin deletes the extra ones.
#[cfg(feature = "server")]
pub async fn log_username_collisions() {
    match User::case_collisions_in(&DB).await {
        Ok(names) if !names.is_empty() => tracing::warn!(
            "These accounts cannot sign in because their names differ only in letter case: {}. \
             An admin can delete the ones that are not used in Settings > Users.",
            names.join(", ")
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("Could not check usernames: {e}"),
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    async fn version_of(db: &sqlx::SqlitePool, id: &str) -> i64 {
        User::find_by_id_in(db, id)
            .await
            .unwrap()
            .unwrap()
            .credentials_version
    }

    #[tokio::test]
    async fn the_first_user_of_an_empty_install_is_admin() {
        let db = test_db::migrated().await;
        sqlx::query("DELETE FROM users").execute(&db).await.unwrap();
        let a = User::create_in(&db, "a", "pw").await.unwrap();
        let b = User::create_in(&db, "b", "pw").await.unwrap();
        assert_eq!(a.role, Role::Admin);
        assert_eq!(b.role, Role::User);
    }

    #[tokio::test]
    async fn a_password_change_bumps_the_version() {
        let db = test_db::migrated().await;
        let user = User::create_in(&db, "a", "pw").await.unwrap();
        assert_eq!(user.credentials_version, 0);
        let new = User::update_password_in(&db, &user.id, "other")
            .await
            .unwrap();
        assert_eq!(new, 1);
        assert_eq!(version_of(&db, &user.id).await, 1);
        let user = User::find_by_id_in(&db, &user.id).await.unwrap().unwrap();
        assert!(user.password_matches("other"));
        assert!(!user.password_matches("pw"));
    }

    #[tokio::test]
    async fn linking_bumps_only_an_unlinked_row() {
        let db = test_db::migrated().await;
        let user = User::create_in(&db, "a", "pw").await.unwrap();
        User::link_navidrome_in(&db, &user.id, "cipher", "connected")
            .await
            .unwrap();
        assert_eq!(version_of(&db, &user.id).await, 1);
        User::link_navidrome_in(&db, &user.id, "cipher2", "connected")
            .await
            .unwrap();
        assert_eq!(version_of(&db, &user.id).await, 1);
        User::update_navidrome_token_in(&db, &user.id, Some("cipher3"), "offline")
            .await
            .unwrap();
        assert_eq!(version_of(&db, &user.id).await, 1);
    }

    #[tokio::test]
    async fn an_oidc_identity_belongs_to_one_account() {
        let db = test_db::migrated().await;
        let a = User::create_in(&db, "a", "pw").await.unwrap();
        let b = User::create_in(&db, "b", "pw").await.unwrap();
        let link = |id: String, issuer: &'static str, subject: &'static str| {
            let db = db.clone();
            async move {
                User::link_oidc_identity_in(&db, &id, issuer, subject, "name")
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            link(a.id.clone(), "https://id", "1").await,
            OidcLink::Linked
        );
        // An account keeps the identity it has.
        assert_eq!(
            link(a.id.clone(), "https://id", "2").await,
            OidcLink::AccountLinked
        );
        let a = User::find_by_id_in(&db, &a.id).await.unwrap().unwrap();
        assert_eq!(a.oidc_subject.as_deref(), Some("1"));
        assert_eq!(a.credentials_version, 1);
        // The pair is unique; the same subject at another issuer is not.
        assert_eq!(
            link(b.id.clone(), "https://id", "1").await,
            OidcLink::IdentityTaken
        );
        assert_eq!(
            version_of(&db, &b.id).await,
            0,
            "a refused link changes nothing"
        );
        assert_eq!(
            link(b.id.clone(), "https://other", "1").await,
            OidcLink::Linked
        );
        assert_eq!(
            link("nobody".into(), "https://id", "3").await,
            OidcLink::AccountLinked
        );
        // Nor does a new account get an identity another one has.
        assert!(matches!(
            User::create_oidc_in(&db, "c", "https://id", "1").await,
            Err(UserChangeError::Db(_))
        ));
        assert!(matches!(
            User::find_by_name_in(&db, "c").await.unwrap(),
            NameMatch::None
        ));
        // Never one half of an identity.
        assert!(
            sqlx::query("UPDATE users SET oidc_subject = NULL WHERE id = ?")
                .bind(&a.id)
                .execute(&db)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_user_list_says_who_has_a_password_and_which_identity_is_linked() {
        let db = test_db::migrated().await;
        let local = User::create_in(&db, "local", "pw").await.unwrap();
        let sso = User::create_oidc_in(&db, "sso", "https://id", "1")
            .await
            .unwrap();
        assert_eq!(
            UserSummary::from(&local),
            UserSummary {
                id: local.id.clone(),
                username: "local".into(),
                role: Role::User,
                has_password: true,
                sso_name: None,
            }
        );
        let summary = UserSummary::from(&sso);
        assert!(!summary.has_password);
        assert_eq!(summary.sso_name.as_deref(), Some("sso"));

        // The name follows the provider's, and a link always has one.
        User::note_oidc_username_in(&db, &sso.id, "sso-renamed")
            .await
            .unwrap();
        let sso = User::find_by_id_in(&db, &sso.id).await.unwrap().unwrap();
        assert_eq!(
            UserSummary::from(&sso).sso_name.as_deref(),
            Some("sso-renamed")
        );
        assert_eq!(sso.username, "sso");
        User::note_oidc_username_in(&db, &local.id, "nobody")
            .await
            .unwrap();
        let local = User::find_by_id_in(&db, &local.id).await.unwrap().unwrap();
        assert_eq!(local.oidc_username, None);
        assert!(
            sqlx::query("UPDATE users SET oidc_username = NULL WHERE id = ?")
                .bind(&sso.id)
                .execute(&db)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unlinking_removes_the_identity_and_ends_the_sessions() {
        let db = test_db::migrated().await;
        let a = User::create_in(&db, "a", "pw").await.unwrap();
        User::link_oidc_identity_in(&db, &a.id, "https://id", "1", "a-at-id")
            .await
            .unwrap();
        assert!(User::unlink_oidc_identity_in(&db, &a.id).await.unwrap());
        let after = User::find_by_id_in(&db, &a.id).await.unwrap().unwrap();
        assert_eq!(
            (
                &after.oidc_issuer,
                &after.oidc_subject,
                &after.oidc_username
            ),
            (&None, &None, &None)
        );
        assert_eq!(after.credentials_version, 2);
        assert!(after.password_matches("pw"));
        // Nothing to remove: nothing changes.
        assert!(!User::unlink_oidc_identity_in(&db, &a.id).await.unwrap());
        assert_eq!(version_of(&db, &a.id).await, 2);
        // The identity is free again, for this account or another.
        let b = User::create_in(&db, "b", "pw").await.unwrap();
        assert_eq!(
            User::link_oidc_identity_in(&db, &b.id, "https://id", "1", "b-at-id")
                .await
                .unwrap(),
            OidcLink::Linked
        );
    }

    #[tokio::test]
    async fn the_last_admin_cannot_be_deleted_or_demoted() {
        let db = test_db::migrated().await;
        let admin: String = sqlx::query_scalar("SELECT id FROM users WHERE role = 'admin'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            User::delete_in(&db, &admin).await,
            Err(UserChangeError::LastAdmin)
        );
        assert_eq!(
            User::set_role_in(&db, &admin, Role::User).await,
            Err(UserChangeError::LastAdmin)
        );
        let second = User::create_in(&db, "second", "pw").await.unwrap();
        User::set_role_in(&db, &second.id, Role::Admin)
            .await
            .unwrap();
        User::set_role_in(&db, &admin, Role::User).await.unwrap();
        User::delete_in(&db, &admin).await.unwrap();
        assert!(User::find_by_id_in(&db, &admin).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_missing_user_is_not_found() {
        let db = test_db::migrated().await;
        assert_eq!(
            User::delete_in(&db, "nobody").await,
            Err(UserChangeError::NotFound)
        );
        assert_eq!(
            User::set_role_in(&db, "nobody", Role::Admin).await,
            Err(UserChangeError::NotFound)
        );
        assert!(User::find_by_id_in(&db, "nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn names_are_unique_without_letter_case() {
        let db = test_db::migrated().await;
        let bob = User::create_in(&db, "bob", "pw").await.unwrap();
        assert_eq!(
            User::create_in(&db, "BOB", "pw").await.unwrap_err(),
            UserChangeError::UsernameTaken
        );
        let eve = User::create_in(&db, "eve", "pw").await.unwrap();
        assert_eq!(
            User::update_username_in(&db, &eve.id, "Bob").await,
            Err(UserChangeError::UsernameTaken)
        );
        // A change of one's own name's letter case is not a collision.
        User::update_username_in(&db, &bob.id, "Bob").await.unwrap();
        assert!(matches!(
            User::find_by_name_in(&db, "bOB").await.unwrap(),
            NameMatch::One(u) if u.id == bob.id
        ));
        assert!(matches!(
            User::find_by_name_in(&db, "nobody").await.unwrap(),
            NameMatch::None
        ));
    }

    #[tokio::test]
    async fn rows_that_collide_from_before_the_rule_are_several_and_listed() {
        let db = test_db::migrated_before(test_db::USERNAME_CASE).await;
        for (id, name) in [("u1", "bob"), ("u2", "Bob"), ("u3", "eve")] {
            sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
                .bind(id)
                .bind(name)
                .execute(&db)
                .await
                .unwrap();
        }
        test_db::apply_from(&db, test_db::USERNAME_CASE).await;
        assert!(matches!(
            User::find_by_name_in(&db, "bob").await.unwrap(),
            NameMatch::Several
        ));
        assert_eq!(
            User::case_collisions_in(&db).await.unwrap(),
            vec!["Bob".to_string(), "bob".to_string()]
        );
        assert_eq!(
            User::create_in(&db, "BOB", "pw").await.unwrap_err(),
            UserChangeError::UsernameTaken
        );
    }

    #[tokio::test]
    async fn a_new_account_has_seen_the_running_version() {
        let db = test_db::migrated().await;
        let user = User::create_in(&db, "a", "pw").await.unwrap();
        assert_eq!(
            User::changelog_seen_in(&db, &user.id).await.unwrap(),
            shared::changelog::running_version()
        );
    }

    #[tokio::test]
    async fn closing_the_release_notes_records_the_version() {
        let db = test_db::migrated().await;
        let user = User::create_in(&db, "a", "pw").await.unwrap();
        let other = User::create_in(&db, "b", "pw").await.unwrap();
        let later = Version::parse("12.3.4").unwrap();
        User::mark_changelog_seen_in(&db, &user.id, later)
            .await
            .unwrap();
        assert_eq!(User::changelog_seen_in(&db, &user.id).await.unwrap(), later);
        // A tab still on an older version closing its notes lowers nothing.
        User::mark_changelog_seen_in(&db, &user.id, Version::parse("12.3.3").unwrap())
            .await
            .unwrap();
        assert_eq!(User::changelog_seen_in(&db, &user.id).await.unwrap(), later);
        assert_eq!(
            User::changelog_seen_in(&db, &other.id).await.unwrap(),
            shared::changelog::running_version()
        );
        assert_eq!(
            User::mark_changelog_seen_in(&db, "nobody", later).await,
            Err("User not found".to_string())
        );
        assert_eq!(
            User::changelog_seen_in(&db, "nobody").await,
            Err("User not found".to_string())
        );
    }

    #[tokio::test]
    async fn a_row_inserted_without_a_seen_version_fails_to_read() {
        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u', 'u', 'x')")
            .execute(&db)
            .await
            .unwrap();
        assert!(User::changelog_seen_in(&db, "u").await.is_err());
    }

    #[test]
    fn users_never_serialize_their_navidrome_token() {
        let user = User {
            id: "u".into(),
            username: "a".into(),
            password_hash: "h".into(),
            navidrome_token: Some("cipher".into()),
            navidrome_status: "unknown".into(),
            role: Role::User,
            credentials_version: 3,
            oidc_issuer: None,
            oidc_subject: None,
            oidc_username: None,
        };
        let json = serde_json::to_value(user).unwrap();
        assert!(json.get("navidrome_token").is_none() && json.get("credentials_version").is_none());
    }
}
