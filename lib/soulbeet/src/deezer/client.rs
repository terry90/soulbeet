use std::sync::Arc;

use serde::de::DeserializeOwned;
use shared::recommendation::fold_name;

use super::models::{ArtistHit, DeezerTrack, Envelope, RadioTrack};
use crate::knowledge::{cached, KnowledgeError, TTL_DEEZER_ARTIST, TTL_DEEZER_RADIO};
use crate::polite::{Pace, PoliteClient, RemoteError};
use crate::traits::KnowledgeCache;

/// Deezer's "no data" error code, an empty answer and not a failure.
const NO_DATA: u32 = 800;

/// Deezer's keyless public API, reduced to the two calls discovery needs.
pub struct DeezerClient {
    http: PoliteClient,
    cache: Arc<dyn KnowledgeCache>,
}

impl DeezerClient {
    pub fn new(
        base_url: &str,
        pace: Pace,
        cache: Arc<dyn KnowledgeCache>,
    ) -> Result<Self, RemoteError> {
        let http = PoliteClient::new(base_url, "Deezer", pace)?.with_quota_check(is_quota_error);
        Ok(Self { http, cache })
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
