use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::error::CoreResult;
use crate::models::{
    LibraryStats, LocalAlbum, LocalArtist, LocalDiscoveryTrack, LocalPlaylist, LocalTrack,
    PersistedPlaybackSession,
};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens (and migrates) the local client SQLite database in WAL mode.
pub async fn open_pool(db_path: &Path) -> CoreResult<SqlitePool> {
    let is_memory = db_path == Path::new(":memory:");
    if !is_memory {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
    }

    let opts = if is_memory {
        SqliteConnectOptions::new().in_memory(true)
    } else {
        SqliteConnectOptions::new().filename(db_path)
    }
    .create_if_missing(true)
    .journal_mode(SqliteJournalMode::Wal)
    .synchronous(SqliteSynchronous::Normal)
    .busy_timeout(Duration::from_secs(5));

    let max_conns = if is_memory { 1 } else { 8 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_conns)
        .connect_with(opts)
        .await?;

    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

pub async fn get_cursor(pool: &SqlitePool, key: &str) -> CoreResult<Option<String>> {
    let value = sqlx::query_scalar::<_, String>("SELECT value FROM sync_cursor WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

pub async fn set_cursor(pool: &SqlitePool, key: &str, value: &str) -> CoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO sync_cursor (key, value, updated_at)
        VALUES (?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(key)
    .bind(value)
    .bind(now_unix())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_artist(pool: &SqlitePool, artist: &LocalArtist) -> CoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO artists (id, name, album_count, cover_art_id, starred, musicbrainz_id, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            album_count = excluded.album_count,
            cover_art_id = excluded.cover_art_id,
            starred = excluded.starred,
            musicbrainz_id = COALESCE(excluded.musicbrainz_id, artists.musicbrainz_id),
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&artist.id)
    .bind(&artist.name)
    .bind(artist.album_count)
    .bind(&artist.cover_art_id)
    .bind(artist.starred)
    .bind(&artist.musicbrainz_id)
    .bind(artist.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_album(pool: &SqlitePool, album: &LocalAlbum) -> CoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO albums (
            id, name, artist_id, artist_name, year, genre,
            song_count, duration_secs, cover_art_id, starred, created_at, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            artist_id = COALESCE(excluded.artist_id, albums.artist_id),
            artist_name = excluded.artist_name,
            year = COALESCE(excluded.year, albums.year),
            genre = COALESCE(excluded.genre, albums.genre),
            song_count = excluded.song_count,
            duration_secs = excluded.duration_secs,
            cover_art_id = COALESCE(excluded.cover_art_id, albums.cover_art_id),
            starred = excluded.starred,
            created_at = COALESCE(excluded.created_at, albums.created_at),
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&album.id)
    .bind(&album.name)
    .bind(&album.artist_id)
    .bind(&album.artist_name)
    .bind(album.year)
    .bind(&album.genre)
    .bind(album.song_count)
    .bind(album.duration_secs)
    .bind(&album.cover_art_id)
    .bind(album.starred)
    .bind(&album.created_at)
    .bind(album.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts or updates a track in the local mirror.
/// When `preserve_pending_curation` is true, existing local `starred` and `user_rating`
/// values are preserved so un-replayed offline outbox mutations are not clobbered by sync.
pub async fn upsert_track(
    pool: &SqlitePool,
    track: &LocalTrack,
    preserve_pending_curation: bool,
) -> CoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO tracks (
            id, title, artist_id, artist_name, album_id, album_name,
            track_number, disc_number, duration_secs, year, genre,
            bit_rate, suffix, content_type, size_bytes, cover_art_id, path,
            starred, user_rating, play_count, last_played_at, musicbrainz_id,
            is_discovery, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            artist_id = COALESCE(excluded.artist_id, tracks.artist_id),
            artist_name = excluded.artist_name,
            album_id = COALESCE(excluded.album_id, tracks.album_id),
            album_name = excluded.album_name,
            track_number = COALESCE(excluded.track_number, tracks.track_number),
            disc_number = COALESCE(excluded.disc_number, tracks.disc_number),
            duration_secs = excluded.duration_secs,
            year = COALESCE(excluded.year, tracks.year),
            genre = COALESCE(excluded.genre, tracks.genre),
            bit_rate = COALESCE(excluded.bit_rate, tracks.bit_rate),
            suffix = COALESCE(excluded.suffix, tracks.suffix),
            content_type = COALESCE(excluded.content_type, tracks.content_type),
            size_bytes = COALESCE(excluded.size_bytes, tracks.size_bytes),
            cover_art_id = COALESCE(excluded.cover_art_id, tracks.cover_art_id),
            path = COALESCE(excluded.path, tracks.path),
            starred = CASE WHEN ? THEN tracks.starred ELSE excluded.starred END,
            user_rating = CASE WHEN ? THEN tracks.user_rating ELSE excluded.user_rating END,
            play_count = MAX(tracks.play_count, excluded.play_count),
            last_played_at = COALESCE(excluded.last_played_at, tracks.last_played_at),
            musicbrainz_id = COALESCE(excluded.musicbrainz_id, tracks.musicbrainz_id),
            is_discovery = CASE WHEN ? THEN tracks.is_discovery ELSE excluded.is_discovery END,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&track.id)
    .bind(&track.title)
    .bind(&track.artist_id)
    .bind(&track.artist_name)
    .bind(&track.album_id)
    .bind(&track.album_name)
    .bind(track.track_number)
    .bind(track.disc_number)
    .bind(track.duration_secs)
    .bind(track.year)
    .bind(&track.genre)
    .bind(track.bit_rate)
    .bind(&track.suffix)
    .bind(&track.content_type)
    .bind(track.size_bytes)
    .bind(&track.cover_art_id)
    .bind(&track.path)
    .bind(track.starred)
    .bind(track.user_rating)
    .bind(track.play_count)
    .bind(track.last_played_at)
    .bind(&track.musicbrainz_id)
    .bind(track.is_discovery)
    .bind(track.updated_at)
    .bind(preserve_pending_curation)
    .bind(preserve_pending_curation)
    .bind(preserve_pending_curation)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_playlist(pool: &SqlitePool, playlist: &LocalPlaylist) -> CoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO playlists (
            id, name, comment, owner, song_count, duration_secs,
            cover_art_id, is_smart, kind, slug, changed_at, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            comment = excluded.comment,
            owner = excluded.owner,
            song_count = excluded.song_count,
            duration_secs = excluded.duration_secs,
            cover_art_id = COALESCE(excluded.cover_art_id, playlists.cover_art_id),
            is_smart = excluded.is_smart,
            kind = excluded.kind,
            slug = COALESCE(excluded.slug, playlists.slug),
            changed_at = excluded.changed_at,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&playlist.id)
    .bind(&playlist.name)
    .bind(&playlist.comment)
    .bind(&playlist.owner)
    .bind(playlist.song_count)
    .bind(playlist.duration_secs)
    .bind(&playlist.cover_art_id)
    .bind(playlist.is_smart)
    .bind(&playlist.kind)
    .bind(&playlist.slug)
    .bind(&playlist.changed_at)
    .bind(playlist.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn replace_playlist_tracks(
    pool: &SqlitePool,
    playlist_id: &str,
    track_ids: &[String],
) -> CoreResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;

    for (idx, track_id) in track_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES (?, ?, ?)",
        )
        .bind(playlist_id)
        .bind(idx as i64)
        .bind(track_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

pub async fn upsert_discovery_track(
    pool: &SqlitePool,
    track: &LocalDiscoveryTrack,
    preserve_pending_curation: bool,
) -> CoreResult<()> {
    let normalized_status = track.status.trim().to_ascii_lowercase();
    sqlx::query(
        r#"
        INSERT INTO discovery_tracks (
            id, song_id, artist, title, album, folder_path,
            profile_slug, mix_slug, status, created_at, expires_at, reason_json,
            exclude_from_core_taste, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            song_id = COALESCE(excluded.song_id, discovery_tracks.song_id),
            artist = excluded.artist,
            title = excluded.title,
            album = excluded.album,
            folder_path = excluded.folder_path,
            profile_slug = excluded.profile_slug,
            mix_slug = excluded.mix_slug,
            status = CASE WHEN ? THEN discovery_tracks.status ELSE excluded.status END,
            created_at = COALESCE(excluded.created_at, discovery_tracks.created_at),
            expires_at = excluded.expires_at,
            reason_json = COALESCE(excluded.reason_json, discovery_tracks.reason_json),
            exclude_from_core_taste = excluded.exclude_from_core_taste,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&track.id)
    .bind(&track.song_id)
    .bind(&track.artist)
    .bind(&track.title)
    .bind(&track.album)
    .bind(&track.folder_path)
    .bind(&track.profile_slug)
    .bind(&track.mix_slug)
    .bind(&normalized_status)
    .bind(&track.created_at)
    .bind(&track.expires_at)
    .bind(&track.reason_json)
    .bind(track.exclude_from_core_taste)
    .bind(track.updated_at)
    .bind(preserve_pending_curation)
    .execute(pool)
    .await?;

    if !preserve_pending_curation {
        if let Some(song_id) = &track.song_id {
            let is_pending = normalized_status == "pending";
            sqlx::query("UPDATE tracks SET is_discovery = ? WHERE id = ?")
                .bind(is_pending)
                .bind(song_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

pub async fn pending_outbox_song_ids(pool: &SqlitePool) -> CoreResult<HashSet<String>> {
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT song_id FROM mutation_outbox WHERE dead_letter = 0",
    )
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().collect())
}

pub async fn get_track(pool: &SqlitePool, track_id: &str) -> CoreResult<Option<LocalTrack>> {
    let row = sqlx::query_as::<_, LocalTrack>("SELECT * FROM tracks WHERE id = ?")
        .bind(track_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

const TRACK_ID_BATCH_SIZE: usize = 500;

/// Fetches tracks for the given IDs, chunking queries to stay well below SQLite's variable limit.
/// Preserves the exact input ordering and duplicates while skipping IDs not present in the mirror.
pub async fn get_tracks_by_ids(pool: &SqlitePool, ids: &[String]) -> CoreResult<Vec<LocalTrack>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut unique_ids = Vec::with_capacity(ids.len());
    let mut seen = HashSet::with_capacity(ids.len());
    for id in ids {
        if seen.insert(id.as_str()) {
            unique_ids.push(id.as_str());
        }
    }

    let mut by_id: HashMap<String, LocalTrack> = HashMap::with_capacity(unique_ids.len());
    for chunk in unique_ids.chunks(TRACK_ID_BATCH_SIZE) {
        let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM tracks WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id);
        }
        sep.push_unseparated(")");
        let rows = qb.build_query_as::<LocalTrack>().fetch_all(pool).await?;
        for row in rows {
            by_id.insert(row.id.clone(), row);
        }
    }

    let mut ordered = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(track) = by_id.get(id) {
            ordered.push(track.clone());
        }
    }
    Ok(ordered)
}

pub async fn list_tracks_for_playlist(
    pool: &SqlitePool,
    playlist_id: &str,
) -> CoreResult<Vec<LocalTrack>> {
    let rows = sqlx::query_as::<_, LocalTrack>(
        r#"
        SELECT t.*
        FROM playlist_tracks pt
        INNER JOIN tracks t ON t.id = pt.track_id
        WHERE pt.playlist_id = ?
        ORDER BY pt.position ASC
        "#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_album(pool: &SqlitePool, album_id: &str) -> CoreResult<Option<LocalAlbum>> {
    let row = sqlx::query_as::<_, LocalAlbum>("SELECT * FROM albums WHERE id = ?")
        .bind(album_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_artist(pool: &SqlitePool, artist_id: &str) -> CoreResult<Option<LocalArtist>> {
    let row = sqlx::query_as::<_, LocalArtist>("SELECT * FROM artists WHERE id = ?")
        .bind(artist_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn list_artists(pool: &SqlitePool) -> CoreResult<Vec<LocalArtist>> {
    let rows =
        sqlx::query_as::<_, LocalArtist>("SELECT * FROM artists ORDER BY name COLLATE NOCASE ASC")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

pub async fn list_albums(
    pool: &SqlitePool,
    artist_id: Option<&str>,
) -> CoreResult<Vec<LocalAlbum>> {
    let rows = if let Some(aid) = artist_id {
        sqlx::query_as::<_, LocalAlbum>(
            "SELECT * FROM albums WHERE artist_id = ? ORDER BY COALESCE(year, 0) DESC, name COLLATE NOCASE ASC",
        )
        .bind(aid)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, LocalAlbum>("SELECT * FROM albums ORDER BY name COLLATE NOCASE ASC")
            .fetch_all(pool)
            .await?
    };
    Ok(rows)
}

pub async fn list_tracks_by_album(
    pool: &SqlitePool,
    album_id: &str,
) -> CoreResult<Vec<LocalTrack>> {
    let rows = sqlx::query_as::<_, LocalTrack>(
        r#"
        SELECT * FROM tracks
        WHERE album_id = ?
        ORDER BY COALESCE(disc_number, 1) ASC, COALESCE(track_number, 0) ASC, title COLLATE NOCASE ASC
        "#,
    )
    .bind(album_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_playlists(pool: &SqlitePool) -> CoreResult<Vec<LocalPlaylist>> {
    let rows = sqlx::query_as::<_, LocalPlaylist>(
        r#"
        SELECT
            p.id,
            p.name,
            p.comment,
            p.owner,
            CASE
                WHEN EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
                    THEN (SELECT COUNT(1) FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
                ELSE p.song_count
            END AS song_count,
            p.duration_secs,
            p.cover_art_id,
            p.is_smart,
            p.kind,
            p.slug,
            p.changed_at,
            p.updated_at
        FROM playlists p
        WHERE p.kind != 'seasonal'
           OR EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
           OR p.song_count > 0
        ORDER BY p.updated_at DESC, p.name COLLATE NOCASE ASC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_starred_tracks(pool: &SqlitePool) -> CoreResult<Vec<LocalTrack>> {
    let rows = sqlx::query_as::<_, LocalTrack>(
        "SELECT * FROM tracks WHERE starred = 1 ORDER BY updated_at DESC, title COLLATE NOCASE ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_discovery_triage(
    pool: &SqlitePool,
    pending_only: bool,
) -> CoreResult<Vec<LocalDiscoveryTrack>> {
    let rows = if pending_only {
        sqlx::query_as::<_, LocalDiscoveryTrack>(
            "SELECT * FROM discovery_tracks WHERE status = 'pending' ORDER BY updated_at DESC",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, LocalDiscoveryTrack>(
            "SELECT * FROM discovery_tracks ORDER BY updated_at DESC",
        )
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

pub async fn library_stats(pool: &SqlitePool) -> CoreResult<LibraryStats> {
    let artist_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM artists")
        .fetch_one(pool)
        .await?;
    let album_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM albums")
        .fetch_one(pool)
        .await?;
    let (track_count, starred_track_count): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(1), COALESCE(SUM(CASE WHEN starred = 1 THEN 1 ELSE 0 END), 0) FROM tracks",
    )
    .fetch_one(pool)
    .await?;
    let playlist_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM playlists")
        .fetch_one(pool)
        .await?;
    let pending_discovery_count: i64 =
        sqlx::query_scalar("SELECT COUNT(1) FROM discovery_tracks WHERE status = 'pending'")
            .fetch_one(pool)
            .await?;

    Ok(LibraryStats {
        artist_count: artist_count.max(0) as u64,
        album_count: album_count.max(0) as u64,
        track_count: track_count.max(0) as u64,
        playlist_count: playlist_count.max(0) as u64,
        starred_track_count: starred_track_count.max(0) as u64,
        pending_discovery_count: pending_discovery_count.max(0) as u64,
    })
}

pub async fn save_playback_session(
    pool: &SqlitePool,
    session: &PersistedPlaybackSession,
) -> CoreResult<()> {
    let queue_json = serde_json::to_string(&session.queue_track_ids)?;
    sqlx::query(
        r#"
        INSERT INTO playback_session (id, queue_json, current_index, position_ms, is_playing, source_label, updated_at)
        VALUES (1, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            queue_json = excluded.queue_json,
            current_index = excluded.current_index,
            position_ms = excluded.position_ms,
            is_playing = excluded.is_playing,
            source_label = excluded.source_label,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(queue_json)
    .bind(i64::try_from(session.current_index).unwrap_or(i64::MAX))
    .bind(i64::try_from(session.position_ms).unwrap_or(i64::MAX))
    .bind(session.is_playing)
    .bind(&session.source_label)
    .bind(session.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_playback_session(
    pool: &SqlitePool,
) -> CoreResult<Option<PersistedPlaybackSession>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        queue_json: String,
        current_index: i64,
        position_ms: i64,
        is_playing: bool,
        source_label: Option<String>,
        updated_at: i64,
    }

    let Some(row) = sqlx::query_as::<_, Row>(
        "SELECT queue_json, current_index, position_ms, is_playing, source_label, updated_at FROM playback_session WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let queue_track_ids: Vec<String> = serde_json::from_str(&row.queue_json)?;
    Ok(Some(PersistedPlaybackSession {
        queue_track_ids,
        current_index: row.current_index.max(0) as usize,
        position_ms: row.position_ms.max(0) as u64,
        is_playing: row.is_playing,
        source_label: row.source_label,
        updated_at: row.updated_at,
    }))
}
