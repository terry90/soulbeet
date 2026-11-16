//! MCP acquisition: what a request asks for, and the seam the tools and the job runner act
//! through.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::{PreviewedFile, ReplaceSet};
use crate::core::deletion::tokens::{peek_in, take_in, DeleteTokenRefusal};
use crate::core::deletion::{App, REPLACE_CONSENT_SECS};
use crate::core::folders::owned_in;
use crate::core::index::NavidromeOf;
use crate::core::library::{find, MatchKind, Reference};
use crate::core::sources::{self, Candidate, RECENT_FAILURES_SECS};
use crate::ledger::store::{self, AttemptRecord, JobRecord, StoredSearch, TimelineEntry};
use crate::ledger::{
    Admitted, BatchOrigin, IndexWait, JobKind, JobOptions, JobState, Ledger, NewJob, NewJobBatch,
    SlotResult, SourceFilters, StoredImport, MCP_UNFINISHED_JOBS_PER_USER,
};
use crate::models::folder::Folder;
use crate::models::user_settings::UserSettings;
use crate::services::NavidromeLookupError;
use runner::{spawn_drive, Acquire};
use serde_json::{json, Map, Value};
use shared::download::{DownloadProgress, DownloadQuery, DownloadableItem, IndexState, JobSlot};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::metadata::{Album, Track};
use shared::navidrome::DeletionReason;
use soulbeet::ImportMode;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

pub(crate) mod decide;
pub(crate) mod retry;
pub mod runner;
#[cfg(test)]
pub(crate) mod test_support;

/// An album asked for by its MusicBrainz release (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumRef {
    pub artist: String,
    pub title: String,
    pub release_mbid: String,
}

/// What a request asks for (D1): a track as `find_in_library` describes one, or an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Wanted {
    Track(Reference),
    Album(AlbumRef),
}

impl Wanted {
    pub fn kind(&self) -> JobKind {
        match self {
            Wanted::Track(_) => JobKind::Track,
            Wanted::Album(_) => JobKind::Album,
        }
    }

    /// The query a job and its search store: one track (its duration as "m:ss"), or an album
    /// whose id and mbid are the release MBID, its tracks empty until hydrated.
    pub fn query(&self) -> DownloadQuery {
        match self {
            Wanted::Track(r) => DownloadQuery::new(vec![Track {
                id: String::new(),
                title: r.title.clone(),
                artist: r.artist.clone(),
                album_id: None,
                album_title: r.album.clone(),
                release_date: None,
                duration: r.duration_s.map(|s| format!("{}:{:02}", s / 60, s % 60)),
                mbid: r.recording_mbid.clone(),
                release_mbid: None,
            }]),
            Wanted::Album(a) => DownloadQuery::new(vec![]).album(Album {
                id: a.release_mbid.clone(),
                title: a.title.clone(),
                artist: a.artist.clone(),
                release_date: None,
                mbid: Some(a.release_mbid.clone()),
                cover_url: None,
                release_types: None,
            }),
        }
    }

    /// The reference as a refused item's facts: `kind`, and a track's five fields (`null` when
    /// not given) or an album's three.
    pub(crate) fn facts(&self) -> Map<String, Value> {
        let facts = match self {
            Wanted::Track(r) => json!({"kind": "track", "artist": r.artist, "title": r.title,
                "album": r.album, "recording_mbid": r.recording_mbid, "duration_s": r.duration_s}),
            Wanted::Album(a) => json!({"kind": "album", "artist": a.artist, "title": a.title,
                "release_mbid": a.release_mbid}),
        };
        let Value::Object(facts) = facts else {
            unreachable!("an object literal is an object");
        };
        facts
    }
}

/// What `ensure` did with one item.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EnsureItem {
    InLibrary {
        song_ids: Vec<String>,
    },
    /// A job was made; `fuzzy` are the song ids that only fuzzily match, for Claude to judge;
    /// `replacing` the token songs whose previewed copies it replaces (D9).
    Created {
        job_id: String,
        fuzzy: Vec<String>,
        replacing: Vec<String>,
    },
    /// The user's unfinished job `job_id` already wants this recording: already_running with
    /// `facts` (the reference, the job and its state).
    AlreadyRunning {
        job_id: String,
        failure: Failure,
        facts: Map<String, Value>,
    },
    Refused(Failure, Map<String, Value>),
}

/// What `ensure` did, its items in input order.
#[derive(Debug)]
pub(crate) struct Ensured {
    /// The batch holding the jobs made, or, `for_playlist`, the one whose playlist waits for the
    /// jobs already running; None when no batch was written.
    pub batch_id: Option<String>,
    pub items: Vec<EnsureItem>,
    /// False when the user has no Navidrome link: nothing was looked up (step 5 D9).
    pub library_checked: bool,
    /// The songs of a replace_existing token that no created job replaces, in token order.
    pub replace_unmatched: Vec<String>,
}

