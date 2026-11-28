use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::StreamExt;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as TokioMutex, RwLock};
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

/// RAII guard that removes an in-progress `.part` file on error, cancellation, or drop
/// unless explicitly disarmed after a successful atomic `rename`.
struct PartFileGuard {
    path: PathBuf,
    active_parts: Arc<StdMutex<HashSet<PathBuf>>>,
    armed: bool,
}

impl PartFileGuard {
    fn new(path: PathBuf, active_parts: Arc<StdMutex<HashSet<PathBuf>>>) -> Self {
        {
            let mut guard = active_parts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.insert(path.clone());
        }
        Self {
            path,
            active_parts,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
        let mut guard = self
            .active_parts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&self.path);
    }
}

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
    cache_lock: Arc<TokioMutex<()>>,
    active_parts: Arc<StdMutex<HashSet<PathBuf>>>,
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
            cache_lock: Arc::new(TokioMutex::new(())),
            active_parts: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Removes orphaned `*.part` temporary files left behind by prior crashes or aborts,
    /// while skipping any `.part` files currently owned by an active download guard.
    pub async fn sweep_orphaned_parts(&self) {
        for dir in [&self.cache_dir, &self.cover_art_dir] {
            sweep_orphaned_parts_in_dir(dir, &self.active_parts).await;
        }
    }

    pub async fn policy(&self) -> CachePolicy {
        self.policy.read().await.clone()
    }

    pub async fn set_policy(&self, policy: CachePolicy) -> CoreResult<()> {
        let _guard = self.cache_lock.lock().await;
        *self.policy.write().await = policy;
        self.sweep_orphaned_parts().await;
        self.enforce_budget_locked(None).await?;
        self.enforce_cover_art_budget_locked(None).await?;
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
        {
            let _guard = self.cache_lock.lock().await;
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
                    sqlx::query(
                        "UPDATE offline_downloads SET last_accessed_at = ? WHERE track_id = ?",
                    )
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
        let _guard = self.cache_lock.lock().await;
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

        self.recompute_pin_flags_locked().await?;
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
        let _guard = self.cache_lock.lock().await;
        let (kind, id) = target.as_db_pair();
        sqlx::query("DELETE FROM offline_pins WHERE target_kind = ? AND target_id = ?")
            .bind(kind)
            .bind(id)
            .execute(&self.pool)
            .await?;

        self.recompute_pin_flags_locked().await?;
        self.enforce_budget_locked(None).await
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
        let never = AtomicBool::new(false);
        self.download_track_cancellable(track_id, profile, pinned, pin_source, &never)
            .await
    }

    /// Cooperatively cancellable variant of [`Self::download_track`].
    /// Checks `cancelled` during HTTP streaming (cleaning up the `.part` file via RAII guard),
    /// while executing the rename + SQLite registration + budget eviction atomically under
    /// `cache_lock`.
    pub async fn download_track_cancellable(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
        pinned: bool,
        pin_source: Option<&str>,
        cancelled: &AtomicBool,
    ) -> CoreResult<OfflineTrackEntry> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "download cancelled",
            )));
        }

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

        let mut part_guard = PartFileGuard::new(part_path.clone(), self.active_parts.clone());

        let size_bytes: u64 = {
            let mut file = tokio::fs::File::create(&part_path).await?;
            let mut stream = resp.bytes_stream();
            let mut size_bytes: u64 = 0;
            while let Some(chunk) = stream.next().await {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(CoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "download cancelled",
                    )));
                }
                let bytes = chunk?;
                file.write_all(&bytes).await?;
                size_bytes = size_bytes.saturating_add(bytes.len() as u64);
            }
            file.flush().await?;
            drop(file);
            if size_bytes == 0 {
                return Err(parse_subsonic_error_bytes(&[], content_type.as_deref()));
            }
            size_bytes
        };

        if cancelled.load(Ordering::Relaxed) {
            return Err(CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "download cancelled",
            )));
        }

        let _guard = self.cache_lock.lock().await;
        let prev_path: Option<String> =
            sqlx::query_scalar("SELECT local_path FROM offline_downloads WHERE track_id = ?")
                .bind(track_id)
                .fetch_optional(&self.pool)
                .await?;

        tokio::fs::rename(&part_path, &final_path).await?;
        part_guard.disarm();

        let new_path_str = final_path.display().to_string();
        match self
            .register_cached_file_locked(
                track_id,
                &final_path,
                profile.format_label(),
                profile.max_bit_rate(),
                size_bytes,
                pinned,
                pin_source,
                now_unix(),
                prev_path.clone(),
            )
            .await
        {
            Ok(entry) => Ok(entry),
            Err(err) => {
                if prev_path.as_deref() != Some(new_path_str.as_str()) {
                    let _ = tokio::fs::remove_file(&final_path).await;
                }
                Err(err)
            }
        }
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
        let _guard = self.cache_lock.lock().await;
        let prev_path: Option<String> =
            sqlx::query_scalar("SELECT local_path FROM offline_downloads WHERE track_id = ?")
                .bind(track_id)
                .fetch_optional(&self.pool)
                .await?;
        self.register_cached_file_locked(
            track_id,
            local_path,
            format,
            bit_rate,
            size_bytes,
            pinned,
            pin_source,
            accessed_at,
            prev_path,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn register_cached_file_locked(
        &self,
        track_id: &str,
        local_path: &Path,
        format: &str,
        bit_rate: Option<u32>,
        size_bytes: u64,
        pinned: bool,
        pin_source: Option<&str>,
        accessed_at: i64,
        prev_path: Option<String>,
    ) -> CoreResult<OfflineTrackEntry> {
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

        let mut tx = self.pool.begin().await?;
        if pinned {
            // Persist explicit track pin in `offline_pins` so a later `pin()`/`unpin()` on
            // another target running `recompute_pin_flags` never silently unpins this track.
            sqlx::query(
                r#"
                INSERT INTO offline_pins (target_kind, target_id, pinned_at)
                VALUES ('track', ?, ?)
                ON CONFLICT(target_kind, target_id) DO UPDATE SET pinned_at = excluded.pinned_at
                "#,
            )
            .bind(track_id)
            .bind(accessed_at)
            .execute(&mut *tx)
            .await?;
        }

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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        if let Some(old_path) = prev_path {
            if old_path != new_path_str {
                let _ = tokio::fs::remove_file(&old_path).await;
            }
        }

        self.enforce_budget_locked(Some(track_id)).await?;
        Ok(entry)
    }

    /// Evicts unpinned audio entries in strict LRU order (`last_accessed_at ASC`) until total
    /// audio cache bytes are within `policy.max_bytes`. Pinned items are never evicted.
    pub async fn enforce_budget(&self) -> CoreResult<usize> {
        let _guard = self.cache_lock.lock().await;
        self.sweep_orphaned_parts().await;
        self.enforce_budget_locked(None).await
    }

    async fn enforce_budget_locked(
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

        let candidates: Vec<(String, String, i64, i64)> = sqlx::query_as(
            r#"
            SELECT track_id, local_path, size_bytes, last_accessed_at
            FROM offline_downloads
            WHERE pinned = 0
            ORDER BY last_accessed_at ASC, downloaded_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut evicted = 0usize;
        for (track_id, local_path, size_bytes, last_accessed_at) in candidates {
            if total <= max_bytes {
                break;
            }
            if protected_track_id == Some(track_id.as_str()) {
                continue;
            }
            // Guard eviction delete against concurrent pin or access touch; only unlink the
            // backing file when this delete actually claimed the row.
            let res = sqlx::query(
                "DELETE FROM offline_downloads WHERE track_id = ? AND pinned = 0 AND last_accessed_at = ?",
            )
            .bind(&track_id)
            .bind(last_accessed_at)
            .execute(&self.pool)
            .await?;
            if res.rows_affected() > 0 {
                let _ = tokio::fs::remove_file(&local_path).await;
                total = (total - size_bytes).max(0);
                evicted += 1;
            }
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
        let never = AtomicBool::new(false);
        self.prefetch_queue_cancellable(queue_track_ids, current_index, profile, is_wifi, &never)
            .await
    }

    pub async fn prefetch_queue_cancellable(
        &self,
        queue_track_ids: &[String],
        current_index: usize,
        profile: &AudioQualityProfile,
        is_wifi: bool,
        cancelled: &AtomicBool,
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
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(1) FROM offline_downloads WHERE track_id = ?",
            )
            .bind(track_id)
            .fetch_one(&self.pool)
            .await?;
            if exists > 0 {
                continue;
            }
            match self
                .download_track_cancellable(track_id, profile, false, Some("prefetch"), cancelled)
                .await
            {
                Ok(_) => downloaded.push(track_id.clone()),
                Err(err @ CoreError::NotConfigured(_)) => return Err(err),
                Err(_) if cancelled.load(Ordering::Relaxed) => break,
                Err(err) => {
                    tracing::warn!(
                        track_id = %track_id,
                        "prefetch download failed; continuing batch: {err}"
                    );
                }
            }
        }
        Ok(downloaded)
    }

    /// Applies auto-pinning rules (`Starred`, `WeeklyRadar`, active `SeasonalPack`) when allowed by network state.
    pub async fn run_auto_cache_rules(
        &self,
        profile: &AudioQualityProfile,
        is_wifi: bool,
    ) -> CoreResult<Vec<String>> {
        let never = AtomicBool::new(false);
        self.run_auto_cache_rules_cancellable(profile, is_wifi, &never)
            .await
    }

    pub async fn run_auto_cache_rules_cancellable(
        &self,
        profile: &AudioQualityProfile,
        is_wifi: bool,
        cancelled: &AtomicBool,
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
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            // Pass `pinned: false` here because the collection pin target was already recorded
            // in `offline_pins` above; `register_cached_file_locked` computes `effective_pinned`
            // from `offline_pins`, so unpinning the collection later cleanly unpins these tracks.
            match self
                .download_track_cancellable(&track_id, profile, false, Some(&source), cancelled)
                .await
            {
                Ok(_) => completed.push(track_id),
                Err(err @ CoreError::NotConfigured(_)) => return Err(err),
                Err(_) if cancelled.load(Ordering::Relaxed) => break,
                Err(err) => {
                    tracing::warn!(
                        track_id = %track_id,
                        pin_source = %source,
                        "auto-cache download failed; continuing batch: {err}"
                    );
                }
            }
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
        {
            let _guard = self.cache_lock.lock().await;
            if let Some(local) = self.cover_art_local_path(cover_art_id).await? {
                let _ = sqlx::query(
                    "UPDATE cover_art_cache SET last_accessed_at = ? WHERE cover_art_id = ?",
                )
                .bind(now_unix())
                .bind(cover_art_id)
                .execute(&self.pool)
                .await;
                return Ok((local, false));
            }
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

        tokio::fs::create_dir_all(&self.cover_art_dir).await?;
        let safe_id = encode_filename_key(cover_art_id);
        let final_path = self.cover_art_dir.join(format!("{safe_id}.{ext}"));
        let part_path = self
            .cover_art_dir
            .join(format!("{safe_id}.{}.part", Uuid::new_v4()));

        let mut part_guard = PartFileGuard::new(part_path.clone(), self.active_parts.clone());
        tokio::fs::write(&part_path, &bytes).await?;

        let _guard = self.cache_lock.lock().await;
        let prev_path: Option<String> = sqlx::query_scalar(
            "SELECT local_path FROM cover_art_cache WHERE cover_art_id = ?",
        )
        .bind(cover_art_id)
        .fetch_optional(&self.pool)
        .await?;

        tokio::fs::rename(&part_path, &final_path).await?;
        part_guard.disarm();

        let now = now_unix();
        let new_path_str = final_path.display().to_string();
        let size_i64 = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let db_res = sqlx::query(
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
        .await;

        if let Err(err) = db_res {
            if prev_path.as_deref() != Some(new_path_str.as_str()) {
                let _ = tokio::fs::remove_file(&final_path).await;
            }
            return Err(err.into());
        }

        if let Some(old_path) = prev_path {
            if old_path != new_path_str {
                let _ = tokio::fs::remove_file(&old_path).await;
            }
        }

        self.enforce_cover_art_budget_locked(Some(cover_art_id))
            .await?;
        Ok((final_path, true))
    }

    async fn enforce_cover_art_budget_locked(
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
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT cover_art_id, local_path, size_bytes, last_accessed_at FROM cover_art_cache ORDER BY last_accessed_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut evicted = 0usize;
        for (id, path, size, last_accessed_at) in rows {
            if total <= max_bytes {
                break;
            }
            if protected_art_id == Some(id.as_str()) {
                continue;
            }
            let res = sqlx::query(
                "DELETE FROM cover_art_cache WHERE cover_art_id = ? AND last_accessed_at = ?",
            )
            .bind(&id)
            .bind(last_accessed_at)
            .execute(&self.pool)
            .await?;
            if res.rows_affected() > 0 {
                let _ = tokio::fs::remove_file(&path).await;
                total = (total - size).max(0);
                evicted += 1;
            }
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

    async fn recompute_pin_flags_locked(&self) -> CoreResult<()> {
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

async fn sweep_orphaned_parts_in_dir(dir: &Path, active_parts: &StdMutex<HashSet<PathBuf>>) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("part") {
            continue;
        }
        let is_active = {
            let guard = active_parts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.contains(&path)
        };
        if !is_active {
            let _ = tokio::fs::remove_file(&path).await;
        }
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
