#[cfg(feature = "server")]
use crate::db::get_pool;
#[cfg(feature = "server")]
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use sqlx::Row;
#[cfg(feature = "server")]
use uuid::Uuid;

#[cfg(feature = "server")]
use super::folder::Folder;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
}

#[cfg(feature = "server")]
impl User {
    pub async fn create(username: &str, password: &str) -> Result<User, String> {
        let pool = get_pool().await;
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| e.to_string())?
            .to_string();

        let id = Uuid::new_v4().to_string();

        let user = sqlx::query_as::<_, User>(
            "INSERT INTO users (id, username, password_hash) VALUES (?, ?, ?) RETURNING id, username, password_hash"
        )
        .bind(&id)
        .bind(username)
        .bind(password_hash)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;

        Ok(user)
    }

    pub async fn verify(username: &str, password: &str) -> Result<User, String> {
        let pool = get_pool().await;
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("User not found")?;

        let parsed_hash = PasswordHash::new(&user.password_hash).map_err(|e| e.to_string())?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .map_err(|_| "Invalid password")?;

        Ok(user)
    }

    pub async fn get_folders(&self) -> Result<Vec<Folder>, String> {
        let pool = get_pool().await;
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ?")
            .bind(&self.id)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn get_all() -> Result<Vec<User>, String> {
        let pool = get_pool().await;
        sqlx::query_as::<_, User>("SELECT * FROM users")
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn get_by_id(id: &str) -> Result<User, String> {
        let pool = get_pool().await;
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("User not found")?;

        Ok(user)
    }

    pub async fn update_password(id: &str, password: &str) -> Result<(), String> {
        let pool = get_pool().await;
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| e.to_string())?
            .to_string();

        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(password_hash)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<(), String> {
        let pool = get_pool().await;
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
