#[cfg(feature = "server")]
use dioxus::fullstack::Lazy;
#[cfg(feature = "server")]
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
#[cfg(feature = "server")]
use std::{str::FromStr, time::Duration};

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

/// How long a statement waits for another connection's write lock before it
/// fails with "database is locked". SQLite's writers take turns; a search, a
/// job and the session checks of every page load write at the same time.
#[cfg(feature = "server")]
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection idle this long is closed, and none lives longer than
/// `MAX_LIFETIME`, so a connection left holding a lock does not hold it
/// until the server restarts.
#[cfg(feature = "server")]
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
#[cfg(feature = "server")]
pub(crate) const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// The connection settings for `database_url`: WAL, so readers never wait for
/// a writer (session checks no longer fail while a search is being recorded),
/// and `BUSY_TIMEOUT` for writers waiting on each other. WAL is stored in the
/// database file, so an existing database switches on its next start.
#[cfg(feature = "server")]
pub(crate) fn connect_options(database_url: &str) -> Result<SqliteConnectOptions, sqlx::Error> {
    Ok(SqliteConnectOptions::from_str(database_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(BUSY_TIMEOUT))
}

#[cfg(feature = "server")]
pub(crate) fn pool_options() -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .max_connections(5)
        .idle_timeout(IDLE_TIMEOUT)
        .max_lifetime(MAX_LIFETIME)
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

    let options = connect_options(database_url).expect("Invalid DATABASE_URL");
    let pool = pool_options()
        .connect_with(options)
        .await
        .expect("Failed to connect to database");

    migrate(&pool, shared::changelog::running_version())
        .await
        .expect("Failed to run migrations");

    dioxus::Ok(pool)
});

/// Runs the migrations. On a new database the accounts they seed are new
/// users, so they start at `running`, the version of this build: release
/// notes are for the accounts of an install that ran an earlier version.
#[cfg(feature = "server")]
pub(crate) async fn migrate(
    pool: &SqlitePool,
    running: shared::changelog::Version,
) -> Result<(), sqlx::Error> {
    let new: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    MIGRATOR.run(pool).await?;
    if new {
        sqlx::query("UPDATE users SET changelog_seen = ?")
            .bind(running.to_string())
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "server"))]
pub(crate) mod test_db {
    use super::{migrate, MIGRATOR};
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
    pub const CHANGELOG_SEEN: i64 = 20261125000000;
    pub const REMOVE_LASTFM: i64 = 20261130000000;

