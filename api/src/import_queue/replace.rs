//! The library's earlier copy of an import, replaced only through the delete guard (D2), and for
//! a replacing job only the copies its confirmed preview named (D9).

use crate::auth::UserId;
use crate::core::delete_guard::{
    previewed_at, Checked, ForgetVia, Guard, NotRemoved, Origin, PreviewedFile, ReplaceSet,
};
use crate::core::deletion::replace_consent_expired;
use crate::core::{logged, stored_time};
use crate::ledger::{Replacement, ReplacementOutcome};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::navidrome::DeletionReason;
use soulbeet::{
    duplicate_note, DuplicateNote, ImportOutcome, ImportResult, ImportedItem, MusicImporter,
    WantedRemoval,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Why a removal of a run is not made: an earlier removal of the same run deleted one of its
/// files or moved a new file there, or one of its files is a file this run imported.
const OVERLAPS: &str = "this import already replaced or added one of these files";

/// Why the files after one whose removal did not report are kept.
const AFTER_UNCONFIRMED: &str = "an earlier removal of this import stopped before it reported";

/// Minimum tolerance (15 seconds) for replacement duration guard before the
/// 10% relative threshold dominates.
const DURATION_GUARD_MIN_TOLERANCE_S: u32 = 15;

/// A removal refused because the newly imported file's duration differs too far
/// from the existing library file.
fn replacement_duration_mismatch(new_s: u32, old_s: u32) -> Failure {
    Failure::new(
        FailureCode::ReplacementDurationMismatch,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        format!(
            "Replacement duration ({new_s}s) differs too much from existing copy ({old_s}s)"
        ),
    )
}

/// Compares the newly imported file(s) against the existing library file(s)
/// before deleting any existing copy. If both durations are known (> 0) and
/// `|new_s - old_s| > max(15s, old_s / 10)`, returns `ReplacementDurationMismatch`.
async fn check_replacement_durations(
    checked: &[Checked],
    imported: &[(u64, PathBuf)],
    target: &Path,
) -> Result<(), Failure> {
    let existing_inputs: Vec<(PathBuf, Option<u32>)> = checked
        .iter()
        .map(|c| {
            let navidrome_s = (c.song.duration.is_finite() && c.song.duration >= 1.0)
                .then(|| c.song.duration.round() as u32);
            (c.path.clone(), navidrome_s)
        })
        .collect();
    let imported_paths: Vec<PathBuf> = imported.iter().map(|(_, rel)| target.join(rel)).collect();

    tokio::task::spawn_blocking(move || {
        let old_durations: Vec<Option<u32>> = existing_inputs
            .iter()
            .map(|(path, fallback)| soulbeet::audio_duration_secs(path).or(*fallback))
            .collect();
        let new_durations: Vec<Option<u32>> = imported_paths
            .iter()
            .map(|path| soulbeet::audio_duration_secs(path))
            .collect();

        if old_durations.len() == new_durations.len() {
            for (old_s, new_s) in old_durations.into_iter().zip(new_durations) {
                if let (Some(old_s), Some(new_s)) = (old_s, new_s) {
                    if old_s > 0 && new_s > 0 {
                        let tolerance = DURATION_GUARD_MIN_TOLERANCE_S.max(old_s / 10);
                        if new_s.abs_diff(old_s) > tolerance {
                            return Err(replacement_duration_mismatch(new_s, old_s));
                        }
                    }
                }
            }
        } else if old_durations.iter().all(|d| d.is_some_and(|s| s > 0))
            && new_durations.iter().all(|d| d.is_some_and(|s| s > 0))
        {
            let old_total: u32 = old_durations.into_iter().flatten().sum();
            let new_total: u32 = new_durations.into_iter().flatten().sum();
            let tolerance = DURATION_GUARD_MIN_TOLERANCE_S.max(old_total / 10);
            if new_total.abs_diff(old_total) > tolerance {
                return Err(replacement_duration_mismatch(new_total, old_total));
            }
        }
        Ok(())
    })
    .await
    .unwrap_or(Ok(()))
}

/// A removal refused because it overlaps an earlier one of its run (OVERLAPS): the run itself
/// holds one of its files.
fn overlapping() -> Failure {
    Failure::new(
        FailureCode::DeleteInUse,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        OVERLAPS,
    )
}

/// Which earlier copies settle_removals may delete.
pub(super) enum Removals<'a> {
    /// Step 6's rule: the guard alone decides, as `origin`.
    Guarded(&'a Origin),
    /// A replacing job (D9): before `set.expires_at`, only an earlier item whose Checked passes
    /// `is_in(&set.files)` counts as allowed (its refusal is the note's reason like any other),
    /// deleted as `Origin::Mcp { grant_id: set.grant_id }`; from `expires_at` on, every removal
    /// is kept with replace_consent_expired(set.expires_at) and nothing is checked.
    Previewed { set: &'a ReplaceSet, now: i64 },
}

/// A run's result with what became of each removal it wanted.
pub(super) struct Settled {
    pub result: ImportResult,
    /// One per removal settled, in order.
    pub replaced: Vec<Replacement>,
    /// Previewed: the song ids of `set.files` whose path no deletion of this call removed, in set
    /// order. Guarded: empty.
    pub not_replaced: Vec<String>,
}

/// Settles the removals beets wanted in `result`, an import into `target` (the canonical
/// library folder beets ran on), as one run, and appends what became of each earlier copy to
/// the outcome's warnings, and the first new copy that could not be moved into place to its
/// failure. The removals are taken out of `result`, which keeps none. Only an Imported outcome
/// is settled: any other is returned with nothing removed, so a run the row reports as failed
/// deletes nothing.
pub(super) async fn settle_removals(
    guard: &Guard<'_>,
    importer: &dyn MusicImporter,
    actor: &UserId,
    removals: Removals<'_>,
    exempt_jobs: &[String],
    target: &Path,
    mut result: ImportResult,
) -> Settled {
    let (origin, previewed) = match removals {
        Removals::Guarded(origin) => (origin.clone(), None),
        Removals::Previewed { set, now } => (
            Origin::Mcp {
                grant_id: set.grant_id.clone(),
            },
            Some((set, now)),
        ),
    };
    let wanted = std::mem::take(&mut result.removals);
    let mut run = Run {
        remover: Remover {
            guard,
            importer,
            actor,
            origin: &origin,
            previewed: previewed.map(|(set, _)| set.files.as_slice()),
            expired: previewed
                .filter(|(set, now)| *now >= set.expires_at)
                .map(
                    |(set, _)| match stored_time(Stage::Delete, set.expires_at) {
                        Ok(at) => replace_consent_expired(at),
                        Err(unreadable) => unreadable,
                    },
                ),
            exempt_jobs,
            target,
        },
        taken: HashSet::new(),
        removed: Vec::new(),
        unmoved: None,
    };
    let mut replaced = Vec::new();
    if let ImportOutcome::Imported {
        warnings,
        items,
        failure,
        ..
    } = &mut result.outcome
    {
        run.taken = items
            .iter()
            .map(|item| item.path.clone())
            .chain(
                wanted
                    .iter()
                    .flat_map(|r| r.imported.iter().map(|(_, p)| p.clone())),
            )
            .collect();
        for removal in &wanted {
            let (notes, outcome) = run.settle(removal, items).await;
            warnings.extend(notes);
            replaced.extend(outcome.map(|outcome| Replacement {
                existing: removal.existing.clone(),
                outcome,
            }));
        }
        if failure.is_none() {
            *failure = run.unmoved;
        }
    }
    Settled {
        result,
        replaced,
        not_replaced: not_replaced(previewed.map(|(set, _)| set), &run.removed),
    }
}

/// The song ids of `set`'s files whose path is not one of `removed`, in set order; none without
/// a set.
pub(super) fn not_replaced(set: Option<&ReplaceSet>, removed: &[PathBuf]) -> Vec<String> {
    (set.into_iter())
        .flat_map(|set| &set.files)
        .filter(|file| !removed.contains(&file.path))
        .map(|file| file.song_id.clone())
        .collect()
}

struct Remover<'a> {
    guard: &'a Guard<'a>,
    importer: &'a dyn MusicImporter,
    actor: &'a UserId,
    origin: &'a Origin,
    /// For a replacing job, the previewed files, the only ones it may delete.
    previewed: Option<&'a [PreviewedFile]>,
    /// For a replacing job whose consent ran out, why every removal is kept.
    expired: Option<Failure>,
    exempt_jobs: &'a [String],
    target: &'a Path,
}

impl Remover<'_> {
    /// `item`, an earlier copy in the target, checked by the guard and, for a replacing job,
    /// against the preview.
    async fn check(&self, item: &ImportedItem) -> Result<Checked, Failure> {
        // A copy the preview does not hold is kept before the guard's checks, whose rule 4
        // re-sends a rating to Navidrome; the checked file is held to the preview again after.
        if let Some(previewed) = self.previewed {
            previewed_at(&self.target.join(&item.path), previewed).await?;
        }
        let checked = (self.guard)
            .check_file(self.actor, item, self.target, self.exempt_jobs)
            .await?;
        if let Some(previewed) = self.previewed {
            checked.is_in(previewed)?;
        }
        Ok(checked)
    }
}

