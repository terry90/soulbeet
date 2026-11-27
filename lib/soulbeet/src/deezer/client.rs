use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;
use shared::metadata::{
    AlbumWithTracks, ArtistCatalog, CatalogAlbum, CatalogAlbumDetail, CatalogArtist,
    CatalogProvider, CatalogTrack, UnifiedCatalogSearch,
};
use shared::recommendation::fold_name;
use shared::search_norm::{classify_track_variant, fold_search_text};

use super::models::{
    AlbumDetailResponse, ArtistDetailResponse, ArtistHit, DeezerAlbumDetail, DeezerAlbumTrack,
    DeezerArtistAlbum, DeezerArtistDetail, DeezerPlaylist, DeezerPlaylistTrack, DeezerSearchAlbum,
    DeezerSearchArtist, DeezerSearchTrack, DeezerTrack, Envelope, PlaylistResponse,
    PlaylistTrackItem, RadioTrack,
};
use crate::catalog_rank::{
    clean_release_date, dedup_and_order_artist_albums, dedup_artist_top_tracks,
    format_duration_secs, map_record_type, pick_url, rank_and_dedup_deezer_albums,
    rank_and_dedup_deezer_artists, rank_and_dedup_deezer_tracks,
};
use crate::knowledge::{
    cached, KnowledgeError, TTL_DEEZER_ARTIST, TTL_DEEZER_DETAIL, TTL_DEEZER_RADIO,
    TTL_DEEZER_SEARCH,
};
use crate::playlist_import::{
    pinned_https_redirect, read_bounded_bytes, MAX_RESPONSE_BYTES,
};
use crate::polite::{HostGate, Pace, PoliteClient, RemoteError, USER_AGENT};
use crate::traits::KnowledgeCache;

/// Deezer's "no data" error code, an empty answer and not a failure.
const NO_DATA: u32 = 800;

/// Upper bound on search/lookup query lengths sent to Deezer.
const MAX_QUERY_CHARS: usize = 256;

/// Upper bound on unhashed folded cache key fragments before hashing with SHA-256.
const MAX_CACHE_FRAGMENT_CHARS: usize = 160;

fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn bound_query(query: &str) -> &str {
    truncate_chars(query.trim(), MAX_QUERY_CHARS).trim()
}

fn bound_cache_fragment(raw: &str) -> String {
    if raw.chars().count() <= MAX_CACHE_FRAGMENT_CHARS {
        return raw.to_string();
    }
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let prefix = truncate_chars(raw, 96);
    let digest = Sha256::digest(raw.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut hex, "{b:02x}");
    }
    format!("{prefix}:sha256:{hex}")
}

fn search_cache_fragment(query: &str) -> String {
    let folded = fold_search_text(query);
    let raw = if folded.is_empty() {
        query.trim().to_lowercase()
    } else {
        folded
    };
    bound_cache_fragment(&raw)
}