/// The one thing ensure_in_library hands `ensure` for a replacement (D9): the app and the
/// secret of a confirmation token of a preview with reason replaced.
pub(crate) struct ReplaceConsent {
    pub app: App,
    pub secret: String,
}

/// Why `ensure` wrote no batch.
#[derive(Debug)]
pub(crate) enum EnsureRefusal {
    /// Refused before the token, if any, was taken: nothing is written and the token is unused.
    Call(CallRefused),
    /// The token was refused (peek_in or take_in): nothing is written.
    Token(DeleteTokenRefusal),
    /// The token was taken, then the batch could not be written: the token is used up, so the
    /// call cannot be made again with it. `song_ids` are the token's.
    Taken {
        failure: Failure,
        song_ids: Vec<String>,
    },
}

impl From<CallRefused> for EnsureRefusal {
    fn from(refused: CallRefused) -> EnsureRefusal {
        EnsureRefusal::Call(refused)
    }
}

impl From<Failure> for EnsureRefusal {
    fn from(failure: Failure) -> EnsureRefusal {
        EnsureRefusal::Call(failure.into())
    }
}

/// Why `ensure` wrote nothing: the failure and, for folder_required with folders, the folders the
/// call may name.
#[derive(Debug)]
pub(crate) struct CallRefused {
    pub failure: Failure,
    pub folders: Vec<Folder>,
}

impl From<Failure> for CallRefused {
    fn from(failure: Failure) -> CallRefused {
        CallRefused {
            failure,
            folders: vec![],
        }
    }
}

/// The folder: `folder_id` when ctx owns it (else `folder_not_found`), else the default one,
/// else folder_required: with the user's folders to choose from when they have some.
pub(crate) async fn folder_for(
    db: &SqlitePool,
    ctx: &UserCtx,
    folder_id: Option<&str>,
) -> Result<Folder, CallRefused> {
    if let Some(id) = folder_id {
        return Ok(owned_in(db, ctx, id).await?);
    }
    let failed = |e| database_failure(Stage::Input, e);
    let folders = Folder::get_all_by_user_in(db, &ctx.user_id)
        .await
        .map_err(failed)?;
    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(failed)?;
    // An empty stored id is no choice: Settings clears the default so.
    let default = (settings.default_download_folder_id)
        .filter(|id| !id.is_empty())
        .and_then(|id| folders.iter().find(|f| f.id == id));
    if let Some(folder) = default {
        return Ok(folder.clone());
    }
    if folders.is_empty() {
        let failure = Failure::new(
            FailureCode::FolderRequired,
            Stage::Input,
            RetryAdvice::NeedsUser,
            "This user has no library folder",
        )
        .with_user_action(UserAction::new(
            UserActionCode::AddFolder,
            "Add a library folder",
            "Settings > Library",
        ));
        return Err(failure.into());
    }
    Err(CallRefused {
        failure: Failure::new(
            FailureCode::FolderRequired,
            Stage::Input,
            RetryAdvice::NeedsDecision,
            "No folder_id was given and no default folder is set",
        ),
        folders,
    })
}

