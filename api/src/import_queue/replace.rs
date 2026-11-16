//! The library's earlier copy of an import, replaced only through the delete guard (D2).

use crate::auth::UserId;
use crate::core::delete_guard::{ForgetVia, Guard, NotRemoved, Origin};
use crate::core::logged;
use shared::failure::Failure;
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
    origin: Origin,
    exempt_jobs: &[String],
    target: &Path,
    mut result: ImportResult,
) -> ImportResult {
    let removals = std::mem::take(&mut result.removals);
    let ImportOutcome::Imported {
        warnings,
        items,
        failure,
        ..
    } = &mut result.outcome
    else {
        return result;
    };
    let mut run = Run {
        replace: Replacement {
            guard,
            importer,
            actor,
            origin,
            exempt_jobs,
            target,
        },
        taken: items
            .iter()
            .map(|item| item.path.clone())
            .chain(
                removals
                    .iter()
                    .flat_map(|r| r.imported.iter().map(|(_, p)| p.clone())),
            )
            .collect(),
        unmoved: None,
    };
    for removal in &removals {
        warnings.extend(run.settle(removal, items).await);
    }
    if failure.is_none() {
        *failure = run.unmoved;
    }
    result
}

struct Replacement<'a> {
    guard: &'a Guard<'a>,
    importer: &'a dyn MusicImporter,
    actor: &'a UserId,
    origin: Origin,
    exempt_jobs: &'a [String],
    target: &'a Path,
}

/// One run's removals, settled in turn.
struct Run<'a> {
    replace: Replacement<'a>,
    /// Paths, relative to the target as beets names items, that a removal of this run must not
    /// touch: every file the run imported, and each file an earlier removal deleted or moved a
    /// new file to.
    taken: HashSet<PathBuf>,
    /// The failure of the first new copy that could not be moved into place.
    unmoved: Option<Failure>,
}

impl Run<'_> {
    /// The notes of one removal. One whose task imported nothing is skipped: its new items never
    /// reached the library (D2). One that names no earlier item is skipped too: there is
    /// nothing it may replace. One that names a path `taken` holds is refused (OVERLAPS). Every
    /// earlier item is checked before any is deleted; the new items are moved into place only
    /// once all are deleted and forgotten cleanly, and `items` then names them where the
    /// importer has them after the move, failed or not.
    async fn settle(&mut self, removal: &WantedRemoval, items: &mut [ImportedItem]) -> Vec<String> {
        if removal.imported.is_empty() || removal.existing.is_empty() {
            return vec![];
        }
        let replace = &self.replace;
        let target = replace.target;
        let earlier: Vec<PathBuf> = removal
            .existing
            .iter()
            .map(|item| target.join(&item.path))
            .collect();
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
            return vec![duplicate_note(
                &earlier,
                target,
                DuplicateNote::Blocked(OVERLAPS),
            )];
        }
        let mut checked = Vec::with_capacity(removal.existing.len());
        for (item, file) in removal.existing.iter().zip(&earlier) {
            match replace
                .guard
                .check_file(replace.actor, item, target, replace.exempt_jobs)
                .await
            {
                Ok(c) => checked.push(c),
                Err(f) => {
                    warn!(
                        "Kept the earlier copy {} of an import into {}: {}",
                        file.display(),
                        target.display(),
                        logged(&f)
                    );
                    return vec![duplicate_note(
                        &earlier,
                        target,
                        DuplicateNote::Blocked(&f.message),
                    )];
                }
            }
        }
        let mut warnings = Vec::new();
        for (deleted, c) in checked.into_iter().enumerate() {
            let forget = ForgetVia::Worker {
                importer: replace.importer,
                library: target,
            };
            let outcome = replace
                .guard
                .carry_out(
                    replace.actor,
                    c,
                    DeletionReason::Replaced,
                    replace.origin,
                    forget,
                )
                .await;
            match outcome {
                Ok(done) => {
                    self.taken.insert(removal.existing[deleted].path.clone());
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
                    return notes;
                }
            }
        }
        let mut notes = vec![duplicate_note(&earlier, target, DuplicateNote::Replaced)];
        // A forget that did not drop exactly one item leaves beets' database unsure, so the new
        // items stay where beets put them.
        if warnings.is_empty() {
            let ids: Vec<u64> = removal.imported.iter().map(|(id, _)| *id).collect();
            let moved = replace.importer.relocate(target, &ids).await;
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
        notes
    }
}

/// The notes of a removal whose deletions stopped at `earlier[deleted]`: the files deleted before
/// it, removed; that file and the rest, kept with its reason when `stop` kept it; when its
/// removal is unconfirmed, its own message, which says it may have been deleted, and the rest
/// kept with that reason.
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
        NotRemoved::Kept(f) => (&earlier[deleted..], &f.message),
        NotRemoved::Unconfirmed(f) => {
            notes.push(f.message.clone());
            (&earlier[deleted + 1..], &f.message)
        }
    };
    if !kept.is_empty() {
        notes.push(duplicate_note(kept, target, DuplicateNote::Blocked(reason)));
    }
    notes
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
    use crate::core::delete_guard::test_support::{wanted, Library};
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
            Origin::Ui,
            exempt_jobs,
            target,
            result,
        )
        .await
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
    async fn a_copy_another_listener_rated_stays_and_the_row_says_why() {
        let lib = Library::new().await;
        lib.fake.rates("bob", "s1", 5);
        let stub = StubImporter::new();
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec!["kept the library's earlier copy A/x.flac: Another listener rated or starred this track".to_string()]
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
            vec!["kept the library's earlier copy A/x.flac, A/y.flac: Another listener rated or starred this track".to_string()]
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
                    "kept the library's earlier copy B/y.flac: Could not delete {}: Permission denied (os error 13)",
                    y.display()
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
            "beets may have moved the new copy, and Soulful could not read where item 7 is: beet export failed (exit status: 1): ",
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
        };
        let unconfirmed = "/music/alice/A/y.flac may have been deleted: the removal stopped before it reported (task 7 panicked)";
        assert_eq!(
            stopped_notes(
                &earlier,
                1,
                target,
                &NotRemoved::Unconfirmed(failed(unconfirmed))
            ),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                unconfirmed.to_string(),
                format!("kept the library's earlier copy A/z.flac: {unconfirmed}"),
            ]
        );
        assert_eq!(
            stopped_notes(
                &earlier[..2],
                1,
                target,
                &NotRemoved::Unconfirmed(failed(unconfirmed))
            ),
            vec![
                "removed the library's earlier copy A/x.flac".to_string(),
                unconfirmed.to_string(),
            ]
        );
        // A stopped removal that left its file keeps it, as a refusal does.
        let kept = "Could not delete /music/alice/A/y.flac: task 7 panicked";
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
}
