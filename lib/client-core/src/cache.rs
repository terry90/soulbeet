use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, UNIX_EPOCH};

use futures::StreamExt;
use image::{ImageReader, Limits, RgbaImage};
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
    verified_cover_art_paths: Arc<StdMutex<HashSet<String>>>,
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
            verified_cover_art_paths: Arc::new(StdMutex::new(HashSet::new())),
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
        let entry = match self
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
            Ok(entry) => entry,
            Err(err) => {
                if prev_path.as_deref() != Some(new_path_str.as_str()) {
                    let _ = tokio::fs::remove_file(&final_path).await;
                }
                return Err(err);
            }
        };
        // Enforce LRU budget strictly after the DB commit-rollback guard so a post-commit
        // budget error can never unlink the newly committed audio file.
        self.enforce_budget_locked(Some(track_id)).await?;
        Ok(entry)
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
        let entry = self
            .register_cached_file_locked(
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
            .await?;
        self.enforce_budget_locked(Some(track_id)).await?;
        Ok(entry)
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

    /// Local cover-art path lookup with zero network I/O (safe for Android Auto `ContentProvider`
    /// Binder threads). Verifies the file exists as a regular file canonically contained within
    /// `cover_art_dir`, and self-heals stale cached placeholder images (e.g. resized Navidrome
    /// blue-vinyl or Last.fm gray-star placeholders written by older versions) by evicting the
    /// file and its `cover_art_cache` row safely under `cache_lock`. Valid files are verified at
    /// most once per process.
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
        let meta = match tokio::fs::metadata(&pb).await {
            Ok(m) if m.is_file() => m,
            _ => return Ok(None),
        };

        let canonical_base = tokio::fs::canonicalize(&self.cover_art_dir).await.ok();
        let canonical_file = tokio::fs::canonicalize(&pb).await.ok();
        let canonical_file = match (canonical_base, canonical_file) {
            (Some(base), Some(file)) if file.starts_with(&base) => file,
            _ => return Ok(None),
        };

        let orig_len = meta.len();
        let orig_mtime = file_mtime_nanos(&meta);
        let verify_key = cover_art_verify_key(&canonical_file, orig_len, orig_mtime);
        if self.is_cover_art_verified(&verify_key) {
            return Ok(Some(pb));
        }

        if orig_len <= MAX_PLACEHOLDER_INSPECT_BYTES as u64 {
            if let Ok(bytes) = tokio::fs::read(&pb).await {
                let is_placeholder =
                    tokio::task::spawn_blocking(move || is_navidrome_placeholder_bytes(&bytes))
                        .await
                        .unwrap_or(false);
                if is_placeholder {
                    // Acquire cache_lock and re-stat before unlinking so a concurrent writer's
                    // freshly validated file at the same path is never deleted.
                    let _guard = self.cache_lock.lock().await;
                    if let Ok(cur_meta) = tokio::fs::metadata(&pb).await {
                        if cur_meta.is_file() {
                            let cur_len = cur_meta.len();
                            let cur_mtime = file_mtime_nanos(&cur_meta);
                            if cur_len == orig_len && cur_mtime == orig_mtime {
                                let _ = tokio::fs::remove_file(&pb).await;
                                let _ = sqlx::query(
                                    "DELETE FROM cover_art_cache WHERE cover_art_id = ? AND local_path = ?",
                                )
                                .bind(cover_art_id)
                                .bind(&p)
                                .execute(&self.pool)
                                .await;
                                return Ok(None);
                            }
                            let cur_key =
                                cover_art_verify_key(&canonical_file, cur_len, cur_mtime);
                            if self.is_cover_art_verified(&cur_key) {
                                return Ok(Some(pb));
                            }
                        }
                    }
                    return Ok(None);
                }
            }
        }

        self.mark_cover_art_verified(verify_key);
        Ok(Some(pb))
    }

    fn is_cover_art_verified(&self, verify_key: &str) -> bool {
        let guard = self
            .verified_cover_art_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.contains(verify_key)
    }

    fn mark_cover_art_verified(&self, verify_key: String) {
        let mut guard = self
            .verified_cover_art_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= MAX_VERIFIED_COVER_ART_ENTRIES {
            guard.clear();
        }
        guard.insert(verify_key);
    }

    async fn record_cover_art_missing(&self, cover_art_id: &str) {
        let _ = sqlx::query(
            r#"
            INSERT INTO cover_art_missing (cover_art_id, checked_at)
            VALUES (?, ?)
            ON CONFLICT(cover_art_id) DO UPDATE SET checked_at = excluded.checked_at
            "#,
        )
        .bind(cover_art_id)
        .bind(now_unix())
        .execute(&self.pool)
        .await;
    }

    /// Warms up to `limit` uncached cover-art items in priority order (playlists, recent albums,
    /// starred/offline tracks, artists) with bounded concurrency (2) and per-item timeout (6s).
    pub async fn warm_cover_art_cache(
        &self,
        limit: usize,
        size: Option<u32>,
        offline_only: bool,
    ) -> CoreResult<usize> {
        let never = AtomicBool::new(false);
        self.warm_cover_art_cache_cancellable(limit, size, offline_only, &never)
            .await
    }

    /// Cooperatively cancellable cover-art cache warmer.
    /// Fetches distinct uncached `cover_art_id`s in priority order with concurrency 2 so Navidrome
    /// artwork throttling is respected. Excludes IDs recorded in `cover_art_missing` within the
    /// last 24 hours so artless items never starve real covers. Per-item errors (placeholders,
    /// 404s, timeouts) are skipped without failing the batch.
    pub async fn warm_cover_art_cache_cancellable(
        &self,
        limit: usize,
        size: Option<u32>,
        offline_only: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<usize> {
        if offline_only || limit == 0 || cancelled.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if self.navidrome.read().await.is_none() {
            return Ok(0);
        }

        let query_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let missing_cutoff = now_unix().saturating_sub(COVER_ART_MISSING_TTL_SECS);
        let mut candidates: Vec<String> = Vec::with_capacity(limit.min(256));
        let mut seen: HashSet<String> = HashSet::with_capacity(limit.min(256));

        // 1. Playlists (most recently updated first)
        if candidates.len() < limit {
            let rows: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT p.cover_art_id
                FROM playlists p
                LEFT JOIN cover_art_cache c ON c.cover_art_id = p.cover_art_id
                LEFT JOIN cover_art_missing m
                  ON m.cover_art_id = p.cover_art_id AND m.checked_at > ?
                WHERE p.cover_art_id IS NOT NULL
                  AND TRIM(p.cover_art_id) != ''
                  AND c.cover_art_id IS NULL
                  AND m.cover_art_id IS NULL
                GROUP BY p.cover_art_id
                ORDER BY MAX(p.updated_at) DESC
                LIMIT ?
                "#,
            )
            .bind(missing_cutoff)
            .bind(query_limit)
            .fetch_all(&self.pool)
            .await?;
            push_cover_art_bucket(rows, &mut candidates, &mut seen, limit);
        }

        // 2. Recent albums
        if candidates.len() < limit {
            let rows: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT a.cover_art_id
                FROM albums a
                LEFT JOIN cover_art_cache c ON c.cover_art_id = a.cover_art_id
                LEFT JOIN cover_art_missing m
                  ON m.cover_art_id = a.cover_art_id AND m.checked_at > ?
                WHERE a.cover_art_id IS NOT NULL
                  AND TRIM(a.cover_art_id) != ''
                  AND c.cover_art_id IS NULL
                  AND m.cover_art_id IS NULL
                GROUP BY a.cover_art_id
                ORDER BY MAX(a.updated_at) DESC
                LIMIT ?
                "#,
            )
            .bind(missing_cutoff)
            .bind(query_limit)
            .fetch_all(&self.pool)
            .await?;
            push_cover_art_bucket(rows, &mut candidates, &mut seen, limit);
        }

        // 3. Starred or offline tracks
        if candidates.len() < limit {
            let rows: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT t.cover_art_id
                FROM tracks t
                LEFT JOIN cover_art_cache c ON c.cover_art_id = t.cover_art_id
                LEFT JOIN cover_art_missing m
                  ON m.cover_art_id = t.cover_art_id AND m.checked_at > ?
                WHERE t.cover_art_id IS NOT NULL
                  AND TRIM(t.cover_art_id) != ''
                  AND c.cover_art_id IS NULL
                  AND m.cover_art_id IS NULL
                  AND (t.starred = 1 OR t.id IN (SELECT track_id FROM offline_downloads))
                GROUP BY t.cover_art_id
                ORDER BY MAX(t.updated_at) DESC
                LIMIT ?
                "#,
            )
            .bind(missing_cutoff)
            .bind(query_limit)
            .fetch_all(&self.pool)
            .await?;
            push_cover_art_bucket(rows, &mut candidates, &mut seen, limit);
        }

        // 4. Artists (alphabetical)
        if candidates.len() < limit {
            let rows: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT ar.cover_art_id
                FROM artists ar
                LEFT JOIN cover_art_cache c ON c.cover_art_id = ar.cover_art_id
                LEFT JOIN cover_art_missing m
                  ON m.cover_art_id = ar.cover_art_id AND m.checked_at > ?
                WHERE ar.cover_art_id IS NOT NULL
                  AND TRIM(ar.cover_art_id) != ''
                  AND c.cover_art_id IS NULL
                  AND m.cover_art_id IS NULL
                GROUP BY ar.cover_art_id
                ORDER BY MIN(ar.name COLLATE NOCASE) ASC
                LIMIT ?
                "#,
            )
            .bind(missing_cutoff)
            .bind(query_limit)
            .fetch_all(&self.pool)
            .await?;
            push_cover_art_bucket(rows, &mut candidates, &mut seen, limit);
        }

        candidates.truncate(limit);
        if candidates.is_empty() {
            return Ok(0);
        }

        let mut fetched_count = 0usize;
        let mut stream = futures::stream::iter(candidates)
            .map(|cover_art_id| async move {
                if cancelled.load(Ordering::Relaxed) {
                    return false;
                }
                match tokio::time::timeout(
                    Duration::from_secs(6),
                    self.get_or_fetch_cover_art_with_status(&cover_art_id, size, false),
                )
                .await
                {
                    Ok(Ok((_, newly_fetched))) => newly_fetched,
                    Ok(Err(err)) => {
                        tracing::debug!(
                            cover_art_id = %cover_art_id,
                            "cover art warm skipped item: {err}"
                        );
                        false
                    }
                    Err(_) => {
                        tracing::debug!(
                            cover_art_id = %cover_art_id,
                            "cover art warm timed out after 6s"
                        );
                        false
                    }
                }
            })
            .buffer_unordered(2);

        while let Some(newly_fetched) = stream.next().await {
            if newly_fetched {
                fetched_count += 1;
            }
        }

        Ok(fetched_count)
    }

    /// Returns cached cover-art path, or fetches and persists it on cache miss (unless `offline_only`).
    /// Rejects Subsonic HTTP 200 JSON/XML/plain-text error envelopes, empty bodies, and Navidrome
    /// built-in placeholder images so Soulbeet's own artwork renderer handles missing art cleanly.
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
        // Perform local lookup and any placeholder pixel check outside `cache_lock`.
        if let Some(local) = self.cover_art_local_path(cover_art_id).await? {
            let local_str = local.display().to_string();
            let _guard = self.cache_lock.lock().await;
            let res = sqlx::query(
                "UPDATE cover_art_cache SET last_accessed_at = ? WHERE cover_art_id = ? AND local_path = ?",
            )
            .bind(now_unix())
            .bind(cover_art_id)
            .bind(&local_str)
            .execute(&self.pool)
            .await;
            if res.map(|r| r.rows_affected() > 0).unwrap_or(false)
                && tokio::fs::try_exists(&local).await.unwrap_or(false)
            {
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
        let (bytes, content_type) = match subsonic.fetch_bytes(&url).await {
            Ok(res) => res,
            Err(err) => {
                if matches!(
                    err,
                    CoreError::ServerHttp {
                        status: 404 | 410,
                        ..
                    } | CoreError::Subsonic { code: 70, .. }
                ) {
                    self.record_cover_art_missing(cover_art_id).await;
                }
                return Err(err);
            }
        };

        if bytes.is_empty() || is_subsonic_error_content_type(content_type.as_deref()) {
            let err = parse_subsonic_error_bytes(&bytes, content_type.as_deref());
            if matches!(err, CoreError::Subsonic { code: 70, .. }) {
                self.record_cover_art_missing(cover_art_id).await;
            }
            return Err(err);
        }

        let (bytes, is_placeholder) = tokio::task::spawn_blocking(move || {
            let is_placeholder = is_navidrome_placeholder_bytes(&bytes);
            (bytes, is_placeholder)
        })
        .await
        .map_err(|err| {
            CoreError::Io(std::io::Error::other(format!(
                "cover art placeholder check failed: {err}"
            )))
        })?;

        if is_placeholder {
            self.record_cover_art_missing(cover_art_id).await;
            return Err(CoreError::InvalidInput(format!(
                "navidrome placeholder artwork for {cover_art_id}"
            )));
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

        let _ = sqlx::query("DELETE FROM cover_art_missing WHERE cover_art_id = ?")
            .bind(cover_art_id)
            .execute(&self.pool)
            .await;

        if let (Ok(meta), Ok(canonical_file)) = (
            tokio::fs::metadata(&final_path).await,
            tokio::fs::canonicalize(&final_path).await,
        ) {
            let key =
                cover_art_verify_key(&canonical_file, meta.len(), file_mtime_nanos(&meta));
            self.mark_cover_art_verified(key);
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

fn push_cover_art_bucket(
    rows: Vec<String>,
    candidates: &mut Vec<String>,
    seen: &mut HashSet<String>,
    limit: usize,
) {
    for raw in rows {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            candidates.push(trimmed.to_string());
            if candidates.len() >= limit {
                break;
            }
        }
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

const NAVIDROME_ALBUM_PLACEHOLDER_LEN: u64 = 69_228;
const NAVIDROME_ALBUM_PLACEHOLDER_FNV1A: u64 = 0xbed0_ba17_fa90_9643;
const NAVIDROME_ARTIST_PLACEHOLDER_LEN: u64 = 546;
const NAVIDROME_ARTIST_PLACEHOLDER_FNV1A: u64 = 0xa027_5d7a_1708_302f;
const MAX_PLACEHOLDER_INSPECT_BYTES: usize = 2 * 1024 * 1024;
const MAX_PLACEHOLDER_DIMENSION: u32 = 2048;
const MAX_PLACEHOLDER_DECODE_ALLOC: u64 = 32 * 1024 * 1024;
const MAX_VERIFIED_COVER_ART_ENTRIES: usize = 2048;
const COVER_ART_MISSING_TTL_SECS: i64 = 86_400;

fn file_mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn cover_art_verify_key(canonical_file: &Path, len: u64, mtime_nanos: u128) -> String {
    format!("{}|{len}|{mtime_nanos}", canonical_file.display())
}

fn is_navidrome_placeholder_size(len: u64) -> bool {
    len == NAVIDROME_ALBUM_PLACEHOLDER_LEN || len == NAVIDROME_ARTIST_PLACEHOLDER_LEN
}

fn is_nearly_square(w: u32, h: u32) -> bool {
    if w < 32 || h < 32 {
        return false;
    }
    let max_dim = u64::from(w.max(h));
    let min_dim = u64::from(w.min(h));
    max_dim * 100 <= min_dim * 105
}

/// Detects Navidrome's built-in `album-placeholder.webp` (blue vinyl), `artist-placeholder.webp`
/// (gray star), resized/re-encoded variants returned by `/rest/getCoverArt?size=...`, and
/// Last.fm's generic gray-star placeholder so we never persist or display default placeholder art.
pub fn is_navidrome_placeholder_bytes(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    // Fast path: exact raw byte length + FNV-1a hash of Navidrome's embedded placeholder assets.
    let len = bytes.len() as u64;
    if is_navidrome_placeholder_size(len) {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        if (len == NAVIDROME_ALBUM_PLACEHOLDER_LEN && h == NAVIDROME_ALBUM_PLACEHOLDER_FNV1A)
            || (len == NAVIDROME_ARTIST_PLACEHOLDER_LEN && h == NAVIDROME_ARTIST_PLACEHOLDER_FNV1A)
        {
            return true;
        }
    }

    if bytes.len() > MAX_PLACEHOLDER_INSPECT_BYTES {
        return false;
    }

    // Header-only dimension check before allocating any pixel buffer.
    let Ok(dim_reader) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    let Ok((w, h)) = dim_reader.into_dimensions() else {
        return false;
    };
    if w > MAX_PLACEHOLDER_DIMENSION || h > MAX_PLACEHOLDER_DIMENSION || !is_nearly_square(w, h) {
        return false;
    }

    let Ok(mut reader) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PLACEHOLDER_DIMENSION);
    limits.max_image_height = Some(MAX_PLACEHOLDER_DIMENSION);
    limits.max_alloc = Some(MAX_PLACEHOLDER_DECODE_ALLOC);
    reader.limits(limits);

    let Ok(dynamic) = reader.decode() else {
        return false;
    };
    let img = dynamic.to_rgba8();
    matches_navidrome_blue_vinyl(&img) || matches_gray_star_placeholder(&img)
}

fn sample_rgba(img: &RgbaImage, fx: f32, fy: f32) -> [u8; 4] {
    let (w, h) = img.dimensions();
    let x = ((w as f32) * fx).floor().clamp(0.0, (w - 1) as f32) as u32;
    let y = ((h as f32) * fy).floor().clamp(0.0, (h - 1) as f32) as u32;
    img.get_pixel(x, y).0
}

fn channel_spread(p: [u8; 4]) -> u8 {
    let max_c = p[0].max(p[1]).max(p[2]);
    let min_c = p[0].min(p[1]).min(p[2]);
    max_c - min_c
}

fn is_navidrome_blue(p: [u8; 4]) -> bool {
    let [r, g, b, a] = p;
    a >= 200
        && b >= 150
        && r <= 85
        && b >= g.saturating_add(50)
        && b >= r.saturating_add(100)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VinylCornerBg {
    Transparent,
    Black,
    White,
}

fn classify_vinyl_corner(p: [u8; 4]) -> Option<VinylCornerBg> {
    let [r, g, b, a] = p;
    if a < 32 {
        Some(VinylCornerBg::Transparent)
    } else if r <= 25 && g <= 25 && b <= 25 {
        Some(VinylCornerBg::Black)
    } else if r >= 230 && g >= 230 && b >= 230 {
        Some(VinylCornerBg::White)
    } else {
        None
    }
}

fn matches_vinyl_bg(p: [u8; 4], bg: VinylCornerBg) -> bool {
    let [r, g, b, a] = p;
    match bg {
        VinylCornerBg::Transparent => a < 64,
        VinylCornerBg::Black => a >= 200 && r <= 30 && g <= 30 && b <= 30,
        VinylCornerBg::White => a >= 200 && r >= 225 && g >= 225 && b >= 225,
    }
}

fn matches_navidrome_blue_vinyl(img: &RgbaImage) -> bool {
    let (w, h) = img.dimensions();
    if !is_nearly_square(w, h) {
        return false;
    }

    let corners = [
        sample_rgba(img, 0.02, 0.02),
        sample_rgba(img, 0.98, 0.02),
        sample_rgba(img, 0.02, 0.98),
        sample_rgba(img, 0.98, 0.98),
    ];
    let Some(bg) = classify_vinyl_corner(corners[0]) else {
        return false;
    };
    if !corners.iter().all(|&c| classify_vinyl_corner(c) == Some(bg)) {
        return false;
    }

    // Center spindle area is dark charcoal neutral.
    let center = sample_rgba(img, 0.50, 0.50);
    if center[3] < 200
        || center[0] > 75
        || center[1] > 75
        || center[2] > 75
        || channel_spread(center) > 30
    {
        return false;
    }

    // Symmetric blue vinyl ring points around the center.
    let ring_points = [
        sample_rgba(img, 0.25, 0.50),
        sample_rgba(img, 0.75, 0.50),
        sample_rgba(img, 0.50, 0.25),
        sample_rgba(img, 0.50, 0.75),
        sample_rgba(img, 0.25, 0.25),
        sample_rgba(img, 0.75, 0.25),
        sample_rgba(img, 0.25, 0.75),
        sample_rgba(img, 0.75, 0.75),
    ];
    let blue_ring_hits = ring_points.iter().filter(|&&p| is_navidrome_blue(p)).count();
    if blue_ring_hits < 7 {
        return false;
    }

    // Sample a 32x32 grid to verify overall color distribution and reject normal blue covers.
    const GRID: u32 = 32;
    let total = (GRID * GRID) as usize;
    let mut blue_count = 0usize;
    let mut bg_count = 0usize;
    let mut other_chromatic_count = 0usize;

    for gy in 0..GRID {
        let y = (((gy as f32 + 0.5) / GRID as f32) * h as f32) as u32;
        let y = y.min(h - 1);
        for gx in 0..GRID {
            let x = (((gx as f32 + 0.5) / GRID as f32) * w as f32) as u32;
            let x = x.min(w - 1);
            let p = img.get_pixel(x, y).0;
            if matches_vinyl_bg(p, bg) {
                bg_count += 1;
            } else if is_navidrome_blue(p) {
                blue_count += 1;
            } else if p[3] >= 128 && channel_spread(p) > 40 {
                let is_blue_edge = p[2] > p[0].saturating_add(30) && p[2] > p[1].saturating_add(15);
                if !is_blue_edge {
                    other_chromatic_count += 1;
                }
            }
        }
    }

    let blue_pct = blue_count * 100 / total;
    let bg_pct = bg_count * 100 / total;
    let other_pct = other_chromatic_count * 100 / total;

    (32..=68).contains(&blue_pct) && (22..=58).contains(&bg_pct) && other_pct <= 3
}

fn matches_gray_star_placeholder(img: &RgbaImage) -> bool {
    let (w, h) = img.dimensions();
    if !is_nearly_square(w, h) {
        return false;
    }

    let corners = [
        sample_rgba(img, 0.02, 0.02),
        sample_rgba(img, 0.98, 0.02),
        sample_rgba(img, 0.02, 0.98),
        sample_rgba(img, 0.98, 0.98),
    ];
    let mut bg_sum = 0u32;
    for &c in &corners {
        if c[3] < 200 || channel_spread(c) > 14 {
            return false;
        }
        let lum = (u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3;
        if !(150..=245).contains(&lum) {
            return false;
        }
        bg_sum += lum;
    }
    let bg = (bg_sum / 4) as u8;
    for &c in &corners {
        let lum = ((u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3) as u8;
        if lum.abs_diff(bg) > 14 {
            return false;
        }
    }

    // Center and upper star tip must be bright near-white above the gray background.
    let center = sample_rgba(img, 0.50, 0.50);
    let upper_tip = sample_rgba(img, 0.50, 0.25);
    for p in [center, upper_tip] {
        let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
        if p[3] < 200 || channel_spread(p) > 14 || lum < 235 || lum < bg.saturating_add(10) {
            return false;
        }
    }

    // Outer diagonal quadrants sit on the flat gray canvas outside the star.
    let outer_diags = [
        sample_rgba(img, 0.20, 0.20),
        sample_rgba(img, 0.80, 0.20),
        sample_rgba(img, 0.20, 0.80),
        sample_rgba(img, 0.80, 0.80),
    ];
    for p in outer_diags {
        let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
        if p[3] < 200 || channel_spread(p) > 16 || lum.abs_diff(bg) > 16 {
            return false;
        }
    }

    // Full 32x32 grid verification rejects monochrome portraits/covers that have dark details.
    const GRID: u32 = 32;
    let total = (GRID * GRID) as usize;
    let mut achromatic = 0usize;
    let mut bg_pixels = 0usize;
    let mut star_pixels = 0usize;
    let mut darker_than_bg = 0usize;

    for gy in 0..GRID {
        let y = (((gy as f32 + 0.5) / GRID as f32) * h as f32) as u32;
        let y = y.min(h - 1);
        for gx in 0..GRID {
            let x = (((gx as f32 + 0.5) / GRID as f32) * w as f32) as u32;
            let x = x.min(w - 1);
            let p = img.get_pixel(x, y).0;
            if p[3] < 200 {
                return false;
            }
            if channel_spread(p) <= 16 {
                achromatic += 1;
            }
            let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
            if lum.abs_diff(bg) <= 18 {
                bg_pixels += 1;
            } else if lum >= 235 && lum >= bg.saturating_add(10) {
                star_pixels += 1;
            } else if lum < bg.saturating_sub(25) {
                darker_than_bg += 1;
            }
        }
    }

    let achromatic_pct = achromatic * 100 / total;
    let bg_pct = bg_pixels * 100 / total;
    let star_pct = star_pixels * 100 / total;
    let dark_pct = darker_than_bg * 100 / total;

    achromatic_pct >= 97 && bg_pct >= 45 && (10..=50).contains(&star_pct) && dark_pct == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba};
    use std::f32::consts::PI;

    fn encode_image(img: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        if format == ImageFormat::Jpeg {
            let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            rgb.write_to(&mut buf, ImageFormat::Jpeg).unwrap();
        } else {
            img.write_to(&mut buf, format).unwrap();
        }
        buf.into_inner()
    }

    fn synthesize_blue_vinyl(size: u32, corner_rgba: [u8; 4]) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(size, size, Rgba(corner_rgba));
        let cx = (size as f32) * 0.5;
        let cy = (size as f32) * 0.5;
        let outer_r = (size as f32) * 0.45;
        let inner_r = (size as f32) * 0.14;
        for y in 0..size {
            for x in 0..size {
                let dx = (x as f32) - cx;
                let dy = (y as f32) - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist <= inner_r {
                    img.put_pixel(x, y, Rgba([34, 33, 30, 255]));
                } else if dist <= outer_r {
                    img.put_pixel(x, y, Rgba([0, 92, 214, 255]));
                }
            }
        }
        img
    }

    fn synthesize_gray_star(size: u32, bg_gray: u8, star_white: u8) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(size, size, Rgba([bg_gray, bg_gray, bg_gray, 255]));
        // Draw a 5-point star centered at (0.5, 0.5) matching Navidrome / Last.fm geometry.
        let cx = (size as f32) * 0.5;
        let cy = (size as f32) * 0.5;
        let outer_r = (size as f32) * 0.36;
        let inner_r = (size as f32) * 0.15;
        let mut verts: [(f32, f32); 10] = [(0.0, 0.0); 10];
        for (i, v) in verts.iter_mut().enumerate() {
            let angle = -PI / 2.0 + (i as f32) * PI / 5.0;
            let r = if i % 2 == 0 { outer_r } else { inner_r };
            *v = (cx + r * angle.cos(), cy + r * angle.sin());
        }
        for y in 0..size {
            for x in 0..size {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let mut inside = false;
                let mut j = verts.len() - 1;
                for i in 0..verts.len() {
                    let (xi, yi) = verts[i];
                    let (xj, yj) = verts[j];
                    let intersects = ((yi > py) != (yj > py))
                        && (px < (xj - xi) * (py - yi) / (yj - yi) + xi);
                    if intersects {
                        inside = !inside;
                    }
                    j = i;
                }
                if inside {
                    img.put_pixel(x, y, Rgba([star_white, star_white, star_white, 255]));
                }
            }
        }
        img
    }

    #[test]
    fn detects_resized_blue_vinyl_jpeg_and_transparent_png() {
        for size in [320u32, 512u32] {
            let black_matte = synthesize_blue_vinyl(size, [0, 0, 0, 255]);
            let black_jpeg = encode_image(&black_matte, ImageFormat::Jpeg);
            assert!(
                is_navidrome_placeholder_bytes(&black_jpeg),
                "expected {size}x{size} black-matte blue vinyl JPEG to be rejected"
            );

            let white_matte = synthesize_blue_vinyl(size, [255, 255, 255, 255]);
            let white_jpeg = encode_image(&white_matte, ImageFormat::Jpeg);
            assert!(
                is_navidrome_placeholder_bytes(&white_jpeg),
                "expected {size}x{size} white-matte blue vinyl JPEG to be rejected"
            );
        }

        let transparent = synthesize_blue_vinyl(256, [0, 0, 0, 0]);
        let png = encode_image(&transparent, ImageFormat::Png);
        assert!(is_navidrome_placeholder_bytes(&png));
    }

    #[test]
    fn detects_navidrome_and_lastfm_gray_star_placeholders() {
        // Navidrome artist-placeholder style (235 gray + 255 white star)
        let navi_star = synthesize_gray_star(300, 235, 255);
        assert!(is_navidrome_placeholder_bytes(&encode_image(
            &navi_star,
            ImageFormat::Png
        )));
        assert!(is_navidrome_placeholder_bytes(&encode_image(
            &navi_star,
            ImageFormat::Jpeg
        )));

        // Last.fm generic gray star style (~198 gray + 245 white star)
        let lastfm_star = synthesize_gray_star(320, 198, 245);
        assert!(is_navidrome_placeholder_bytes(&encode_image(
            &lastfm_star,
            ImageFormat::Jpeg
        )));
    }

    #[test]
    fn accepts_normal_colorful_monochrome_portrait_and_non_square_art() {
        // Colorful 4-quadrant album art (2x2 playlist collage style)
        let mut collage = RgbaImage::new(320, 320);
        for y in 0..320 {
            for x in 0..320 {
                let color = match (x < 160, y < 160) {
                    (true, true) => [210, 60, 45, 255],
                    (false, true) => [45, 165, 110, 255],
                    (true, false) => [230, 180, 50, 255],
                    (false, false) => [120, 55, 175, 255],
                };
                collage.put_pixel(x, y, Rgba(color));
            }
        }
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &collage,
            ImageFormat::Jpeg
        )));

        // Solid bright blue cover (no dark vinyl spindle or matte corners)
        let solid_blue = RgbaImage::from_pixel(320, 320, Rgba([0, 92, 214, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &solid_blue,
            ImageFormat::Jpeg
        )));

        // Flat off-white / gray minimalist album cover (no bright white star contrast)
        let flat_gray = RgbaImage::from_pixel(320, 320, Rgba([225, 225, 225, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &flat_gray,
            ImageFormat::Jpeg
        )));

        // Monochrome album cover / portrait with light-gray background, bright center, AND dark
        // photographic features (must NOT false-positive as gray star placeholder).
        let mut mono_portrait = synthesize_gray_star(320, 225, 252);
        for y in 230..290 {
            for x in 100..220 {
                mono_portrait.put_pixel(x, y, Rgba([40, 40, 40, 255]));
            }
        }
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &mono_portrait,
            ImageFormat::Jpeg
        )));

        // Wide non-square banner rejected immediately at dimension-check stage.
        let banner = RgbaImage::from_pixel(400, 200, Rgba([235, 235, 235, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &banner,
            ImageFormat::Png
        )));
    }
}
