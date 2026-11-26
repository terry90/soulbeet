use std::collections::HashSet;
use std::sync::Arc;

use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::db::{
    get_cursor, now_unix, pending_outbox_song_ids, replace_playlist_tracks, set_cursor,
    upsert_album, upsert_artist, upsert_discovery_track, upsert_playlist, upsert_track,
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

/// Synchronizes local SQLite mirror from Soulbeet (`/api/client/bootstrap`, Discovery)
/// and Navidrome OpenSubsonic (`getArtists`, `getAlbumList2`, `getPlaylists`, `getStarred2`, `getGenres`).
#[derive(Clone)]
pub struct SyncEngine {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
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
        }
    }

    /// Calls `GET /api/client/bootstrap` on Soulbeet to refresh scoped OpenSubsonic credentials
    /// and persist server curation settings (e.g. `promote_threshold`).
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
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }
        let dto: ClientBootstrapDto = resp.json().await?;
        *self.navidrome.write().await = dto.navidrome.clone();
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
        let mut report = SyncReport::default();

        if self.soulbeet_base_url.is_some() {
            if let Err(err) = self.fetch_bootstrap().await {
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

        let pending_songs = pending_outbox_song_ids(&self.pool).await?;
        let now = now_unix();

        // 1. Artists
        let artists = subsonic.get_artists().await?;
        for artist in artists {
            upsert_artist(
                &self.pool,
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

        // 2. Albums + Tracks (full alphabetical pagination vs delta newest/recent)
        let mut newest_created = get_cursor(&self.pool, CURSOR_LAST_ALBUM_CREATED)
            .await?
            .unwrap_or_default();
        let previous_created = newest_created.clone();

        if full_sync {
            let mut offset = 0usize;
            loop {
                let page = subsonic
                    .get_album_list2("alphabeticalByName", ALBUM_PAGE_SIZE, offset)
                    .await?;
                let count = page.len();
                for summary in page {
                    if let Some(c) = &summary.created {
                        if is_timestamp_newer(c, &newest_created) {
                            newest_created = c.clone();
                        }
                    }
                    let detail = subsonic.get_album(&summary.id).await?;
                    report.tracks_synced +=
                        self.store_album_detail(&detail, &pending_songs, now).await?;
                    report.albums_synced += 1;
                }
                if count < ALBUM_PAGE_SIZE {
                    break;
                }
                offset += count;
            }
            set_cursor(&self.pool, CURSOR_LAST_FULL_SYNC, &now.to_string()).await?;
        } else {
            let mut seen_album_ids = HashSet::new();

            // Paginate `newest` until a page is shorter than DELTA_PAGE_SIZE or yields no new albums.
            let mut offset = 0usize;
            loop {
                let page = subsonic
                    .get_album_list2("newest", DELTA_PAGE_SIZE, offset)
                    .await?;
                let count = page.len();
                let mut page_had_new = false;
                for summary in page {
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
                    report.tracks_synced +=
                        self.store_album_detail(&detail, &pending_songs, now).await?;
                    report.albums_synced += 1;
                }
                if count < DELTA_PAGE_SIZE || !page_had_new {
                    break;
                }
                offset += count;
            }

            // Refresh recently played albums (single page) for updated play counts / metadata.
            let recent_page = subsonic.get_album_list2("recent", DELTA_PAGE_SIZE, 0).await?;
            for summary in recent_page {
                if !seen_album_ids.insert(summary.id.clone()) {
                    continue;
                }
                if let Some(c) = &summary.created {
                    if is_timestamp_newer(c, &newest_created) {
                        newest_created = c.clone();
                    }
                }
                let detail = subsonic.get_album(&summary.id).await?;
                report.tracks_synced +=
                    self.store_album_detail(&detail, &pending_songs, now).await?;
                report.albums_synced += 1;
            }
        }

        if !newest_created.is_empty() {
            set_cursor(&self.pool, CURSOR_LAST_ALBUM_CREATED, &newest_created).await?;
        }

        // 3. Playlists (delta via changed_at for static playlists; always refresh smart or 0-count playlists) + prune deleted playlists
        let playlists = subsonic.get_playlists().await?;
        let mut synced_playlist_ids = HashSet::with_capacity(playlists.len());
        for pl in playlists {
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

            upsert_playlist(
                &self.pool,
                &LocalPlaylist {
                    id: pl.id.clone(),
                    name: pl.name.clone(),
                    comment: pl.comment.clone(),
                    owner: pl.owner.clone(),
                    song_count: initial_song_count,
                    duration_secs: initial_duration_secs,
                    cover_art_id: pl.cover_art.clone(),
                    is_smart,
                    kind,
                    slug,
                    changed_at: pl.changed.clone(),
                    updated_at: now,
                },
            )
            .await?;

            if needs_tracks {
                let detail = subsonic.get_playlist(&pl.id).await?;
                let mut track_ids = Vec::with_capacity(detail.entry.len());
                let mut total_duration_secs: i64 = 0;
                for song in detail.entry {
                    total_duration_secs += song.duration.max(0);
                    let track_id = song.id.clone();
                    let preserve = pending_songs.contains(&track_id);
                    upsert_track(&self.pool, &song_to_local(song, now), preserve).await?;
                    track_ids.push(track_id);
                }
                replace_playlist_tracks(&self.pool, &pl.id, &track_ids).await?;
                let live_song_count = track_ids.len() as i64;
                let live_duration_secs = if total_duration_secs > 0 || track_ids.is_empty() {
                    total_duration_secs
                } else {
                    detail.duration
                };
                sqlx::query(
                    "UPDATE playlists SET song_count = ?, duration_secs = ?, updated_at = ? WHERE id = ?",
                )
                .bind(live_song_count)
                .bind(live_duration_secs)
                .bind(now)
                .bind(&pl.id)
                .execute(&self.pool)
                .await?;
            }
            report.playlists_synced += 1;
        }

        // Prune playlists that were deleted on Navidrome.
        let local_playlist_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM playlists")
            .fetch_all(&self.pool)
            .await?;
        let deleted_playlist_ids: Vec<String> = local_playlist_ids
            .into_iter()
            .filter(|id| !synced_playlist_ids.contains(id))
            .collect();
        if !deleted_playlist_ids.is_empty() {
            let mut tx = self.pool.begin().await?;
            for pl_id in deleted_playlist_ids {
                sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?")
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

        // 4. Starred reconciliation for songs, albums, and artists (protecting pending outbox edits)
        let starred = subsonic.get_starred2().await?;
        let starred_song_ids: HashSet<String> =
            starred.song.iter().map(|s| s.id.clone()).collect();
        for song in starred.song {
            let preserve = pending_songs.contains(&song.id);
            let mut local = song_to_local(song, now);
            local.starred = true;
            upsert_track(&self.pool, &local, preserve).await?;
            report.starred_reconciled += 1;
        }
        let locally_starred_songs: Vec<String> =
            sqlx::query_scalar("SELECT id FROM tracks WHERE starred = 1")
                .fetch_all(&self.pool)
                .await?;
        for id in locally_starred_songs {
            if !starred_song_ids.contains(&id) && !pending_songs.contains(&id) {
                sqlx::query("UPDATE tracks SET starred = 0, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        let starred_album_ids: HashSet<String> =
            starred.album.iter().map(|a| a.id.clone()).collect();
        for album_id in &starred_album_ids {
            sqlx::query("UPDATE albums SET starred = 1, updated_at = ? WHERE id = ?")
                .bind(now)
                .bind(album_id)
                .execute(&self.pool)
                .await?;
            report.starred_reconciled += 1;
        }
        let locally_starred_albums: Vec<String> =
            sqlx::query_scalar("SELECT id FROM albums WHERE starred = 1")
                .fetch_all(&self.pool)
                .await?;
        for id in locally_starred_albums {
            if !starred_album_ids.contains(&id) {
                sqlx::query("UPDATE albums SET starred = 0, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        let starred_artist_ids: HashSet<String> =
            starred.artist.iter().map(|a| a.id.clone()).collect();
        for artist_id in &starred_artist_ids {
            sqlx::query("UPDATE artists SET starred = 1, updated_at = ? WHERE id = ?")
                .bind(now)
                .bind(artist_id)
                .execute(&self.pool)
                .await?;
            report.starred_reconciled += 1;
        }
        let locally_starred_artists: Vec<String> =
            sqlx::query_scalar("SELECT id FROM artists WHERE starred = 1")
                .fetch_all(&self.pool)
                .await?;
        for id in locally_starred_artists {
            if !starred_artist_ids.contains(&id) {
                sqlx::query("UPDATE artists SET starred = 0, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        // 5. Genres snapshot
        match subsonic.get_genres().await {
            Ok(genres) => {
                let json = serde_json::to_string(&genres)?;
                set_cursor(&self.pool, CURSOR_GENRES_JSON, &json).await?;
            }
            Err(err) => report.warnings.push(format!("genres sync failed: {err}")),
        }

        // 6. Optional Soulbeet Discovery triage queue sync
        if let Some(base) = self.soulbeet_base_url.as_deref() {
            match self
                .sync_discovery_triage(base, &pending_songs, now)
                .await
            {
                Ok(n) => report.discovery_tracks_synced = n,
                Err(err) => report.warnings.push(format!("discovery sync failed: {err}")),
            }
        }

        Ok(report)
    }

    async fn store_album_detail(
        &self,
        detail: &SubsonicAlbumDetail,
        pending_songs: &HashSet<String>,
        now: i64,
    ) -> CoreResult<usize> {
        upsert_album(
            &self.pool,
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
            let preserve = pending_songs.contains(&song.id);
            upsert_track(&self.pool, &song_to_local(song.clone(), now), preserve).await?;
            synced += 1;
        }
        Ok(synced)
    }

    async fn sync_discovery_triage(
        &self,
        base: &str,
        pending_songs: &HashSet<String>,
        now: i64,
    ) -> CoreResult<usize> {
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

        for item in items {
            synced_ids.insert(item.id.clone());
            let preserve = item
                .song_id
                .as_deref()
                .is_some_and(|sid| pending_songs.contains(sid));
            let reason_json = item
                .reason
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let normalized_status = item.status.trim().to_ascii_lowercase();
            let album = item.album.filter(|a| !a.is_empty());
            let folder_path = item.path.filter(|p| !p.is_empty());
            let profile_slug = item.profile.filter(|p| !p.is_empty());

            upsert_discovery_track(
                &self.pool,
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
                preserve,
            )
            .await?;
        }

        // Prune stale discovery tracks no longer in server triage queue (protecting pending outbox edits).
        let local_rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, song_id FROM discovery_tracks")
                .fetch_all(&self.pool)
                .await?;
        for (id, song_id) in local_rows {
            if synced_ids.contains(&id) {
                continue;
            }
            if song_id
                .as_deref()
                .is_some_and(|sid| pending_songs.contains(sid))
            {
                continue;
            }
            sqlx::query("DELETE FROM discovery_tracks WHERE id = ?")
                .bind(&id)
                .execute(&self.pool)
                .await?;
            if let Some(sid) = song_id {
                let still_pending: i64 = sqlx::query_scalar(
                    "SELECT COUNT(1) FROM discovery_tracks WHERE song_id = ? AND status = 'pending'",
                )
                .bind(&sid)
                .fetch_one(&self.pool)
                .await?;
                if still_pending == 0 {
                    sqlx::query("UPDATE tracks SET is_discovery = 0, updated_at = ? WHERE id = ?")
                        .bind(now)
                        .bind(&sid)
                        .execute(&self.pool)
                        .await?;
                }
            }
        }

        Ok(count)
    }
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
    )
}

fn classify_playlist(name: &str, comment: Option<&str>) -> (String, Option<String>, bool) {
    let lower_name = name.to_ascii_lowercase();
    let lower_comment = comment.unwrap_or_default().to_ascii_lowercase();

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
        return ("discovery".to_string(), None, true);
    }

    if lower_comment.contains("seasonal:") || lower_name.contains("seasonal") {
        let slug = lower_comment
            .split("seasonal:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .map(ToString::to_string)
            .or_else(|| Some(slugify(&lower_name)));
        return ("seasonal".to_string(), slug, true);
    }
    if lower_name.contains("weekly radar") || lower_name.contains("discovery") {
        return ("discovery".to_string(), Some(slugify(&lower_name)), true);
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
            .or_else(|| Some(slugify(&lower_name)));
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