/// Design 11.5. Refuses before anything is looked up or written when the importer is broken, the
/// download backend is not configured, there is no folder, or the user's Navidrome cannot be
/// reached (no link at all is no lookup: every item gets a job). Then looks each track up with
/// `find`, one at a time in input order: one the library holds by MBID or exactly is
/// `InLibrary`, one whose lookup failed is refused with that failure. Makes one batch, labelled
/// `label`, else after the jobs it holds (`items_label`), with a job for each other item (an
/// album always gets one), each with a new search id, and starts the runner on each new job.
/// With `for_playlist`, the batch is made also when every item's job is already running for an
/// earlier call, so it can hold the order a playlist fills them in.
///
/// With `replace` (D9), the token is checked first (peek_in, reason replaced) and refused
/// before anything is looked up. Each of its songs whose previewed file is inside the folder
/// goes to the first track item, in input order, whose MBID or exact matches include it: that
/// item gets a job instead of being `InLibrary`. When some item replaces, the token is taken
/// right before the batch is written, and each replacing job's options hold a ReplaceSet of only
/// its songs' files, until REPLACE_CONSENT_SECS after the take; every other job's `replace` is
/// None, whatever `options` held. The token's songs no created job replaces are
/// `replace_unmatched`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ensure(
    acquire: &Arc<Acquire>,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    folder_id: Option<&str>,
    label: Option<String>,
    items: Vec<Wanted>,
    options: JobOptions,
    for_playlist: bool,
    replace: Option<ReplaceConsent>,
) -> Result<Ensured, EnsureRefusal> {
    if let Some(failure) = acquire.effects.importer_blocked().await {
        return Err(failure.into());
    }
    acquire.effects.backend().await?;
    let now = crate::oauth::now_unix();
    let previewed = match &replace {
        Some(ReplaceConsent { app, secret }) => {
            let expect = Some(DeletionReason::Replaced);
            let peeked = (peek_in(&acquire.db, app, secret, expect, now).await)
                .map_err(EnsureRefusal::Token)?;
            peeked.files
        }
        None => vec![],
    };
    let folder = folder_for(&acquire.db, ctx, folder_id).await?;
    let navi = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(navi) => Some(navi),
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => None,
        Err(e) => return Err(e.failure(Stage::LibraryCheck, e.to_string()).into()),
    };
    // The token songs an item may still take: only a file in the folder the jobs import into
    // can be a duplicate beets reports there.
    let mut unclaimed: Vec<&PreviewedFile> = match previewed.is_empty() {
        true => vec![],
        false => match tokio::fs::canonicalize(&folder.path).await {
            Ok(root) => (previewed.iter())
                .filter(|f| f.path.starts_with(&root))
                .collect(),
            Err(e) => {
                warn!("Could not resolve folder {}: {e}", folder.path);
                vec![]
            }
        },
    };
    // Each item's result, None while it waits for its job; and the items that get one, with
    // their fuzzy matches and the token songs they replace.
    let mut results: Vec<Option<EnsureItem>> = Vec::with_capacity(items.len());
    let mut wanting: Vec<(usize, Vec<String>, Vec<String>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let (Some(navi), Wanted::Track(reference)) = (&navi, item) else {
            results.push(None);
            wanting.push((i, vec![], vec![]));
            continue;
        };
        match find(navi, reference).await {
            Ok(found) => {
                let (owned, fuzzy): (Vec<_>, Vec<_>) =
                    (found.matches.into_iter()).partition(|m| m.how.kind != MatchKind::Fuzzy);
                let fuzzy: Vec<String> = fuzzy.into_iter().map(|m| m.song.song_id).collect();
                let (replacing, rest): (Vec<&PreviewedFile>, Vec<&PreviewedFile>) = (unclaimed
                    .into_iter())
                .partition(|f| owned.iter().any(|m| m.song.song_id == f.song_id));
                unclaimed = rest;
                if !replacing.is_empty() {
                    let replacing = replacing.iter().map(|f| f.song_id.clone()).collect();
                    results.push(None);
                    wanting.push((i, fuzzy, replacing));
                } else if owned.is_empty() {
                    results.push(None);
                    wanting.push((i, fuzzy, vec![]));
                } else {
                    let song_ids = owned.into_iter().map(|m| m.song.song_id).collect();
                    results.push(Some(EnsureItem::InLibrary { song_ids }));
                }
            }
            Err(failure) => results.push(Some(EnsureItem::Refused(failure, item.facts()))),
        }
    }
    let library_checked = navi.is_some();
    // Taken only when some job will replace, right before the batch is written: two calls with
    // one token make one batch.
    let taken = match &replace {
        Some(ReplaceConsent { app, secret })
            if wanting
                .iter()
                .any(|(_, _, replacing)| !replacing.is_empty()) =>
        {
            let expect = Some(DeletionReason::Replaced);
            let taken = (take_in(&acquire.db, app, secret, expect, now).await)
                .map_err(EnsureRefusal::Token)?;
            Some((app.grant_id.clone(), taken.files))
        }
        _ => None,
    };
    let after_take = |failure: Failure| match &taken {
        Some((_, files)) => EnsureRefusal::Taken {
            failure,
            song_ids: files.iter().map(|f| f.song_id.clone()).collect(),
        },
        None => failure.into(),
    };
    let mut batch_id = None;
    let mut replaced: Vec<String> = Vec::new();
    if !wanting.is_empty() {
        // Without a label given, the writer names the batch after the jobs it takes.
        let batch = NewJobBatch {
            id: uuid::Uuid::new_v4().to_string(),
            user: ctx.id(),
            label,
            origin: BatchOrigin::Mcp,
            for_playlist,
        };
        let id = batch.id.clone();
        let jobs: Vec<NewJob> = (wanting.iter())
            .map(|(i, _, replacing)| {
                let set =
                    (taken.as_ref())
                        .filter(|_| !replacing.is_empty())
                        .map(|(grant_id, files)| ReplaceSet {
                            grant_id: grant_id.clone(),
                            files: (files.iter())
                                .filter(|f| replacing.contains(&f.song_id))
                                .cloned()
                                .collect(),
                            expires_at: now + REPLACE_CONSENT_SECS,
                        });
                let options = JobOptions {
                    replace: set,
                    ..options.clone()
                };
                new_job(ctx, &folder, &items[*i], &options)
            })
            .collect();
        let ids: Vec<String> = jobs.iter().map(|job| job.id.clone()).collect();
        let admitted = (acquire.ledger.create_batch(batch, jobs).await)
            .map_err(|e| after_take(database_failure(Stage::Input, e)))?;
        if admitted.len() != ids.len() {
            // The writer answers each job it is given, in order.
            let e = format!("{} jobs written, {} answered", ids.len(), admitted.len());
            return Err(after_take(database_failure(Stage::Input, e)));
        }
        for (((i, fuzzy, replacing), job_id), admitted) in
            wanting.into_iter().zip(ids).zip(admitted)
        {
            let with_reference = |more: Map<String, Value>| {
                let mut all = items[i].facts();
                all.extend(more);
                all
            };
            results[i] = Some(match admitted {
                Admitted::Created => {
                    spawn_drive(acquire.clone(), ctx.clone(), job_id.clone());
                    batch_id = Some(id.clone());
                    replaced.extend(replacing.iter().cloned());
                    EnsureItem::Created {
                        job_id,
                        fuzzy,
                        replacing,
                    }
                }
                Admitted::AlreadyRunning { job, state } => {
                    if for_playlist {
                        batch_id = Some(id.clone());
                    }
                    let (failure, facts) = job_already_running(&job, state);
                    EnsureItem::AlreadyRunning {
                        job_id: job,
                        failure,
                        facts: with_reference(facts),
                    }
                }
                Admitted::OverLimit { unfinished } => {
                    let (failure, facts) = over_limit(unfinished);
                    EnsureItem::Refused(failure, with_reference(facts))
                }
            });
        }
    }
    // Every item has its answer: one looked up, or one that waited for its job.
    let items = (results.into_iter())
        .map(|item| item.expect("each item is answered"))
        .collect();
    let replace_unmatched = (previewed.iter())
        .filter(|f| !replaced.contains(&f.song_id))
        .map(|f| f.song_id.clone())
        .collect();
    Ok(Ensured {
        batch_id,
        items,
        library_checked,
        replace_unmatched,
    })
}

