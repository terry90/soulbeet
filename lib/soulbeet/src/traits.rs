use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shared::{
    download::{
        DownloadProgress, DownloadableItem, MatchChoices, QueuedDownload, SearchResult, SearchSpec,
        SkipKind, SourcePresence,
    },
    failure::{Failure, UserAction},
    metadata::{AlbumWithTracks, SearchResult as MetadataSearchResult},
    recommendation::{
        CandidateSet, KnownMusic, Listen, Neighbour, PlayedArtist, PlayedTrack, ProfileConfig,
        RecordingFacts, SignalReport, SourceErrors, TimePeriod, TrackRef, UserMusicProfile,
        WeightedTag,
    },
    system::ImporterHealth,
};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend_error::BackendResult;
use crate::history::HistoryError;
use crate::knowledge::{CacheError, KnowledgeError};
use crate::musicbrainz::ArtistMatch;
use crate::MetadataError;

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<MetadataSearchResult>, MetadataError>;

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<MetadataSearchResult>, MetadataError>;

    async fn get_album(&self, id: &str) -> std::result::Result<AlbumWithTracks, MetadataError>;
}

/// A source of files. Files of a `download` call land at
/// `<DOWNLOAD_PATH>/<destination>/<name>`, `name` being one of
/// `shared::download::landing_names(last segment of DownloadableItem::id)` or a `ticks_copy` of
/// it, as `resolve_in_destination` reads.
///
/// What the callers rely on:
/// - `QueuedDownload::id` is stable, and is the id `get_downloads_for` and
///   `get_download` list the transfer under.
/// - Each `QueuedDownload` that `download` returns, and each `DownloadProgress`
///   listed, names the sent item's `source` in `source` and its
///   `DownloadableItem::id` in `item`.
/// - Every `DownloadableItem` that `poll_search` returns names `id()` in
///   `backend`, so its sends come back here.
/// - A succeeded transfer stays listed until `cancel_download` with `remove`.
/// - A backend that keeps transfers in memory loses them on restart; `get_download` then answers
///   None, which reconcile treats as a lost transfer. An ended record may be forgotten 7 days
///   after it ended.
/// - `remove` forgets an ended transfer and never moves or deletes a file in the destination; a
///   backend's own partial files are its own.
/// - An item `download` cannot send gets no answer: the caller fails it.
#[async_trait]
pub trait DownloadBackend: Send + Sync {
    /// The id the registry keeps this backend under, and the only one it is looked up by.
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String>;
    async fn poll_search(&self, search_id: &str) -> BackendResult<SearchResult>;
    /// A fresh id for one `download` call, drawn before the call so the caller can store it
    /// first. Opaque to the caller.
    fn new_request(&self) -> String;
    /// Enqueue `items` into `destination`, a folder under the backend's
    /// download directory shared by every item of the call. `request` is
    /// one `new_request` gave, for this call only: every transfer the call
    /// makes is listed with it as `DownloadProgress::request`.
    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
    ) -> BackendResult<Vec<QueuedDownload>>;
    /// Transfers held for the given peers. Scoped rather than global: the
    /// backend's full list grows with its whole history, and every caller
    /// here is waiting on a specific batch (#78). This is the polling path.
    async fn get_downloads_for(&self, sources: &[String]) -> BackendResult<Vec<DownloadProgress>>;
    /// The transfer `source` holds under `id`, as `get_downloads_for` reads it; None when the backend no longer has it.
    async fn get_download(&self, source: &str, id: &str)
        -> BackendResult<Option<DownloadProgress>>;
    async fn cancel_download(
        &self,
        source: &str,
        download_id: &str,
        remove: bool,
    ) -> BackendResult<()>;
    /// The transfer's place in the line it waits in before it starts: the source's queue for a
    /// peer backend (asked now, or the last place the source gave when it cannot be asked), the
    /// backend's own queue otherwise; Some(0) once it is not waiting; None when the backend no
    /// longer has the transfer.
    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>>;
    /// Whether this backend can fetch from the source now.
    async fn source_status(&self, source: &str) -> BackendResult<SourcePresence>;
    /// Ends a running search early; its next poll reads what it found so far.
    async fn stop_search(&self, search_id: &str) -> BackendResult<()>;
    /// Ok when the backend can take work now; else why not, as a failure
    /// with the backend's own answer as its cause. Called every 10 s per open tab: it costs the
    /// remote service at most one request per minute; cache inside the backend.
    async fn health_check(&self) -> std::result::Result<(), Failure>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Album,
    Singleton,
}

/// How the importer decides on a match. Kept with an import the ledger stores, so a restart
/// runs it as the user chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverChoice {
    /// As a quiet import does: apply a strong match, skip the rest.
    Auto,
    /// No lookup; the files keep their own tags.
    AsIs,
    /// Apply what `data_source` returns for `id`, however close it is.
    Pick { data_source: String, id: String },
}

