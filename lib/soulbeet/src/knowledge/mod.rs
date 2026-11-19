use std::future::Future;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::polite::RemoteError;
use crate::traits::KnowledgeCache;
use crate::MetadataError;

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
            Self::MusicBrainz(_) => "MusicBrainz",
            Self::Cache(_) => "Soulbeet cache",
        }
    }
}

/// Serve `key` from the cache, or run `fetch` and store its answer for `ttl`.
/// A failed fetch is returned and never stored.
pub async fn cached<T, F>(
    cache: &dyn KnowledgeCache,
    key: &str,
    ttl: Duration,
    fetch: F,
) -> Result<T, KnowledgeError>
where
    T: Serialize + DeserializeOwned,
    F: Future<Output = Result<T, KnowledgeError>>,
{
    if let Some(stored) = cache.get(key).await? {
        return serde_json::from_str(&stored)
            .map_err(|e| CacheError(format!("unreadable cache entry {key}: {e}")).into());
    }
    let value = fetch.await?;
    let json = serde_json::to_string(&value)
        .map_err(|e| CacheError(format!("cannot store {key}: {e}")))?;
    cache.put(key, &json, ttl).await?;
    Ok(value)
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

    async fn put(&self, key: &str, value: &str, ttl: Duration) -> Result<(), CacheError> {
        self.0
            .lock()
            .unwrap()
            .push((key.to_string(), value.to_string(), ttl));
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
}