/// A searching MCP job of `ctx` for `item` into `folder`, under a new search id: a track imports
/// as a singleton, an album whole (step 5 D1).
fn new_job(ctx: &UserCtx, folder: &Folder, item: &Wanted, options: &JobOptions) -> NewJob {
    NewJob {
        id: uuid::Uuid::new_v4().to_string(),
        user: ctx.id(),
        folder_id: folder.id.clone(),
        wanted: item.query(),
        import_mode: match item.kind() {
            JobKind::Track => ImportMode::Singleton,
            JobKind::Album => ImportMode::Album,
        },
        state: JobState::Searching,
        search_id: Some(uuid::Uuid::new_v4().to_string()),
        picked: vec![],
        kind: item.kind(),
        options: Some(options.clone()),
    }
}

/// The user's unfinished job `job`, in `state`, wants the same recording.
pub(crate) fn job_already_running(job: &str, state: JobState) -> (Failure, Map<String, Value>) {
    let failure = Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "A job for this recording is already running",
    );
    let facts = Map::from_iter([
        ("job_id".to_string(), json!(job)),
        ("state".to_string(), json!(state.as_str())),
    ]);
    (failure, facts)
}

/// The user has `unfinished` MCP jobs, MCP_UNFINISHED_JOBS_PER_USER or more.
pub(crate) fn over_limit(unfinished: u32) -> (Failure, Map<String, Value>) {
    let failure = Failure::new(
        FailureCode::RateLimited,
        Stage::Input,
        RetryAdvice::SameSourceLater,
        format!("This user has {unfinished} unfinished downloads from connected apps, the most allowed; follow them with get_batch or list_batches before asking for more"),
    );
    let facts = Map::from_iter([
        ("unfinished_jobs".to_string(), json!(unfinished)),
        ("limit".to_string(), json!(MCP_UNFINISHED_JOBS_PER_USER)),
    ]);
    (failure, facts)
}

/// One slot of a job as the job tools show it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlotView {
    pub slot: u32,
    pub state: JobState,
    /// The last row the ledger stored, its `job` set.
    pub row: Option<DownloadProgress>,
    /// The last row sent while the slot is not terminal, kept in memory only.
    pub live: Option<DownloadProgress>,
    /// Oldest first.
    pub attempts: Vec<AttemptRecord>,
    pub result: Option<SlotResult>,
    /// Its import's wait for Navidrome, kept once the wait ended.
    pub index_wait: Option<IndexWait>,
    /// The import it waits for a decision on, while it does.
    pub kept: Option<StoredImport>,
}

