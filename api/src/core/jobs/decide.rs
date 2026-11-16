//! Import decisions by job (design 11.6): what a job waiting for an import decision kept, read
//! from the import queue, and the decision on it, made through the queue as the web page's Retry
//! makes one. Of two decisions on one kept import, the queue's take lets only the first through.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use shared::download::{ImportChoice, ImportRefusal, KeptImport, MatchChoices};
use shared::failure::{Failure, FailureCode, Stage};
use soulbeet::{ImportMode, SourceFile, TaskCount};
use tracing::warn;

use super::retry::wrong_job_state;
use super::runner::Acquire;
use super::{job_not_found, job_view, JobView};
use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::import_queue::{files_gone, nothing_to_retry, FileLeft, ImportJob};
use crate::ledger::JobState;

/// Why a read or a decision of a kept import was refused.
#[derive(Debug)]
pub(crate) enum Why {
    /// The job, the import queue or the database refused: the failure and its facts.
    Failed(Failure, Map<String, Value>),
    /// `slot` must be `expected`: the job keeps several imports.
    Slot { expected: String },
}

/// A refused read or decision of a kept import.
#[derive(Debug)]
pub(crate) struct Refused {
    pub why: Why,
    /// The job as read after the refusal, for its state and the calls it takes; None for an id
    /// that is no job of the caller's, or a job that could not be read again.
    pub job: Option<JobView>,
    /// The refusal changed the job: the take found the kept files gone and failed the import's
    /// slots.
    pub changed: bool,
    /// The tool whose call the job's calls leave out: it would be refused the same way.
    pub refused_tool: Option<&'static str>,
}

impl Refused {
    /// `failure` alone: an id the caller has no job for, or a read that failed.
    fn bare(failure: Failure) -> Refused {
        Refused {
            why: Why::Failed(failure, Map::new()),
            job: None,
            changed: false,
            refused_tool: None,
        }
    }
}

/// A decision made: the job as it is after it, or the failure of reading it then, and the kept
/// files a discard could not delete (`in_download_folder`), which stay on disk.
#[derive(Debug)]
pub(crate) struct Decided {
    pub job: Result<JobView, Failure>,
    pub files_left: Vec<FileLeft>,
}

/// What a job's kept import offers to decide on.
#[derive(Debug)]
pub(crate) struct ImportChoices {
    pub job: JobView,
    /// The folder of the kept files, relative to the download folder.
    pub kept_folder: PathBuf,
    /// The files of the last automatic run with their tags, each path relative to `kept_folder`.
    pub files: Vec<SourceFile>,
    /// How many more files that run read, which the kept import does not keep.
    pub files_left_out: u32,
    /// None while no run of the import is known (a row kept before runs were).
    pub tasks: Option<TaskCount>,
    pub output_tail: Option<String>,
    /// What beets weighed and found too far; None when no automatic run left any.
    pub candidates: Option<MatchChoices>,
    /// The choices resolve_import takes for it.
    pub allowed: Vec<&'static str>,
}

/// A decision on a kept import.
#[derive(Debug)]
pub(crate) enum Decision {
    /// beets decides again, as the first run did.
    Retry,
    AsIs,
    /// One of the matches beets found.
    Candidate {
        data_source: String,
        id: String,
    },
    /// A MusicBrainz MBID or link, as given.
    MusicBrainz(String),
    /// The kept copy is deleted and its slots dismissed.
    Discard,
}

/// One import a job waits for a decision on.
#[derive(Debug)]
pub(crate) struct WaitingImport<'a> {
    /// Its slots waiting for the decision, in order.
    pub slots: Vec<u32>,
    /// The row item the import queue keeps it under.
    item: &'a str,
}

/// The imports `view`'s slots wait for a decision on, by their first slot: slots of one import
/// (an album imported whole) share it, an album the web page imported track by track has one
/// per track.
pub(crate) fn kept_imports(view: &JobView) -> Vec<WaitingImport<'_>> {
    let mut imports: Vec<(Option<&[PathBuf]>, WaitingImport)> = Vec::new();
    for slot in &view.slots {
        let (JobState::NeedsDecision, Some(row)) = (slot.state, &slot.row) else {
            continue;
        };
        let sources = (slot.kept.as_ref()).map(|kept| kept.sources.as_slice());
        let same = (imports.iter_mut()).find(|(of, _)| sources.is_some() && *of == sources);
        match same {
            Some((_, import)) => import.slots.push(slot.slot),
            None => imports.push((
                sources,
                WaitingImport {
                    slots: vec![slot.slot],
                    item: &row.item,
                },
            )),
        }
    }
    imports.into_iter().map(|(_, import)| import).collect()
}

