use std::collections::HashSet;
use std::sync::Arc;

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
    NavidromeCredentials, SyncReport,
};
use crate::subsonic::{SubsonicAlbumDetail, SubsonicClient, SubsonicSong};

const CURSOR_LAST_FULL_SYNC: &str = "last_full_sync_unix";
const CURSOR_LAST_ALBUM_CREATED: &str = "last_album_created_at";
const CURSOR_GENRES_JSON: &str = "genres_json";
const ALBUM_PAGE_SIZE: usize = 250;

/// Synchronizes local SQLite mirror from Soulbeet (`/api/client/bootstrap`, Discovery)
/// and Navidrome OpenSubsonic (`getArtists`, `getAlbumList2`, `getPlaylists`, `getStarred2`, `getGenres`).
#[derive(Clone)]
pub struct SyncEngine {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth_header: Option<String>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
}

impl SyncEngine {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        soulbeet_base_url: Option<String>,
        soulbeet_auth_header: Option<String>,
        navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    ) -> Self {
        Self {
            pool,
            http,
            soulbeet_base_url,
            soulbeet_auth_header,
            navidrome,
        }
    }

    /// Calls `GET /api/client/bootstrap` on Soulbeet to refresh scoped OpenSubsonic credentials.
    pub async fn fetch_bootstrap(&self) -> CoreResult<Option<ClientBootstrapDto>> {
        let Some(base) = self.soulbeet_base_url.as_deref() else {
            return Ok(None);
        };
        let url = format!("{}/api/client/bootstrap", base.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(auth) = &self.soulbeet_auth_header {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
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
        if let Some(creds) = dto.navidrome.clone() {
            *self.navidrome.write().await = Some(creds);
        }
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
                        if c > &newest_created {
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
            for list_type in ["newest", "recent"] {
                let page = subsonic.get_album_list2(list_type, 50, 0).await?;
                for summary in page {
                    if !seen_album_ids.insert(summary.id.clone()) {
                        continue;
                    }
                    let created_newer = summary
                        .created
                        .as_deref()
                        .is_some_and(|c| c > previous_created.as_str());
                    let exists = sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(1) FROM albums WHERE id = ?",
                    )
                    .bind(&summary.id)
                    .fetch_one(&self.pool)
                    .await?
                        > 0;

                    if list_type == "newest" && !created_newer && exists {
                        continue;
                    }
                    if let Some(c) = &summary.created {
                        if c > &newest_created {
                            newest_created = c.clone();
                        }
                    }
                    let detail = subsonic.get_album(&summary.id).await?;
                    report.tracks_synced +=
                        self.store_album_detail(&detail, &pending_songs, now).await?;
                    report.albums_synced += 1;
                }
            }
        }

        if !newest_created.is_empty() {
            set_cursor(&self.pool, CURSOR_LAST_ALBUM_CREATED, &newest_created).await?;
        }

        // 3. Playlists (delta via changed_at + song_count)
        let playlists = subsonic.get_playlists().await?;
        for pl in playlists {
            let existing: Option<(Option<String>, i64)> = sqlx::query_as(
                "SELECT changed_at, song_count FROM playlists WHERE id = ?",
            )
            .bind(&pl.id)
            .fetch_optional(&self.pool)
            .await?;

            let needs_tracks = match existing {
                Some((prev_changed, prev_count)) => {
                    full_sync || prev_changed != pl.changed || prev_count != pl.song_count
                }
                None => true,
            };

            let (kind, slug, is_smart) = classify_playlist(&pl.name, pl.comment.as_deref());
            upsert_playlist(
                &self.pool,
                &LocalPlaylist {
                    id: pl.id.clone(),
                    name: pl.name.clone(),
                    comment: pl.comment.clone(),
                    owner: pl.owner.clone(),
                    song_count: pl.song_count,
                    duration_secs: pl.duration,
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
                for song in detail.entry {
                    let track_id = song.id.clone();
                    let preserve = pending_songs.contains(&track_id);
                    upsert_track(&self.pool, &song_to_local(song, now), preserve).await?;
                    track_ids.push(track_id);
                }
                replace_playlist_tracks(&self.pool, &pl.id, &track_ids).await?;
            }
            report.playlists_synced += 1;
        }

        // 4. Starred reconciliation (protecting pending outbox edits)
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
        // Clear stale stars on tracks that are neither currently starred nor pending in outbox
        let locally_starred: Vec<String> =
            sqlx::query_scalar("SELECT id FROM tracks WHERE starred = 1")
                .fetch_all(&self.pool)
                .await?;
        for id in locally_starred {
            if !starred_song_ids.contains(&id) && !pending_songs.contains(&id) {
                sqlx::query("UPDATE tracks SET starred = 0, updated_at = ? WHERE id = ?")
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
            match self.sync_discovery_triage(base, now).await {
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

    async fn sync_discovery_triage(&self, base: &str, now: i64) -> CoreResult<usize> {
        let url = format!("{}/api/discovery/triage", base.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(auth) = &self.soulbeet_auth_header {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(CoreError::ServerHttp {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }

        #[derive(Deserialize)]
        struct TriageItem {
            id: String,
            #[serde(default)]
            song_id: Option<String>,
            artist: String,
            title: String,
            #[serde(default)]
            album: Option<String>,
            #[serde(default)]
            folder_path: Option<String>,
            #[serde(default)]
            profile_slug: Option<String>,
            #[serde(default)]
            mix_slug: Option<String>,
            #[serde(default = "default_pending")]
            status: String,
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
        for item in items {
            let reason_json = item
                .reason
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            upsert_discovery_track(
                &self.pool,
                &LocalDiscoveryTrack {
                    id: item.id,
                    song_id: item.song_id,
                    artist: item.artist,
                    title: item.title,
                    album: item.album,
                    folder_path: item.folder_path,
                    profile_slug: item.profile_slug,
                    mix_slug: item.mix_slug,
                    status: item.status,
                    expires_at: item.expires_at,
                    reason_json,
                    exclude_from_core_taste: item.exclude_from_core_taste,
                    updated_at: now,
                },
            )
            .await?;
        }
        Ok(count)
    }
}

fn song_to_local(song: SubsonicSong, now: i64) -> LocalTrack {
    let is_discovery = song
        .path
        .as_deref()
        .is_some_and(|p| p.contains("Discovery/") || p.contains("Discovery\\"));
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

fn classify_playlist(name: &str, comment: Option<&str>) -> (String, Option<String>, bool) {
    let lower_name = name.to_ascii_lowercase();
    let lower_comment = comment.unwrap_or_default().to_ascii_lowercase();
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
        || lower_name.contains("late night")
        || lower_name.contains("morning boost")
    {
        return ("moment".to_string(), Some(slugify(&lower_name)), false);
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