    pub async fn empty() -> SqlitePool {
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
        migrate(&pool, shared::changelog::running_version())
            .await
            .expect("migrations");
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
        migrate(&pool, shared::changelog::running_version())
            .await
            .expect("migrations");
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

    /// Applies the migration `version` and every later one, as an upgrade to
    /// this build does.
    pub async fn apply_from(pool: &SqlitePool, version: i64) {
        for m in MIGRATOR.iter().filter(|m| m.version >= version) {
            sqlx::raw_sql(&m.sql)
                .execute(pool)
                .await
                .expect("migration");
        }
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

    /// A database file opened the way the server opens `DATABASE_URL`.
    async fn server_pool(dir: &std::path::Path) -> SqlitePool {
        let path = dir.join("soulbeet.db");
        std::fs::File::create(&path).unwrap();
        let url = format!("sqlite:{}", path.display());
        super::pool_options()
            .connect_with(super::connect_options(&url).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_server_database_uses_wal_and_waits_for_writers() {
        let dir = tempfile::tempdir().unwrap();
        let db = server_pool(dir.path()).await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(timeout, super::BUSY_TIMEOUT.as_millis() as i64);
    }

    #[tokio::test]
    async fn a_reader_does_not_wait_for_an_open_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = server_pool(dir.path()).await;
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&db)
            .await
            .unwrap();
        // EXCLUSIVE holds the lock a rollback-journal commit takes. Without
        // WAL this read waits for it, then fails with "database is locked";
        // in WAL mode it does not stop readers.
        let mut writer = db.begin_with("BEGIN EXCLUSIVE").await.unwrap();
        sqlx::query("INSERT INTO t (x) VALUES (1)")
            .execute(&mut *writer)
            .await
            .unwrap();
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM t").fetch_one(&db),
        )
        .await
        .expect("the read waited for the writer")
        .unwrap();
        assert_eq!(seen, 0);
        writer.commit().await.unwrap();
    }

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

    async fn changelog_seen_of(db: &SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT changelog_seen FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn accounts_from_before_release_notes_have_seen_0_8_0() {
        let db = test_db::migrated_before(test_db::CHANGELOG_SEEN).await;
        insert_user(&db, "u1", "bob").await;
        test_db::apply(&db, test_db::CHANGELOG_SEEN).await;
        assert_eq!(changelog_seen_of(&db, SEEDED).await, "0.8.0");
        assert_eq!(changelog_seen_of(&db, "u1").await, "0.8.0");
    }

    #[tokio::test]
    async fn a_new_install_starts_its_seeded_admin_at_the_running_version() {
        let db = test_db::empty().await;
        let running = shared::changelog::Version::parse("9.4.1").unwrap();
        super::migrate(&db, running).await.unwrap();
        assert_eq!(changelog_seen_of(&db, SEEDED).await, "9.4.1");

        // A later start migrates an existing database, which keeps what each account saw.
        insert_user(&db, "u1", "bob").await;
        sqlx::query("UPDATE users SET changelog_seen = '0.9.0' WHERE id = 'u1'")
            .execute(&db)
            .await
            .unwrap();
        let newer = shared::changelog::Version::parse("9.5.0").unwrap();
        super::migrate(&db, newer).await.unwrap();
        assert_eq!(changelog_seen_of(&db, SEEDED).await, "9.4.1");
        assert_eq!(changelog_seen_of(&db, "u1").await, "0.9.0");
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
    async fn rows_from_0_9_0_load_once_last_fm_is_removed() {
        let db = test_db::migrated_before(test_db::REMOVE_LASTFM).await;
        insert_user(&db, "u1", "bob").await;
        insert_folder(&db, "u1", "/music/bob").await;
        sqlx::query(
            "INSERT INTO user_settings (user_id, default_metadata_provider, lastfm_api_key, \
             lastfm_username, listenbrainz_username, discovery_track_count, \
             discovery_lifetime_days) VALUES ('u1', 'lastfm', '3f2b9c1e8d7a6b5c4d3e2f1a0b9c8d7e', \
             'bob_fm', 'bob_lb', ?, ?)",
        )
        // The per-profile JSON 0.9.0 keeps in these two columns.
        .bind(r#"{"Conservative":10,"Balanced":10,"Adventurous":10}"#)
        .bind(r#"{"Conservative":7,"Balanced":7,"Adventurous":7}"#)
        .execute(&db)
        .await
        .unwrap();
        // An album a Last.fm search gave, kept by a job and its search as 0.9.0 wrote them.
        let wanted = r#"{"album":{"id":"lastfm:album:marrow choir:winter songs","title":"Winter Songs","artist":"Marrow Choir","release_date":null},"tracks":[],"backend":null,"metadata_provider":"lastfm"}"#;
        sqlx::query(
            "INSERT INTO batches (id, user_id, origin, created_at) VALUES ('b1', 'u1', 'ui', 7)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO searches (id, user_id, expires_at, query) VALUES ('s1', 'u1', 9999999999, ?)",
        )
        .bind(wanted)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, \
             import_mode, state, batch_id, kind, search_id) \
             VALUES ('j1', 'u1', '/music/bob', 7, 7, ?, 'album', 'searching', 'b1', 'album', 's1')",
        )
        .bind(wanted)
        .execute(&db)
        .await
        .unwrap();
        test_db::apply(&db, test_db::REMOVE_LASTFM).await;
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('user_settings')")
                .fetch_all(&db)
                .await
                .unwrap();
        for gone in [
            "default_metadata_provider",
            "lastfm_api_key",
            "lastfm_username",
        ] {
            assert!(!columns.iter().any(|c| c == gone), "{gone}");
        }
        let s = crate::models::user_settings::UserSettings::get_in(&db, "u1")
            .await
            .unwrap();
        assert_eq!(s.listenbrainz_username.as_deref(), Some("bob_lb"));
        let user = crate::auth::UserId::new("u1");
        let job = crate::ledger::store::job_record(&db, "j1", &user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job.wanted.album.unwrap().id,
            "lastfm:album:marrow choir:winter songs"
        );
        let search = crate::ledger::store::search_of(&db, "s1", &user)
            .await
            .unwrap()
            .unwrap();
        assert!(search.query.tracks.is_empty());
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
