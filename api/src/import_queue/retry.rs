//! The last job of each row that was not imported, so the row can be
//! retried without the client naming any path. Kept in memory per user;
//! after a restart the ledger's kept imports are restored.

use super::choice::{files_gone, nothing_to_retry, resolve, validate_tag_overrides};
use super::{Duplicates, ImportHandle, ImportJob, ImportQueue, IMPORT_QUEUE};
use crate::auth::UserId;
use crate::core::discovery::Replace;
use crate::ledger::{self, JobEvent};
use shared::download::{
    AsIsTagOverrides, DownloadProgress, ImportChoice, ImportRefusal, JobSlot, KeptImport,
    MatchChoices, MatchKind,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use soulbeet::tags::{write_tags, WantedTags};
use soulbeet::{DriverChoice, ImportMode, ImportOutcome, SkipReason};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tracing::{info, warn};

/// Rows a client loses on reload are never forgotten by it, so each user
/// keeps at most this many, the oldest dropped first.
const MAX_PER_USER: usize = 200;

/// Whether a job of `duplicates` that ended with `outcome` is kept for a retry.
/// Not when it imported, when nothing was left to import, or when it skipped
/// what the library has: no retry can fix those.
pub(super) fn keeps_retry(outcome: &ImportOutcome, duplicates: &Duplicates) -> bool {
    !matches!(
        outcome,
        ImportOutcome::Imported { .. } | ImportOutcome::Skipped(SkipReason::NothingToImport)
    ) && !skipped_duplicate(outcome, duplicates)
}

/// Whether a job of `duplicates` that ended with `outcome` skipped what the
/// library has because it was asked to.
pub(super) fn skipped_duplicate(outcome: &ImportOutcome, duplicates: &Duplicates) -> bool {
    matches!(duplicates, Duplicates::Skip)
        && matches!(
            outcome,
            ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { .. })
        )
}

/// Whether `rows` tell their person the queue runs their import again by itself after its
/// library database was locked (`rows::retried_in`).
fn promises_lock_retry(rows: &[DownloadProgress]) -> bool {
    rows.iter().any(|row| {
        row.failure.as_ref().is_some_and(|failure| {
            failure.code == FailureCode::ImportLibraryLocked
                && failure.retry_after_seconds.is_some()
        })
    })
}

/// A kept import discarded: the job, and its files the OS refused to delete.
#[derive(Debug)]
pub struct Discarded {
    pub job: ImportJob,
    pub files_left: Vec<FileLeft>,
}

/// A downloaded file of a discarded import that is still on disk, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct FileLeft {
    pub path: PathBuf,
    pub failure: Failure,
}

/// The OS refused to delete a file of a discarded import (`e`): a person removes it.
fn not_deleted(e: &std::io::Error) -> Failure {
    Failure::new(
        FailureCode::DeleteFailed,
        Stage::Import,
        RetryAdvice::NeedsUser,
        "Soulbeet could not delete this downloaded file; it stays on disk",
    )
    .with_cause(Service::Filesystem, None, e.to_string())
}

struct Kept {
    /// Order of recording, for dropping the oldest.
    seq: u64,
    /// Shared by all rows of the job.
    job: Arc<ImportJob>,
}

impl Kept {
    /// The ledger slot of the job's row for `item`.
    fn slot(&self, item: &str) -> Option<JobSlot> {
        let owner = self.job.owner.as_ref()?;
        owner.rows.iter().find(|row| row.item == item)?.job.clone()
    }
}

impl ImportJob {
    /// Whether one of the job's rows is `slot`.
    fn has_slot(&self, slot: &JobSlot) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.rows.iter().any(|row| row.job.as_ref() == Some(slot)))
    }
}

#[derive(Default)]
struct Store {
    next_seq: u64,
    /// user id -> row item -> job
    users: HashMap<UserId, HashMap<String, Kept>>,
}

#[derive(Default)]
pub(super) struct RetryStore {
    store: Mutex<Store>,
}

impl RetryStore {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps the job for each of its rows when `keep`, and drops what its
    /// rows kept before otherwise. Returns the slots of the rows this dropped:
    /// other jobs' rows its rows replaced or removed, and rows dropped to stay
    /// within MAX_PER_USER.
    pub(super) fn record(&self, job: &Arc<ImportJob>, keep: bool) -> Vec<JobSlot> {
        let Some(owner) = &job.owner else {
            return vec![];
        };
        let mut dropped = vec![];
        let mut store = self.store();
        let seq = store.next_seq;
        store.next_seq += 1;
        let user = store.users.entry(owner.user_id.clone()).or_default();
        for row in &owner.rows {
            let before = if keep {
                let kept = Kept {
                    seq,
                    job: job.clone(),
                };
                user.insert(row.item.clone(), kept)
            } else {
                user.remove(&row.item)
            };
            dropped.extend(
                before
                    .and_then(|kept| kept.slot(&row.item))
                    .filter(|slot| !job.has_slot(slot)),
            );
        }
        while user.len() > MAX_PER_USER {
            let oldest = user
                .iter()
                .min_by_key(|(_, kept)| kept.seq)
                .map(|(item, _)| item.clone());
            let Some(item) = oldest else {
                break;
            };
            dropped.extend(user.remove(&item).and_then(|kept| kept.slot(&item)));
        }
        if user.is_empty() {
            store.users.remove(&owner.user_id);
        }
        dropped
    }

    /// Removes and returns the job of `item` once `accept` takes it, along
    /// with every entry of its sources, so no row of it can resubmit it a
    /// second time. A job `accept` refuses stays kept. Also returns the slots
    /// of other jobs' rows whose entries went with it.
    fn take<T>(
        &self,
        owner: &UserId,
        item: &str,
        accept: impl FnOnce(&ImportJob) -> Result<T, ImportRefusal>,
    ) -> Result<(Arc<ImportJob>, T, Vec<JobSlot>), ImportRefusal> {
        let mut store = self.store();
        let user = store.users.get_mut(owner).ok_or_else(nothing_to_retry)?;
        let kept = user.get(item).ok_or_else(nothing_to_retry)?;
        let accepted = accept(&kept.job)?;
        let job = kept.job.clone();
        let mut dropped = vec![];
        user.retain(|item, kept| {
            if kept.job.sources != job.sources {
                return true;
            }
            dropped.extend(kept.slot(item).filter(|slot| !job.has_slot(slot)));
            false
        });
        if user.is_empty() {
            store.users.remove(owner);
        }
        Ok((job, accepted, dropped))
    }