/// One run's removals, settled in turn.
struct Run<'a> {
    remover: Remover<'a>,
    /// Paths, relative to the target as beets names items, that a removal of this run must not
    /// touch: every file the run imported, and each file an earlier removal deleted or moved a
    /// new file to.
    taken: HashSet<PathBuf>,
    /// The canonical files this run deleted.
    removed: Vec<PathBuf>,
    /// The failure of the first new copy that could not be moved into place.
    unmoved: Option<Failure>,
}

impl Run<'_> {
    /// The notes of one removal and what became of it. One whose task imported nothing is
    /// skipped (no outcome): its new items never reached the library (D2). One that names no
    /// earlier item is skipped too: there is nothing it may replace. For a replacing job whose
    /// consent ran out every one is kept, checked for nothing. One that names a path `taken`
    /// holds is refused (OVERLAPS). Every earlier item is checked before any is deleted; the new
    /// items are moved into place only once all are deleted and forgotten cleanly, and `items`
    /// then names them where the importer has them after the move, failed or not.
    async fn settle(
        &mut self,
        removal: &WantedRemoval,
        items: &mut [ImportedItem],
    ) -> (Vec<String>, Option<ReplacementOutcome>) {
        if removal.imported.is_empty() || removal.existing.is_empty() {
            return (vec![], None);
        }
        let remover = &self.remover;
        let target = remover.target;
        let earlier: Vec<PathBuf> = removal
            .existing
            .iter()
            .map(|item| target.join(&item.path))
            .collect();
        let kept = |f: &Failure| {
            let note = duplicate_note(&earlier, target, DuplicateNote::Blocked(&f.message));
            let outcome = ReplacementOutcome::Kept { failure: f.clone() };
            (vec![note], Some(outcome))
        };
        if let Some(f) = &remover.expired {
            warn!(
                "Kept the earlier copy {} of an import into {}: {}",
                earlier[0].display(),
                target.display(),
                logged(f)
            );
            return kept(f);
        }
        if removal
            .existing
            .iter()
            .any(|item| self.taken.contains(&item.path))
        {
            warn!(
                "Kept the earlier copy {} of an import into {}: {OVERLAPS}",
                earlier[0].display(),
                target.display()
            );
            return kept(&overlapping());
        }
        let mut checked = Vec::with_capacity(removal.existing.len());
        for (item, file) in removal.existing.iter().zip(&earlier) {
            match remover.check(item).await {
                Ok(c) => checked.push(c),
                Err(f) => {
                    warn!(
                        "Kept the earlier copy {} of an import into {}: {}",
                        file.display(),
                        target.display(),
                        logged(&f)
                    );
                    return kept(&f);
                }
            }
        }
        if let Err(f) = check_replacement_durations(&checked, &removal.imported, target).await {
            warn!(
                "Kept the earlier copy {} of an import into {}: {}",
                earlier[0].display(),
                target.display(),
                logged(&f)
            );
            return kept(&f);
        }
        let mut warnings = Vec::new();
        let mut removed = Vec::new();
        for (deleted, c) in checked.into_iter().enumerate() {
            let forget = ForgetVia::Worker {
                importer: remover.importer,
                library: target,
            };
            let path = c.path.clone();
            let outcome = remover
                .guard
                .carry_out(
                    remover.actor,
                    c,
                    DeletionReason::Replaced,
                    remover.origin,
                    forget,
                )
                .await;
            match outcome {
                Ok(done) => {
                    let item = removal.existing[deleted].path.clone();
                    self.taken.insert(item.clone());
                    self.removed.push(path);
                    removed.push(item);
                    warnings.extend(done);
                }
                Err(stop) => {
                    let (what, f) = match &stop {
                        NotRemoved::Kept(f) => ("Kept", f),
                        NotRemoved::Unconfirmed(f) => ("May have removed", f),
                    };
                    warn!(
                        "{what} the earlier copy {} of an import into {}: {}",
                        earlier[deleted].display(),
                        target.display(),
                        logged(f)
                    );
                    let mut notes = stopped_notes(&earlier, deleted, target, &stop);
                    notes.extend(warnings);
                    let outcome = match stop {
                        NotRemoved::Kept(failure) if removed.is_empty() => {
                            ReplacementOutcome::Kept { failure }
                        }
                        NotRemoved::Kept(failure) | NotRemoved::Unconfirmed(failure) => {
                            ReplacementOutcome::PartlyRemoved { removed, failure }
                        }
                    };
                    return (notes, Some(outcome));
                }
            }
        }
        let mut notes = vec![duplicate_note(&earlier, target, DuplicateNote::Replaced)];
        // A forget that did not drop exactly one item leaves beets' database unsure, so the new
        // items stay where beets put them.
        if warnings.is_empty() {
            let ids: Vec<u64> = removal.imported.iter().map(|(id, _)| *id).collect();
            let moved = remover.importer.relocate(target, &ids).await;
            self.taken
                .extend(moved.now.iter().map(|(_, now)| now.clone()));
            renamed(items, &removal.imported, &moved.now);
            if let Some(f) = moved.failure {
                warn!(
                    "Moving the new copy of an import into {}: {}",
                    target.display(),
                    logged(&f)
                );
                notes.push(f.message.clone());
                self.unmoved.get_or_insert(f);
            }
        }
        notes.extend(warnings);
        (notes, Some(ReplacementOutcome::Replaced))
    }
}