/// How a run handles the duplicates beets finds in the library: as beets' config says, with a
/// `remove` reported instead of made, skipped whatever the config says, or each reported as a
/// removal whatever the config says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateRun {
    /// beets acts as configured.
    AsConfigured,
    /// beets keeps the earlier copy and reports the removal it wanted.
    ReportRemovals,
    /// beets skips a task whose music the library already has, whatever its config says.
    Skip,
    /// beets keeps every earlier copy and reports each as a wanted removal, whatever its config
    /// says.
    Replace,
}

/// One beets task's wanted removal: the earlier items beets would have deleted, and the items
/// the task imported once their files were in place (empty when they never were).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedRemoval {
    /// As `ImportedItem` names a library item: the path relative to the library folder.
    pub existing: Vec<ImportedItem>,
    /// Each imported item's id and where beets stored it, relative to the library folder when
    /// inside it.
    pub imported: Vec<(u64, PathBuf)>,
}

/// What a relocate did with the items it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocation {
    /// Each item the importer could be read for after the move, with the path its file has now:
    /// relative to the library folder when inside it, else as the importer gives it.
    pub now: Vec<(u64, PathBuf)>,
    /// Why the move is not done: the importer failed to move the items, moved one out of the
    /// library, or could not be read for one (whose id is then not in `now`).
    pub failure: Option<Failure>,
}

/// One library item, as beets stored it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedItem {
    /// Relative to the library folder when inside it, else absolute.
    pub path: PathBuf,
    /// The MusicBrainz recording beets stored (mb_trackid); None when it stored none.
    pub recording_mbid: Option<String>,
    pub artist: String,
    pub title: String,
    /// beets' item id; None for an item not read from beets.
    #[serde(default)]
    pub beets_id: Option<u64>,
    /// The MusicBrainz release beets stored (mb_albumid); None when it stored none.
    #[serde(default)]
    pub release_mbid: Option<String>,
    /// Empty for a waiting import stored before the album was reported.
    #[serde(default)]
    pub album: String,
    /// How close the match beets applied was to the files, in thousandths, as a candidate's
    /// `per_mille`; None when beets applied none (an as-is import, an item already in the
    /// library) or the item was not read from beets.
    #[serde(default)]
    pub match_per_mille: Option<u16>,
}

/// One file of an import, with the tags beets read from it before matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub track: Option<u32>,
    pub length_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported {
        untagged: bool,
        warnings: Vec<String>,
        items: Vec<ImportedItem>,
        /// The first step after the import that failed, as `warnings` also says (a new copy that
        /// could not be moved into place), typed for the row.
        failure: Option<Failure>,
    },
    Skipped(SkipReason),
    LookupFailed {
        source: String,
        message: String,
    },
    Failed {
        kind: FailKind,
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

/// What a `Failed` import outcome went wrong with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// beets ran and failed, or the import worker stopped.
    Beets,
    /// The importer could not be set up or started.
    Setup,
    TargetUnwritable,
    /// The disk beets writes to is full.
    DiskFull,
    /// SQLite kept the library database locked past beets' own wait: another
    /// process held it, or its file system does not lock files (a network share).
    LibraryLocked,
    AsIsMissingTags,
    ChoiceInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The library has it. `existing` holds the items beets found, when it reported the skip
    /// with them; empty when only the run log said so.
    AlreadyInLibrary {
        existing: Vec<ImportedItem>,
    },
    /// No candidate was close enough. The count is the number of candidates
    /// beets weighed, when every task in the run saw the same number.
    NoMatch {
        candidates: Option<u32>,
    },
    NothingToImport,
}

