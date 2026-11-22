//! Discovery's own cleanup of its staging folder (`Discovery/<profile>`): its current rules,
//! plus the path containment of rule 1 (design section 13).

use super::unlink::{unlink_if_same, FileId, UnlinkError};
use super::{
    changed_since_check, delete_failed, forget_detached, not_a_track, outside_library, prune,
    rules, NOT_A_FILE,
};
use crate::auth::UserId;
use crate::core::database_failure;
use crate::core::discovery::track_not_pending;
use crate::folder_policy::{canonical_folder_path, PolicyRows};
use crate::import_queue::ImportQueue;
use crate::models::discovery_history::DiscoveryHistoryRow;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::folder::Folder;
use shared::failure::{Failure, Service, Stage};
use shared::navidrome::{DiscoveryStatus, DiscoveryTrack};
use sqlx::SqlitePool;
use std::path::Path;
use tracing::{info, warn};

/// What a staging removal writes to the discovery history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedOutcome {
    /// The listener rated it down or pressed Remove.
    Removed,
    /// It stayed pending past its profile's lifetime.
    Expired,
}

impl StagedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            StagedOutcome::Removed => "removed",
            StagedOutcome::Expired => "expired",
        }
    }
}

/// Removes a pending discovery track's file: claims it (claim_removal_in), then
/// remove_claimed_in.
pub(crate) async fn remove_staged_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    track: &DiscoveryTrack,
    folder: &Folder,
    outcome: StagedOutcome,
) -> Result<(), Failure> {
    claim_removal_in(db, track).await?;
    remove_claimed_in(db, queue, track, folder, outcome).await
}

/// Claims `track` for its removal, Pending to Removing (D16): `track_not_pending` when another
/// call holds or ended it. A server that stops before the removal ends leaves it Removing, which
/// the next start puts back to Pending.
pub(crate) async fn claim_removal_in(
    db: &SqlitePool,
    track: &DiscoveryTrack,
) -> Result<(), Failure> {
    let claimed = DiscoveryTrackRow::try_set_removing_in(db, &track.id)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?;
    match claimed {
        true => Ok(()),
        false => Err(track_not_pending()),
    }
}

/// Removes the file of `track`, which claim_removal_in claimed, marks the track Removed and sets
/// the outcome of the folder owner's discovery history for its artist and title (a status or
/// history write that fails is logged and does not fail the removal: a track left Removing goes
/// back to Pending at the next start, and the expiry then marks it, its file being gone). The
/// track is Pending again when the removal fails. The stored path must canonicalize to a regular
/// file strictly inside the canonical `Discovery/<profile>` of `folder`, in no deeper folder of
/// another user; a file already gone is only marked. The file goes through unlink_if_same with
/// the identity read just before; empty parents are pruned up to the profile folder, and the
/// file's item is dropped from the profile folder's library through `queue` (forget_detached).
pub(crate) async fn remove_claimed_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    track: &DiscoveryTrack,
    folder: &Folder,
    outcome: StagedOutcome,
) -> Result<(), Failure> {
    let stored = Path::new(&track.path);
    let removed = match tokio::fs::canonicalize(stored).await {
        Ok(path) => remove_contained(db, queue, &path, track, folder).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(delete_failed(&e)),
    };
    if let Err(failure) = removed {
        if let Err(e) =
            DiscoveryTrackRow::update_status_in(db, &track.id, &DiscoveryStatus::Pending).await
        {
            warn!("Could not set '{}' back to pending: {e}", track.title);
        }
        return Err(failure);
    }
    if let Err(e) =
        DiscoveryTrackRow::update_status_in(db, &track.id, &DiscoveryStatus::Removed).await
    {
        warn!("Could not mark '{}' removed: {e}", track.title);
    }
    if let Err(e) = DiscoveryHistoryRow::update_outcome_in(
        db,
        &folder.user_id,
        &track.artist,
        &track.title,
        outcome.as_str(),
    )
    .await
    {
        warn!(
            "Failed to update history for removed track '{}': {e}",
            track.title
        );
    }
    info!(
        "Removed discovery track: {} - {} ({})",
        track.artist,
        track.title,
        outcome.as_str()
    );
    Ok(())
}