/// A job with everything the ledger keeps of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobView {
    pub job: JobRecord,
    pub slots: Vec<SlotView>,
    pub timeline: Vec<TimelineEntry>,
    /// The search the job reads its sources from, while it is kept.
    pub search: Option<StoredSearch>,
    /// Its failure could not be written: it waits with no runner until the next start
    /// (`Ledger::strand`).
    pub stranded: bool,
}

/// Which jobs a tool shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JobSelect {
    Ids(Vec<String>),
    /// The user's jobs of `batch_id` when given, else all of them; with `states` not empty only
    /// those in one of them.
    Filter {
        batch_id: Option<String>,
        states: Vec<JobState>,
    },
}

/// `ctx`'s jobs as `select` says, in creation order, as their records: what a wait and a page
/// read. An id that is not one of `ctx`'s jobs is left out (get_jobs reports it).
pub(crate) async fn job_records(
    db: &SqlitePool,
    ctx: &UserCtx,
    select: &JobSelect,
) -> Result<Vec<JobRecord>, Failure> {
    let (ids, batch_id, states) = match select {
        JobSelect::Ids(ids) => (Some(ids.as_slice()), None, &[][..]),
        JobSelect::Filter { batch_id, states } => (None, batch_id.as_deref(), states.as_slice()),
    };
    store::jobs_of(db, &ctx.id(), ids, batch_id, states)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Everything the ledger keeps of `jobs`, `ctx`'s, in their order: the views of the jobs a
/// tool shows.
pub(crate) async fn views_of(
    db: &SqlitePool,
    ledger: &Ledger,
    ctx: &UserCtx,
    jobs: Vec<JobRecord>,
) -> Result<Vec<JobView>, Failure> {
    let failed = |e| database_failure(Stage::Input, e);
    let mut views = Vec::with_capacity(jobs.len());
    for job in jobs {
        let mut attempts: HashMap<u32, Vec<AttemptRecord>> = store::attempts_of(db, &job.id)
            .await
            .map_err(failed)?
            .into_iter()
            .collect();
        let slots = store::tracks_of(db, &job.id)
            .await
            .map_err(failed)?
            .into_iter()
            .map(|track| SlotView {
                live: ledger.live_row(&JobSlot {
                    job: job.id.clone(),
                    slot: track.slot,
                }),
                attempts: attempts.remove(&track.slot).unwrap_or_default(),
                slot: track.slot,
                state: track.state,
                row: track.row,
                result: track.result,
                index_wait: track.index_wait,
                kept: track.kept,
            })
            .collect();
        let timeline = store::timeline_of(db, &job.id).await.map_err(failed)?;
        let search = match &job.search_id {
            Some(id) => store::search_of(db, id, &ctx.id()).await.map_err(failed)?,
            None => None,
        };
        views.push(JobView {
            stranded: ledger.stranded(&job.id),
            job,
            slots,
            timeline,
            search,
        });
    }
    Ok(views)
}

/// `ctx`'s job `job` as the job tools read it; None when it is no job of `ctx`'s.
pub(crate) async fn job_view(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
) -> Result<Option<JobView>, Failure> {
    let select = JobSelect::Ids(vec![job.to_string()]);
    let records = job_records(&acquire.db, ctx, &select).await?;
    let mut views = views_of(&acquire.db, &acquire.ledger, ctx, records).await?;
    Ok(views.pop())
}

/// What a failed slot failed with: its index failure, else its row's failure.
pub(crate) fn slot_failure(slot: &SlotView) -> Option<&Failure> {
    let row = slot.row.as_ref()?;
    match &row.index {
        Some(IndexState::Failed(f)) => Some(f),
        _ => row.failure.as_ref(),
    }
}

/// The failed slot whose failure is the job's: the first in slot order that carries one.
pub(crate) fn failed_slot(view: &JobView) -> Option<&SlotView> {
    (view.slots.iter()).find(|slot| slot.state == JobState::Failed && slot_failure(slot).is_some())
}

/// The job's failure: `failed_slot`'s, else the failure of a job that failed before it had a
/// slot.
pub(crate) fn job_failure(view: &JobView) -> Option<Failure> {
    match failed_slot(view) {
        Some(slot) => slot_failure(slot).cloned(),
        None => view.job.failure.clone(),
    }
}

/// The candidates of a job's search it has not tried, best first (D11): the search's candidates
/// as the job's own view keeps them (its filters, the sources it excludes, each source's recent
/// failures) whose source none of its attempts used. Empty while the search runs, failed, or is
/// gone or expired.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Untried {
    pub candidates: Vec<Candidate>,
}