    fn peek(&self, owner: &UserId, item: &str) -> Option<Arc<ImportJob>> {
        let store = self.store();
        Some(store.users.get(owner)?.get(item)?.job.clone())
    }

    /// Drops the kept jobs of `items` and returns the slots of their rows.
    fn forget(&self, owner: &UserId, items: &[String]) -> Vec<JobSlot> {
        let mut store = self.store();
        let Some(user) = store.users.get_mut(owner) else {
            return vec![];
        };
        let dropped = items
            .iter()
            .filter_map(|item| user.remove(item)?.slot(item))
            .collect();
        if user.is_empty() {
            store.users.remove(owner);
        }
        dropped
    }
}

impl ImportQueue {
    pub async fn retry(
        &self,
        user: &UserId,
        item: &str,
        choice: ImportChoice,
    ) -> Result<(), ImportRefusal> {
        self.retry_with(user, item, choice, |duplicates| duplicates)
            .await
    }

    /// `retry` decided by a connected app, which removes earlier copies only as `replace` says
    /// (`Duplicates::for_app`).
    pub(crate) async fn retry_for_app(
        &self,
        user: &UserId,
        item: &str,
        choice: ImportChoice,
        replace: Replace,
    ) -> Result<(), ImportRefusal> {
        self.retry_with(user, item, choice, |duplicates| duplicates.for_app(replace))
            .await
    }

    /// `retry`, the kept import's duplicates rule passed through `rule`.
    async fn retry_with(
        &self,
        user: &UserId,
        item: &str,
        choice: ImportChoice,
        rule: impl FnOnce(Duplicates) -> Duplicates,
    ) -> Result<(), ImportRefusal> {
        let tag_overrides = match &choice {
            ImportChoice::AsIsWithTags(overrides) => Some(validate_tag_overrides(overrides)?),
            _ => None,
        };
        self.resubmit_with_tags(
            user,
            item,
            tag_overrides,
            |kept| {
                Ok(ImportJob {
                    choice: resolve(&choice, kept)?,
                    duplicates: rule(kept.duplicates.clone()),
                    // A person's retry gets every automatic run again.
                    lock_retries: 0,
                    ..kept.clone()
                })
            },
        )
        .await
    }

    /// Starts `retry_locked` of `job` after `wait`. Not async, as `enqueue` is not: the run's
    /// future awaits a finish, so the check that it is `Send` cannot sit in a finish.
    pub(super) fn retry_later(self, job: Arc<ImportJob>, wait: Duration) {
        tokio::spawn(self.retry_locked(job, wait));
    }

    /// Runs `job`, kept for its rows' Retry after a run found its library database locked,
    /// again after `wait`, as a retry would, unless a person retried, discarded or replaced it
    /// meanwhile.
    async fn retry_locked(self, job: Arc<ImportJob>, wait: Duration) {
        tokio::time::sleep(wait).await;
        let Some(owner) = &job.owner else {
            return;
        };
        let Some(row) = owner.rows.first() else {
            return;
        };
        let retried = self
            .resubmit(&owner.user_id, &row.item, |kept| {
                match std::ptr::eq(kept, Arc::as_ptr(&job)) {
                    true => Ok(ImportJob {
                        lock_retries: kept.lock_retries + 1,
                        ..kept.clone()
                    }),
                    false => Err(nothing_to_retry()),
                }
            })
            .await;
        if let Err(refusal) = retried {
            info!(
                "No automatic run of {:?} after its library database was locked: {}",
                job.sources, refusal.failure.message
            );
        }
    }

    /// Takes the kept job of `item` once `next` builds the job to submit from it, and submits
    /// that one. A job `next` refuses stays kept.
    async fn resubmit(
        &self,
        user: &UserId,
        item: &str,
        next: impl FnOnce(&ImportJob) -> Result<ImportJob, ImportRefusal>,
    ) -> Result<(), ImportRefusal> {
        self.resubmit_with_tags(user, item, None, next).await
    }

    async fn resubmit_with_tags(
        &self,
        user: &UserId,
        item: &str,
        tag_overrides: Option<AsIsTagOverrides>,
        next: impl FnOnce(&ImportJob) -> Result<ImportJob, ImportRefusal>,
    ) -> Result<(), ImportRefusal> {
        let (kept_original, job) = {
            // A finish recording what it kept cannot be split by the take.
            let _emit = self.shared.emit_lock.lock().await;
            let (original, job, dropped) = self.shared.retries.take(user, item, next)?;
            self.shared.dismiss(dropped);
            (original, job)
        };
        if !job.holds_audio().await {
            let refusal = files_gone();
            // The job is no longer kept, so its slots end as a restart would end them.
            if let Some(owner) = &job.owner {
                let rows = owner
                    .rows
                    .iter()
                    .map(|row| row.clone().failed_with(refusal.failure.clone()))
                    .collect();
                self.shared.deps.record(JobEvent::Slots {
                    state: ledger::JobState::Failed,
                    rows,
                });
            }
            return Err(refusal);
        }
        let is_as_is = job.choice == DriverChoice::AsIs;
        let user_id = user.to_string();
        if let Some(ref overrides) = tag_overrides {
            let fallback_root = crate::config::CONFIG.download_path();
            if let Err(err_msg) =
                stamp_tags_on_singleton(&job, overrides, Some(fallback_root.as_path())).await
            {
                // Re-keep the job so the user can fix or try another option.
                let _emit = self.shared.emit_lock.lock().await;
                let _ = self.shared.retries.record(&kept_original, true);
                return Err(ImportRefusal {
                    failure: Failure::new(
                        FailureCode::ImportChoiceInvalid,
                        Stage::Import,
                        RetryAdvice::NeedsDecision,
                        format!("Could not write tags before as-is import: {err_msg}"),
                    ),
                    kept: KeptImport::Kept,
                });
            }
        }
        let handle = self.submit(job).await;
        if is_as_is && Arc::ptr_eq(&self.shared, &IMPORT_QUEUE.shared) {
            tokio::spawn(mark_as_is_on_success(handle, user_id, tag_overrides));
        }
        Ok(())
    }