/// Unlinks the canonical file `path` of `track` when it is a regular file strictly inside the
/// track's canonical profile folder and rule 1 gives it to the folder's owner, then prunes and
/// queues the forget of its item in the profile folder's library.
async fn remove_contained(
    db: &SqlitePool,
    queue: &ImportQueue,
    path: &Path,
    track: &DiscoveryTrack,
    folder: &Folder,
) -> Result<(), Failure> {
    let outside = || {
        info!(
            "{} is not inside the folder Discovery/{} of {}",
            path.display(),
            track.profile,
            folder.path
        );
        outside_library("The file is not inside its discovery profile's folder")
    };
    let profile = canonical_folder_path(Path::new(&folder.discovery_profile_path(&track.profile)))
        .map_err(|e| outside().with_cause(Service::Filesystem, None, e.to_string()))?;
    if path == profile || !path.starts_with(&profile) {
        return Err(outside());
    }
    let rows = PolicyRows::load_in(db)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?;
    let (folders, _) = rows.inputs();
    rules::owning_folders(path, &folders, &UserId::new(&folder.user_id))?;
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| delete_failed(&e))?;
    if !meta.is_file() {
        return Err(not_a_track(NOT_A_FILE));
    }
    let (target, id) = (path.to_path_buf(), FileId::of(&meta));
    match tokio::task::spawn_blocking(move || unlink_if_same(&target, id)).await {
        Ok(Ok(())) => {}
        Ok(Err(UnlinkError::Changed)) => return Err(changed_since_check()),
        Ok(Err(UnlinkError::Io(e))) => return Err(delete_failed(&e)),
        Err(e) => return Err(delete_failed(&std::io::Error::other(e))),
    }
    prune(path, &profile).await;
    // beets imported it into the profile folder's own library (discovery's import target).
    forget_detached(queue, path.to_path_buf(), profile);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Library;
    use super::*;
    use crate::import_queue::test_support::stub_queue;
    use shared::failure::FailureCode;
    use soulbeet::ForgetOutcome;

    fn alices_folder(lib: &Library) -> Folder {
        Folder {
            id: "alice-music".into(),
            user_id: "alice".into(),
            name: "Music".into(),
            path: lib.root.join("music/alice").to_string_lossy().into_owned(),
            shared: false,
        }
    }

    #[tokio::test]
    async fn a_staged_file_is_removed_only_from_inside_its_profile_folder() {
        let lib = Library::new().await;
        let (queue, stub) = stub_queue();
        let staged = lib
            .root
            .join("music/alice/Discovery/Explore/Marrow Choir/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let profile =
            std::fs::canonicalize(lib.root.join("music/alice/Discovery/Explore")).unwrap();
        let canonical = std::fs::canonicalize(&staged).unwrap();
        let folder = alices_folder(&lib);
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        remove_staged_in(&lib.db, &queue, &track, &folder, StagedOutcome::Removed)
            .await
            .unwrap();
        assert!(
            !staged.exists()
                && !staged.parent().unwrap().exists()
                && lib.root.join("music/alice/Discovery/Explore").is_dir()
        );
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "removed".into())
        );
        // Its item leaves the library of the profile folder it was imported into.
        assert_eq!(stub.forgets_once(1).await, vec![(profile, canonical)]);

        let library_file = lib.root.join("music/alice/A/x.flac");
        let outside = lib
            .pending_track("t2", "alice-music", "Explore", &library_file, None)
            .await;
        let f = remove_staged_in(&lib.db, &queue, &outside, &folder, StagedOutcome::Removed)
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::DeleteOutsideLibrary,
                "The file is not inside its discovery profile's folder"
            )
        );
        assert!(library_file.exists());
        assert_eq!(lib.track_status("t2").await, "Pending");
        assert_ne!(lib.history_outcome("t2").await, "removed");

        let gone = lib
            .pending_track(
                "t3",
                "alice-music",
                "Explore",
                &lib.root.join("music/alice/Discovery/Explore/gone.flac"),
                None,
            )
            .await;
        remove_staged_in(&lib.db, &queue, &gone, &folder, StagedOutcome::Expired)
            .await
            .unwrap();
        assert_eq!(
            (
                lib.track_status("t3").await,
                lib.history_outcome("t3").await
            ),
            ("Removed".into(), "expired".into())
        );
        // A file kept, or already gone, has no forget queued: a gone one's item waits for the
        // prune at the next start, which first proves the folder is there.
        assert_eq!(stub.forgets().len(), 1);
    }

    #[tokio::test]
    async fn a_removal_whose_forget_fails_is_still_done() {
        let lib = Library::new().await;
        let (queue, stub) = stub_queue();
        stub.script_forget(ForgetOutcome::Failed("database is locked".into()));
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        remove_staged_in(
            &lib.db,
            &queue,
            &track,
            &alices_folder(&lib),
            StagedOutcome::Expired,
        )
        .await
        .unwrap();
        assert_eq!(stub.forgets_once(1).await.len(), 1);
        assert!(!staged.exists());
        assert_eq!(lib.track_status("t1").await, "Removed");
    }

    #[tokio::test]
    async fn another_users_folder_inside_the_profile_folder_keeps_its_files() {
        // alice's folder is shared, so bob may register one inside its staging folder.
        let lib = Library::new().await;
        sqlx::query("UPDATE folders SET shared = 1 WHERE id = 'alice-music'")
            .execute(&lib.db)
            .await
            .unwrap();
        lib.folder(
            "bob",
            "bob-inside",
            "music/alice/Discovery/Explore/Marrow Choir",
            false,
        )
        .await;
        let staged = lib
            .root
            .join("music/alice/Discovery/Explore/Marrow Choir/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let f = remove_staged_in(
            &lib.db,
            &stub_queue().0,
            &track,
            &alices_folder(&lib),
            StagedOutcome::Removed,
        )
        .await
        .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::DeleteOutsideLibrary,
                "The file is not inside one of your library folders"
            )
        );
        assert!(staged.exists());
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Pending".into(), "suggested".into())
        );
    }

    #[tokio::test]
    async fn a_removal_cut_off_by_a_stop_goes_back_to_pending_at_the_next_start() {
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        // The server stops after the claim, before the file is touched.
        claim_removal_in(&lib.db, &track).await.unwrap();
        assert_eq!(lib.track_status("t1").await, "Removing");
        assert_eq!(
            claim_removal_in(&lib.db, &track).await,
            Err(track_not_pending())
        );
        assert_eq!(
            DiscoveryTrackRow::reset_stale_claims_in(&lib.db).await,
            Ok(1)
        );
        assert_eq!(lib.track_status("t1").await, "Pending");
        remove_staged_in(
            &lib.db,
            &stub_queue().0,
            &track,
            &alices_folder(&lib),
            StagedOutcome::Expired,
        )
        .await
        .unwrap();
        assert!(!staged.exists());
        assert_eq!(lib.track_status("t1").await, "Removed");
    }

    #[tokio::test]
    async fn a_stored_path_that_cannot_be_resolved_marks_nothing() {
        // A path through a file: canonicalize fails with ENOTDIR, not NotFound.
        let lib = Library::new().await;
        let stored = lib.root.join("music/alice/A/x.flac/03.flac");
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &stored, None)
            .await;
        let f = remove_staged_in(
            &lib.db,
            &stub_queue().0,
            &track,
            &alices_folder(&lib),
            StagedOutcome::Expired,
        )
        .await
        .unwrap_err();
        assert_eq!(
            (f.code, f.cause.map(|c| c.raw)),
            (
                FailureCode::DeleteFailed,
                Some(std::io::Error::from_raw_os_error(20).to_string()) // ENOTDIR
            )
        );
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Pending".into(), "suggested".into())
        );
    }
}