impl Untried {
    /// What next_source takes: the first a job may take without Claude naming it, neither
    /// flagged (D5) nor below the floor (`Candidate::automatic`).
    pub(crate) fn best(&self) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.automatic())
    }
}

/// `view`'s untried candidates at `now`.
pub(crate) async fn untried(db: &SqlitePool, view: &JobView, now: i64) -> Result<Untried, Failure> {
    Ok(untried_given(view, &recent_failures(db, now).await?, now))
}

/// Each source's failed transfers in the RECENT_FAILURES_SECS before `now` (D5).
pub(crate) async fn recent_failures(
    db: &SqlitePool,
    now: i64,
) -> Result<HashMap<String, u32>, Failure> {
    store::recent_failures(db, now - RECENT_FAILURES_SECS)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// `view`'s untried candidates at `now`, each source's recent failures being `failures`.
pub(crate) fn untried_given(view: &JobView, failures: &HashMap<String, u32>, now: i64) -> Untried {
    let Some(search) = view.search.as_ref().filter(|s| s.expires_at > now) else {
        return Untried::default();
    };
    let Some(groups) = &search.candidates else {
        return Untried::default();
    };
    let kept = sources::view(
        &search.id,
        groups,
        &job_filters(view),
        failures,
        &view.job.excluded_sources,
    );
    let tried: Vec<&str> = (view.slots.iter())
        .flat_map(|slot| &slot.attempts)
        .filter_map(|a| a.source.as_deref())
        .collect();
    let candidates = (kept.candidates.into_iter())
        .filter(|c| !tried.contains(&c.group.source.as_str()))
        .collect();
    Untried { candidates }
}

/// The filters the job's search is read with, by its runner and every tool: the job's own, else
/// (a web UI job keeps none) its search's.
pub(crate) fn job_filters(view: &JobView) -> SourceFilters {
    match (
        &view.job.options,
        view.search.as_ref().and_then(|s| s.spec.as_ref()),
    ) {
        (Some(options), _) => options.filters.clone(),
        (None, Some(spec)) => spec.filters.clone(),
        (None, None) => SourceFilters::default(),
    }
}

/// How retry_job tries a job again (design 11.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryMode {
    SameSource,
    NextSource,
    NewSearch,
    IndexOnly,
    RejectResult,
}

impl RetryMode {
    pub(crate) const ALL: [RetryMode; 5] = [
        RetryMode::SameSource,
        RetryMode::NextSource,
        RetryMode::NewSearch,
        RetryMode::IndexOnly,
        RetryMode::RejectResult,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RetryMode::SameSource => "same_source",
            RetryMode::NextSource => "next_source",
            RetryMode::NewSearch => "new_search",
            RetryMode::IndexOnly => "index_only",
            RetryMode::RejectResult => "reject_result",
        }
    }

    /// The states the mode tries a job in: each within its Transition's allowed_from (same_source
    /// and next_source are Queue, new_search Search, index_only Reindex, reject_result Reject),
    /// a job waiting for a source choice taking its source by download_source.
    pub(crate) fn states(self) -> &'static [JobState] {
        use JobState::*;
        match self {
            RetryMode::SameSource | RetryMode::NextSource => &[Failed, Cancelled],
            RetryMode::NewSearch => &[NeedsSourceChoice, Failed, Cancelled],
            RetryMode::IndexOnly => &[Failed],
            RetryMode::RejectResult => &[Done],
        }
    }
}

/// D11: whether `mode` may run on the job now, the one predicate retry_job refuses with and
/// `next` offers by. Only an MCP job is retried here; a job whose import found the recording in
/// the library is not retried at all. same_source and next_source take track jobs only, and not
/// one that failed waiting for Navidrome.
/// same_source takes the source its failure names (`source_tried`), for a cancelled job or a
/// failure that says to try that source later (never a failed search, which tried none);
/// next_source the best untried
/// candidate; index_only a slot whose wait for Navidrome failed; reject_result a done slot's
/// source.
pub(crate) fn allowed(view: &JobView, untried: &Untried, mode: RetryMode) -> bool {
    let job = &view.job;
    if job.origin != BatchOrigin::Mcp || !mode.states().contains(&job.state) {
        return false;
    }
    let failure = job_failure(view);
    if (failure.as_ref()).is_some_and(|f| f.code == FailureCode::ImportAlreadyInLibrary) {
        return false;
    }
    let source_modes = job.kind == JobKind::Track && takes_another_source(view);
    match mode {
        RetryMode::SameSource => {
            let later = || {
                failure.as_ref().is_some_and(|f| {
                    f.retry == RetryAdvice::SameSourceLater && f.stage != Stage::SourceSearch
                })
            };
            source_modes
                && source_tried(view).is_some()
                && (job.state == JobState::Cancelled || later())
        }
        RetryMode::NextSource => source_modes && untried.best().is_some(),
        RetryMode::NewSearch => true,
        RetryMode::IndexOnly => (view.slots.iter()).any(index_failed),
        RetryMode::RejectResult => done_source(view).is_some(),
    }
}