/// The kept import of `ctx`'s job `job` waiting for a decision (needs_decision): with `slot`, the
/// one that slot waits on; without, the job's only one, invalid_input listing the slots when it
/// has several (an album the web page imported track by track). Its folder, the files the last
/// automatic run read with their tags, that run's tasks and output, beets' matches, and what
/// resolve_import takes for it. import_files_gone when its files were deleted, leaving it kept.
pub(crate) async fn import_choices(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    slot: Option<u32>,
) -> Result<ImportChoices, Refused> {
    const TOOL: &str = "get_import_choices";
    let (view, item) = waiting(acquire, ctx, job, slot, TOOL).await?;
    let kept = match acquire.queue.kept(&ctx.id(), &item) {
        Ok(kept) => kept,
        Err(refusal) => return Err(refused(acquire, ctx, job, refusal, Some(TOOL)).await),
    };
    if !kept.holds_audio().await {
        // Still kept: resolve_import or dismiss_job ends it.
        let gone = ImportRefusal {
            failure: files_gone().failure,
            kept: KeptImport::Kept,
        };
        return Err(refused(acquire, ctx, job, gone, Some(TOOL)).await);
    }
    let folder = kept_folder(&kept);
    let last = kept.last_run.as_deref();
    let files = (last.iter())
        .flat_map(|run| &run.files)
        .map(|file| SourceFile {
            path: relative(&file.path, &folder),
            ..file.clone()
        })
        .collect();
    let candidates = kept.choices.as_deref().cloned();
    let mut allowed = vec!["retry", "as_is"];
    if (candidates.as_ref()).is_some_and(|c| !c.candidates.is_empty()) {
        allowed.push("candidate");
    }
    allowed.extend(["musicbrainz", "discard"]);
    Ok(ImportChoices {
        job: view,
        kept_folder: relative(&folder, &acquire.download_base),
        files,
        files_left_out: last.map_or(0, |run| run.files_left_out),
        tasks: last.map(|run| run.tasks),
        output_tail: last.map(|run| run.output_tail.clone()),
        candidates,
        allowed,
    })
}

/// `decision` on the kept import of `ctx`'s job `job` (`slot` as `import_choices` takes it),
/// through the import queue: a retry with the choice, or the discard dismiss_job makes, which
/// deletes only Soulful's own kept copy. Either takes the kept import first, so one decision
/// already taken refuses the next with nothing_to_retry, the import gone. The decision made
/// answers the job as it is after it, whose import then runs on its own; a refusal carries `kept`
/// and the job as read after it, and is `changed` when it changed the job (the kept files were
/// gone, so its slots failed).
pub(crate) async fn resolve(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    slot: Option<u32>,
    decision: Decision,
) -> Result<Decided, Refused> {
    let (_, item) = waiting(acquire, ctx, job, slot, "resolve_import").await?;
    let choice = match decision {
        Decision::Retry => Some(ImportChoice::Auto),
        Decision::AsIs => Some(ImportChoice::AsIs),
        Decision::Candidate { data_source, id } => {
            Some(ImportChoice::Candidate { data_source, id })
        }
        Decision::MusicBrainz(text) => Some(ImportChoice::MusicBrainz(text)),
        Decision::Discard => None,
    };
    let (queue, user) = (&acquire.queue, ctx.id());
    let decided = match choice {
        Some(choice) => queue.retry(&user, &item, choice).await.map(|()| vec![]),
        None => (queue.discard(&user, &item).await).map(|discarded| discarded.files_left),
    };
    match decided {
        Ok(left) => Ok(Decided {
            job: read_after(acquire, ctx, job).await,
            files_left: in_download_folder(acquire, left),
        }),
        Err(refusal) => Err(refused(acquire, ctx, job, refusal, None).await),
    }
}

