use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;
use shared::recommendation::fold_name;

use super::models::{
    ArtistHit, DeezerPlaylist, DeezerPlaylistTrack, DeezerTrack, Envelope, PlaylistResponse,
    PlaylistTrackItem, RadioTrack,
};
use crate::knowledge::{cached, KnowledgeError, TTL_DEEZER_ARTIST, TTL_DEEZER_RADIO};
use crate::playlist_import::{
    pinned_https_redirect, read_bounded_bytes, MAX_RESPONSE_BYTES,
};
use crate::polite::{HostGate, Pace, PoliteClient, RemoteError, USER_AGENT};
use crate::traits::KnowledgeCache;

/// Deezer's "no data" error code, an empty answer and not a failure.
const NO_DATA: u32 = 800;

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
        let wanted = fold_name(name);
        let key = format!("kc1:deezer:artist:{wanted}");
        cached(self.cache.as_ref(), &key, TTL_DEEZER_ARTIST, async {
            let q: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
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
}
