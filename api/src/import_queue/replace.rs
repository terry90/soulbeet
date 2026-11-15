//! The library's earlier copy of an import, replaced only through the delete guard (D2).

use crate::auth::UserId;
use crate::core::delete_guard::{ForgetVia, Guard, Origin};
use crate::core::logged;
use shared::navidrome::DeletionReason;
use soulbeet::{
    duplicate_note, DuplicateNote, ImportOutcome, ImportResult, ImportedItem, MusicImporter,
    WantedRemoval,
};
use std::path::{Path, PathBuf};
use tracing::warn;

/// Settles each removal beets wanted in `result`, an import into `target` (the canonical
/// library folder beets ran on), and appends what became of each earlier copy to the outcome's
/// warnings. Only an Imported outcome is settled: any other is returned unchanged, so a run the
/// row reports as failed deletes nothing.
pub(super) async fn settle_removals(
    guard: &Guard<'_>,
    importer: &dyn MusicImporter,
    actor: &UserId,
    origin: Origin,
    exempt_jobs: &[String],
    target: &Path,
    mut result: ImportResult,
) -> ImportResult {
    let ImportOutcome::Imported {
        warnings, items, ..
    } = &mut result.outcome
    else {
        return result;
    };
    let replace = Replacement {
        guard,
        importer,
        actor,
        origin,
        exempt_jobs,
        target,
    };
    for removal in &result.removals {
        warnings.extend(replace.settle(removal, items).await);
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

impl Replacement<'_> {
    /// The notes of one removal. One whose task imported nothing is skipped: its new items never
    /// reached the library (D2). Every earlier item is checked before any is deleted; the new
    /// items are moved into place only once all are deleted and forgotten cleanly, and `items`
    /// then names them where they are.
    async fn settle(&self, removal: &WantedRemoval, items: &mut [ImportedItem]) -> Vec<String> {
        if removal.imported.is_empty() {
            return vec![];
        }
        let target = self.target;
        let earlier: Vec<PathBuf> = removal
            .existing
            .iter()
            .map(|item| target.join(&item.path))
            .collect();
        let mut checked = Vec::with_capacity(removal.existing.len());
        for (item, file) in removal.existing.iter().zip(&earlier) {
            match self
                .guard
                .check_file(self.actor, item, target, self.exempt_jobs)
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
                importer: self.importer,
                library: target,
            };
            match self
                .guard
                .carry_out(self.actor, c, DeletionReason::Replaced, self.origin, forget)
                .await
            {
                Ok(done) => warnings.extend(done),
                Err(f) => {
                    warn!(
                        "Kept the earlier copy {} of an import into {}: {}",
                        earlier[deleted].display(),
                        target.display(),
                        logged(&f)
                    );
                    let mut notes = Vec::new();
                    if deleted > 0 {
                        notes.push(duplicate_note(
                            &earlier[..deleted],
                            target,
                            DuplicateNote::Removed,
                        ));
                    }
                    notes.push(duplicate_note(
                        &earlier[deleted..],
                        target,
                        DuplicateNote::Blocked(&f.message),
                    ));
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
            match self.importer.relocate(target, &ids).await {
                Ok(moved) => renamed(items, &removal.imported, &moved, target),
                Err(e) => notes.push(format!("Could not move the new copy into place: {e}")),
            }
        }
        notes.extend(warnings);
        notes
    }
}

/// Gives each of `items` that is an item of `imported` the path `moved` gives its id, relative
/// to `target` as beets reports items. Each item is matched by the path it had before the move.
fn renamed(
    items: &mut [ImportedItem],
    imported: &[(u64, PathBuf)],
    moved: &[(u64, PathBuf)],
    target: &Path,
) {
    for item in items.iter_mut() {
        let Some((id, _)) = imported.iter().find(|(_, was)| *was == item.path) else {
            continue;
        };
        if let Some((_, now)) = moved.iter().find(|(moved_id, _)| moved_id == id) {
            item.path = now.strip_prefix(target).unwrap_or(now).to_path_buf();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::{wanted, Library};
    use crate::import_queue::test_support::{imported, StubImporter};
    use soulbeet::{FailKind, ForgetOutcome};

    /// An earlier library item by "Marrow Choir" on "Winter Songs", as Report::apply hands it
    /// over: its path relative to the library folder.
    fn earlier(rel: &str, title: &str) -> ImportedItem {
        ImportedItem {
            path: rel.into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: title.into(),
            album: "Winter Songs".into(),
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
        stub.script_relocate(Ok(vec![(
            7,
            target.join("Marrow Choir/Winter Songs/03.flac"),
        )]));
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
        assert_eq!(done, failed);
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

    #[tokio::test]
    async fn a_new_copy_that_cannot_be_moved_says_so() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        stub.script_relocate(Err("beets exited with code 1".into()));
        let target = lib.root.join("music/alice");
        let result = imported_with(vec![removal("A/x.flac", vec![new_copy(7, 3)])]);
        let done = settle(&lib, &stub, &[], &target, result).await;
        assert_eq!(
            notes(&done),
            vec![
                "replaced the library's earlier copy A/x.flac".to_string(),
                "Could not move the new copy into place: beets exited with code 1".to_string(),
            ]
        );
        assert_eq!(item_paths(&done), vec![new_copy(7, 3).1]);
    }

    #[test]
    fn each_new_item_takes_the_path_its_own_id_was_moved_to() {
        let target = Path::new("/music/alice");
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
            (7, target.join(new_copy(8, 4).1)),
            (8, target.join("Marrow Choir/Winter Songs/04.flac")),
        ];
        renamed(
            &mut items,
            &[new_copy(7, 3), new_copy(8, 4)],
            &moved,
            target,
        );
        assert_eq!(
            items.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            vec![
                new_copy(8, 4).1,
                PathBuf::from("Marrow Choir/Winter Songs/04.flac")
            ]
        );
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
        let done = settle(&lib, &stub, &["j1".to_string()], &target, result).await;
        assert_eq!(
            notes(&done),
            vec!["replaced the library's earlier copy A/x.flac".to_string()]
        );
    }
}