/// `left` with each path relative to the download folder, so no answer names a path of the
/// server.
pub(crate) fn in_download_folder(acquire: &Acquire, left: Vec<FileLeft>) -> Vec<FileLeft> {
    (left.into_iter())
        .map(|file| FileLeft {
            path: relative(&file.path, &acquire.download_base),
            ..file
        })
        .collect()
}

/// `ctx`'s job `job` once the writes sent before are applied: what the import queue recorded of
/// a decision is in it.
async fn read_after(acquire: &Acquire, ctx: &UserCtx, job: &str) -> Result<JobView, Failure> {
    (acquire.ledger.applied().await).map_err(|e| database_failure(Stage::Input, e))?;
    job_view(acquire, ctx, job).await?.ok_or_else(job_not_found)
}

/// `ctx`'s job `job` and the row item of the kept import `tool` acts on, as `import_choices`
/// picks it. A job of another user reads like an unknown one.
async fn waiting(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    slot: Option<u32>,
    tool: &'static str,
) -> Result<(JobView, String), Refused> {
    let view = (job_view(acquire, ctx, job).await)
        .map_err(Refused::bare)?
        .ok_or_else(|| Refused::bare(job_not_found()))?;
    if view.job.state != JobState::NeedsDecision {
        let (failure, facts) = wrong_job_state(view.job.state, tool, &[JobState::NeedsDecision]);
        return Err(refusal_of(view, Why::Failed(failure, facts), Some(tool)));
    }
    let imports = kept_imports(&view);
    let chosen = match slot {
        Some(slot) => imports.iter().find(|import| import.slots.contains(&slot)),
        None if imports.len() == 1 => imports.first(),
        None => None,
    };
    if let Some(import) = chosen {
        let item = import.item.to_string();
        return Ok((view, item));
    }
    if imports.is_empty() {
        // No waiting slot has a row to name its kept import by.
        let nothing = nothing_to_retry();
        let facts = kept_facts(nothing.kept);
        return Err(refusal_of(view, Why::Failed(nothing.failure, facts), None));
    }
    let slots: Vec<String> = (imports.iter())
        .flat_map(|import| &import.slots)
        .map(u32::to_string)
        .collect();
    let expected = format!(
        "one of {}, the slots waiting for a decision",
        slots.join(", ")
    );
    Err(refusal_of(view, Why::Slot { expected }, None))
}

/// The import queue's refusal of a call on `ctx`'s job `job`, its `kept` in the facts, changed
/// when the take failed the job's slots, with the job as read again now. `refused_tool`'s call is
/// left out of the calls the job takes: it would answer the same.
async fn refused(
    acquire: &Acquire,
    ctx: &UserCtx,
    job: &str,
    refusal: ImportRefusal,
    refused_tool: Option<&'static str>,
) -> Refused {
    let ImportRefusal { failure, kept } = refusal;
    // A take that found the files gone failed the import's slots.
    let changed = failure.code == FailureCode::ImportFilesGone && kept == KeptImport::Gone;
    let job = match read_after(acquire, ctx, job).await {
        Ok(view) => Some(view),
        Err(f) => {
            warn!(
                "Could not read job {job} again after a refused decision: {}",
                f.message
            );
            None
        }
    };
    Refused {
        why: Why::Failed(failure, kept_facts(kept)),
        job,
        changed,
        refused_tool,
    }
}

/// `why` of `view`'s job, unchanged, leaving `refused_tool`'s call out of the calls it takes.
fn refusal_of(view: JobView, why: Why, refused_tool: Option<&'static str>) -> Refused {
    Refused {
        why,
        job: Some(view),
        changed: false,
        refused_tool,
    }
}

/// `{"kept": "kept"|"gone"}`: what the import queue still keeps.
fn kept_facts(kept: KeptImport) -> Map<String, Value> {
    let kept = match kept {
        KeptImport::Kept => "kept",
        KeptImport::Gone => "gone",
    };
    Map::from_iter([("kept".to_string(), json!(kept))])
}

/// The folder `job`'s files are kept in: an album's source folder, a track's file's folder.
fn kept_folder(job: &ImportJob) -> PathBuf {
    let Some(source) = job.sources.first() else {
        return PathBuf::new();
    };
    match job.mode {
        ImportMode::Album => source.clone(),
        ImportMode::Singleton => source.parent().unwrap_or(source).to_path_buf(),
    }
}

