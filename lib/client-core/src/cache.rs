use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::now_unix;
use crate::error::{CoreError, CoreResult};
use crate::models::{
    AudioQualityProfile, CachePolicy, CacheStats, NavidromeCredentials, OfflineTrackEntry,
    PinRecord, PinTarget, PlaybackSource,
};
use crate::subsonic::{
    is_subsonic_error_content_type, parse_subsonic_error_bytes, SubsonicClient,
};

/// Manages offline audio downloads, profile-aware streaming URLs, explicit pinning,
/// disk-budget LRU eviction (never evicting pinned tracks), and local cover-art caching.
#[derive(Clone)]
pub struct CacheManager {
    pool: SqlitePool,
    http: Client,
    cache_dir: PathBuf,
    cover_art_dir: PathBuf,
    policy: Arc<RwLock<CachePolicy>>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
}

impl CacheManager {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        cache_dir: PathBuf,
        cover_art_dir: PathBuf,
        policy: CachePolicy,
        navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    ) -> Self {
        Self {
            pool,
            http,
            cache_dir,
            cover_art_dir,
            policy: Arc::new(RwLock::new(policy)),
            navidrome,
        }
    }

    pub async fn policy(&self) -> CachePolicy {
        self.policy.read().await.clone()
    }

    pub async fn set_policy(&self, policy: CachePolicy) -> CoreResult<()> {
        *self.policy.write().await = policy;
        self.enforce_budget().await?;
        Ok(())
    }

    /// Resolves whether a track should play from local disk cache or stream over HTTP.
    /// Touches `last_accessed_at` on cache hit so LRU reflects active listening.
    pub async fn resolve_playback_source(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
        offline_only: bool,
    ) -> CoreResult<PlaybackSource> {
        let cached = sqlx::query_as::<_, OfflineTrackEntry>(
            "SELECT * FROM offline_downloads WHERE track_id = ?",
        )
        .bind(track_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(entry) = cached {
            if tokio::fs::try_exists(&entry.local_path)
                .await
                .unwrap_or(false)
            {
                sqlx::query("UPDATE offline_downloads SET last_accessed_at = ? WHERE track_id = ?")
                    .bind(now_unix())
                    .bind(track_id)
                    .execute(&self.pool)
                    .await?;
                return Ok(PlaybackSource::LocalFile {
                    path: entry.local_path,
                    format: entry.format,
                    bit_rate: entry.bit_rate.map(|b| b.max(0) as u32),
                });
            }
            // Stale DB row whose backing file disappeared externally: prune row.
            sqlx::query("DELETE FROM offline_downloads WHERE track_id = ?")
                .bind(track_id)
                .execute(&self.pool)
                .await?;
        }

        if offline_only {
            return Err(CoreError::Offline(format!("track {track_id} not cached")));
        }

        let creds = self
            .navidrome
            .read()
            .await
            .clone()
            .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
        let subsonic = SubsonicClient::new(self.http.clone(), creds);
        let url = subsonic.stream_url(track_id, profile)?;
        Ok(PlaybackSource::RemoteStream {
            url,
            format: profile.format_label().to_string(),
            bit_rate: profile.max_bit_rate(),
        })
    }

    /// Pins a track, album, artist, playlist, or dynamic collection (Starred / Weekly Radar / Seasonal).
    /// Returns the list of track IDs in this pin target that are not yet cached locally.
    pub async fn pin(&self, target: &PinTarget) -> CoreResult<Vec<String>> {
        let (kind, id) = target.as_db_pair();
        sqlx::query(
            r#"
            INSERT INTO offline_pins (target_kind, target_id, pinned_at)
            VALUES (?, ?, ?)
            ON CONFLICT(target_kind, target_id) DO UPDATE SET pinned_at = excluded.pinned_at
            "#,
        )
        .bind(kind)
        .bind(&id)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;

        self.recompute_pin_flags().await?;
        let target_tracks = self.resolve_pin_target_tracks(target).await?;
        let cached: HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT track_id FROM offline_downloads")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .collect();

        Ok(target_tracks
            .into_iter()
            .filter(|tid| !cached.contains(tid))
            .collect())
    }

    /// Unpins a target and recomputes `pinned` flags across all remaining active pins.
    /// Enforces disk budget afterwards in case newly unpinned files exceed `max_bytes`.
    pub async fn unpin(&self, target: &PinTarget) -> CoreResult<usize> {
        let (kind, id) = target.as_db_pair();
        sqlx::query("DELETE FROM offline_pins WHERE target_kind = ? AND target_id = ?")
            .bind(kind)
            .bind(id)
            .execute(&self.pool)
            .await?;

        self.recompute_pin_flags().await?;
        self.enforce_budget().await
    }

    /// Downloads a track according to the active audio quality profile (`FLAC Direct` or `Opus 160`).
    /// Rejects HTTP 200 Subsonic JSON/XML/plain-text error envelopes and empty payloads so error
    /// responses are never persisted as audio files.
    pub async fn download_track(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
        pinned: bool,
        pin_source: Option<&str>,
    ) -> CoreResult<OfflineTrackEntry> {
        let creds = self
            .navidrome
            .read()
            .await
            .clone()
            .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
        let subsonic = SubsonicClient::new(self.http.clone(), creds);
        let url = subsonic.download_url(track_id, profile)?;

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        if is_subsonic_error_content_type(content_type.as_deref()) {
            let bytes = resp.bytes().await?;
            return Err(parse_subsonic_error_bytes(&bytes, content_type.as_deref()));
        }

        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let safe_id = encode_filename_key(track_id);
        let safe_ext = sanitize_extension(profile.file_extension());
        let final_path = self.cache_dir.join(format!("{safe_id}.{safe_ext}"));
        let part_path = self
            .cache_dir
            .join(format!("{safe_id}.{}.part", Uuid::new_v4()));

        let write_result: CoreResult<u64> = async {
            let mut file = tokio::fs::File::create(&part_path).await?;
            let mut stream = resp.bytes_stream();
            let mut size_bytes: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let bytes = chunk?;
                file.write_all(&bytes).await?;
                size_bytes = size_bytes.saturating_add(bytes.len() as u64);
            }
            file.flush().await?;
            drop(file);
            if size_bytes == 0 {
                return Err(parse_subsonic_error_bytes(&[], content_type.as_deref()));
            }
            tokio::fs::rename(&part_path, &final_path).await?;
            Ok(size_bytes)
        }
        .await;

        let size_bytes = match write_result {
            Ok(n) => n,
            Err(err) => {
                let _ = tokio::fs::remove_file(&part_path).await;
                return Err(err);
            }
        };

        self.register_cached_file(
            track_id,
            &final_path,
            profile.format_label(),
            profile.max_bit_rate(),
            size_bytes,
            pinned,
            pin_source,
            now_unix(),
        )
        .await
    }

    /// Registers a locally written audio file in `offline_downloads` and enforces the LRU budget
    /// (excluding the newly registered `track_id` from eviction in this call).
    #[allow(clippy::too_many_arguments)]
    pub async fn register_cached_file(
        &self,
        track_id: &str,
        local_path: &Path,
        format: &str,
        bit_rate: Option<u32>,
        size_bytes: u64,
        pinned: bool,
        pin_source: Option<&str>,
        accessed_at: i64,
    ) -> CoreResult<OfflineTrackEntry> {
        let prev_path: Option<String> =
            sqlx::query_scalar("SELECT local_path FROM offline_downloads WHERE track_id = ?")
                .bind(track_id)
                .fetch_optional(&self.pool)
                .await?;

        let effective_pinned = pinned || self.is_track_pinned_by_rules(track_id).await?;
        let new_path_str = local_path.display().to_string();
        let size_i64 = i64::try_from(size_bytes).unwrap_or(i64::MAX);

        let entry = OfflineTrackEntry {
            track_id: track_id.to_string(),
            local_path: new_path_str.clone(),
            format: format.to_string(),
            bit_rate: bit_rate.map(i64::from),
            size_bytes: size_i64,
            pinned: effective_pinned,
            pin_source: pin_source.map(ToString::to_string),
            downloaded_at: accessed_at,
            last_accessed_at: accessed_at,
        };

        sqlx::query(
            r#"
            INSERT INTO offline_downloads (
                track_id, local_path, format, bit_rate, size_bytes,
                pinned, pin_source, downloaded_at, last_accessed_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(track_id) DO UPDATE SET
                local_path = excluded.local_path,
                format = excluded.format,
                bit_rate = excluded.bit_rate,
                size_bytes = excluded.size_bytes,
                pinned = CASE WHEN offline_downloads.pinned = 1 OR excluded.pinned = 1 THEN 1 ELSE 0 END,
                pin_source = COALESCE(excluded.pin_source, offline_downloads.pin_source),
                last_accessed_at = excluded.last_accessed_at
            "#,
        )
        .bind(&entry.track_id)
        .bind(&entry.local_path)
        .bind(&entry.format)
        .bind(entry.bit_rate)
        .bind(entry.size_bytes)
        .bind(entry.pinned)
        .bind(&entry.pin_source)
        .bind(entry.downloaded_at)
        .bind(entry.last_accessed_at)
        .execute(&self.pool)
        .await?;

        if let Some(old_path) = prev_path {
            if old_path != new_path_str {
                let _ = tokio::fs::remove_file(&old_path).await;
            }
        }

        self.enforce_budget_excluding(Some(track_id)).await?;
        Ok(entry)
    }

    /// Evicts unpinned audio entries in strict LRU order (`last_accessed_at ASC`) until total
    /// audio cache bytes are within `policy.max_bytes`. Pinned items are never evicted.
    pub async fn enforce_budget(&self) -> CoreResult<usize> {
        self.enforce_budget_excluding(None).await
    }

    async fn enforce_budget_excluding(
        &self,
        protected_track_id: Option<&str>,
    ) -> CoreResult<usize> {
        let max_bytes = i64::try_from(self.policy.read().await.max_bytes).unwrap_or(i64::MAX);
        let mut total: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM offline_downloads")
                .fetch_one(&self.pool)
                .await?;

        if total <= max_bytes {
            return Ok(0);
        }

        let candidates: Vec<(String, String, i64)> = sqlx::query_as(
            r#"
            SELECT track_id, local_path, size_bytes
            FROM offline_downloads
            WHERE pinned = 0
            ORDER BY last_accessed_at ASC, downloaded_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut evicted = 0usize;
        for (track_id, local_path, size_bytes) in candidates {
            if total <= max_bytes {
                break;
            }
            if protected_track_id == Some(track_id.as_str()) {
                continue;
            }
            let _ = tokio::fs::remove_file(&local_path).await;
            sqlx::query("DELETE FROM offline_downloads WHERE track_id = ?")
                .bind(&track_id)
                .execute(&self.pool)
                .await?;
            total = (total - size_bytes).max(0);
            evicted += 1;
        }

        Ok(evicted)
    }

    /// Prefetches the next `policy.prefetch_queue_count` uncached tracks in the active queue.
    pub async fn prefetch_queue(
        &self,
        queue_track_ids: &[String],
        current_index: usize,
        profile: &AudioQualityProfile,
        is_wifi: bool,
    ) -> CoreResult<Vec<String>> {
        let policy = self.policy.read().await.clone();
        if policy.wifi_only_auto_cache && !is_wifi {
            return Ok(Vec::new());
        }
        let start = current_index.saturating_add(1);
        let end = start
            .saturating_add(policy.prefetch_queue_count)
            .min(queue_track_ids.len());
        if start >= end {
            return Ok(Vec::new());
        }

        let mut downloaded = Vec::new();
        for track_id in &queue_track_ids[start..end] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(1) FROM offline_downloads WHERE track_id = ?",
            )
            .bind(track_id)
            .fetch_one(&self.pool)
            .await?;
            if exists > 0 {
                continue;
            }
            self.download_track(track_id, profile, false, Some("prefetch"))
                .await?;
            downloaded.push(track_id.clone());
        }
        Ok(downloaded)
    }

    /// Applies auto-pinning rules (`Starred`, `WeeklyRadar`, active `SeasonalPack`) when allowed by network state.
    pub async fn run_auto_cache_rules(
        &self,
        profile: &AudioQualityProfile,
        is_wifi: bool,
    ) -> CoreResult<Vec<String>> {
        let policy = self.policy.read().await.clone();
        if policy.wifi_only_auto_cache && !is_wifi {
            return Ok(Vec::new());
        }

        let mut to_download = Vec::new();
        let mut seen = HashSet::new();

        if policy.auto_pin_starred {
            for tid in self.pin(&PinTarget::Starred).await? {
                if seen.insert(tid.clone()) {
                    to_download.push((tid, "starred".to_string()));
                }
            }
        }
        if policy.auto_pin_weekly_radar {
            for tid in self.pin(&PinTarget::WeeklyRadar).await? {
                if seen.insert(tid.clone()) {
                    to_download.push((tid, "weekly_radar".to_string()));
                }
            }
        }
        if policy.auto_pin_active_seasonal {
            let slugs: Vec<String> = sqlx::query_scalar(
                "SELECT slug FROM playlists WHERE kind = 'seasonal' AND slug IS NOT NULL",
            )
            .fetch_all(&self.pool)
            .await?;
            for slug in slugs {
                for tid in self.pin(&PinTarget::SeasonalPack(slug.clone())).await? {
                    if seen.insert(tid.clone()) {
                        to_download.push((tid, format!("seasonal:{slug}")));
                    }
                }
            }
        }

        let mut completed = Vec::new();
        for (track_id, source) in to_download {
            self.download_track(&track_id, profile, true, Some(&source))
                .await?;
            completed.push(track_id);
        }
        Ok(completed)
    }

    /// Pure read-only local cover-art path lookup with zero network I/O and zero SQLite writes
    /// (safe for Android Auto `ContentProvider` Binder threads even while `sync_now` holds write locks).
    /// Verifies the file exists as a regular file canonically contained within `cover_art_dir`.
    pub async fn cover_art_local_path(&self, cover_art_id: &str) -> CoreResult<Option<PathBuf>> {
        let path = sqlx::query_scalar::<_, String>(
            "SELECT local_path FROM cover_art_cache WHERE cover_art_id = ?",
        )
        .bind(cover_art_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(p) = path else {
            return Ok(None);
        };

        let pb = PathBuf::from(&p);
        let is_file = tokio::fs::metadata(&pb)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_file {
            return Ok(None);
        }

        let canonical_base = tokio::fs::canonicalize(&self.cover_art_dir).await.ok();
        let canonical_file = tokio::fs::canonicalize(&pb).await.ok();
        match (canonical_base, canonical_file) {
            (Some(base), Some(file)) if file.starts_with(&base) => Ok(Some(pb)),
            _ => Ok(None),
        }
    }

    /// Returns cached cover-art path, or fetches and persists it on cache miss (unless `offline_only`).
    /// Rejects Subsonic HTTP 200 JSON/XML/plain-text error envelopes and empty bodies.
    pub async fn get_or_fetch_cover_art(
        &self,
        cover_art_id: &str,
        size: Option<u32>,
        offline_only: bool,
    ) -> CoreResult<PathBuf> {
        let (path, _) = self
            .get_or_fetch_cover_art_with_status(cover_art_id, size, offline_only)
            .await?;
        Ok(path)
    }

    /// Same as [`Self::get_or_fetch_cover_art`], returning `(path, newly_fetched)` so callers can
    /// avoid emitting cache-change notifications on cache hits.
    pub async fn get_or_fetch_cover_art_with_status(
        &self,
        cover_art_id: &str,
        size: Option<u32>,
        offline_only: bool,
    ) -> CoreResult<(PathBuf, bool)> {
        if let Some(local) = self.cover_art_local_path(cover_art_id).await? {
            return Ok((local, false));
        }
        if offline_only {
            return Err(CoreError::Offline(format!(
                "cover art {cover_art_id} not cached"
            )));
        }
        let creds = self
            .navidrome
            .read()
            .await
            .clone()
            .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
        let subsonic = SubsonicClient::new(self.http.clone(), creds);
        let url = subsonic.cover_art_url(cover_art_id, size)?;
        let (bytes, content_type) = subsonic.fetch_bytes(&url).await?;

        if bytes.is_empty() || is_subsonic_error_content_type(content_type.as_deref()) {
            return Err(parse_subsonic_error_bytes(&bytes, content_type.as_deref()));
        }

        let content_type = content_type.unwrap_or_else(|| "image/jpeg".to_string());
        let ext = if content_type.contains("png") {
            "png"
        } else if content_type.contains("webp") {
            "webp"
        } else {
            "jpg"
        };

        let prev_path: Option<String> = sqlx::query_scalar(
            "SELECT local_path FROM cover_art_cache WHERE cover_art_id = ?",
        )
        .bind(cover_art_id)
        .fetch_optional(&self.pool)
        .await?;

        tokio::fs::create_dir_all(&self.cover_art_dir).await?;
        let safe_id = encode_filename_key(cover_art_id);
        let final_path = self.cover_art_dir.join(format!("{safe_id}.{ext}"));
        let part_path = self
            .cover_art_dir
            .join(format!("{safe_id}.{}.part", Uuid::new_v4()));

        let write_res: CoreResult<()> = async {
            tokio::fs::write(&part_path, &bytes).await?;
            tokio::fs::rename(&part_path, &final_path).await?;
            Ok(())
        }
        .await;
        if let Err(err) = write_res {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(err);
        }

        let now = now_unix();
        let new_path_str = final_path.display().to_string();
        let size_i64 = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        sqlx::query(
            r#"
            INSERT INTO cover_art_cache (cover_art_id, local_path, content_type, size_bytes, downloaded_at, last_accessed_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(cover_art_id) DO UPDATE SET
                local_path = excluded.local_path,
                content_type = excluded.content_type,
                size_bytes = excluded.size_bytes,
                last_accessed_at = excluded.last_accessed_at
            "#,
        )
        .bind(cover_art_id)
        .bind(&new_path_str)
        .bind(&content_type)
        .bind(size_i64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        if let Some(old_path) = prev_path {
            if old_path != new_path_str {
                let _ = tokio::fs::remove_file(&old_path).await;
            }
        }

        self.enforce_cover_art_budget_excluding(Some(cover_art_id))
            .await?;
        Ok((final_path, true))
    }

    async fn enforce_cover_art_budget_excluding(
        &self,
        protected_art_id: Option<&str>,
    ) -> CoreResult<usize> {
        let max_bytes =
            i64::try_from(self.policy.read().await.cover_art_max_bytes).unwrap_or(i64::MAX);
        let mut total: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM cover_art_cache")
                .fetch_one(&self.pool)
                .await?;
        if total <= max_bytes {
            return Ok(0);
        }
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT cover_art_id, local_path, size_bytes FROM cover_art_cache ORDER BY last_accessed_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut evicted = 0usize;
        for (id, path, size) in rows {
            if total <= max_bytes {
                break;
            }
            if protected_art_id == Some(id.as_str()) {
                continue;
            }
            let _ = tokio::fs::remove_file(&path).await;
            sqlx::query("DELETE FROM cover_art_cache WHERE cover_art_id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
            total = (total - size).max(0);
            evicted += 1;
        }
        Ok(evicted)
    }

    pub async fn stats(&self) -> CoreResult<CacheStats> {
        let (total_audio, pinned_audio, total_count, pinned_count): (i64, i64, i64, i64) =
            sqlx::query_as(
                r#"
                SELECT
                    COALESCE(SUM(size_bytes), 0),
                    COALESCE(SUM(CASE WHEN pinned = 1 THEN size_bytes ELSE 0 END), 0),
                    COUNT(1),
                    COALESCE(SUM(CASE WHEN pinned = 1 THEN 1 ELSE 0 END), 0)
                FROM offline_downloads
                "#,
            )
            .fetch_one(&self.pool)
            .await?;

        let (cover_bytes, cover_count): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(size_bytes), 0), COUNT(1) FROM cover_art_cache",
        )
        .fetch_one(&self.pool)
        .await?;

        let max_audio_bytes = self.policy.read().await.max_bytes;
        Ok(CacheStats {
            total_audio_bytes: total_audio.max(0) as u64,
            pinned_audio_bytes: pinned_audio.max(0) as u64,
            unpinned_audio_bytes: (total_audio - pinned_audio).max(0) as u64,
            cached_track_count: total_count.max(0) as u64,
            pinned_track_count: pinned_count.max(0) as u64,
            total_cover_art_bytes: cover_bytes.max(0) as u64,
            cached_cover_art_count: cover_count.max(0) as u64,
            max_audio_bytes,
        })
    }

    pub async fn list_pinned_targets(&self) -> CoreResult<Vec<PinRecord>> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT target_kind, target_id, pinned_at FROM offline_pins ORDER BY pinned_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(kind, id, pinned_at)| {
                PinTarget::from_db_pair(&kind, &id).map(|target| PinRecord { target, pinned_at })
            })
            .collect())
    }

    pub async fn list_offline_tracks(&self) -> CoreResult<Vec<OfflineTrackEntry>> {
        let rows = sqlx::query_as::<_, OfflineTrackEntry>(
            "SELECT * FROM offline_downloads ORDER BY pinned DESC, last_accessed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn is_track_pinned_by_rules(&self, track_id: &str) -> CoreResult<bool> {
        let pins: Vec<(String, String)> =
            sqlx::query_as("SELECT target_kind, target_id FROM offline_pins")
                .fetch_all(&self.pool)
                .await?;
        for (kind, id) in pins {
            if let Some(target) = PinTarget::from_db_pair(&kind, &id) {
                if self
                    .resolve_pin_target_tracks(&target)
                    .await?
                    .iter()
                    .any(|tid| tid == track_id)
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn recompute_pin_flags(&self) -> CoreResult<()> {
        let pins: Vec<(String, String)> =
            sqlx::query_as("SELECT target_kind, target_id FROM offline_pins")
                .fetch_all(&self.pool)
                .await?;

        let mut pinned_track_ids = HashSet::new();
        for (kind, id) in pins {
            if let Some(target) = PinTarget::from_db_pair(&kind, &id) {
                for tid in self.resolve_pin_target_tracks(&target).await? {
                    pinned_track_ids.insert(tid);
                }
            }
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE offline_downloads SET pinned = 0")
            .execute(&mut *tx)
            .await?;
        for tid in pinned_track_ids {
            sqlx::query("UPDATE offline_downloads SET pinned = 1 WHERE track_id = ?")
                .bind(tid)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn resolve_pin_target_tracks(&self, target: &PinTarget) -> CoreResult<Vec<String>> {
        let ids = match target {
            PinTarget::Track(id) => vec![id.clone()],
            PinTarget::Album(album_id) => {
                sqlx::query_scalar("SELECT id FROM tracks WHERE album_id = ?")
                    .bind(album_id)
                    .fetch_all(&self.pool)
                    .await?
            }
            PinTarget::Artist(artist_id) => {
                sqlx::query_scalar("SELECT id FROM tracks WHERE artist_id = ?")
                    .bind(artist_id)
                    .fetch_all(&self.pool)
                    .await?
            }
            PinTarget::Playlist(playlist_id) => {
                sqlx::query_scalar(
                    "SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position ASC",
                )
                .bind(playlist_id)
                .fetch_all(&self.pool)
                .await?
            }
            PinTarget::Starred => {
                sqlx::query_scalar("SELECT id FROM tracks WHERE starred = 1")
                    .fetch_all(&self.pool)
                    .await?
            }
            PinTarget::WeeklyRadar => {
                sqlx::query_scalar(
                    r#"
                    SELECT DISTINCT pt.track_id
                    FROM playlists p
                    INNER JOIN playlist_tracks pt ON pt.playlist_id = p.id
                    WHERE p.kind = 'discovery' OR LOWER(p.name) LIKE '%weekly radar%'
                    "#,
                )
                .fetch_all(&self.pool)
                .await?
            }
            PinTarget::SeasonalPack(slug) => {
                sqlx::query_scalar(
                    r#"
                    SELECT DISTINCT pt.track_id
                    FROM playlists p
                    INNER JOIN playlist_tracks pt ON pt.playlist_id = p.id
                    WHERE p.kind = 'seasonal' AND p.slug = ?
                    "#,
                )
                .bind(slug)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(ids)
    }
}

/// Collision-free, path-traversal-proof filename encoder.
/// Keeps `[A-Za-z0-9-]` literal and escapes every other byte (including `_`, `.`, `/`, `\`)
/// as `_{hex:02x}` so distinct IDs never collide and traversal sequences are impossible.
fn encode_filename_key(raw: &str) -> String {
    if raw.is_empty() {
        return "_empty".to_string();
    }
    let mut out = String::with_capacity(raw.len() + 8);
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            out.push(char::from(byte));
        } else {
            let _ = write!(&mut out, "_{byte:02x}");
        }
    }
    out
}

/// Strips all non-alphanumeric characters from a file extension to prevent path traversal
/// (e.g. from `AudioQualityProfile::CustomTranscode`).
fn sanitize_extension(raw: &str) -> String {
    let ext: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if ext.is_empty() {
        "bin".to_string()
    } else {
        ext
    }
}