/// The notes of a removal whose deletions stopped at `earlier[deleted]`: the files deleted before
/// it, removed; that file and the rest, kept with its reason when `stop` kept it; when its
/// removal is unconfirmed, that file as one that may have been removed, and the rest kept
/// because of it.
fn stopped_notes(
    earlier: &[PathBuf],
    deleted: usize,
    target: &Path,
    stop: &NotRemoved,
) -> Vec<String> {
    let mut notes = Vec::new();
    if deleted > 0 {
        notes.push(duplicate_note(
            &earlier[..deleted],
            target,
            DuplicateNote::Removed,
        ));
    }
    let (kept, reason) = match stop {
        NotRemoved::Kept(f) => (&earlier[deleted..], kept_reason(f)),
        NotRemoved::Unconfirmed(_) => {
            notes.push(duplicate_note(
                &earlier[deleted..=deleted],
                target,
                DuplicateNote::Unconfirmed,
            ));
            (&earlier[deleted + 1..], AFTER_UNCONFIRMED.to_string())
        }
    };
    if !kept.is_empty() {
        notes.push(duplicate_note(
            kept,
            target,
            DuplicateNote::Blocked(&reason),
        ));
    }
    notes
}

/// Why `f` kept a copy, as its note says it: the message, and for a failed unlink the
/// filesystem's reason too (delete_failed's cause, an OS error, which names no path).
fn kept_reason(f: &Failure) -> String {
    match &f.cause {
        Some(cause)
            if f.code == FailureCode::DeleteFailed && cause.service == Service::Filesystem =>
        {
            format!("{}: {}", f.message, cause.raw)
        }
        _ => f.message.clone(),
    }
}

