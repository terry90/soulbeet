use std::future::Future;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tracing::info;

use crate::polite::RemoteError;
use crate::traits::KnowledgeCache;
use crate::MetadataError;

pub mod metabrainz;

pub const DAY: Duration = Duration::from_secs(86_400);
pub const TTL_SIMILAR_ARTISTS: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_TOP_TRACKS: Duration = Duration::from_secs(14 * 86_400);
pub const TTL_ARTIST_TAGS: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_POPULARITY: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_TAG_SIMILARITY: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_GENRES: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_TAG_TRACKS: Duration = Duration::from_secs(14 * 86_400);
pub const TTL_RECORDINGS: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_MBID: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_DEEZER_ARTIST: Duration = Duration::from_secs(30 * 86_400);
pub const TTL_DEEZER_RADIO: Duration = Duration::from_secs(7 * 86_400);

#[derive(Error, Debug)]
#[error("{0}")]
pub struct CacheError(pub String);

#[derive(Error, Debug)]
pub enum KnowledgeError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("ListenBrainz Labs no longer has the similarity data Soulbeet asks for ({algorithm})")]
    AlgorithmRetired { algorithm: String },
    #[error("ListenBrainz refused the saved token")]
    TokenRefused,
    #[error("ListenBrainz now asks for a token for this; save one in Settings > Library")]
    TokenRequired,
    #[error("Deezer refused the request ({code}): {message}")]
    Deezer { code: u32, message: String },
    #[error(transparent)]
    MusicBrainz(MetadataError),
    #[error(transparent)]
    Cache(#[from] CacheError),
}

impl KnowledgeError {
    /// The source name reports use for this failure.
    pub fn service(&self) -> &'static str {
        match self {
            Self::Remote(e) => e.service(),
            Self::AlgorithmRetired { .. } => "ListenBrainz Labs",
            Self::TokenRefused | Self::TokenRequired => "ListenBrainz",
            Self::Deezer { .. } => "Deezer",
            Self::MusicBrainz(_) => "MusicBrainz",
            Self::Cache(_) => "Soulbeet cache",
        }
    }
}

/// Where the knowledge clients connect. An override takes `host[:port]` and
/// is reached over http, as `MUSICBRAINZ_HOST` is.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeHosts {
    pub listenbrainz: String,
    pub labs: String,
    pub deezer: String,
}

impl KnowledgeHosts {
    pub fn from_env() -> Self {
        Self::hosts_from(|var| std::env::var(var).ok())
    }

    fn hosts_from(read: impl Fn(&str) -> Option<String>) -> Self {
        let host = |var: &str, default: &str| match read(var)
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            Some(host) => format!("http://{host}"),
            None => default.to_string(),
        };
        Self {
            listenbrainz: host("LISTENBRAINZ_HOST", "https://api.listenbrainz.org"),
            labs: host(
                "LISTENBRAINZ_LABS_HOST",
                "https://labs.api.listenbrainz.org",
            ),
            deezer: host("DEEZER_HOST", "https://api.deezer.com"),
        }
    }
}

/// Serve `key` from the cache, or run `fetch` and store its answer for `ttl`.
/// A failed fetch is returned and never stored.
pub(crate) async fn cached<T, F>(
    cache: &dyn KnowledgeCache,
    key: &str,
    ttl: Duration,
    fetch: F,
) -> Result<T, KnowledgeError>
where
    T: Serialize + DeserializeOwned,
    F: Future<Output = Result<T, KnowledgeError>>,
{
    if let Some(stored) = read(cache, key).await? {
        return Ok(stored);
    }
    let value = fetch.await?;
    store(cache, key, &value, ttl).await?;
    Ok(value)
}

/// The value stored under `key`. A value that does not parse reads as a
/// miss: the cached types can change shape between releases, and the fresh
/// answer then replaces the old entry.
pub(crate) async fn read<T: DeserializeOwned>(
    cache: &dyn KnowledgeCache,
    key: &str,
) -> Result<Option<T>, KnowledgeError> {
    let Some(stored) = cache.get(key).await? else {
        return Ok(None);
    };
    match serde_json::from_str(&stored) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            info!("Cache entry {key} no longer reads, fetching it again: {e}");
            Ok(None)
        }
    }
}