fn encode_query(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

/// Deezer's keyless public API, reduced to the two calls discovery needs.
pub struct DeezerClient {
    http: PoliteClient,
    base_url: String,
    pace: Pace,
    cache: Arc<dyn KnowledgeCache>,
}

impl DeezerClient {
    pub fn new(
        base_url: &str,
        pace: Pace,
        cache: Arc<dyn KnowledgeCache>,
    ) -> Result<Self, RemoteError> {
        let http = PoliteClient::new(base_url, "Deezer", pace)?.with_quota_check(is_quota_error);
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            pace,
            cache,
        })
    }

    pub fn without_redirects(
        base_url: &str,
        pace: Pace,
        cache: Arc<dyn KnowledgeCache>,
    ) -> Result<Self, RemoteError> {
        let http = PoliteClient::without_redirects(base_url, "Deezer", pace)?
            .with_quota_check(is_quota_error);
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            pace,
            cache,
        })
    }

    /// The artist whose name equals `name` once folded; a near match is not one.
    pub async fn artist_id(&self, name: &str) -> Result<Option<u64>, KnowledgeError> {
        let bounded = bound_query(name);
        if bounded.is_empty() {
            return Ok(None);
        }
        let wanted = fold_name(bounded);
        let key = format!("kc1:deezer:artist:{}", bound_cache_fragment(&wanted));
        cached(self.cache.as_ref(), &key, TTL_DEEZER_ARTIST, async {
            let q: String = url::form_urlencoded::byte_serialize(bounded.as_bytes()).collect();
            let hits: Vec<ArtistHit> = self.list(&format!("/search/artist?q={q}&limit=10")).await?;
            Ok(hits
                .into_iter()
                .find(|h| fold_name(&h.name) == wanted)
                .map(|h| h.id))
        })
        .await
    }

    pub async fn artist_radio(&self, id: u64) -> Result<Vec<DeezerTrack>, KnowledgeError> {
        let key = format!("kc1:deezer:radio:{id}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_RADIO, async {
            let tracks: Vec<RadioTrack> =
                self.list(&format!("/artist/{id}/radio?limit=100")).await?;
            Ok(tracks
                .into_iter()
                .map(|t| DeezerTrack {
                    artist: t.artist.name,
                    title: t.title,
                })
                .collect())
        })
        .await
    }

    pub async fn search_tracks(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DeezerSearchTrack>, KnowledgeError> {
        let trimmed = bound_query(query);
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let key = format!(
            "kc1:deezer:search:track:{}:{limit}",
            search_cache_fragment(trimmed)
        );
        cached(self.cache.as_ref(), &key, TTL_DEEZER_SEARCH, async {
            let q = encode_query(trimmed);
            self.list(&format!("/search/track?q={q}&limit={limit}"))
                .await
        })
        .await
    }

    pub async fn search_albums(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DeezerSearchAlbum>, KnowledgeError> {
        let trimmed = bound_query(query);
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let key = format!(
            "kc1:deezer:search:album:{}:{limit}",
            search_cache_fragment(trimmed)
        );
        cached(self.cache.as_ref(), &key, TTL_DEEZER_SEARCH, async {
            let q = encode_query(trimmed);
            self.list(&format!("/search/album?q={q}&limit={limit}"))
                .await
        })
        .await
    }

    pub async fn search_artists(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DeezerSearchArtist>, KnowledgeError> {
        let trimmed = bound_query(query);
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let key = format!(
            "kc1:deezer:search:artist:{}:{limit}",
            search_cache_fragment(trimmed)
        );
        cached(self.cache.as_ref(), &key, TTL_DEEZER_SEARCH, async {
            let q = encode_query(trimmed);
            self.list(&format!("/search/artist?q={q}&limit={limit}"))
                .await
        })
        .await
    }

    pub async fn album_detail(&self, id: u64) -> Result<Option<DeezerAlbumDetail>, KnowledgeError> {
        let key = format!("kc1:deezer:album:{id}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_DETAIL, async {
            let Some(resp) = self
                .http
                .get::<AlbumDetailResponse>(&format!("/album/{id}"))
                .await?
            else {
                return Ok(None);
            };
            if let Some(e) = resp.error {
                if e.code == NO_DATA {
                    return Ok(None);
                }
                return Err(KnowledgeError::Deezer {
                    code: e.code,
                    message: e.message,
                });
            }
            let Some(album_id) = resp.id else {
                return Err(RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "album missing id".into(),
                }
                .into());
            };
            let Some(title) = resp.title else {
                return Err(RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "album missing title".into(),
                }
                .into());
            };
            let Some(artist) = resp.artist else {
                return Err(RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "album missing artist".into(),
                }
                .into());
            };
            let tracks = resp
                .tracks
                .map(|t| {
                    t.data
                        .into_iter()
                        .map(|item| DeezerAlbumTrack {
                            id: item.id,
                            title: item.title,
                            duration: item.duration,
                            artist: item.artist.unwrap_or_else(|| artist.clone()),
                        })
                        .collect()
                })
                .unwrap_or_default();

            Ok(Some(DeezerAlbumDetail {
                id: album_id,
                title,
                cover_medium: resp.cover_medium,
                cover_big: resp.cover_big,
                release_date: clean_release_date(resp.release_date),
                record_type: resp.record_type,
                nb_tracks: resp.nb_tracks,
                artist,
                tracks,
            }))
        })
        .await
    }

    pub async fn artist_detail(
        &self,
        id: u64,
    ) -> Result<Option<DeezerArtistDetail>, KnowledgeError> {
        let key = format!("kc1:deezer:artist_detail:{id}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_DETAIL, async {
            let Some(resp) = self
                .http
                .get::<ArtistDetailResponse>(&format!("/artist/{id}"))
                .await?
            else {
                return Ok(None);
            };
            if let Some(e) = resp.error {
                if e.code == NO_DATA {
                    return Ok(None);
                }
                return Err(KnowledgeError::Deezer {
                    code: e.code,
                    message: e.message,
                });
            }
            let Some(artist_id) = resp.id else {
                return Err(RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "artist missing id".into(),
                }
                .into());
            };
            let Some(name) = resp.name else {
                return Err(RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "artist missing name".into(),
                }
                .into());
            };

            Ok(Some(DeezerArtistDetail {
                id: artist_id,
                name,
                picture_medium: resp.picture_medium,
                picture_big: resp.picture_big,
                nb_fan: resp.nb_fan,
                nb_album: resp.nb_album,
            }))
        })
        .await
    }

    pub async fn artist_albums(
        &self,
        id: u64,
        limit: usize,
    ) -> Result<Vec<DeezerArtistAlbum>, KnowledgeError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = format!("kc1:deezer:artist_albums:{id}:{limit}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_DETAIL, async {
            self.list(&format!("/artist/{id}/albums?limit={limit}"))
                .await
        })
        .await
    }

    pub async fn artist_top_tracks(
        &self,
        id: u64,
        limit: usize,
    ) -> Result<Vec<DeezerSearchTrack>, KnowledgeError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = format!("kc1:deezer:artist_top:{id}:{limit}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_DETAIL, async {
            self.list(&format!("/artist/{id}/top?limit={limit}")).await
        })
        .await
    }

    /// Unified catalog search across tracks, albums, and artists, ranked and deduplicated
    /// through the canonical catalog ranking engine.
    pub async fn search_unified(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<UnifiedCatalogSearch, KnowledgeError> {
        let trimmed = bound_query(query);
        if trimmed.is_empty() || limit == 0 {
            return Ok(UnifiedCatalogSearch {
                query: trimmed.to_string(),
                provider: CatalogProvider::Deezer,
                fallback_used: false,
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
            });
        }

        let (raw_tracks, raw_albums, raw_artists) = tokio::try_join!(
            self.search_tracks(trimmed, 50),
            self.search_albums(trimmed, 25),
            self.search_artists(trimmed, 15),
        )?;

        Ok(UnifiedCatalogSearch {
            query: trimmed.to_string(),
            provider: CatalogProvider::Deezer,
            fallback_used: false,
            tracks: rank_and_dedup_deezer_tracks(trimmed, raw_tracks, limit),
            albums: rank_and_dedup_deezer_albums(trimmed, raw_albums, limit),
            artists: rank_and_dedup_deezer_artists(trimmed, raw_artists, limit),
        })
    }

    pub async fn get_catalog_album_detail(
        &self,
        id: u64,
    ) -> Result<Option<CatalogAlbumDetail>, KnowledgeError> {
        let Some(detail) = self.album_detail(id).await? else {
            return Ok(None);
        };

        let album_id = format!("deezer:album:{}", detail.id);
        let artist_id = format!("deezer:artist:{}", detail.artist.id);
        let release_date = clean_release_date(detail.release_date);
        let album_cover = pick_url(detail.cover_big.clone(), detail.cover_medium.clone());
        let track_cover = pick_url(detail.cover_medium, detail.cover_big);
        let track_count = detail
            .nb_tracks
            .or_else(|| (!detail.tracks.is_empty()).then_some(detail.tracks.len() as u32));

        let album = CatalogAlbum {
            id: album_id.clone(),
            title: detail.title.clone(),
            artist: detail.artist.name,
            artist_id: Some(artist_id),
            release_date: release_date.clone(),
            first_release_date: release_date.clone(),
            cover_url: album_cover,
            mbid: None,
            release_types: map_record_type(detail.record_type.as_deref()),
            track_count,
            provider: CatalogProvider::Deezer,
        };

        let tracks = detail
            .tracks
            .into_iter()
            .map(|t| {
                let variant = classify_track_variant(
                    &t.artist.name,
                    &t.title,
                    Some(&detail.title),
                    None,
                    false,
                );
                CatalogTrack {
                    id: format!("deezer:track:{}", t.id),
                    title: t.title,
                    artist: t.artist.name,
                    artist_id: Some(format!("deezer:artist:{}", t.artist.id)),
                    album_id: Some(album_id.clone()),
                    album_title: Some(detail.title.clone()),
                    release_date: release_date.clone(),
                    duration: format_duration_secs(t.duration),
                    duration_secs: (t.duration > 0).then_some(t.duration),
                    mbid: None,
                    release_mbid: None,
                    cover_url: track_cover.clone(),
                    variant_label: variant.label().map(str::to_string),
                    provider: CatalogProvider::Deezer,
                }
            })
            .collect();

        Ok(Some(CatalogAlbumDetail { album, tracks }))
    }

    pub async fn get_album_with_tracks(
        &self,
        id: u64,
    ) -> Result<Option<AlbumWithTracks>, KnowledgeError> {
        let Some(detail) = self.get_catalog_album_detail(id).await? else {
            return Ok(None);
        };
        Ok(Some(AlbumWithTracks {
            album: detail.album.to_album(),
            tracks: detail.tracks.iter().map(CatalogTrack::to_track).collect(),
        }))
    }

    pub async fn get_artist_catalog(
        &self,
        id: u64,
    ) -> Result<Option<ArtistCatalog>, KnowledgeError> {
        const ALBUMS_LIMIT: usize = 100;
        const TOP_TRACKS_LIMIT: usize = 25;

        let Some(detail) = self.artist_detail(id).await? else {
            return Ok(None);
        };
        let (raw_top, raw_albums) = tokio::try_join!(
            self.artist_top_tracks(id, TOP_TRACKS_LIMIT),
            self.artist_albums(id, ALBUMS_LIMIT),
        )?;

        let complete = raw_albums.len() < ALBUMS_LIMIT;
        let top_tracks = dedup_artist_top_tracks(raw_top, TOP_TRACKS_LIMIT);
        let albums = dedup_and_order_artist_albums(detail.id, &detail.name, raw_albums);

        let artist = CatalogArtist {
            id: format!("deezer:artist:{}", detail.id),
            name: detail.name,
            disambiguation: None,
            picture_url: pick_url(detail.picture_big, detail.picture_medium),
            fan_count: detail.nb_fan,
            mbid: None,
            provider: CatalogProvider::Deezer,
        };

        Ok(Some(ArtistCatalog {
            artist,
            provider: CatalogProvider::Deezer,
            others: Vec::new(),
            top_tracks,
            albums,
            albums_complete: complete,
            singles_complete: complete,
        }))
    }

    /// Reads a public Deezer playlist and paginates `/playlist/{id}/tracks` up to `max_tracks`.
    pub async fn playlist(
        &self,
        id: &str,
        max_tracks: usize,
    ) -> Result<Option<DeezerPlaylist>, KnowledgeError> {
        self.playlist_bounded(id, max_tracks, false).await
    }

    /// Reads a public Deezer playlist with strict response size bounds and redirect pinning
    /// (restricted to `https://api.deezer.com` on the default port when `enforce_deezer_host` is
    /// true, or blocking all redirects otherwise).
    pub(crate) async fn playlist_bounded(
        &self,
        id: &str,
        max_tracks: usize,
        enforce_deezer_host: bool,
    ) -> Result<Option<DeezerPlaylist>, KnowledgeError> {
        const PAGE_SIZE: usize = 100;
        const MAX_EXTRA_PAGES: usize = 10;

        let redirect = if enforce_deezer_host {
            pinned_https_redirect("api.deezer.com")
        } else {
            Policy::none()
        };
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(self.pace.timeout)
            .connect_timeout(Duration::from_secs(5))
            .redirect(redirect)
            .build()
            .map_err(|e| RemoteError::Unavailable {
                service: "Deezer",
                detail: e.without_url().to_string(),
            })?;
        let gate = HostGate::of(&self.base_url).map_err(|detail| RemoteError::Unavailable {
            service: "Deezer",
            detail,
        })?;

        let Some(resp) = self
            .get_bounded::<PlaylistResponse>(&http, &gate, &format!("/playlist/{id}"))
            .await?
        else {
            return Ok(None);
        };
        if let Some(e) = resp.error {
            if e.code == NO_DATA {
                return Ok(None);
            }
            return Err(KnowledgeError::Deezer {
                code: e.code,
                message: e.message,
            });
        }
        let Some(playlist_id) = resp.id else {
            return Err(RemoteError::Unreadable {
                service: "Deezer",
                detail: "playlist missing id".into(),
            }
            .into());
        };
        let title = resp.title.unwrap_or_default().trim().to_string();
        let description = resp
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());

        let mut raw_tracks = Vec::new();
        let mut has_more = false;
        if let Some(page) = resp.tracks {
            has_more = page.next.is_some();
            raw_tracks.extend(page.data);
        }

        let mut pages_fetched = 0usize;
        while raw_tracks.len() < max_tracks
            && (has_more || raw_tracks.len() < resp.nb_tracks as usize)
            && pages_fetched < MAX_EXTRA_PAGES
        {
            let index = raw_tracks.len();
            let path = format!("/playlist/{id}/tracks?index={index}&limit={PAGE_SIZE}");
            let Some(envelope) = self
                .get_bounded::<Envelope<PlaylistTrackItem>>(&http, &gate, &path)
                .await?
            else {
                break;
            };
            if let Some(e) = envelope.error {
                if e.code == NO_DATA {
                    break;
                }
                return Err(KnowledgeError::Deezer {
                    code: e.code,
                    message: e.message,
                });
            }
            let page_items = envelope.data.unwrap_or_default();
            if page_items.is_empty() {
                break;
            }
            has_more = envelope.next.is_some();
            raw_tracks.extend(page_items);
            pages_fetched += 1;
        }

        let raw_total_hint = (raw_tracks.len() + usize::from(has_more)) as u32;
        let nb_tracks = resp.nb_tracks.max(raw_total_hint);

        let tracks = raw_tracks
            .into_iter()
            .filter_map(|t| {
                let title = t.title.trim().to_string();
                let artist = t.artist.map(|a| a.name.trim().to_string()).unwrap_or_default();
                if title.is_empty() || artist.is_empty() {
                    return None;
                }
                let album = t
                    .album
                    .map(|a| a.title.trim().to_string())
                    .filter(|a| !a.is_empty());
                let duration_s = t.duration.filter(|&d| d > 0);
                Some(DeezerPlaylistTrack {
                    artist,
                    title,
                    album,
                    duration_s,
                })
            })
            .take(max_tracks)
            .collect();

        Ok(Some(DeezerPlaylist {
            id: playlist_id,
            title,
            description,
            nb_tracks,
            tracks,
        }))
    }

    async fn get_bounded<T: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        gate: &HostGate,
        path: &str,
    ) -> Result<Option<T>, KnowledgeError> {
        let url = format!("{}{path}", self.base_url);
        let mut last = RemoteError::Unavailable {
            service: "Deezer",
            detail: String::new(),
        };
        for attempt in 0..=self.pace.retries {
            let wait = self.pace.backoff * 2u32.pow(attempt);
            gate.wait_for_turn(self.pace.min_interval).await;
            let resp = match http.get(&url).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last = RemoteError::Unavailable {
                        service: "Deezer",
                        detail: e.without_url().to_string(),
                    };
                    if attempt < self.pace.retries {
                        tokio::time::sleep(wait).await;
                    }
                    continue;
                }
            };
            let status = resp.status();
            let bytes = match read_bounded_bytes(resp, MAX_RESPONSE_BYTES).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    return Err(RemoteError::Unreadable {
                        service: "Deezer",
                        detail: e.to_string(),
                    }
                    .into());
                }
            };
            let quota = status.is_success() && is_quota_error(&bytes);
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || quota {
                gate.hold(wait);
                last = RemoteError::RateLimited { service: "Deezer" };
                if attempt < self.pace.retries {
                    tokio::time::sleep(wait).await;
                }
                continue;
            }
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                gate.hold(wait);
            }
            if status.is_server_error() {
                last = RemoteError::Unavailable {
                    service: "Deezer",
                    detail: format!("HTTP {}", status.as_u16()),
                };
                if attempt < self.pace.retries {
                    tokio::time::sleep(wait).await;
                }
                continue;
            }
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(RemoteError::Status {
                    service: "Deezer",
                    status: status.as_u16(),
                    body: String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(200)
                        .collect(),
                }
                .into());
            }
            return serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                RemoteError::Unreadable {
                    service: "Deezer",
                    detail: e.to_string(),
                }
                .into()
            });
        }
        Err(last.into())
    }

    async fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, KnowledgeError> {
        let Some(envelope) = self.http.get::<Envelope<T>>(path).await? else {
            return Ok(Vec::new());
        };
        match envelope.error {
            Some(e) if e.code == NO_DATA => Ok(Vec::new()),
            Some(e) => Err(KnowledgeError::Deezer {
                code: e.code,
                message: e.message,
            }),
            // An answer with neither data nor error must not be cached as "nothing there".
            None => envelope.data.ok_or_else(|| {
                RemoteError::Unreadable {
                    service: "Deezer",
                    detail: "no data and no error".into(),
                }
                .into()
            }),
        }
    }
}

