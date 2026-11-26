use std::collections::{HashMap, HashSet};
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
                INNER JOIN tracks t ON t.id = dt.song_id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE dt.status = 'pending'
                "#,
            )
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM discovery_tracks dt
                INNER JOIN tracks t ON t.id = dt.song_id
                WHERE dt.status = 'pending'
                "#,
            )
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

        let remaining_limit = AUTO_FOR_YOU_LIMIT.saturating_sub(nodes.len()) as i64;
        if remaining_limit <= 0 {
            return Ok(nodes);
        }

        // Seasonal Packs, Moment Mixes, and Discovery playlists (filter offline_only BEFORE LIMIT!)
        let playlists = if offline_only {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT DISTINCT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(1) FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
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
                INNER JOIN playlist_tracks pt ON pt.playlist_id = p.id
                INNER JOIN offline_downloads od ON od.track_id = pt.track_id
                WHERE p.kind IN ('seasonal', 'moment', 'discovery')
                ORDER BY
                    CASE p.kind WHEN 'seasonal' THEN 0 WHEN 'moment' THEN 1 ELSE 2 END ASC,
                    p.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(remaining_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(1) FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
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
                WHERE p.kind IN ('seasonal', 'moment', 'discovery')
                ORDER BY
                    CASE p.kind WHEN 'seasonal' THEN 0 WHEN 'moment' THEN 1 ELSE 2 END ASC,
                    p.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(remaining_limit)
            .fetch_all(&self.pool)
            .await?
        };

        let offline_playlists = self.offline_playlist_set().await?;
        for pl in playlists {
            let offline_ready = offline_only || offline_playlists.contains(&pl.id);
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

    async fn playlist_nodes(
        &self,
        art_map: &HashMap<String, String>,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let playlists = if offline_only {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT DISTINCT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(1) FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
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
                INNER JOIN playlist_tracks pt ON pt.playlist_id = p.id
                INNER JOIN offline_downloads od ON od.track_id = pt.track_id
                WHERE p.kind = 'user'
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
                SELECT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(1) FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
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
                WHERE p.kind = 'user'
                ORDER BY
                    CASE WHEN EXISTS (
                        SELECT 1 FROM offline_pins op
                        WHERE op.target_kind = 'playlist' AND op.target_id = p.id
                    ) THEN 0 ELSE 1 END ASC,
                    p.updated_at DESC,
                    p.name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(AUTO_PLAYLISTS_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let offline_playlists = self.offline_playlist_set().await?;
        let mut nodes = Vec::with_capacity(playlists.len());
        for pl in playlists {
            let offline_ready = offline_only || offline_playlists.contains(&pl.id);
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
        let offline_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(1)
            FROM offline_downloads od
            INNER JOIN tracks t ON t.id = od.track_id
            "#,
        )
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
            is_offline_ready: offline_count > 0,
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

        let offline_albums = self.offline_album_set().await?;
        Ok(albums
            .into_iter()
            .map(|a| {
                let is_off = offline_only || offline_albums.contains(&a.id);
                AutoBrowseNode {
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
                    is_offline_ready: is_off,
                    is_pending_discovery: false,
                }
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

        let offline_artists = self.offline_artist_set().await?;
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
            let is_off = offline_only || offline_artists.contains(&artist.id);
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
                is_offline_ready: is_off,
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

        let offline_artists = self.offline_artist_set().await?;
        Ok(artists
            .into_iter()
            .map(|ar| {
                let is_off = offline_only || offline_artists.contains(&ar.id);
                AutoBrowseNode {
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
                    is_offline_ready: is_off,
                    is_pending_discovery: false,
                }
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
        let offline_albums = self.offline_album_set().await?;
        Ok(albums
            .into_iter()
            .map(|a| {
                let is_off = offline_only || offline_albums.contains(&a.id);
                AutoBrowseNode {
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
                    is_offline_ready: is_off,
                    is_pending_discovery: false,
                }
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
                LIMIT 100
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
                LIMIT 100
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
        let (prefix_stripped, suffix_stripped) = strip_voice_affixes(raw_query);
        let q_raw = normalize_match_text(&prefix_stripped);
        let q_stripped = normalize_match_text(&suffix_stripped);
        if q_raw.is_empty() && q_stripped.is_empty() {
            return Ok(None);
        }

        let mut query_variants = Vec::with_capacity(2);
        if !q_raw.is_empty() {
            query_variants.push(q_raw.as_str());
        }
        if !q_stripped.is_empty() && q_stripped != q_raw {
            query_variants.push(q_stripped.as_str());
        }

        // Rank candidate matches across Playlists, Artists, and Albums using exact & word-boundary
        // scoring so short playlist names (e.g. "Punk") never hijack longer queries ("Play Daft Punk").
        // Score tuple: (match_tier, kind_priority, sub_priority, coverage_bps)
        struct RankedCandidate {
            score: (u8, u8, u8, u32),
            kind: VoiceMatchKind,
            target_id: String,
            label: String,
            track_ids: Vec<String>,
        }

        let mut best: Option<RankedCandidate> = None;
        let consider = |best: &mut Option<RankedCandidate>, cand: RankedCandidate| {
            let replace = match best {
                Some(prev) => cand.score > prev.score,
                None => true,
            };
            if replace {
                *best = Some(cand);
            }
        };

        // 1. Playlists / Seasonal Packs / Moment Mixes
        let playlists = sqlx::query_as::<_, LocalPlaylist>("SELECT * FROM playlists")
            .fetch_all(&self.pool)
            .await?;
        for pl in playlists {
            let name_norm = normalize_match_text(&pl.name);
            let slug_norm = pl
                .slug
                .as_deref()
                .map(normalize_match_text)
                .unwrap_or_default();

            let mut best_match: Option<(u8, u32)> = None;
            for q in &query_variants {
                for target in [name_norm.as_str(), slug_norm.as_str()] {
                    if let Some(m) = score_voice_match(target, q) {
                        if best_match.is_none_or(|prev| m > prev) {
                            best_match = Some(m);
                        }
                    }
                }
            }

            if let Some((tier, coverage)) = best_match {
                let pl_sub_prio = match pl.kind.as_str() {
                    "seasonal" => 3,
                    "moment" => 2,
                    "discovery" => 1,
                    _ => 0,
                };
                // Playlists take slight priority on equal tier (e.g. seasonal mixes).
                let score = (tier, 3, pl_sub_prio, coverage);
                if best.as_ref().is_none_or(|b| score > b.score) {
                    let track_ids = self.playlist_track_ids(&pl.id, offline_only).await?;
                    if !track_ids.is_empty() {
                        consider(
                            &mut best,
                            RankedCandidate {
                                score,
                                kind: VoiceMatchKind::Playlist,
                                target_id: pl.id,
                                label: pl.name,
                                track_ids,
                            },
                        );
                    }
                }
            }
        }

        // Only scan Artists & Albums if we don't already have an unbeatable Tier-4 exact Playlist match.
        let have_exact_playlist = best
            .as_ref()
            .is_some_and(|b| b.kind == VoiceMatchKind::Playlist && b.score.0 == 4);

        if !have_exact_playlist {
            // 2. Artists
            let artists = sqlx::query_as::<_, LocalArtist>("SELECT * FROM artists")
                .fetch_all(&self.pool)
                .await?;
            for artist in artists {
                let artist_norm = normalize_match_text(&artist.name);
                let mut best_match: Option<(u8, u32)> = None;
                for q in &query_variants {
                    if let Some(m) = score_voice_match(&artist_norm, q) {
                        if best_match.is_none_or(|prev| m > prev) {
                            best_match = Some(m);
                        }
                    }
                }
                if let Some((tier, coverage)) = best_match {
                    let score = (tier, 2, 0, coverage);
                    if best.as_ref().is_none_or(|b| score > b.score) {
                        let track_ids = self.artist_track_ids(&artist.id, offline_only).await?;
                        if !track_ids.is_empty() {
                            consider(
                                &mut best,
                                RankedCandidate {
                                    score,
                                    kind: VoiceMatchKind::Artist,
                                    target_id: artist.id,
                                    label: artist.name,
                                    track_ids,
                                },
                            );
                        }
                    }
                }
            }

            // 3. Albums
            let have_exact_artist = best.as_ref().is_some_and(|b| b.score.0 == 4);
            if !have_exact_artist {
                let albums = sqlx::query_as::<_, LocalAlbum>("SELECT * FROM albums")
                    .fetch_all(&self.pool)
                    .await?;
                for album in albums {
                    let album_norm = normalize_match_text(&album.name);
                    let mut best_match: Option<(u8, u32)> = None;
                    for q in &query_variants {
                        if let Some(m) = score_voice_match(&album_norm, q) {
                            if best_match.is_none_or(|prev| m > prev) {
                                best_match = Some(m);
                            }
                        }
                    }
                    if let Some((tier, coverage)) = best_match {
                        let score = (tier, 1, 0, coverage);
                        if best.as_ref().is_none_or(|b| score > b.score) {
                            let track_ids = self.album_track_ids(&album.id, offline_only).await?;
                            if !track_ids.is_empty() {
                                consider(
                                    &mut best,
                                    RankedCandidate {
                                        score,
                                        kind: VoiceMatchKind::Album,
                                        target_id: album.id,
                                        label: album.name,
                                        track_ids,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        if let Some(winner) = best {
            return Ok(Some(VoiceMatchResult {
                kind: winner.kind,
                target_id: winner.target_id,
                label: winner.label,
                track_ids: winner.track_ids,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }));
        }

        // 4. FTS5 Track lookup
        let fts_source = if !suffix_stripped.is_empty() {
            suffix_stripped.as_str()
        } else {
            prefix_stripped.as_str()
        };
        if let Some(fts) = build_fts5_query(fts_source) {
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

    async fn artist_track_ids(
        &self,
        artist_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<String>> {
        let ids = if offline_only {
            sqlx::query_scalar(
                r#"
                SELECT t.id FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.artist_id = ?
                ORDER BY t.starred DESC, t.play_count DESC
                LIMIT 50
                "#,
            )
            .bind(artist_id)
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
            .bind(artist_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(ids)
    }

    async fn album_track_ids(
        &self,
        album_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<String>> {
        let ids = if offline_only {
            sqlx::query_scalar(
                r#"
                SELECT t.id FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.album_id = ?
                ORDER BY COALESCE(t.disc_number, 1), COALESCE(t.track_number, 0)
                LIMIT 100
                "#,
            )
            .bind(album_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                r#"
                SELECT id FROM tracks
                WHERE album_id = ?
                ORDER BY COALESCE(disc_number, 1), COALESCE(track_number, 0)
                LIMIT 100
                "#,
            )
            .bind(album_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(ids)
    }

    async fn cover_art_map(&self) -> CoreResult<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT cover_art_id, local_path FROM cover_art_cache")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    async fn offline_track_set(&self) -> CoreResult<HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT track_id FROM offline_downloads")
            .fetch_all(&self.pool)
            .await?;
        Ok(ids.into_iter().collect())
    }

    async fn offline_playlist_set(&self) -> CoreResult<HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT pt.playlist_id
            FROM playlist_tracks pt
            INNER JOIN offline_downloads od ON od.track_id = pt.track_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().collect())
    }

    async fn offline_album_set(&self) -> CoreResult<HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT t.album_id
            FROM tracks t
            INNER JOIN offline_downloads od ON od.track_id = t.id
            WHERE t.album_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().collect())
    }

    async fn offline_artist_set(&self) -> CoreResult<HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT t.artist_id
            FROM tracks t
            INNER JOIN offline_downloads od ON od.track_id = t.id
            WHERE t.artist_id IS NOT NULL
            "#,
        )
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
    let stripped = if trimmed
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("the "))
    {
        trimmed[4..].trim_start()
    } else {
        trimmed
    };
    let folded = fold_diacritics_ascii(stripped);
    folded
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('#')
}

/// Returns `(prefix_stripped, prefix_and_suffix_stripped)` so both exact names like `"Drive Mix"`
/// and spoken intents like `"Play Noël mix"` rank accurately.
fn strip_voice_affixes(raw: &str) -> (String, String) {
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

    let mut stripped = s.clone();
    let lower2 = stripped.to_ascii_lowercase();
    for suffix in [" playlist", " radio", " mix"] {
        if lower2.ends_with(suffix) {
            stripped = stripped[..stripped.len() - suffix.len()].trim().to_string();
            break;
        }
    }

    (s, stripped)
}

/// Scores how well `candidate` matches `query` (both already normalized via `normalize_match_text`).
/// Returns `Some((tier, coverage_bps))` where higher tiers represent stronger matches:
/// - Tier 4: exact normalized match
/// - Tier 3: candidate starts with query on a word boundary
/// - Tier 2: candidate contains query as a contiguous word-bounded phrase, or contains every query word
/// - Tier 1: every query token is a word-prefix of some word in candidate (query length >= 3)
fn score_voice_match(candidate: &str, query: &str) -> Option<(u8, u32)> {
    if candidate.is_empty() || query.is_empty() {
        return None;
    }

    let coverage = ((query.len() as u64 * 10_000) / candidate.len().max(1) as u64).min(10_000) as u32;

    if candidate == query {
        return Some((4, 10_000));
    }

    if let Some(rest) = candidate.strip_prefix(query) {
        if rest.is_empty() || rest.starts_with(' ') {
            return Some((3, coverage));
        }
    }

    let cand_words: Vec<&str> = candidate.split_whitespace().collect();
    let query_words: Vec<&str> = query.split_whitespace().collect();
    if query_words.is_empty() {
        return None;
    }

    // Contiguous whole-word sequence match inside candidate
    if cand_words.len() >= query_words.len()
        && cand_words
            .windows(query_words.len())
            .any(|w| w == query_words.as_slice())
    {
        return Some((2, coverage));
    }

    // All query words appear as whole words in candidate (in any order)
    if query_words
        .iter()
        .all(|qw| cand_words.iter().any(|cw| cw == qw))
    {
        return Some((2, coverage.saturating_sub(1)));
    }

    // Prefix match on words (require at least 3 chars in query to avoid noisy 1-2 char matches)
    if query.len() >= 3
        && query_words
            .iter()
            .all(|qw| cand_words.iter().any(|cw| cw.starts_with(qw)))
    {
        return Some((1, coverage));
    }

    None
}

/// Normalizes text for voice & slug matching: folds diacritics/ligatures, converts `_`, `-`, and
/// punctuation into spaces, and collapses consecutive whitespace.
fn normalize_match_text(input: &str) -> String {
    let folded = fold_diacritics_ascii(input);
    let mut out = String::with_capacity(folded.len());
    let mut prev_space = true;
    for ch in folded.chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Unicode-aware Latin diacritic & ligature folder (handles precomposed characters, `œ/æ/ß`,
/// and strips NFD combining diacritical marks so `"Noël"`, `"Noe\u{0308}l"`, and `"Édith"` all fold cleanly).
fn fold_diacritics_ascii(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        // Skip Unicode combining diacritical marks (NFD decomposition).
        if matches!(
            ch,
            '\u{0300}'..='\u{036F}'
                | '\u{1AB0}'..='\u{1AFF}'
                | '\u{1DC0}'..='\u{1DFF}'
                | '\u{20D0}'..='\u{20FF}'
                | '\u{FE20}'..='\u{FE2F}'
        ) {
            continue;
        }
        match ch {
            'œ' | 'Œ' => out.push_str("oe"),
            'æ' | 'Æ' => out.push_str("ae"),
            'ß' | 'ẞ' => out.push_str("ss"),
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą'
            | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' => out.push('a'),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' | 'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => out.push('c'),
            'ď' | 'đ' | 'Ď' | 'Đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě'
            | 'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => out.push('e'),
            'ğ' | 'ĝ' | 'ġ' | 'ģ' | 'Ğ' | 'Ĝ' | 'Ġ' | 'Ģ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı'
            | 'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' => out.push('i'),
            'ł' | 'ĺ' | 'ļ' | 'ľ' | 'Ł' | 'Ĺ' | 'Ļ' | 'Ľ' => out.push('l'),
            'ñ' | 'ń' | 'ņ' | 'ň' | 'Ñ' | 'Ń' | 'Ņ' | 'Ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő'
            | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' => out.push('o'),
            'ŕ' | 'ŗ' | 'ř' | 'Ŕ' | 'Ŗ' | 'Ř' => out.push('r'),
            'ś' | 'ŝ' | 'ş' | 'š' | 'Ś' | 'Ŝ' | 'Ş' | 'Š' => out.push('s'),
            'ţ' | 'ť' | 'ŧ' | 'Ţ' | 'Ť' | 'Ŧ' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų'
            | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => out.push('u'),
            'ý' | 'ÿ' | 'ŷ' | 'Ý' | 'Ÿ' | 'Ŷ' => out.push('y'),
            'ź' | 'ż' | 'ž' | 'Ź' | 'Ż' | 'Ž' => out.push('z'),
            other => {
                for lc in other.to_lowercase() {
                    out.push(lc);
                }
            }
        }
    }
    out
}
