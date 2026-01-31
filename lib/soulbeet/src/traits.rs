use async_trait::async_trait;
use shared::{
    download::{DownloadProgress, DownloadableItem, QueuedDownload, SearchResult},
    library::DuplicateReport,
    musicbrainz::{Album, AlbumWithTracks, SearchResult as MetadataSearchResult, Track},
};
use std::path::Path;

use crate::error::Result;

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MetadataSearchResult>>;

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MetadataSearchResult>>;

    async fn get_album(&self, id: &str) -> Result<AlbumWithTracks>;
}

#[async_trait]
pub trait DownloadBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    async fn start_search(&self, album: Option<&Album>, tracks: &[Track]) -> Result<String>;
    async fn poll_search(&self, search_id: &str) -> Result<SearchResult>;
    async fn download(&self, items: Vec<DownloadableItem>) -> Result<Vec<QueuedDownload>>;
    async fn get_downloads(&self) -> Result<Vec<DownloadProgress>>;
    async fn health_check(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportResult {
    Success,
    Skipped,
    Failed(String),
    TimedOut,
}

#[async_trait]
pub trait MusicImporter: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    async fn import(
        &self,
        sources: &[&Path],
        target: &Path,
        as_album: bool,
    ) -> Result<ImportResult>;

    async fn find_duplicates(&self, libraries: &[&Path]) -> Result<DuplicateReport>;
    async fn health_check(&self) -> bool;
}

pub struct FallbackMetadataProvider {
    providers: Vec<Box<dyn MetadataProvider>>,
}

impl FallbackMetadataProvider {
    pub fn new(providers: Vec<Box<dyn MetadataProvider>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl MetadataProvider for FallbackMetadataProvider {
    fn id(&self) -> &'static str {
        "fallback"
    }

    fn name(&self) -> &'static str {
        "Fallback"
    }

    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MetadataSearchResult>> {
        for provider in &self.providers {
            match provider.search_albums(artist, query, limit).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("{} failed: {}", provider.name(), e);
                    continue;
                }
            }
        }
        Ok(vec![])
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MetadataSearchResult>> {
        for provider in &self.providers {
            match provider.search_tracks(artist, query, limit).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("{} failed: {}", provider.name(), e);
                    continue;
                }
            }
        }
        Ok(vec![])
    }

    async fn get_album(&self, id: &str) -> Result<AlbumWithTracks> {
        for provider in &self.providers {
            match provider.get_album(id).await {
                Ok(album) => return Ok(album),
                Err(e) => {
                    tracing::warn!("{} failed: {}", provider.name(), e);
                    continue;
                }
            }
        }
        Err(crate::error::SoulseekError::Api {
            status: 404,
            message: "Album not found".to_string(),
        })
    }
}
