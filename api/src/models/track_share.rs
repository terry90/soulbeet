#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use sqlx::SqlitePool;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::Type))]
#[cfg_attr(feature = "server", sqlx(rename_all = "snake_case"))]
#[serde(rename_all = "snake_case")]
pub enum LinkMode {
    Hardlink,
    Reflink,
    Copy,
}

impl LinkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkMode::Hardlink => "hardlink",
            LinkMode::Reflink => "reflink",
            LinkMode::Copy => "copy",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::Type))]
#[cfg_attr(feature = "server", sqlx(rename_all = "snake_case"))]
#[serde(rename_all = "snake_case")]
pub enum ShareOrigin {
    Ui,
    Mcp,
    Migration,
}

impl ShareOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareOrigin::Ui => "ui",
            ShareOrigin::Mcp => "mcp",
            ShareOrigin::Migration => "migration",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareDirection {
    #[default]
    All,
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct ShareHistoryEntry {
    pub id: String,
    pub batch_id: String,
    pub sender_user_id: String,
    pub sender_username: String,
    pub recipient_user_id: String,
    pub recipient_username: String,
    pub source_song_id: String,
    pub target_song_id: Option<String>,
    pub source_path: String,
    pub target_path: String,
    pub artist: String,
    pub album: String,
    pub title: String,
    pub link_mode: LinkMode,
    pub origin: ShareOrigin,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareHistoryPage {
    pub entries: Vec<ShareHistoryEntry>,
    pub next_cursor: Option<String>,
}

#[cfg(feature = "server")]
pub struct NewTrackShare<'a> {
    pub batch_id: &'a str,
    pub sender_user_id: &'a str,
    pub recipient_user_id: &'a str,
    pub source_song_id: &'a str,
    pub target_song_id: Option<&'a str>,
    pub source_path: &'a str,
    pub target_path: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub title: &'a str,
    pub link_mode: LinkMode,
    pub origin: ShareOrigin,
}

#[cfg(feature = "server")]
pub struct TrackShareRow;

#[cfg(feature = "server")]
impl TrackShareRow {
    pub(crate) async fn insert_in(
        db: &SqlitePool,
        record: &NewTrackShare<'_>,
    ) -> Result<String, String> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO track_shares (
                id, batch_id, sender_user_id, recipient_user_id,
                source_song_id, target_song_id, source_path, target_path,
                artist, album, title, link_mode, origin, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(record.batch_id)
        .bind(record.sender_user_id)
        .bind(record.recipient_user_id)
        .bind(record.source_song_id)
        .bind(record.target_song_id)
        .bind(record.source_path)
        .bind(record.target_path)
        .bind(record.artist)
        .bind(record.album)
        .bind(record.title)
        .bind(record.link_mode.as_str())
        .bind(record.origin.as_str())
        .bind(&now)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(id)
    }

    pub async fn page_for_user(
        user_id: &str,
        direction: ShareDirection,
        limit: usize,
        offset: usize,
    ) -> Result<ShareHistoryPage, String> {
        Self::page_for_user_in(&DB, user_id, direction, limit, offset).await
    }

    pub(crate) async fn page_for_user_in(
        db: &SqlitePool,
        user_id: &str,
        direction: ShareDirection,
        limit: usize,
        offset: usize,
    ) -> Result<ShareHistoryPage, String> {
        let limit = limit.clamp(1, 100) as i64;
        let offset = offset as i64;
        let where_clause = match direction {
            ShareDirection::Incoming => "ts.recipient_user_id = ?",
            ShareDirection::Outgoing => "ts.sender_user_id = ?",
            ShareDirection::All => "(ts.sender_user_id = ? OR ts.recipient_user_id = ?)",
        };
        let sql = format!(
            "SELECT ts.id, ts.batch_id, ts.sender_user_id, su.username AS sender_username, \
                    ts.recipient_user_id, ru.username AS recipient_username, \
                    ts.source_song_id, ts.target_song_id, ts.source_path, ts.target_path, \
                    ts.artist, ts.album, ts.title, ts.link_mode, ts.origin, ts.created_at \
             FROM track_shares ts \
             JOIN users su ON su.id = ts.sender_user_id \
             JOIN users ru ON ru.id = ts.recipient_user_id \
             WHERE {where_clause} \
             ORDER BY ts.created_at DESC, ts.rowid DESC \
             LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query_as::<_, ShareHistoryEntry>(&sql).bind(user_id);
        if direction == ShareDirection::All {
            q = q.bind(user_id);
        }
        let mut rows = q
            .bind(limit + 1)
            .bind(offset)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?;
        let next_cursor = if rows.len() > limit as usize {
            rows.truncate(limit as usize);
            Some((offset + limit).to_string())
        } else {
            None
        };
        Ok(ShareHistoryPage {
            entries: rows,
            next_cursor,
        })
    }
}