pub(crate) async fn store<T: Serialize>(
    cache: &dyn KnowledgeCache,
    key: &str,
    value: &T,
    ttl: Duration,
) -> Result<(), KnowledgeError> {
    let json =
        serde_json::to_string(value).map_err(|e| CacheError(format!("cannot store {key}: {e}")))?;
    cache.put(key, &json, ttl).await?;
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryCache(std::sync::Mutex<Vec<(String, String, Duration)>>);

#[cfg(test)]
impl MemoryCache {
    pub fn ttls(&self) -> Vec<(String, Duration)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _, t)| (k.clone(), *t))
            .collect()
    }

    pub fn put_now(&self, key: &str, value: &str) {
        self.0
            .lock()
            .unwrap()
            .push((key.to_string(), value.to_string(), DAY));
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl KnowledgeCache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v.clone()))
    }

    // Replaces the key's entry, as the SQLite cache does.
    async fn put(&self, key: &str, value: &str, ttl: Duration) -> Result<(), CacheError> {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|(k, _, _)| k != key);
        entries.push((key.to_string(), value.to_string(), ttl));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_cached_answer_is_served_without_fetching() {
        let cache = MemoryCache::default();
        cache.put_now("kc1:t", "[1,2]");
        let got: Vec<u8> = cached(&cache, "kc1:t", TTL_TAG_TRACKS, async { panic!("fetched") })
            .await
            .unwrap();
        assert_eq!(got, vec![1, 2]);
    }

    #[tokio::test]
    async fn an_entry_of_an_older_shape_is_fetched_again_and_replaced() {
        let cache = MemoryCache::default();
        cache.put_now("kc1:t", r#"{"tracks":[1]}"#);
        let got: Vec<u8> = cached(&cache, "kc1:t", TTL_TAG_TRACKS, async { Ok(vec![3]) })
            .await
            .unwrap();
        assert_eq!(got, vec![3]);
        assert_eq!(cache.get("kc1:t").await.unwrap().as_deref(), Some("[3]"));
    }

    #[tokio::test]
    async fn an_answer_is_stored_for_its_ttl_and_a_failure_is_not() {
        let cache = MemoryCache::default();
        let _: Vec<u8> = cached(&cache, "kc1:a", TTL_SIMILAR_ARTISTS, async { Ok(vec![1]) })
            .await
            .unwrap();
        let failed: Result<Vec<u8>, _> = cached(&cache, "kc1:b", TTL_SIMILAR_ARTISTS, async {
            Err(KnowledgeError::TokenRequired)
        })
        .await;
        assert!(failed.is_err());
        assert_eq!(
            cache.ttls(),
            vec![("kc1:a".to_string(), Duration::from_secs(30 * 86_400))]
        );
    }

    #[test]
    fn every_knowledge_error_names_its_source() {
        assert_eq!(
            KnowledgeError::AlgorithmRetired {
                algorithm: "x".into()
            }
            .service(),
            "ListenBrainz Labs"
        );
        assert_eq!(
            KnowledgeError::Remote(RemoteError::RateLimited { service: "Deezer" }).service(),
            "Deezer"
        );
        assert_eq!(
            KnowledgeError::Cache(CacheError("locked".into())).service(),
            "Soulbeet cache"
        );
    }

    #[test]
    fn hosts_are_https_by_default_and_http_when_overridden() {
        let h = KnowledgeHosts::hosts_from(|k| {
            (k == "LISTENBRAINZ_HOST").then(|| "stubs:5060".to_string())
        });
        assert_eq!(
            (h.listenbrainz.as_str(), h.labs.as_str(), h.deezer.as_str()),
            (
                "http://stubs:5060",
                "https://labs.api.listenbrainz.org",
                "https://api.deezer.com"
            )
        );
        let empty = KnowledgeHosts::hosts_from(|_| Some(String::new()));
        assert_eq!(empty.listenbrainz, "https://api.listenbrainz.org");
    }
}