impl From<&SkipReason> for SkipKind {
    fn from(reason: &SkipReason) -> Self {
        match reason {
            SkipReason::AlreadyInLibrary { .. } => SkipKind::AlreadyInLibrary,
            SkipReason::NoMatch { candidates } => SkipKind::NoMatch {
                candidates: *candidates,
            },
            SkipReason::NothingToImport => SkipKind::NothingToImport,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    Idle(Duration),
    Total(Duration),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// What a user can pick from after an automatic import skipped its only
    /// task for want of a close match.
    pub choices: Option<MatchChoices>,
    /// What a person must do before these files can import, when only a
    /// person can fix what went wrong. The importer words it, since it names
    /// the importer's own setup.
    pub user_action: Option<UserAction>,
    /// The removals beets wanted in a `DuplicateRun::ReportRemovals` or `Replace` run, one per task.
    pub removals: Vec<WantedRemoval>,
    /// The files of every task an automatic run recorded, in task order, each tag cut to
    /// `shared::failure::TEXT_MAX_CHARS` characters, since a peer chose them.
    pub files: Vec<SourceFile>,
}

impl ImportResult {
    fn without_run(outcome: ImportOutcome) -> Self {
        ImportResult {
            outcome,
            output_tail: String::new(),
            tasks: TaskCount::default(),
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        }
    }

    pub fn refused(issue: String) -> Self {
        Self::without_run(ImportOutcome::Refused { issue })
    }

    pub fn failed(kind: FailKind, message: String) -> Self {
        Self::without_run(ImportOutcome::Failed { kind, message })
    }

    pub fn nothing_to_import() -> Self {
        Self::without_run(ImportOutcome::Skipped(SkipReason::NothingToImport))
    }

    pub fn with_user_action(mut self, action: UserAction) -> Self {
        self.user_action = Some(action);
        self
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

/// The first `chars` characters of `s`.
pub(crate) fn clip(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or(s, |(i, _)| &s[..i])
}

impl fmt::Display for ImportOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportOutcome::Imported {
                untagged, warnings, ..
            } => {
                f.write_str(if *untagged {
                    "Imported as-is (untagged)"
                } else {
                    "Imported"
                })?;
                warnings.iter().try_for_each(|w| write!(f, "; {w}"))
            }
            ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing }) => {
                f.write_str("Already in library")?;
                if !existing.is_empty() {
                    let files = existing
                        .iter()
                        .map(|i| i.path.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(f, ": {files}")?;
                }
                Ok(())
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
            ImportOutcome::Failed { message, .. } => f.write_str(message),
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

/// What became of a request to drop one file from a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetOutcome {
    Removed,
    /// The library holds no item at that path, or the folder has no library.
    NotInLibrary,
    /// Several items have that path; none was removed.
    Several(usize),
    Failed(String),
}

/// What became of a request to drop from a library the items whose file is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    /// The folder has no library.
    NoLibrary,
    /// The items whose file is gone were dropped: `removed` of them; the library keeps `kept`.
    Pruned {
        removed: u64,
        kept: u64,
    },
    /// Some items' files are gone and none of the library's `items` has its file there, so the
    /// folder may be a share that is not mounted: nothing was dropped.
    Unproven {
        items: u64,
    },
    Failed(String),
}

/// What became of a request to register an existing file in a folder's beets library
/// without moving, copying, writing tags, or deleting any file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    AlreadyInLibrary,
    Failed(String),
}

/// Metadata beets stores for a single tracked library file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeetsTrackInfo {
    pub beets_id: u64,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub genre: String,
    pub comments: String,
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
}

/// Direct tag modifications to apply to an audio file via `beet modify -y -M -w`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagEdits {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub genre: Option<String>,
    pub comment: Option<String>,
}

impl TagEdits {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.album_artist.is_none()
            && self.year.is_none()
            && self.track.is_none()
            && self.disc.is_none()
            && self.genre.is_none()
            && self.comment.is_none()
    }
}

#[async_trait]
pub trait MusicImporter: Send + Sync {
    async fn import(
        &self,
        sources: &[&Path],
        target: &Path,
        mode: ImportMode,
        choice: &DriverChoice,
        duplicates: DuplicateRun,
    ) -> ImportResult;

    /// Moves the items with `item_ids` of the library of `library` to the paths the importer
    /// gives them, then reads where each item's file is, also when the move failed.
    async fn relocate(&self, library: &Path, item_ids: &[u64]) -> Relocation;

    /// Moves the item at `file` of the library of `library` to the path beets' path template
    /// gives it (`beet move`), then reads its new path.
    async fn relocate_file(&self, library: &Path, file: &Path) -> Relocation;

    /// The result of a job not run because of `issue`, the title of one of
    /// this importer's blocking health issues.
    fn refused(&self, issue: String) -> ImportResult;

    /// Removes the item at exactly `file` from the library of `library`,
    /// leaving the file alone.
    async fn forget(&self, library: &Path, file: &Path) -> ForgetOutcome;

    /// Registers an existing `file` inside `library`'s beets database without copying,
    /// moving, writing tags, or removing duplicates.
    async fn register_in_place(&self, library: &Path, file: &Path) -> RegisterOutcome;

    /// Reads beets' stored metadata for the item at `file` inside `library`, or `Ok(None)`
    /// when not tracked in that library.
    async fn inspect_file(
        &self,
        library: &Path,
        file: &Path,
    ) -> std::result::Result<Option<BeetsTrackInfo>, String>;

    /// Applies direct tag edits to `file` inside `library` in place (`--nomove --write`),
    /// registering the file first if needed.
    async fn modify_tags(
        &self,
        library: &Path,
        file: &Path,
        edits: &TagEdits,
    ) -> std::result::Result<BeetsTrackInfo, Failure>;

