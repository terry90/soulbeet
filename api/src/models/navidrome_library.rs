use sqlx::SqlitePool;

/// Where a Navidrome library is on this machine.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct NavidromeLibraryRow {
    pub library_id: i64,
    /// The library's path as Navidrome reports it (`libraryPath`).
    pub navidrome_path: String,
    /// Where that path is on this machine.
    pub local_path: String,
}

impl NavidromeLibraryRow {
    pub(crate) async fn get_in(db: &SqlitePool, library_id: i64) -> Result<Option<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT library_id, navidrome_path, local_path FROM navidrome_libraries \
             WHERE library_id = ?",
        )
        .bind(library_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }

    /// Stores `row`, replacing what was known of its library.
    pub(crate) async fn upsert_in(db: &SqlitePool, row: &Self) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO navidrome_libraries (library_id, navidrome_path, local_path) \
             VALUES (?, ?, ?) \
             ON CONFLICT (library_id) DO UPDATE SET navidrome_path = excluded.navidrome_path, \
             local_path = excluded.local_path, learned_at = datetime('now')",
        )
        .bind(row.library_id)
        .bind(&row.navidrome_path)
        .bind(&row.local_path)
        .execute(db)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}
