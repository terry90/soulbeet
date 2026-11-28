use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local, Timelike};
use reqwest::Client;
use serde::Deserialize;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::db::{
    get_cursor, now_unix, replace_playlist_tracks_tx, set_cursor, upsert_album_exec,
    upsert_artist_exec, upsert_discovery_track_tx, upsert_playlist_exec, upsert_track_exec,
};
use crate::error::{CoreError, CoreResult};
use crate::models::{
    ClientBootstrapDto, LocalAlbum, LocalArtist, LocalDiscoveryTrack, LocalPlaylist, LocalTrack,
    NavidromeCredentials, SoulbeetAuth, SyncReport,
};
use crate::subsonic::{SubsonicAlbumDetail, SubsonicClient, SubsonicSong};

const CURSOR_LAST_FULL_SYNC: &str = "last_full_sync_unix";
const CURSOR_LAST_ALBUM_CREATED: &str = "last_album_created_at";
const CURSOR_GENRES_JSON: &str = "genres_json";
pub const CURSOR_PROMOTE_THRESHOLD: &str = "promote_threshold";
const ALBUM_PAGE_SIZE: usize = 250;
const DELTA_PAGE_SIZE: usize = 50;
const RATING_SYNC_PAGE_SIZE: usize = 500;

/// Synchronizes local SQLite mirror from Soulbeet (`/api/client/bootstrap`, Discovery)
/// and Navidrome OpenSubsonic (`getArtists`, `getAlbumList2`, `getPlaylists`, `getStarred2`, `getGenres`).
#[derive(Clone)]
pub struct SyncEngine {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    sync_lock: Arc<TokioMutex<()>>,
}