/// `path` relative to `base`, so a result never names a path of the server. A path that is not
/// under `base` as written (beets reports it by another mount of the same folder) is taken from
/// after the last folder named as `base`'s own last folder, so a disc's subfolder stays; else it
/// is its last name.
fn relative(path: &Path, base: &Path) -> PathBuf {
    if let Ok(inside) = path.strip_prefix(base) {
        return inside.to_path_buf();
    }
    let parts: Vec<_> = path.components().collect();
    let after_base = base.file_name().and_then(|name| {
        let at = parts.iter().rposition(|part| part.as_os_str() == name)?;
        Some(parts[at + 1..].iter().collect::<PathBuf>())
    });
    match after_base {
        Some(inside) if inside.components().next().is_some() => inside,
        _ => path.file_name().map(PathBuf::from).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shared::failure::FailureCode;
    use soulbeet::TaskCount;

    use super::*;
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{
        kept_album_job, kept_track_job_with, no_match_with_files,
    };
    use crate::core::test_ctx;
    use crate::ledger::{store, JobState};

    /// The failure and facts of a refusal that is not about `slot`.
    fn failed(refused: &Refused) -> (&Failure, &Map<String, Value>) {
        match &refused.why {
            Why::Failed(failure, facts) => (failure, facts),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_kept_file_is_named_inside_its_folder_wherever_beets_saw_it() {
        let folder = Path::new("/downloads/Come Over [1f2e3d4c]");
        assert_eq!(
            relative(&folder.join("CD2/01.flac"), folder),
            Path::new("CD2/01.flac")
        );
        // The same folder by another mount: the disc's folder stays.
        let mounted = Path::new("/mnt/data/downloads/Come Over [1f2e3d4c]/CD2/01.flac");
        assert_eq!(relative(mounted, folder), Path::new("CD2/01.flac"));
        assert_eq!(
            relative(Path::new("/elsewhere/01.flac"), folder),
            Path::new("01.flac")
        );
    }

    #[tokio::test]
    async fn the_choices_name_the_kept_files_their_tags_and_beets_candidates() {
        let (acq, effects) = Acquire::for_alice().await;
        // beets weighed 2 candidates for "Song Number 1".
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(2)).await;
        let choices = import_choices(&acq, &test_ctx("alice"), &job, None)
            .await
            .unwrap();
        assert_eq!(
            choices
                .files
                .iter()
                .map(|f| (f.artist.as_str(), f.title.as_str()))
                .collect::<Vec<_>>(),
            vec![("Zzqx Artist", "Song Number 1")]
        );
        assert_eq!(choices.candidates.map(|c| c.candidates.len()), Some(2));
        assert_eq!(
            choices.allowed,
            vec!["retry", "as_is", "candidate", "musicbrainz", "discard"]
        );
        assert_eq!(
            choices.tasks,
            Some(TaskCount {
                imported: 0,
                total: 1
            })
        );
        assert_eq!(
            choices.kept_folder,
            std::path::Path::new("Come Over [1f2e3d4c]")
        );
    }

    #[tokio::test]
    async fn a_kept_import_is_taken_by_one_decision_only() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(2)).await;
        let ctx = test_ctx("alice");
        let (a, b) = tokio::join!(
            resolve(&acq, &ctx, &job, None, Decision::AsIs),
            resolve(&acq, &ctx, &job, None, Decision::Discard)
        );
        // Whichever takes the kept import first wins; the other is refused the same way in
        // either order.
        let discard_won = b.is_ok();
        let refused = [a, b]
            .into_iter()
            .filter_map(Result::err)
            .collect::<Vec<_>>();
        assert_eq!(refused.len(), 1);
        let (failure, facts) = failed(&refused[0]);
        assert_eq!(
            (failure.code, failure.message.as_str(), &facts["kept"]),
            (
                FailureCode::UnknownHandle,
                "Nothing to retry",
                &json!("gone")
            )
        );
        assert!(!refused[0].changed);
        acq.ledger.settled().await;
        let state = store::job_state(&acq.db, &job).await.unwrap();
        assert_eq!(state == JobState::Dismissed, discard_won, "{state:?}");
    }

    #[tokio::test]
    async fn a_job_keeping_several_imports_is_decided_one_slot_at_a_time() {
        let (acq, _) = Acquire::for_alice().await;
        let dir = tempfile::tempdir().unwrap();
        let files = ["07.flac", "08.flac"].map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, b"flac").unwrap();
            file
        });
        // Each file imported apart, each import kept.
        let job = kept_album_job(&acq, [&files[0], &files[1]], [true, true]).await;
        let ctx = test_ctx("alice");
        let refused = import_choices(&acq, &ctx, &job, None).await.unwrap_err();
        assert!(
            matches!(&refused.why, Why::Slot { expected }
                if expected == "one of 0, 1, the slots waiting for a decision"),
            "{refused:?}"
        );
        assert_eq!(
            refused.refused_tool, None,
            "its calls with a slot are offered"
        );
        // Restored after a restart from a row kept before runs were: no run is known.
        let second = import_choices(&acq, &ctx, &job, Some(1)).await.unwrap();
        assert_eq!((second.tasks, second.output_tail), (None, None));
        let view = resolve(&acq, &ctx, &job, Some(1), Decision::Discard)
            .await
            .unwrap()
            .job
            .unwrap();
        let states: Vec<JobState> = view.slots.iter().map(|s| s.state).collect();
        assert_eq!(states, vec![JobState::NeedsDecision, JobState::Dismissed]);
        assert!(acq.queue.kept(&ctx.id(), "a/07.flac").is_ok());
        // One import is left: no slot is needed now.
        assert!(import_choices(&acq, &ctx, &job, None).await.is_ok());
    }

    #[tokio::test]
    async fn deleted_kept_files_are_shown_gone_and_a_decision_fails_the_job() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(1)).await;
        std::fs::remove_dir_all(acq.download_base.join("Come Over [1f2e3d4c]")).unwrap();
        let ctx = test_ctx("alice");
        let shown = import_choices(&acq, &ctx, &job, None).await.unwrap_err();
        let (failure, facts) = failed(&shown);
        assert_eq!(
            (failure.code, &facts["kept"]),
            (FailureCode::ImportFilesGone, &json!("kept"))
        );
        assert!(!shown.changed);
        assert_eq!(shown.refused_tool, Some("get_import_choices"));
        let decided = resolve(&acq, &ctx, &job, None, Decision::AsIs)
            .await
            .unwrap_err();
        let (failure, facts) = failed(&decided);
        assert_eq!(
            (failure.code, &facts["kept"]),
            (FailureCode::ImportFilesGone, &json!("gone"))
        );
        assert!(decided.changed);
        assert_eq!(decided.job.unwrap().job.state, JobState::Failed);
        assert_eq!(effects.importer().calls(), 1, "nothing imported again");
    }

    /// The other order of the race above: the discard asks first.
    #[tokio::test]
    async fn a_retry_after_a_discard_took_the_import_is_refused_alike() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(2)).await;
        let ctx = test_ctx("alice");
        let (discarded, retried) = tokio::join!(
            resolve(&acq, &ctx, &job, None, Decision::Discard),
            resolve(&acq, &ctx, &job, None, Decision::Retry)
        );
        assert_eq!(
            discarded.unwrap().job.unwrap().job.state,
            JobState::Dismissed
        );
        let refused = retried.unwrap_err();
        let (failure, facts) = failed(&refused);
        assert_eq!(
            (failure.code, &facts["kept"]),
            (FailureCode::UnknownHandle, &json!("gone"))
        );
        // Read after the discard: nothing is left to decide.
        assert_eq!(refused.job.unwrap().job.state, JobState::Dismissed);
        assert_eq!(effects.importer().calls(), 1, "only the first run imported");
    }

    #[tokio::test]
    async fn a_musicbrainz_link_to_the_wrong_entity_keeps_the_import() {
        let (acq, effects) = Acquire::for_alice().await;
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(0)).await;
        let refused = resolve(
            &acq,
            &test_ctx("alice"),
            &job,
            None,
            Decision::MusicBrainz(
                "https://musicbrainz.org/release/6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
            ),
        )
        .await
        .unwrap_err();
        let (failure, facts) = failed(&refused);
        assert_eq!(
            failure.message,
            "Track imports need a MusicBrainz recording, not a release"
        );
        assert_eq!(facts["kept"], "kept");
        assert_eq!(refused.job.unwrap().job.state, JobState::NeedsDecision);
    }
}