/// Gives each of `items` that is an item of `imported` the path `moved` gives its id, both
/// relative to the target as beets reports items. Each item is matched by the path it had
/// before the move.
fn renamed(items: &mut [ImportedItem], imported: &[(u64, PathBuf)], moved: &[(u64, PathBuf)]) {
    for item in items.iter_mut() {
        let Some((id, _)) = imported.iter().find(|(_, was)| *was == item.path) else {
            continue;
        };
        if let Some((_, now)) = moved.iter().find(|(moved_id, _)| moved_id == id) {
            item.path = now.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::not_previewed;
    use crate::core::delete_guard::quota::permission_not_granted;
    use crate::core::delete_guard::test_support::{library_verdict, wanted, Library};
    use crate::core::deletion::REPLACE_CONSENT_SECS;
    use crate::import_queue::test_support::{imported, StubImporter};
    use soulbeet::{FailKind, ForgetOutcome, Relocation};

    /// An earlier library item by "Marrow Choir" on "Winter Songs", as Report::apply hands it
    /// over: its path relative to the library folder.
    fn earlier(rel: &str, title: &str) -> ImportedItem {
        ImportedItem {
            path: rel.into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: title.into(),
            album: "Winter Songs".into(),
            beets_id: None,
            release_mbid: None,
            match_per_mille: None,
        }
    }

    fn removal(rel: &str, imported: Vec<(u64, PathBuf)>) -> WantedRemoval {
        WantedRemoval {
            existing: vec![earlier(rel, "O Holy Night")],
            imported,
        }
    }

    /// New item `id`, track `n`, where beets put it beside the earlier album.
    fn new_copy(id: u64, n: u32) -> (u64, PathBuf) {
        (
            id,
            format!("Marrow Choir/Winter Songs [1]/0{n}.flac").into(),
        )
    }

    /// An imported run whose items are the new copies of `removals`, as beets reports them.
    fn imported_with(removals: Vec<WantedRemoval>) -> ImportResult {
        let items = removals
            .iter()
            .flat_map(|r| &r.imported)
            .map(|(_, path)| ImportedItem {
                path: path.clone(),
                ..earlier("", "O Holy Night")
            })
            .collect();
        ImportResult {
            outcome: ImportOutcome::Imported {
                untagged: false,
                warnings: vec![],
                items,
                failure: None,
            },
            removals,
            ..imported()
        }
    }

    fn notes(result: &ImportResult) -> Vec<String> {
        match &result.outcome {
            ImportOutcome::Imported { warnings, .. } => warnings.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn item_paths(result: &ImportResult) -> Vec<PathBuf> {
        match &result.outcome {
            ImportOutcome::Imported { items, .. } => items.iter().map(|i| i.path.clone()).collect(),
            other => panic!("{other:?}"),
        }
    }

    async fn settle(
        lib: &Library,
        stub: &StubImporter,
        exempt_jobs: &[String],
        target: &Path,
        result: ImportResult,
    ) -> ImportResult {
        settle_removals(
            &lib.guard(),
            stub,
            &lib.alice(),
            Removals::Guarded(&Origin::Ui),
            exempt_jobs,
            target,
            result,
        )
        .await
        .result
    }

    #[tokio::test]
    async fn a_copy_the_guard_allows_is_removed_and_the_new_one_moved_into_its_place() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        stub.script_relocate(Relocation {
            now: vec![(7, "Marrow Choir/Winter Songs/03.flac".into())],
            failure: None,
        });
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec!["replaced the library's earlier copy A/x.flac".to_string()]
        );
        assert!(!target.join("A/x.flac").exists());
        assert_eq!(stub.relocations(), vec![(target.clone(), vec![7])]);
        assert_eq!(
            item_paths(&done),
            vec![PathBuf::from("Marrow Choir/Winter Songs/03.flac")]
        );
        let reasons: Vec<(String, String)> =
            sqlx::query_as("SELECT origin, reason FROM deletion_reviews")
                .fetch_all(&lib.db)
                .await
                .unwrap();
        assert_eq!(reasons, vec![("ui".into(), "replaced".into())]);
    }

    #[tokio::test]
    async fn replacement_duration_mismatch_keeps_existing_file_and_records_failure() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        // Write a real 200s WAV as the existing copy and a 120s WAV as the newly imported copy
        // (diff = 80s > max(15, 20) = 20s -> refused!).
        let wav_old = soulbeet::verify::encode_test_wav_pcm16(8_000, &vec![0.1; 8_000 * 200]);
        let wav_new = soulbeet::verify::encode_test_wav_pcm16(8_000, &vec![0.1; 8_000 * 120]);
        lib.song("s2", "alice/A/real.flac", &wav_old);
        lib.fake.rates("alice", "s2", 1);
        let new_rel = new_copy(7, 3).1;
        let new_abs = target.join(&new_rel);
        std::fs::create_dir_all(new_abs.parent().unwrap()).unwrap();
        std::fs::write(&new_abs, &wav_new).unwrap();

        let result = imported_with(vec![removal("A/real.flac", vec![new_copy(7, 3)])]);
        let settled = settle_removals(
            &lib.guard(),
            &stub,
            &lib.alice(),
            Removals::Guarded(&Origin::Ui),
            &[],
            &target,
            result,
        )
        .await;

        assert!(
            target.join("A/real.flac").exists(),
            "existing copy must be kept"
        );
        assert!(stub.forgets().is_empty() && stub.relocations().is_empty());
        assert_eq!(settled.replaced.len(), 1);
        match &settled.replaced[0].outcome {
            ReplacementOutcome::Kept { failure } => {
                assert_eq!(failure.code, FailureCode::ReplacementDurationMismatch);
                assert_eq!(failure.stage, Stage::Delete);
            }
            other => panic!("expected Kept with ReplacementDurationMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replacement_duration_within_tolerance_allows_replacement() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        // 200s existing vs 208s new (diff = 8s <= max(15, 20) = 20s -> allowed!).
        let wav_old = soulbeet::verify::encode_test_wav_pcm16(8_000, &vec![0.1; 8_000 * 200]);
        let wav_new = soulbeet::verify::encode_test_wav_pcm16(8_000, &vec![0.1; 8_000 * 208]);
        lib.song("s2", "alice/A/real.flac", &wav_old);
        lib.fake.rates("alice", "s2", 1);
        let new_rel = new_copy(7, 3).1;
        let new_abs = target.join(&new_rel);
        std::fs::create_dir_all(new_abs.parent().unwrap()).unwrap();
        std::fs::write(&new_abs, &wav_new).unwrap();

        let result = imported_with(vec![removal("A/real.flac", vec![new_copy(7, 3)])]);
        let settled = settle_removals(
            &lib.guard(),
            &stub,
            &lib.alice(),
            Removals::Guarded(&Origin::Ui),
            &[],
            &target,
            result,
        )
        .await;

        assert!(
            !target.join("A/real.flac").exists(),
            "existing copy should be replaced"
        );
        assert_eq!(settled.replaced.len(), 1);
        assert!(matches!(
            settled.replaced[0].outcome,
            ReplacementOutcome::Replaced
        ));
    }

    #[tokio::test]
    async fn a_copy_another_listener_rated_stays_and_the_row_says_why() {
        let lib = Library::new().await;
        lib.fake.rates("bob", "s1", 5);
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec!["kept the library's earlier copy A/x.flac: Another listener rated, starred or kept this track in a playlist".to_string()]
        );
        assert!(target.join("A/x.flac").exists());
        assert!(stub.relocations().is_empty() && stub.forgets().is_empty());
    }

    #[tokio::test]
    async fn nothing_is_removed_for_a_task_beets_did_not_import() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert!(notes(&done).is_empty());
        assert!(target.join("A/x.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_run_that_did_not_import_checks_and_removes_nothing() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let failed = ImportResult {
            removals: vec![removal("A/x.flac", vec![new_copy(7, 3)])],
            ..ImportResult::failed(FailKind::Beets, "beets exited with code 1".into())
        };
        let done = settle(&lib, &stub, &[], &target, failed.clone()).await;
        assert_eq!(
            done,
            ImportResult {
                removals: vec![],
                ..failed
            }
        );
        assert!(target.join("A/x.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
        assert!(stub.forgets().is_empty() && stub.relocations().is_empty());
    }

    #[tokio::test]
    async fn an_album_copy_is_replaced_only_when_every_track_may_go() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.stars("bob", "s2");
        let mut both = removal("A/x.flac", vec![new_copy(7, 3), new_copy(8, 4)]);
        both.existing.push(earlier("A/y.flac", "Silent Night"));
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let done = settle(&lib, &stub, &[], &target, imported_with(vec![both])).await;
        assert_eq!(
            notes(&done),
            vec!["kept the library's earlier copy A/x.flac, A/y.flac: Another listener rated, starred or kept this track in a playlist".to_string()]
        );
        assert!(target.join("A/x.flac").exists() && target.join("A/y.flac").exists());
        assert!(stub.forgets().is_empty() && stub.relocations().is_empty());
    }

    #[tokio::test]
    async fn a_deletion_that_fails_part_way_keeps_the_rest_and_says_which_went() {
        use std::os::unix::fs::PermissionsExt;
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        // Rule 4 keeps a file nobody rated while Navidrome sends no average for it.
        lib.fake.rates("alice", "s2", 1);
        let target = lib.root.join("music/alice");
        let x = std::fs::canonicalize(target.join("A/x.flac")).unwrap();
        let y = std::fs::canonicalize(target.join("B/y.flac")).unwrap();
        let mut both = removal("A/x.flac", vec![new_copy(7, 3), new_copy(8, 4)]);
        both.existing.push(earlier("B/y.flac", "Silent Night"));
        // Both pass the checks (no write is needed to stat a file); only the second unlink is refused.
        std::fs::set_permissions(target.join("B"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let stub = StubImporter::new();
        let done = settle(&lib, &stub, &[], &target, imported_with(vec![both])).await;
        std::fs::set_permissions(target.join("B"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            notes(&done),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                format!(
                    "kept the library's earlier copy B/y.flac: Could not delete the file: {}",
                    std::io::Error::from_raw_os_error(13) // EACCES
                ),
            ]
        );
        assert!(!x.exists() && y.exists());
        assert_eq!(stub.forgets(), vec![(target.clone(), x)]);
        assert!(stub.relocations().is_empty());
    }

    #[tokio::test]
    async fn a_forget_that_leaves_beets_unsure_leaves_the_new_copy_where_beets_put_it() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        stub.script_forget(ForgetOutcome::NotInLibrary);
        let target = lib.root.join("music/alice");
        let x = std::fs::canonicalize(target.join("A/x.flac")).unwrap();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec![
                "replaced the library's earlier copy A/x.flac".to_string(),
                format!(
                    "{} was not in the library of {}",
                    x.display(),
                    target.display()
                ),
            ]
        );
        assert!(stub.relocations().is_empty());
        assert_eq!(item_paths(&done), vec![new_copy(7, 3).1]);
    }

    fn failure_of(result: &ImportResult) -> Option<Failure> {
        match &result.outcome {
            ImportOutcome::Imported { failure, .. } => failure.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_new_copy_that_cannot_be_moved_says_so_and_names_the_items_where_they_are() {
        // As move_items words a move beets failed part way, and one out of the library.
        let failed = |retry, message: &str| {
            Failure::new(
                shared::failure::FailureCode::ImportBeetsFailed,
                shared::failure::Stage::Import,
                retry,
                message,
            )
        };
        for (now, failure) in [
            (
                new_copy(7, 3).1,
                failed(
                    shared::failure::RetryAdvice::SameSourceLater,
                    "Could not move the new copy into place: beet move failed (exit status: 1): error",
                ),
            ),
            (
                PathBuf::from("/elsewhere/03.flac"),
                failed(
                    shared::failure::RetryAdvice::NotRetryable,
                    "beets moved item 7 to /elsewhere/03.flac, which is not inside /music/alice",
                ),
            ),
        ] {
            let lib = Library::new().await;
            let stub = StubImporter::new();
            stub.script_relocate(Relocation {
                now: vec![(7, now.clone())],
                failure: Some(failure.clone()),
            });
            let target = lib.root.join("music/alice");
            let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
            let done = settle(&lib, &stub, &[], &target, result).await;
            assert_eq!(
                notes(&done),
                vec![
                    "replaced the library's earlier copy A/x.flac".to_string(),
                    failure.message.clone(),
                ]
            );
            assert_eq!(item_paths(&done), vec![now]);
            assert_eq!(failure_of(&done), Some(failure));
        }
    }

    #[tokio::test]
    async fn an_item_whose_path_beets_cannot_be_read_for_keeps_its_path_and_says_it_may_have_moved()
    {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let unread = Failure::new(
            shared::failure::FailureCode::ImportBeetsFailed,
            shared::failure::Stage::Import,
            shared::failure::RetryAdvice::SameSourceLater,
            "beets may have moved the new copy, and Soulbeet could not read where item 7 is: beet export failed (exit status: 1): ",
        );
        stub.script_relocate(Relocation {
            now: vec![],
            failure: Some(unread.clone()),
        });
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(notes(&done)[1], unread.message);
        assert_eq!(item_paths(&done), vec![new_copy(7, 3).1]);
        assert_eq!(failure_of(&done), Some(unread));
    }

    #[test]
    fn a_removal_that_stopped_says_which_file_may_be_gone_without_calling_it_kept() {
        let target = Path::new("/music/alice");
        let earlier: Vec<PathBuf> = ["A/x.flac", "A/y.flac", "A/z.flac"]
            .iter()
            .map(|rel| target.join(rel))
            .collect();
        // As lost_removal words them for /music/alice/A/y.flac.
        let failed = |message: &str| {
            shared::failure::Failure::new(
                shared::failure::FailureCode::DeleteFailed,
                shared::failure::Stage::Delete,
                shared::failure::RetryAdvice::SameSourceLater,
                message,
            )
            .with_cause(shared::failure::Service::Soulful, None, "task 7 panicked")
        };
        let unconfirmed =
            failed("The file may have been deleted: the removal stopped before it reported");
        let may_be_gone = "may have removed the library's earlier copy A/y.flac: its removal stopped before it reported";
        assert_eq!(
            stopped_notes(
                &earlier,
                1,
                target,
                &NotRemoved::Unconfirmed(unconfirmed.clone())
            ),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                may_be_gone.to_string(),
                "kept the library's earlier copy A/z.flac: an earlier removal of this import stopped before it reported".to_string(),
            ]
        );
        assert_eq!(
            stopped_notes(
                &earlier[..2],
                1,
                target,
                &NotRemoved::Unconfirmed(unconfirmed)
            ),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                may_be_gone.to_string(),
            ]
        );
        // A stopped removal that left its file keeps it, as a refusal does.
        let kept = "Could not delete the file";
        assert_eq!(
            stopped_notes(&earlier, 1, target, &NotRemoved::Kept(failed(kept))),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                format!("kept the library's earlier copy A/y.flac, A/z.flac: {kept}"),
            ]
        );
    }

    #[test]
    fn each_new_item_takes_the_path_its_own_id_was_moved_to() {
        let mut items = vec![
            ImportedItem {
                path: new_copy(7, 3).1,
                ..earlier("", "O Holy Night")
            },
            ImportedItem {
                path: new_copy(8, 4).1,
                ..earlier("", "Silent Night")
            },
        ];
        // Item 7 moves to the path item 8 had, which moves on.
        let moved = vec![
            (7, new_copy(8, 4).1),
            (8, "Marrow Choir/Winter Songs/04.flac".into()),
        ];
        renamed(&mut items, &[new_copy(7, 3), new_copy(8, 4)], &moved);
        assert_eq!(
            items.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            vec![
                new_copy(8, 4).1,
                PathBuf::from("Marrow Choir/Winter Songs/04.flac")
            ]
        );
    }

    #[tokio::test]
    async fn a_removal_that_names_no_earlier_item_is_skipped() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let none = WantedRemoval {
            existing: vec![],
            imported: vec![new_copy(7, 3)],
        };
        let done = settle(&lib, &stub, &[], &target, imported_with(vec![none])).await;
        assert!(notes(&done).is_empty());
        assert!(stub.relocations().is_empty() && stub.forgets().is_empty());
        assert_eq!(item_paths(&done), vec![new_copy(7, 3).1]);
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_later_removal_of_the_run_never_touches_what_an_earlier_one_replaced_or_added() {
        // Two beets tasks of one album download (the main tracks and a Bonus folder with the
        // same tags) both want the same earlier item removed; removal 1 moves its new copy to
        // the earlier item's path.
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        stub.script_relocate(Relocation {
            now: vec![(7, "A/x.flac".into())],
            failure: None,
        });
        let result = imported_with(vec![
            removal("A/x.flac", vec![new_copy(7, 3)]),
            removal("A/x.flac", vec![new_copy(8, 4)]),
        ]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec![
                "replaced the library's earlier copy A/x.flac".to_string(),
                format!("kept the library's earlier copy A/x.flac: {OVERLAPS}"),
            ]
        );
        assert_eq!(stub.relocations(), vec![(target.clone(), vec![7])]);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            1
        );
        // A removal naming a file the run itself imported is refused before any check.
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let mut own = removal("A/x.flac", vec![new_copy(8, 4)]);
        own.existing
            .push(earlier(new_copy(7, 3).1.to_str().unwrap(), "O Holy Night"));
        let result = imported_with(vec![removal("B/z.flac", vec![new_copy(7, 3)]), own]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done)[1],
            format!(
                "kept the library's earlier copy A/x.flac, {}: {OVERLAPS}",
                new_copy(7, 3).1.display()
            )
        );
        assert!(target.join("A/x.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn the_job_doing_the_replacement_does_not_keep_its_own_target() {
        let lib = Library::new().await;
        lib.job(
            "j1",
            "alice-music",
            "importing",
            wanted("Marrow Choir", "O Holy Night", None),
        )
        .await;
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        // Without the exemption the same job keeps the earlier copy.
        let kept = settle(&lib, &stub, &[], &target, result.clone()).await;
        assert_eq!(
            notes(&kept),
            vec!["kept the library's earlier copy A/x.flac: A download or import that is not finished wants this track".to_string()]
        );
        assert!(target.join("A/x.flac").exists());
        let done = settle(&lib, &stub, &["j1".to_string()], &target, result).await;
        assert_eq!(
            notes(&done),
            vec!["replaced the library's earlier copy A/x.flac".to_string()]
        );
    }

    /// The files of `songs` as a preview would store them, confirmed by g1 until `expires_at`.
    async fn replace_set(lib: &Library, songs: &[&str], expires_at: i64) -> ReplaceSet {
        let mut files = vec![];
        for id in songs {
            let checked = library_verdict(lib.guard().check_song(&lib.alice(), id).await);
            files.push(checked.previewed());
        }
        ReplaceSet {
            grant_id: "g1".into(),
            files,
            expires_at,
        }
    }

    fn later() -> i64 {
        chrono::Utc::now().timestamp() + REPLACE_CONSENT_SECS
    }

    async fn replace(
        lib: &Library,
        stub: &StubImporter,
        set: &ReplaceSet,
        now: i64,
        result: ImportResult,
    ) -> Settled {
        let target = lib.root.join("music/alice");
        settle_removals(
            &lib.guard(),
            stub,
            &lib.alice(),
            Removals::Previewed { set, now },
            &[],
            &target,
            result,
        )
        .await
    }

    fn outcomes(s: &Settled) -> Vec<&ReplacementOutcome> {
        s.replaced.iter().map(|r| &r.outcome).collect()
    }

    #[tokio::test]
    async fn a_previewed_copy_is_replaced_for_the_app_that_confirmed_it() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let set = replace_set(&lib, &["s1"], later()).await;
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let s = replace(&lib, &stub, &set, now, result).await;
        assert_eq!(
            notes(&s.result),
            vec!["replaced the library's earlier copy A/x.flac".to_string()]
        );
        assert_eq!(
            (outcomes(&s), s.not_replaced.len()),
            (vec![&ReplacementOutcome::Replaced], 0)
        );
        assert_eq!(
            s.replaced[0].existing,
            vec![earlier("A/x.flac", "O Holy Night")]
        );
        assert!(!lib.root.join("music/alice/A/x.flac").exists());
        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT origin, reason, client FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        assert_eq!(
            row,
            ("mcp".into(), "replaced".into(), Some("claude.ai".into()))
        );
    }

    #[tokio::test]
    async fn a_copy_outside_the_confirmed_preview_stays() {
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.app("g1", "alice", true).await;
        let only_s2 = replace_set(&lib, &["s2"], later()).await;
        let rated = lib.fake.ratings_set();
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let s = replace(&lib, &stub, &only_s2, now, result).await;
        // Kept before the guard's checks: rule 4 re-sent no rating for a copy never previewed.
        assert_eq!(lib.fake.ratings_set(), rated);
        assert_eq!(
            notes(&s.result),
            vec!["kept the library's earlier copy A/x.flac: This file was not in the preview this deletion was confirmed with".to_string()]
        );
        assert_eq!(
            s.replaced[0].outcome,
            ReplacementOutcome::Kept {
                failure: not_previewed()
            }
        );
        assert_eq!(s.not_replaced, vec!["s2".to_string()]);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert!(lib.root.join("music/alice/B/y.flac").exists());
        assert!(stub.relocations().is_empty() && stub.forgets().is_empty());
    }

    #[tokio::test]
    async fn a_removal_holding_an_unpreviewed_copy_keeps_every_copy_it_names() {
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.app("g1", "alice", true).await;
        let only_s1 = replace_set(&lib, &["s1"], later()).await;
        let mut both = removal("A/x.flac", vec![new_copy(7, 3), new_copy(8, 4)]);
        both.existing.push(earlier("B/y.flac", "Silent Night"));
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let s = replace(&lib, &stub, &only_s1, now, imported_with(vec![both])).await;
        assert_eq!(
            s.replaced[0].outcome,
            ReplacementOutcome::Kept {
                failure: not_previewed()
            }
        );
        assert_eq!(s.not_replaced, vec!["s1".to_string()]);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert!(lib.root.join("music/alice/B/y.flac").exists());
    }

    #[tokio::test]
    async fn a_previewed_copy_beets_saw_no_duplicate_of_is_reported_not_replaced() {
        // beets compares artist and title (duplicate_keys, beets/config_default.yaml:50-52): a
        // cover with another title is never reported, so its previewed copy has no removal.
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.app("g1", "alice", true).await;
        let set = replace_set(&lib, &["s1", "s2"], later()).await;
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let s = replace(&lib, &stub, &set, now, result).await;
        assert_eq!(outcomes(&s), vec![&ReplacementOutcome::Replaced]);
        assert_eq!(s.not_replaced, vec!["s2".to_string()]);
        assert!(lib.root.join("music/alice/B/y.flac").exists());
        // A run with no removal at all replaces nothing and lists every previewed song.
        let s = replace(&lib, &stub, &set, now, imported_with(vec![])).await;
        assert!(s.replaced.is_empty());
        assert_eq!(s.not_replaced, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[tokio::test]
    async fn a_replacement_whose_app_lost_the_switch_keeps_the_copy() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let set = replace_set(&lib, &["s1"], later()).await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = 0")
            .execute(&lib.db)
            .await
            .unwrap();
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let s = replace(&lib, &stub, &set, now, result).await;
        assert_eq!(
            (s.replaced[0].outcome.clone(), s.not_replaced),
            (
                ReplacementOutcome::Kept {
                    failure: permission_not_granted()
                },
                vec!["s1".to_string()]
            )
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_replacement_imported_after_its_consent_ran_out_keeps_the_copy() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let now = 1_800_000_000;
        let set = replace_set(&lib, &["s1"], now).await;
        let rated = lib.fake.ratings_set();
        let stub = StubImporter::new();
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let s = replace(&lib, &stub, &set, now, result).await;
        let ran_out = replace_consent_expired(chrono::DateTime::from_timestamp(now, 0).unwrap());
        assert_eq!(
            s.replaced[0].outcome,
            ReplacementOutcome::Kept {
                failure: ran_out.clone()
            }
        );
        assert_eq!(
            ran_out.message,
            "The confirmation to replace this copy ran out at 2027-01-15 08:00:00 UTC; preview it again with reason replaced"
        );
        assert_eq!(
            notes(&s.result),
            vec![format!(
                "kept the library's earlier copy A/x.flac: {}",
                ran_out.message
            )]
        );
        assert_eq!(lib.fake.ratings_set(), rated, "nothing was checked");
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_replacement_that_fails_part_way_says_what_went() {
        use std::os::unix::fs::PermissionsExt;
        let lib = Library::new().await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.app("g1", "alice", true).await;
        let set = replace_set(&lib, &["s1", "s2"], later()).await;
        let target = lib.root.join("music/alice");
        let x = std::fs::canonicalize(target.join("A/x.flac")).unwrap();
        let mut both = removal("A/x.flac", vec![new_copy(7, 3), new_copy(8, 4)]);
        both.existing.push(earlier("B/y.flac", "Silent Night"));
        // Both pass the checks; only the second unlink is refused (step 6's part way test).
        std::fs::set_permissions(target.join("B"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let stub = StubImporter::new();
        let now = chrono::Utc::now().timestamp();
        let s = replace(&lib, &stub, &set, now, imported_with(vec![both])).await;
        std::fs::set_permissions(target.join("B"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(&s.replaced[0].outcome, ReplacementOutcome::PartlyRemoved { removed, failure }
                if *removed == vec![PathBuf::from("A/x.flac")] && failure.code == FailureCode::DeleteFailed),
            "{:?}",
            s.replaced
        );
        assert_eq!(s.not_replaced, vec!["s2".to_string()]);
        assert!(!x.exists() && target.join("B/y.flac").exists());
    }
}
