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
use sqlx::SqlitePool;
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
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, PartialEq)]
pub enum UserChangeError {
    NotFound,
    LastAdmin,
    Db(String),
}

#[cfg(feature = "server")]
impl fmt::Display for UserChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("User not found"),
            Self::LastAdmin => f.write_str("Soulful needs at least one admin"),
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

/// The Soulful rows a name Navidrome verified can belong to. Navidrome
/// compares names without letter case, so `Bob` signs in as `bob`.
#[cfg(feature = "server")]
#[derive(Debug)] // User has no PartialEq; tests match on the variant
pub enum NavidromeNameMatch {
    None,
    One(User),
    /// No row has the exact name and several match it without case.
    Several,
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
    pub async fn create(username: &str, password: &str) -> Result<User, String> {
        Self::create_in(&DB, username, password).await
    }

    pub(crate) async fn create_in(
        db: &SqlitePool,
        username: &str,
        password: &str,
    ) -> Result<User, String> {
        let password_hash = hash_password(password)?;
        let id = Uuid::new_v4().to_string();

        sqlx::query_as::<_, User>(
            "INSERT INTO users (id, username, password_hash, navidrome_status, role) \
             VALUES (?, ?, ?, ?, CASE WHEN EXISTS (SELECT 1 FROM users WHERE role = 'admin') \
             THEN 'user' ELSE 'admin' END) RETURNING *",
        )
        .bind(&id)
        .bind(username)
        .bind(password_hash)
        .bind(shared::system::NavidromeStatus::Unknown.as_str())
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())
    }

    /// The user whose name and password these are, or `None` for an unknown
    /// name or a wrong password. `Err` is a database failure.
    pub(crate) async fn verify_in(
        db: &SqlitePool,
        username: &str,
        password: &str,
    ) -> Result<Option<User>, String> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(user.filter(|u| u.password_matches(password)))
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

    pub async fn update_username(id: &str, new_username: &str) -> Result<(), String> {
        sqlx::query("UPDATE users SET username = ? WHERE id = ?")
            .bind(new_username)
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
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

    pub async fn get_by_username(username: &str) -> Result<Option<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    /// The row with exactly this name, else the only row whose name matches
    /// it without letter case, the way Navidrome compares names.
    pub(crate) async fn match_navidrome_name_in(
        db: &SqlitePool,
        username: &str,
    ) -> Result<NavidromeNameMatch, String> {
        let mut rows =
            sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ? COLLATE NOCASE")
                .bind(username)
                .fetch_all(db)
                .await
                .map_err(|e| e.to_string())?;
        if let Some(i) = rows.iter().position(|u| u.username == username) {
            return Ok(NavidromeNameMatch::One(rows.swap_remove(i)));
        }
        Ok(match rows.len() {
            0 => NavidromeNameMatch::None,
            1 => NavidromeNameMatch::One(rows.remove(0)),
            _ => NavidromeNameMatch::Several,
        })
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

    pub async fn get_connected_users() -> Result<Vec<User>, String> {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE navidrome_status IN (?, ?)")
            .bind(shared::system::NavidromeStatus::Connected.as_str())
            .bind(shared::system::NavidromeStatus::MissingReportRealPath.as_str())
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
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
        };
        let json = serde_json::to_value(user).unwrap();
        assert!(json.get("navidrome_token").is_none() && json.get("credentials_version").is_none());
    }
}
