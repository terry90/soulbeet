use std::collections::{HashMap, HashSet};
use std::time::Instant;

use sqlx::{QueryBuilder, Sqlite, SqlitePool};

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

pub const AUTO_FOR_YOU_DRIVE_MIX: &str = "for_you:drive_mix";
pub const AUTO_FOR_YOU_TRIAGE: &str = "for_you:triage";
pub const AUTO_PLAYLIST_STARRED: &str = "playlist:starred";

pub const AUTO_LIB_OFFLINE: &str = "lib:offline";
pub const AUTO_LIB_RECENT: &str = "lib:recent";
pub const AUTO_LIB_RECENT_ARTISTS: &str = "lib:recent_artists";

pub const AUTO_RADIO_SHUFFLE: &str = "radio:shuffle";
pub const AUTO_RADIO_MOOD_UPBEAT: &str = "radio:mood:upbeat";
pub const AUTO_RADIO_MOOD_CHILL: &str = "radio:mood:chill";
pub const AUTO_RADIO_MOOD_LATE_NIGHT: &str = "radio:mood:late_night";

pub const AUTO_FOR_YOU_LIMIT: usize = 16;
pub const AUTO_PLAYLISTS_LIMIT: usize = 24;
pub const AUTO_RECENT_LIMIT: usize = 25;
pub const AUTO_RADIO_LIMIT: usize = 16;
pub const AUTO_BUCKET_ARTISTS_LIMIT: usize = 50;
pub const AUTO_BUCKET_ALBUMS_LIMIT: usize = 50;
pub const AUTO_DRIVE_MIX_LIMIT: usize = 30;
pub const AUTO_CONTINUE_LISTENING_LIMIT: usize = 4;

pub const GROUP_RIGHT_NOW: &str = "Right Now";
pub const GROUP_DISCOVER: &str = "Discover";
pub const GROUP_CONTINUE_LISTENING: &str = "Continue Listening";
pub const GROUP_QUICK_STATIONS: &str = "Quick Stations";
pub const GROUP_GENRES: &str = "Genres";
pub const GROUP_ARTIST_RADIO: &str = "Artist Radio";
pub const GROUP_ARTISTS: &str = "Artists (A-Z)";
pub const GROUP_ALBUMS: &str = "Albums (A-Z)";

pub const AZ_BUCKETS: [(&str, &str, char, char); 6] = [
    ("az:a-d", "A–D", 'A', 'D'),
    ("az:e-h", "E–H", 'E', 'H'),
    ("az:i-l", "I–L", 'I', 'L'),
    ("az:m-p", "M–P", 'M', 'P'),
    ("az:q-t", "Q–T", 'Q', 'T'),
    ("az:u-z", "U–Z / #", 'U', 'Z'),
];