/// Whether the job may take another source (same_source, next_source, download_source): not
/// when its recording is already in the library, nor when its file is, waiting for Navidrome to
/// list it (a failure at the index stage).
pub(crate) fn takes_another_source(view: &JobView) -> bool {
    !job_failure(view)
        .is_some_and(|f| f.code == FailureCode::ImportAlreadyInLibrary || f.stage == Stage::Index)
}

/// The row a slot shows: its live row while it runs, else the last row stored.
pub(crate) fn shown_row(slot: &SlotView) -> Option<&DownloadProgress> {
    slot.live.as_ref().or(slot.row.as_ref())
}

/// The attempt `row` is of: the newest of the slot's whose candidate is the row's transfer (its
/// source and file). Failover records each alternate it was refused as an attempt after the one
/// that failed, so the newest attempt is not always the row's.
fn row_attempt<'a>(slot: &'a SlotView, row: &DownloadProgress) -> Option<&'a AttemptRecord> {
    (slot.attempts.iter().rev()).find(|a| {
        (a.candidate.as_ref()).is_some_and(|c| c.source == row.source && c.id == row.item)
    })
}

/// The attempt the slot's shown row is of (`row_attempt`). Without a row, the newest attempt
/// while it has not ended (a pick not sent yet).
pub(crate) fn current_attempt(slot: &SlotView) -> Option<&AttemptRecord> {
    match shown_row(slot) {
        Some(row) => row_attempt(slot, row),
        None => slot.attempts.last().filter(|a| a.ended_at.is_none()),
    }
}

/// The file the slot's pick took: the file of the transfer its shown row is of, the one its
/// failure names; without a row (nothing was sent), the file of its newest attempt that had one.
pub(crate) fn picked_file(slot: &SlotView) -> Option<&DownloadableItem> {
    let attempt = match shown_row(slot) {
        Some(row) => row_attempt(slot, row),
        None => (slot.attempts.iter().rev()).find(|a| a.candidate.is_some()),
    };
    attempt?.candidate.as_ref()
}

/// The first slot and its `picked_file`: what same_source takes again (a track job has one
/// slot).
fn source_tried(view: &JobView) -> Option<(u32, &DownloadableItem)> {
    let slot = view.slots.first()?;
    Some((slot.slot, picked_file(slot)?))
}

/// Whether `slot` failed waiting for Navidrome: its row's index failed at the index stage.
fn index_failed(slot: &SlotView) -> bool {
    slot.state == JobState::Failed
        && matches!(
            slot.row.as_ref().and_then(|r| r.index.as_ref()),
            Some(IndexState::Failed(f)) if f.stage == Stage::Index
        )
}

/// The source of the first done slot's last attempt: the one a rejected result came from.
fn done_source(view: &JobView) -> Option<&str> {
    let done = view.slots.iter().find(|s| s.state == JobState::Done)?;
    done.attempts.iter().rev().find_map(|a| a.source.as_deref())
}