impl SyncEngine {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        soulbeet_base_url: Option<String>,
        soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
        navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    ) -> Self {
        Self {
            pool,
            http,
            soulbeet_base_url,
            soulbeet_auth,
            navidrome,
            sync_lock: Arc::new(TokioMutex::new(())),
        }
    }

    /// Calls `GET /api/client/bootstrap` on Soulbeet to refresh scoped OpenSubsonic credentials
    /// and persist server curation settings (e.g. `promote_threshold`).
    ///
    /// Preserves existing cached `navidrome` credentials if bootstrap transiently omits them
    /// (`navidrome: null`), clearing them only on explicit auth rejection (HTTP 401/403).
    pub async fn fetch_bootstrap(&self) -> CoreResult<Option<ClientBootstrapDto>> {
        let Some(base) = self.soulbeet_base_url.as_deref() else {
            return Ok(None);
        };
        let url = format!("{}/api/client/bootstrap", base.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            req = req.header(name, val);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            if matches!(status_code, 401 | 403) {
                *self.navidrome.write().await = None;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status_code,
                body,
            });
        }
        let dto: ClientBootstrapDto = resp.json().await?;
        if let Some(creds) = dto.navidrome.clone() {
            *self.navidrome.write().await = Some(creds);
        } else if self.navidrome.read().await.is_some() {
            tracing::warn!(
                "bootstrap returned navidrome=None; preserving existing cached Navidrome credentials"
            );
        }
        set_cursor(
            &self.pool,
            CURSOR_PROMOTE_THRESHOLD,
            &dto.promote_threshold.to_string(),
        )
        .await?;
        Ok(Some(dto))
    }

    /// Performs an initial full sync or incremental delta sync against Navidrome + Soulbeet.
    pub async fn sync(&self, force_full: bool) -> CoreResult<SyncReport> {
        let never = AtomicBool::new(false);
        self.sync_cancellable(force_full, &never).await
    }

    /// Cooperatively cancellable variant of [`Self::sync`], serialized via `sync_lock`.
    /// Checks `cancelled` between network pages and database transactions so partial transactions
    /// or premature full-sync pruning never occur.
    pub async fn sync_cancellable(
        &self,
        force_full: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<SyncReport> {
        let _sync_guard = self.sync_lock.lock().await;
        let result = self.sync_locked(force_full, cancelled).await;
        if let Err(CoreError::Subsonic { code: 40, .. }) = &result {
            *self.navidrome.write().await = None;
        }
        result
    }

    async fn sync_locked(
        &self,
        force_full: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<SyncReport> {
        let mut report = SyncReport::default();

        if cancelled.load(Ordering::Relaxed) {
            return Ok(report);
        }

        if self.soulbeet_base_url.is_some() {
            if let Err(err) = self.fetch_bootstrap().await {
                if matches!(err, CoreError::ServerHttp { status: 401 | 403, .. }) {
                    return Err(err);
                }
                report.warnings.push(format!("bootstrap failed: {err}"));
            }
        }

        let creds = self
            .navidrome
            .read()
            .await
            .clone()
            .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
        let subsonic = SubsonicClient::new(self.http.clone(), creds);

        let last_full = get_cursor(&self.pool, CURSOR_LAST_FULL_SYNC).await?;
        let full_sync = force_full || last_full.is_none();
        report.full_sync = full_sync;

        let now = now_unix();

        // 1. Artists (batched in one transaction)
        if cancelled.load(Ordering::Relaxed) {
            return Ok(report);
        }
        let artists = subsonic.get_artists().await?;
        let mut synced_artist_ids = HashSet::with_capacity(artists.len());
        {
            let mut tx = self.pool.begin().await?;
            for artist in artists {
                synced_artist_ids.insert(artist.id.clone());
                upsert_artist_exec(
                    &mut *tx,
                    &LocalArtist {
                        id: artist.id,
                        name: artist.name,
                        album_count: artist.album_count,
                        cover_art_id: artist.cover_art,
                        starred: artist.starred.is_some(),
                        musicbrainz_id: artist.music_brainz_id,
                        updated_at: now,
                    },
                )
                .await?;
                report.artists_synced += 1;
            }
            tx.commit().await?;
        }

        // 2. Albums + Tracks (full alphabetical pagination vs delta newest/recent)
        let mut newest_created = get_cursor(&self.pool, CURSOR_LAST_ALBUM_CREATED)
            .await?
            .unwrap_or_default();
        let previous_created = newest_created.clone();
        let mut synced_album_ids: HashSet<String> = HashSet::new();
        let mut synced_track_ids: HashSet<String> = HashSet::new();

        if full_sync {
            let mut offset = 0usize;
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(report);
                }
                let page = subsonic
                    .get_album_list2("alphabeticalByName", ALBUM_PAGE_SIZE, offset)
                    .await?;
                let count = page.len();
                let mut page_details = Vec::with_capacity(count);
                for summary in page {
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(report);
                    }
                    if let Some(c) = &summary.created {
                        if is_timestamp_newer(c, &newest_created) {
                            newest_created = c.clone();
                        }
                    }
                    let detail = subsonic.get_album(&summary.id).await?;
                    page_details.push(detail);
                }
                if !page_details.is_empty() {
                    let mut tx = self.pool.begin().await?;
                    for detail in &page_details {
                        synced_album_ids.insert(detail.id.clone());
                        for song in &detail.song {
                            synced_track_ids.insert(song.id.clone());
                        }
                        report.tracks_synced +=
                            store_album_detail_tx(&mut tx, detail, now).await?;
                        report.albums_synced += 1;
                    }
                    tx.commit().await?;
                }
                if count < ALBUM_PAGE_SIZE {
                    break;
                }
                offset += count;
            }
        } else {
            let mut seen_album_ids = HashSet::new();

            // Paginate `newest` until a page is shorter than DELTA_PAGE_SIZE or yields no new albums.
            let mut offset = 0usize;
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(report);
                }
                let page = subsonic
                    .get_album_list2("newest", DELTA_PAGE_SIZE, offset)
                    .await?;
                let count = page.len();
                let mut page_had_new = false;
                let mut page_details = Vec::new();
                for summary in page {
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(report);
                    }
                    if !seen_album_ids.insert(summary.id.clone()) {
                        continue;
                    }
                    let created_newer = summary
                        .created
                        .as_deref()
                        .is_some_and(|c| is_timestamp_newer(c, &previous_created));
                    let exists = sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(1) FROM albums WHERE id = ?",
                    )
                    .bind(&summary.id)
                    .fetch_one(&self.pool)
                    .await?
                        > 0;

                    if !created_newer && exists {
                        continue;
                    }
                    page_had_new = true;
                    if let Some(c) = &summary.created {
                        if is_timestamp_newer(c, &newest_created) {
                            newest_created = c.clone();
                        }
                    }
                    let detail = subsonic.get_album(&summary.id).await?;
                    page_details.push(detail);
                }
                if !page_details.is_empty() {
                    let mut tx = self.pool.begin().await?;
                    for detail in &page_details {
                        report.tracks_synced +=
                            store_album_detail_tx(&mut tx, detail, now).await?;
                        report.albums_synced += 1;
                    }
                    tx.commit().await?;
                }
                if count < DELTA_PAGE_SIZE || !page_had_new {
                    break;
                }
                offset += count;
            }

            // Refresh recently played albums (single page) for updated play counts / metadata.
            if !cancelled.load(Ordering::Relaxed) {
                let recent_page = subsonic.get_album_list2("recent", DELTA_PAGE_SIZE, 0).await?;
                let mut recent_details = Vec::new();
                for summary in recent_page {
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(report);
                    }
                    if !seen_album_ids.insert(summary.id.clone()) {
                        continue;
                    }
                    if let Some(c) = &summary.created {
                        if is_timestamp_newer(c, &newest_created) {
                            newest_created = c.clone();
                        }
                    }
                    let detail = subsonic.get_album(&summary.id).await?;
                    recent_details.push(detail);
                }
                if !recent_details.is_empty() {
                    let mut tx = self.pool.begin().await?;
                    for detail in &recent_details {
                        report.tracks_synced +=
                            store_album_detail_tx(&mut tx, detail, now).await?;
                        report.albums_synced += 1;
                    }
                    tx.commit().await?;
                }
            }
        }

        if !newest_created.is_empty() {
            set_cursor(&self.pool, CURSOR_LAST_ALBUM_CREATED, &newest_created).await?;
        }

        // 3. Playlists (delta via changed_at for static playlists; always refresh smart or 0-count playlists) + prune deleted playlists
        if cancelled.load(Ordering::Relaxed) {
            return Ok(report);
        }
        let playlists = subsonic.get_playlists().await?;
        let mut synced_playlist_ids = HashSet::with_capacity(playlists.len());
        for pl in playlists {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(report);
            }
            synced_playlist_ids.insert(pl.id.clone());
            let (kind, slug, is_smart) = classify_playlist(&pl.name, pl.comment.as_deref());
            let existing: Option<(Option<String>, i64, i64)> = sqlx::query_as(
                "SELECT changed_at, song_count, duration_secs FROM playlists WHERE id = ?",
            )
            .bind(&pl.id)
            .fetch_optional(&self.pool)
            .await?;

            let needs_tracks = match &existing {
                Some((prev_changed, _, _)) => {
                    full_sync
                        || is_smart
                        || pl.song_count == 0
                        || *prev_changed != pl.changed
                }
                None => true,
            };

            let (initial_song_count, initial_duration_secs) = match &existing {
                Some((_, prev_count, prev_dur)) => (*prev_count, *prev_dur),
                None => (pl.song_count, pl.duration),
            };

            let display_name = if (kind == "seasonal" || kind == "moment")
                && pl.name.starts_with("Soulbeet: ")
            {
                slug.as_deref()
                    .map(|s| shared::recommendation::canonical_mix_title(s, None, None))
                    .unwrap_or_else(|| pl.name.clone())
            } else {
                pl.name.clone()
            };

            let detail_opt = if needs_tracks {
                Some(subsonic.get_playlist(&pl.id).await?)
            } else {
                None
            };

            let mut tx = self.pool.begin().await?;
            let normalized_cover_art = pl
                .cover_art
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            upsert_playlist_exec(
                &mut *tx,
                &LocalPlaylist {
                    id: pl.id.clone(),
                    name: display_name,
                    comment: pl.comment.clone(),
                    owner: pl.owner.clone(),
                    song_count: initial_song_count,
                    duration_secs: initial_duration_secs,
                    cover_art_id: normalized_cover_art.clone(),
                    is_smart,
                    kind,
                    slug,
                    changed_at: pl.changed.clone(),
                    updated_at: now,
                },
            )
            .await?;

            if let Some(detail) = detail_opt {
                let mut track_ids = Vec::with_capacity(detail.entry.len());
                let mut total_duration_secs: i64 = 0;
                let mut first_track_cover_art: Option<String> = None;
                for song in detail.entry {
                    total_duration_secs += song.duration.max(0);
                    let track_id = song.id.clone();
                    if full_sync {
                        synced_track_ids.insert(track_id.clone());
                    }
                    let local_song = song_to_local(song, now);
                    if first_track_cover_art.is_none() {
                        first_track_cover_art = local_song
                            .cover_art_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string);
                    }
                    upsert_track_exec(&mut *tx, &local_song, false).await?;
                    track_ids.push(track_id);
                }
                replace_playlist_tracks_tx(&mut tx, &pl.id, &track_ids).await?;
                let live_song_count = track_ids.len() as i64;
                let live_duration_secs = if total_duration_secs > 0 || track_ids.is_empty() {
                    total_duration_secs
                } else {
                    detail.duration
                };
                let effective_cover_art = normalized_cover_art.or(first_track_cover_art);
                sqlx::query(
                    r#"
                    UPDATE playlists
                    SET song_count = ?,
                        duration_secs = ?,
                        cover_art_id = COALESCE(NULLIF(TRIM(cover_art_id), ''), ?),
                        updated_at = ?
                    WHERE id = ?
                    "#,
                )
                .bind(live_song_count)
                .bind(live_duration_secs)
                .bind(&effective_cover_art)
                .bind(now)
                .bind(&pl.id)
                .execute(&mut *tx)
                .await?;
            } else if normalized_cover_art.is_none() {
                let first_track_cover_art: Option<String> = sqlx::query_scalar(
                    r#"
                    SELECT t.cover_art_id
                    FROM playlist_tracks pt
                    INNER JOIN tracks t ON t.id = pt.track_id
                    WHERE pt.playlist_id = ?
                      AND t.cover_art_id IS NOT NULL
                      AND TRIM(t.cover_art_id) != ''
                    ORDER BY pt.position ASC
                    LIMIT 1
                    "#,
                )
                .bind(&pl.id)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(fallback_art) = first_track_cover_art {
                    sqlx::query(
                        r#"
                        UPDATE playlists
                        SET cover_art_id = COALESCE(NULLIF(TRIM(cover_art_id), ''), ?),
                            updated_at = ?
                        WHERE id = ?
                        "#,
                    )
                    .bind(&fallback_art)
                    .bind(now)
                    .bind(&pl.id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            tx.commit().await?;
            report.playlists_synced += 1;
        }

        // Prune playlists that were deleted on Navidrome (preserve synthetic contextual mixes `mix:*`).
        let local_playlist_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM playlists")
            .fetch_all(&self.pool)
            .await?;
        let deleted_playlist_ids: Vec<String> = local_playlist_ids
            .into_iter()
            .filter(|id| !id.starts_with("mix:") && !synced_playlist_ids.contains(id))
            .collect();
        if !deleted_playlist_ids.is_empty() {
            let mut tx = self.pool.begin().await?;
            for pl_id in deleted_playlist_ids {
                sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?")
                    .bind(&pl_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "DELETE FROM offline_pins WHERE target_kind = 'playlist' AND target_id = ?",
                )
                .bind(&pl_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM playlists WHERE id = ?")
                    .bind(&pl_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
        }

        // 4. Starred reconciliation for songs, albums, and artists (atomic SQL NOT EXISTS outbox protection)
        if cancelled.load(Ordering::Relaxed) {
            return Ok(report);
        }
        let starred = subsonic.get_starred2().await?;
        let starred_song_ids: HashSet<String> =
            starred.song.iter().map(|s| s.id.clone()).collect();
        let starred_album_ids: HashSet<String> =
            starred.album.iter().map(|a| a.id.clone()).collect();
        let starred_artist_ids: HashSet<String> =
            starred.artist.iter().map(|a| a.id.clone()).collect();
        {
            let mut tx = self.pool.begin().await?;
            for song in starred.song {
                if full_sync {
                    synced_track_ids.insert(song.id.clone());
                }
                let mut local = song_to_local(song, now);
                local.starred = true;
                upsert_track_exec(&mut *tx, &local, false).await?;
                report.starred_reconciled += 1;
            }
            let locally_starred_songs: Vec<String> =
                sqlx::query_scalar("SELECT id FROM tracks WHERE starred = 1")
                    .fetch_all(&mut *tx)
                    .await?;
            for id in locally_starred_songs {
                if !starred_song_ids.contains(&id) {
                    sqlx::query(
                        r#"
                        UPDATE tracks
                        SET starred = 0, updated_at = ?
                        WHERE id = ?
                          AND NOT EXISTS (
                              SELECT 1 FROM mutation_outbox
                              WHERE song_id = tracks.id AND dead_letter = 0
                          )
                        "#,
                    )
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                }
            }

            for album_id in &starred_album_ids {
                sqlx::query("UPDATE albums SET starred = 1, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(album_id)
                    .execute(&mut *tx)
                    .await?;
                report.starred_reconciled += 1;
            }
            let locally_starred_albums: Vec<String> =
                sqlx::query_scalar("SELECT id FROM albums WHERE starred = 1")
                    .fetch_all(&mut *tx)
                    .await?;
            for id in locally_starred_albums {
                if !starred_album_ids.contains(&id) {
                    sqlx::query("UPDATE albums SET starred = 0, updated_at = ? WHERE id = ?")
                        .bind(now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }

            for artist_id in &starred_artist_ids {
                sqlx::query("UPDATE artists SET starred = 1, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(artist_id)
                    .execute(&mut *tx)
                    .await?;
                report.starred_reconciled += 1;
            }
            let locally_starred_artists: Vec<String> =
                sqlx::query_scalar("SELECT id FROM artists WHERE starred = 1")
                    .fetch_all(&mut *tx)
                    .await?;
            for id in locally_starred_artists {
                if !starred_artist_ids.contains(&id) {
                    sqlx::query("UPDATE artists SET starred = 0, updated_at = ? WHERE id = ?")
                        .bind(now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            tx.commit().await?;
        }

        // 4a. Optional Soulbeet contextual mixes sync (Drive Mix, Seasonals, Moments) runs BEFORE
        // full-sync pruning so contextual-only tracks are added to `synced_track_ids` and their
        // offline cached audio is never deleted during a force-full sync.
        if !cancelled.load(Ordering::Relaxed) {
            if let Some(base) = self.soulbeet_base_url.as_deref() {
                match self.sync_contextual_mixes(base, now, cancelled).await {
                    Ok(ctx_track_ids) => {
                        if full_sync {
                            synced_track_ids.extend(ctx_track_ids);
                        }
                    }
                    Err(err) => report
                        .warnings
                        .push(format!("contextual mixes sync failed: {err}")),
                }
            }
        }

        // 4b. Prune deleted tracks, albums, and artists after a complete full sync.
        if full_sync && !cancelled.load(Ordering::Relaxed) {
            self.prune_absent_library_rows(&synced_artist_ids, &synced_album_ids, &synced_track_ids)
                .await?;
            set_cursor(&self.pool, CURSOR_LAST_FULL_SYNC, &now.to_string()).await?;
        }

        // 4c. Incremental rating/annotation refresh via paginated search3 (empty query)
        // Delta sync skips unchanged albums, so tracks rated in Navidrome would otherwise keep stale user_rating.
        if !full_sync && !cancelled.load(Ordering::Relaxed) {
            if let Err(err) = self.sync_song_ratings(&subsonic, now, cancelled).await {
                report
                    .warnings
                    .push(format!("rating sync failed: {err}"));
            }
        }

        // 5. Genres snapshot
        if !cancelled.load(Ordering::Relaxed) {
            match subsonic.get_genres().await {
                Ok(genres) => {
                    let json = serde_json::to_string(&genres)?;
                    set_cursor(&self.pool, CURSOR_GENRES_JSON, &json).await?;
                }
                Err(err) => report.warnings.push(format!("genres sync failed: {err}")),
            }
        }

        // 6. Optional Soulbeet Discovery triage queue sync
        if !cancelled.load(Ordering::Relaxed) {
            if let Some(base) = self.soulbeet_base_url.as_deref() {
                match self.sync_discovery_triage(base, now).await {
                    Ok(n) => report.discovery_tracks_synced = n,
                    Err(err) => report.warnings.push(format!("discovery sync failed: {err}")),
                }
            }
        }

        Ok(report)
    }

    async fn prune_absent_library_rows(
        &self,
        synced_artist_ids: &HashSet<String>,
        synced_album_ids: &HashSet<String>,
        synced_track_ids: &HashSet<String>,
    ) -> CoreResult<()> {
        let mut tx = self.pool.begin().await?;
        let mut orphan_audio_paths: Vec<String> = Vec::new();

        let local_track_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM tracks")
            .fetch_all(&mut *tx)
            .await?;
        for tid in local_track_ids {
            if !synced_track_ids.contains(&tid) {
                sqlx::query("DELETE FROM playlist_tracks WHERE track_id = ?")
                    .bind(&tid)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "DELETE FROM offline_pins WHERE target_kind = 'track' AND target_id = ?",
                )
                .bind(&tid)
                .execute(&mut *tx)
                .await?;
                if let Some(local_path) = sqlx::query_scalar::<_, String>(
                    "SELECT local_path FROM offline_downloads WHERE track_id = ?",
                )
                .bind(&tid)
                .fetch_optional(&mut *tx)
                .await?
                {
                    orphan_audio_paths.push(local_path);
                    sqlx::query("DELETE FROM offline_downloads WHERE track_id = ?")
                        .bind(&tid)
                        .execute(&mut *tx)
                        .await?;
                }
                sqlx::query("DELETE FROM tracks WHERE id = ?")
                    .bind(&tid)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        let local_album_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM albums")
            .fetch_all(&mut *tx)
            .await?;
        for aid in local_album_ids {
            if !synced_album_ids.contains(&aid) {
                sqlx::query(
                    "DELETE FROM offline_pins WHERE target_kind = 'album' AND target_id = ?",
                )
                .bind(&aid)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM albums WHERE id = ?")
                    .bind(&aid)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        let local_artist_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM artists")
            .fetch_all(&mut *tx)
            .await?;
        for arid in local_artist_ids {
            if !synced_artist_ids.contains(&arid) {
                sqlx::query(
                    "DELETE FROM offline_pins WHERE target_kind = 'artist' AND target_id = ?",
                )
                .bind(&arid)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM artists WHERE id = ?")
                    .bind(&arid)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        // Sweep any pre-existing orphaned offline_downloads or offline_pins whose target
        // rows no longer exist in the mirrored library.
        let dangling_paths: Vec<String> = sqlx::query_scalar(
            "SELECT local_path FROM offline_downloads WHERE track_id NOT IN (SELECT id FROM tracks)",
        )
        .fetch_all(&mut *tx)
        .await?;
        orphan_audio_paths.extend(dangling_paths);
        sqlx::query("DELETE FROM offline_downloads WHERE track_id NOT IN (SELECT id FROM tracks)")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            DELETE FROM offline_pins
            WHERE (target_kind = 'track' AND target_id NOT IN (SELECT id FROM tracks))
               OR (target_kind = 'album' AND target_id NOT IN (SELECT id FROM albums))
               OR (target_kind = 'artist' AND target_id NOT IN (SELECT id FROM artists))
               OR (target_kind = 'playlist' AND target_id NOT IN (SELECT id FROM playlists))
            "#,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        for path in orphan_audio_paths {
            let _ = tokio::fs::remove_file(&path).await;
        }
        Ok(())
    }

    async fn sync_song_ratings(
        &self,
        subsonic: &SubsonicClient,
        now: i64,
        cancelled: &AtomicBool,
    ) -> CoreResult<usize> {
        let mut offset = 0usize;
        let mut remote_songs: Vec<(String, i64, bool)> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let page = subsonic
                .search3_songs("", RATING_SYNC_PAGE_SIZE, offset)
                .await?;
            let count = page.len();
            if count == 0 {
                break;
            }
            for song in page {
                if seen_ids.insert(song.id.clone()) {
                    remote_songs.push((
                        song.id,
                        song.user_rating.unwrap_or(0),
                        song.starred.is_some(),
                    ));
                }
            }
            if count < RATING_SYNC_PAGE_SIZE {
                break;
            }
            offset += count;
        }

        if cancelled.load(Ordering::Relaxed) {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        let local_rows: Vec<(String, i64, bool)> =
            sqlx::query_as("SELECT id, user_rating, starred FROM tracks")
                .fetch_all(&mut *tx)
                .await?;
        let local_map: HashMap<String, (i64, bool)> = local_rows
            .into_iter()
            .map(|(id, rating, starred)| (id, (rating, starred)))
            .collect();

        let mut updated = 0usize;

        for (id, remote_rating, remote_starred) in &remote_songs {
            if let Some(&(local_rating, local_starred)) = local_map.get(id) {
                if local_rating != *remote_rating || local_starred != *remote_starred {
                    let res = sqlx::query(
                        r#"
                        UPDATE tracks
                        SET user_rating = ?, starred = ?, updated_at = ?
                        WHERE id = ?
                          AND NOT EXISTS (
                              SELECT 1 FROM mutation_outbox
                              WHERE song_id = tracks.id AND dead_letter = 0
                          )
                        "#,
                    )
                    .bind(*remote_rating)
                    .bind(*remote_starred)
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    if res.rows_affected() > 0 {
                        updated += 1;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(updated)
    }

    async fn sync_discovery_triage(&self, base: &str, now: i64) -> CoreResult<usize> {
        let url = format!("{}/api/discovery/triage", base.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            req = req.header(name, val);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(CoreError::ServerHttp {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }

        /// Matches PR #129's `GET /api/discovery/triage` (`Vec<shared::navidrome::DiscoveryTrack>`).
        #[derive(Deserialize)]
        struct TriageItem {
            id: String,
            #[serde(default)]
            song_id: Option<String>,
            artist: String,
            title: String,
            #[serde(default)]
            album: Option<String>,
            #[serde(default, alias = "folder_path")]
            path: Option<String>,
            #[serde(default, alias = "profile_slug")]
            profile: Option<String>,
            #[serde(default, alias = "mix_slug")]
            pack_slug: Option<String>,
            #[serde(default)]
            rating: Option<u8>,
            #[serde(default = "default_pending")]
            status: String,
            #[serde(default)]
            created_at: Option<String>,
            #[serde(default)]
            expires_at: Option<String>,
            #[serde(default)]
            reason: Option<serde_json::Value>,
            #[serde(default)]
            exclude_from_core_taste: bool,
        }

        fn default_pending() -> String {
            "pending".to_string()
        }

        let items: Vec<TriageItem> = resp.json().await?;
        let count = items.len();
        let mut synced_ids = HashSet::with_capacity(count);
        let mut tx = self.pool.begin().await?;

        for item in items {
            synced_ids.insert(item.id.clone());
            let reason_json = item
                .reason
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let normalized_status = item.status.trim().to_ascii_lowercase();
            let album = item.album.filter(|a| !a.is_empty());
            let folder_path = item.path.filter(|p| !p.is_empty());
            let profile_slug = item.profile.filter(|p| !p.is_empty());
            let song_id_for_rating = item.song_id.clone();
            let rating = item.rating;

            upsert_discovery_track_tx(
                &mut tx,
                &LocalDiscoveryTrack {
                    id: item.id,
                    song_id: item.song_id,
                    artist: item.artist,
                    title: item.title,
                    album,
                    folder_path,
                    profile_slug,
                    mix_slug: item.pack_slug,
                    status: normalized_status,
                    created_at: item.created_at,
                    expires_at: item.expires_at,
                    reason_json,
                    exclude_from_core_taste: item.exclude_from_core_taste,
                    updated_at: now,
                },
                false,
            )
            .await?;

            if let (Some(sid), Some(r)) = (song_id_for_rating.as_deref(), rating) {
                sqlx::query(
                    r#"
                    UPDATE tracks
                    SET user_rating = ?, updated_at = ?
                    WHERE id = ?
                      AND NOT EXISTS (
                          SELECT 1 FROM mutation_outbox
                          WHERE song_id = tracks.id AND dead_letter = 0
                      )
                    "#,
                )
                .bind(i64::from(r.min(5)))
                .bind(now)
                .bind(sid)
                .execute(&mut *tx)
                .await?;
            }
        }

        // Prune stale discovery tracks no longer in server triage queue (protecting pending outbox edits).
        let local_rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, song_id FROM discovery_tracks")
                .fetch_all(&mut *tx)
                .await?;
        for (id, song_id) in local_rows {
            if synced_ids.contains(&id) {
                continue;
            }
            let res = sqlx::query(
                r#"
                DELETE FROM discovery_tracks
                WHERE id = ?
                  AND (
                      song_id IS NULL
                      OR NOT EXISTS (
                          SELECT 1 FROM mutation_outbox
                          WHERE song_id = discovery_tracks.song_id AND dead_letter = 0
                      )
                  )
                "#,
            )
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() > 0 {
                if let Some(sid) = song_id {
                    let still_pending: i64 = sqlx::query_scalar(
                        "SELECT COUNT(1) FROM discovery_tracks WHERE song_id = ? AND status = 'pending'",
                    )
                    .bind(&sid)
                    .fetch_one(&mut *tx)
                    .await?;
                    if still_pending == 0 {
                        sqlx::query(
                            r#"
                            UPDATE tracks
                            SET is_discovery = 0, updated_at = ?
                            WHERE id = ?
                              AND NOT EXISTS (
                                  SELECT 1 FROM mutation_outbox
                                  WHERE song_id = tracks.id AND dead_letter = 0
                              )
                            "#,
                        )
                        .bind(now)
                        .bind(&sid)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(count)
    }

    /// Syncs Soulbeet's active contextual mixes (including Drive Mix, seasonal packs, and moment
    /// mixes) into the local `playlists` mirror so Android Auto's "For You" tab has instant,
    /// zero-network access to Drive Mix and every active mix even when not materialized in Navidrome.
    /// Returns the set of track IDs referenced by active synthetic `mix:*` playlists so full-sync
    /// pruning preserves their cached rows and offline audio files.
    async fn sync_contextual_mixes(
        &self,
        base: &str,
        now: i64,
        cancelled: &AtomicBool,
    ) -> CoreResult<HashSet<String>> {
        let local_now = Local::now();
        let local_hour = local_now.hour();
        let local_weekday = local_now.weekday().number_from_monday();

        let url = format!(
            "{}/api/discovery/contextual-mixes?local_hour={local_hour}&local_weekday={local_weekday}",
            base.trim_end_matches('/')
        );
        let mut req = self.http.get(&url);
        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            req = req.header(name, val);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(HashSet::new());
        }
        if !resp.status().is_success() {
            return Err(CoreError::ServerHttp {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }

        let mixes: Vec<shared::recommendation::ContextualMixSummary> = resp.json().await?;
        // Record ALL active synthetic IDs upfront (before capping or fetching /playlist) so
        // transient network failures or pagination caps never delete cached offline `mix:*` rows.
        let mut active_synthetic_ids: HashSet<String> = mixes
            .iter()
            .map(|m| format!("mix:{}", m.slug))
            .collect();
        let mut protected_track_ids: HashSet<String> = HashSet::new();

        for mix in mixes.into_iter().take(12) {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let kind_str = match mix.kind {
                shared::recommendation::ContextualMixKind::Seasonal => "seasonal",
                shared::recommendation::ContextualMixKind::Moment => "moment",
                shared::recommendation::ContextualMixKind::Rolling => "discovery",
            };
            let title = mix
                .custom_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    shared::recommendation::canonical_mix_title(
                        &mix.slug,
                        mix.country_code.as_deref(),
                        None,
                    )
                });

            let synthetic_id = format!("mix:{}", mix.slug);

            if let Some(ref nav_pl_id) = mix.navidrome_playlist_id {
                // Enrich existing Navidrome playlist row with canonical kind, slug, and clean title.
                let res = sqlx::query(
                    "UPDATE playlists SET name = ?, kind = ?, slug = ?, updated_at = ? WHERE id = ?",
                )
                .bind(&title)
                .bind(kind_str)
                .bind(&mix.slug)
                .bind(now)
                .bind(nav_pl_id)
                .execute(&self.pool)
                .await?;
                if res.rows_affected() > 0 {
                    // Real Navidrome playlist exists locally; any older synthetic shadow can be pruned.
                    active_synthetic_ids.remove(&synthetic_id);
                    continue;
                }
            }

            // Ephemeral contextual mix (e.g. Drive Mix, Focus, Morning, Late Night, Weather):
            // materialize its tracklist once into synthetic playlist `mix:<slug>` so Android Auto
            // can browse and play it offline-first with zero network calls on tap.
            let should_materialize =
                mix.slug == "drive" || kind_str == "moment" || kind_str == "seasonal";
            if !should_materialize {
                active_synthetic_ids.remove(&synthetic_id);
                continue;
            }

            let pl_url = format!(
                "{}/api/discovery/contextual-mixes/playlist?slug={}",
                base.trim_end_matches('/'),
                url::form_urlencoded::byte_serialize(mix.slug.as_bytes()).collect::<String>()
            );
            let mut pl_req = self.http.get(&pl_url);
            if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
                let (name, val) = auth.header_name_and_value();
                pl_req = pl_req.header(name, val);
            }
            let pl_resp = match pl_req.send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                    active_synthetic_ids.remove(&synthetic_id);
                    continue;
                }
                _ => {
                    // Transient error: preserve any previously cached tracklist for this active mix.
                    continue;
                }
            };
            let Ok(materialized) = pl_resp
                .json::<shared::recommendation::ContextualMixPlaylist>()
                .await
            else {
                continue;
            };
            if materialized.tracks.is_empty() {
                active_synthetic_ids.remove(&synthetic_id);
                continue;
            }

            let mut tx = self.pool.begin().await?;
            let mut track_ids = Vec::with_capacity(materialized.tracks.len());
            let mut total_duration_secs: i64 = 0;

            for mt in materialized.tracks {
                let dur = i64::from(mt.duration_s.unwrap_or(0));
                total_duration_secs += dur;
                let exists: i64 =
                    sqlx::query_scalar("SELECT COUNT(1) FROM tracks WHERE id = ?")
                        .bind(&mt.song_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if exists == 0 {
                    upsert_track_exec(
                        &mut *tx,
                        &LocalTrack {
                            id: mt.song_id.clone(),
                            title: mt.title,
                            artist_id: None,
                            artist_name: mt.artist,
                            album_id: None,
                            album_name: mt.album.unwrap_or_default(),
                            track_number: None,
                            disc_number: None,
                            duration_secs: dur,
                            year: None,
                            genre: None,
                            bit_rate: None,
                            suffix: None,
                            content_type: None,
                            size_bytes: None,
                            cover_art_id: (!mt.song_id.trim().is_empty()).then(|| mt.song_id.clone()),
                            path: None,
                            starred: mt.starred,
                            user_rating: i64::from(mt.rating.unwrap_or(0)),
                            play_count: 0,
                            last_played_at: None,
                            musicbrainz_id: None,
                            is_discovery: !mt.owned,
                            updated_at: now,
                        },
                        false,
                    )
                    .await?;
                }
                protected_track_ids.insert(mt.song_id.clone());
                track_ids.push(mt.song_id);
            }

            let comment = if mix.offline_priority {
                Some(format!("soulbeet:contextual:{} offline_priority", mix.slug))
            } else {
                Some(format!("soulbeet:contextual:{}", mix.slug))
            };
            upsert_playlist_exec(
                &mut *tx,
                &LocalPlaylist {
                    id: synthetic_id.clone(),
                    name: title,
                    comment,
                    owner: None,
                    song_count: track_ids.len() as i64,
                    duration_secs: total_duration_secs,
                    cover_art_id: None,
                    is_smart: true,
                    kind: kind_str.to_string(),
                    slug: Some(mix.slug),
                    changed_at: None,
                    updated_at: now,
                },
            )
            .await?;
            replace_playlist_tracks_tx(&mut tx, &synthetic_id, &track_ids).await?;
            tx.commit().await?;
        }

        // Prune stale synthetic `mix:*` rows no longer active (including offline_pins cleanup).
        if !cancelled.load(Ordering::Relaxed) {
            let existing_mix_ids: Vec<String> =
                sqlx::query_scalar("SELECT id FROM playlists WHERE id LIKE 'mix:%'")
                    .fetch_all(&self.pool)
                    .await?;
            let stale: Vec<String> = existing_mix_ids
                .into_iter()
                .filter(|id| !active_synthetic_ids.contains(id))
                .collect();
            if !stale.is_empty() {
                let mut tx = self.pool.begin().await?;
                for id in stale {
                    sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?")
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(
                        "DELETE FROM offline_pins WHERE target_kind = 'playlist' AND target_id = ?",
                    )
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("DELETE FROM playlists WHERE id = ?")
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
        }

        // Also protect tracks of any preserved (not-pruned) synthetic `mix:*` playlists.
        let preserved_tracks: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT track_id FROM playlist_tracks WHERE playlist_id LIKE 'mix:%'",
        )
        .fetch_all(&self.pool)
        .await?;
        protected_track_ids.extend(preserved_tracks);

        Ok(protected_track_ids)
    }
}

async fn store_album_detail_tx(
    tx: &mut Transaction<'_, Sqlite>,
    detail: &SubsonicAlbumDetail,
    now: i64,
) -> CoreResult<usize> {
    upsert_album_exec(
        &mut **tx,
        &LocalAlbum {
            id: detail.id.clone(),
            name: detail.name.clone(),
            artist_id: detail.artist_id.clone(),
            artist_name: detail.artist.clone(),
            year: detail.year,
            genre: detail.genre.clone(),
            song_count: detail.song_count,
            duration_secs: detail.duration,
            cover_art_id: detail.cover_art.clone(),
            starred: detail.starred.is_some(),
            created_at: detail.created.clone(),
            updated_at: now,
        },
    )
    .await?;

    let mut synced = 0usize;
    for song in &detail.song {
        upsert_track_exec(&mut **tx, &song_to_local(song.clone(), now), false).await?;
        synced += 1;
    }
    Ok(synced)
}

fn is_timestamp_newer(candidate: &str, baseline: &str) -> bool {
    if baseline.is_empty() {
        return !candidate.is_empty();
    }
    match (
        DateTime::parse_from_rfc3339(candidate),
        DateTime::parse_from_rfc3339(baseline),
    ) {
        (Ok(c), Ok(b)) => c > b,
        _ => candidate > baseline,
    }
}

fn is_discovery_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    trimmed
        .get(.."discovery/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("discovery/"))
}

fn song_to_local(song: SubsonicSong, now: i64) -> LocalTrack {
    let is_discovery = song.path.as_deref().is_some_and(is_discovery_path);
    LocalTrack {
        id: song.id,
        title: song.title,
        artist_id: song.artist_id,
        artist_name: song.artist,
        album_id: song.album_id,
        album_name: song.album,
        track_number: song.track,
        disc_number: song.disc_number,
        duration_secs: song.duration,
        year: song.year,
        genre: song.genre,
        bit_rate: song.bit_rate,
        suffix: song.suffix,
        content_type: song.content_type,
        size_bytes: song.size,
        cover_art_id: song.cover_art,
        path: song.path,
        starred: song.starred.is_some(),
        user_rating: song.user_rating.unwrap_or(0),
        play_count: song.play_count.unwrap_or(0),
        last_played_at: None,
        musicbrainz_id: song.music_brainz_id,
        is_discovery,
        updated_at: now,
    }
}

fn is_moment_slug(slug: &str) -> bool {
    matches!(
        slug,
        "drive"
            | "drive-mix"
            | "drive_mix"
            | "morning"
            | "morning-boost"
            | "morning_boost"
            | "focus"
            | "focus-flow"
            | "focus_flow"
            | "late_night"
            | "late-night"
            | "evening_unwind"
            | "evening-unwind"
            | "friday_night"
            | "friday-night"
            | "sunday_morning"
            | "sunday-morning"
            | "dinner_groove"
            | "dinner-groove"
            | "rainy_day"
            | "snow_day"
            | "storm_night"
            | "sunny_day"
    )
}

fn infer_discovery_slug(lower_name: &str) -> Option<String> {
    if lower_name.contains("weekly radar")
        || lower_name.contains("fresh picks")
        || lower_name.contains("balanced")
    {
        Some("weekly_radar".to_string())
    } else if lower_name.contains("comfort") || lower_name.contains("conservative") {
        Some("comfort".to_string())
    } else if lower_name.contains("deep cuts") || lower_name.contains("adventurous") {
        Some("deep_cuts".to_string())
    } else if lower_name.contains("fresh arrivals") {
        Some("fresh_arrivals".to_string())
    } else {
        None
    }
}

fn is_canonical_winter_holiday_name(lower_name: &str) -> bool {
    let trimmed = lower_name.trim();
    let stripped = trimmed
        .strip_prefix("soulbeet:")
        .map(str::trim)
        .unwrap_or(trimmed);
    matches!(
        stripped,
        "christmas"
            | "christmas mix"
            | "winter holiday"
            | "winter holiday mix"
            | "holiday mix"
            | "noël"
            | "noel"
            | "noël mix"
            | "noel mix"
            | "noël à la française"
            | "noel a la francaise"
    )
}

pub fn classify_playlist(name: &str, comment: Option<&str>) -> (String, Option<String>, bool) {
    let lower_name = name.to_lowercase();
    let lower_comment = comment.unwrap_or_default().to_lowercase();

    if let Some(rest) = lower_comment.split("soulbeet:contextual:").nth(1) {
        let slug = rest
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| Some(slugify(&lower_name)));
        let kind = if slug.as_deref().is_some_and(is_moment_slug) {
            "moment"
        } else {
            "seasonal"
        };
        return (kind.to_string(), slug, true);
    }

    if lower_comment.contains("soulbeet discovery") {
        let slug = infer_discovery_slug(&lower_name).or_else(|| Some(slugify(&lower_name)));
        return ("discovery".to_string(), slug, true);
    }

    if lower_comment.contains("seasonal:") || lower_name.contains("seasonal") {
        let slug = lower_comment
            .split("seasonal:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .map(ToString::to_string)
            .or_else(|| {
                if is_canonical_winter_holiday_name(&lower_name) {
                    Some("winter_holiday".to_string())
                } else {
                    Some(slugify(&lower_name))
                }
            });
        return ("seasonal".to_string(), slug, true);
    }
    if lower_name.contains("weekly radar")
        || lower_name.contains("comfort mix")
        || lower_name.contains("comfort zone")
        || lower_name.contains("deep cuts")
        || lower_name.contains("fresh picks")
        || lower_name.contains("fresh arrivals")
        || lower_name.contains("discovery")
    {
        let slug = infer_discovery_slug(&lower_name).or_else(|| Some(slugify(&lower_name)));
        return ("discovery".to_string(), slug, true);
    }
    if is_canonical_winter_holiday_name(&lower_name) {
        return (
            "seasonal".to_string(),
            Some("winter_holiday".to_string()),
            true,
        );
    }
    if lower_comment.contains("moment:")
        || lower_name.contains("drive mix")
        || lower_name.contains("focus flow")
        || lower_name.contains("deep focus")
        || lower_name.contains("late night")
        || lower_name.contains("morning boost")
        || lower_name.contains("morning lift")
    {
        let slug = lower_comment
            .split("moment:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .map(ToString::to_string)
            .or_else(|| {
                if lower_name.contains("drive") {
                    Some("drive".to_string())
                } else {
                    Some(slugify(&lower_name))
                }
            });
        return ("moment".to_string(), slug, false);
    }
    if lower_comment.contains("smart") {
        return ("smart".to_string(), None, true);
    }
    ("user".to_string(), None, false)
}

fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}
