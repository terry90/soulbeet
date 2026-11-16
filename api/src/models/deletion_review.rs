#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use shared::navidrome::{DeletionReason, DeletionReview};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

/// A row of the deletion audit.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct DeletionReviewRow {
    pub id: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: Option<String>,
    pub rating: Option<i32>,
    pub reason: String,
    pub created_at: String,
    pub origin: String,
    pub client: Option<String>,
}

/// One deletion, as the guard records it.
#[cfg(feature = "server")]
#[derive(Clone, Debug)]
pub struct DeletionRecord {
    /// The row's id, chosen before the removal so a removal that lost track of itself can
    /// record it once.
    pub id: String,
    pub user_id: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    /// The caller's own rating, None when unrated.
    pub rating: Option<u8>,
    /// What started the removal: "ui", "auto" or "mcp".
    pub origin: &'static str,
    pub reason: DeletionReason,
    /// The connected app's host for an "mcp" removal, else None.
    pub client: Option<String>,
}

#[cfg(feature = "server")]
impl TryFrom<DeletionReviewRow> for DeletionReview {
    type Error = String;

    fn try_from(row: DeletionReviewRow) -> Result<Self, String> {
        let reason = DeletionReason::parse(&row.reason)
            .ok_or_else(|| format!("Unknown deletion reason {}", row.reason))?;
        Ok(DeletionReview {
            id: row.id,
            song_id: row.song_id,
            title: row.title,
            artist: row.artist,
            album: row.album,
            path: row.path,
            rating: row.rating.map(|r| r as u8),
            reason,
            created_at: row.created_at,
            origin: row.origin,
            client: row.client,
        })
    }
}