/// The job does not exist or is another user's.
pub(crate) fn job_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Job not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::connection::test_util::{app_with_acquire, grant_of};
    use crate::core::delete_guard::test_support::previewed_file;
    use crate::core::deletion::tokens::issue_in;
    use crate::core::jobs::test_support::auto_options;
    use crate::core::test_ctx;
    use crate::ledger::items_label;
    use crate::ledger::store;
    use crate::test_navidrome::{native_song, NavidromeFor, NavidromeStub, HOLY, SILENT};

    fn track(title: &str) -> Wanted {
        Wanted::Track(Reference {
            artist: "Marrow Choir".into(),
            title: title.into(),
            album: None,
            recording_mbid: None,
            duration_s: None,
        })
    }

    /// The options of u1's jobs of `batch_id` as stored, in creation order.
    async fn stored_options(acquire: &Acquire, batch_id: &str) -> Vec<JobOptions> {
        let jobs = store::jobs_of(&acquire.db, &UserId::new("u1"), None, Some(batch_id), &[])
            .await
            .unwrap();
        jobs.into_iter().map(|job| job.options.unwrap()).collect()
    }

    #[tokio::test]
    async fn a_caller_cannot_hand_ensure_a_set_without_a_token() {
        let (_, acquire, effects, _) = app_with_acquire().await;
        let earlier = effects.library().join("x.flac");
        std::fs::write(&earlier, b"audio").unwrap();
        let handed = JobOptions {
            replace: Some(ReplaceSet {
                grant_id: "g1".into(),
                files: vec![previewed_file("s1", &earlier)],
                expires_at: 1_800_000_000,
            }),
            ..auto_options()
        };
        let ensured = ensure(
            &acquire,
            &NavidromeFor(Err(|| NavidromeLookupError::NotLinked)),
            &test_ctx("u1"),
            None,
            None,
            vec![track("O Holy Night")],
            handed,
            false,
            None,
        )
        .await
        .unwrap();
        let batch_id = ensured.batch_id.unwrap();
        let options = stored_options(&acquire, &batch_id).await;
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].replace, None);
    }

    #[tokio::test]
    async fn a_replacing_job_holds_only_its_own_songs_for_a_day() {
        let (db, acquire, effects, _) = app_with_acquire().await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1")
            .execute(&db)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        for (id, title) in [HOLY, SILENT] {
            let query = format!("title=Marrow+Choir+{}", title.replace(' ', "+"));
            let found = format!("[{}]", native_song(id, "Marrow Choir", title, 263.4, None));
            navi.answer_when("/api/song", &query, 200, &found);
        }
        let library = std::fs::canonicalize(effects.library()).unwrap();
        let file = |id: &str, name: &str| {
            let path = library.join(name);
            std::fs::write(&path, b"audio").unwrap();
            previewed_file(id, &path)
        };
        let files = vec![file(SILENT.0, "silent.flac"), file(HOLY.0, "holy.flac")];
        let app = App {
            user: UserId::new("u1"),
            grant_id: grant_of(&db, "u1").await,
        };
        let now = crate::oauth::now_unix();
        let issued = issue_in(&db, &app, DeletionReason::Replaced, &files, now)
            .await
            .unwrap();
        let ensured = ensure(
            &acquire,
            &NavidromeFor(Ok(navi.client())),
            &test_ctx("u1"),
            None,
            None,
            vec![track("O Holy Night"), track("Silent Night")],
            auto_options(),
            false,
            Some(ReplaceConsent {
                app: app.clone(),
                secret: issued.secret,
            }),
        )
        .await
        .unwrap();
        let replacing: Vec<Vec<String>> = (ensured.items.iter())
            .map(|item| match item {
                EnsureItem::Created { replacing, .. } => replacing.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            replacing,
            vec![vec![HOLY.0.to_string()], vec![SILENT.0.to_string()]]
        );
        assert!(ensured.replace_unmatched.is_empty());
        let options = stored_options(&acquire, &ensured.batch_id.unwrap()).await;
        let sets: Vec<ReplaceSet> = options.into_iter().map(|o| o.replace.unwrap()).collect();
        assert_eq!(
            sets.iter().map(|s| s.files.clone()).collect::<Vec<_>>(),
            vec![vec![files[1].clone()], vec![files[0].clone()]]
        );
        for set in &sets {
            assert_eq!(set.grant_id, app.grant_id);
            assert!(
                (set.expires_at - (now + REPLACE_CONSENT_SECS)).abs() <= 1,
                "{}",
                set.expires_at
            );
        }
    }

    #[test]
    fn a_reference_becomes_the_query_a_search_scores_against() {
        let track = Wanted::Track(Reference {
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: Some("Winter Songs".into()),
            recording_mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            duration_s: Some(263),
        });
        let query = track.query();
        assert_eq!(query.album, None);
        let t = &query.tracks[0];
        assert_eq!(
            (
                t.duration.as_deref(),
                t.duration_secs(),
                t.album_title.as_deref(),
                t.mbid.as_deref()
            ),
            (
                Some("4:23"),
                Some(263),
                Some("Winter Songs"),
                Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8")
            )
        );
        assert_eq!(
            (track.kind(), items_label(&[&query])),
            (
                JobKind::Track,
                Some("Marrow Choir - O Holy Night".to_string())
            )
        );

        let album = Wanted::Album(AlbumRef {
            artist: "Kowloon".into(),
            title: "Come Over".into(),
            release_mbid: "6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
        });
        let query = album.query();
        let a = query.album.as_ref().unwrap();
        assert_eq!(
            (a.id.as_str(), a.mbid.as_deref(), query.tracks.len()),
            (
                "6a09041b-0f79-3278-88d9-0552dd28d37f",
                Some("6a09041b-0f79-3278-88d9-0552dd28d37f"),
                0
            )
        );
        assert_eq!(
            (album.kind(), items_label(&[&query, &query])),
            (
                JobKind::Album,
                Some("Kowloon - Come Over and 1 more".to_string())
            )
        );
    }
}
