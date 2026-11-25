use std::collections::HashMap;
use std::time::Instant;

use sqlx::SqlitePool;

use crate::db::{load_playback_session, save_playback_session};
use crate::error::CoreResult;
use crate::models::{
    AutoBrowseKind, AutoBrowseNode, LocalAlbum, LocalArtist, LocalPlaylist, LocalTrack,
    PersistedPlaybackSession, VoiceMatchKind, VoiceMatchResult,
};
use crate::search::build_fts5_query;

pub const AUTO_ROOT_ID: &str = "root";
pub const AUTO_TAB_FOR_YOU: &str = "tab:for_you";
pub const AUTO_TAB_PLAYLISTS: &str = "tab:playlists";
pub const AUTO_TAB_LIBRARY: &str = "tab:library";
pub const AUTO_TAB_RADIO: &str = "tab:radio";

pub const AUTO_LIB_OFFLINE: &str = "lib:offline";
pub const AUTO_LIB_RECENT: &str = "lib:recent";

pub const AUTO_FOR_YOU_LIMIT: usize = 12;
pub const AUTO_PLAYLISTS_LIMIT: usize = 24;
pub const AUTO_RECENT_LIMIT: usize = 25;
pub const AUTO_RADIO_LIMIT: usize = 16;
pub const AUTO_BUCKET_ARTISTS_LIMIT: usize = 50;

pub const AZ_BUCKETS: [(&str, &str, char, char); 6] = [
    ("az:a-d", "A–D", 'A', 'D'),
    ("az:e-h", "E–H", 'E', 'H'),
    ("az:i-l", "I–L", 'I', 'L'),
    ("az:m-p", "M–P", 'M', 'P'),
    ("az:q-t", "Q–T", 'Q', 'T'),
    ("az:u-z", "U–Z / #", 'U', 'Z'),
];

/// Pure local SQLite engine backing Android Auto's strictly bounded 4-tab browse tree,
/// `<10ms` local voice search resolver, and persisted playback resumption on car ignition.
#[derive(Clone)]
pub struct AutoBrowseEngine {
    pool: SqlitePool,
}

impl AutoBrowseEngine {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Returns the 4 fixed Android Auto root tabs (`For You`, `Playlists`, `Library`, `Radio`).
    pub fn root_tabs(&self) -> Vec<AutoBrowseNode> {
        vec![
            folder_node(AUTO_TAB_FOR_YOU, "For You", None, AutoBrowseKind::Tab),
            folder_node(AUTO_TAB_PLAYLISTS, "Playlists", None, AutoBrowseKind::Tab),
            folder_node(AUTO_TAB_LIBRARY, "Library", None, AutoBrowseKind::Tab),
            folder_node(AUTO_TAB_RADIO, "Radio", None, AutoBrowseKind::Tab),
        ]
    }