    /// Keeps `job` for its rows' Retry as a finished automatic run would have
    /// (a kept import read back after a restart), when its sources still hold
    /// audio; files_gone() otherwise, keeping nothing. When `shown`, the rows
    /// its last run left, say the queue runs it again by itself after its
    /// library database was locked, the queue does, from its first wait.
    pub async fn restore(
        &self,
        job: ImportJob,
        shown: &[DownloadProgress],
    ) -> Result<(), ImportRefusal> {
        if !job.holds_audio().await {
            return Err(files_gone());
        }
        let job = Arc::new(job);
        let _emit = self.shared.emit_lock.lock().await;
        let dropped = self.shared.retries.record(&job, true);
        self.shared.dismiss(dropped);
        let wait = self.shared.lock_waits.get(job.lock_retries).copied();
        if let Some(wait) = wait.filter(|_| promises_lock_retry(shown)) {
            // The Arc the store keeps, which `retry_locked` looks for.
            self.clone().retry_later(job, wait);
        }
        Ok(())
    }

    /// The kept job of `item`, left kept: its sources, mode, matches and last run.
    pub fn kept(&self, user: &UserId, item: &str) -> Result<Arc<ImportJob>, ImportRefusal> {
        self.shared
            .retries
            .peek(user, item)
            .ok_or_else(nothing_to_retry)
    }

    /// What the kept job of `item` can pick from, leaving it kept.
    pub fn import_choices(&self, user: &UserId, item: &str) -> Result<MatchChoices, ImportRefusal> {
        let job = self.kept(user, item)?;
        Ok(job.choices.as_deref().cloned().unwrap_or(MatchChoices {
            kind: match job.mode {
                ImportMode::Album => MatchKind::Album,
                ImportMode::Singleton => MatchKind::Track,
            },
            candidates: vec![],
            missing_tags: false,
        }))
    }

    /// Takes the kept job of `item` as a retry takes it, so a retry after it
    /// finds nothing, dismisses its rows' slots and deletes its own downloaded
    /// files under its cleanup root (a job without one keeps them). Taken before the
    /// delete, so a deleted file was never also resubmitted. The job with the
    /// files it could not delete, which stay on disk, or nothing_to_retry().
    pub async fn discard(&self, user: &UserId, item: &str) -> Result<Discarded, ImportRefusal> {
        let job = {
            let _emit = self.shared.emit_lock.lock().await;
            let (job, (), dropped) = self.shared.retries.take(user, item, |_| Ok(()))?;
            let own = job.owner.iter().flat_map(|owner| &owner.rows);
            let mut slots: Vec<JobSlot> = own.filter_map(|row| row.job.clone()).collect();
            slots.extend(dropped);
            self.shared.dismiss(slots);
            Arc::unwrap_or_clone(job)
        };
        let mut files_left = Vec::new();
        if let Some(root) = &job.cleanup_root {
            for (path, e) in crate::core::acquire::discard_files(&job.files, root).await {
                warn!(
                    "Could not delete the discarded import {}: {e}",
                    path.display()
                );
                // A file gone already is not left, nor is a folder left behind.
                if e.kind() != std::io::ErrorKind::NotFound && job.files.contains(&path) {
                    files_left.push(FileLeft {
                        path,
                        failure: not_deleted(&e),
                    });
                }
            }
        }
        Ok(Discarded { job, files_left })
    }

    /// Drops the kept jobs of `items` and dismisses their slots (D5).
    pub fn forget_retries(&self, user: &UserId, items: &[String]) {
        let dropped = self.shared.retries.forget(user, items);
        self.shared.dismiss(dropped);
    }
}

/// Stamps `overrides` onto the single downloaded file of a singleton `ImportJob`
/// (with `musicbrainz: None` and `album_artist = artist` so beets' path template stays clean).
/// Never walks `job.sources`: operates strictly on `job.files[0]` after rejecting albums,
/// multi-file jobs, symlinks, and any path escaping `job.cleanup_root` (or `fallback_root`).
async fn stamp_tags_on_singleton(
    job: &ImportJob,
    overrides: &AsIsTagOverrides,
    fallback_root: Option<&Path>,
) -> Result<(), String> {
    if job.mode != ImportMode::Singleton || job.files.len() != 1 {
        return Err(
            "Tagging and importing as-is is only supported for single-track imports".into(),
        );
    }
    let path = &job.files[0];
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| format!("Could not inspect file before tagging: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("Refused to write tags: target is not a regular file".into());
    }
    let canonical_file = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("Could not resolve file path before tagging: {e}"))?;
    let Some(root) = job.cleanup_root.as_deref().or(fallback_root) else {
        return Err(
            "Refused to write tags: no download root configured for path containment check".into(),
        );
    };
    let canonical_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| format!("Could not resolve download root before tagging: {e}"))?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err("Refused to write tags: file is outside the download directory".into());
    }
    let wanted = WantedTags {
        title: overrides.title.clone(),
        artist: overrides.artist.clone(),
        album: overrides.album.clone().unwrap_or_default(),
        album_artist: overrides.artist.clone(),
        track_no: None,
        track_total: None,
        date: None,
        musicbrainz: None,
        comment: None,
    };
    tokio::task::spawn_blocking(move || {
        write_tags(&canonical_file, &wanted).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("tag writer panicked: {e}"))?
}

