#[cfg(feature = "server")]
use dioxus::fullstack::Lazy;
#[cfg(feature = "server")]
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};

#[cfg(feature = "server")]
use crate::config::CONFIG;

#[cfg(feature = "server")]
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The file a `sqlite:` database URL names; `None` for an in-memory database
/// or another kind of URL.
#[cfg(feature = "server")]
pub(crate) fn database_file(database_url: &str) -> Option<&std::path::Path> {
    let path = database_url.strip_prefix("sqlite:")?;
    (!path.is_empty() && path != ":memory:").then(|| std::path::Path::new(path))
}

#[cfg(feature = "server")]
pub static DB: Lazy<SqlitePool> = Lazy::new(|| async move {
    let database_url = CONFIG.database_url();

    if let Some(path) = database_file(database_url) {
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
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::path::Path;

    pub const HARDENING: i64 = 20261114000000;
    pub const USERNAME_CASE: i64 = 20261115000001;
    pub const FOREIGN_FOLDER_CHOICES: i64 = 20261115000002;
    pub const DELETION_AUDIT: i64 = 20261116000004;
    pub const MCP_JOBS: i64 = 20261117000000;
    pub const DELETE_TOOLS: i64 = 20261120000000;
    pub const USER_CHANGE_NUMBERS: i64 = 20261123000000;
    pub const REPORT_REAL_PATH_STATUS: i64 = 20261124000000;

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

    /// A migrated database in `dir/oauth.db` with up to 4 connections, for
    /// tests of writers running at once (`migrated` has one connection, which
    /// serializes them).
    pub async fn migrated_file(dir: &Path) -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(dir.join("oauth.db"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("file database");
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

    async fn folder_choices(db: &SqlitePool, user: &str) -> (Option<String>, Option<String>) {
        sqlx::query_as(
            "SELECT discovery_folder_id, default_download_folder_id FROM user_settings \
             WHERE user_id = ?",
        )
        .bind(user)
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn folder_choices_naming_another_users_folder_are_cleared() {
        let db = test_db::migrated_before(test_db::FOREIGN_FOLDER_CHOICES).await;
        insert_user(&db, "u1", "bob").await;
        insert_user(&db, "u2", "eve").await;
        insert_folder(&db, "u1", "/music/bob").await;
        insert_folder(&db, "u2", "/music/eve").await;
        // Bob chose Eve's folder for both; Eve chose her own and a folder
        // that no longer exists.
        for (user, discovery, download) in [
            ("u1", "/music/eve", "/music/eve"),
            ("u2", "/music/eve", "gone"),
        ] {
            sqlx::query(
                "INSERT INTO user_settings (user_id, discovery_folder_id, default_download_folder_id) \
                 VALUES (?, ?, ?)",
            )
            .bind(user)
            .bind(discovery)
            .bind(download)
            .execute(&db)
            .await
            .unwrap();
        }
        test_db::apply(&db, test_db::FOREIGN_FOLDER_CHOICES).await;
        assert_eq!(folder_choices(&db, "u1").await, (None, None));
        assert_eq!(
            folder_choices(&db, "u2").await,
            (Some("/music/eve".into()), Some("gone".into()))
        );
    }

    /// One client, one grant and one token for `user`.
    async fn grant_for(db: &SqlitePool, user: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO oauth_clients (client_id, kind, redirect_uris, created_at) \
             VALUES ('dcr_c', 'dcr', '[\"https://claude.ai/api/mcp/auth_callback\"]', 0)",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_grants (id, user_id, client_id, redirect_uri, resource, scope, created_at) \
             VALUES (?, ?, 'dcr_c', 'https://claude.ai/api/mcp/auth_callback', \
             'https://music.example.com/mcp', 'soulful', 0)",
        )
        .bind(format!("g-{user}"))
        .bind(user)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_tokens (token_hash, grant_id, kind, expires_at) \
             VALUES (?, ?, 'access', 9999999999)",
        )
        .bind(format!("h-{user}"))
        .bind(format!("g-{user}"))
        .execute(db)
        .await
        .unwrap();
    }

    /// (grants, tokens) of `user`.
    async fn grants_of(db: &SqlitePool, user: &str) -> (i64, i64) {
        let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_grants WHERE user_id = ?")
            .bind(user)
            .fetch_one(db)
            .await
            .unwrap();
        let tokens: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_tokens WHERE grant_id = ?")
                .bind(format!("g-{user}"))
                .fetch_one(db)
                .await
                .unwrap();
        (grants, tokens)
    }

    /// One unredeemed code and one consent request waiting for `user`; the
    /// client of `grant_for` must exist.
    async fn pending_for(db: &SqlitePool, user: &str) {
        sqlx::query(
            "INSERT INTO oauth_codes (code_hash, user_id, client_id, redirect_uri, \
             code_challenge, scope, resource, expires_at) \
             VALUES (?, ?, 'dcr_c', 'https://claude.ai/api/mcp/auth_callback', 'x', 'soulful', \
             'https://music.example.com/mcp', 9999999999)",
        )
        .bind(format!("c-{user}"))
        .bind(user)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_requests (id_hash, user_id, client_id, client_kind, \
             client_redirect_uris, redirect_uri, code_challenge, scope, resource, expires_at) \
             VALUES (?, ?, 'dcr_c', 'dcr', '[\"https://claude.ai/api/mcp/auth_callback\"]', \
             'https://claude.ai/api/mcp/auth_callback', 'x', 'soulful', \
             'https://music.example.com/mcp', 9999999999)",
        )
        .bind(format!("r-{user}"))
        .bind(user)
        .execute(db)
        .await
        .unwrap();
    }

    /// (codes, consent requests) of `user`.
    async fn pending_of(db: &SqlitePool, user: &str) -> (i64, i64) {
        let codes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes WHERE user_id = ?")
            .bind(user)
            .fetch_one(db)
            .await
            .unwrap();
        let requests: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_requests WHERE user_id = ?")
                .bind(user)
                .fetch_one(db)
                .await
                .unwrap();
        (codes, requests)
    }

    #[tokio::test]
    async fn a_credential_change_ends_the_users_connected_apps() {
        let db = test_db::migrated().await;
        insert_user(&db, "u1", "bob").await;
        insert_user(&db, "u2", "eve").await;
        for user in ["u1", "u2"] {
            grant_for(&db, user).await;
            pending_for(&db, user).await;
        }
        sqlx::query(
            "UPDATE users SET credentials_version = credentials_version + 1 WHERE id = 'u1'",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(grants_of(&db, "u1").await, (0, 0));
        assert_eq!(pending_of(&db, "u1").await, (0, 0));
        assert_eq!(grants_of(&db, "u2").await, (1, 1));
        assert_eq!(pending_of(&db, "u2").await, (1, 1));
    }

    #[tokio::test]
    async fn an_update_that_keeps_the_version_keeps_the_apps() {
        // The Navidrome login adds (navidrome_token IS NULL): 0 for a linked
        // row (User::link_navidrome_in).
        let db = test_db::migrated().await;
        insert_user(&db, "u1", "bob").await;
        grant_for(&db, "u1").await;
        sqlx::query(
            "UPDATE users SET credentials_version = credentials_version + 0, \
             navidrome_status = 'connected' WHERE id = 'u1'",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(grants_of(&db, "u1").await, (1, 1));
    }

    #[tokio::test]
    async fn deleting_a_user_deletes_their_apps() {
        let db = test_db::migrated().await;
        insert_user(&db, "u1", "bob").await;
        insert_user(&db, "u2", "eve").await;
        grant_for(&db, "u1").await;
        grant_for(&db, "u2").await;
        sqlx::query("DELETE FROM users WHERE id = 'u1'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            (grants_of(&db, "u1").await, grants_of(&db, "u2").await),
            ((0, 0), (1, 1))
        );
    }

    #[tokio::test]
    async fn deleting_a_client_deletes_its_grants_and_codes() {
        let db = test_db::migrated().await;
        insert_user(&db, "u1", "bob").await;
        grant_for(&db, "u1").await;
        sqlx::query(
            "INSERT INTO oauth_codes (code_hash, user_id, client_id, redirect_uri, \
             code_challenge, scope, resource, expires_at) \
             VALUES ('c', 'u1', 'dcr_c', 'https://claude.ai/api/mcp/auth_callback', 'x', \
             'soulful', 'https://music.example.com/mcp', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("DELETE FROM oauth_clients WHERE client_id = 'dcr_c'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(grants_of(&db, "u1").await, (0, 0));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM oauth_codes")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_file_database_holds_four_connections_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db::migrated_file(dir.path()).await;
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(db.acquire().await.unwrap());
        }
        assert_eq!(db.size(), 4);
    }

    #[tokio::test]
    async fn the_migration_gives_existing_jobs_a_batch_and_a_kind() {
        let db = test_db::migrated_before(test_db::MCP_JOBS).await;
        insert_user(&db, "u1", "bob").await;
        insert_folder(&db, "u1", "/music/bob").await;
        let album = r#"{"album":{"id":"r1","title":"Winter Songs","artist":"Marrow Choir","release_date":null},"tracks":[]}"#;
        let track = r#"{"album":null,"tracks":[]}"#;
        for (id, wanted, label) in [
            ("j-album", album, "Winter Songs"),
            ("j-track", track, "O Holy Night"),
        ] {
            sqlx::query(
                "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, \
                 import_mode, state, batch_label) VALUES (?, 'u1', '/music/bob', 7, 7, ?, 'album', 'done', ?)",
            )
            .bind(id)
            .bind(wanted)
            .bind(label)
            .execute(&db)
            .await
            .unwrap();
        }
        test_db::apply(&db, test_db::MCP_JOBS).await;
        let jobs: Vec<(String, String, i64, Option<String>, String, String)> = sqlx::query_as(
            "SELECT j.id, b.id, b.created_at, b.label, b.origin, j.kind \
             FROM jobs j JOIN batches b ON b.id = j.batch_id ORDER BY j.id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            jobs,
            vec![
                (
                    "j-album".into(),
                    "j-album".into(),
                    7,
                    Some("Winter Songs".into()),
                    "ui".into(),
                    "album".into()
                ),
                (
                    "j-track".into(),
                    "j-track".into(),
                    7,
                    Some("O Holy Night".into()),
                    "ui".into(),
                    "track".into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_user_marked_for_report_real_path_is_connected() {
        let db = test_db::migrated_before(test_db::REPORT_REAL_PATH_STATUS).await;
        insert_user(&db, "u1", "bob").await;
        insert_user(&db, "u2", "carol").await;
        for (id, status) in [("u1", "missing_report_real_path"), ("u2", "offline")] {
            sqlx::query("UPDATE users SET navidrome_status = ? WHERE id = ?")
                .bind(status)
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
        }
        test_db::apply(&db, test_db::REPORT_REAL_PATH_STATUS).await;
        let statuses: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, navidrome_status FROM users WHERE id IN ('u1', 'u2') ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("u1".to_string(), "connected".to_string()),
                ("u2".to_string(), "offline".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn a_jobs_user_change_number_starts_at_its_change_number() {
        let db = test_db::migrated_before(test_db::USER_CHANGE_NUMBERS).await;
        insert_user(&db, "u1", "bob").await;
        insert_folder(&db, "u1", "/music/bob").await;
        sqlx::query(
            "INSERT INTO batches (id, user_id, origin, created_at) VALUES ('b1', 'u1', 'ui', 7)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, \
             import_mode, state, batch_id, kind, changed_seq) \
             VALUES ('j1', 'u1', '/music/bob', 7, 7, '{}', 'album', 'done', 'b1', 'album', 41)",
        )
        .execute(&db)
        .await
        .unwrap();
        test_db::apply(&db, test_db::USER_CHANGE_NUMBERS).await;
        let seq: i64 = sqlx::query_scalar("SELECT user_seq FROM jobs WHERE id = 'j1'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(seq, 41);
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
