use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use reqwest::Client;
use serde_json::{json, Value};
use shared::metadata::{
    Album as WireAlbum, ArtistCatalog, CatalogAlbum, CatalogAlbumDetail, CatalogArtist,
    CatalogProvider, CatalogTrack, UnifiedCatalogSearch,
};
use shared::search_norm::{
    album_dedup_key, base_song_title_key, canonical_artist_key, classify_track_variant,
    fold_search_text, track_dedup_key, TrackVariantKind,
};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use tokio::sync::RwLock;

const MAX_SEARCH_TOKENS: usize = 16;

use crate::error::{CoreError, CoreResult};
use crate::models::{
    LocalAlbum, LocalArtist, LocalTrack, RemoteAlbumDetail, RemoteArtistDetail, RemoteCatalogAlbum,
    RemoteCatalogArtist, RemoteCatalogResults, RemoteCatalogTrack, SearchResults, SoulbeetAuth,
};

/// Sub-millisecond local FTS5 search engine paired with online Soulbeet unified catalog search,
/// album detail, artist discography, and 1-tap remote album download.
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

    /// Runs an instant local-only search (SQLite FTS5 tracks + local albums + local artists)
    /// with zero network calls.
    pub async fn search_local(
        &self,
        raw_query: &str,
        limit: usize,
        offline_only: bool,
    ) -> CoreResult<SearchResults> {
        let start = Instant::now();
        let trimmed = raw_query.trim();
        let cap = limit.clamp(1, 100);

        if trimmed.is_empty() {
            return Ok(SearchResults {
                query: String::new(),
                offline_only,
                elapsed_ms: 0,
                local_tracks: Vec::new(),
                local_albums: Vec::new(),
                local_artists: Vec::new(),
                remote_tracks: Vec::new(),
                remote_albums: Vec::new(),
                remote_artists: Vec::new(),
                remote_provider: None,
                remote_fallback_used: false,
            });
        }

        let local_tracks = self
            .search_local_tracks(trimmed, cap as i64, offline_only)
            .await?;
        let local_albums = self.search_local_albums(trimmed, cap, offline_only).await?;
        let local_artists = self
            .search_local_artists(trimmed, cap, offline_only)
            .await?;

        Ok(SearchResults {
            query: trimmed.to_string(),
            offline_only,
            elapsed_ms: start.elapsed().as_millis() as u64,
            local_tracks,
            local_albums,
            local_artists,
            remote_tracks: Vec::new(),
            remote_albums: Vec::new(),
            remote_artists: Vec::new(),
            remote_provider: None,
            remote_fallback_used: false,
        })
    }

    /// Runs a remote unified catalog search (`POST /api/metadata/search/unified`) and filters out
    /// tracks and albums already owned in the local SQLite mirror using canonical normalization.
    ///
    /// Returns empty immediately when `offline_only == true`, query is blank, or Soulbeet
    /// URL/auth is not configured.
    pub async fn search_remote(
        &self,
        raw_query: &str,
        provider: Option<CatalogProvider>,
        limit: usize,
        offline_only: bool,
    ) -> CoreResult<RemoteCatalogResults> {
        let trimmed = raw_query.trim();
        let requested_provider = provider
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();

        if offline_only || trimmed.is_empty() {
            return Ok(RemoteCatalogResults {
                query: trimmed.to_string(),
                provider: requested_provider,
                fallback_used: false,
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
            });
        }

        let Some(base) = self.soulbeet_base_url.as_deref() else {
            return Ok(RemoteCatalogResults {
                query: trimmed.to_string(),
                provider: requested_provider,
                fallback_used: false,
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
            });
        };

        let auth_header = {
            let guard = self.soulbeet_auth.read().await;
            guard.as_ref().map(SoulbeetAuth::header_name_and_value)
        };
        let Some((header_name, header_val)) = auth_header else {
            return Ok(RemoteCatalogResults {
                query: trimmed.to_string(),
                provider: requested_provider,
                fallback_used: false,
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
            });
        };

        let cap = limit.clamp(1, 100);
        let url = format!("{}/api/metadata/search/unified", base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header(header_name, header_val)
            .json(&json!({
                "input": {
                    "query": trimmed,
                    "provider": provider,
                    "limit": cap,
                }
            }))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }

        let payload: UnifiedCatalogSearch = resp.json().await?;
        let raw_tracks: Vec<RemoteCatalogTrack> =
            payload.tracks.into_iter().map(convert_track).collect();
        let raw_albums: Vec<RemoteCatalogAlbum> =
            payload.albums.into_iter().map(convert_album).collect();
        let artists: Vec<RemoteCatalogArtist> = payload
            .artists
            .into_iter()
            .map(convert_artist)
            .take(cap)
            .collect();

        let tracks = self
            .deduplicate_remote_tracks(raw_tracks)
            .await?
            .into_iter()
            .take(cap)
            .collect();
        let albums = self
            .deduplicate_remote_albums(raw_albums)
            .await?
            .into_iter()
            .take(cap)
            .collect();

        Ok(RemoteCatalogResults {
            query: if payload.query.is_empty() {
                trimmed.to_string()
            } else {
                payload.query
            },
            provider: payload.provider.as_str().to_string(),
            fallback_used: payload.fallback_used,
            tracks,
            albums,
            artists,
        })
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
        let mut results = self.search_local(raw_query, limit, offline_only).await?;
        if results.query.is_empty() {
            return Ok(results);
        }

        if !offline_only {
            match self.search_remote(&results.query, None, limit, false).await {
                Ok(remote) => {
                    results.remote_tracks = remote.tracks;
                    results.remote_albums = remote.albums;
                    results.remote_artists = remote.artists;
                    results.remote_provider =
                        (!remote.provider.is_empty()).then_some(remote.provider);
                    results.remote_fallback_used = remote.fallback_used;
                }
                Err(err) => {
                    tracing::warn!(
                        query = %results.query,
                        "remote unified catalog search failed: {err}"
                    );
                }
            }
        }

        results.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(results)
    }

    /// Fetches full remote album detail (album metadata + ordered tracklist) via
    /// `POST /api/metadata/album/detail`.
    pub async fn get_remote_album(&self, album_id: &str) -> CoreResult<RemoteAlbumDetail> {
        let trimmed = album_id.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidInput(
                "album_id must not be empty".to_string(),
            ));
        }
        let base = self
            .soulbeet_base_url
            .as_deref()
            .ok_or(CoreError::NotConfigured("soulbeet_base_url"))?;
        let url = format!("{}/api/metadata/album/detail", base.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&json!({
            "input": {
                "id": trimmed,
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
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }
        let detail: CatalogAlbumDetail = resp.json().await?;
        Ok(RemoteAlbumDetail {
            album: convert_album(detail.album),
            tracks: detail.tracks.into_iter().map(convert_track).collect(),
        })
    }

    /// Fetches full remote artist detail (artist info + disambiguation list + top tracks +
    /// discography) via `POST /api/metadata/artist`.
    pub async fn get_remote_artist(
        &self,
        artist_id: &str,
        name: Option<&str>,
    ) -> CoreResult<Option<RemoteArtistDetail>> {
        let trimmed = artist_id.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidInput(
                "artist_id must not be empty".to_string(),
            ));
        }
        let base = self
            .soulbeet_base_url
            .as_deref()
            .ok_or(CoreError::NotConfigured("soulbeet_base_url"))?;
        let url = format!("{}/api/metadata/artist", base.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&json!({
            "input": {
                "id": trimmed,
                "name": name.map(str::trim).filter(|s| !s.is_empty()),
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
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }
        let catalog: Option<ArtistCatalog> = resp.json().await?;
        Ok(catalog.map(|c| RemoteArtistDetail {
            artist: convert_artist(c.artist),
            provider: c.provider.as_str().to_string(),
            others: c.others.into_iter().map(convert_artist).collect(),
            top_tracks: c.top_tracks.into_iter().map(convert_track).collect(),
            albums: c.albums.into_iter().map(convert_album).collect(),
        }))
    }

    /// Triggers a 1-tap server-side album download via `POST /api/download/album` and returns
    /// the spawned `batch_id`.
    pub async fn download_remote_album(&self, album: &RemoteCatalogAlbum) -> CoreResult<String> {
        if album.id.trim().is_empty() || album.title.trim().is_empty() {
            return Err(CoreError::InvalidInput(
                "album id and title must not be empty".to_string(),
            ));
        }
        let base = self
            .soulbeet_base_url
            .as_deref()
            .ok_or(CoreError::NotConfigured("soulbeet_base_url"))?;
        let url = format!("{}/api/download/album", base.trim_end_matches('/'));
        let wire_album = WireAlbum {
            id: album.id.clone(),
            title: album.title.clone(),
            artist: album.artist.clone(),
            release_date: album.release_date.clone(),
            mbid: album.mbid.clone(),
            cover_url: album.cover_url.clone(),
            release_types: if album.release_types.is_empty() {
                None
            } else {
                Some(album.release_types.clone())
            },
            first_release_date: None,
        };
        let mut req = self.http.post(&url).json(&json!({
            "req": {
                "album": wire_album,
                "folder_id": Value::Null,
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
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body,
            });
        }

        let val: Value = resp.json().await?;
        if let Some(batch_id) = val
            .get("Accepted")
            .and_then(|v| v.get("batch_id"))
            .and_then(Value::as_str)
        {
            return Ok(batch_id.to_string());
        }
        if let Some(err_obj) = val.get("Error") {
            let msg = err_obj
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err_obj.to_string());
            return Err(CoreError::ServerHttp {
                status: status.as_u16(),
                body: msg,
            });
        }
        Err(CoreError::ServerHttp {
            status: status.as_u16(),
            body: format!("unexpected /api/download/album response: {val}"),
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

    async fn search_local_albums(
        &self,
        trimmed: &str,
        cap: usize,
        offline_only: bool,
    ) -> CoreResult<Vec<LocalAlbum>> {
        let folded_query = fold_search_text(trimmed);
        let tokens: Vec<&str> = folded_query
            .split_whitespace()
            .take(MAX_SEARCH_TOKENS)
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        // Scan only lightweight (id, name, artist_name) columns for diacritic-folded matching,
        // then hydrate at most `cap` full `LocalAlbum` rows.
        let candidates: Vec<(String, String, String)> = if offline_only {
            sqlx::query_as(
                r#"
                SELECT DISTINCT a.id, a.name, a.artist_name
                FROM albums a
                INNER JOIN tracks t ON t.album_id = a.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY a.name COLLATE NOCASE ASC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as("SELECT id, name, artist_name FROM albums ORDER BY name COLLATE NOCASE ASC")
                .fetch_all(&self.pool)
                .await?
        };

        let matched_ids: Vec<String> = candidates
            .into_iter()
            .filter(|(_, name, artist_name)| {
                let haystack = format!(
                    "{} {}",
                    fold_search_text(name),
                    fold_search_text(artist_name)
                );
                tokens.iter().all(|tok| haystack.contains(tok))
            })
            .take(cap)
            .map(|(id, _, _)| id)
            .collect();

        if matched_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM albums WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in &matched_ids {
            sep.push_bind(id);
        }
        sep.push_unseparated(")");
        let rows = qb.build_query_as::<LocalAlbum>().fetch_all(&self.pool).await?;
        let mut by_id: HashMap<String, LocalAlbum> =
            rows.into_iter().map(|a| (a.id.clone(), a)).collect();
        Ok(matched_ids
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect())
    }

    async fn search_local_artists(
        &self,
        trimmed: &str,
        cap: usize,
        offline_only: bool,
    ) -> CoreResult<Vec<LocalArtist>> {
        let folded_query = fold_search_text(trimmed);
        let tokens: Vec<&str> = folded_query
            .split_whitespace()
            .take(MAX_SEARCH_TOKENS)
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        // Scan only lightweight (id, name) columns for diacritic-folded matching,
        // then hydrate at most `cap` full `LocalArtist` rows.
        let candidates: Vec<(String, String)> = if offline_only {
            sqlx::query_as(
                r#"
                SELECT DISTINCT ar.id, ar.name
                FROM artists ar
                INNER JOIN tracks t ON t.artist_id = ar.id
                INNER JOIN offline_downloads od ON od.track_id = t.id
                ORDER BY ar.name COLLATE NOCASE ASC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as("SELECT id, name FROM artists ORDER BY name COLLATE NOCASE ASC")
                .fetch_all(&self.pool)
                .await?
        };

        let matched_ids: Vec<String> = candidates
            .into_iter()
            .filter(|(_, name)| {
                let haystack = fold_search_text(name);
                tokens.iter().all(|tok| haystack.contains(tok))
            })
            .take(cap)
            .map(|(id, _)| id)
            .collect();

        if matched_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM artists WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in &matched_ids {
            sep.push_bind(id);
        }
        sep.push_unseparated(")");
        let rows = qb.build_query_as::<LocalArtist>().fetch_all(&self.pool).await?;
        let mut by_id: HashMap<String, LocalArtist> =
            rows.into_iter().map(|a| (a.id.clone(), a)).collect();
        Ok(matched_ids
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect())
    }

    async fn deduplicate_remote_tracks(
        &self,
        remote: Vec<RemoteCatalogTrack>,
    ) -> CoreResult<Vec<RemoteCatalogTrack>> {
        if remote.is_empty() {
            return Ok(remote);
        }

        let owned_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT artist_name, title FROM tracks")
                .fetch_all(&self.pool)
                .await?;

        Ok(deduplicate_tracks_against_owned(&owned_rows, remote))
    }

    async fn deduplicate_remote_albums(
        &self,
        remote: Vec<RemoteCatalogAlbum>,
    ) -> CoreResult<Vec<RemoteCatalogAlbum>> {
        if remote.is_empty() {
            return Ok(remote);
        }

        let owned_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT artist_name, name FROM albums")
                .fetch_all(&self.pool)
                .await?;

        Ok(deduplicate_albums_against_owned(&owned_rows, remote))
    }
}

fn convert_track(t: CatalogTrack) -> RemoteCatalogTrack {
    let duration_secs = t
        .duration_secs
        .or_else(|| t.to_track().duration_secs())
        .unwrap_or(0);
    let year = parse_release_year(t.release_date.as_deref());
    RemoteCatalogTrack {
        id: t.id,
        mbid: t.mbid.unwrap_or_default(),
        release_mbid: t.release_mbid,
        title: t.title,
        artist: t.artist,
        artist_id: t.artist_id,
        album: t.album_title.unwrap_or_default(),
        album_id: t.album_id,
        duration_secs,
        year,
        cover_url: t.cover_url,
        variant_label: t.variant_label,
        provider: t.provider.as_str().to_string(),
    }
}

fn convert_album(a: CatalogAlbum) -> RemoteCatalogAlbum {
    RemoteCatalogAlbum {
        id: a.id,
        title: a.title,
        artist: a.artist,
        artist_id: a.artist_id,
        release_date: a.release_date.or(a.first_release_date),
        cover_url: a.cover_url,
        mbid: a.mbid,
        release_types: a.release_types.unwrap_or_default(),
        track_count: a.track_count,
        provider: a.provider.as_str().to_string(),
    }
}

fn convert_artist(a: CatalogArtist) -> RemoteCatalogArtist {
    RemoteCatalogArtist {
        id: a.id,
        name: a.name,
        disambiguation: a.disambiguation,
        picture_url: a.picture_url,
        fan_count: a.fan_count,
        mbid: a.mbid,
        provider: a.provider.as_str().to_string(),
    }
}

fn parse_release_year(release_date: Option<&str>) -> Option<u32> {
    release_date?.get(..4)?.parse::<u32>().ok()
}

/// Converts arbitrary user input into a safe FTS5 prefix-matching query string.
/// Strips Unicode combining diacritical marks first (so NFD `"Noe\u{0308}l"` stays one token
/// instead of splitting at the combining mark), strips punctuation and FTS operators, and caps
/// the token count to prevent FTS5 query DoS.
pub fn build_fts5_query(raw: &str) -> Option<String> {
    let normalized = shared::recommendation::without_marks(raw);
    let tokens: Vec<String> = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .take(MAX_SEARCH_TOKENS)
        .map(|tok| format!("\"{}\"*", tok.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

fn deduplicate_tracks_against_owned(
    owned_rows: &[(String, String)],
    remote: Vec<RemoteCatalogTrack>,
) -> Vec<RemoteCatalogTrack> {
    let mut owned_exact: HashSet<(String, String)> = HashSet::with_capacity(owned_rows.len());
    let mut owned_studio_base: HashSet<(String, String)> =
        HashSet::with_capacity(owned_rows.len());

    for (artist, title) in owned_rows {
        owned_exact.insert(track_dedup_key(artist, title));
        if classify_track_variant(artist, title, None, None, false) == TrackVariantKind::Studio {
            owned_studio_base.insert((canonical_artist_key(artist), base_song_title_key(title)));
        }
    }

    remote
        .into_iter()
        .filter(|r| {
            if owned_exact.contains(&track_dedup_key(&r.artist, &r.title)) {
                return false;
            }
            let is_remote_studio = r.variant_label.is_none()
                && classify_track_variant(
                    &r.artist,
                    &r.title,
                    (!r.album.is_empty()).then_some(r.album.as_str()),
                    None,
                    false,
                ) == TrackVariantKind::Studio;
            if is_remote_studio
                && owned_studio_base
                    .contains(&(canonical_artist_key(&r.artist), base_song_title_key(&r.title)))
            {
                return false;
            }
            true
        })
        .collect()
}

fn deduplicate_albums_against_owned(
    owned_rows: &[(String, String)],
    remote: Vec<RemoteCatalogAlbum>,
) -> Vec<RemoteCatalogAlbum> {
    let owned_keys: HashSet<(String, String)> = owned_rows
        .iter()
        .map(|(artist, album)| album_dedup_key(artist, album))
        .collect();

    remote
        .into_iter()
        .filter(|r| !owned_keys.contains(&album_dedup_key(&r.artist, &r.title)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_remote_track(
        artist: &str,
        title: &str,
        variant_label: Option<&str>,
    ) -> RemoteCatalogTrack {
        RemoteCatalogTrack {
            id: "r1".to_string(),
            mbid: String::new(),
            release_mbid: None,
            title: title.to_string(),
            artist: artist.to_string(),
            artist_id: None,
            album: "Album".to_string(),
            album_id: None,
            duration_secs: 240,
            year: Some(1976),
            cover_url: None,
            variant_label: variant_label.map(str::to_string),
            provider: "deezer".to_string(),
        }
    }

    #[test]
    fn dedup_matches_diacritics_parentheses_and_smart_quotes() {
        let owned = vec![
            (
                "Blue Oyster Cult".to_string(),
                "Don't Fear the Reaper".to_string(),
            ),
            ("4 Non Blondes".to_string(), "What’s Up".to_string()),
        ];
        let remote = vec![
            sample_remote_track("Blue Öyster Cult", "(Don't Fear) The Reaper", None),
            sample_remote_track("4 Non Blondes", "What's Up?", None),
            sample_remote_track(
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live)",
                Some("Live"),
            ),
            sample_remote_track("Blue Öyster Cult", "Burnin' for You", None),
        ];

        let kept = deduplicate_tracks_against_owned(&owned, remote);
        let titles: Vec<&str> = kept.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["(Don't Fear) The Reaper (Live)", "Burnin' for You"]
        );
    }

    #[test]
    fn dedup_hides_owned_albums_across_editions_and_accents() {
        let owned = vec![(
            "Blue Oyster Cult".to_string(),
            "Agents of Fortune".to_string(),
        )];
        let remote = vec![
            RemoteCatalogAlbum {
                id: "a1".to_string(),
                title: "Agents of Fortune (Remastered 2001)".to_string(),
                artist: "Blue Öyster Cult".to_string(),
                artist_id: None,
                release_date: Some("1976-05-21".to_string()),
                cover_url: None,
                mbid: None,
                release_types: vec!["Album".to_string()],
                track_count: Some(10),
                provider: "deezer".to_string(),
            },
            RemoteCatalogAlbum {
                id: "a2".to_string(),
                title: "Fire of Unknown Origin".to_string(),
                artist: "Blue Öyster Cult".to_string(),
                artist_id: None,
                release_date: Some("1981-06-01".to_string()),
                cover_url: None,
                mbid: None,
                release_types: vec!["Album".to_string()],
                track_count: Some(9),
                provider: "deezer".to_string(),
            },
        ];

        let kept = deduplicate_albums_against_owned(&owned, remote);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "a2");
    }

    #[test]
    fn build_fts5_query_handles_nfd_diacritics_and_caps_tokens() {
        // NFD combining diaeresis must NOT split "Noe\u{0308}l" or "O\u{0308}yster".
        assert_eq!(
            build_fts5_query("Noe\u{0308}l O\u{0308}yster"),
            Some("\"Noel\"* \"Oyster\"*".to_string())
        );
        assert_eq!(
            build_fts5_query("Noël Öyster"),
            Some("\"Noel\"* \"Oyster\"*".to_string())
        );

        // Huge pasted input is capped at MAX_SEARCH_TOKENS (16).
        let huge = (0..100)
            .map(|i| format!("tok{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = build_fts5_query(&huge).unwrap();
        assert_eq!(q.split_whitespace().count(), MAX_SEARCH_TOKENS);
    }
}
