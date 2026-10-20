use async_trait::async_trait;
use shared::{
    download::{DownloadProgress, DownloadableItem, QueuedDownload, SearchResult},
    library::DuplicateReport,
    metadata::{Album, AlbumWithTracks, SearchResult as MetadataSearchResult, Track},
    recommendation::{
        ArtistPopularity, CandidateSet, Listen, ProfileConfig, RankedArtist, RankedTrack,
        SignalReport, SimilarArtist, SimilarTrack, TimePeriod, UserMusicProfile, WeightedTag,
    },
    system::ImporterHealth,
};
use std::fmt;
use std::path::Path;
use std::time::Duration;

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
    /// Enqueue `items` into `destination`, a folder under the backend's
    /// download directory shared by every item of the call.
    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
    ) -> Result<Vec<QueuedDownload>>;
    /// Transfers held for the given peers. Scoped rather than global: the
    /// backend's full list grows with its whole history, and every caller
    /// here is waiting on a specific batch (#78). This is the polling path.
    async fn get_downloads_for(&self, sources: &[String]) -> Result<Vec<DownloadProgress>>;

    /// Every transfer the backend holds. Only for the case that genuinely
    /// needs peers outside the batch: adopting a file retried from a source
    /// nobody is tracking.
    async fn get_downloads(&self) -> Result<Vec<DownloadProgress>>;
    async fn cancel_download(&self, username: &str, download_id: &str, remove: bool) -> Result<()>;
    async fn health_check(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Album,
    Singleton,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported {
        untagged: bool,
        warnings: Vec<String>,
    },
    Skipped(SkipReason),
    LookupFailed {
        source: String,
        message: String,
    },
    Failed {
        message: String,
    },
    TimedOut {
        limit: TimeoutKind,
        last_line: Option<String>,
    },
    /// Not attempted, because the importer is known to be broken.
    Refused {
        issue: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyInLibrary,
    /// No candidate was close enough. The count is the number of candidates
    /// beets weighed, when every task in the run saw the same number.
    NoMatch {
        candidates: Option<u32>,
    },
    NothingToImport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    Idle(Duration),
    Total(Duration),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskCount {
    pub imported: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub outcome: ImportOutcome,
    /// The last lines beets printed, for the row's disclosure.
    pub output_tail: String,
    pub tasks: TaskCount,
}

impl ImportResult {
    fn without_run(outcome: ImportOutcome) -> Self {
        ImportResult {
            outcome,
            output_tail: String::new(),
            tasks: TaskCount::default(),
        }
    }

    pub fn refused(issue: String) -> Self {
        Self::without_run(ImportOutcome::Refused { issue })
    }

    pub fn failed(message: String) -> Self {
        Self::without_run(ImportOutcome::Failed { message })
    }

    pub fn nothing_to_import() -> Self {
        Self::without_run(ImportOutcome::Skipped(SkipReason::NothingToImport))
    }
}

/// Whole minutes when the duration is a whole number of minutes.
fn minutes(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs % 60.0 == 0.0 {
        format!("{}", secs as u64 / 60)
    } else {
        format!("{:.1}", secs / 60.0)
    }
}

/// How much of beets' last line a timeout message quotes.
const LAST_LINE_CHARS: usize = 200;

fn clip(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or(s, |(i, _)| &s[..i])
}

impl fmt::Display for ImportOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportOutcome::Imported { untagged, warnings } => {
                f.write_str(if *untagged {
                    "Imported as-is (untagged)"
                } else {
                    "Imported"
                })?;
                warnings.iter().try_for_each(|w| write!(f, "; {w}"))
            }
            ImportOutcome::Skipped(SkipReason::AlreadyInLibrary) => {
                f.write_str("Already in library")
            }
            ImportOutcome::Skipped(SkipReason::NoMatch {
                candidates: Some(1),
            }) => f.write_str("No close match (1 candidate)"),
            ImportOutcome::Skipped(SkipReason::NoMatch {
                candidates: Some(n),
            }) => {
                write!(f, "No close match ({n} candidates)")
            }
            ImportOutcome::Skipped(SkipReason::NoMatch { candidates: None }) => {
                f.write_str("No close match")
            }
            ImportOutcome::Skipped(SkipReason::NothingToImport) => {
                f.write_str("Nothing left to import")
            }
            ImportOutcome::LookupFailed { source, message } => {
                write!(f, "Lookup failed in {source}: {message}")
            }
            ImportOutcome::Failed { message } => f.write_str(message),
            ImportOutcome::TimedOut {
                limit: TimeoutKind::Idle(d),
                last_line: Some(line),
            } => write!(
                f,
                "No output from beets for {} min (last: {})",
                minutes(*d),
                clip(line, LAST_LINE_CHARS)
            ),
            ImportOutcome::TimedOut {
                limit: TimeoutKind::Idle(d),
                last_line: None,
            } => write!(f, "No output from beets for {} min", minutes(*d)),
            ImportOutcome::TimedOut {
                limit: TimeoutKind::Total(d),
                ..
            } => write!(f, "Import exceeded {} min", minutes(*d)),
            ImportOutcome::Refused { issue } => f.write_str(issue),
        }
    }
}

impl fmt::Display for ImportResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.outcome)?;
        if self.tasks.total > 1 {
            write!(
                f,
                " ({} of {} imported)",
                self.tasks.imported, self.tasks.total
            )?;
        }
        Ok(())
    }
}

#[async_trait]
pub trait MusicImporter: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    async fn import(&self, sources: &[&Path], target: &Path, mode: ImportMode) -> ImportResult;

    async fn find_duplicates(
        &self,
        libraries: &[&Path],
    ) -> std::result::Result<DuplicateReport, String>;

    /// The last known health, without waiting. `Checking` before the first check.
    fn health(&self) -> ImporterHealth;

    /// Checks again and returns the new health. Concurrent callers share one check.
    async fn refresh_health(&self) -> ImporterHealth;

    /// Whether what the last check depended on has changed since. True before
    /// the first check, false while one is running.
    async fn health_stale(&self) -> bool;
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

#[async_trait]
pub trait ScrobbleProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    // Listening history
    async fn get_listens(&self, count: u32) -> Result<Vec<Listen>>;
    async fn get_top_artists(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedArtist>>;
    async fn get_top_tracks(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedTrack>>;

    // Metadata
    async fn get_artist_tags(&self, artist: &str) -> Result<Vec<WeightedTag>>;
    async fn get_artist_popularity(&self, artist: &str) -> Result<ArtistPopularity>;
    async fn get_global_popularity_median(&self) -> Result<u64>;

    // Similarity
    async fn get_similar_artists(&self, artist: &str, limit: u32) -> Result<Vec<SimilarArtist>>;
    async fn get_similar_tracks(
        &self,
        artist: &str,
        track: &str,
        limit: u32,
    ) -> Result<Vec<SimilarTrack>>;

    // Tag/genre exploration
    async fn get_tag_top_tracks(&self, tag: &str, limit: u32) -> Result<Vec<RankedTrack>>;
    async fn get_related_tags(&self, tag: &str) -> Result<Vec<String>>;

    // Artist tracks
    async fn get_artist_top_tracks(&self, artist: &str, limit: u32) -> Result<Vec<RankedTrack>>;
}

#[async_trait]
pub trait CandidateGenerator: Send + Sync {
    fn name(&self) -> &str;
    async fn generate_candidates(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> Result<(CandidateSet, Vec<SignalReport>, usize)>;
}
