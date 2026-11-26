use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use reqwest::Client;
use serde_json::json;
use shared::metadata::SearchResult;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::error::CoreResult;
use crate::models::{
    LocalAlbum, LocalArtist, LocalTrack, RemoteCatalogTrack, SearchResults, SoulbeetAuth,
};

/// Sub-millisecond local FTS5 search engine merged with online Soulbeet MusicBrainz catalog results.
/// Completely suppresses remote queries when `offline_only == true`.
#[derive(Clone)]
pub struct SearchEngine {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
}

impl SearchEngine {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        soulbeet_base_url: Option<String>,
        soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    ) -> Self {
        Self {
            pool,
            http,
            soulbeet_base_url,
            soulbeet_auth,
        }
    }

    /// Runs a unified search: instant local FTS5 tracks + albums + artists, optionally merged
    /// with Soulbeet remote catalog results when `offline_only` is false.
    pub async fn search(
        &self,
        raw_query: &str,
        limit: usize,
        offline_only: bool,
    ) -> CoreResult<SearchResults> {
        let start = Instant::now();
        let trimmed = raw_query.trim();
        let cap = limit.clamp(1, 100) as i64;

        if trimmed.is_empty() {
            return Ok(SearchResults {
                query: String::new(),
                offline_only,
                elapsed_ms: 0,
                local_tracks: Vec::new(),
                local_albums: Vec::new(),
                local_artists: Vec::new(),
                remote_tracks: Vec::new(),
            });
        }

        let local_tracks = self.search_local_tracks(trimmed, cap, offline_only).await?;
        let like_pattern = format!("%{}%", trimmed.to_lowercase());

        let local_albums = if offline_only {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT DISTINCT a.*
                FROM albums a
                INNER JOIN tracks t ON t.album_id = a.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE LOWER(a.name) LIKE ? OR LOWER(a.artist_name) LIKE ?
                ORDER BY a.name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(&like_pattern)
            .bind(&like_pattern)
            .bind(cap)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalAlbum>(
                r#"
                SELECT * FROM albums
                WHERE LOWER(name) LIKE ? OR LOWER(artist_name) LIKE ?
                ORDER BY name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(&like_pattern)
            .bind(&like_pattern)
            .bind(cap)
            .fetch_all(&self.pool)
            .await?
        };

        let local_artists = if offline_only {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT DISTINCT ar.*
                FROM artists ar
                INNER JOIN tracks t ON t.artist_id = ar.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE LOWER(ar.name) LIKE ?
                ORDER BY ar.name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(&like_pattern)
            .bind(cap)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalArtist>(
                r#"
                SELECT * FROM artists
                WHERE LOWER(name) LIKE ?
                ORDER BY name COLLATE NOCASE ASC
                LIMIT ?
                "#,
            )
            .bind(&like_pattern)
            .bind(cap)
            .fetch_all(&self.pool)
            .await?
        };

        // Strictly suppress network catalog lookup in offline-only mode.
        let remote_tracks = if !offline_only {
            match self.fetch_remote_catalog(trimmed, limit).await {
                Ok(remote) => deduplicate_remote(&local_tracks, remote),
                Err(err) => {
                    tracing::warn!(query = %trimmed, "remote catalog search failed: {err}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        Ok(SearchResults {
            query: trimmed.to_string(),
            offline_only,
            elapsed_ms: start.elapsed().as_millis() as u64,
            local_tracks,
            local_albums,
            local_artists,
            remote_tracks,
        })
    }

    pub async fn search_local_tracks(
        &self,
        raw_query: &str,
        limit: i64,
        offline_only: bool,
    ) -> CoreResult<Vec<LocalTrack>> {
        let Some(fts_query) = build_fts5_query(raw_query) else {
            return Ok(Vec::new());
        };

        let rows = if offline_only {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks_fts
                INNER JOIN tracks t ON t.rowid = tracks_fts.rowid
                INNER JOIN offline_downloads od ON od.track_id = t.id
                WHERE tracks_fts MATCH ?
                ORDER BY bm25(tracks_fts, 10.0, 6.0, 3.0, 1.0) ASC, t.play_count DESC
                LIMIT ?
                "#,
            )
            .bind(&fts_query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LocalTrack>(
                r#"
                SELECT t.*
                FROM tracks_fts
                INNER JOIN tracks t ON t.rowid = tracks_fts.rowid
                WHERE tracks_fts MATCH ?
                ORDER BY bm25(tracks_fts, 10.0, 6.0, 3.0, 1.0) ASC, t.play_count DESC
                LIMIT ?
                "#,
            )
            .bind(&fts_query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn fetch_remote_catalog(
        &self,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<RemoteCatalogTrack>> {
        let Some(base) = self.soulbeet_base_url.as_deref() else {
            return Ok(Vec::new());
        };
        let url = format!("{}/api/metadata/search/track", base.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&json!({
            "input": {
                "artist": null,
                "query": query,
            }
        }));
        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            req = req.header(name, val);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = status.as_u16(),
                query = %query,
                body = %body,
                "POST /api/metadata/search/track returned non-2xx"
            );
            return Ok(Vec::new());
        }

        let items: Vec<SearchResult> = match resp.json().await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    query = %query,
                    "failed to decode POST /api/metadata/search/track response: {err}"
                );
                return Ok(Vec::new());
            }
        };

        Ok(items
            .into_iter()
            .filter_map(|item| match item {
                SearchResult::Track(t) => Some(RemoteCatalogTrack {
                    duration_secs: t.duration_secs().map(u64::from),
                    year: parse_release_year(t.release_date.as_deref()),
                    mbid: t.mbid,
                    title: t.title,
                    artist: t.artist,
                    album: t.album_title,
                }),
                SearchResult::Album(_) => None,
            })
            .take(limit.clamp(1, 100))
            .collect())
    }
}

fn parse_release_year(release_date: Option<&str>) -> Option<i32> {
    release_date?.get(..4)?.parse::<i32>().ok()
}

/// Converts arbitrary user input into a safe FTS5 prefix-matching query string.
/// Punctuation and FTS operators are stripped so user text can never cause syntax errors.
pub fn build_fts5_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{}\"*", tok.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

fn deduplicate_remote(
    local: &[LocalTrack],
    remote: Vec<RemoteCatalogTrack>,
) -> Vec<RemoteCatalogTrack> {
    let owned_keys: HashSet<(String, String)> = local
        .iter()
        .map(|t| {
            (
                t.artist_name.trim().to_ascii_lowercase(),
                t.title.trim().to_ascii_lowercase(),
            )
        })
        .collect();
    remote
        .into_iter()
        .filter(|r| {
            !owned_keys.contains(&(
                r.artist.trim().to_ascii_lowercase(),
                r.title.trim().to_ascii_lowercase(),
            ))
        })
        .collect()
}