    /// Re-matches `sources` in place (`--nocopy --nomove`) inside `library` against metadata
    /// sources. When `pretend` is true, no tags or database rows are written (`-p -W`).
    async fn rematch_in_place(
        &self,
        sources: &[&Path],
        library: &Path,
        mode: ImportMode,
        choice: &DriverChoice,
        pretend: bool,
    ) -> ImportResult;

    /// Removes from the library of `library` every item whose file is gone, leaving every file
    /// alone; none while no item of the library has its file there.
    async fn prune(&self, library: &Path) -> PruneOutcome;

    /// The last known health, without waiting. `Checking` before the first check.
    fn health(&self) -> ImporterHealth;

    /// Checks again and returns the new health. Concurrent callers share one check.
    async fn refresh_health(&self) -> ImporterHealth;

    /// Whether what the last check depended on has changed since. True before
    /// the first check, false while one is running.
    async fn health_stale(&self) -> bool;
}

#[async_trait]
pub trait ListeningHistory: Send + Sync {
    fn name(&self) -> &'static str;
    async fn top_artists(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> std::result::Result<Vec<PlayedArtist>, HistoryError>;
    async fn top_tracks(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> std::result::Result<Vec<PlayedTrack>, HistoryError>;
    async fn listens(&self, count: usize) -> std::result::Result<Vec<Listen>, HistoryError>;
    /// Most recently favoured first, so a generator taking the first few gets the user's
    /// current favourites.
    async fn favourites(&self) -> std::result::Result<Vec<TrackRef>, HistoryError>;
    /// Every artist and track the user already has, which no generator may recommend.
    async fn known(&self) -> std::result::Result<KnownMusic, HistoryError>;
}

#[async_trait]
pub trait MusicKnowledge: Send + Sync {
    fn name(&self) -> &'static str;
    /// Ok(None): searched, no single exact match. Err: the lookup failed.
    async fn resolve_artist(
        &self,
        name: &str,
    ) -> std::result::Result<Option<String>, KnowledgeError>;
    async fn similar_artists(
        &self,
        seed_name: &str,
        seed_mbid: &str,
        limit: usize,
    ) -> std::result::Result<Vec<Neighbour>, KnowledgeError>;
    async fn artist_top_tracks(
        &self,
        artist_mbid: &str,
        limit: usize,
    ) -> std::result::Result<Vec<TrackRef>, KnowledgeError>;
    async fn artist_tags(
        &self,
        artist_mbids: &[String],
    ) -> std::result::Result<HashMap<String, Vec<WeightedTag>>, KnowledgeError>;
    async fn artist_popularity(
        &self,
        artist_mbids: &[String],
    ) -> std::result::Result<HashMap<String, u64>, KnowledgeError>;
    async fn popularity_reference(&self) -> std::result::Result<u64, KnowledgeError>;
    async fn related_tags(&self, tag: &str) -> std::result::Result<Vec<String>, KnowledgeError>;
    async fn tag_tracks(
        &self,
        tag: &str,
        count: usize,
    ) -> std::result::Result<Vec<TrackRef>, KnowledgeError>;
    async fn recordings(
        &self,
        recording_mbids: &[String],
    ) -> std::result::Result<HashMap<String, RecordingFacts>, KnowledgeError>;
}

#[async_trait]
pub trait KnowledgeCache: Send + Sync {
    /// None when absent or expired.
    async fn get(&self, key: &str) -> std::result::Result<Option<String>, CacheError>;
    async fn put(
        &self,
        key: &str,
        value: &str,
        ttl: Duration,
    ) -> std::result::Result<(), CacheError>;
}

#[async_trait]
pub trait MusicBrainzLookup: Send + Sync {
    async fn search_artists(
        &self,
        name: &str,
        limit: u8,
    ) -> std::result::Result<Vec<ArtistMatch>, MetadataError>;
    async fn genre_names(&self) -> std::result::Result<Vec<String>, MetadataError>;
}

/// What one generator found in a run. A failing source is recorded in `errors`
/// and the generator goes on with the others.
#[derive(Debug)]
pub struct GeneratorOutput {
    pub candidates: CandidateSet,
    pub signals: Vec<SignalReport>,
    pub errors: SourceErrors,
}

#[async_trait]
pub trait CandidateGenerator: Send + Sync {
    /// The key of the blend and the pipeline report.
    fn name(&self) -> &'static str;
    /// What the user sees when the report names this generator.
    fn label(&self) -> &'static str;
    /// This generator's share of the blend, before the shares of the generators
    /// that found nothing are given to the others.
    fn weight(&self) -> f64;
    async fn generate(&self, profile: &UserMusicProfile, config: &ProfileConfig)
        -> GeneratorOutput;
}