    /// Resolves children for any browse node using strictly bounded local SQLite queries (zero network).
    pub async fn children(
        &self,
        parent_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let art_map = self.cover_art_map().await?;
        match parent_id {
            AUTO_ROOT_ID => Ok(self.root_tabs()),
            AUTO_TAB_FOR_YOU => self.for_you_nodes(&art_map, offline_only).await,
            AUTO_TAB_PLAYLISTS => self.playlist_nodes(&art_map, offline_only).await,
            AUTO_TAB_LIBRARY => self.library_root_nodes().await,
            AUTO_TAB_RADIO => self.radio_nodes(&art_map, offline_only).await,
            AUTO_LIB_OFFLINE => self.offline_track_nodes(&art_map).await,
            AUTO_LIB_RECENT => self.recent_album_nodes(&art_map, offline_only).await,
            "for_you:triage" => self.pending_triage_nodes(&art_map, offline_only).await,
            id if id.starts_with("az:") => self.az_bucket_artists(id, &art_map, offline_only).await,
            id if id.starts_with("playlist:") => {
                let pl_id = &id["playlist:".len()..];
                self.playlist_track_nodes(pl_id, &art_map, offline_only)
                    .await
            }
            id if id.starts_with("artist:") => {
                let artist_id = &id["artist:".len()..];
                self.artist_album_nodes(artist_id, &art_map, offline_only)
                    .await
            }
            id if id.starts_with("album:") => {
                let album_id = &id["album:".len()..];
                self.album_track_nodes(album_id, &art_map, offline_only)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn for_you_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let mut nodes = Vec::new();

        // Pending Discovery triage shortcut if pending items exist
        let pending_count: i64 = if offline_only {
            sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM discovery_tracks dt
                INNER JOIN offline_downloads od ON od.track_id = dt.song_id
                WHERE dt.status = 'pending'
                "#,
            )
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar("SELECT COUNT(1) FROM discovery_tracks WHERE status = 'pending'")
                .fetch_one(&self.pool)
                .await?
        };
        if pending_count > 0 {
            nodes.push(folder_node(
                "for_you:triage",
                "Discovery Triage",
                Some(format!("{pending_count} tracks to review")),
                AutoBrowseKind::Folder,
            ));
        }

        // Seasonal Packs, Moment Mixes, and Discovery playlists first
        let playlists = sqlx::query_as::<_, LocalPlaylist>(
            r#"
            SELECT * FROM playlists
            WHERE kind IN ('seasonal', 'moment', 'discovery')
            ORDER BY
                CASE kind WHEN 'seasonal' THEN 0 WHEN 'moment' THEN 1 ELSE 2 END ASC,
                updated_at DESC
            LIMIT ?
            "#,
        )
        .bind((AUTO_FOR_YOU_LIMIT.saturating_sub(nodes.len())) as i64)
        .fetch_all(&self.pool)
        .await?;

        for pl in playlists {
            if offline_only && !self.playlist_has_offline_tracks(&pl.id).await? {
                continue;
            }
            let offline_ready = self.playlist_has_offline_tracks(&pl.id).await?;
            nodes.push(AutoBrowseNode {
                id: format!("playlist:{}", pl.id),
                title: pl.name,
                subtitle: Some(format!("{} tracks", pl.song_count)),
                kind: AutoBrowseKind::Playlist,
                playable: true,
                browsable: true,
                cover_art_local_path: pl
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: pl.cover_art_id,
                is_offline_ready: offline_ready,
                is_pending_discovery: pl.kind == "discovery" || pl.kind == "seasonal",
            });
            if nodes.len() >= AUTO_FOR_YOU_LIMIT {
                break;
            }
        }

        Ok(nodes)
    }

    async fn playlist_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let playlists = if offline_only {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT DISTINCT p.*
                FROM playlists p
                INNER JOIN playlist_tracks pt ON pt.playlist_id = p.id
                INNER JOIN offline_downloads od ON od.track_id = pt.track_id
                ORDER BY p.updated_at DESC, p.name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(AUTO_PLAYLISTS_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT * FROM playlists
                ORDER BY
                    CASE WHEN EXISTS (
                        SELECT 1 FROM offline_pins op
                        WHERE op.target_kind = 'playlist' AND op.target_id = playlists.id
                    ) THEN 0 ELSE 1 END ASC,
                    updated_at DESC,
                    name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(AUTO_PLAYLISTS_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let mut nodes = Vec::with_capacity(playlists.len());
        for pl in playlists {
            let offline_ready = self.playlist_has_offline_tracks(&pl.id).await?;
            nodes.push(AutoBrowseNode {
                id: format!("playlist:{}", pl.id),
                title: pl.name,
                subtitle: Some(format!("{} tracks", pl.song_count)),
                kind: AutoBrowseKind::Playlist,
                playable: true,
                browsable: true,
                cover_art_local_path: pl
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: pl.cover_art_id,
                is_offline_ready: offline_ready,
                is_pending_discovery: pl.kind == "discovery" || pl.kind == "seasonal",
            });
        }
        Ok(nodes)
    }

    async fn library_root_nodes(&self) -> CoreResult<Vec<AutoBrowseNode>> {
        let offline_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM offline_downloads")
            .fetch_one(&self.pool)
            .await?;

        let mut nodes = Vec::with_capacity(2 + AZ_BUCKETS.len());
        nodes.push(AutoBrowseNode {
            id: AUTO_LIB_OFFLINE.to_string(),
            title: "Offline Downloaded".to_string(),
            subtitle: Some(format!("{offline_count} tracks ready")),
            kind: AutoBrowseKind::Folder,
            playable: offline_count > 0,
            browsable: true,
            cover_art_id: None,
            cover_art_local_path: None,
            is_offline_ready: true,
            is_pending_discovery: false,
        });
        nodes.push(folder_node(
            AUTO_LIB_RECENT,
            "Recently Added",
            Some(format!("Up to {AUTO_RECENT_LIMIT} albums")),
            AutoBrowseKind::Folder,
        ));
        for (bucket_id, label, _, _) in AZ_BUCKETS {
            nodes.push(folder_node(
                bucket_id,
                label,
                Some("Artists".to_string()),
                AutoBrowseKind::Folder,
            ));
        }
        Ok(nodes)
    }

    async fn offline_track_nodes(
        &self,
        art_map: &HashMap<String, String>,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = sqlx::query_as::<_, LocalTrack>(
            r#"
            SELECT t.*
            FROM offline_downloads od
            INNER JOIN tracks t ON t.id = od.track_id
            ORDER BY od.pinned DESC, od.last_accessed_at DESC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(tracks
            .into_iter()
            .map(|t| track_to_node(t, art_map, true))
            .collect())
    }

    async fn recent_album_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let albums = if offline_only {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT DISTINCT a.*
                FROM albums a
                INNER JOIN tracks t ON t.album_id = a.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY COALESCE(a.created_at, '') DESC, a.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RECENT_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT * FROM albums
                ORDER BY COALESCE(created_at, '') DESC, updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RECENT_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(albums
            .into_iter()
            .map(|a| AutoBrowseNode {
                id: format!("album:{}", a.id),
                title: a.name,
                subtitle: Some(a.artist_name),
                kind: AutoBrowseKind::Album,
                playable: true,
                browsable: true,
                cover_art_local_path: a
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: a.cover_art_id,
                is_offline_ready: offline_only,
                is_pending_discovery: false,
            })
            .collect())
    }

    async fn az_bucket_artists(
        &self,
        bucket_id: &str,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let Some(&(_, _, start_ch, end_ch)) = AZ_BUCKETS.iter().find(|b| b.0 == bucket_id) else {
            return Ok(Vec::new());
        };

        let artists = if offline_only {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT DISTINCT ar.*
                FROM artists ar
                INNER JOIN tracks t ON t.artist_id = ar.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY ar.name COLLATE NOCASE ASC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalArtist>(
                "SELECT * FROM artists ORDER BY name COLLATE NOCASE ASC",
            )
            .fetch_all(&self.pool)
            .await?
        };

        let is_last_bucket = bucket_id == "az:u-z";
        let mut nodes = Vec::new();
        for artist in artists {
            let first = first_sort_char(&artist.name);
            let matches = if is_last_bucket {
                (first >= start_ch && first <= end_ch) || !first.is_ascii_alphabetic()
            } else {
                first >= start_ch && first <= end_ch
            };
            if !matches {
                continue;
            }
            nodes.push(AutoBrowseNode {
                id: format!("artist:{}", artist.id),
                title: artist.name,
                subtitle: Some(format!("{} albums", artist.album_count)),
                kind: AutoBrowseKind::Artist,
                playable: true,
                browsable: true,
                cover_art_local_path: artist
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: artist.cover_art_id,
                is_offline_ready: offline_only,
                is_pending_discovery: false,
            });
            if nodes.len() >= AUTO_BUCKET_ARTISTS_LIMIT {
                break;
            }
        }
        Ok(nodes)
    }

    async fn radio_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let artists = if offline_only {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT DISTINCT ar.*
                FROM artists ar
                INNER JOIN tracks t ON t.artist_id = ar.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY ar.starred DESC, ar.album_count DESC, ar.name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RADIO_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT * FROM artists
                ORDER BY starred DESC, album_count DESC, name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RADIO_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(artists
            .into_iter()
            .map(|ar| AutoBrowseNode {
                id: format!("radio:artist:{}", ar.id),
                title: format!("{} Radio", ar.name),
                subtitle: Some("Instant station".to_string()),
                kind: AutoBrowseKind::RadioSeed,
                playable: true,
                browsable: false,
                cover_art_local_path: ar
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: ar.cover_art_id,
                is_offline_ready: offline_only,
                is_pending_discovery: false,
            })
            .collect())
    }

    async fn pending_triage_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM discovery_tracks dt
                INNER JOIN tracks t ON t.id = dt.song_id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE dt.status = 'pending'
                ORDER BY dt.updated_at DESC
                LIMIT 25
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM discovery_tracks dt
                INNER JOIN tracks t ON t.id = dt.song_id
                WHERE dt.status = 'pending'
                ORDER BY dt.updated_at DESC
                LIMIT 25
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };

        let offline_set = self.offline_track_set().await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, art_map, is_off)
            })
            .collect())
    }

    async fn playlist_track_nodes(
        &self,
        playlist_id: &str,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM playlist_tracks pt
                INNER JOIN tracks t ON t.id = pt.track_id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE pt.playlist_id = ?
                ORDER BY pt.position ASC
                LIMIT 100
                "#,
            )
            .bind(playlist_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM playlist_tracks pt
                INNER JOIN tracks t ON t.id = pt.track_id
                WHERE pt.playlist_id = ?
                ORDER BY pt.position ASC
                LIMIT 100
                "#,
            )
            .bind(playlist_id)
            .fetch_all(&self.pool)
            .await?
        };
        let offline_set = self.offline_track_set().await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, art_map, is_off)
            })
            .collect())
    }

    async fn artist_album_nodes(
        &self,
        artist_id: &str,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let albums = if offline_only {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT DISTINCT a.*
                FROM albums a
                INNER JOIN tracks t ON t.album_id = a.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE a.artist_id = ?
                ORDER BY COALESCE(a.year, 0) DESC, a.name COLLATE NOCASE ASC
                LIMIT 50
                "#,
            )
            .bind(artist_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT * FROM albums
                WHERE artist_id = ?
                ORDER BY COALESCE(year, 0) DESC, name COLLATE NOCASE ASC
                LIMIT 50
                "#,
            )
            .bind(artist_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(albums
            .into_iter()
            .map(|a| AutoBrowseNode {
                id: format!("album:{}", a.id),
                title: a.name,
                subtitle: a.year.map(|y| y.to_string()).or(Some(a.artist_name)),
                kind: AutoBrowseKind::Album,
                playable: true,
                browsable: true,
                cover_art_local_path: a
                    .cover_art_id
                    .as_deref()
                    .and_then(|cid| art_map.get(cid))
                    .cloned(),
                cover_art_id: a.cover_art_id,
                is_offline_ready: offline_only,
                is_pending_discovery: false,
            })
            .collect())
    }

    async fn album_track_nodes(
        &self,
        album_id: &str,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.album_id = ?
                ORDER BY COALESCE(t.disc_number, 1) ASC, COALESCE(t.track_number, 0) ASC
                "#,
            )
            .bind(album_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT * FROM tracks
                WHERE album_id = ?
                ORDER BY COALESCE(disc_number, 1) ASC, COALESCE(track_number, 0) ASC
                "#,
            )
            .bind(album_id)
            .fetch_all(&self.pool)
            .await?
        };
        let offline_set = self.offline_track_set().await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, art_map, is_off)
            })
            .collect())
    }

    /// Sub-10ms pure local SQLite voice command resolver for Android Auto Assistant intents
    /// (e.g., `"Play Noël mix"`, `"Play Daft Punk"`, `"Play Get Lucky"`).
    pub async fn voice_search(
        &self,
        raw_query: &str,
        offline_only: bool,
    ) -> CoreResult<Option<VoiceMatchResult>> {
        let start = Instant::now();
        let cleaned = strip_voice_prefix(raw_query);
        if cleaned.is_empty() {
            return Ok(None);
        }
        let folded_query = fold_diacritics_ascii(&cleaned);

        // 1. Playlists / Seasonal Packs / Moment Mixes (diacritic-insensitive match)
        let playlists = sqlx::query_as::<_, LocalPlaylist>(
            "SELECT * FROM playlists ORDER BY CASE kind WHEN 'seasonal' THEN 0 WHEN 'moment' THEN 1 ELSE 2 END ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        for pl in playlists {
            let name_folded = fold_diacritics_ascii(&pl.name);
            let slug_folded = pl
                .slug
                .as_deref()
                .map(fold_diacritics_ascii)
                .unwrap_or_default();
            if name_folded.contains(&folded_query)
                || folded_query.contains(&name_folded)
                || (!slug_folded.is_empty() && folded_query.contains(&slug_folded))
            {
                let track_ids = self.playlist_track_ids(&pl.id, offline_only).await?;
                if !track_ids.is_empty() {
                    return Ok(Some(VoiceMatchResult {
                        kind: VoiceMatchKind::Playlist,
                        target_id: pl.id,
                        label: pl.name,
                        track_ids,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }));
                }
            }
        }

        // 2. Artists
        let artists = sqlx::query_as::<_, LocalArtist>("SELECT * FROM artists")
            .fetch_all(&self.pool)
            .await?;
        for artist in artists {
            let artist_folded = fold_diacritics_ascii(&artist.name);
            if artist_folded == folded_query || artist_folded.contains(&folded_query) {
                let track_ids: Vec<String> = if offline_only {
                    sqlx::query_scalar(
                        r#"
                        SELECT t.id FROM tracks t
                        INNER JOIN offline_downloads od ON od.track_id = t.id
                        WHERE t.artist_id = ?
                        ORDER BY t.starred DESC, t.play_count DESC
                        LIMIT 50
                        "#,
                    )
                    .bind(&artist.id)
                    .fetch_all(&self.pool)
                    .await?
                } else {
                    sqlx::query_scalar(
                        r#"
                        SELECT id FROM tracks
                        WHERE artist_id = ?
                        ORDER BY starred DESC, play_count DESC
                        LIMIT 50
                        "#,
                    )
                    .bind(&artist.id)
                    .fetch_all(&self.pool)
                    .await?
                };
                if !track_ids.is_empty() {
                    return Ok(Some(VoiceMatchResult {
                        kind: VoiceMatchKind::Artist,
                        target_id: artist.id,
                        label: artist.name,
                        track_ids,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }));
                }
            }
        }

        // 3. Albums
        let albums = sqlx::query_as::<_, LocalAlbum>("SELECT * FROM albums")
            .fetch_all(&self.pool)
            .await?;
        for album in albums {
            let album_folded = fold_diacritics_ascii(&album.name);
            if album_folded == folded_query || album_folded.contains(&folded_query) {
                let track_ids: Vec<String> = if offline_only {
                    sqlx::query_scalar(
                        r#"
                        SELECT t.id FROM tracks t
                        INNER JOIN offline_downloads od ON od.track_id = t.id
                        WHERE t.album_id = ?
                        ORDER BY COALESCE(t.disc_number, 1), COALESCE(t.track_number, 0)
                        "#,
                    )
                    .bind(&album.id)
                    .fetch_all(&self.pool)
                    .await?
                } else {
                    sqlx::query_scalar(
                        r#"
                        SELECT id FROM tracks
                        WHERE album_id = ?
                        ORDER BY COALESCE(disc_number, 1), COALESCE(track_number, 0)
                        "#,
                    )
                    .bind(&album.id)
                    .fetch_all(&self.pool)
                    .await?
                };
                if !track_ids.is_empty() {
                    return Ok(Some(VoiceMatchResult {
                        kind: VoiceMatchKind::Album,
                        target_id: album.id,
                        label: album.name,
                        track_ids,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }));
                }
            }
        }

        // 4. FTS5 Track lookup
        if let Some(fts) = build_fts5_query(&cleaned) {
            let tracks = if offline_only {
                sqlx::query_as::<_, LocalTrack>(
                    r#"
                    SELECT t.*
                    FROM tracks_fts
                    INNER JOIN tracks t ON t.rowid = tracks_fts.rowid
                    INNER JOIN offline_downloads od ON od.track_id = t.id
                    WHERE tracks_fts MATCH ?
                    ORDER BY bm25(tracks_fts, 10.0, 6.0, 3.0, 1.0) ASC
                    LIMIT 25
                    "#,
                )
                .bind(&fts)
                .fetch_all(&self.pool)
                .await?
            } else {
                sqlx::query_as::<_, LocalTrack>(
                    r#"
                    SELECT t.*
                    FROM tracks_fts
                    INNER JOIN tracks t ON t.rowid = tracks_fts.rowid
                    WHERE tracks_fts MATCH ?
                    ORDER BY bm25(tracks_fts, 10.0, 6.0, 3.0, 1.0) ASC
                    LIMIT 25
                    "#,
                )
                .bind(&fts)
                .fetch_all(&self.pool)
                .await?
            };

            if let Some(first) = tracks.first() {
                return Ok(Some(VoiceMatchResult {
                    kind: VoiceMatchKind::Track,
                    target_id: first.id.clone(),
                    label: format!("{} — {}", first.artist_name, first.title),
                    track_ids: tracks.into_iter().map(|t| t.id).collect(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                }));
            }
        }

        Ok(None)
    }

    pub async fn save_resumption_session(
        &self,
        session: &PersistedPlaybackSession,
    ) -> CoreResult<()> {
        save_playback_session(&self.pool, session).await
    }

    pub async fn load_resumption_session(&self) -> CoreResult<Option<PersistedPlaybackSession>> {
        load_playback_session(&self.pool).await
    }

    async fn playlist_track_ids(
        &self,
        playlist_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<String>> {
        let ids = if offline_only {
            sqlx::query_scalar(
                r#"
                SELECT pt.track_id
                FROM playlist_tracks pt
                INNER JOIN offline_downloads od ON od.track_id = pt.track_id
                WHERE pt.playlist_id = ?
                ORDER BY pt.position ASC
                "#,
            )
            .bind(playlist_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position ASC",
            )
            .bind(playlist_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(ids)
    }

    async fn playlist_has_offline_tracks(&self, playlist_id: &str) -> CoreResult<bool> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(1)
            FROM playlist_tracks pt
            INNER JOIN offline_downloads od ON od.track_id = pt.track_id
            WHERE pt.playlist_id = ?
            "#,
        )
        .bind(playlist_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    async fn cover_art_map(&self) -> CoreResult<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT cover_art_id, local_path FROM cover_art_cache")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    async fn offline_track_set(&self) -> CoreResult<std::collections::HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT track_id FROM offline_downloads")
            .fetch_all(&self.pool)
            .await?;
        Ok(ids.into_iter().collect())
    }
}

fn folder_node(
    id: &str,
    title: &str,
    subtitle: Option<String>,
    kind: AutoBrowseKind,
) -> AutoBrowseNode {
    AutoBrowseNode {
        id: id.to_string(),
        title: title.to_string(),
        subtitle,
        kind,
        playable: false,
        browsable: true,
        cover_art_id: None,
        cover_art_local_path: None,
        is_offline_ready: false,
        is_pending_discovery: false,
    }
}

fn track_to_node(
    track: LocalTrack,
    art_map: &HashMap<String, String>,
    is_offline_ready: bool,
) -> AutoBrowseNode {
    AutoBrowseNode {
        id: format!("track:{}", track.id),
        title: track.title,
        subtitle: Some(track.artist_name),
        kind: AutoBrowseKind::Track,
        playable: true,
        browsable: false,
        cover_art_local_path: track
            .cover_art_id
            .as_deref()
            .and_then(|cid| art_map.get(cid))
            .cloned(),
        cover_art_id: track.cover_art_id,
        is_offline_ready,
        is_pending_discovery: track.is_discovery,
    }
}

fn first_sort_char(name: &str) -> char {
    let trimmed = name.trim();
    let stripped = trimmed
        .strip_prefix("The ")
        .or_else(|| trimmed.strip_prefix("the "))
        .unwrap_or(trimmed);
    stripped
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('#')
}

fn strip_voice_prefix(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    let lower = s.to_ascii_lowercase();
    for prefix in [
        "play some ",
        "play the ",
        "play my ",
        "play ",
        "listen to ",
        "put on ",
        "joue ",
        "lance ",
        "mets ",
    ] {
        if lower.starts_with(prefix) {
            s = s[prefix.len()..].trim().to_string();
            break;
        }
    }
    let lower2 = s.to_ascii_lowercase();
    for suffix in [" playlist", " radio"] {
        if lower2.ends_with(suffix) {
            s = s[..s.len() - suffix.len()].trim().to_string();
            break;
        }
    }
    s
}

/// Lightweight diacritic folder for Latin characters so `"Noel mix"` matches `"Noël à la Française"`.
fn fold_diacritics_ascii(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'a',
            'ç' | 'Ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
            'ñ' | 'Ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
            'ý' | 'ÿ' | 'Ý' | 'Ÿ' => 'y',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}