/// Marks matching `fast_acquisitions` rows as `imported_as_is = 1` so background archival
/// upgrades never overwrite a track the user explicitly imported as-is.
pub(crate) async fn mark_fast_acquisitions_as_is_in(
    db: &sqlx::SqlitePool,
    user_id: &str,
    items: &[(String, String)],
) {
    for (artist, title) in items {
        if artist.trim().is_empty() || title.trim().is_empty() {
            continue;
        }
        if let Err(e) = sqlx::query(
            "UPDATE fast_acquisitions \
             SET imported_as_is = 1 \
             WHERE user_id = ? AND lower(artist) = lower(?) AND lower(title) = lower(?)",
        )
        .bind(user_id)
        .bind(artist)
        .bind(title)
        .execute(db)
        .await
        {
            warn!(
                "Could not mark fast_acquisitions imported_as_is for {} - {}: {e}",
                artist, title
            );
        }
    }
}

async fn mark_as_is_on_success(
    handle: ImportHandle,
    user_id: String,
    overrides: Option<AsIsTagOverrides>,
) {
    let outcome = handle.wait().await;
    let ImportOutcome::Imported { items, .. } = outcome.result.outcome else {
        return;
    };
    let mut pairs: Vec<(String, String)> = items
        .into_iter()
        .map(|item| (item.artist, item.title))
        .collect();
    if let Some(ov) = overrides {
        if !pairs.iter().any(|(a, t)| {
            a.eq_ignore_ascii_case(&ov.artist) && t.eq_ignore_ascii_case(&ov.title)
        }) {
            pairs.push((ov.artist, ov.title));
        }
    }
    if pairs.is_empty() {
        return;
    }
    mark_fast_acquisitions_as_is_in(&crate::db::DB, &user_id, &pairs).await;
}

pub async fn retry_import_for(
    user: &UserId,
    item: &str,
    choice: ImportChoice,
) -> Result<(), ImportRefusal> {
    IMPORT_QUEUE.retry(user, item, choice).await
}

pub fn import_choices_for(user: &UserId, item: &str) -> Result<MatchChoices, ImportRefusal> {
    IMPORT_QUEUE.import_choices(user, item)
}

pub fn forget_retries(user: &UserId, items: &[String]) {
    IMPORT_QUEUE.forget_retries(user, items);
}

#[cfg(test)]
mod tests {
    use crate::auth::UserId;
    use crate::import_queue::choice::{
        files_gone, no_choices, not_an_id, nothing_to_retry, unknown_candidate,
    };
    use crate::import_queue::test_support::{
        alice, bob, broken, candidate, no_match, ready, track_choices, RecordingDeps, StubImporter,
    };
    use crate::import_queue::tests::{file, finish, job, owned, queue};
    use crate::import_queue::{ImportJob, ImportMode, ImportQueue};
    use crate::ledger::{JobEvent, JobState as LedgerState};
    use shared::download::{
        AsIsTagOverrides, DownloadProgress, DownloadState, ImportChoice, JobSlot, KeptImport,
        MatchChoices, MatchKind,
    };
    use shared::failure::FailureCode;
    use soulbeet::{DriverChoice, FailKind, ImportResult};
    use std::sync::Arc;
    use std::time::Duration;

