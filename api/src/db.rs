#[cfg(feature = "server")]
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use sqlx::{sqlite::SqlitePoolOptions, Pool, Row, Sqlite};
#[cfg(feature = "server")]
use std::sync::OnceLock;
#[cfg(feature = "server")]
use tokio::sync::OnceCell;
#[cfg(feature = "server")]
use uuid::Uuid;

#[cfg(feature = "server")]
static POOL: OnceCell<Pool<Sqlite>> = OnceCell::const_new();

#[cfg(feature = "server")]
pub async fn get_pool() -> &'static Pool<Sqlite> {
    POOL.get_or_init(|| async {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:soulful.db".to_string());

        if database_url.starts_with("sqlite:") {
            let path_str = database_url.trim_start_matches("sqlite:");
            let path = std::path::Path::new(path_str);
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("Failed to create database directory");
                }
                std::fs::File::create(path).expect("Failed to create database file");
            }
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("Failed to connect to database");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("Failed to run migrations");

        pool
    })
    .await
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Folder {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub path: String,
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
}

#[cfg(feature = "server")]
impl Folder {
    pub async fn create(user_id: &str, name: &str, path: &str) -> Result<Folder, String> {
        let pool = get_pool().await;
        let id = Uuid::new_v4().to_string();

        let folder = sqlx::query_as::<_, Folder>(
            "INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(name)
        .bind(path)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;

        Ok(folder)
    }

    pub async fn get_all_by_user(user_id: &str) -> Result<Vec<Folder>, String> {
        let pool = get_pool().await;
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())
    }
}
