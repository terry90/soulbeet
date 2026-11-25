//! Audit log of library track moves across folders.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRecord {
    pub id: String,
    pub user_id: String,
    pub song_id: String,
    pub new_song_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_folder_id: String,
    pub source_path: String,
    pub target_folder_id: String,
    pub target_path: String,
    pub origin: String,
    pub client: Option<String>,
    pub status: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveHistoryEntry {
    pub id: String,
    pub user_id: String,
    pub song_id: String,
    pub new_song_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_folder_id: String,
    pub source_path: String,
    pub target_folder_id: String,
    pub target_path: String,
    pub origin: String,
    pub client: Option<String>,
    pub status: String,
    pub warnings: Vec<String>,
    pub created_at: String,
}

#[derive(sqlx::FromRow)]
pub struct MoveHistoryRow {
    id: String,
    user_id: String,
    song_id: String,
    new_song_id: Option<String>,
    title: String,
    artist: String,
    album: String,
    source_folder_id: String,
    source_path: String,
    target_folder_id: String,
    target_path: String,
    origin: String,
    client: Option<String>,
    status: String,
    warnings: String,
    created_at: String,
}

impl MoveHistoryRow {
    fn into_entry(self) -> MoveHistoryEntry {
        let warnings = serde_json::from_str(&self.warnings).unwrap_or_default();
        MoveHistoryEntry {
            id: self.id,
            user_id: self.user_id,
            song_id: self.song_id,
            new_song_id: self.new_song_id,
            title: self.title,
            artist: self.artist,
            album: self.album,
            source_folder_id: self.source_folder_id,
            source_path: self.source_path,
            target_folder_id: self.target_folder_id,
            target_path: self.target_path,
            origin: self.origin,
            client: self.client,
            status: self.status,
            warnings,
            created_at: self.created_at,
        }
    }

    pub async fn insert_in(db: &SqlitePool, record: &MoveRecord) -> Result<(), String> {
        let warnings = serde_json::to_string(&record.warnings).map_err(|e| e.to_string())?;
        sqlx::query(
            "INSERT INTO move_history \
             (id, user_id, song_id, new_song_id, title, artist, album, \
              source_folder_id, source_path, target_folder_id, target_path, \
              origin, client, status, warnings) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.user_id)
        .bind(&record.song_id)
        .bind(&record.new_song_id)
        .bind(&record.title)
        .bind(&record.artist)
        .bind(&record.album)
        .bind(&record.source_folder_id)
        .bind(&record.source_path)
        .bind(&record.target_folder_id)
        .bind(&record.target_path)
        .bind(&record.origin)
        .bind(&record.client)
        .bind(&record.status)
        .bind(warnings)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn page_in(
        db: &SqlitePool,
        user_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<MoveHistoryEntry>, String> {
        let rows = sqlx::query_as::<_, MoveHistoryRow>(
            "SELECT id, user_id, song_id, new_song_id, title, artist, album, \
             source_folder_id, source_path, target_folder_id, target_path, \
             origin, client, status, warnings, created_at \
             FROM move_history \
             WHERE user_id = ? \
             ORDER BY created_at DESC, rowid DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(MoveHistoryRow::into_entry).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn move_history_inserts_and_pages_newest_first() {
        let db = test_db::migrated().await;
        let rec = |id: &str, song: &str| MoveRecord {
            id: id.into(),
            user_id: "alice".into(),
            song_id: song.into(),
            new_song_id: Some(song.into()),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            source_folder_id: "f-personal".into(),
            source_path: "/music/alice/Artist/Album/01.flac".into(),
            target_folder_id: "f-common".into(),
            target_path: "/music/common/Artist/Album/01.flac".into(),
            origin: "mcp".into(),
            client: Some("claude.ai".into()),
            status: "moved".into(),
            warnings: vec!["split album".into()],
        };
        MoveHistoryRow::insert_in(&db, &rec("m1", "s1")).await.unwrap();
        MoveHistoryRow::insert_in(&db, &rec("m2", "s2")).await.unwrap();
        let page = MoveHistoryRow::page_in(&db, "alice", 0, 10).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, "m2");
        assert_eq!(page[1].id, "m1");
        assert_eq!(page[0].warnings, vec!["split album"]);
        assert!(MoveHistoryRow::page_in(&db, "bob", 0, 10).await.unwrap().is_empty());
    }
}