#[cfg(feature = "server")]
impl DeletionReviewRow {
    /// Records `record` on `conn` (inside the caller's transaction, when it has one), unless a
    /// row with its id is there already.
    pub(crate) async fn insert_in(
        conn: &mut sqlx::SqliteConnection,
        record: &DeletionRecord,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, path, rating, origin, reason, client)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&record.id)
        .bind(&record.user_id)
        .bind(&record.song_id)
        .bind(&record.title)
        .bind(&record.artist)
        .bind(&record.album)
        .bind(&record.path)
        .bind(record.rating.map(i32::from))
        .bind(record.origin)
        .bind(record.reason.as_str())
        .bind(&record.client)
        .execute(conn)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn history(user_id: &str, limit: u32) -> Result<Vec<DeletionReview>, String> {
        Self::history_in(&DB, user_id, limit).await
    }

    /// The user's latest `limit` deletions, newest first.
    pub(crate) async fn history_in(
        db: &SqlitePool,
        user_id: &str,
        limit: u32,
    ) -> Result<Vec<DeletionReview>, String> {
        Self::page_in(db, user_id, 0, limit).await
    }

    /// The user's deletions newest first (ties by id, as created_at has whole seconds), `limit`
    /// of them from `offset`, with what started each and the app's host. A reason this version
    /// does not know is an error.
    pub(crate) async fn page_in(
        db: &SqlitePool,
        user_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<DeletionReview>, String> {
        sqlx::query_as::<_, DeletionReviewRow>(
            "SELECT id, song_id, title, artist, album, path, rating, reason, created_at, origin, client
             FROM deletion_reviews WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(DeletionReview::try_from)
        .collect()
    }

    /// The song id and path of each of the user's recorded deletions.
    pub(crate) async fn deleted_files_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, String> {
        sqlx::query_as(
            "SELECT song_id, path FROM deletion_reviews WHERE user_id = ? AND path IS NOT NULL",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    /// "artist:title" lowercased, of the user's tracks deleted as disliked.
    pub async fn disliked_keys(user_id: &str) -> Result<Vec<String>, String> {
        Self::disliked_keys_in(&DB, user_id).await
    }

    pub(crate) async fn disliked_keys_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<String>, String> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT artist, title FROM deletion_reviews WHERE user_id = ? AND reason = ?",
        )
        .bind(user_id)
        .bind(DeletionReason::Disliked.as_str())
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(artist, title)| format!("{}:{}", artist.to_lowercase(), title.to_lowercase()))
            .collect())
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn a_deletion_recorded_before_the_upgrade_reads_as_a_disliked_auto_deletion() {
        let db = test_db::migrated_before(test_db::DELETION_AUDIT).await;
        sqlx::query(
            "INSERT INTO deletion_reviews (id, song_id, title, artist, album, path, rating, action, user_id) \
             VALUES ('r1', 's1', 'O Holy Night', 'Marrow Choir', 'Winter Songs', '/music/x.flac', 1, 'Deleted', 'alice')",
        )
        .execute(&db)
        .await
        .unwrap();
        test_db::apply(&db, test_db::DELETION_AUDIT).await;
        test_db::apply(&db, test_db::DELETE_TOOLS).await;
        let history = DeletionReviewRow::history_in(&db, "alice", 10)
            .await
            .unwrap();
        assert_eq!(
            (history[0].song_id.as_str(), history[0].reason),
            ("s1", DeletionReason::Disliked)
        );
    }

    #[tokio::test]
    async fn a_row_from_before_the_upgrade_has_no_app() {
        let db = test_db::migrated_before(test_db::DELETE_TOOLS).await;
        sqlx::query(
            "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, path, rating, origin, reason) \
             VALUES ('r1', 'alice', 's1', 'O Holy Night', 'Marrow Choir', 'Winter Songs', '/music/x.flac', 1, 'auto', 'disliked')",
        )
        .execute(&db)
        .await
        .unwrap();
        test_db::apply(&db, test_db::DELETE_TOOLS).await;
        let page = DeletionReviewRow::page_in(&db, "alice", 0, 10)
            .await
            .unwrap();
        assert_eq!(
            (
                page.len(),
                page[0].origin.as_str(),
                page[0].client.as_deref(),
                page[0].reason
            ),
            (1, "auto", None, DeletionReason::Disliked)
        );
    }

    #[tokio::test]
    async fn pages_run_newest_first() {
        let db = test_db::migrated().await;
        for (title, ago) in [
            ("first", "-3 hours"),
            ("second", "-2 hours"),
            ("third", "-1 hours"),
        ] {
            sqlx::query(
                "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, origin, reason, client, created_at) \
                 VALUES (?, 'alice', 's', ?, 'Marrow Choir', 'Winter Songs', 'mcp', 'cleanup', 'claude.ai', datetime('now', ?))",
            )
            .bind(title)
            .bind(title)
            .bind(ago)
            .execute(&db)
            .await
            .unwrap();
        }
        let titles = |page: Vec<DeletionReview>| -> Vec<String> {
            page.into_iter().map(|r| r.title).collect()
        };
        let all = DeletionReviewRow::page_in(&db, "alice", 0, 10)
            .await
            .unwrap();
        assert_eq!(
            (
                all[0].origin.as_str(),
                all[0].client.as_deref(),
                all[0].reason
            ),
            ("mcp", Some("claude.ai"), DeletionReason::Cleanup)
        );
        assert_eq!(titles(all), ["third", "second", "first"]);
        assert_eq!(
            titles(
                DeletionReviewRow::page_in(&db, "alice", 1, 1)
                    .await
                    .unwrap()
            ),
            ["second"]
        );
        assert_eq!(
            titles(
                DeletionReviewRow::history_in(&db, "alice", 2)
                    .await
                    .unwrap()
            ),
            titles(
                DeletionReviewRow::page_in(&db, "alice", 0, 2)
                    .await
                    .unwrap()
            )
        );
    }

    #[tokio::test]
    async fn a_reason_this_version_does_not_know_is_an_error() {
        let db = test_db::migrated().await;
        sqlx::query(
            "INSERT INTO deletion_reviews (id, user_id, song_id, title, artist, album, origin, reason) \
             VALUES ('r1', 'alice', 's', 't', 'a', 'al', 'auto', 'because')",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            DeletionReviewRow::page_in(&db, "alice", 0, 10)
                .await
                .unwrap_err(),
            "Unknown deletion reason because"
        );
    }

    #[tokio::test]
    async fn discovery_excludes_only_tracks_deleted_as_disliked() {
        let db = test_db::migrated().await;
        let mut conn = db.acquire().await.unwrap();
        for (title, reason) in [
            ("O Holy Night", DeletionReason::Disliked),
            ("Silent Night", DeletionReason::Replaced),
        ] {
            DeletionReviewRow::insert_in(
                &mut conn,
                &DeletionRecord {
                    id: title.to_string(),
                    user_id: "alice".into(),
                    song_id: "s".into(),
                    title: title.into(),
                    artist: "Marrow Choir".into(),
                    album: "Winter Songs".into(),
                    path: "/music/x.flac".into(),
                    rating: Some(1),
                    origin: "auto",
                    reason,
                    client: None,
                },
            )
            .await
            .unwrap();
        }
        drop(conn);
        assert_eq!(
            DeletionReviewRow::disliked_keys_in(&db, "alice")
                .await
                .unwrap(),
            vec!["marrow choir:o holy night".to_string()]
        );
        assert_eq!(
            DeletionReviewRow::history_in(&db, "alice", 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