    async fn until_imported(deps: &RecordingDeps, item: &str) {
        let done = || {
            deps.progress_for(&alice())
                .iter()
                .flatten()
                .any(|(i, s)| i == item && *s == DownloadState::Imported)
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retried job never imported");
    }

    /// Waits for the `runs`th finished row of `item` and returns it.
    async fn done_row(deps: &RecordingDeps, item: &str, runs: usize) -> DownloadProgress {
        let done = || {
            deps.rows_for(&alice())
                .into_iter()
                .flatten()
                .filter(|r| {
                    r.item == item
                        && !matches!(
                            r.state,
                            DownloadState::ImportQueued(_) | DownloadState::Importing
                        )
                })
                .nth(runs - 1)
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(row) = done() {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("job never finished")
    }

    fn found() -> MatchChoices {
        track_choices(vec![candidate("FakeSrc", "w1"), candidate("FakeSrc", "w2")])
    }

    /// Submits `a` as a singleton job that skips with `found()`, and waits
    /// for its row.
    async fn skipped(
        tmp: &std::path::Path,
    ) -> (
        ImportQueue,
        Arc<StubImporter>,
        Arc<RecordingDeps>,
        ImportJob,
    ) {
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult {
            choices: Some(found()),
            ..no_match(2)
        });
        let skipping = owned(
            job(file(tmp, "dl/a.flac"), &tmp.join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(skipping.clone()).await).await;
        (queue, stub, deps, skipping)
    }

    #[tokio::test]
    async fn a_skipped_automatic_import_offers_its_matches_until_one_is_picked() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        assert!(done_row(&deps, "a", 1).await.retryable);
        // Looking does not use up the row.
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w2".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(
            stub.choices(),
            vec![
                DriverChoice::Auto,
                DriverChoice::Pick {
                    data_source: "FakeSrc".into(),
                    id: "w2".into()
                }
            ]
        );
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
    }

    #[tokio::test]
    async fn a_refused_choice_leaves_the_row_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        let unknown = ImportChoice::Candidate {
            data_source: "OtherSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(
            queue.retry(&alice(), "a", unknown).await,
            Err(unknown_candidate())
        );
        assert_eq!(
            queue
                .retry(&alice(), "a", ImportChoice::MusicBrainz("nope".into()))
                .await,
            Err(not_an_id())
        );
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.choices(), vec![DriverChoice::Auto, DriverChoice::AsIs]);
    }

    #[tokio::test]
    async fn a_chosen_run_keeps_the_matches_and_an_automatic_run_replaces_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.script(ImportResult::failed(
            FailKind::ChoiceInvalid,
            "FakeSrc has no recording w1".into(),
        ));
        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Ok(()));
        let failed_pick = done_row(&deps, "a", 2).await;
        assert!(failed_pick.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        stub.script(ImportResult::failed(
            FailKind::AsIsMissingTags,
            "Import as-is needs tags".into(),
        ));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
        assert!(done_row(&deps, "a", 3).await.retryable);

        // An automatic run that ends some other way leaves nothing to pick.
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let failed_auto = done_row(&deps, "a", 4).await;
        assert!(failed_auto.retryable);
        assert_eq!(
            queue.import_choices(&alice(), "a"),
            Ok(MatchChoices {
                kind: MatchKind::Track,
                candidates: vec![],
                missing_tags: false,
            })
        );
        let pick = ImportChoice::Candidate {
            data_source: "FakeSrc".into(),
            id: "w1".into(),
        };
        assert_eq!(queue.retry(&alice(), "a", pick).await, Err(no_choices()));
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::AsIs).await, Ok(()));
    }

    #[tokio::test]
    async fn an_automatic_retry_the_importer_never_ran_keeps_the_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;

        stub.set_health(broken());
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let refused = done_row(&deps, "a", 2).await;
        assert!(refused.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));

        stub.set_health(ready());
        stub.panic_next();
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        let stopped = done_row(&deps, "a", 3).await;
        assert!(stopped.retryable);
        assert_eq!(queue.import_choices(&alice(), "a"), Ok(found()));
    }

    #[tokio::test]
    async fn a_musicbrainz_link_picks_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps, _) = skipped(tmp.path()).await;
        let mbid = "6a09041b-cc7f-45d3-b302-39752c525af6";

        let link = format!("https://musicbrainz.org/recording/{mbid}");
        assert_eq!(
            queue
                .retry(&alice(), "a", ImportChoice::MusicBrainz(link))
                .await,
            Ok(())
        );
        until_imported(&deps, "a").await;
        assert_eq!(
            stub.choices()[1],
            DriverChoice::Pick {
                data_source: "MusicBrainz".into(),
                id: mbid.into()
            }
        );
    }

    #[tokio::test]
    async fn choices_need_a_kept_job_of_this_user() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _, _) = skipped(tmp.path()).await;
        assert_eq!(queue.import_choices(&bob(), "a"), Err(nothing_to_retry()));
        assert_eq!(queue.import_choices(&alice(), "b"), Err(nothing_to_retry()));

        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(
            job(file(tmp.path(), "dl/b.flac"), &tmp.path().join("lib")),
            &alice(),
            &["b"],
        );
        finish(queue.submit(failing).await).await;
        assert_eq!(
            queue.import_choices(&alice(), "b"),
            Ok(MatchChoices {
                kind: MatchKind::Track,
                candidates: vec![],
                missing_tags: false,
            })
        );
    }

    fn locked() -> ImportResult {
        ImportResult::failed(
            FailKind::LibraryLocked,
            "The beets library database is locked: sqlite3.OperationalError: database is locked"
                .into(),
        )
    }

    /// A queue whose runs after a locked library db wait `waits`.
    fn locking_queue(waits: Vec<Duration>) -> (ImportQueue, Arc<StubImporter>, Arc<RecordingDeps>) {
        let stub = Arc::new(StubImporter::new());
        let deps = RecordingDeps::new(stub.clone());
        (
            ImportQueue::with_lock_waits(deps.clone(), waits),
            stub,
            deps,
        )
    }

    #[tokio::test]
    async fn a_locked_library_is_imported_again_by_the_queue_after_each_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let wait = Duration::from_millis(20);
        let (queue, stub, deps) = locking_queue(vec![wait, wait]);
        stub.script(locked());
        stub.script(locked());
        let locking = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(locking).await).await;

        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 3);
        assert_eq!(stub.choices(), vec![DriverChoice::Auto; 3]);
        for runs in [1, 2] {
            let row = done_row(&deps, "a", runs).await;
            let failure = row.failure.expect("a failure");
            assert_eq!(failure.code, FailureCode::ImportLibraryLocked);
            assert_eq!(
                failure.message,
                format!("{}; Soulbeet tries again in 0 s", locked())
            );
            assert_eq!(failure.retry_after_seconds, Some(0));
            assert!(row.retryable);
        }
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
    }

    #[tokio::test]
    async fn a_lock_that_outlasts_the_waits_is_left_for_a_person_to_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let wait = Duration::from_millis(20);
        let (queue, stub, deps) = locking_queue(vec![wait]);
        stub.script(locked());
        stub.script(locked());
        let locking = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(locking).await).await;

        let last = done_row(&deps, "a", 2).await;
        assert_eq!(last.state, DownloadState::Failed(locked().to_string()));
        assert!(last.retryable);
        tokio::time::sleep(wait * 5).await;
        assert_eq!(stub.calls(), 2);

        // A person's retry gets the automatic runs again.
        stub.script(locked());
        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 4);
    }

    #[tokio::test]
    async fn a_retry_during_the_wait_leaves_the_queue_nothing_to_run() {
        let tmp = tempfile::tempdir().unwrap();
        let wait = Duration::from_millis(100);
        let (queue, stub, deps) = locking_queue(vec![wait]);
        stub.script(locked());
        let locking = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(locking).await).await;

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        until_imported(&deps, "a").await;
        tokio::time::sleep(wait * 3).await;
        assert_eq!(stub.calls(), 2);
    }

    /// Only a kept job is run again: one without rows has no Retry to keep.
    #[tokio::test]
    async fn a_locked_import_without_rows_is_not_run_again() {
        let tmp = tempfile::tempdir().unwrap();
        let wait = Duration::from_millis(10);
        let (queue, stub, _) = locking_queue(vec![wait]);
        stub.script(locked());
        let outcome = finish(
            queue
                .submit(job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")))
                .await,
        )
        .await;
        assert_eq!(outcome.result, locked());
        tokio::time::sleep(wait * 10).await;
        assert_eq!(stub.calls(), 1);
    }

    /// A locked import kept by a queue that waits `waits`, with the row its run left.
    async fn locked_before_a_restart(
        tmp: &std::path::Path,
        waits: Vec<Duration>,
    ) -> (ImportJob, DownloadProgress) {
        let (queue, stub, deps) = locking_queue(waits);
        stub.script(locked());
        let locking = owned(
            job(file(tmp, "dl/a.flac"), &tmp.join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(locking.clone()).await).await;
        let restored = ImportJob::from_stored(locking.stored().unwrap(), alice());
        (restored, done_row(&deps, "a", 1).await)
    }

    #[tokio::test]
    async fn a_locked_import_restored_after_a_restart_is_imported_again_by_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let (restored, shown) =
            locked_before_a_restart(tmp.path(), vec![Duration::from_secs(3600)]).await;
        let promised = shown.failure.as_ref().and_then(|f| f.retry_after_seconds);
        assert_eq!(promised, Some(3600));

        let (queue, stub, deps) = locking_queue(vec![Duration::from_millis(20)]);
        assert_eq!(queue.restore(restored, &[shown]).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 1);
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
    }

    #[tokio::test]
    async fn a_locked_import_out_of_tries_is_only_kept_after_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (restored, shown) = locked_before_a_restart(tmp.path(), vec![]).await;
        assert_eq!(shown.failure.as_ref().unwrap().retry_after_seconds, None);

        let wait = Duration::from_millis(20);
        let (queue, stub, _) = locking_queue(vec![wait]);
        assert_eq!(queue.restore(restored, &[shown]).await, Ok(()));
        tokio::time::sleep(wait * 5).await;
        assert_eq!(stub.calls(), 0);
        assert!(queue.import_choices(&alice(), "a").is_ok());
    }

    #[tokio::test]
    async fn retry_resubmits_the_stored_job_and_forgets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let album = tmp.path().join("dl/X");
        file(&album, "1.flac");
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));

        let mut failing = owned(job(album.clone(), &lib), &alice(), &["a", "b"]);
        failing.mode = ImportMode::Album;
        finish(queue.submit(failing.clone()).await).await;

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 2);
        assert_eq!(stub.seen()[1].0, vec![album]);
        // The whole album went back in, so its other row has nothing left.
        assert_eq!(
            queue.retry(&alice(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );

        // An import that succeeds later drops the stored job too.
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        finish(queue.submit(failing.clone()).await).await;
        finish(queue.submit(failing).await).await;
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn a_second_retry_click_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.set_health(broken());
        let refused = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(refused).await).await;
        stub.set_health(ready());

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        until_imported(&deps, "a").await;
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn a_retry_whose_files_are_gone_is_refused_without_importing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(
            job(source.clone(), &tmp.path().join("lib")),
            &alice(),
            &["a"],
        );
        finish(queue.submit(failing).await).await;
        std::fs::remove_file(&source).unwrap();

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(files_gone())
        );
        // The job is no longer kept, so its slot fails with the refusal.
        let events = deps.ledger_events();
        let Some(JobEvent::Slots {
            state: LedgerState::Failed,
            rows,
        }) = events.last()
        else {
            panic!("{events:?}")
        };
        let ended: Vec<_> = rows
            .iter()
            .map(|row| (row.job.clone(), row.failure.clone()))
            .collect();
        assert_eq!(ended, vec![(Some(first("j1")), Some(files_gone().failure))]);
        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    #[tokio::test]
    async fn nothing_to_import_is_not_kept_for_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let source = file(tmp.path(), "dl/a.flac");
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let failing = owned(job(source.clone(), &lib), &alice(), &["a"]);
        finish(queue.submit(failing.clone()).await).await;
        std::fs::remove_file(&source).unwrap();

        // The same job again, now with nothing on disk, drops the stored one.
        finish(queue.submit(failing).await).await;

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(stub.calls(), 1);
    }

    /// A singleton job of `user` for `item`, its row the ledger's `slot`.
    fn single(user: &UserId, item: &str, slot: JobSlot) -> Arc<ImportJob> {
        use crate::import_queue::test_support::row;
        use crate::import_queue::JobOwner;
        Arc::new(ImportJob {
            sources: vec![std::path::PathBuf::from(format!("/dl/{item}.flac"))],
            files: vec![std::path::PathBuf::from(format!("/dl/{item}.flac"))],
            target: std::path::PathBuf::from("/music"),
            mode: ImportMode::Singleton,
            owner: Some(JobOwner {
                user_id: user.clone(),
                rows: vec![row(item).with_job(slot)],
            }),
            snapshot_target: false,
            cleanup_root: None,
            choice: DriverChoice::Auto,
            choices: None,
            duplicates: crate::import_queue::Duplicates::AsConfigured,
            last_run: None,
            lock_retries: 0,
        })
    }

    /// `job` with its rows moved to the ledger job `id`.
    fn under(mut job: ImportJob, id: &str) -> ImportJob {
        for row in &mut job.owner.as_mut().unwrap().rows {
            row.job.as_mut().unwrap().job = id.into();
        }
        job
    }

    /// Every slot the queue recorded as dismissed, in order.
    fn dismissed(deps: &RecordingDeps) -> Vec<JobSlot> {
        deps.ledger_events()
            .into_iter()
            .filter_map(|event| match event {
                JobEvent::Dismiss(slots) => Some(slots),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn first(id: &str) -> JobSlot {
        JobSlot {
            job: id.into(),
            slot: 0,
        }
    }

    #[test]
    fn a_row_another_job_kept_hands_back_that_jobs_slot() {
        let store = super::RetryStore::default();
        let a = |id: &str| single(&alice(), "a", first(id));
        assert!(store.record(&a("j1"), true).is_empty());
        // The same job kept again replaces only itself.
        assert!(store.record(&a("j1"), true).is_empty());
        assert_eq!(store.record(&a("j2"), true), vec![first("j1")]);
        assert_eq!(store.record(&a("j3"), false), vec![first("j2")]);
        assert!(store.record(&a("j3"), false).is_empty());
    }

    #[tokio::test]
    async fn a_later_job_for_the_same_row_dismisses_the_earlier_kept_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, deps) = queue(StubImporter::new());
        let submit = |id: &str, dir: &str| {
            let source = file(tmp.path(), &format!("{dir}/a.flac"));
            under(owned(job(source, &lib), &alice(), &["a"]), id)
        };
        stub.script(no_match(1));
        finish(queue.submit(submit("j1", "dl1")).await).await;
        stub.script(no_match(1));
        finish(queue.submit(submit("j2", "dl2")).await).await;
        assert_eq!(dismissed(&deps), vec![first("j1")]);

        // A later job that imports drops the kept one too.
        finish(queue.submit(submit("j3", "dl3")).await).await;
        assert_eq!(dismissed(&deps), vec![first("j1"), first("j2")]);
        assert_eq!(queue.import_choices(&alice(), "a"), Err(nothing_to_retry()));
    }

    #[tokio::test]
    async fn a_restored_import_dismisses_the_kept_row_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, _, deps) = queue(StubImporter::new());
        for (id, dir) in [("j1", "dl1"), ("j2", "dl2")] {
            let source = file(tmp.path(), &format!("{dir}/a.flac"));
            let kept = under(owned(job(source, &lib), &alice(), &["a"]), id);
            let restored = ImportJob::from_stored(kept.stored().unwrap(), alice());
            assert_eq!(queue.restore(restored, &[]).await, Ok(()));
        }
        assert_eq!(dismissed(&deps), vec![first("j1")]);
    }

    #[tokio::test]
    async fn a_retry_dismisses_other_jobs_rows_of_the_same_files() {
        let tmp = tempfile::tempdir().unwrap();
        let source = file(tmp.path(), "dl/a.flac");
        let lib = tmp.path().join("lib");
        let (queue, stub, deps) = queue(StubImporter::new());
        for (id, item) in [("j1", "a"), ("j2", "b")] {
            stub.script(no_match(1));
            let kept = under(owned(job(source.clone(), &lib), &alice(), &[item]), id);
            finish(queue.submit(kept).await).await;
        }
        assert!(dismissed(&deps).is_empty());

        assert_eq!(queue.retry(&alice(), "a", ImportChoice::Auto).await, Ok(()));
        assert_eq!(dismissed(&deps), vec![first("j2")]);
        assert_eq!(
            queue.retry(&alice(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[test]
    fn each_user_keeps_only_the_newest_retries() {
        use super::{RetryStore, MAX_PER_USER};

        let store = RetryStore::default();
        let slot = |item: &str| JobSlot {
            job: format!("j-{item}"),
            slot: 0,
        };
        let record =
            |user: &UserId, item: &str| store.record(&single(user, item, slot(item)), true);
        for i in 0..MAX_PER_USER {
            assert!(record(&alice(), &format!("{i}")).is_empty());
        }
        // The oldest row is dropped, and its slot returned for dismissing.
        assert_eq!(
            record(&alice(), &format!("{MAX_PER_USER}")),
            vec![slot("0")]
        );
        assert!(record(&bob(), "0").is_empty());

        let take = |user: &UserId, item: &str| store.take(user, item, |_| Ok(()));
        assert_eq!(take(&alice(), "0").unwrap_err(), nothing_to_retry());
        for i in 1..=MAX_PER_USER {
            assert!(take(&alice(), &format!("{i}")).is_ok(), "row {i}");
        }
        assert!(take(&bob(), "0").is_ok());
    }

    #[tokio::test]
    async fn a_kept_import_is_found_by_the_owners_id_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::Beets,
            "beets exited with code 1".into(),
        ));
        let alice = alice();
        let failing = owned(
            job(file(tmp.path(), "dl/a.flac"), &tmp.path().join("lib")),
            &alice,
            &["a"],
        );
        finish(queue.submit(failing).await).await;

        assert_eq!(
            queue
                .retry(&UserId::new("alice"), "a", ImportChoice::Auto)
                .await,
            Err(nothing_to_retry())
        );
        assert_ne!(
            queue.retry(&alice, "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
    }

    #[tokio::test]
    async fn forget_drops_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let (queue, stub, _) = queue(StubImporter::new());
        for item in ["a", "b"] {
            stub.script(ImportResult::failed(
                FailKind::Beets,
                "beets exited with code 1".into(),
            ));
            let failing = owned(
                job(file(tmp.path(), &format!("dl/{item}.flac")), &lib),
                &alice(),
                &[item],
            );
            finish(queue.submit(failing).await).await;
        }

        queue.forget_retries(&bob(), &["b".into()]);
        queue.forget_retries(&alice(), &["a".into()]);

        assert_eq!(
            queue.retry(&alice(), "a", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(
            queue.retry(&bob(), "b", ImportChoice::Auto).await,
            Err(nothing_to_retry())
        );
        assert_eq!(queue.retry(&alice(), "b", ImportChoice::Auto).await, Ok(()));
    }

    /// Minimal valid 1-frame silent FLAC for testing tag stamping end-to-end.
    fn minimal_flac() -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.extend([0x80, 0, 0, 34]);
        out.extend(4096u16.to_be_bytes());
        out.extend(4096u16.to_be_bytes());
        out.extend([0; 6]);
        let packed: u64 = (44_100u64 << 44) | (15 << 36) | 4096;
        out.extend(packed.to_be_bytes());
        out.extend([0; 16]);
        // Frame 0 with valid CRC-8 and CRC-16.
        let mut frame = vec![0xFF, 0xF8, 0xC9, 0x08, 0];
        let mut crc8 = 0u8;
        for &b in &frame {
            crc8 ^= b;
            for _ in 0..8 {
                crc8 = if crc8 & 0x80 != 0 {
                    (crc8 << 1) ^ 0x07
                } else {
                    crc8 << 1
                };
            }
        }
        frame.push(crc8);
        frame.extend([0x00, 0x00, 0x00]);
        let mut crc16 = 0u16;
        for &b in &frame {
            crc16 ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc16 = if crc16 & 0x8000 != 0 {
                    (crc16 << 1) ^ 0x8005
                } else {
                    crc16 << 1
                };
            }
        }
        frame.extend(crc16.to_be_bytes());
        out.extend(frame);
        out
    }

    #[tokio::test]
    async fn as_is_with_tags_validates_stamps_tags_and_runs_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl/bootleg.flac");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, minimal_flac()).unwrap();

        let (queue, stub, deps) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::AsIsMissingTags,
            "Import as-is needs artist and title tags".into(),
        ));
        let mut failing = owned(job(src.clone(), &tmp.path().join("lib")), &alice(), &["a"]);
        failing.cleanup_root = Some(tmp.path().join("dl"));
        finish(queue.submit(failing).await).await;

        let choices = queue.import_choices(&alice(), "a").unwrap();
        assert!(choices.missing_tags);

        // Blank artist or title is refused while keeping the job.
        let refused = queue
            .retry(
                &alice(),
                "a",
                ImportChoice::AsIsWithTags(AsIsTagOverrides {
                    artist: "   ".into(),
                    title: "Dub Edit".into(),
                    album: None,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.failure.code, FailureCode::InvalidInput);
        assert_eq!(refused.kept, KeptImport::Kept);

        // Valid overrides stamp tags onto the FLAC file and invoke DriverChoice::AsIs.
        queue
            .retry(
                &alice(),
                "a",
                ImportChoice::AsIsWithTags(AsIsTagOverrides {
                    artist: "Burial".into(),
                    title: "Unreleased Edit".into(),
                    album: Some("White Label".into()),
                }),
            )
            .await
            .unwrap();
        until_imported(&deps, "a").await;
        assert_eq!(
            stub.choices(),
            vec![DriverChoice::Auto, DriverChoice::AsIs]
        );
    }

    #[tokio::test]
    async fn as_is_with_tags_rejects_album_and_multi_file_without_modifying_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dl/album");
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("01.flac");
        let f2 = dir.join("02.flac");
        let bytes = minimal_flac();
        std::fs::write(&f1, &bytes).unwrap();
        std::fs::write(&f2, &bytes).unwrap();

        let (queue, stub, _) = queue(StubImporter::new());
        stub.script(ImportResult::failed(
            FailKind::AsIsMissingTags,
            "Import as-is needs tags".into(),
        ));
        let mut album = owned(job(dir.clone(), &tmp.path().join("lib")), &alice(), &["a", "b"]);
        album.mode = ImportMode::Album;
        album.files = vec![f1.clone(), f2.clone()];
        album.cleanup_root = Some(tmp.path().join("dl"));
        finish(queue.submit(album).await).await;

        let refused = queue
            .retry(
                &alice(),
                "a",
                ImportChoice::AsIsWithTags(AsIsTagOverrides {
                    artist: "Artist".into(),
                    title: "Single Title".into(),
                    album: None,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.failure.code, FailureCode::InvalidInput);
        assert_eq!(refused.kept, KeptImport::Kept);
        assert_eq!(std::fs::read(&f1).unwrap(), bytes);
        assert_eq!(std::fs::read(&f2).unwrap(), bytes);
        assert_eq!(stub.calls(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn as_is_with_tags_rejects_symlinks_and_out_of_root_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dl_root = tmp.path().join("dl");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dl_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let real_target = outside.join("secret.flac");
        let bytes = minimal_flac();
        std::fs::write(&real_target, &bytes).unwrap();
        let link = dl_root.join("linked.flac");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let (queue, _stub, _) = queue(StubImporter::new());
        _stub.script(ImportResult::failed(
            FailKind::AsIsMissingTags,
            "Import as-is needs tags".into(),
        ));
        let mut linked_job = owned(job(link.clone(), &tmp.path().join("lib")), &alice(), &["s"]);
        linked_job.cleanup_root = Some(dl_root.clone());
        finish(queue.submit(linked_job).await).await;

        let refused = queue
            .retry(
                &alice(),
                "s",
                ImportChoice::AsIsWithTags(AsIsTagOverrides {
                    artist: "Artist".into(),
                    title: "Title".into(),
                    album: None,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.failure.code, FailureCode::ImportChoiceInvalid);
        assert_eq!(refused.kept, KeptImport::Kept);
        assert_eq!(std::fs::read(&real_target).unwrap(), bytes);

        // Regular file outside cleanup_root is also rejected.
        let mut outside_job = owned(
            job(real_target.clone(), &tmp.path().join("lib")),
            &alice(),
            &["o"],
        );
        outside_job.cleanup_root = Some(dl_root);
        let err = super::stamp_tags_on_singleton(
            &outside_job,
            &AsIsTagOverrides {
                artist: "Artist".into(),
                title: "Title".into(),
                album: None,
            },
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("outside the download directory"), "{err}");
        assert_eq!(std::fs::read(&real_target).unwrap(), bytes);
    }

    #[tokio::test]
    async fn mark_fast_acquisitions_as_is_updates_matching_rows() {
        let pool = crate::db::test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES ('u1', 'alice', 'h', 'user')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f1', 'u1', 'Music', '/music')")
            .execute(&pool)
            .await
            .unwrap();
        crate::models::archival_upgrade::FastAcquisitionRow::insert_in(
            &pool,
            crate::models::archival_upgrade::NewFastAcquisition {
                user_id: "u1",
                folder_id: "f1",
                navidrome_song_id: None,
                discovery_track_id: None,
                artist: "Burial",
                title: "Unreleased Edit",
                album: None,
                recording_mbid: None,
                backend_id: "ytdlp",
                codec: "opus",
                is_lossy: true,
                duration_s: Some(240),
                imported_as_is: false,
            },
        )
        .await
        .unwrap();

        super::mark_fast_acquisitions_as_is_in(
            &pool,
            "u1",
            &[("burial".into(), "unreleased edit".into())],
        )
        .await;

        let flag: i64 = sqlx::query_scalar(
            "SELECT imported_as_is FROM fast_acquisitions WHERE user_id = 'u1' AND artist = 'Burial'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(flag, 1);
    }
}
