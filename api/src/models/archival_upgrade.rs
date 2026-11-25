#[cfg(feature = "server")]
use shared::download::{ArchivalUpgradeJob, ArchivalUpgradeStatus};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FastAcquisitionRow {
    pub id: String,
    pub user_id: String,
    pub folder_id: String,
    pub navidrome_song_id: Option<String>,
    pub discovery_track_id: Option<String>,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub recording_mbid: Option<String>,
    pub backend_id: String,
    pub codec: String,
    pub is_lossy: i64,
    pub created_at: String,
}

#[cfg(feature = "server")]
pub struct NewFastAcquisition<'a> {
    pub user_id: &'a str,
    pub folder_id: &'a str,
    pub navidrome_song_id: Option<&'a str>,
    pub discovery_track_id: Option<&'a str>,
    pub artist: &'a str,
    pub title: &'a str,
    pub album: Option<&'a str>,
    pub recording_mbid: Option<&'a str>,
    pub backend_id: &'a str,
    pub codec: &'a str,
    pub is_lossy: bool,
}

#[cfg(feature = "server")]
impl FastAcquisitionRow {
    pub async fn insert_in(
        db: &SqlitePool,
        item: NewFastAcquisition<'_>,
    ) -> Result<Self, String> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO fast_acquisitions \
             (id, user_id, folder_id, navidrome_song_id, discovery_track_id, artist, title, album, \
              recording_mbid, backend_id, codec, is_lossy) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(item.user_id)
        .bind(item.folder_id)
        .bind(item.navidrome_song_id)
        .bind(item.discovery_track_id)
        .bind(item.artist)
        .bind(item.title)
        .bind(item.album)
        .bind(item.recording_mbid)
        .bind(item.backend_id)
        .bind(item.codec)
        .bind(if item.is_lossy { 1_i64 } else { 0_i64 })
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;

        sqlx::query_as::<_, Self>("SELECT * FROM fast_acquisitions WHERE id = ?")
            .bind(&id)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn find_matching_in(
        db: &SqlitePool,
        user_id: &str,
        navidrome_song_id: Option<&str>,
        discovery_track_id: Option<&str>,
        artist: &str,
        title: &str,
    ) -> Result<Option<Self>, String> {
        if let Some(song_id) = navidrome_song_id.filter(|s| !s.is_empty()) {
            if let Some(row) = sqlx::query_as::<_, Self>(
                "SELECT * FROM fast_acquisitions \
                 WHERE user_id = ? AND navidrome_song_id = ? \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id)
            .bind(song_id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            {
                return Ok(Some(row));
            }
        }

        if let Some(track_id) = discovery_track_id.filter(|s| !s.is_empty()) {
            if let Some(row) = sqlx::query_as::<_, Self>(
                "SELECT * FROM fast_acquisitions \
                 WHERE user_id = ? AND discovery_track_id = ? \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id)
            .bind(track_id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            {
                return Ok(Some(row));
            }
        }

        sqlx::query_as::<_, Self>(
            "SELECT * FROM fast_acquisitions \
             WHERE user_id = ? AND lower(artist) = lower(?) AND lower(title) = lower(?) \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(artist)
        .bind(title)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArchivalUpgradeRow {
    pub id: String,
    pub user_id: String,
    pub folder_id: String,
    pub navidrome_song_id: Option<String>,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub recording_mbid: Option<String>,
    pub original_codec: String,
    pub original_backend: String,
    pub trigger_reason: String,
    pub status: String,
    pub job_id: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(feature = "server")]
impl From<ArchivalUpgradeRow> for ArchivalUpgradeJob {
    fn from(row: ArchivalUpgradeRow) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            folder_id: row.folder_id,
            navidrome_song_id: row.navidrome_song_id,
            artist: row.artist,
            title: row.title,
            album: row.album,
            recording_mbid: row.recording_mbid,
            original_codec: row.original_codec,
            original_backend: row.original_backend,
            trigger_reason: row.trigger_reason,
            status: ArchivalUpgradeStatus::parse(&row.status),
            job_id: row.job_id,
            attempts: row.attempts.max(0) as u32,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[cfg(feature = "server")]
pub struct NewArchivalUpgrade<'a> {
    pub user_id: &'a str,
    pub folder_id: &'a str,
    pub navidrome_song_id: Option<&'a str>,
    pub artist: &'a str,
    pub title: &'a str,
    pub album: Option<&'a str>,
    pub recording_mbid: Option<&'a str>,
    pub original_codec: &'a str,
    pub original_backend: &'a str,
    pub trigger_reason: &'a str,
}

#[cfg(feature = "server")]
impl ArchivalUpgradeRow {
    pub async fn find_active_or_completed_in(
        db: &SqlitePool,
        user_id: &str,
        navidrome_song_id: Option<&str>,
        artist: &str,
        title: &str,
    ) -> Result<Option<Self>, String> {
        if let Some(song_id) = navidrome_song_id.filter(|s| !s.is_empty()) {
            if let Some(row) = sqlx::query_as::<_, Self>(
                "SELECT * FROM archival_upgrade_jobs \
                 WHERE user_id = ? AND navidrome_song_id = ? \
                   AND status IN ('queued', 'searching', 'downloading', 'completed') \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id)
            .bind(song_id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            {
                return Ok(Some(row));
            }
        }

        sqlx::query_as::<_, Self>(
            "SELECT * FROM archival_upgrade_jobs \
             WHERE user_id = ? AND lower(artist) = lower(?) AND lower(title) = lower(?) \
               AND status IN ('queued', 'searching', 'downloading', 'completed') \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(artist)
        .bind(title)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn insert_in(
        db: &SqlitePool,
        item: NewArchivalUpgrade<'_>,
    ) -> Result<Self, String> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO archival_upgrade_jobs \
             (id, user_id, folder_id, navidrome_song_id, artist, title, album, recording_mbid, \
              original_codec, original_backend, trigger_reason, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued')",
        )
        .bind(&id)
        .bind(item.user_id)
        .bind(item.folder_id)
        .bind(item.navidrome_song_id)
        .bind(item.artist)
        .bind(item.title)
        .bind(item.album)
        .bind(item.recording_mbid)
        .bind(item.original_codec)
        .bind(item.original_backend)
        .bind(item.trigger_reason)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;

        sqlx::query_as::<_, Self>("SELECT * FROM archival_upgrade_jobs WHERE id = ?")
            .bind(&id)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn list_for_user_in(
        db: &SqlitePool,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT * FROM archival_upgrade_jobs \
             WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn next_queued_in(
        db: &SqlitePool,
        max_attempts: i64,
    ) -> Result<Option<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT * FROM archival_upgrade_jobs \
             WHERE status = 'queued' AND attempts < ? \
             ORDER BY created_at ASC, rowid ASC LIMIT 1",
        )
        .bind(max_attempts)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn mark_status_in(
        db: &SqlitePool,
        id: &str,
        status: ArchivalUpgradeStatus,
        job_id: Option<&str>,
        attempts_delta: i64,
        last_error: Option<&str>,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE archival_upgrade_jobs \
             SET status = ?, \
                 job_id = COALESCE(?, job_id), \
                 attempts = attempts + ?, \
                 last_error = ?, \
                 updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(job_id)
        .bind(attempts_delta)
        .bind(last_error)
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn get_by_id_in(db: &SqlitePool, id: &str) -> Result<Option<Self>, String> {
        sqlx::query_as::<_, Self>("SELECT * FROM archival_upgrade_jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())
    }
}