/// Code 4 is Deezer's quota answer, sent with HTTP 200.
pub(crate) fn is_quota_error(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["code"].as_u64())
        == Some(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{MemoryCache, DAY};
    use crate::test_http::{serve, Reply};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    const NUNO: &str = include_str!("fixtures/radio_nuno_canavarro.json");

    fn client(base: &str) -> DeezerClient {
        DeezerClient::new(base, Pace::immediate(), Arc::new(MemoryCache::default())).unwrap()
    }

    #[tokio::test]
    async fn an_artist_is_found_only_by_its_exact_name() {
        let fixture = include_str!("fixtures/search_artist_fishmans.json");
        let (base, seen) = serve(vec![ok(fixture), ok(fixture)]);
        let dz = client(&base);
        assert_eq!(dz.artist_id("Fishmans").await.unwrap(), Some(13628349));
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/search/artist?q=Fishmans&limit=10"
        );
        assert_eq!(dz.artist_id("Fishman").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_radio_is_read_in_order() {
        let (base, seen) = serve(vec![ok(NUNO)]);
        let tracks = client(&base).artist_radio(4193009).await.unwrap();
        assert_eq!(tracks.len(), 5);
        assert_eq!(
            tracks[0],
            DeezerTrack {
                artist: "Carlos Maria Trindade".into(),
                title: "Segresos M".into()
            }
        );
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/artist/4193009/radio?limit=100"
        );
    }

    #[tokio::test]
    async fn a_radio_without_data_is_empty() {
        let (base, _) = serve(vec![ok(include_str!("fixtures/radio_no_data.json"))]);
        assert_eq!(client(&base).artist_radio(1).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn an_answer_without_data_or_error_is_not_cached() {
        let (base, seen) = serve(vec![ok("{}"), ok(NUNO)]);
        let dz = client(&base);
        assert!(matches!(
            dz.artist_radio(4193009).await,
            Err(KnowledgeError::Remote(RemoteError::Unreadable { .. }))
        ));
        assert_eq!(dz.artist_radio(4193009).await.unwrap().len(), 5);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_quota_answer_is_retried() {
        let (base, seen) = serve(vec![
            ok(include_str!("fixtures/quota_exceeded.json")),
            ok(NUNO),
        ]);
        let tracks = client(&base).artist_radio(4193009).await.unwrap();
        assert_eq!(tracks.len(), 5);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn another_error_code_is_a_failure() {
        let (base, _) = serve(vec![ok(
            r#"{"error":{"type":"OAuthException","message":"Invalid OAuth access token","code":300}}"#,
        )]);
        match client(&base).artist_radio(1).await {
            Err(KnowledgeError::Deezer { code, message }) => {
                assert_eq!(
                    (code, message.as_str()),
                    (300, "Invalid OAuth access token")
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn ids_are_kept_30_days_and_radios_7() {
        let (base, _) = serve(vec![
            ok(include_str!("fixtures/search_artist_fishmans.json")),
            ok(NUNO),
        ]);
        let cache = Arc::new(MemoryCache::default());
        let dz = DeezerClient::new(&base, Pace::immediate(), cache.clone()).unwrap();
        dz.artist_id("Fishmans").await.unwrap();
        dz.artist_radio(4193009).await.unwrap();
        assert_eq!(
            cache.ttls(),
            vec![
                ("kc1:deezer:artist:fishmans".to_string(), 30 * DAY),
                ("kc1:deezer:radio:4193009".to_string(), 7 * DAY),
            ]
        );
    }

    #[tokio::test]
    async fn search_endpoints_deserialize_and_cache_with_search_ttl() {
        let tracks_json = r#"{
            "data": [{
                "id": 101,
                "title": "(Don't Fear) The Reaper",
                "title_short": "(Don't Fear) The Reaper",
                "duration": 308,
                "rank": 415000,
                "artist": {"id": 55, "name": "Blue Öyster Cult", "picture_medium": "https://img/a.jpg"},
                "album": {"id": 99, "title": "Agents Of Fortune", "cover_medium": "https://img/m.jpg", "cover_big": "https://img/b.jpg"}
            }]
        }"#;
        let albums_json = r#"{
            "data": [{
                "id": 99,
                "title": "Agents Of Fortune",
                "cover_medium": "https://img/m.jpg",
                "cover_big": "https://img/b.jpg",
                "nb_tracks": 10,
                "record_type": "album",
                "artist": {"id": 55, "name": "Blue Öyster Cult"}
            }]
        }"#;
        let artists_json = r#"{
            "data": [{
                "id": 55,
                "name": "Blue Öyster Cult",
                "picture_medium": "https://img/pm.jpg",
                "picture_big": "https://img/pb.jpg",
                "nb_fan": 550000,
                "nb_album": 28
            }]
        }"#;

        let (base, seen) = serve(vec![ok(tracks_json), ok(albums_json), ok(artists_json)]);
        let cache = Arc::new(MemoryCache::default());
        let dz = DeezerClient::new(&base, Pace::immediate(), cache.clone()).unwrap();

        let tracks = dz.search_tracks("Blue Oyster Cult", 50).await.unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, 101);
        assert_eq!(tracks[0].duration, 308);
        assert_eq!(tracks[0].rank, 415_000);
        assert_eq!(tracks[0].album.title, "Agents Of Fortune");

        let albums = dz.search_albums("Blue Oyster Cult", 25).await.unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].nb_tracks, Some(10));
        assert_eq!(albums[0].record_type.as_deref(), Some("album"));

        let artists = dz.search_artists("Blue Oyster Cult", 15).await.unwrap();
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].nb_fan, Some(550_000));
        assert_eq!(artists[0].nb_album, Some(28));

        // Second call hits cache without another HTTP request.
        let cached_tracks = dz.search_tracks("blue öyster cult", 50).await.unwrap();
        assert_eq!(cached_tracks, tracks);

        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].target, "/search/track?q=Blue+Oyster+Cult&limit=50");
        assert_eq!(requests[1].target, "/search/album?q=Blue+Oyster+Cult&limit=25");
        assert_eq!(
            requests[2].target,
            "/search/artist?q=Blue+Oyster+Cult&limit=15"
        );
        assert_eq!(
            cache.ttls(),
            vec![
                (
                    "kc1:deezer:search:track:blue oyster cult:50".to_string(),
                    TTL_DEEZER_SEARCH
                ),
                (
                    "kc1:deezer:search:album:blue oyster cult:25".to_string(),
                    TTL_DEEZER_SEARCH
                ),
                (
                    "kc1:deezer:search:artist:blue oyster cult:15".to_string(),
                    TTL_DEEZER_SEARCH
                ),
            ]
        );
    }

    #[tokio::test]
    async fn album_detail_and_no_data_800_and_conversions() {
        let album_json = r#"{
            "id": 99,
            "title": "Agents Of Fortune",
            "cover_medium": "https://img/m.jpg",
            "cover_big": "https://img/b.jpg",
            "release_date": "1976-05-21",
            "record_type": "album",
            "nb_tracks": 2,
            "artist": {"id": 55, "name": "Blue Öyster Cult", "picture_medium": "https://img/a.jpg"},
            "tracks": {
                "data": [
                    {"id": 101, "title": "(Don't Fear) The Reaper", "duration": 308, "artist": {"id": 55, "name": "Blue Öyster Cult"}},
                    {"id": 102, "title": "E.T.I. (Extra Terrestrial Intelligence)", "duration": 223, "artist": {"id": 55, "name": "Blue Öyster Cult"}}
                ]
            }
        }"#;
        let no_data = include_str!("fixtures/radio_no_data.json");

        let (base, seen) = serve(vec![ok(album_json), ok(no_data)]);
        let cache = Arc::new(MemoryCache::default());
        let dz = DeezerClient::new(&base, Pace::immediate(), cache.clone()).unwrap();

        let catalog = dz.get_catalog_album_detail(99).await.unwrap().unwrap();
        assert_eq!(catalog.album.id, "deezer:album:99");
        assert_eq!(catalog.album.artist_id.as_deref(), Some("deezer:artist:55"));
        assert_eq!(catalog.album.release_date.as_deref(), Some("1976-05-21"));
        assert_eq!(catalog.album.release_types, Some(vec!["Album".into()]));
        assert_eq!(catalog.album.track_count, Some(2));
        assert_eq!(catalog.tracks.len(), 2);
        assert_eq!(catalog.tracks[0].id, "deezer:track:101");
        assert_eq!(catalog.tracks[0].album_id.as_deref(), Some("deezer:album:99"));
        assert_eq!(catalog.tracks[0].duration.as_deref(), Some("05:08"));
        assert_eq!(catalog.tracks[0].duration_secs, Some(308));
        assert_eq!(catalog.tracks[0].mbid, None);

        // Uses cached album detail for AlbumWithTracks conversion.
        let legacy = dz.get_album_with_tracks(99).await.unwrap().unwrap();
        assert_eq!(legacy.album.id, "deezer:album:99");
        assert_eq!(legacy.tracks.len(), 2);
        assert_eq!(legacy.tracks[0].duration.as_deref(), Some("05:08"));

        // Error 800 (NO_DATA) returns Ok(None).
        assert_eq!(dz.album_detail(40404).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(seen.lock().unwrap()[0].target, "/album/99");
        assert_eq!(seen.lock().unwrap()[1].target, "/album/40404");
        assert_eq!(
            cache.ttls(),
            vec![
                ("kc1:deezer:album:99".to_string(), TTL_DEEZER_DETAIL),
                ("kc1:deezer:album:40404".to_string(), TTL_DEEZER_DETAIL),
            ]
        );
    }

    #[tokio::test]
    async fn artist_catalog_and_unified_search_use_mock_http_and_rank_results() {
        use crate::test_http::serve_fn;

        let (base, seen) = serve_fn(|req| match req.target.as_str() {
            "/artist/55" => ok(r#"{
                "id": 55,
                "name": "Blue Öyster Cult",
                "picture_medium": "https://img/pm.jpg",
                "picture_big": "https://img/pb.jpg",
                "nb_fan": 550000,
                "nb_album": 28
            }"#),
            "/artist/55/top?limit=25" => ok(r#"{
                "data": [
                    {
                        "id": 201,
                        "title": "(Don't Fear) The Reaper",
                        "duration": 308,
                        "rank": 737000,
                        "artist": {"id": 55, "name": "Blue Öyster Cult"},
                        "album": {"id": 900, "title": "The Essential Blue Öyster Cult"}
                    },
                    {
                        "id": 202,
                        "title": "(Don't Fear) The Reaper",
                        "duration": 308,
                        "rank": 415000,
                        "artist": {"id": 55, "name": "Blue Öyster Cult"},
                        "album": {"id": 99, "title": "Agents Of Fortune"}
                    }
                ]
            }"#),
            "/artist/55/albums?limit=100" => ok(r#"{
                "data": [
                    {"id": 900, "title": "The Essential Blue Öyster Cult", "release_date": "2003-01-01", "record_type": "compile"},
                    {"id": 99, "title": "Agents Of Fortune", "release_date": "1976-05-21", "record_type": "album"},
                    {"id": 100, "title": "Fire of Unknown Origin", "release_date": "1981-06-01", "record_type": "album"},
                    {"id": 101, "title": "Burnin' for You", "release_date": "1981-07-01", "record_type": "single"}
                ]
            }"#),
            "/artist/999" => ok(include_str!("fixtures/radio_no_data.json")),
            t if t.starts_with("/search/track?") => ok(r#"{
                "data": [
                    {
                        "id": 1,
                        "title": "(Don't Fear) The Reaper",
                        "duration": 308,
                        "rank": 737000,
                        "artist": {"id": 55, "name": "Blue Öyster Cult"},
                        "album": {"id": 900, "title": "The Essential Blue Öyster Cult"}
                    },
                    {
                        "id": 2,
                        "title": "(Don't Fear) The Reaper",
                        "duration": 308,
                        "rank": 415000,
                        "artist": {"id": 55, "name": "Blue Öyster Cult"},
                        "album": {"id": 99, "title": "Agents Of Fortune"}
                    },
                    {
                        "id": 3,
                        "title": "(Don't Fear) The Reaper (In the Style of Blue Oyster Cult)",
                        "duration": 305,
                        "rank": 800000,
                        "artist": {"id": 77, "name": "ProSource Karaoke"},
                        "album": {"id": 770, "title": "Karaoke Hits"}
                    }
                ]
            }"#),
            t if t.starts_with("/search/album?") => ok(r#"{
                "data": [
                    {
                        "id": 900,
                        "title": "The Essential Blue Öyster Cult",
                        "nb_tracks": 32,
                        "record_type": "compile",
                        "artist": {"id": 55, "name": "Blue Öyster Cult"}
                    },
                    {
                        "id": 99,
                        "title": "Agents Of Fortune",
                        "nb_tracks": 10,
                        "record_type": "album",
                        "artist": {"id": 55, "name": "Blue Öyster Cult"}
                    }
                ]
            }"#),
            t if t.starts_with("/search/artist?") => ok(r#"{
                "data": [
                    {"id": 10, "name": "Deep Purple", "nb_fan": 3500000, "nb_album": 45},
                    {"id": 55, "name": "Blue Öyster Cult", "nb_fan": 550000, "nb_album": 28}
                ]
            }"#),
            other => panic!("unexpected target: {other}"),
        });

        let dz = client(&base);

        // Artist catalog
        let cat = dz.get_artist_catalog(55).await.unwrap().unwrap();
        assert_eq!(cat.artist.id, "deezer:artist:55");
        assert_eq!(cat.artist.fan_count, Some(550_000));
        assert!(cat.albums_complete);
        assert!(cat.singles_complete);
        assert_eq!(cat.top_tracks.len(), 1);
        assert_eq!(
            cat.top_tracks[0].album_title.as_deref(),
            Some("Agents Of Fortune")
        );
        // Albums & EPs newest first, then singles, then compilations
        let titles: Vec<&str> = cat.albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Fire of Unknown Origin",
                "Agents Of Fortune",
                "Burnin' for You",
                "The Essential Blue Öyster Cult"
            ]
        );
        assert_eq!(dz.get_artist_catalog(999).await.unwrap(), None);

        // Unified search
        let unified = dz.search_unified("Blue Oyster Cult", 10).await.unwrap();
        assert_eq!(unified.provider, CatalogProvider::Deezer);
        assert!(!unified.fallback_used);
        assert_eq!(unified.tracks.len(), 1);
        assert_eq!(
            unified.tracks[0].album_title.as_deref(),
            Some("Agents Of Fortune")
        );
        assert_eq!(unified.albums[0].title, "Agents Of Fortune");
        assert_eq!(unified.artists[0].name, "Blue Öyster Cult");
        assert_eq!(seen.lock().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn search_queries_and_cache_keys_are_bounded_at_utf8_char_boundaries() {
        let (base, seen) = serve(vec![ok(r#"{"data": []}"#)]);
        let cache = Arc::new(MemoryCache::default());
        let dz = DeezerClient::new(&base, Pace::immediate(), cache.clone()).unwrap();

        let huge_query = "é".repeat(4_000);
        let tracks = dz.search_tracks(&huge_query, 10).await.unwrap();
        assert!(tracks.is_empty());

        let target = &seen.lock().unwrap()[0].target;
        let expected_q = encode_query(&"é".repeat(MAX_QUERY_CHARS));
        assert_eq!(target, &format!("/search/track?q={expected_q}&limit=10"));

        let ttls = cache.ttls();
        assert_eq!(ttls.len(), 1);
        assert!(
            ttls[0].0.contains(":sha256:"),
            "long cache key should be hashed: {}",
            ttls[0].0
        );
        assert!(ttls[0].0.len() <= 220, "cache key too long: {}", ttls[0].0.len());
    }
}
