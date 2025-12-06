#[cfg(feature = "server")]
use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite};
#[cfg(feature = "server")]
use tokio::sync::OnceCell;

#[cfg(feature = "server")]
static POOL: OnceCell<Pool<Sqlite>> = OnceCell::const_new();

#[cfg(feature = "server")]
pub async fn get_pool() -> &'static Pool<Sqlite> {
    POOL.get_or_init(|| async {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:soulbeet.db".to_string());

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
