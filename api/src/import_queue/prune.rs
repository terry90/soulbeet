//! Drops, once at each start, the items whose file is gone from every beets library Soulbeet
//! imports into: those a removed, expired or promoted discovery track left before removing it
//! dropped its item too. Rows only, never a file (prune.py).

use super::{ImportQueue, IMPORT_QUEUE};
use crate::models::folder::Folder;
use shared::recommendation::DiscoveryProfile;
use soulbeet::PruneOutcome;
use sqlx::SqlitePool;
use std::collections::BTreeSet;
use std::sync::Once;
use tracing::{debug, info, warn};

static STARTED: Once = Once::new();

/// Prunes every library in the background, once per process. Never awaited: the serve callback
/// runs before the server accepts connections.
pub fn start_library_prune() {
    STARTED.call_once(|| {
        tokio::spawn(prune_libraries_in(&crate::db::DB, &IMPORT_QUEUE));
    });
}

/// Prunes through `queue`, one after the other, the library of each folder and of each of its
/// Discovery profile folders, each once the work queued on it has run. A folder that is not
/// there has nothing pruned, and no folder is created.
async fn prune_libraries_in(db: &SqlitePool, queue: &ImportQueue) {
    let folders = match Folder::get_all_with_owners_in(db).await {
        Ok(folders) => folders,
        Err(e) => {
            warn!("Could not read the folders, so no beets library is pruned: {e}");
            return;
        }
    };
    let mut libraries = BTreeSet::new();
    for owned in &folders {
        let folder = &owned.folder;
        let profiles = DiscoveryProfile::NAMES.map(|name| folder.discovery_profile_path(name));
        for path in std::iter::once(folder.path.clone()).chain(profiles) {
            match tokio::fs::canonicalize(&path).await {
                Ok(library) => {
                    libraries.insert(library);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!("No folder at {path}, so no beets library to prune there")
                }
                Err(e) => {
                    warn!("Could not resolve {path}, so its beets library is not pruned: {e}")
                }
            }
        }
    }
    for library in libraries {
        let library_text = library.display();
        match queue.prune(&library).await {
            PruneOutcome::NoLibrary | PruneOutcome::Pruned { removed: 0, .. } => {}
            PruneOutcome::Pruned { removed, kept } => info!(
                "Dropped {removed} items whose file is gone from the beets library of \
                 {library_text}, which keeps {kept}"
            ),
            PruneOutcome::Unproven { items } => info!(
                "Left the beets library of {library_text} as it is: some of its {items} items' \
                 files are gone and none is there, as when its folder is not mounted"
            ),
            PruneOutcome::Failed(e) => {
                warn!("Could not prune the beets library of {library_text}: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::import_queue::test_support::stub_queue;

    #[tokio::test]
    async fn each_folder_and_discovery_profile_there_is_pruned_once_and_none_is_made() {
        let lib = Library::new().await;
        let balanced = lib.root.join("music/alice/Discovery/Balanced");
        std::fs::create_dir_all(&balanced).unwrap();
        // carol's folder does not exist on disk, and alice's carries no other profile folder.
        lib.folder("carol", "carol-gone", "music/carol", false)
            .await;
        let (queue, stub) = stub_queue();

        prune_libraries_in(&lib.db, &queue).await;

        let canonical = |rel: &str| std::fs::canonicalize(lib.root.join(rel)).unwrap();
        let mut expected = vec![
            canonical("music/alice"),
            canonical("music/alice/Discovery/Balanced"),
            canonical("music/bob"),
        ];
        expected.sort();
        assert_eq!(stub.prunes(), expected);
        assert!(!lib.root.join("music/carol").exists());
        assert!(!lib.root.join("music/alice/Discovery/Conservative").exists());
        assert!(!lib.root.join("music/bob/Discovery").exists());
    }
}
