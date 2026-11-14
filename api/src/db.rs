#[cfg(feature = "server")]
use dioxus::fullstack::Lazy;
#[cfg(feature = "server")]
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};

#[cfg(feature = "server")]
use crate::config::CONFIG;

#[cfg(feature = "server")]
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[cfg(feature = "server")]
pub static DB: Lazy<SqlitePool> = Lazy::new(|| async move {
    let database_url = CONFIG.database_url();

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
        .connect(database_url)
        .await
        .expect("Failed to connect to database");

    MIGRATOR.run(&pool).await.expect("Failed to run migrations");

    dioxus::Ok(pool)
});

#[cfg(all(test, feature = "server"))]
pub(crate) mod test_db {
    use super::MIGRATOR;
    use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};

    pub const HARDENING: i64 = 20261114000000;

    async fn empty() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory database")
    }

    pub async fn migrated() -> SqlitePool {
        let pool = empty().await;
        MIGRATOR.run(&pool).await.expect("migrations");
        pool
    }

    pub async fn migrated_before(version: i64) -> SqlitePool {
        let pool = empty().await;
        for m in MIGRATOR.iter().filter(|m| m.version < version) {
            sqlx::raw_sql(&m.sql)
                .execute(&pool)
                .await
                .expect("migration");
        }
        pool
    }

    pub async fn apply(pool: &SqlitePool, version: i64) {
        let m = MIGRATOR
            .iter()
            .find(|m| m.version == version)
            .expect("migration version exists");
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .expect("migration");
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::test_db;
    use sqlx::SqlitePool;

    const SEEDED: &str = "00000000-0000-0000-0000-000000000000";

    async fn insert_user(db: &SqlitePool, id: &str, name: &str) {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id)
            .bind(name)
            .execute(db)
            .await
            .unwrap();
    }

    async fn insert_folder(db: &SqlitePool, user: &str, path: &str) {
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'f', ?)")
            .bind(path)
            .bind(user)
            .bind(path)
            .execute(db)
            .await
            .unwrap();
    }

    async fn role_of(db: &SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn version_of(db: &SqlitePool, id: &str) -> i64 {
        sqlx::query_scalar("SELECT credentials_version FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hardening_makes_the_seeded_admin_an_admin() {
        let db = test_db::migrated_before(test_db::HARDENING).await;
        insert_user(&db, "u1", "bob").await;
        test_db::apply(&db, test_db::HARDENING).await;
        assert_eq!(role_of(&db, SEEDED).await, "admin");
        assert_eq!(role_of(&db, "u1").await, "user");
        assert_eq!(version_of(&db, "u1").await, 0);
    }

    #[tokio::test]
    async fn hardening_promotes_the_first_user_when_the_seeded_admin_is_gone() {
        let db = test_db::migrated_before(test_db::HARDENING).await;
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(SEEDED)
            .execute(&db)
            .await
            .unwrap();
        insert_user(&db, "u1", "bob").await;
        insert_user(&db, "u2", "eve").await;
        test_db::apply(&db, test_db::HARDENING).await;
        assert_eq!(role_of(&db, "u1").await, "admin");
        assert_eq!(role_of(&db, "u2").await, "user");
    }

    #[tokio::test]
    async fn hardening_approves_the_parents_of_admin_folders() {
        let db = test_db::migrated_before(test_db::HARDENING).await;
        insert_user(&db, "u1", "bob").await;
        for path in ["/music/admin", "/srv/a/b/", "/music/other", "/top"] {
            insert_folder(&db, SEEDED, path).await;
        }
        insert_folder(&db, "u1", "/home/bob/music").await;
        test_db::apply(&db, test_db::HARDENING).await;
        let roots: Vec<String> = sqlx::query_scalar("SELECT path FROM folder_roots ORDER BY path")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(roots, vec!["/music", "/srv/a"]);
    }

    #[tokio::test]
    async fn role_accepts_only_admin_and_user() {
        let db = test_db::migrated().await;
        let r = sqlx::query("UPDATE users SET role = 'owner'")
            .execute(&db)
            .await;
        assert!(r.is_err());
    }
}
