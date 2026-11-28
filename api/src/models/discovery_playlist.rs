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
    #[serde(default)]
    pub exclude_from_core_taste: bool,
    #[serde(default)]
    pub reason_json: Option<String>,
    #[serde(default)]
    pub culture_bucket: Option<String>,
    #[serde(default)]
    pub pack_slug: Option<String>,
}

impl From<DiscoveryTrackRow> for DiscoveryTrack {
    fn from(row: DiscoveryTrackRow) -> Self {
        let reason = row
            .reason_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .map(Box::new);
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
            exclude_from_core_taste: row.exclude_from_core_taste,
            reason,
            culture_bucket: row.culture_bucket,
            pack_slug: row.pack_slug,
        }
    }
}

/// Optional contextual/seasonal metadata persisted alongside a staged discovery track.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Default)]
pub struct DiscoveryTrackContext<'a> {
    pub exclude_from_core_taste: bool,
    pub reason_json: Option<&'a str>,
    pub culture_bucket: Option<&'a str>,
    pub pack_slug: Option<&'a str>,
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
        Self::create_with_context(
            song_id,
            title,
            artist,
            album,
            path,
            folder_id,
            profile,
            DiscoveryTrackContext::default(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_context(
        song_id: Option<&str>,
        title: &str,
        artist: &str,
        album: &str,
        path: &str,
        folder_id: &str,
        profile: &str,
        ctx: DiscoveryTrackContext<'_>,
    ) -> Result<DiscoveryTrack, String> {
        Self::create_with_context_in(
            &DB, song_id, title, artist, album, path, folder_id, profile, ctx,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_with_context_in(
        db: &sqlx::SqlitePool,
        song_id: Option<&str>,
        title: &str,
        artist: &str,
        album: &str,
        path: &str,
        folder_id: &str,
        profile: &str,
        ctx: DiscoveryTrackContext<'_>,
    ) -> Result<DiscoveryTrack, String> {
        let id = Uuid::new_v4().to_string();
        let row = sqlx::query_as::<_, DiscoveryTrackRow>(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, \
                 exclude_from_core_taste, reason_json, culture_bucket, pack_slug) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(song_id)
        .bind(title)
        .bind(artist)
        .bind(album)
        .bind(path)
        .bind(folder_id)
        .bind(profile)
        .bind(ctx.exclude_from_core_taste)
        .bind(ctx.reason_json)
        .bind(ctx.culture_bucket)
        .bind(ctx.pack_slug)
        .fetch_one(db)
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

    /// The Pending tracks of `folder_id`, oldest first.
    pub(crate) async fn get_pending_by_folder_in(
        db: &sqlx::SqlitePool,
        folder_id: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks WHERE folder_id = ? AND status = ? ORDER BY created_at",
        )
        .bind(folder_id)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Pending tracks in every folder of `user_id`.
    pub(crate) async fn pending_for_user_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT t.* FROM discovery_tracks t JOIN folders f ON f.id = t.folder_id \
             WHERE f.user_id = ? AND t.status = ? ORDER BY t.created_at",
        )
        .bind(user_id)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Discovery tracks for `user_id` flagged `exclude_from_core_taste = 1` (e.g. seasonal packs)
    /// so core recommendation runs keep seasonal listening isolated from the user's taste profile.
    pub(crate) async fn excluded_from_core_taste_for_user_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT t.* FROM discovery_tracks t JOIN folders f ON f.id = t.folder_id \
             WHERE f.user_id = ? AND t.exclude_from_core_taste = 1",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_pending_by_folder_and_profile(
        folder_id: &str,
        profile: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        Self::get_pending_by_folder_and_profile_in(&DB, folder_id, profile).await
    }

    /// The Pending rolling tracks of `profile` in `folder_id` (excluding seasonal packs), oldest first.
    pub(crate) async fn get_pending_by_folder_and_profile_in(
        db: &sqlx::SqlitePool,
        folder_id: &str,
        profile: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks \
             WHERE folder_id = ? AND profile = ? AND pack_slug IS NULL AND status = ? \
             ORDER BY created_at",
        )
        .bind(folder_id)
        .bind(profile)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Pending tracks staged for a specific seasonal pack (`pack_slug`) in `folder_id`, oldest first.
    pub(crate) async fn get_pending_by_folder_and_pack_in(
        db: &sqlx::SqlitePool,
        folder_id: &str,
        pack_slug: &str,
    ) -> Result<Vec<DiscoveryTrack>, String> {
        let rows = sqlx::query_as::<_, DiscoveryTrackRow>(
            "SELECT * FROM discovery_tracks \
             WHERE folder_id = ? AND pack_slug = ? AND status = ? \
             ORDER BY created_at",
        )
        .bind(folder_id)
        .bind(pack_slug)
        .bind(DiscoveryStatus::Pending.to_string())
        .fetch_all(db)
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
        Self::get_by_id_in(&DB, id).await
    }

    pub(crate) async fn get_by_id_in(
        db: &sqlx::SqlitePool,
        id: &str,
    ) -> Result<Option<DiscoveryTrack>, String> {
        let row =
            sqlx::query_as::<_, DiscoveryTrackRow>("SELECT * FROM discovery_tracks WHERE id = ?")
                .bind(id)
                .fetch_optional(db)
                .await
                .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub(crate) async fn update_status_in(
        db: &sqlx::SqlitePool,
        id: &str,
        status: &DiscoveryStatus,
    ) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET status = ? WHERE id = ?")
            .bind(status.to_string())
            .bind(id)
            .execute(db)
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
        Self::update_song_id_in(&DB, id, song_id).await
    }

    pub(crate) async fn clear_song_id_in(db: &sqlx::SqlitePool, id: &str) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET song_id = NULL WHERE id = ?")
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) async fn update_song_id_in(
        db: &sqlx::SqlitePool,
        id: &str,
        song_id: &str,
    ) -> Result<(), String> {
        sqlx::query("UPDATE discovery_tracks SET song_id = ? WHERE id = ?")
            .bind(song_id)
            .bind(id)
            .execute(db)
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
    pub(crate) async fn try_set_promoting_in(
        db: &sqlx::SqlitePool,
        id: &str,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE discovery_tracks SET status = 'Promoting' WHERE id = ? AND status = 'Pending'",
        )
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected() == 1)
    }

    /// Marks the track Removing if it is Pending (D16): true when this call changed it, false
    /// when another call holds or ended it.
    pub(crate) async fn try_set_removing_in(
        db: &sqlx::SqlitePool,
        id: &str,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE discovery_tracks SET status = 'Removing' WHERE id = ? AND status = 'Pending'",
        )
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected() == 1)
    }

    /// Puts every track claimed for a promote or a removal (Promoting, Removing) back to
    /// Pending. Called once at startup, for a server that stopped while one ran: the expiry or
    /// a later call then deals with its file.
    pub(crate) async fn reset_stale_claims_in(db: &sqlx::SqlitePool) -> Result<u64, String> {
        let result = sqlx::query(
            "UPDATE discovery_tracks SET status = 'Pending' WHERE status IN ('Promoting', 'Removing')",
        )
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected())
    }
}
