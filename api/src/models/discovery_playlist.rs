use serde::{Deserialize, Serialize};
use shared::navidrome::{DiscoveryStatus, DiscoveryTrack};

#[cfg(feature = "server")]
use crate::db::DB;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct DiscoveryTrackRow {
    pub id: String,
    pub song_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub folder_id: String,
    pub profile: String,
    pub rating: Option<i32>,
    pub status: String,
    pub created_at: String,
}

impl From<DiscoveryTrackRow> for DiscoveryTrack {
    fn from(row: DiscoveryTrackRow) -> Self {
        DiscoveryTrack {
            id: row.id,
            song_id: row.song_id,
            title: row.title,
            artist: row.artist,
            album: row.album,
            path: row.path,
            folder_id: row.folder_id,
            profile: row.profile,
            rating: row.rating.map(|r| r as u8),
            status: row.status.parse().unwrap_or(DiscoveryStatus::Pending),
            created_at: row.created_at,
        }
    }
}

#[cfg(feature = "server")]
impl DiscoveryTrackRow {
    pub async fn create(
        song_id: Option<&str>,
        title: &str,
        artist: &str,
        album: &str,
        path: &str,
        folder_id: &str,
        profile: &str,
    ) -> Result<DiscoveryTrack, String> {
        let id = Uuid::new_v4().to_string();
        let row = sqlx::query_as::<_, DiscoveryTrackRow>(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *"
        )
        .bind(&id)
        .bind(song_id)
        .bind(title)
        .bind(artist)
        .bind(album)
        .bind(path)
        .bind(folder_id)
        .bind(profile)
        .fetch_one(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.into())
    }

    pub async fn get_by_folder(folder_id: &str) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks WHERE folder_id = ? ORDER BY created_at DESC",
        )
        .bind(folder_id)
        .fetch_all(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_pending_by_folder(folder_id: &str) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks WHERE folder_id = ? AND status = ? ORDER BY created_at",
        )
        .bind(folder_id)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_pending_by_folder_and_profile(
        folder_id: &str,
        profile: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks WHERE folder_id = ? AND profile = ? AND status = ? ORDER BY created_at",
        )
        .bind(folder_id)
        .bind(profile)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_by_path(path: &str) -> Result<Option<DiscoveryTrack>, String> {
        let row =
            sqlx::query_as::<_, DiscoveryTrackRow>("SELECT * FROM discovery_tracks WHERE path = ?")
                .bind(path)
                .fetch_optional(&*DB)
                .await
                .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub async fn get_by_id(id: &str) -> Result<Option<DiscoveryTrack>, String> {
        let row =
            sqlx::query_as::<_, DiscoveryTrackRow>("SELECT * FROM discovery_tracks WHERE id = ?")
                .bind(id)
                .fetch_optional(&*DB)
                .await
                .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub async fn update_status(id: &str, status: &DiscoveryStatus) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET status = ? WHERE id = ?")
            .bind(status.to_string())
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn update_rating(id: &str, rating: Option<u8>) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET rating = ? WHERE id = ?")
            .bind(rating.map(|r| r as i32))
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn update_song_id(id: &str, song_id: &str) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET song_id = ? WHERE id = ?")
            .bind(song_id)
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM discovery_tracks WHERE id = ?")
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Atomically set status to Promoting if currently Pending.
    /// Returns Ok(true) if the CAS succeeded (this caller owns the promote),
    /// Ok(false) if another caller already changed the status.
    pub async fn try_set_promoting(id: &str) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE discovery_tracks SET status = 'Promoting' WHERE id = ? AND status = 'Pending'",
        )
        .bind(id)
        .execute(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected() == 1)
    }

    /// Reset any tracks stuck in Promoting back to Pending.
    /// Called at automation loop start to handle server crashes mid-promote.
    pub async fn reset_stale_promoting() -> Result<u64, String> {
        let result = sqlx::query(
            "UPDATE discovery_tracks SET status = 'Pending' WHERE status = 'Promoting'",
        )
        .execute(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected())
    }
}