pub const AZ_ALBUM_BUCKETS: [(&str, &str, char, char); 6] = [
    ("az_album:a-d", "A–D", 'A', 'D'),
    ("az_album:e-h", "E–H", 'E', 'H'),
    ("az_album:i-l", "I–L", 'I', 'L'),
    ("az_album:m-p", "M–P", 'M', 'P'),
    ("az_album:q-t", "Q–T", 'Q', 'T'),
    ("az_album:u-z", "U–Z / #", 'U', 'Z'),
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
        match parent_id {
            AUTO_ROOT_ID => Ok(self.root_tabs()),
            AUTO_TAB_FOR_YOU => self.for_you_nodes(offline_only).await,
            AUTO_TAB_PLAYLISTS => self.playlist_nodes(offline_only).await,
            AUTO_TAB_LIBRARY => self.library_root_nodes().await,
            AUTO_TAB_RADIO => self.radio_nodes(offline_only).await,
            AUTO_LIB_OFFLINE => self.offline_track_nodes().await,
            AUTO_LIB_RECENT => self.recent_album_nodes(offline_only).await,
            AUTO_LIB_RECENT_ARTISTS => self.recent_artist_nodes(offline_only).await,
            AUTO_FOR_YOU_DRIVE_MIX => self.drive_mix_track_nodes(offline_only).await,
            AUTO_FOR_YOU_TRIAGE => self.pending_triage_nodes(offline_only).await,
            AUTO_PLAYLIST_STARRED => self.starred_track_nodes(offline_only).await,
            AUTO_RADIO_SHUFFLE => self.shuffle_track_nodes(offline_only).await,
            id if id.starts_with("radio:mood:") => {
                let mood = &id["radio:mood:".len()..];
                self.mood_radio_track_nodes(mood, offline_only).await
            }
            id if id.starts_with("radio:genre:") => {
                let genre = &id["radio:genre:".len()..];
                self.genre_radio_track_nodes(genre, offline_only).await
            }
            id if id.starts_with("az_album:") => {
                self.az_bucket_albums(id, offline_only).await
            }
            id if id.starts_with("az:") => self.az_bucket_artists(id, offline_only).await,
            id if id.starts_with("playlist:") => {
                let pl_id = &id["playlist:".len()..];
                self.playlist_track_nodes(pl_id, offline_only).await
            }
            id if id.starts_with("artist:") => {
                let artist_id = &id["artist:".len()..];
                self.artist_album_nodes(artist_id, offline_only).await
            }
            id if id.starts_with("album:") => {
                let album_id = &id["album:".len()..];
                self.album_track_nodes(album_id, offline_only).await
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn for_you_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let mut nodes = Vec::new();
        let offline_playlists = self.offline_playlist_set().await?;

        // Fetch contextual/seasonal/moment/discovery playlists (filtering offline_only BEFORE LIMIT).
        let playlists = if offline_only {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT DISTINCT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 INNER JOIN offline_downloads od2 ON od2.track_id = pt2.track_id WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(DISTINCT pt2.track_id) FROM playlist_tracks pt2 INNER JOIN offline_downloads od2 ON od2.track_id = pt2.track_id WHERE pt2.playlist_id = p.id)
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
                WHERE p.kind IN ('seasonal', 'moment', 'discovery', 'rolling')
                ORDER BY
                    CASE
                        WHEN LOWER(COALESCE(p.slug, '')) IN ('drive', 'drive_mix', 'drive-mix') THEN 0
                        WHEN p.kind = 'seasonal' THEN 1
                        WHEN p.kind = 'moment' THEN 2
                        ELSE 3
                    END ASC,
                    p.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_FOR_YOU_LIMIT as i64)
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
                WHERE p.kind IN ('seasonal', 'moment', 'discovery', 'rolling')
                  AND (
                      p.kind NOT IN ('seasonal', 'moment')
                      OR EXISTS (SELECT 1 FROM playlist_tracks pt2 WHERE pt2.playlist_id = p.id)
                      OR p.song_count > 0
                  )
                ORDER BY
                    CASE
                        WHEN LOWER(COALESCE(p.slug, '')) IN ('drive', 'drive_mix', 'drive-mix') THEN 0
                        WHEN p.kind = 'seasonal' THEN 1
                        WHEN p.kind = 'moment' THEN 2
                        ELSE 3
                    END ASC,
                    p.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_FOR_YOU_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let art_map = self
            .cover_art_map_for(playlists.iter().filter_map(|p| p.cover_art_id.as_deref()))
            .await?;

        let mut right_now_playlists: Vec<LocalPlaylist> = Vec::new();
        let mut discover_playlists: Vec<LocalPlaylist> = Vec::new();
        let mut has_synced_drive = false;

        for pl in playlists {
            let sleeve = resolve_playlist_sleeve_slug(&pl);
            if sleeve.as_deref() == Some("drive") {
                has_synced_drive = true;
            }
            if pl.kind == "discovery" || pl.kind == "rolling" {
                discover_playlists.push(pl);
            } else {
                right_now_playlists.push(pl);
            }
        }

        // --- Group 1: "Right Now" (Drive Mix always #1, then active seasonal & moment mixes) ---
        if !has_synced_drive {
            let (drive_count, drive_offline_count) = self.drive_mix_counts(offline_only).await?;
            if drive_count > 0 {
                let subtitle = if drive_offline_count > 0 {
                    format!("Offline-first · {drive_count} tracks")
                } else {
                    format!("Instant car mix · {drive_count} tracks")
                };
                nodes.push(AutoBrowseNode {
                    id: AUTO_FOR_YOU_DRIVE_MIX.to_string(),
                    title: "Drive Mix".to_string(),
                    subtitle: Some(subtitle),
                    kind: AutoBrowseKind::Playlist,
                    playable: true,
                    browsable: true,
                    cover_art_id: None,
                    cover_art_local_path: None,
                    is_offline_ready: drive_offline_count > 0,
                    is_pending_discovery: false,
                    group_title: Some(GROUP_RIGHT_NOW.to_string()),
                    sleeve_slug: Some("drive".to_string()),
                });
            }
        }

        for pl in right_now_playlists {
            if nodes.len() >= AUTO_FOR_YOU_LIMIT {
                break;
            }
            let offline_ready = offline_only || offline_playlists.contains(&pl.id);
            let sleeve = resolve_playlist_sleeve_slug(&pl);
            let subtitle = format_for_you_subtitle(&pl, sleeve.as_deref(), offline_ready);
            let use_sleeve = sleeve.is_some();
            nodes.push(AutoBrowseNode {
                id: format!("playlist:{}", pl.id),
                title: pl.name,
                subtitle: Some(subtitle),
                kind: AutoBrowseKind::Playlist,
                playable: true,
                browsable: true,
                cover_art_local_path: if use_sleeve {
                    None
                } else {
                    pl.cover_art_id
                        .as_deref()
                        .and_then(|cid| art_map.get(cid))
                        .cloned()
                },
                cover_art_id: if use_sleeve { None } else { pl.cover_art_id },
                is_offline_ready: offline_ready,
                is_pending_discovery: pl.kind == "seasonal",
                group_title: Some(GROUP_RIGHT_NOW.to_string()),
                sleeve_slug: sleeve,
            });
        }

        // --- Group 2: "Discover" (Weekly Radar, Comfort Mix, Deep Cuts + Discovery Triage) ---
        for pl in discover_playlists {
            if nodes.len() >= AUTO_FOR_YOU_LIMIT {
                break;
            }
            let offline_ready = offline_only || offline_playlists.contains(&pl.id);
            let sleeve = resolve_playlist_sleeve_slug(&pl).or_else(|| Some("weekly_radar".to_string()));
            let subtitle = format_for_you_subtitle(&pl, sleeve.as_deref(), offline_ready);
            nodes.push(AutoBrowseNode {
                id: format!("playlist:{}", pl.id),
                title: pl.name,
                subtitle: Some(subtitle),
                kind: AutoBrowseKind::Playlist,
                playable: true,
                browsable: true,
                cover_art_local_path: None,
                cover_art_id: None,
                is_offline_ready: offline_ready,
                is_pending_discovery: true,
                group_title: Some(GROUP_DISCOVER.to_string()),
                sleeve_slug: sleeve,
            });
        }

        // Discovery Triage sits cleanly in "Discover" with a dedicated sleeve and 1-tap playability.
        if nodes.len() < AUTO_FOR_YOU_LIMIT {
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
                let offline_pending: i64 = if offline_only {
                    pending_count
                } else {
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
                };
                nodes.push(AutoBrowseNode {
                    id: AUTO_FOR_YOU_TRIAGE.to_string(),
                    title: "Discovery Triage".to_string(),
                    subtitle: Some(format!("{pending_count} pending picks")),
                    kind: AutoBrowseKind::Folder,
                    playable: true,
                    browsable: true,
                    cover_art_id: None,
                    cover_art_local_path: None,
                    is_offline_ready: offline_pending > 0,
                    is_pending_discovery: true,
                    group_title: Some(GROUP_DISCOVER.to_string()),
                    sleeve_slug: Some("triage".to_string()),
                });
            }
        }

        // --- Group 3: "Continue Listening" (up to 4 recent albums rendered as wide list rows) ---
        let remaining = AUTO_FOR_YOU_LIMIT.saturating_sub(nodes.len());
        let continue_cap = remaining.min(AUTO_CONTINUE_LISTENING_LIMIT);
        if continue_cap > 0 {
            let continue_albums = self
                .continue_listening_album_nodes(continue_cap, offline_only)
                .await?;
            nodes.extend(continue_albums);
        }

        Ok(nodes)
    }

    async fn continue_listening_album_nodes(
        &self,
        limit: usize,
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
            .bind(limit as i64)
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
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let art_map = self
            .cover_art_map_for(albums.iter().filter_map(|a| a.cover_art_id.as_deref()))
            .await?;
        let offline_albums = self.offline_album_set().await?;
        Ok(albums
            .into_iter()
            .map(|a| {
                let is_off = offline_only || offline_albums.contains(&a.id);
                let subtitle = if is_off {
                    format!("{} · Offline · {} tracks", a.artist_name, a.song_count)
                } else if a.song_count > 0 {
                    format!("{} · {} tracks", a.artist_name, a.song_count)
                } else {
                    a.artist_name.clone()
                };
                AutoBrowseNode {
                    id: format!("album:{}", a.id),
                    title: a.name,
                    subtitle: Some(subtitle),
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
                    group_title: Some(GROUP_CONTINUE_LISTENING.to_string()),
                    sleeve_slug: None,
                }
            })
            .collect())
    }

    async fn playlist_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let mut nodes = Vec::new();

        // Pin Starred Favorites at #1 when starred tracks exist.
        let starred_count: i64 = if offline_only {
            sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.starred = 1
                "#,
            )
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar("SELECT COUNT(1) FROM tracks WHERE starred = 1")
                .fetch_one(&self.pool)
                .await?
        };
        if starred_count > 0 {
            let starred_offline: i64 = if offline_only {
                starred_count
            } else {
                sqlx::query_scalar(
                    r#"
                    SELECT COUNT(1)
                    FROM tracks t
                    INNER JOIN offline_downloads od ON od.track_id = t.id
                    WHERE t.starred = 1
                    "#,
                )
                .fetch_one(&self.pool)
                .await?
            };
            let subtitle = if starred_offline > 0 {
                format!("Offline-ready · {starred_count} favorites")
            } else {
                format!("{starred_count} favorites")
            };
            nodes.push(AutoBrowseNode {
                id: AUTO_PLAYLIST_STARRED.to_string(),
                title: "Starred Favorites".to_string(),
                subtitle: Some(subtitle),
                kind: AutoBrowseKind::Playlist,
                playable: true,
                browsable: true,
                cover_art_id: None,
                cover_art_local_path: None,
                is_offline_ready: starred_offline > 0,
                is_pending_discovery: false,
                group_title: None,
                sleeve_slug: Some("starred".to_string()),
            });
        }

        let remaining_limit = AUTO_PLAYLISTS_LIMIT.saturating_sub(nodes.len()) as i64;
        if remaining_limit <= 0 {
            return Ok(nodes);
        }

        let playlists = if offline_only {
            sqlx::query_as::<_, LocalPlaylist>(
                r#"
                SELECT DISTINCT
                    p.id,
                    p.name,
                    p.comment,
                    p.owner,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM playlist_tracks pt2 INNER JOIN offline_downloads od2 ON od2.track_id = pt2.track_id WHERE pt2.playlist_id = p.id)
                            THEN (SELECT COUNT(DISTINCT pt2.track_id) FROM playlist_tracks pt2 INNER JOIN offline_downloads od2 ON od2.track_id = pt2.track_id WHERE pt2.playlist_id = p.id)
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
                WHERE p.kind IN ('user', 'smart')
                ORDER BY p.updated_at DESC, p.name COLLATE NOCASE ASC
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
                WHERE p.kind IN ('user', 'smart')
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
            .bind(remaining_limit)
            .fetch_all(&self.pool)
            .await?
        };

        let art_map = self
            .cover_art_map_for(playlists.iter().filter_map(|p| p.cover_art_id.as_deref()))
            .await?;
        let offline_playlists = self.offline_playlist_set().await?;
        for pl in playlists {
            let offline_ready = offline_only || offline_playlists.contains(&pl.id);
            let subtitle = if pl.is_smart {
                format!("Smart · {} tracks", pl.song_count)
            } else if offline_ready {
                format!("Offline · {} tracks", pl.song_count)
            } else {
                format!("{} tracks", pl.song_count)
            };
            nodes.push(AutoBrowseNode {
                id: format!("playlist:{}", pl.id),
                title: pl.name,
                subtitle: Some(subtitle),
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
                is_pending_discovery: false,
                group_title: None,
                sleeve_slug: None,
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

        let mut nodes = Vec::with_capacity(3 + AZ_BUCKETS.len() + AZ_ALBUM_BUCKETS.len());
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
            group_title: None,
            sleeve_slug: Some("offline".to_string()),
        });
        nodes.push(folder_node(
            AUTO_LIB_RECENT,
            "Recently Added",
            Some(format!("Up to {AUTO_RECENT_LIMIT} albums")),
            AutoBrowseKind::Folder,
        ));
        nodes.push(folder_node(
            AUTO_LIB_RECENT_ARTISTS,
            "Recent Artists",
            Some(format!("Top {AUTO_RECENT_LIMIT} artists")),
            AutoBrowseKind::Folder,
        ));
        for (bucket_id, label, _, _) in AZ_BUCKETS {
            let mut n = folder_node(
                bucket_id,
                label,
                Some("Artists".to_string()),
                AutoBrowseKind::Folder,
            );
            n.group_title = Some(GROUP_ARTISTS.to_string());
            nodes.push(n);
        }
        for (bucket_id, label, _, _) in AZ_ALBUM_BUCKETS {
            let mut n = folder_node(
                bucket_id,
                label,
                Some("Albums".to_string()),
                AutoBrowseKind::Folder,
            );
            n.group_title = Some(GROUP_ALBUMS.to_string());
            nodes.push(n);
        }
        Ok(nodes)
    }

    async fn offline_track_nodes(&self) -> CoreResult<Vec<AutoBrowseNode>> {
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

        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| track_to_node(t, &art_map, true))
            .collect())
    }

    async fn drive_mix_counts(&self, offline_only: bool) -> CoreResult<(usize, usize)> {
        let tracks = self.drive_mix_tracks(offline_only).await?;
        if tracks.is_empty() {
            return Ok((0, 0));
        }
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        let offline_count = if offline_only {
            tracks.len()
        } else {
            tracks.iter().filter(|t| offline_set.contains(&t.id)).count()
        };
        Ok((tracks.len(), offline_count))
    }

    async fn drive_mix_tracks(&self, offline_only: bool) -> CoreResult<Vec<LocalTrack>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.user_rating != 1
                ORDER BY
                    t.starred DESC,
                    CASE WHEN t.user_rating >= 4 THEN 1 ELSE 0 END DESC,
                    t.play_count DESC,
                    od.pinned DESC,
                    t.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_DRIVE_MIX_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                LEFT JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.user_rating != 1
                ORDER BY
                    CASE WHEN od.track_id IS NOT NULL THEN 0 ELSE 1 END ASC,
                    t.starred DESC,
                    CASE WHEN t.user_rating >= 4 THEN 1 ELSE 0 END DESC,
                    t.play_count DESC,
                    t.updated_at DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_DRIVE_MIX_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(tracks)
    }

    async fn drive_mix_track_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = self.drive_mix_tracks(offline_only).await?;
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_only || offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn starred_track_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.starred = 1
                ORDER BY t.play_count DESC, t.updated_at DESC
                LIMIT 100
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT * FROM tracks
                WHERE starred = 1
                ORDER BY play_count DESC, updated_at DESC
                LIMIT 100
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_only || offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn shuffle_track_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.user_rating != 1
                ORDER BY RANDOM()
                LIMIT 50
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT * FROM tracks
                WHERE user_rating != 1
                ORDER BY RANDOM()
                LIMIT 50
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_only || offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn mood_radio_track_nodes(
        &self,
        mood: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let keywords: &[&str] = match mood {
            "upbeat" => &[
                "%pop%", "%dance%", "%electro%", "%funk%", "%disco%", "%house%", "%hip%", "%rock%",
                "%punk%",
            ],
            "chill" => &[
                "%ambient%", "%chill%", "%lo-fi%", "%lofi%", "%jazz%", "%soul%", "%acoustic%",
                "%folk%", "%downtempo%",
            ],
            "late_night" => &[
                "%synth%", "%electro%", "%ambient%", "%trip%", "%jazz%", "%r&b%", "%soul%",
                "%indie%", "%dream%",
            ],
            _ => &[],
        };

        let mut tracks: Vec<LocalTrack> = if !keywords.is_empty() {
            let mut qb = QueryBuilder::<Sqlite>::new(if offline_only {
                "SELECT t.* FROM tracks t INNER JOIN offline_downloads od ON od.track_id = t.id WHERE t.user_rating != 1 AND ("
            } else {
                "SELECT t.* FROM tracks t WHERE t.user_rating != 1 AND ("
            });
            for (idx, kw) in keywords.iter().enumerate() {
                if idx > 0 {
                    qb.push(" OR ");
                }
                qb.push("LOWER(COALESCE(t.genre, '')) LIKE ");
                qb.push_bind(*kw);
            }
            qb.push(") ORDER BY t.starred DESC, RANDOM() LIMIT 50");
            qb.build_query_as().fetch_all(&self.pool).await?
        } else {
            Vec::new()
        };

        if tracks.is_empty() {
            return self.shuffle_track_nodes(offline_only).await;
        }
        // Keep deterministic non-empty list
        tracks.truncate(50);
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_only || offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn genre_radio_track_nodes(
        &self,
        genre: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let tracks = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE LOWER(TRIM(COALESCE(t.genre, ''))) = LOWER(TRIM(?)) AND t.user_rating != 1
                ORDER BY t.starred DESC, RANDOM()
                LIMIT 50
                "#,
            )
            .bind(genre)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT * FROM tracks
                WHERE LOWER(TRIM(COALESCE(genre, ''))) = LOWER(TRIM(?)) AND user_rating != 1
                ORDER BY starred DESC, RANDOM()
                LIMIT 50
                "#,
            )
            .bind(genre)
            .fetch_all(&self.pool)
            .await?
        };
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_only || offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn recent_album_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
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

        let art_map = self
            .cover_art_map_for(albums.iter().filter_map(|a| a.cover_art_id.as_deref()))
            .await?;
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
                    group_title: None,
                    sleeve_slug: None,
                }
            })
            .collect())
    }

    async fn recent_artist_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let artists = if offline_only {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT DISTINCT ar.*
                FROM artists ar
                INNER JOIN tracks t ON t.artist_id = ar.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY ar.starred DESC, ar.updated_at DESC, ar.album_count DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RECENT_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT * FROM artists
                ORDER BY starred DESC, updated_at DESC, album_count DESC
                LIMIT ?
                "#,
            )
            .bind(AUTO_RECENT_LIMIT as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let art_map = self
            .cover_art_map_for(artists.iter().filter_map(|ar| ar.cover_art_id.as_deref()))
            .await?;
        let offline_artists = self.offline_artist_set().await?;
        Ok(artists
            .into_iter()
            .map(|artist| {
                let is_off = offline_only || offline_artists.contains(&artist.id);
                AutoBrowseNode {
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
                    group_title: None,
                    sleeve_slug: None,
                }
            })
            .collect())
    }

    async fn az_bucket_artists(
        &self,
        bucket_id: &str,
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
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalArtist>("SELECT * FROM artists")
                .fetch_all(&self.pool)
                .await?
        };

        let is_last_bucket = bucket_id == "az:u-z";
        // Filter into this A-Z bucket and sort by article-stripped, diacritic-folded sort key
        // BEFORE applying the 50-item cap so artists like "The Beatles" sort under "B" and are
        // never pushed past the limit by raw SQL "T..." ordering.
        let mut bucketed: Vec<(String, LocalArtist)> = artists
            .into_iter()
            .filter_map(|artist| {
                let sort_key = artist_sort_key(&artist.name);
                let first = sort_key
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase())
                    .unwrap_or('#');
                let matches = if is_last_bucket {
                    (first >= start_ch && first <= end_ch) || !first.is_ascii_alphabetic()
                } else {
                    first >= start_ch && first <= end_ch
                };
                matches.then_some((sort_key, artist))
            })
            .collect();

        bucketed.sort_by(|(ka, a), (kb, b)| {
            ka.cmp(kb)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        bucketed.truncate(AUTO_BUCKET_ARTISTS_LIMIT);

        let art_map = self
            .cover_art_map_for(
                bucketed
                    .iter()
                    .filter_map(|(_, ar)| ar.cover_art_id.as_deref()),
            )
            .await?;
        let offline_artists = self.offline_artist_set().await?;
        let nodes = bucketed
            .into_iter()
            .map(|(_, artist)| {
                let is_off = offline_only || offline_artists.contains(&artist.id);
                AutoBrowseNode {
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
                    group_title: None,
                    sleeve_slug: None,
                }
            })
            .collect();
        Ok(nodes)
    }

    async fn az_bucket_albums(
        &self,
        bucket_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        let Some(&(_, _, start_ch, end_ch)) = AZ_ALBUM_BUCKETS.iter().find(|b| b.0 == bucket_id)
        else {
            return Ok(Vec::new());
        };

        let albums = if offline_only {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT DISTINCT a.*
                FROM albums a
                INNER JOIN tracks t ON t.album_id = a.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalAlbum>("SELECT * FROM albums")
                .fetch_all(&self.pool)
                .await?
        };

        let is_last_bucket = bucket_id == "az_album:u-z";
        let mut bucketed: Vec<(String, LocalAlbum)> = albums
            .into_iter()
            .filter_map(|album| {
                let sort_key = artist_sort_key(&album.name);
                let first = sort_key
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase())
                    .unwrap_or('#');
                let matches = if is_last_bucket {
                    (first >= start_ch && first <= end_ch) || !first.is_ascii_alphabetic()
                } else {
                    first >= start_ch && first <= end_ch
                };
                matches.then_some((sort_key, album))
            })
            .collect();

        bucketed.sort_by(|(ka, a), (kb, b)| {
            ka.cmp(kb)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        bucketed.truncate(AUTO_BUCKET_ALBUMS_LIMIT);

        let art_map = self
            .cover_art_map_for(
                bucketed
                    .iter()
                    .filter_map(|(_, al)| al.cover_art_id.as_deref()),
            )
            .await?;
        let offline_albums = self.offline_album_set().await?;
        let nodes = bucketed
            .into_iter()
            .map(|(_, album)| {
                let is_off = offline_only || offline_albums.contains(&album.id);
                AutoBrowseNode {
                    id: format!("album:{}", album.id),
                    title: album.name,
                    subtitle: Some(album.artist_name),
                    kind: AutoBrowseKind::Album,
                    playable: true,
                    browsable: true,
                    cover_art_local_path: album
                        .cover_art_id
                        .as_deref()
                        .and_then(|cid| art_map.get(cid))
                        .cloned(),
                    cover_art_id: album.cover_art_id,
                    is_offline_ready: is_off,
                    is_pending_discovery: false,
                    group_title: None,
                    sleeve_slug: None,
                }
            })
            .collect();
        Ok(nodes)
    }

    async fn radio_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
        let mut nodes = Vec::new();

        let track_count: i64 = if offline_only {
            sqlx::query_scalar(
                "SELECT COUNT(1) FROM tracks t INNER JOIN offline_downloads od ON od.track_id = t.id WHERE t.user_rating != 1",
            )
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar("SELECT COUNT(1) FROM tracks WHERE user_rating != 1")
                .fetch_one(&self.pool)
                .await?
        };
        let has_offline_tracks: bool = if offline_only {
            track_count > 0
        } else {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(1) FROM offline_downloads")
                .fetch_one(&self.pool)
                .await?
                > 0
        };

        // --- Group 1: Quick Stations (Library Shuffle + Mood Stations) ---
        if track_count > 0 {
            nodes.push(AutoBrowseNode {
                id: AUTO_RADIO_SHUFFLE.to_string(),
                title: "Library Shuffle".to_string(),
                subtitle: Some(if has_offline_tracks {
                    "Instant mix · Offline ready".to_string()
                } else {
                    "Instant mix".to_string()
                }),
                kind: AutoBrowseKind::RadioSeed,
                playable: true,
                browsable: false,
                cover_art_id: None,
                cover_art_local_path: None,
                is_offline_ready: has_offline_tracks,
                is_pending_discovery: false,
                group_title: Some(GROUP_QUICK_STATIONS.to_string()),
                sleeve_slug: Some("shuffle".to_string()),
            });

            for (id, title, sub, sleeve) in [
                (
                    AUTO_RADIO_MOOD_UPBEAT,
                    "Upbeat",
                    "High-energy station",
                    "mood:upbeat",
                ),
                (
                    AUTO_RADIO_MOOD_CHILL,
                    "Chill",
                    "Laid-back station",
                    "mood:chill",
                ),
                (
                    AUTO_RADIO_MOOD_LATE_NIGHT,
                    "Late Night",
                    "After-hours station",
                    "late_night",
                ),
            ] {
                nodes.push(AutoBrowseNode {
                    id: id.to_string(),
                    title: title.to_string(),
                    subtitle: Some(sub.to_string()),
                    kind: AutoBrowseKind::RadioSeed,
                    playable: true,
                    browsable: false,
                    cover_art_id: None,
                    cover_art_local_path: None,
                    is_offline_ready: has_offline_tracks,
                    is_pending_discovery: false,
                    group_title: Some(GROUP_QUICK_STATIONS.to_string()),
                    sleeve_slug: Some(sleeve.to_string()),
                });
            }
        }

        // --- Group 2: Top 4 User Genre Radios ---
        let genres: Vec<(String, i64)> = if offline_only {
            sqlx::query_as(
                r#"
                SELECT TRIM(t.genre) AS g, COUNT(1) AS cnt
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.genre IS NOT NULL AND TRIM(t.genre) != '' AND t.user_rating != 1
                GROUP BY LOWER(TRIM(t.genre))
                ORDER BY cnt DESC
                LIMIT 4
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                r#"
                SELECT TRIM(genre) AS g, COUNT(1) AS cnt
                FROM tracks
                WHERE genre IS NOT NULL AND TRIM(genre) != '' AND user_rating != 1
                GROUP BY LOWER(TRIM(genre))
                ORDER BY cnt DESC
                LIMIT 4
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        let offline_genres: HashSet<String> = if offline_only {
            HashSet::new()
        } else {
            sqlx::query_scalar::<_, String>(
                r#"
                SELECT DISTINCT LOWER(TRIM(t.genre))
                FROM tracks t
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE t.genre IS NOT NULL AND TRIM(t.genre) != '' AND t.user_rating != 1
                "#,
            )
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect()
        };
        for (genre, cnt) in genres {
            if nodes.len() >= AUTO_RADIO_LIMIT {
                break;
            }
            let genre_offline = offline_only || offline_genres.contains(&genre.to_ascii_lowercase());
            nodes.push(AutoBrowseNode {
                id: format!("radio:genre:{genre}"),
                title: format!("{genre} Radio"),
                subtitle: Some(format!("{cnt} tracks")),
                kind: AutoBrowseKind::RadioSeed,
                playable: true,
                browsable: false,
                cover_art_id: None,
                cover_art_local_path: None,
                is_offline_ready: genre_offline,
                is_pending_discovery: false,
                group_title: Some(GROUP_GENRES.to_string()),
                sleeve_slug: Some(format!("genre:{genre}")),
            });
        }

        // --- Group 3: Artist Radio (top starred / largest artists) ---
        let artist_limit = AUTO_RADIO_LIMIT.saturating_sub(nodes.len()).min(8);
        if artist_limit > 0 {
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
                .bind(artist_limit as i64)
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
                .bind(artist_limit as i64)
                .fetch_all(&self.pool)
                .await?
            };

            let art_map = self
                .cover_art_map_for(artists.iter().filter_map(|ar| ar.cover_art_id.as_deref()))
                .await?;
            let offline_artists = self.offline_artist_set().await?;
            for ar in artists {
                let is_off = offline_only || offline_artists.contains(&ar.id);
                nodes.push(AutoBrowseNode {
                    id: format!("radio:artist:{}", ar.id),
                    title: format!("{} Radio", ar.name),
                    subtitle: Some("Artist station".to_string()),
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
                    group_title: Some(GROUP_ARTIST_RADIO.to_string()),
                    sleeve_slug: None,
                });
            }
        }

        Ok(nodes)
    }

    async fn pending_triage_nodes(&self, offline_only: bool) -> CoreResult<Vec<AutoBrowseNode>> {
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

        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn playlist_track_nodes(
        &self,
        playlist_id: &str,
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
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    async fn artist_album_nodes(
        &self,
        artist_id: &str,
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
        let art_map = self
            .cover_art_map_for(albums.iter().filter_map(|a| a.cover_art_id.as_deref()))
            .await?;
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
                    group_title: None,
                    sleeve_slug: None,
                }
            })
            .collect())
    }

    async fn album_track_nodes(
        &self,
        album_id: &str,
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
        let art_map = self
            .cover_art_map_for(tracks.iter().filter_map(|t| t.cover_art_id.as_deref()))
            .await?;
        let offline_set = self
            .offline_track_set_for(tracks.iter().map(|t| t.id.as_str()))
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| {
                let is_off = offline_set.contains(&t.id);
                track_to_node(t, &art_map, is_off)
            })
            .collect())
    }

    /// Sub-10ms pure local SQLite voice command resolver for Android Auto Assistant intents
    /// (e.g., `"Play Drive Mix"`, `"Play Noël mix"`, `"Play Daft Punk"`, `"Play Get Lucky"`).
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

        // 1b. Built-in virtual mixes ("Drive Mix", "Starred Favorites", "Library Shuffle", "Discovery Triage")
        let is_exact_playlist = best
            .as_ref()
            .is_some_and(|b| b.kind == VoiceMatchKind::Playlist && b.score.0 == 4);
        if !is_exact_playlist {
            if query_variants.iter().any(|q| *q == "drive" || *q == "drive mix") {
                let tracks = self.drive_mix_tracks(offline_only).await?;
                if !tracks.is_empty() {
                    consider(
                        &mut best,
                        RankedCandidate {
                            score: (4, 3, 2, 10_000),
                            kind: VoiceMatchKind::Playlist,
                            target_id: AUTO_FOR_YOU_DRIVE_MIX.to_string(),
                            label: "Drive Mix".to_string(),
                            track_ids: tracks.into_iter().map(|t| t.id).collect(),
                        },
                    );
                }
            } else if query_variants
                .iter()
                .any(|q| *q == "starred" || *q == "starred favorites" || *q == "favorites" || *q == "liked")
            {
                let nodes = self.starred_track_nodes(offline_only).await?;
                if !nodes.is_empty() {
                    consider(
                        &mut best,
                        RankedCandidate {
                            score: (4, 3, 2, 10_000),
                            kind: VoiceMatchKind::Playlist,
                            target_id: AUTO_PLAYLIST_STARRED.to_string(),
                            label: "Starred Favorites".to_string(),
                            track_ids: nodes
                                .into_iter()
                                .map(|n| n.id.trim_start_matches("track:").to_string())
                                .collect(),
                        },
                    );
                }
            } else if query_variants
                .iter()
                .any(|q| *q == "shuffle" || *q == "library shuffle")
            {
                let nodes = self.shuffle_track_nodes(offline_only).await?;
                if !nodes.is_empty() {
                    consider(
                        &mut best,
                        RankedCandidate {
                            score: (4, 3, 1, 10_000),
                            kind: VoiceMatchKind::Playlist,
                            target_id: AUTO_RADIO_SHUFFLE.to_string(),
                            label: "Library Shuffle".to_string(),
                            track_ids: nodes
                                .into_iter()
                                .map(|n| n.id.trim_start_matches("track:").to_string())
                                .collect(),
                        },
                    );
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
                    label: format!("{} - {}", first.artist_name, first.title),
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

    async fn cover_art_map_for<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> CoreResult<HashMap<String, String>> {
        let mut unique: Vec<&'a str> = Vec::new();
        let mut seen: HashSet<&'a str> = HashSet::new();
        for id in ids {
            if !id.is_empty() && seen.insert(id) {
                unique.push(id);
            }
        }
        if unique.is_empty() {
            return Ok(HashMap::new());
        }

        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT cover_art_id, local_path FROM cover_art_cache WHERE cover_art_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in &unique {
            sep.push_bind(*id);
        }
        sep.push_unseparated(")");
        let rows: Vec<(String, String)> = qb.build_query_as().fetch_all(&self.pool).await?;

        let mut map = HashMap::with_capacity(rows.len());
        for (id, path) in rows {
            let is_file = tokio::fs::metadata(&path)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false);
            if is_file {
                map.insert(id, path);
            }
        }
        Ok(map)
    }

    async fn offline_track_set_for<'a>(
        &self,
        track_ids: impl IntoIterator<Item = &'a str>,
    ) -> CoreResult<HashSet<String>> {
        let mut unique: Vec<&'a str> = Vec::new();
        let mut seen: HashSet<&'a str> = HashSet::new();
        for id in track_ids {
            if seen.insert(id) {
                unique.push(id);
            }
        }
        if unique.is_empty() {
            return Ok(HashSet::new());
        }
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT track_id FROM offline_downloads WHERE track_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in &unique {
            sep.push_bind(*id);
        }
        sep.push_unseparated(")");
        let ids: Vec<String> = qb.build_query_scalar().fetch_all(&self.pool).await?;
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

fn resolve_playlist_sleeve_slug(pl: &LocalPlaylist) -> Option<String> {
    if let Some(raw) = pl.slug.as_deref() {
        let norm = normalize_sleeve_slug(raw);
        if !norm.is_empty() {
            return Some(norm);
        }
    }
    let lower = fold_diacritics_ascii(&pl.name);
    if lower.contains("drive") {
        Some("drive".to_string())
    } else if lower.contains("weekly radar") || lower.contains("balanced") {
        Some("weekly_radar".to_string())
    } else if lower.contains("comfort") || lower.contains("conservative") {
        Some("comfort".to_string())
    } else if lower.contains("deep cuts") || lower.contains("adventurous") {
        Some("deep_cuts".to_string())
    } else if lower.contains("noel") || lower.contains("christmas") || lower.contains("winter holiday") {
        Some("winter_holiday".to_string())
    } else if lower.contains("new year") || lower.contains("nye") {
        Some("new_years_eve".to_string())
    } else if lower.contains("cozy winter") {
        Some("cozy_winter".to_string())
    } else if lower.contains("lunar") {
        Some("lunar_new_year".to_string())
    } else if lower.contains("valentine") {
        Some("valentines".to_string())
    } else if lower.contains("carnival") {
        Some("carnival".to_string())
    } else if lower.contains("celtic") {
        Some("celtic_spring".to_string())
    } else if lower.contains("solstice") {
        Some("summer_solstice".to_string())
    } else if lower.contains("late summer") {
        Some("late_summer_reset".to_string())
    } else if lower.contains("halloween") {
        Some("halloween".to_string())
    } else if lower.contains("spring") {
        Some("spring".to_string())
    } else if lower.contains("summer") {
        Some("summer".to_string())
    } else if lower.contains("autumn") || lower.contains("fall") {
        Some("autumn".to_string())
    } else if lower.contains("rainy") {
        Some("rainy_day".to_string())
    } else if lower.contains("snow") {
        Some("snow_day".to_string())
    } else if lower.contains("storm") {
        Some("storm_night".to_string())
    } else if lower.contains("sunny") {
        Some("sunny_day".to_string())
    } else if lower.contains("evening") || lower.contains("unwind") {
        Some("evening_unwind".to_string())
    } else if lower.contains("friday") {
        Some("friday_night".to_string())
    } else if lower.contains("sunday") {
        Some("sunday_morning".to_string())
    } else if lower.contains("morning") {
        Some("morning".to_string())
    } else if lower.contains("focus") {
        Some("focus".to_string())
    } else if lower.contains("dinner") {
        Some("dinner_groove".to_string())
    } else if lower.contains("late night") {
        Some("late_night".to_string())
    } else if lower.contains("forgotten") {
        Some("forgotten_gems".to_string())
    } else if lower.contains("fresh") {
        Some("fresh_arrivals".to_string())
    } else if lower.contains("80s") {
        Some("decade_80s".to_string())
    } else if lower.contains("90s") {
        Some("decade_90s".to_string())
    } else if lower.contains("2000s") || lower.contains("00s") {
        Some("decade_2000s".to_string())
    } else if pl.kind == "discovery" || pl.kind == "rolling" {
        Some("weekly_radar".to_string())
    } else if pl.kind == "seasonal" || pl.kind == "moment" {
        Some("fallback".to_string())
    } else {
        None
    }
}

fn normalize_sleeve_slug(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    let stripped = trimmed
        .strip_prefix("mix.seasonal.")
        .or_else(|| trimmed.strip_prefix("mix.moment."))
        .or_else(|| trimmed.strip_prefix("mix.rolling."))
        .or_else(|| trimmed.strip_prefix("mix.custom."))
        .unwrap_or(&trimmed);
    let normalized = stripped.replace('-', "_");
    match normalized.as_str() {
        "drive_mix" => "drive".to_string(),
        "balanced" => "weekly_radar".to_string(),
        "conservative" => "comfort".to_string(),
        "adventurous" => "deep_cuts".to_string(),
        "noel" | "christmas" => "winter_holiday".to_string(),
        _ => normalized,
    }
}

fn format_for_you_subtitle(
    pl: &LocalPlaylist,
    sleeve: Option<&str>,
    offline_ready: bool,
) -> String {
    let count = pl.song_count;
    let track_word = if count == 1 { "track" } else { "tracks" };
    if sleeve == Some("drive") {
        return if offline_ready {
            format!("Offline-first · {count} {track_word}")
        } else {
            format!("Car mix · {count} {track_word}")
        };
    }
    match pl.kind.as_str() {
        "seasonal" => {
            if offline_ready {
                format!("Seasonal · Offline · {count} {track_word}")
            } else {
                format!("Seasonal · {count} {track_word}")
            }
        }
        "discovery" | "rolling" => {
            if offline_ready {
                format!("Offline · {count} Discovery picks")
            } else {
                format!("{count} Discovery picks")
            }
        }
        _ => {
            if offline_ready {
                format!("Offline · {count} {track_word}")
            } else {
                format!("{count} {track_word}")
            }
        }
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
        group_title: None,
        sleeve_slug: None,
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
        group_title: None,
        sleeve_slug: None,
    }
}

fn artist_sort_key(name: &str) -> String {
    let trimmed = name.trim();
    let stripped = if trimmed
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("the "))
    {
        trimmed[4..].trim_start()
    } else {
        trimmed
    };
    fold_diacritics_ascii(stripped)
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
